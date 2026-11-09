//! Промпты приложения A (D21): тексты ролей и режимов, блоки `<env>`, `<task>`,
//! `<control>`, экранирование недоверенного текста (D23), лимиты (PRM-001, PRM-002,
//! часть 02, §4), бюджет `AGENTS.md` (D22)
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
///
/// Строковые поля экранируются [`escape_untrusted`]: ветка и путь приходят из
/// недоверенного репозитория (ARC-041), git допускает `<` в именах ссылок. Ветка
/// обрезается до [`BRANCH_MAX_CHARS`] символов.
pub fn render_env(e: &EnvInfo) -> String {
    let n_ctx = e.n_ctx.to_string();
    let branch: String = e.branch.chars().take(BRANCH_MAX_CHARS).collect();
    let esc =
        [&e.os, &e.shell, &e.project_dir, &e.date, &e.budget, &branch].map(|v| escape_untrusted(v));
    let [os, shell, project_dir, date, budget, branch] = &esc;
    fill(
        ENV,
        &[
            ("os", os),
            ("shell", shell),
            ("project_dir", project_dir),
            ("date", date),
            ("tier", e.tier.as_str()),
            ("n_ctx", &n_ctx),
            ("budget", budget),
            ("branch", branch),
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
/// Все строковые поля экранируются [`escape_untrusted`]: цель, критерии и handoff пишет
/// модель-родитель, читавшая недоверенные данные, память — данные (MEM-006). Поддельные
/// `</task>`, `<grants>`, `<control>` в значениях не становятся разметкой.
pub fn render_task(t: &TaskBlock) -> String {
    let run_id = t.run_id.to_string();
    let parent_id = t.parent_id.to_string();
    let esc = [
        &t.goal,
        &t.done_when,
        &t.scope,
        &t.workspace,
        &t.budget,
        &t.handoff,
        &t.memory,
        &t.grants,
    ]
    .map(|v| escape_untrusted(v));
    let [goal, criteria, scope, path, budget, handoff, memory, grants] = &esc;
    fill(
        TASK,
        &[
            ("run_id", &run_id),
            ("parent_id", &parent_id),
            ("goal", goal),
            ("criteria", criteria),
            ("files_or_sources", scope),
            ("path", path),
            ("budget", budget),
            ("minimal_handoff", handoff),
            ("zam_brief", memory),
            ("grants", grants),
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
/// [`escape_untrusted`] (D23); `command` — идентификатор рантайма (D36, hex), символы
/// вне `[A-Za-z0-9_-]` заменяются на `_`.
pub fn render_control(
    from: ControlFrom,
    action: ControlAction,
    command: &str,
    note: &str,
) -> String {
    let note = escape_untrusted(note);
    let command = attr_id(command);
    fill(
        CONTROL,
        &[
            ("runtime|parent|user", from.as_str()),
            ("correct|resume", action.as_str()),
            ("command_id", &command),
            ("note", &note),
        ],
    )
}

/// Имена тегов разметки промпта, которые недоверенный текст не может открыть или
/// закрыть (D23; `memory` — обёртка MEM-006 и снимка первого сообщения, `state` — resume).
pub const RESERVED_TAGS: &[&str] = &["control", "task", "grants", "env", "memory", "state"];

/// Экранирование тегов разметки промпта в недоверенном тексте (D23, PRM-011).
///
/// Каждое `<`, за которым идут пробельные или невидимые символы и `/` в любом порядке
/// и количестве, а затем имя из [`RESERVED_TAGS`] без учёта регистра ASCII, заменяется
/// на `&lt;`; остальной текст не меняется. Совпадение по префиксу (`<controller`,
/// `<environment` тоже экранируются): лишнее экранирование безопасно, пропущенное — нет.
/// Пробельные — `char::is_whitespace`, невидимые — нулевой ширины, мягкий перенос,
/// управление направлением письма и BOM. Функция идемпотентна.
pub fn escape_untrusted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if opens_reserved(after) {
            out.push_str("&lt;");
        } else {
            out.push('<');
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Символ без видимого начертания, который модель может пропустить при чтении тега.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Текст после `<` — `[пробелы, невидимые, /]*` и имя зарезервированного тега.
fn opens_reserved(after: &str) -> bool {
    let t = after.trim_start_matches(|c: char| c == '/' || c.is_whitespace() || is_invisible(c));
    RESERVED_TAGS.iter().any(|tag| {
        t.as_bytes()
            .get(..tag.len())
            .is_some_and(|w| w.eq_ignore_ascii_case(tag.as_bytes()))
    })
}

/// Длина имени ветки в `<env>`, символы: длинное имя иначе раздувало бы блок сверх
/// [`ENV_LIMIT`].
pub const BRANCH_MAX_CHARS: usize = 64;

/// Идентификатор команды в атрибуте `command`: только ASCII-буквы, цифры, `-` и `_`
/// (D36 даёт hex); остальные символы заменяются на `_`, чтобы кавычка не закрыла атрибут.
fn attr_id(s: &str) -> String {
    let clean: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean != s {
        tracing::warn!("идентификатор команды с недопустимыми символами заменён");
    }
    clean
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

/// Лимит системного промпта роли (PRM-001, PRM-002), токены: блоки 2–5 раздела 2.1
/// части 02 — Core с блоками tier и инструкциями capability, адаптер, overlay режима,
/// `AGENTS.md`.
///
/// Top standard и full считаются вместе с `<env>` (часть 02, §4: «Лимит 3000 считается
/// по блокам 2–5 раздела 2.1 плюс `<env>`»; для standard — так же, консервативно). Top
/// small — без `<env>`: у него свой слот [`ENV_LIMIT`] (PRM-002: «top small ≤ 800 +
/// `<env>` ≤ 200»), см. [`env_in_role_limit`]. Overlay ограничен ещё и своим слотом
/// [`MODE_LIMIT`] (PRM-008). Orchestrator 500 и worker 300 — блоки 2–5 при любом тире.
///
/// Соблюдение лимита фиксированными текстами проверяет CI-счётчик токенизаторами
/// (PRM-007, план B2); [`approx_tokens`] в рантайме — только планирование с запасом.
pub fn role_limit(role: RunRole, tier: Tier) -> u32 {
    match (role, tier) {
        (RunRole::Top, Tier::Small) => 800,
        (RunRole::Top, Tier::Standard) => 2000,
        (RunRole::Top, Tier::Full) => 3000,
        (RunRole::Orchestrator, _) => 500,
        (RunRole::Worker, _) => 300,
    }
}

/// Входит ли `<env>` в [`role_limit`]: да у top standard и full, нет у top small
/// (отдельный слот [`ENV_LIMIT`]) и у orchestrator и worker (`<env>` — данные первого
/// сообщения, как `<task>`).
pub fn env_in_role_limit(role: RunRole, tier: Tier) -> bool {
    role == RunRole::Top && tier != Tier::Small
}

/// Предел блока `AGENTS.md`, токены: «`AGENTS.md` и резерв ≤ 700» распределения Full
/// (часть 02, §4); на младших тирах — тот же потолок, остаток лимита обычно меньше.
pub const AGENTS_MD_LIMIT: u32 = 700;

/// Бюджет `AGENTS.md` (блок 5), токены: остаток [`role_limit`] после блоков 2–4
/// (`fixed_tokens`) и, если `<env>` входит в лимит, после резерва [`ENV_LIMIT`], но не
/// больше [`AGENTS_MD_LIMIT`]. Резерв, а не фактический `<env>`, — чтобы обрезка
/// `AGENTS.md` (блок 5, общий префикс) не зависела от ветки и пути. Orchestrator и
/// worker `AGENTS.md` не получают (N-07) — 0.
pub fn agents_md_budget(role: RunRole, tier: Tier, fixed_tokens: u32) -> u32 {
    if role != RunRole::Top {
        return 0;
    }
    let env = if env_in_role_limit(role, tier) {
        ENV_LIMIT
    } else {
        0
    };
    role_limit(role, tier)
        .saturating_sub(fixed_tokens)
        .saturating_sub(env)
        .min(AGENTS_MD_LIMIT)
}

/// `AGENTS.md` в пределах `budget` токенов ([`approx_tokens`]) (D22, D23): текст
/// экранируется [`escape_untrusted`]; не помещается — обрезается по строкам с конца,
/// последняя строка `[AGENTS.md truncated: N lines omitted]` входит в бюджет. Не
/// помещается и она (или текст пуст) — `None`, блок опускается целиком.
pub fn fit_agents_md(text: &str, budget: u32) -> Option<String> {
    let text = escape_untrusted(text.trim_end());
    if text.is_empty() {
        return None;
    }
    if approx_tokens(&text) <= budget {
        return Some(text);
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let total = lines.len();
    let marker = |omitted: usize| format!("[AGENTS.md truncated: {omitted} lines omitted]");
    let fits = |len: usize| len.div_ceil(4) <= budget as usize;
    // Длина растёт с k не убывая: строка добавляет ≥ 1 байт, маркер теряет ≤ 1 цифру.
    let mut kept = None;
    let mut kept_len = 0usize;
    for (k, line) in lines.iter().enumerate() {
        let len = if k == 0 {
            marker(total).len()
        } else {
            kept_len + 1 + marker(total - k).len()
        };
        if !fits(len) {
            break;
        }
        kept = Some(k);
        kept_len += line.len() + usize::from(k > 0);
    }
    let Some(k) = kept else {
        tracing::warn!(budget, "AGENTS.md не помещается в лимит промпта и опущен");
        return None;
    };
    let mut out = lines[..k].join("\n");
    if k > 0 {
        out.push('\n');
    }
    out.push_str(&marker(total - k));
    Some(out)
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
            name.chars().take(BRANCH_MAX_CHARS).collect()
        };
    }
    match head.char_indices().nth(SHORT_SHA) {
        Some((i, _)) => head[..i].into(),
        None if head.is_empty() => "-".into(),
        None => head.into(),
    }
}
