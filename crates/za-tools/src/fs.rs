//! `read`, `search`, `glob` (D87, таблица D74, D97, D98).
//!
//! Вызов: разбор аргументов → [`classify`] (без ввода-вывода: сетевые и device-пути
//! отвергаются здесь) → в `spawn_blocking` канонизация корня, [`resolve`] и работа с ФС.
//! Файлы открываются только через [`open_checked`] (без следования ссылке на последнем
//! компоненте, со сверкой дескриптора). Обход ([`ignore::WalkBuilder`]) — в одном потоке,
//! по именам, без перехода по ссылкам и junction (они пропускаются целиком), без смены
//! файловой системы, без `.gitignore` родительских каталогов и глобальных правил git;
//! `.gitignore`, `.ignore` и `.git/info/exclude` действуют и без git. Каталоги `.git` и
//! `.zero-agent` пропускаются на любой глубине. Обход останавливают флаг отмены (бросок
//! future вызова) и дедлайн `timeout − 1 с`.
//!
//! Пути в выводе — [`display`]: внутри проекта относительные через `/`, снаружи абсолютные.
//! Экранирование и предел размера результата — на стороне пакета (D94, D44).

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use globset::{GlobBuilder, GlobMatcher};
use grep_matcher::LineTerminator;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::{DirEntry, WalkBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use za_core::paths::{LexPath, ResolvedPath, Roots, classify, display, open_checked, resolve};
use za_core::{
    Access, Approval, BoxFuture, RawPath, RiskClass, Subjects, Tool, ToolCx, ToolKind, ToolOutput,
    ToolSchema, ToolSpec,
};
use za_protocol::{EffectClass, RunId, RunRole, Tier};

use crate::nearest::nearest;
use crate::tracker::ReadTracker;
use crate::{GLOB_MAX, READ_DEFAULT_LIMIT, READ_MAX_BYTES, SEARCH_MAX};

/// Байт начала файла, по которым он признаётся двоичным (NUL среди них).
const BINARY_SNIFF: usize = 8 * 1024;
/// Предел символов строки `read`.
const READ_LINE_CHARS: usize = 2000;
/// Предел символов строки совпадения `search`.
const SEARCH_LINE_CHARS: usize = 300;
/// Сколько похожих имён подсказывать.
const SIMILAR_MAX: usize = 3;
/// Запас между дедлайном обхода и тайм-аутом вызова (D87).
const WALK_MARGIN: Duration = Duration::from_secs(1);
/// Отметка порядка байт UTF-8.
const BOM: &[u8] = b"\xEF\xBB\xBF";
/// Каталоги, в которые обход не заходит (D87): репозиторий и служебные файлы агента.
const SKIP_DIRS: &[&str] = &[".git", ".zero-agent"];

// Тексты для модели (D23, D94): английский, только ASCII-пунктуация.
const LINE_CUT: &str = "... [line cut]";
const NOT_UTF8: &str = "[file is not UTF-8]";
const TOO_LARGE: &str = "[file is larger than 4 MiB; only the first 4 MiB are read]";
const EMPTY: &str = "[file is empty]";
const NO_MATCHES: &str = "no matches";
const NO_FILES: &str = "no files matched";
const INTERNAL: &str = "internal error: file tool failed";

// ---- тексты с подстановкой: одна функция и для вывода, и для образца в `texts` ----

fn not_found_text(shown: &str, similar: &[String]) -> String {
    if similar.is_empty() {
        format!("not found: {shown}")
    } else {
        format!("not found: {shown}; similar: {}", similar.join(", "))
    }
}

fn binary_text(size: u64, mime: &str) -> String {
    format!("binary file, {size} bytes, type {mime}")
}

fn directory_text(shown: &str) -> String {
    format!("not a regular file: {shown} is a directory")
}

fn io_text(shown: &str, kind: io::ErrorKind) -> String {
    format!("cannot read {shown}: {kind}")
}

fn past_end_text(start: usize, lines: usize, capped: bool) -> String {
    if capped {
        format!("start {start} is past the first 4 MiB of the file ({lines} lines)")
    } else {
        format!("start {start} is past the end of the file ({lines} lines)")
    }
}

fn invisible_text(n: usize) -> String {
    format!("[file contains {n} invisible or bidi control characters]")
}

fn invalid_args_text(e: &str) -> String {
    format!("invalid arguments: {e}")
}

fn invalid_pattern_text(e: &str) -> String {
    format!("invalid pattern: {e}")
}

fn invalid_glob_text(e: &str) -> String {
    format!("invalid glob: {e}")
}

fn more_matches_text(n: u64) -> String {
    format!("... {n} more matches")
}

fn more_files_text(n: u64) -> String {
    format!("... {n} more files")
}

fn stopped_text(secs: u64) -> String {
    format!("... search stopped after {secs} s")
}

// ---- пределы обхода ----

/// Остановка обхода `search` и `glob` (D87): флаг отмены и дедлайн.
#[derive(Debug, Clone)]
pub struct WalkLimits {
    started: Instant,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
}

impl WalkLimits {
    /// Пределы с дедлайном `deadline`; отсчёт времени — с этого вызова.
    pub fn new(deadline: Instant) -> Self {
        Self {
            started: Instant::now(),
            deadline,
            cancel: Arc::default(),
        }
    }

    /// Остановить обход (бросок future вызова).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn stopped(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || Instant::now() >= self.deadline
    }

    fn note(&self) -> String {
        stopped_text(self.started.elapsed().as_secs())
    }
}

/// Бросок future вызова останавливает обход в `spawn_blocking`: сам поток пула не
/// прерывается, но обход проверяет флаг на каждом файле и совпадении.
struct CancelOnDrop(WalkLimits);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

// ---- общее ----

fn spec(schema: ToolSchema, tier: Tier, secs: u64, limit: usize) -> ToolSpec {
    ToolSpec {
        schema,
        class: EffectClass::ReadOnly,
        kind: ToolKind::Read,
        approval: Approval::Mode,
        risk: RiskClass::Read,
        roles: vec![RunRole::Top, RunRole::Orchestrator, RunRole::Worker],
        min_tier: tier,
        timeout: Duration::from_secs(secs),
        output_limit: Some(limit),
        // Метка по вызову (внешний путь) — дело пакета (D82).
        tainted: false,
    }
}

fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, ToolOutput> {
    serde_json::from_value(args).map_err(|e| ToolOutput::error(invalid_args_text(&e.to_string())))
}

fn read_subject(arg: &str) -> Subjects {
    Subjects {
        paths: vec![RawPath {
            arg: arg.to_owned(),
            access: Access::Read,
        }],
        ..Subjects::default()
    }
}

/// Работа с ФС вне потоков tokio.
async fn blocking<F>(f: F) -> ToolOutput
where
    F: FnOnce() -> ToolOutput + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(error = %e, "задача файлового инструмента упала");
            ToolOutput::error(INTERNAL)
        }
    }
}

/// Канонический корень проекта и разрешённый путь (блокирующая).
///
/// До задачи 12 пакет не передаёт разрешённые пути в [`ToolCx`], поэтому инструмент
/// разрешает путь сам. `ZA_HOME` здесь не нужен: защищённость считается только для
/// записи, а эти инструменты только читают.
fn resolve_in(project: &Path, lex: &LexPath) -> Result<(PathBuf, ResolvedPath), ToolOutput> {
    let roots = Roots::canonical(project, project).map_err(|e| {
        tracing::warn!(project = %project.display(), error = %e, "корень проекта не канонизирован");
        ToolOutput::error(format!(
            "project directory is not available: {}",
            project.display()
        ))
    })?;
    let r = resolve(&roots, lex, Access::Read).map_err(|e| ToolOutput::error(e.to_string()))?;
    Ok((roots.project, r))
}

/// Нет такого пути: подсказка похожих имён того же каталога.
fn not_found(root: &Path, path: &Path) -> ToolOutput {
    let similar = match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => {
            let names: Vec<String> = std::fs::read_dir(dir)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            nearest(&name.to_string_lossy(), &names, SIMILAR_MAX)
                .into_iter()
                .map(|n| display(root, &dir.join(n)))
                .collect()
        }
        _ => Vec::new(),
    };
    ToolOutput::error(not_found_text(&display(root, path), &similar))
}

fn is_missing(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Ошибка открытия для модели. Системные ошибки — по виду (английский текст вида, а не
/// локализованное сообщение ОС); отказы `open_checked` — их собственный текст.
fn open_error(root: &Path, path: &Path, e: &io::Error) -> ToolOutput {
    if is_missing(e) {
        not_found(root, path)
    } else if e.raw_os_error().is_some() {
        ToolOutput::error(io_text(&display(root, path), e.kind()))
    } else {
        ToolOutput::error(e.to_string())
    }
}

/// Строка не длиннее `max` символов, иначе начало и [`LINE_CUT`].
fn cut(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}{LINE_CUT}", &s[..i]),
        None => s.to_owned(),
    }
}

// ---- read ----

/// Аргументы `read`.
#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    start: Option<u64>,
    limit: Option<u64>,
}

struct ReadTool {
    spec: ToolSpec,
    tracker: Arc<ReadTracker>,
}

/// Инструмент `read(path, start=1, limit=500)`; прочитанный текстовый файл (целиком в
/// пределах [`READ_MAX_BYTES`]) отмечается в `tracker`.
pub fn read_tool(tracker: Arc<ReadTracker>) -> Arc<dyn Tool> {
    let schema = ToolSchema {
        name: "read".into(),
        description: "Read a text file. Returns numbered lines: line number, tab, text.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path, relative to the project root" },
                "start": { "type": "integer", "minimum": 1, "description": "First line (default 1)" },
                "limit": { "type": "integer", "minimum": 1, "description": "Number of lines (default 500)" }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    };
    Arc::new(ReadTool {
        spec: spec(schema, Tier::Small, 30, 128 * 1024),
        tracker,
    })
}

impl Tool for ReadTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let tracker = self.tracker.clone();
        Box::pin(async move {
            let a: ReadArgs = match parse(args) {
                Ok(a) => a,
                Err(o) => return o,
            };
            let (start, limit) = match (a.start.unwrap_or(1), a.limit) {
                (0, _) => return ToolOutput::error(invalid_args_text("start must be at least 1")),
                (_, Some(0)) => {
                    return ToolOutput::error(invalid_args_text("limit must be at least 1"));
                }
                (s, l) => (
                    usize::try_from(s).unwrap_or(usize::MAX),
                    l.map_or(READ_DEFAULT_LIMIT, |l| {
                        usize::try_from(l).unwrap_or(usize::MAX)
                    }),
                ),
            };
            let lex = match classify(&cx.project_dir, &a.path) {
                Ok(l) => l,
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            let run = cx.run;
            blocking(move || read_blocking(&cx.project_dir, &lex, start, limit, run, &tracker))
                .await
        })
    }

    fn subjects(&self, args: &Value) -> Subjects {
        match args.get("path").and_then(Value::as_str) {
            Some(p) => read_subject(p),
            None => Subjects::default(),
        }
    }

    fn texts(&self) -> Vec<(&'static str, String)> {
        vec![
            ("runtime/fs.read.binary", binary_text(68, "image/png")),
            ("runtime/fs.read.directory", directory_text("src")),
            ("runtime/fs.read.empty", EMPTY.to_owned()),
            ("runtime/fs.read.internal", INTERNAL.to_owned()),
            (
                "runtime/fs.read.invalid_args",
                invalid_args_text("missing field `path`"),
            ),
            ("runtime/fs.read.invisible", invisible_text(1)),
            (
                "runtime/fs.read.io_error",
                io_text("a.txt", io::ErrorKind::PermissionDenied),
            ),
            ("runtime/fs.read.line_cut", LINE_CUT.to_owned()),
            (
                "runtime/fs.read.not_found",
                not_found_text("src/mian.rs", &["src/main.rs".to_owned()]),
            ),
            ("runtime/fs.read.not_utf8", NOT_UTF8.to_owned()),
            ("runtime/fs.read.past_end", past_end_text(10, 3, false)),
            (
                "runtime/fs.read.past_end_capped",
                past_end_text(10, 3, true),
            ),
            ("runtime/fs.read.too_large", TOO_LARGE.to_owned()),
        ]
    }
}

fn read_blocking(
    project: &Path,
    lex: &LexPath,
    start: usize,
    limit: usize,
    run: RunId,
    tracker: &ReadTracker,
) -> ToolOutput {
    let (root, r) = match resolve_in(project, lex) {
        Ok(x) => x,
        Err(o) => return o,
    };
    if r.path.is_dir() {
        return ToolOutput::error(directory_text(&display(&root, &r.path)));
    }
    match read_file(&r, start, limit) {
        Ok((out, hash)) => {
            if let Some(h) = hash {
                tracker.mark(run, &r.path, &h);
            }
            out
        }
        Err(e) => open_error(&root, &r.path, &e),
    }
}

/// Чтение строк `start..start + limit` файла `r` (блокирующая: только в `spawn_blocking`).
///
/// Файл открывается через [`open_checked`]; ошибка открытия (нет файла, не обычный файл,
/// ссылка, подмена) — `Err`. Формат — `{n:>6}\t{text}\n`; `\r` в конце строки и BOM не
/// показываются; строка длиннее 2000 символов режется. Файл с NUL в первых 8 КиБ —
/// `binary file, N bytes, type <mime>`. В конце — пометки: файл не в UTF-8 (строки
/// декодированы с заменой), невидимые и bidi-символы в показанных строках (они не
/// удаляются: `old` у `edit` должен совпасть с файлом), файл больше [`READ_MAX_BYTES`].
pub fn read_at(r: &ResolvedPath, start: usize, limit: usize) -> io::Result<ToolOutput> {
    read_file(r, start, limit).map(|(o, _)| o)
}

/// [`read_at`] и BLAKE3 (hex) сырых байт файла — если текстовый файл прочитан целиком.
fn read_file(
    r: &ResolvedPath,
    start: usize,
    limit: usize,
) -> io::Result<(ToolOutput, Option<String>)> {
    let f = open_checked(r, false)?;
    let mut bytes = Vec::new();
    (&f).take(BINARY_SNIFF as u64).read_to_end(&mut bytes)?;
    if bytes.contains(&0) {
        let size = f.metadata()?.len();
        return Ok((ToolOutput::ok(binary_text(size, mime(&bytes))), None));
    }
    let rest = (READ_MAX_BYTES + 1).saturating_sub(bytes.len()) as u64;
    (&f).take(rest).read_to_end(&mut bytes)?;
    let capped = bytes.len() > READ_MAX_BYTES;
    if capped {
        bytes.truncate(READ_MAX_BYTES);
    }
    let hash = (!capped).then(|| blake3::hash(&bytes).to_hex().to_string());
    let text = bytes.strip_prefix(BOM).unwrap_or(&bytes);
    Ok((render(text, start, limit, capped), hash))
}

fn render(text: &[u8], start: usize, limit: usize, capped: bool) -> ToolOutput {
    let body = text.strip_suffix(b"\n").unwrap_or(text);
    let total = if text.is_empty() {
        0
    } else {
        body.iter().filter(|&&b| b == b'\n').count() + 1
    };
    if total == 0 {
        return ToolOutput::ok(EMPTY);
    }
    if start > total {
        return ToolOutput::error(past_end_text(start, total, capped));
    }
    let mut out = String::new();
    let mut invisible = 0;
    for (i, line) in body
        .split(|&b| b == b'\n')
        .enumerate()
        .skip(start - 1)
        .take(limit)
    {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let s = String::from_utf8_lossy(line);
        invisible += s.chars().filter(|&c| is_invisible(c)).count();
        out.push_str(&format!("{:>6}\t{}\n", i + 1, cut(&s, READ_LINE_CHARS)));
    }
    let utf8 = match std::str::from_utf8(text) {
        Ok(_) => true,
        // Срез по пределу мог разрезать последний символ — это не порча кодировки.
        Err(e) => capped && e.error_len().is_none(),
    };
    if !utf8 {
        out.push_str(NOT_UTF8);
        out.push('\n');
    }
    if invisible > 0 {
        out.push_str(&invisible_text(invisible));
        out.push('\n');
    }
    if capped {
        out.push_str(TOO_LARGE);
        out.push('\n');
    }
    ToolOutput::ok(out)
}

/// Невидимые и управляющие направлением символы (SEC-051, D82): теги, нулевой ширины,
/// bidi. U+FEFF в начале файла (BOM) снят раньше.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{E0000}'..='\u{E007F}'
        | '\u{200B}'..='\u{200F}'
        | '\u{2060}'..='\u{2064}'
        | '\u{FEFF}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2066}'..='\u{2069}')
}

/// Тип двоичного файла по сигнатуре.
fn mime(head: &[u8]) -> &'static str {
    const SIGNATURES: &[(&[u8], &str)] = &[
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xFF\xD8\xFF", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"%PDF-", "application/pdf"),
        (b"PK\x03\x04", "application/zip"),
        (b"\x1F\x8B", "application/gzip"),
        (b"7z\xBC\xAF\x27\x1C", "application/x-7z-compressed"),
        (b"\x7FELF", "application/x-elf"),
        (b"MZ", "application/vnd.microsoft.portable-executable"),
        (b"\0asm", "application/wasm"),
        (b"SQLite format 3\0", "application/vnd.sqlite3"),
    ];
    if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        return "image/webp";
    }
    SIGNATURES
        .iter()
        .find(|(sig, _)| head.starts_with(sig))
        .map_or("application/octet-stream", |(_, m)| m)
}

// ---- обход ----

/// Обход `base` (D87). Ссылки и junction не обходятся и не выдаются; `.git` и
/// `.zero-agent` пропускаются.
fn walker(base: &Path) -> ignore::Walk {
    let mut wb = WalkBuilder::new(base);
    wb.hidden(false)
        .parents(false)
        .require_git(false)
        .git_global(false)
        .git_exclude(true)
        .git_ignore(true)
        .ignore(true)
        .follow_links(false)
        .same_file_system(true)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(keep);
    wb.build()
}

fn keep(e: &DirEntry) -> bool {
    if e.depth() == 0 {
        return true;
    }
    let Some(ft) = e.file_type() else {
        return false;
    };
    if ft.is_symlink() {
        return false;
    }
    !(ft.is_dir() && SKIP_DIRS.iter().any(|d| e.file_name() == *d))
}

/// Обычные файлы обхода по порядку. Возвращает, остановлен ли обход пределами.
fn walk_files(base: &Path, limits: &WalkLimits, mut f: impl FnMut(&Path)) -> bool {
    for entry in walker(base) {
        if limits.stopped() {
            return true;
        }
        let e = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::debug!(error = %err, "запись обхода пропущена");
                continue;
            }
        };
        if e.file_type().is_some_and(|t| t.is_file()) {
            f(e.path());
        }
    }
    limits.stopped()
}

/// Путь `p` относительно `base` через `/`.
fn rel(base: &Path, p: &Path) -> String {
    p.strip_prefix(base)
        .map(|r| {
            r.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default()
}

fn glob_matcher(pattern: &str) -> Result<GlobMatcher, globset::Error> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(cfg!(windows))
        .build()
        .map(|g| g.compile_matcher())
}

// ---- search ----

/// Аргументы `search`.
#[derive(Deserialize)]
struct SearchArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
}

struct SearchTool {
    spec: ToolSpec,
}

/// Инструмент `search(pattern, path=".", glob?)`.
pub fn search_tool() -> Arc<dyn Tool> {
    let schema = ToolSchema {
        name: "search".into(),
        description: "Search file contents with a regular expression. Returns path:line:text, up to 200 matches. Skips files ignored by .gitignore.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression" },
                "path": { "type": "string", "description": "File or directory (default: project root)" },
                "glob": { "type": "string", "description": "Only files matching this glob, e.g. *.rs" }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    };
    Arc::new(SearchTool {
        spec: spec(schema, Tier::Small, 60, 64 * 1024),
    })
}

impl Tool for SearchTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let timeout = self.spec.timeout;
        Box::pin(async move {
            let a: SearchArgs = match parse(args) {
                Ok(a) => a,
                Err(o) => return o,
            };
            let lex = match classify(&cx.project_dir, a.path.as_deref().unwrap_or(".")) {
                Ok(l) => l,
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            let limits = WalkLimits::new(Instant::now() + timeout.saturating_sub(WALK_MARGIN));
            let _cancel = CancelOnDrop(limits.clone());
            blocking(move || match resolve_in(&cx.project_dir, &lex) {
                Ok((root, base)) => search_in(&root, &base, &a.pattern, a.glob.as_deref(), &limits),
                Err(o) => o,
            })
            .await
        })
    }

    fn subjects(&self, args: &Value) -> Subjects {
        read_subject(args.get("path").and_then(Value::as_str).unwrap_or("."))
    }

    fn texts(&self) -> Vec<(&'static str, String)> {
        vec![
            ("runtime/fs.search.internal", INTERNAL.to_owned()),
            (
                "runtime/fs.search.invalid_args",
                invalid_args_text("missing field `pattern`"),
            ),
            (
                "runtime/fs.search.invalid_glob",
                invalid_glob_text("unclosed character class"),
            ),
            (
                "runtime/fs.search.invalid_pattern",
                invalid_pattern_text("regex parse error"),
            ),
            (
                "runtime/fs.search.io_error",
                io_text("src", io::ErrorKind::PermissionDenied),
            ),
            ("runtime/fs.search.line_cut", LINE_CUT.to_owned()),
            ("runtime/fs.search.more", more_matches_text(1)),
            ("runtime/fs.search.no_matches", NO_MATCHES.to_owned()),
            (
                "runtime/fs.search.not_found",
                not_found_text("src/mian.rs", &["src/main.rs".to_owned()]),
            ),
            ("runtime/fs.search.stopped", stopped_text(1)),
        ]
    }
}

/// Совпадения одного файла: первые [`SEARCH_MAX`] строк, остальные считаются.
struct Hits<'a> {
    shown: String,
    out: &'a mut Vec<String>,
    more: &'a mut u64,
    limits: &'a WalkLimits,
}

impl Sink for Hits<'_> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, io::Error> {
        if self.out.len() < SEARCH_MAX {
            let text = String::from_utf8_lossy(m.bytes());
            let text = text.trim_end_matches(['\n', '\r']);
            self.out.push(format!(
                "{}:{}:{}",
                self.shown,
                m.line_number().unwrap_or(0),
                cut(text, SEARCH_LINE_CHARS)
            ));
        } else {
            *self.more += 1;
        }
        Ok(!self.limits.stopped())
    }
}

/// Поиск `pattern` в `base` (файл или каталог) — блокирующая, только в `spawn_blocking`.
///
/// `root` — канонический корень проекта (для путей вывода). `glob` с `/` сопоставляется с
/// путём от `base`, без `/` — с именем файла; на Windows без учёта регистра. Явно
/// указанный файл ищется, даже если он игнорируется. Двоичные файлы (NUL) пропускаются,
/// строки не в UTF-8 показываются с заменой.
pub fn search_in(
    root: &Path,
    base: &ResolvedPath,
    pattern: &str,
    glob: Option<&str>,
    limits: &WalkLimits,
) -> ToolOutput {
    let matcher = match RegexMatcherBuilder::new()
        .line_terminator(Some(b'\n'))
        .crlf(true)
        .build(pattern)
    {
        Ok(m) => m,
        Err(e) => return ToolOutput::error(invalid_pattern_text(&e.to_string())),
    };
    let filter = match glob.map(glob_matcher).transpose() {
        Ok(f) => f,
        Err(e) => return ToolOutput::error(invalid_glob_text(&e.to_string())),
    };
    let by_path = glob.is_some_and(|g| g.contains('/'));
    let meta = match std::fs::metadata(&base.path) {
        Ok(m) => m,
        Err(e) => return open_error(root, &base.path, &e),
    };
    // Терминатор строки у поиска и у выражения один: CRLF (`$` и перед `\r\n`).
    let mut searcher = SearcherBuilder::new()
        .line_terminator(LineTerminator::crlf())
        .line_number(true)
        .binary_detection(BinaryDetection::quit(0))
        .build();
    let mut out = Vec::new();
    let mut more = 0u64;
    let mut search_file = |path: &Path| {
        let r = ResolvedPath {
            path: path.to_path_buf(),
            inside: base.inside,
            protected: false,
        };
        let file = match open_checked(&r, false) {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "файл поиска пропущен");
                return;
            }
        };
        search_one(
            &mut searcher,
            &matcher,
            &file,
            Hits {
                shown: display(root, path),
                out: &mut out,
                more: &mut more,
                limits,
            },
        );
    };
    let stopped = if meta.is_dir() {
        walk_files(&base.path, limits, |p| {
            let keep = match &filter {
                None => true,
                Some(m) if by_path => m.is_match(rel(&base.path, p)),
                Some(m) => p.file_name().is_some_and(|n| m.is_match(n)),
            };
            if keep {
                search_file(p);
            }
        })
    } else {
        search_file(&base.path);
        limits.stopped()
    };
    let mut text = if out.is_empty() {
        NO_MATCHES.to_owned()
    } else {
        out.join("\n")
    };
    if more > 0 {
        text.push('\n');
        text.push_str(&more_matches_text(more));
    }
    if stopped {
        text.push('\n');
        text.push_str(&limits.note());
    }
    ToolOutput::ok(text)
}

fn search_one(searcher: &mut Searcher, matcher: &RegexMatcher, file: &File, hits: Hits<'_>) {
    let shown = hits.shown.clone();
    if let Err(e) = searcher.search_file(matcher, file, hits) {
        tracing::debug!(path = %shown, error = %e, "ошибка чтения при поиске");
    }
}

// ---- glob ----

/// Аргументы `glob`.
#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
}

struct GlobTool {
    spec: ToolSpec,
}

/// Инструмент `glob(pattern)`.
pub fn glob_tool() -> Arc<dyn Tool> {
    let schema = ToolSchema {
        name: "glob".into(),
        description: "List files matching a glob pattern, e.g. src/**/*.rs. Returns sorted paths, up to 500. Skips files ignored by .gitignore.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern, relative to the project root" }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    };
    Arc::new(GlobTool {
        spec: spec(schema, Tier::Standard, 30, 64 * 1024),
    })
}

/// Литеральный префикс шаблона до каталога с первым метасимволом (`*?[{`) и остаток
/// (Ruling 7.23): `src/**/*.rs` → (`src/`, `**/*.rs`), `C:/**/*.kdbx` → (`C:/`, …),
/// `*.rs` → (``, `*.rs`); шаблон без метасимволов делится по последнему `/`. На Windows
/// `\` — разделитель и заменяется на `/`.
pub fn glob_split(pattern: &str) -> (String, String) {
    let p = if cfg!(windows) {
        pattern.replace('\\', "/")
    } else {
        pattern.to_owned()
    };
    let meta = p.find(['*', '?', '[', '{']).unwrap_or(p.len());
    match p[..meta].rfind('/') {
        Some(i) => (p[..=i].to_owned(), p[i + 1..].to_owned()),
        None => (String::new(), p),
    }
}

/// Аргумент пути префикса для `subjects` и разбора: пустой префикс — корень проекта.
fn prefix_arg(prefix: &str) -> &str {
    if prefix.is_empty() { "." } else { prefix }
}

impl Tool for GlobTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let timeout = self.spec.timeout;
        Box::pin(async move {
            let a: GlobArgs = match parse(args) {
                Ok(a) => a,
                Err(o) => return o,
            };
            let (prefix, rest) = glob_split(&a.pattern);
            let lex = match classify(&cx.project_dir, prefix_arg(&prefix)) {
                Ok(l) => l,
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            let limits = WalkLimits::new(Instant::now() + timeout.saturating_sub(WALK_MARGIN));
            let _cancel = CancelOnDrop(limits.clone());
            blocking(move || match resolve_in(&cx.project_dir, &lex) {
                Ok((root, base)) => glob_in(&root, &base, &rest, &limits),
                Err(o) => o,
            })
            .await
        })
    }

    fn subjects(&self, args: &Value) -> Subjects {
        match args.get("pattern").and_then(Value::as_str) {
            Some(p) => read_subject(prefix_arg(&glob_split(p).0)),
            None => Subjects::default(),
        }
    }

    fn texts(&self) -> Vec<(&'static str, String)> {
        vec![
            ("runtime/fs.glob.internal", INTERNAL.to_owned()),
            (
                "runtime/fs.glob.invalid_args",
                invalid_args_text("missing field `pattern`"),
            ),
            (
                "runtime/fs.glob.invalid_pattern",
                invalid_pattern_text("unclosed character class"),
            ),
            ("runtime/fs.glob.more", more_files_text(1)),
            ("runtime/fs.glob.no_files", NO_FILES.to_owned()),
            ("runtime/fs.glob.stopped", stopped_text(1)),
        ]
    }
}

/// Файлы в `base`, чей путь от `base` (через `/`) подходит под `rest` (`*` не переходит
/// `/`, `**` — любые каталоги; на Windows без учёта регистра), — блокирующая, только в
/// `spawn_blocking`. Пути по возрастанию, до [`GLOB_MAX`]. Пустой `rest` — все файлы.
/// Нет каталога `base` — пустой список.
pub fn glob_in(root: &Path, base: &ResolvedPath, rest: &str, limits: &WalkLimits) -> ToolOutput {
    let matcher = match glob_matcher(if rest.is_empty() { "**" } else { rest }) {
        Ok(m) => m,
        Err(e) => return ToolOutput::error(invalid_pattern_text(&e.to_string())),
    };
    let mut found = BTreeSet::new();
    let mut more = 0u64;
    let stopped = if base.path.is_dir() {
        walk_files(&base.path, limits, |p| {
            if matcher.is_match(rel(&base.path, p)) {
                found.insert(display(root, p));
                if found.len() > GLOB_MAX {
                    found.pop_last();
                    more += 1;
                }
            }
        })
    } else {
        false
    };
    let mut text = if found.is_empty() {
        NO_FILES.to_owned()
    } else {
        found.into_iter().collect::<Vec<_>>().join("\n")
    };
    if more > 0 {
        text.push('\n');
        text.push_str(&more_files_text(more));
    }
    if stopped {
        text.push('\n');
        text.push_str(&limits.note());
    }
    ToolOutput::ok(text)
}
