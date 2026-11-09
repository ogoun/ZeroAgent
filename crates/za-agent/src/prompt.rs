//! Промпты приложения A (D21): тексты ролей и режимов, блоки `<env>`, `<task>`,
//! `<control>`, экранирование недоверенного текста (D23), лимиты (PRM-002, D22)
//! и уровень мышления по тиру (MDL-010, D20).
//!
//! Тексты — файлы `prompts/*.txt`, байт в байт из приложения A (LF, без завершающего
//! перевода строки). Идентификаторы, пути и дата попадают в промпт только через
//! шаблоны `<env>`, `<task>` и `<control>` (PRM-006).

use std::path::Path;

use serde::{Deserialize, Serialize};
use za_llm::{Effort, ReasoningMode};
use za_protocol::{RunId, RunRole, Tier};

/// Версия набора промптов; входит в `FrozenPrefix` и `model.request` (PRM-010).
pub const PROMPT_VERSION: &str = "r1b.1";

/// Лимит блока `<env>`, токены (PRM-002: «top small ≤ 800 + `<env>` ≤ 200»).
pub const ENV_LIMIT: u32 = 200;
/// Лимит overlay режима, токены (PRM-008: собственный слот).
pub const MODE_LIMIT: u32 = 500;

const TOP_CORE: &str = include_str!("../prompts/top_core.txt");
const ORCHESTRATOR: &str = include_str!("../prompts/orchestrator.txt");
const WORKER: &str = include_str!("../prompts/worker.txt");
const ENV: &str = include_str!("../prompts/env.txt");
const TASK: &str = include_str!("../prompts/task.txt");
const CONTROL: &str = include_str!("../prompts/control.txt");
const MODE_CODER: &str = include_str!("../prompts/mode_coder.txt");
const MODE_RESEARCHER: &str = include_str!("../prompts/mode_researcher.txt");
const MODE_OPERATOR: &str = include_str!("../prompts/mode_operator.txt");
const MODE_WRITER: &str = include_str!("../prompts/mode_writer.txt");
const UTILITY_RULES: &str = include_str!("../prompts/utility_rules.txt");

/// Все файлы `prompts/` в порядке имён: `(имя файла, текст)`.
const PROMPT_FILES: &[(&str, &str)] = &[
    ("control.txt", CONTROL),
    ("env.txt", ENV),
    ("mode_coder.txt", MODE_CODER),
    ("mode_operator.txt", MODE_OPERATOR),
    ("mode_researcher.txt", MODE_RESEARCHER),
    ("mode_writer.txt", MODE_WRITER),
    ("orchestrator.txt", ORCHESTRATOR),
    ("task.txt", TASK),
    ("top_core.txt", TOP_CORE),
    ("utility_rules.txt", UTILITY_RULES),
    ("worker.txt", WORKER),
];

/// Режим работы (overlay A.7, блок 4 промпта).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkMode {
    Coder,
    Researcher,
    Operator,
    Writer,
}

/// Core роли: A.1 (top), A.3 (orchestrator), A.4 (worker).
pub fn role_core(role: RunRole) -> &'static str {
    match role {
        RunRole::Top => TOP_CORE,
        RunRole::Orchestrator => ORCHESTRATOR,
        RunRole::Worker => WORKER,
    }
}

/// Overlay режима (A.7), начиная с `# Mode: …`.
pub fn mode_overlay(m: WorkMode) -> &'static str {
    match m {
        WorkMode::Coder => MODE_CODER,
        WorkMode::Researcher => MODE_RESEARCHER,
        WorkMode::Operator => MODE_OPERATOR,
        WorkMode::Writer => MODE_WRITER,
    }
}

/// Правила утилит N-09…N-11 (текст после A.4): передаются в `<task>`, только когда
/// задача создаёт утилиту.
pub fn utility_rules() -> &'static str {
    UTILITY_RULES
}

/// Все тексты `prompts/` в порядке имён файлов: `(имя файла, текст)`.
pub fn prompt_files() -> &'static [(&'static str, &'static str)] {
    PROMPT_FILES
}

/// BLAKE3 hex всех текстов `prompts/` в порядке имён файлов (PRM-010).
///
/// Формат: метка `za-prompts-v1\0`, затем для каждого файла длина (u64 LE) и байты
/// имени, длина и байты текста — граница между файлами однозначна.
pub fn prompt_texts_hash() -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"za-prompts-v1\0");
    for (name, text) in PROMPT_FILES {
        for part in [name.as_bytes(), text.as_bytes()] {
            h.update(&(part.len() as u64).to_le_bytes());
            h.update(part);
        }
    }
    h.finalize().to_hex().to_string()
}

/// Значения блока `<env>` (A.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnvInfo {
    pub os: String,
    pub shell: String,
    pub project_dir: String,
    /// `YYYY-MM-DD` по UTC (D24).
    pub date: String,
    pub tier: Tier,
    pub n_ctx: u32,
    pub budget: String,
    pub branch: String,
}

/// Блок `<env>` (A.2) с подстановкой значений.
pub fn render_env(e: &EnvInfo) -> String {
    let n_ctx = e.n_ctx.to_string();
    fill(
        ENV,
        &[
            ("os", &e.os),
            ("shell", &e.shell),
            ("project_dir", &e.project_dir),
            ("date", &e.date),
            ("tier", e.tier.as_str()),
            ("n_ctx", &n_ctx),
            ("budget", &e.budget),
            ("branch", &e.branch),
        ],
    )
}

/// Значения блока задания субагенту (A.5).
#[derive(Clone, Debug, PartialEq)]
pub struct TaskBlock {
    pub run_id: RunId,
    pub parent_id: RunId,
    pub goal: String,
    /// `{criteria}` шаблона.
    pub done_when: String,
    /// `{files_or_sources}` шаблона.
    pub scope: String,
    /// `{path}` шаблона.
    pub workspace: String,
    pub budget: String,
    /// `{minimal_handoff}` шаблона.
    pub handoff: String,
    /// `{zam_brief}` шаблона.
    pub memory: String,
    pub grants: String,
}

/// Блок `<task>` и `<grants>` (A.5) с подстановкой значений.
///
/// Значения не экранируются: их пишет рантайм или родитель, а не недоверенный источник.
pub fn render_task(t: &TaskBlock) -> String {
    let run_id = t.run_id.to_string();
    let parent_id = t.parent_id.to_string();
    fill(
        TASK,
        &[
            ("run_id", &run_id),
            ("parent_id", &parent_id),
            ("goal", &t.goal),
            ("criteria", &t.done_when),
            ("files_or_sources", &t.scope),
            ("path", &t.workspace),
            ("budget", &t.budget),
            ("minimal_handoff", &t.handoff),
            ("zam_brief", &t.memory),
            ("grants", &t.grants),
        ],
    )
}

/// Источник управляющего сообщения (атрибут `from`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ControlFrom {
    Runtime,
    Parent,
    User,
}

impl ControlFrom {
    fn as_str(self) -> &'static str {
        match self {
            ControlFrom::Runtime => "runtime",
            ControlFrom::Parent => "parent",
            ControlFrom::User => "user",
        }
    }
}

/// Действие управляющего сообщения (атрибут `action`, D23): `correct` и `resume` — A.6,
/// `nudge` и `reminder` — сообщения рантайма.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ControlAction {
    Correct,
    Resume,
    Nudge,
    Reminder,
}

impl ControlAction {
    fn as_str(self) -> &'static str {
        match self {
            ControlAction::Correct => "correct",
            ControlAction::Resume => "resume",
            ControlAction::Nudge => "nudge",
            ControlAction::Reminder => "reminder",
        }
    }
}

/// Управляющее user-сообщение (A.6, шаблон `prompts/control.txt`): открывающий тег,
/// `note` с новой строки, закрывающий тег с новой строки. `note` экранируется
/// [`escape_untrusted`] (D23); `command` — идентификатор рантайма (D36, hex).
pub fn render_control(
    from: ControlFrom,
    action: ControlAction,
    command: &str,
    note: &str,
) -> String {
    let note = escape_untrusted(note);
    fill(
        CONTROL,
        &[
            ("runtime|parent|user", from.as_str()),
            ("correct|resume", action.as_str()),
            ("command_id", command),
            ("note", &note),
        ],
    )
}

/// Экранирование управляющего тега в недоверенном тексте (D23).
///
/// Каждое `<`, за которым идут необязательные пробельные символы, необязательный `/`,
/// необязательные пробельные символы и `control` без учёта регистра ASCII, заменяется
/// на `&lt;`; остальной текст не меняется. Пробельные — по `char::is_whitespace`
/// (шире ASCII: лишнее экранирование безопасно, пропущенное — нет).
pub fn escape_untrusted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if opens_control(after) {
            out.push_str("&lt;");
        } else {
            out.push('<');
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Текст после `<` начинается с `[пробелы][/][пробелы]control` (регистр ASCII не важен).
fn opens_control(after: &str) -> bool {
    let t = after.trim_start();
    let t = t.strip_prefix('/').unwrap_or(t).trim_start();
    t.as_bytes()
        .get(..7)
        .is_some_and(|w| w.eq_ignore_ascii_case(b"control"))
}

/// Подстановка `{ключ}` за один проход: подставленные значения повторно не разбираются,
/// незнакомый `{…}` остаётся как есть.
fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open + 1..];
        let value = tail.find('}').and_then(|close| {
            let key = &tail[..close];
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v, close))
        });
        match value {
            Some((v, close)) => {
                out.push_str(v);
                rest = &tail[close + 1..];
            }
            None => {
                out.push('{');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Лимит блоков 2, 3 и 5 роли (PRM-002, D22), токены; без overlay режима
/// ([`MODE_LIMIT`]) и без `<env>` ([`ENV_LIMIT`]).
pub fn role_limit(role: RunRole, tier: Tier) -> u32 {
    match (role, tier) {
        (RunRole::Top, Tier::Small) => 800,
        (RunRole::Top, Tier::Standard) => 2000,
        (RunRole::Top, Tier::Full) => 3000,
        (RunRole::Orchestrator, _) => 500,
        (RunRole::Worker, _) => 300,
    }
}

/// Оценка рантайма: `ceil(байты UTF-8 / 4)` (D22). Точный счёт — CI (PRM-007).
pub fn approx_tokens(s: &str) -> u32 {
    u32::try_from(s.len().div_ceil(4)).unwrap_or(u32::MAX)
}

/// Мышление по тиру (MDL-010, D20): small — `Effort(Low)`, standard — `Effort(Medium)`,
/// full — `Default`.
pub fn reasoning_for(tier: Tier) -> ReasoningMode {
    match tier {
        Tier::Small => ReasoningMode::Effort(Effort::Low),
        Tier::Standard => ReasoningMode::Effort(Effort::Medium),
        Tier::Full => ReasoningMode::Default,
    }
}

/// Длина сокращённого sha отсоединённого `HEAD`.
const SHORT_SHA: usize = 12;

/// Ветка из `<dir>/.git/HEAD` без запуска `git` (D24): `ref: refs/heads/x` → `x`,
/// иначе первые 12 символов sha; нет файла или он пуст — `-`.
///
/// `.git`-файл рабочего дерева (`gitdir: <путь>`, `git worktree`) ведёт к каталогу
/// репозитория; относительный путь берётся от `project_dir`.
pub fn git_branch(project_dir: &Path) -> String {
    let dot_git = project_dir.join(".git");
    let git_dir = if dot_git.is_file() {
        match std::fs::read_to_string(&dot_git) {
            Ok(text) => match text.trim().strip_prefix("gitdir:") {
                Some(p) => project_dir.join(p.trim()),
                None => return "-".into(),
            },
            Err(e) => {
                tracing::debug!(error = %e, "не прочитан файл .git");
                return "-".into();
            }
        }
    } else {
        dot_git
    };
    let head = match std::fs::read_to_string(git_dir.join("HEAD")) {
        Ok(h) => h,
        Err(e) => {
            tracing::debug!(error = %e, "нет .git/HEAD");
            return "-".into();
        }
    };
    let head = head.trim();
    if let Some(r) = head.strip_prefix("ref:") {
        let r = r.trim();
        let name = r.strip_prefix("refs/heads/").unwrap_or(r);
        return if name.is_empty() {
            "-".into()
        } else {
            name.into()
        };
    }
    match head.char_indices().nth(SHORT_SHA) {
        Some((i, _)) => head[..i].into(),
        None if head.is_empty() => "-".into(),
        None => head.into(),
    }
}
