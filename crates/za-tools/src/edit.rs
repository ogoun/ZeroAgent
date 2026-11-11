//! `edit` и `multi_edit` (D88, таблица D74, D98; раздел 4.1 и таблица 3.4 ТЗ 02).
//!
//! Вызов: разбор аргументов → [`classify`] (сетевые и device-пути, ADS, имена устройств
//! отвергаются без обращения к ФС) → в `spawn_blocking`:
//! 1. [`resolve`] с `Access::Write`: путь канонический, ссылки и junction раскрыты.
//!    Защищённое место (D79: `ZA_HOME`, `.git/`, `.zero-agent/` целиком — включая
//!    рабочие файлы агента, Ruling 2) — `denied: protected path`. Путь вне проекта
//!    инструмент не отвергает: это решение политики (`Ask`, D77), инструмент сообщает его
//!    в [`Tool::subjects`] с `Access::Write`. Файл только для чтения (Windows) или без
//!    права записи владельца, или чужой (Unix) — отказ (I2): `rename` поверх него
//!    обошёл бы `chmod a-w` и сменил бы владельца.
//! 2. Маркер повтора `ZA_HOME/projects/<project_id>/edits/<file_stem>.json` ([`AgentHome`],
//!    m11): вне проекта, поэтому `bash` его не подделает. Каталог — через
//!    [`private_home_dir`] (на Unix 0700 при любом umask) и проверку
//!    [`za_jail::check_registry`]: принадлежит текущему пользователю и закрыт на запись
//!    группе и прочим. Файл уже равен `after` — результат маркера; равен `before` —
//!    правка продолжается (проверку трекера прерванная попытка уже прошла на этом
//!    содержимом, Ruling 1); иначе — ошибка. Отказ после записи маркера удаляет его,
//!    если файл не равен `after` (I3).
//! 3. [`ReadTracker`] по каноническому пути: правка только прочитанного и не изменившегося
//!    файла. Создание нового файла (`old` пуст, файла нет) её пропускает.
//! 4. Патчер [`crate::patch::apply`], для `multi_edit` — по очереди на копии.
//! 5. Защита от перезаписи [`shrink_guard`].
//! 6. Запись по каноническому пути: временный файл (`work_dir("edits")/<file_stem>.tmp`
//!    для цели в проекте, иначе или при другом томе — рядом с целью; [`open_checked`],
//!    на Unix 0600 до записи и права цели перед `rename`), маркер, сверка, что цель не
//!    изменилась, и [`replace_checked`] (`rename`, на Windows с повторами). Ссылка не
//!    подменяется файлом: пишется её цель. Остатки прошлых сбоев убирает
//!    [`clean_edit_leftovers`] при старте run (m3).
//!
//! BOM, стиль переводов строк и отсутствие `\n` в конце сохраняются. Файл не в UTF-8 и
//! файл больше [`READ_MAX_BYTES`] не правятся.

use std::fs::Permissions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use za_core::paths::{
    LexPath, ResolvedPath, Roots, classify, create_parents_checked, display, open_checked,
    private_home_dir, read_dir_checked, remove_checked, replace_checked, resolve, stat_checked,
    within, work_dir,
};
use za_core::{
    Access, Approval, BoxFuture, Postcondition, RawPath, RiskClass, Subjects, Tool, ToolCx,
    ToolKind, ToolOutput, ToolSchema, ToolSpec,
};
use za_protocol::{EffectClass, RunId, RunRole, Tier, deny_reason};

use crate::fs::{
    blocking, directory_text, error_text, invalid_args_text, is_missing, not_found, open_error,
    parse,
};
use crate::patch::{AMBIGUOUS_SHOWN, MatchLevel, PatchError, Stop, apply_until, shrink_guard};
use crate::tracker::{ReadTracker, TrackError};
use crate::{AgentHome, READ_MAX_BYTES};

/// Отметка порядка байт UTF-8.
const BOM: &[u8] = b"\xEF\xBB\xBF";
/// Служебный каталог проекта: целиком защищён от правки (см. модуль).
const ZA_DIR: &str = ".zero-agent";
/// Подкаталог временных файлов правки в `work_dir` и маркеров в `ZA_HOME/projects/<id>`.
const EDITS_DIR: &str = "edits";
/// Временный файл правки старше этого — сирота прошлого сбоя (m3): правка длится не
/// дольше тайм-аута вызова (30 с).
const LEFTOVER_AGE: Duration = Duration::from_secs(120);
/// Права нового файла до umask (N4), как у обычного создания файла.
#[cfg(unix)]
const NEW_FILE_MODE: u32 = 0o666;
/// umask, если его не удалось прочитать (`/proc/self/status` есть только на Linux).
#[cfg(unix)]
const DEFAULT_UMASK: u32 = 0o022;
/// Версия формата маркера.
const MARKER_V: u32 = 1;
/// Предел размера файла маркера при чтении.
const MARKER_MAX: u64 = 1 << 20;
/// Предел времени нечёткого поиска всех правок вызова (подсказка ближайшего участка).
const MATCH_BUDGET: Duration = Duration::from_secs(5);
/// Предел времени построения diff результата.
const DIFF_TIMEOUT: Duration = Duration::from_secs(2);
/// Контекст унифицированного diff (D88).
const DIFF_CONTEXT: usize = 2;

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

/// Неоднозначность: число мест (поиск останавливается на `AMBIGUOUS_SHOWN + 1`, тогда
/// «more than 10») и номера строк без повторов (N5: перекрытия в одной строке).
fn ambiguous_text(lines: &[usize]) -> String {
    let mut uniq: Vec<usize> = lines.iter().take(AMBIGUOUS_SHOWN).copied().collect();
    uniq.dedup();
    let mut shown: Vec<String> = uniq.iter().map(ToString::to_string).collect();
    let count = if lines.len() > AMBIGUOUS_SHOWN {
        shown.push("...".to_owned());
        format!("more than {AMBIGUOUS_SHOWN}")
    } else {
        lines.len().to_string()
    };
    format!(
        "old matches {count} places (lines {}); add surrounding lines",
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
    home: AgentHome,
    multi: bool,
    hook: Option<EditHook>,
}

/// Инструмент `edit(path, old, new, allow_shrink?)`.
pub fn edit_tool(tracker: Arc<ReadTracker>, home: AgentHome) -> Arc<dyn Tool> {
    build(tracker, home, false, None)
}

/// Инструмент `multi_edit(path, edits[{old, new}], allow_shrink?)`.
pub fn multi_edit_tool(tracker: Arc<ReadTracker>, home: AgentHome) -> Arc<dyn Tool> {
    build(tracker, home, true, None)
}

/// [`edit_tool`] с крюком сбоя. Только для тестов (m12), как `CrashHook` и `FaultHook`.
pub fn edit_tool_with_hook(
    tracker: Arc<ReadTracker>,
    home: AgentHome,
    hook: EditHook,
) -> Arc<dyn Tool> {
    build(tracker, home, false, Some(hook))
}

/// [`multi_edit_tool`] с крюком сбоя. Только для тестов (m12).
pub fn multi_edit_tool_with_hook(
    tracker: Arc<ReadTracker>,
    home: AgentHome,
    hook: EditHook,
) -> Arc<dyn Tool> {
    build(tracker, home, true, Some(hook))
}

fn build(
    tracker: Arc<ReadTracker>,
    home: AgentHome,
    multi: bool,
    hook: Option<EditHook>,
) -> Arc<dyn Tool> {
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
        home,
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
        let home = self.home.clone();
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
                home,
                hook,
                cancel: Arc::default(),
            };
            // Бросок future (тайм-аут вызова) останавливает нечёткий поиск в
            // `spawn_blocking` (N1): сам поток пула не прерывается.
            let guard = CancelOnDrop(job.cancel.clone());
            let out = blocking(move || job.run(&lex)).await;
            drop(guard);
            out
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
    home: AgentHome,
    hook: Option<EditHook>,
    /// Отмена вызова: ставится при броске future.
    cancel: Arc<AtomicBool>,
}

/// Ставит флаг отмены при броске future вызова.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Файл до правки.
struct Before {
    /// Сырые байты; `None` — файла нет.
    bytes: Option<Vec<u8>>,
    hash: Option<String>,
    perms: Option<Permissions>,
    /// Можно ли заменить файл (I2): не только для чтения, на Unix — право записи
    /// владельца и владелец — текущий пользователь.
    writable: bool,
    /// Группа файла (Unix): замена её сохраняет (N3).
    gid: Option<u32>,
}

fn hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn short(h: Option<&str>) -> &str {
    h.map_or("none", |h| &h[..8.min(h.len())])
}

impl Job {
    /// Правка; в текстах отказов абсолютный канонический путь цели заменяется путём для
    /// модели (m5): внутри проекта — относительным.
    fn run(self, lex: &LexPath) -> ToolOutput {
        let mut shown_abs: Option<(String, String)> = None;
        let mut out = self.run_inner(lex, &mut shown_abs);
        if out.is_error
            && let Some((abs, shown)) = shown_abs
            && abs != shown
        {
            out.content = out.content.replace(&abs, &shown);
        }
        out
    }

    fn run_inner(&self, lex: &LexPath, shown_abs: &mut Option<(String, String)>) -> ToolOutput {
        let roots = match Roots::canonical(&self.project, &self.home.za_home) {
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
        // Весь `.zero-agent` проекта, включая рабочие файлы агента (Ruling 2).
        if r.protected || within(&root.join(ZA_DIR), &r.path) {
            return ToolOutput::error(denied_text());
        }
        let shown = display(&root, &r.path);
        *shown_abs = Some((r.path.display().to_string(), shown.clone()));
        // Остаток прерванной попытки этого же вызова (m3).
        for tmp in self.tmp_candidates(&root, &r) {
            discard(&tmp);
        }
        let before = match read_before(&root, &r) {
            Ok(b) => b,
            Err(o) => return o,
        };
        if before.bytes.is_some() && !before.writable {
            return ToolOutput::error(write_error_text(&shown, io::ErrorKind::PermissionDenied));
        }

        // 1. Маркер повтора.
        let marker_r = match self.marker_path(&roots.za_home) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(error = %e, "каталог маркеров правок недоступен");
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
        let stop = Stop::after(MATCH_BUDGET).with_cancel(self.cancel.clone());
        for (i, (old, new)) in self.req.edits.iter().enumerate() {
            match apply_until(&after, old, new, &stop) {
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
        let target = Target {
            root: &root,
            r: &r,
            shown: &shown,
        };
        match self.write(&target, &before, &after_bytes, &marker_r, &marker) {
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

    /// Маркер `ZA_HOME/projects/<project_id>/edits/<file_stem>.json` (m11). Каталог —
    /// [`private_home_dir`] от канонического `ZA_HOME`, на Unix — свой и закрытый
    /// ([`za_jail::check_registry`]).
    fn marker_path(&self, za_home: &Path) -> io::Result<ResolvedPath> {
        let dir = private_home_dir(
            za_home,
            &format!("projects/{}/{EDITS_DIR}", self.home.project_id),
        )?;
        za_jail::check_registry(&dir)?;
        Ok(ResolvedPath {
            path: dir.join(format!("{}.json", self.stem)),
            inside: false,
            protected: false,
        })
    }

    /// Места временного файла по порядку: `work_dir("edits")` для цели в проекте (m3),
    /// затем рядом с целью (цель вне проекта или на другом томе).
    fn tmp_candidates(&self, root: &Path, r: &ResolvedPath) -> Vec<ResolvedPath> {
        let mut out = Vec::new();
        if r.inside {
            match work_dir(root, EDITS_DIR) {
                Ok(dir) => out.push(ResolvedPath {
                    path: dir.join(format!("{}.tmp", self.stem)),
                    inside: true,
                    protected: false,
                }),
                Err(e) => tracing::warn!(error = %e, "каталог временных файлов правки недоступен"),
            }
        }
        if let Some(parent) = r.path.parent() {
            out.push(ResolvedPath {
                path: parent.join(format!(".za-edit-{}.tmp", self.stem)),
                inside: r.inside,
                protected: false,
            });
        }
        out
    }

    /// Временный файл, маркер, сверка цели и `rename`. Отказ после записи маркера
    /// удаляет маркер, если файл не равен `after` (I3).
    fn write(
        &self,
        t: &Target<'_>,
        before: &Before,
        bytes: &[u8],
        marker_r: &ResolvedPath,
        marker: &Marker,
    ) -> Result<(), ToolOutput> {
        let werr = |e: &io::Error| {
            if e.raw_os_error().is_some() && e.kind() != io::ErrorKind::CrossesDevices {
                ToolOutput::error(write_error_text(t.shown, e.kind()))
            } else {
                ToolOutput::error(error_text(t.shown, e))
            }
        };
        if before.bytes.is_none() {
            create_parents_checked(t.r).map_err(|e| werr(&e))?;
        }
        let candidates = self.tmp_candidates(t.root, t.r);
        let Some(first) = candidates.first() else {
            return Err(ToolOutput::error(INTERNAL));
        };
        let perms = target_perms(before);
        write_tmp(first, bytes, perms.clone(), before.gid).map_err(|e| {
            discard(first);
            werr(&e)
        })?;
        // Маркер — до `rename` (D88): сбой после замены даёт повтору `after`.
        let marker_json = match serde_json::to_vec(marker) {
            Ok(j) => j,
            Err(e) => {
                tracing::error!(error = %e, "маркер правки не сериализован");
                discard(first);
                return Err(ToolOutput::error(INTERNAL));
            }
        };
        if let Err(e) = write_marker(marker_r, &self.stem, &marker_json) {
            tracing::error!(error = %e, path = %marker_r.path.display(), "маркер правки не записан");
            discard(first);
            return Err(ToolOutput::error(INTERNAL));
        }
        if self.crash(EditPoint::BeforeRename, &t.r.path) {
            return Err(ToolOutput::error(INTERRUPTED));
        }
        let fail = |o: ToolOutput, tmp: &ResolvedPath| {
            discard(tmp);
            drop_marker(marker_r, t.r, &marker.after);
            Err(o)
        };
        // Цель не изменилась с проверок: та же сверка содержимого, что и у трекера.
        match read_before(t.root, t.r) {
            Ok(now) if now.hash == before.hash => {}
            Ok(_) => return fail(ToolOutput::error(CHANGED_DURING), first),
            Err(o) => return fail(o, first),
        }
        let mut tmp = first;
        loop {
            match replace_checked(tmp, t.r) {
                Ok(()) => return Ok(()),
                // Другой том (в проект смонтирован другой том): временный файл — рядом.
                Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                    let Some(next) = candidates.iter().skip_while(|c| c.path != tmp.path).nth(1)
                    else {
                        return fail(werr(&e), tmp);
                    };
                    discard(tmp);
                    if let Err(e) = write_tmp(next, bytes, perms.clone(), before.gid) {
                        return fail(werr(&e), next);
                    }
                    tmp = next;
                }
                Err(e) => return fail(werr(&e), tmp),
            }
        }
    }
}

/// Цель правки для записи.
struct Target<'a> {
    root: &'a Path,
    r: &'a ResolvedPath,
    shown: &'a str,
}

/// Права файла после замены: права цели, а у нового файла на Unix — 0666 с учётом umask
/// процесса (N4).
fn target_perms(before: &Before) -> Option<Permissions> {
    if let Some(p) = &before.perms {
        return Some(p.clone());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(Permissions::from_mode(NEW_FILE_MODE & !umask()))
    }
    #[cfg(not(unix))]
    None
}

/// umask процесса (Unix), прочитанный один раз: `Umask:` из `/proc/self/status` (Linux).
/// `umask(2)` не годится: он меняет маску всего процесса, пока её читают. Нет `/proc` —
/// [`DEFAULT_UMASK`] (записанное ограничение прочих Unix).
#[cfg(unix)]
pub(crate) fn umask() -> u32 {
    static MASK: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MASK.get_or_init(|| {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("Umask:"))
                    .and_then(|v| u32::from_str_radix(v.trim(), 8).ok())
            })
            .unwrap_or(DEFAULT_UMASK)
    })
}

/// Удаляет маркер после отказа (I3), если файл не равен `after`: `rename` не состоялся,
/// и повтор не должен счесть правку начатой. Файл уже равен `after` (сбой сверки после
/// `rename`) — маркер остаётся: повтор вернёт его результат.
fn drop_marker(marker: &ResolvedPath, r: &ResolvedPath, after: &str) {
    let current = open_checked(r, false).ok().and_then(|f| {
        let mut bytes = Vec::new();
        (&f).take(READ_MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()
            .map(|_| hash_hex(&bytes))
    });
    if current.as_deref() == Some(after) {
        return;
    }
    match remove_checked(marker) {
        Ok(()) => {}
        Err(e) if is_missing(&e) => {}
        Err(e) => {
            tracing::warn!(path = %marker.path.display(), error = %e, "маркер правки не удалён")
        }
    }
}

/// Файл можно заменить (I2): Windows — не только для чтения; Unix — право записи
/// владельца и владелец — текущий пользователь.
fn writable(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mode() & 0o200 != 0 && meta.uid() == nix::unistd::geteuid().as_raw()
    }
    #[cfg(not(unix))]
    {
        !meta.permissions().readonly()
    }
}

/// Группа файла: Unix — `st_gid`, прочие ОС — нет.
fn file_gid(meta: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.gid())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// Файл до правки: байты (не больше [`READ_MAX_BYTES`]), хэш, права. Нет файла — пусто.
fn read_before(root: &Path, r: &ResolvedPath) -> Result<Before, ToolOutput> {
    let none = Before {
        bytes: None,
        hash: None,
        perms: None,
        writable: true,
        gid: None,
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
        writable: writable(&meta),
        gid: file_gid(&meta),
        perms: Some(meta.permissions()),
    })
}

/// Временный файл. На Unix он сначала создаётся с правами 0600 (`create_new`,
/// `O_NOFOLLOW`; уже есть — остаток прошлой попытки, N2), затем открывается через
/// [`open_checked`] (обычный файл, не ссылка, одна жёсткая ссылка, путь сверен).
/// Остаток усекается, пишутся байты, `fsync`, группа цели (N3, неудача — `warn`) и права
/// цели — до `rename`.
fn write_tmp(
    tmp: &ResolvedPath,
    bytes: &[u8],
    perms: Option<Permissions>,
    gid: Option<u32>,
) -> io::Result<()> {
    #[cfg(unix)]
    za_core::paths::create_private_file(&tmp.path)?;
    let mut f = open_checked(tmp, true)?;
    f.set_len(0)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    #[cfg(unix)]
    if let Some(g) = gid
        && let Err(e) = std::os::unix::fs::fchown(&f, None, Some(g))
    {
        tracing::warn!(path = %tmp.path.display(), gid = g, error = %e, "группа файла правки не сохранена");
    }
    #[cfg(not(unix))]
    let _ = gid;
    if let Some(p) = perms {
        f.set_permissions(p)?;
    }
    Ok(())
}

/// Удаляет временный файл; нет его — ничего, не удалось — `warn`.
fn discard(tmp: &ResolvedPath) {
    match remove_checked(tmp) {
        Ok(()) => {}
        Err(e) if is_missing(&e) => {}
        Err(e) => {
            tracing::warn!(path = %tmp.path.display(), error = %e, "временный файл правки не удалён")
        }
    }
}

/// Маркер: атомарно, через временный файл в том же каталоге (0600 на Unix).
fn write_marker(marker: &ResolvedPath, stem: &str, json: &[u8]) -> io::Result<()> {
    let Some(dir) = marker.path.parent() else {
        return Err(io::Error::other("маркер без каталога"));
    };
    let tmp = ResolvedPath {
        path: dir.join(format!(".{stem}.json.tmp")),
        inside: false,
        protected: false,
    };
    let r = write_tmp(&tmp, json, None, None).and_then(|()| replace_checked(&tmp, marker));
    if r.is_err() {
        discard(&tmp);
    }
    r
}

/// Маркер повтора, если он есть и читается; испорченный или чужой — `warn` и `None`.
fn load_marker(marker: &ResolvedPath) -> Option<Marker> {
    let f = match open_checked(marker, false) {
        Ok(f) => f,
        Err(e) if is_missing(&e) => return None,
        Err(e) => {
            tracing::warn!(path = %marker.path.display(), error = %e, "маркер правки не открыт");
            return None;
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let me = nix::unistd::geteuid().as_raw();
        if !f.metadata().is_ok_and(|m| m.uid() == me) {
            tracing::warn!(path = %marker.path.display(), "маркер правки чужой, пропущен");
            return None;
        }
    }
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

/// Убирает временные файлы правок — сироты прошлых сбоев (m3) — из
/// `<project>/.zero-agent/work/edits`: только `*.tmp` старше двух минут (живая правка
/// другого run того же проекта длится не дольше тайм-аута вызова). Блокирующая.
/// Вызывается при старте run (подключение — задача 12). Возвращает число удалённых.
/// Временные файлы рядом с целью вне проекта так не находятся; их убирает повтор того же
/// вызова.
pub fn clean_edit_leftovers(project: &Path) -> io::Result<usize> {
    clean_leftovers_older(project, LEFTOVER_AGE)
}

/// [`clean_edit_leftovers`] с заданным возрастом сироты (для тестов).
#[doc(hidden)]
pub fn clean_leftovers_older(project: &Path, age: Duration) -> io::Result<usize> {
    let _drives = za_core::paths::DriveScope::enter();
    let roots = Roots::canonical(project, project)?;
    let dir_r = ResolvedPath {
        path: work_dir(&roots.project, EDITS_DIR)?,
        inside: true,
        protected: false,
    };
    let mut n = 0;
    for name in read_dir_checked(&dir_r)? {
        let name = name.to_string_lossy().into_owned();
        if !name.ends_with(".tmp") {
            continue;
        }
        let f = ResolvedPath {
            path: dir_r.path.join(&name),
            inside: true,
            protected: false,
        };
        let old = stat_checked(&f)
            .ok()
            .filter(std::fs::Metadata::is_file)
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|a| a >= age);
        if old {
            match remove_checked(&f) {
                Ok(()) => n += 1,
                Err(e) => {
                    tracing::warn!(path = %f.path.display(), error = %e, "сирота правки не удалена")
                }
            }
        }
    }
    Ok(n)
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
