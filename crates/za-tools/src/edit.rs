//! `edit` и `multi_edit` (D88, таблица D74, D98; раздел 4.1 и таблица 3.4 ТЗ 02).
//!
//! Вызов: разбор аргументов → [`classify`] (сетевые и device-пути, ADS, имена устройств
//! отвергаются без обращения к ФС) → в `spawn_blocking`:
//! 1. [`resolve`] с `Access::Write`: путь канонический, ссылки и junction раскрыты.
//!    Защищённое место (D79: `.git/`, `.zero-agent/` целиком — включая рабочие файлы
//!    агента, маркеры которых модель подделывать не должна) — `denied: protected path`.
//!    `ZA_HOME` инструмент не знает, его защищает политика по разрешённым путям (D79,
//!    D80). Путь вне проекта инструмент не отвергает: это решение политики (`Ask`, D77),
//!    инструмент сообщает его в [`Tool::subjects`] с `Access::Write`.
//! 2. Маркер повтора `work_dir("edits")/<file_stem>.json`: файл уже равен `after` —
//!    результат маркера; равен `before` — правка продолжается (проверку трекера
//!    прерванная попытка уже прошла на этом содержимом); иначе — ошибка.
//! 3. [`ReadTracker`] по каноническому пути: правка только прочитанного и не изменившегося
//!    файла. Создание нового файла (`old` пуст, файла нет) её пропускает.
//! 4. Патчер [`crate::patch::apply`], для `multi_edit` — по очереди на копии.
//! 5. Защита от перезаписи [`shrink_guard`].
//! 6. Запись по каноническому пути: временный файл рядом с целью ([`open_checked`]), права
//!    цели, маркер, сверка, что цель не изменилась, и [`replace_checked`] (`rename`, на
//!    Windows с повторами). Ссылка не подменяется файлом: пишется её цель.
//!
//! BOM, стиль переводов строк и отсутствие `\n` в конце сохраняются. Файл не в UTF-8 и
//! файл больше [`READ_MAX_BYTES`] не правятся.

use std::fs::Permissions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use za_core::paths::{
    LexPath, ResolvedPath, Roots, classify, create_parents_checked, display, open_checked,
    remove_checked, replace_checked, resolve, stat_checked, work_dir,
};
use za_core::{
    Access, Approval, BoxFuture, Postcondition, RawPath, RiskClass, Subjects, Tool, ToolCx,
    ToolKind, ToolOutput, ToolSchema, ToolSpec,
};
use za_protocol::{EffectClass, RunId, RunRole, Tier, deny_reason};

use crate::READ_MAX_BYTES;
use crate::fs::{
    blocking, directory_text, error_text, invalid_args_text, is_missing, not_found, open_error,
    parse,
};
use crate::patch::{MatchLevel, PatchError, apply, shrink_guard};
use crate::tracker::{ReadTracker, TrackError};

/// Отметка порядка байт UTF-8.
const BOM: &[u8] = b"\xEF\xBB\xBF";
/// Служебный каталог проекта: целиком защищён от правки (см. модуль).
const ZA_DIR: &str = ".zero-agent";
/// Подкаталог маркеров повтора в `work_dir`.
const EDITS_DIR: &str = "edits";
/// Версия формата маркера.
const MARKER_V: u32 = 1;
/// Предел размера файла маркера при чтении.
const MARKER_MAX: u64 = 1 << 20;
/// Предел времени построения diff результата.
const DIFF_TIMEOUT: Duration = Duration::from_secs(2);
/// Контекст унифицированного diff (D88).
const DIFF_CONTEXT: usize = 2;
/// Сколько номеров строк показывать у неоднозначного `old`.
const AMBIGUOUS_SHOWN: usize = 10;

// Тексты для модели (D23, D94): английский, только ASCII-пунктуация.
const NOT_READ: &str = "file not read in this run; read it first";
const CHANGED: &str = "file changed since last read; read it again";
const MARKER_CHANGED: &str = "file changed since the interrupted edit; read it again";
const CHANGED_DURING: &str = "file changed during the edit; read it again";
const NOT_UTF8: &str = "file is not UTF-8; edit refused";
const TOO_LARGE: &str = "file is larger than 4 MiB; edit refused";
const EXISTS: &str = "file already exists and is not empty; put the text to replace in old";
const SAME: &str = "new is the same as old; nothing to change";
const NO_CHANGE: &str = "edit leaves the file unchanged";
const EMPTY_OLD: &str = "old is empty; use edit with empty old to create a file";
const NO_EDITS: &str = "edits is empty";
const INTERNAL: &str = "internal error: edit tool failed";
const INTERRUPTED: &str = "internal error: edit interrupted";
const NOTE_LINE_ENDINGS: &str = "[old matched after normalizing line endings]";
const NOTE_TRAILING: &str = "[old matched ignoring trailing whitespace]";
const NOTE_INDENT: &str = "[old matched ignoring indentation; new was re-indented]";

// ---- тексты с подстановкой: одна функция и для вывода, и для образца в `texts` ----

fn denied_text() -> String {
    format!("denied: {}", deny_reason::PROTECTED_PATH)
}

fn not_found_old_text(closest: Option<&(usize, usize, String)>) -> String {
    match closest {
        None => "old not found".to_owned(),
        Some((a, b, region)) => format!("old not found; closest region lines {a}-{b}:\n{region}"),
    }
}

fn ambiguous_text(lines: &[usize]) -> String {
    let mut shown: Vec<String> = lines
        .iter()
        .take(AMBIGUOUS_SHOWN)
        .map(ToString::to_string)
        .collect();
    if lines.len() > AMBIGUOUS_SHOWN {
        shown.push("...".to_owned());
    }
    format!(
        "old matches {} places (lines {}); add surrounding lines",
        lines.len(),
        shown.join(", ")
    )
}

fn shrink_text(removed: usize, total: usize) -> String {
    format!("edit removes {removed} of {total} lines; pass allow_shrink if intended")
}

fn numbered_text(n: usize, e: &str) -> String {
    format!("edit {n}: {e}")
}

fn project_text(project: &Path) -> String {
    format!("project directory is not available: {}", project.display())
}

fn write_error_text(shown: &str, kind: io::ErrorKind) -> String {
    format!("cannot write {shown}: {kind}")
}

fn header_text(verb: &str, shown: &str, added: usize, removed: usize, h: (&str, &str)) -> String {
    format!(
        "{verb} {shown}: +{added} -{removed} lines, blake3 {}->{}",
        h.0, h.1
    )
}

fn patch_text(e: &PatchError) -> String {
    match e {
        PatchError::NotFound { closest } => not_found_old_text(closest.as_ref()),
        PatchError::Ambiguous { lines } => ambiguous_text(lines),
        PatchError::Shrink { removed, total } => shrink_text(*removed, *total),
        PatchError::Exists => EXISTS.to_owned(),
        PatchError::NotUtf8 => NOT_UTF8.to_owned(),
    }
}

// ---- точки сбоя для тестов ----

/// Точка вызова [`EditHook`] внутри правки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditPoint {
    /// Все проверки пройдены, временный файл ещё не записан.
    BeforeWrite,
    /// Временный файл и маркер записаны, `rename` ещё не сделан.
    BeforeRename,
}

/// Крюк тестов (по образцу `CrashHook`): вызывается в точке [`EditPoint`] с каноническим
/// путём цели. Может менять ФС (подмена между проверкой и записью). `true` — сбой
/// процесса: правка обрывается сразу, без уборки (временный файл и маркер остаются).
pub type EditHook = Arc<dyn Fn(EditPoint, &Path) -> bool + Send + Sync>;

// ---- аргументы ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    old: String,
    new: String,
    #[serde(default)]
    allow_shrink: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pair {
    old: String,
    new: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MultiArgs {
    path: String,
    edits: Vec<Pair>,
    #[serde(default)]
    allow_shrink: bool,
}

/// Правка после разбора: путь, пары `(old, new)`, `allow_shrink`, `multi_edit` ли это.
struct Request {
    path: String,
    edits: Vec<(String, String)>,
    allow_shrink: bool,
    multi: bool,
}

impl Request {
    fn from_args(args: Value, multi: bool) -> Result<Self, ToolOutput> {
        let r = if multi {
            let a: MultiArgs = parse(args)?;
            Self {
                path: a.path,
                edits: a.edits.into_iter().map(|p| (p.old, p.new)).collect(),
                allow_shrink: a.allow_shrink,
                multi,
            }
        } else {
            let a: EditArgs = parse(args)?;
            Self {
                path: a.path,
                edits: vec![(a.old, a.new)],
                allow_shrink: a.allow_shrink,
                multi,
            }
        };
        if r.edits.is_empty() {
            return Err(ToolOutput::error(invalid_args_text(NO_EDITS)));
        }
        for (i, (old, new)) in r.edits.iter().enumerate() {
            if old == new {
                return Err(r.error(i, SAME));
            }
            if multi && old.is_empty() {
                return Err(r.error(i, EMPTY_OLD));
            }
        }
        Ok(r)
    }

    /// Ошибка правки `i` (с 0): у `multi_edit` — с номером правки.
    fn error(&self, i: usize, e: &str) -> ToolOutput {
        if self.multi {
            ToolOutput::error(numbered_text(i + 1, e))
        } else {
            ToolOutput::error(e)
        }
    }

    /// Создание нового файла: единственная правка с пустым `old`.
    fn creates(&self) -> bool {
        !self.multi && self.edits.first().is_some_and(|(old, _)| old.is_empty())
    }
}

/// Маркер повтора (D88): что было до правки, что стало и результат для модели.
#[derive(Serialize, Deserialize)]
struct Marker {
    v: u32,
    path: PathBuf,
    /// BLAKE3 аргументов вызова: маркер другого вызова не применяется.
    args: String,
    /// BLAKE3 файла до правки; `None` — файла не было.
    before: Option<String>,
    after: String,
    output: ToolOutput,
}

// ---- инструмент ----

struct EditTool {
    spec: ToolSpec,
    tracker: Arc<ReadTracker>,
    multi: bool,
    hook: Option<EditHook>,
}

/// Инструмент `edit(path, old, new, allow_shrink?)`.
pub fn edit_tool(tracker: Arc<ReadTracker>) -> Arc<dyn Tool> {
    build(tracker, false, None)
}

/// Инструмент `multi_edit(path, edits[{old, new}], allow_shrink?)`.
pub fn multi_edit_tool(tracker: Arc<ReadTracker>) -> Arc<dyn Tool> {
    build(tracker, true, None)
}

/// [`edit_tool`] с крюком сбоя тестов.
pub fn edit_tool_with_hook(tracker: Arc<ReadTracker>, hook: EditHook) -> Arc<dyn Tool> {
    build(tracker, false, Some(hook))
}

/// [`multi_edit_tool`] с крюком сбоя тестов.
pub fn multi_edit_tool_with_hook(tracker: Arc<ReadTracker>, hook: EditHook) -> Arc<dyn Tool> {
    build(tracker, true, Some(hook))
}

fn build(tracker: Arc<ReadTracker>, multi: bool, hook: Option<EditHook>) -> Arc<dyn Tool> {
    let path =
        json!({ "type": "string", "description": "File path, relative to the project root" });
    let shrink = json!({
        "type": "boolean",
        "description": "Allow removing more than half of the lines of a file with 20+ lines"
    });
    let (schema, tier) = if multi {
        (
            ToolSchema {
                name: "multi_edit".into(),
                description: "Apply several edit replacements to one file in order. \
                    All or nothing: if any edit fails, the file is not changed."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": path,
                        "edits": {
                            "type": "array",
                            "minItems": 1,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old": { "type": "string", "description": "Text to replace (not empty)" },
                                    "new": { "type": "string", "description": "Replacement text" }
                                },
                                "required": ["old", "new"],
                                "additionalProperties": false
                            }
                        },
                        "allow_shrink": shrink
                    },
                    "required": ["path", "edits"],
                    "additionalProperties": false
                }),
            },
            Tier::Standard,
        )
    } else {
        (
            ToolSchema {
                name: "edit".into(),
                description: "Replace the only occurrence of old with new in a file. \
                    Read the file first. Empty old creates a new file."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": path,
                        "old": { "type": "string", "description": "Exact text to replace, with enough lines to be unique" },
                        "new": { "type": "string", "description": "Replacement text" },
                        "allow_shrink": shrink
                    },
                    "required": ["path", "old", "new"],
                    "additionalProperties": false
                }),
            },
            Tier::Small,
        )
    };
    Arc::new(EditTool {
        spec: ToolSpec {
            schema,
            class: EffectClass::Idempotent,
            kind: ToolKind::Edit,
            approval: Approval::Mode,
            risk: RiskClass::LocalWrite,
            roles: vec![RunRole::Top, RunRole::Orchestrator, RunRole::Worker],
            min_tier: tier,
            timeout: Duration::from_secs(30),
            output_limit: Some(64 * 1024),
            tainted: false,
        },
        tracker,
        multi,
        hook,
    })
}

impl Tool for EditTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let tracker = self.tracker.clone();
        let hook = self.hook.clone();
        let multi = self.multi;
        Box::pin(async move {
            let args_hash = blake3::hash(args.to_string().as_bytes())
                .to_hex()
                .to_string();
            let req = match Request::from_args(args, multi) {
                Ok(r) => r,
                Err(o) => return o,
            };
            let lex = match classify(&cx.project_dir, &req.path) {
                Ok(l) => l,
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            let job = Job {
                project: cx.project_dir,
                run: cx.run,
                stem: cx.effect_key.file_stem(),
                args_hash,
                req,
                tracker,
                hook,
            };
            blocking(move || job.run(&lex)).await
        })
    }

    fn subjects(&self, args: &Value) -> Subjects {
        match args.get("path").and_then(Value::as_str) {
            Some(p) => Subjects {
                paths: vec![RawPath {
                    arg: p.to_owned(),
                    access: Access::Write,
                }],
                ..Subjects::default()
            },
            None => Subjects::default(),
        }
    }

    fn texts(&self) -> Vec<(&'static str, String)> {
        if self.multi {
            return vec![
                ("runtime/edit.multi.empty_old", numbered_text(2, EMPTY_OLD)),
                ("runtime/edit.multi.no_edits", invalid_args_text(NO_EDITS)),
                (
                    "runtime/edit.multi.numbered",
                    numbered_text(2, &not_found_old_text(None)),
                ),
            ];
        }
        vec![
            ("runtime/edit.ambiguous", ambiguous_text(&[3, 17])),
            ("runtime/edit.changed", CHANGED.to_owned()),
            ("runtime/edit.changed_during", CHANGED_DURING.to_owned()),
            (
                "runtime/edit.created",
                header_text("created", "src/new.rs", 3, 0, ("none", "1a2b3c4d")),
            ),
            ("runtime/edit.denied", denied_text()),
            (
                "runtime/edit.edited",
                header_text("edited", "src/main.rs", 1, 1, ("1a2b3c4d", "5e6f7a8b")),
            ),
            ("runtime/edit.exists", EXISTS.to_owned()),
            ("runtime/edit.internal", INTERNAL.to_owned()),
            ("runtime/edit.interrupted", INTERRUPTED.to_owned()),
            (
                "runtime/edit.invalid_args",
                invalid_args_text("missing field `old`"),
            ),
            ("runtime/edit.marker_changed", MARKER_CHANGED.to_owned()),
            ("runtime/edit.no_change", NO_CHANGE.to_owned()),
            ("runtime/edit.not_found", not_found_old_text(None)),
            (
                "runtime/edit.not_found_closest",
                not_found_old_text(Some(&(4, 5, "fn main() {\n    run();".to_owned()))),
            ),
            ("runtime/edit.not_read", NOT_READ.to_owned()),
            ("runtime/edit.not_utf8", NOT_UTF8.to_owned()),
            ("runtime/edit.note_indent", NOTE_INDENT.to_owned()),
            (
                "runtime/edit.note_line_endings",
                NOTE_LINE_ENDINGS.to_owned(),
            ),
            ("runtime/edit.note_trailing", NOTE_TRAILING.to_owned()),
            (
                "runtime/edit.project",
                project_text(Path::new("/work/project")),
            ),
            ("runtime/edit.same", SAME.to_owned()),
            ("runtime/edit.shrink", shrink_text(30, 40)),
            ("runtime/edit.too_large", TOO_LARGE.to_owned()),
            (
                "runtime/edit.write_error",
                write_error_text("a.txt", io::ErrorKind::PermissionDenied),
            ),
        ]
    }
}

// ---- исполнение (блокирующее) ----

/// Всё, что нужно правке в `spawn_blocking`.
struct Job {
    project: PathBuf,
    run: RunId,
    stem: String,
    args_hash: String,
    req: Request,
    tracker: Arc<ReadTracker>,
    hook: Option<EditHook>,
}

/// Файл до правки.
struct Before {
    /// Сырые байты; `None` — файла нет.
    bytes: Option<Vec<u8>>,
    hash: Option<String>,
    perms: Option<Permissions>,
}

fn hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn short(h: Option<&str>) -> &str {
    h.map_or("none", |h| &h[..8.min(h.len())])
}

impl Job {
    fn run(self, lex: &LexPath) -> ToolOutput {
        // `ZA_HOME` инструменту неизвестен: на его месте — `.zero-agent` проекта, так
        // защищённым становится весь служебный каталог (см. модуль).
        let roots = match Roots::canonical(&self.project, &self.project.join(ZA_DIR)) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(project = %self.project.display(), error = %e, "корень проекта не канонизирован");
                return ToolOutput::error(project_text(&self.project));
            }
        };
        let root = roots.project.clone();
        let r = match resolve(&roots, lex, Access::Write) {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        if r.protected {
            return ToolOutput::error(denied_text());
        }
        let shown = display(&root, &r.path);
        let before = match read_before(&root, &r) {
            Ok(b) => b,
            Err(o) => return o,
        };

        // 1. Маркер повтора.
        let marker_r = match work_dir(&root, EDITS_DIR) {
            Ok(dir) => ResolvedPath {
                path: dir.join(format!("{}.json", self.stem)),
                inside: true,
                protected: false,
            },
            Err(e) => {
                tracing::error!(error = %e, "каталог маркеров правок не создан");
                return ToolOutput::error(INTERNAL);
            }
        };
        let mut basis = false;
        if let Some(m) = load_marker(&marker_r)
            && m.path == r.path
            && m.args == self.args_hash
        {
            if before.hash.as_deref() == Some(m.after.as_str()) {
                self.tracker.mark(self.run, &r.path, &m.after);
                return m.output;
            }
            if before.hash != m.before {
                return ToolOutput::error(MARKER_CHANGED);
            }
            basis = true;
        }

        // Текст файла: BOM снимается и возвращается, не UTF-8 — отказ.
        let raw = before.bytes.as_deref().unwrap_or_default();
        let (bom, body) = match raw.strip_prefix(BOM) {
            Some(rest) => (true, rest),
            None => (false, raw),
        };
        let Ok(text) = std::str::from_utf8(body) else {
            return ToolOutput::error(NOT_UTF8);
        };
        let creating = before.bytes.is_none();
        if creating && !self.req.creates() {
            return not_found(&root, &r);
        }

        // 2. Трекер чтений.
        if !creating && !basis {
            let h = before.hash.as_deref().unwrap_or_default();
            match self.tracker.check(self.run, &r.path, h) {
                Ok(()) => {}
                Err(TrackError::NotRead) => return ToolOutput::error(NOT_READ),
                Err(TrackError::Changed) => return ToolOutput::error(CHANGED),
            }
        }

        // 3. Патчер; 4. защита от перезаписи.
        let mut after = text.to_owned();
        let mut level = MatchLevel::Exact;
        for (i, (old, new)) in self.req.edits.iter().enumerate() {
            match apply(&after, old, new) {
                Ok((next, l)) => {
                    after = next;
                    if l != MatchLevel::Exact && level == MatchLevel::Exact {
                        level = l;
                    }
                }
                Err(e) => return self.req.error(i, &patch_text(&e)),
            }
        }
        if let Err(e) = shrink_guard(text, &after, self.req.allow_shrink) {
            return ToolOutput::error(patch_text(&e));
        }
        let mut after_bytes = Vec::with_capacity(after.len() + 3);
        if bom {
            after_bytes.extend_from_slice(BOM);
        }
        after_bytes.extend_from_slice(after.as_bytes());
        if !creating && after_bytes == raw {
            return ToolOutput::error(NO_CHANGE);
        }
        let after_hash = hash_hex(&after_bytes);
        let output = render_output(
            &shown,
            text,
            &after,
            level,
            (before.hash.as_deref(), &after_hash),
            creating,
            &r.path,
        );

        // 5. Запись.
        if self.crash(EditPoint::BeforeWrite, &r.path) {
            return ToolOutput::error(INTERRUPTED);
        }
        let marker = Marker {
            v: MARKER_V,
            path: r.path.clone(),
            args: self.args_hash.clone(),
            before: before.hash.clone(),
            after: after_hash.clone(),
            output: output.clone(),
        };
        match self.write(&root, &r, &shown, &before, &after_bytes, &marker_r, &marker) {
            Ok(()) => {
                self.tracker.mark(self.run, &r.path, &after_hash);
                output
            }
            Err(o) => o,
        }
    }

    fn crash(&self, point: EditPoint, path: &Path) -> bool {
        self.hook.as_ref().is_some_and(|h| h(point, path))
    }

    /// Временный файл рядом с целью, маркер, сверка цели и `rename`.
    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        root: &Path,
        r: &ResolvedPath,
        shown: &str,
        before: &Before,
        bytes: &[u8],
        marker_r: &ResolvedPath,
        marker: &Marker,
    ) -> Result<(), ToolOutput> {
        let werr = |e: &io::Error| {
            if e.raw_os_error().is_some() {
                ToolOutput::error(write_error_text(shown, e.kind()))
            } else {
                ToolOutput::error(error_text(shown, e))
            }
        };
        if before.bytes.is_none() {
            create_parents_checked(r).map_err(|e| werr(&e))?;
        }
        let Some(parent) = r.path.parent() else {
            return Err(ToolOutput::error(INTERNAL));
        };
        let tmp = ResolvedPath {
            path: parent.join(format!(".za-edit-{}.tmp", self.stem)),
            inside: r.inside,
            protected: false,
        };
        write_tmp(&tmp, bytes, before.perms.clone()).map_err(|e| {
            discard(&tmp);
            werr(&e)
        })?;
        // Маркер — до `rename` (D88): сбой после замены даёт повтору `after`.
        let marker_json = match serde_json::to_vec(marker) {
            Ok(j) => j,
            Err(e) => {
                tracing::error!(error = %e, "маркер правки не сериализован");
                discard(&tmp);
                return Err(ToolOutput::error(INTERNAL));
            }
        };
        if let Err(e) = write_marker(marker_r, &self.stem, &marker_json) {
            tracing::error!(error = %e, path = %marker_r.path.display(), "маркер правки не записан");
            discard(&tmp);
            return Err(ToolOutput::error(INTERNAL));
        }
        if self.crash(EditPoint::BeforeRename, &r.path) {
            return Err(ToolOutput::error(INTERRUPTED));
        }
        // Цель не изменилась с проверок: та же сверка содержимого, что и у трекера.
        match read_before(root, r) {
            Ok(now) if now.hash == before.hash => {}
            Ok(_) => {
                discard(&tmp);
                return Err(ToolOutput::error(CHANGED_DURING));
            }
            Err(o) => {
                discard(&tmp);
                return Err(o);
            }
        }
        replace_checked(&tmp, r).map_err(|e| {
            discard(&tmp);
            werr(&e)
        })
    }
}

/// Файл до правки: байты (не больше [`READ_MAX_BYTES`]), хэш, права. Нет файла — пусто.
fn read_before(root: &Path, r: &ResolvedPath) -> Result<Before, ToolOutput> {
    let none = Before {
        bytes: None,
        hash: None,
        perms: None,
    };
    match stat_checked(r) {
        Ok(m) if m.is_dir() => {
            return Err(ToolOutput::error(directory_text(&display(root, &r.path))));
        }
        Ok(_) => {}
        Err(e) if is_missing(&e) => return Ok(none),
        Err(e) => return Err(open_error(root, r, &e)),
    }
    let f = match open_checked(r, false) {
        Ok(f) => f,
        Err(e) if is_missing(&e) => return Ok(none),
        Err(e) => return Err(open_error(root, r, &e)),
    };
    let io_err = |e: io::Error| open_error(root, r, &e);
    let meta = f.metadata().map_err(io_err)?;
    let mut bytes = Vec::new();
    (&f).take(READ_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io_err)?;
    if bytes.len() > READ_MAX_BYTES {
        return Err(ToolOutput::error(TOO_LARGE));
    }
    Ok(Before {
        hash: Some(hash_hex(&bytes)),
        bytes: Some(bytes),
        perms: Some(meta.permissions()),
    })
}

/// Временный файл: через [`open_checked`] (обычный файл, не ссылка, путь сверен),
/// усечение остатка прошлой попытки, байты, права цели, `fsync`.
fn write_tmp(tmp: &ResolvedPath, bytes: &[u8], perms: Option<Permissions>) -> io::Result<()> {
    let mut f = open_checked(tmp, true)?;
    f.set_len(0)?;
    f.write_all(bytes)?;
    if let Some(p) = perms {
        f.set_permissions(p)?;
    }
    f.sync_all()
}

/// Удаляет временный файл после отказа; не удалось — `warn`.
fn discard(tmp: &ResolvedPath) {
    match remove_checked(tmp) {
        Ok(()) => {}
        Err(e) if is_missing(&e) => {}
        Err(e) => {
            tracing::warn!(path = %tmp.path.display(), error = %e, "временный файл правки не удалён")
        }
    }
}

/// Маркер: атомарно, через временный файл в том же каталоге.
fn write_marker(marker: &ResolvedPath, stem: &str, json: &[u8]) -> io::Result<()> {
    let Some(dir) = marker.path.parent() else {
        return Err(io::Error::other("маркер без каталога"));
    };
    let tmp = ResolvedPath {
        path: dir.join(format!(".{stem}.json.tmp")),
        inside: true,
        protected: false,
    };
    let r = write_tmp(&tmp, json, None).and_then(|()| replace_checked(&tmp, marker));
    if r.is_err() {
        discard(&tmp);
    }
    r
}

/// Маркер повтора, если он есть и читается; испорченный — `warn` и `None`.
fn load_marker(marker: &ResolvedPath) -> Option<Marker> {
    let f = match open_checked(marker, false) {
        Ok(f) => f,
        Err(e) if is_missing(&e) => return None,
        Err(e) => {
            tracing::warn!(path = %marker.path.display(), error = %e, "маркер правки не открыт");
            return None;
        }
    };
    let mut bytes = Vec::new();
    if let Err(e) = (&f).take(MARKER_MAX).read_to_end(&mut bytes) {
        tracing::warn!(path = %marker.path.display(), error = %e, "маркер правки не прочитан");
        return None;
    }
    match serde_json::from_slice::<Marker>(&bytes) {
        Ok(m) if m.v == MARKER_V => Some(m),
        Ok(m) => {
            tracing::warn!(path = %marker.path.display(), v = m.v, "маркер правки другой версии");
            None
        }
        Err(e) => {
            tracing::warn!(path = %marker.path.display(), error = %e, "маркер правки испорчен");
            None
        }
    }
}

/// Результат (TOOL-003): заголовок, пометка уровня совпадения и унифицированный diff с
/// контекстом 2 (по тексту без BOM и с `\n`); постусловие — BLAKE3 файла после правки.
fn render_output(
    shown: &str,
    before: &str,
    after: &str,
    level: MatchLevel,
    hashes: (Option<&str>, &str),
    created: bool,
    path: &Path,
) -> ToolOutput {
    let (b, a) = (before.replace("\r\n", "\n"), after.replace("\r\n", "\n"));
    let diff = TextDiff::configure()
        .timeout(DIFF_TIMEOUT)
        .diff_lines(&b, &a);
    let (mut added, mut removed) = (0, 0);
    for c in diff.iter_all_changes() {
        match c.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    let verb = if created { "created" } else { "edited" };
    let mut content = header_text(
        verb,
        shown,
        added,
        removed,
        (short(hashes.0), short(Some(hashes.1))),
    );
    content.push('\n');
    let note = match level {
        MatchLevel::Exact => None,
        MatchLevel::LineEndings => Some(NOTE_LINE_ENDINGS),
        MatchLevel::TrailingSpace => Some(NOTE_TRAILING),
        MatchLevel::Indent => Some(NOTE_INDENT),
    };
    if let Some(n) = note {
        content.push_str(n);
        content.push('\n');
    }
    content.push_str(
        &diff
            .unified_diff()
            .context_radius(DIFF_CONTEXT)
            .missing_newline_hint(false)
            .to_string(),
    );
    ToolOutput {
        post: vec![Postcondition::FileHash {
            path: path.to_path_buf(),
            blake3: hashes.1.to_owned(),
        }],
        ..ToolOutput::ok(content)
    }
}
