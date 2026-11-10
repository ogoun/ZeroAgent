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
pub const PROMPT_VERSION: &str = "r1b.2";

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

/// Постоянные тексты рантайма, которые уходят модели, в порядке имён: `(имя, текст)`.
///
/// Рубрика компакции, nudge, тексты результатов-отказов и неизменные части
/// указателей и сводки (решение контроллера по Carry задачи 3 R1-B2). Входят в
/// [`prompt_texts_hash`]: их правка видна как смена версии промптов (PRM-010). Лимита
/// токенов у них нет — в приложении A его нет. Описания встроенных инструментов сюда
/// не входят: они в префиксе (`BlockHashes::tools`).
const RUNTIME_TEXTS: &[(&str, &str)] = &[
    ("runtime/compact.clipped", crate::context::CLIPPED),
    (
        "runtime/compact.no_user_messages",
        crate::context::NO_USER_MESSAGES,
    ),
    ("runtime/compact.recitation", crate::context::RECITATION),
    (
        "runtime/compact.recite_in_tail",
        crate::context::RECITE_IN_TAIL,
    ),
    ("runtime/compact.rubric", crate::context::COMPACT_RUBRIC),
    (
        "runtime/conversation.interrupted",
        crate::conversation::INTERRUPTED,
    ),
    ("runtime/detect.cycle_nudge", crate::detect::CYCLE_NUDGE),
    ("runtime/detect.promise_nudge", crate::detect::PROMISE_NUDGE),
    ("runtime/detect.repeat_mark", crate::detect::REPEAT_MARK),
    ("runtime/detect.repeat_nudge", crate::detect::REPEAT_NUDGE),
    ("runtime/detect.repeat_reject", crate::detect::REPEAT_REJECT),
    (
        "runtime/driver.empty_nudge_subagent",
        crate::driver::EMPTY_NUDGE_SUBAGENT,
    ),
    (
        "runtime/driver.empty_nudge_top",
        crate::driver::EMPTY_NUDGE_TOP,
    ),
    (
        "runtime/driver.truncated_nudge",
        crate::driver::TRUNCATED_NUDGE,
    ),
    ("runtime/spill.not_saved", crate::context::SPILL_NOT_SAVED),
    ("runtime/spill.omitted", crate::context::SPILL_OMITTED),
    ("runtime/spill.search", crate::context::SPILL_SEARCH),
    ("runtime/tools.after_finish", crate::tools::AFTER_FINISH),
    ("runtime/tools.finish_ok", crate::tools::FINISH_OK),
    ("runtime/tools.journal_failed", crate::tools::JOURNAL_FAILED),
    ("runtime/tools.output_lost", crate::tools::OUTPUT_LOST),
    ("runtime/turn.blocked", crate::turn::BLOCKED),
    ("runtime/turn.length_cut", crate::turn::LENGTH_CUT),
    (
        "runtime/turn.length_cut_nudge",
        crate::turn::LENGTH_CUT_NUDGE,
    ),
    ("runtime/turn.mutation_nudge", crate::turn::MUTATION_NUDGE),
];

/// Тексты рантайма, уходящие модели, `(имя, текст)` в порядке имён: постоянные
/// ([`RUNTIME_TEXTS`]) и образцы текстов, собираемых `format!`, — результат их функций
/// рендера с фиксированными аргументами (ошибки инструментов, отказы нормализации,
/// разметка сводки компакции; M-1 ревью задачи 5). Правка шаблона меняет образец, а
/// значит и [`prompt_texts_hash`].
pub fn runtime_texts() -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = RUNTIME_TEXTS
        .iter()
        .map(|(n, t)| (*n, (*t).to_owned()))
        .chain(crate::context::compact_text_samples())
        .chain(crate::tools::text_samples())
        .chain(crate::turn::text_samples())
        .collect();
    out.sort_by(|a, b| a.0.cmp(b.0));
    out
}

/// BLAKE3 hex всех текстов, уходящих модели без подстановки (PRM-010): файлы
/// `prompts/` в порядке имён, затем [`runtime_texts`].
///
/// Формат: метка `za-prompts-v2\0`, затем для каждого текста длина (u64 LE) и байты
/// имени, длина и байты текста — граница между текстами однозначна, имена файлов
/// (`*.txt`) и текстов рантайма (`runtime/…`) не пересекаются. В `v1` (R1-B1) текстов
/// рантайма не было.
pub fn prompt_texts_hash() -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"za-prompts-v2\0");
    let runtime = runtime_texts();
    let files = PROMPT_FILES.iter().map(|(n, t)| (*n, *t));
    for (name, text) in files.chain(runtime.iter().map(|(n, t)| (*n, t.as_str()))) {
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
///
/// Весь блок гарантированно укладывается в [`ENV_LIMIT`] по верхней оценке
/// [`upper_tokens`] (байты): если поля не помещаются, самые длинные из них
/// обрезаются до общего потолка с середины (`начало…конец`, маркер `…`), короткие не
/// трогаются.
pub fn render_env(e: &EnvInfo) -> String {
    let n_ctx = e.n_ctx.to_string();
    let branch: String = e.branch.chars().take(BRANCH_MAX_CHARS).collect();
    let mut esc =
        [&e.os, &e.shell, &e.project_dir, &e.date, &e.budget, &branch].map(|v| escape_untrusted(v));
    let render = |v: &[String; 6]| {
        let [os, shell, project_dir, date, budget, branch] = v;
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
    };
    let full = render(&esc);
    let limit = ENV_LIMIT as usize;
    if full.len() <= limit {
        return full;
    }
    // Место под поля: лимит минус разметка шаблона с пустыми полями.
    let overhead = render(&Default::default()).len();
    let avail = limit.saturating_sub(overhead);
    // Наибольший общий потолок `cap`, при котором сумма min(len, cap) помещается.
    let lens = esc.each_ref().map(String::len);
    let fits = |cap: usize| lens.iter().map(|l| (*l).min(cap)).sum::<usize>() <= avail;
    let (mut lo, mut hi) = (0usize, lens.iter().copied().max().unwrap_or(0));
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fits(mid) { lo = mid } else { hi = mid - 1 }
    }
    for v in &mut esc {
        if v.len() > lo {
            *v = elide_middle(v, lo);
        }
    }
    let out = render(&esc);
    tracing::debug!(bytes = full.len(), "поля <env> обрезаны до лимита");
    out
}

/// Маркер обрезки середины значения.
const ELLIPSIS: &str = "…";

/// `s` не длиннее `max` байт: начало, [`ELLIPSIS`], конец (по границам символов).
/// Если места меньше маркера — только начало.
fn elide_middle(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let floor = |n: usize| (0..=n).rev().find(|i| s.is_char_boundary(*i)).unwrap_or(0);
    if max < ELLIPSIS.len() + 2 {
        return s[..floor(max)].to_string();
    }
    let room = max - ELLIPSIS.len();
    let head = floor(room / 2);
    let tail_len = room - head;
    let tail = (s.len() - tail_len..=s.len())
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(s.len());
    format!("{}{ELLIPSIS}{}", &s[..head], &s[tail..])
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
/// `nudge`, `reminder` и `compact` (рубрика сводки компакции, R1-B2 D46) — сообщения
/// рантайма.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ControlAction {
    Correct,
    Resume,
    Nudge,
    Reminder,
    Compact,
}

impl ControlAction {
    fn as_str(self) -> &'static str {
        match self {
            ControlAction::Correct => "correct",
            ControlAction::Resume => "resume",
            ControlAction::Nudge => "nudge",
            ControlAction::Reminder => "reminder",
            ControlAction::Compact => "compact",
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
    render_control_trusted(from, action, command, &escape_untrusted(note))
}

/// Как [`render_control`], но `note` не экранируется: вызывающий сам экранировал в нём
/// недоверенные части. Нужен блоку `<state>` сообщения `resume` (задача 15, A.6:
/// «resume means continue from <state>»): тег `<state>` — разметка рантайма, а имена и
/// аргументы вызовов внутри блока экранированы [`escape_untrusted`].
pub(crate) fn render_control_trusted(
    from: ControlFrom,
    action: ControlAction,
    command: &str,
    note: &str,
) -> String {
    let command = attr_id(command);
    fill(
        CONTROL,
        &[
            ("runtime|parent|user", from.as_str()),
            ("correct|resume", action.as_str()),
            ("command_id", &command),
            ("note", note),
        ],
    )
}

/// Символы, похожие на `<`: ASCII, одиночная кавычка-уголок, угловые скобки
/// (технические, математические, CJK, орнаментальные), малая и полноширинная формы,
/// гомоглифы (канадское слоговое PA, модификаторы-стрелки ˂ и ˱, «не меньше» ≮) и
/// невидимый тег-символ
/// U+E003C (ASCII smuggling).
pub const LEFT_ANGLES: &[char] = &[
    '<',
    '\u{02C2}',
    '\u{02F1}',
    '\u{1438}',
    '\u{226E}',
    '\u{2039}',
    '\u{2329}',
    '\u{276C}',
    '\u{276E}',
    '\u{2770}',
    '\u{27E8}',
    '\u{27EA}',
    '\u{27EC}',
    '\u{2991}',
    '\u{2993}',
    '\u{2995}',
    '\u{29FC}',
    '\u{3008}',
    '\u{300A}',
    '\u{FE3F}',
    '\u{FE64}',
    '\u{FF1C}',
    '\u{E003C}',
];
/// Символы, похожие на `>` — пары к [`LEFT_ANGLES`].
pub const RIGHT_ANGLES: &[char] = &[
    '>',
    '\u{02C3}',
    '\u{02F2}',
    '\u{1433}',
    '\u{226F}',
    '\u{203A}',
    '\u{232A}',
    '\u{276D}',
    '\u{276F}',
    '\u{2771}',
    '\u{27E9}',
    '\u{27EB}',
    '\u{27ED}',
    '\u{2992}',
    '\u{2994}',
    '\u{2996}',
    '\u{29FD}',
    '\u{3009}',
    '\u{300B}',
    '\u{FE40}',
    '\u{FE65}',
    '\u{FF1E}',
    '\u{E003E}',
];

/// Экранирование разметки промпта в недоверенном тексте (D23, PRM-011, решение
/// контроллера, раунд 2).
///
/// Каждый символ из [`LEFT_ANGLES`] заменяется на `&lt;`, из [`RIGHT_ANGLES`] — на `&gt;`,
/// независимо от того, что за ним: обходы распознавания имени тега (невидимые символы
/// внутри имени, гомоглифы, полноширинные буквы, `ſ`) теряют смысл, потому что без
/// угловой скобки тега нет. Остальной текст не меняется, `&` не трогается, поэтому
/// функция идемпотентна. Цена — `Vec<String>` в выводе инструмента читается как
/// `Vec&lt;String&gt;`; кавычки «» не трогаются.
pub fn escape_untrusted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        // Быстрый путь: из ASCII в списках только `<` и `>`.
        if c.is_ascii() && c != '<' && c != '>' {
            out.push(c);
        } else if LEFT_ANGLES.contains(&c) {
            out.push_str("&lt;");
        } else if RIGHT_ANGLES.contains(&c) {
            out.push_str("&gt;");
        } else {
            out.push(c);
        }
    }
    out
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

/// Слот описаний инструментов в префиксе top small, токены: строка 8k таблицы 2.3
/// части 02 (D64).
const SMALL_TOOLS_SLOT: u32 = 550;

/// Случай CI-проверки лимита промптов (PRM-007, D64): имя, текст и лимит в токенах.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetCase {
    pub name: String,
    pub text: String,
    pub limit: u32,
}

/// `<env>` с максимальной подстановкой (PRM-007): длинные ОС, оболочка, путь и ветка,
/// окно 1M и бюджет со всеми пятью пределами на максимумах их типов.
pub fn max_env() -> EnvInfo {
    let budget = za_protocol::Budget {
        steps: Some(u32::MAX),
        tokens: Some(u64::MAX),
        gpu_ms: Some(u64::MAX),
        usd: Some(f64::MAX),
        duration_ms: Some(u64::MAX),
    };
    EnvInfo {
        os: "Windows 11 Pro for Workstations 10.0.26300 x86_64".into(),
        shell: "powershell 5.1.26100.1".into(),
        project_dir: format!("H:/Documents/GitHub/{}", "x".repeat(100)),
        date: "2026-11-09".into(),
        tier: Tier::Standard,
        n_ctx: 1_048_576,
        budget: crate::driver::budget_text(&budget),
        branch: "b".repeat(BRANCH_MAX_CHARS),
    }
}

/// Все режимы с их именами в случаях [`budget_cases`].
const MODES: [(WorkMode, &str); 4] = [
    (WorkMode::Coder, "coder"),
    (WorkMode::Researcher, "researcher"),
    (WorkMode::Operator, "operator"),
    (WorkMode::Writer, "writer"),
];

/// Блоки 2–5 так, как они стоят в system-сообщении: Core и overlay (адаптер пуст,
/// PRM-009; `AGENTS.md` — недоверенные данные, считается в рантайме [`upper_tokens`]).
fn system_of(role: RunRole, overlay: Option<WorkMode>) -> String {
    za_llm::render_system(&za_llm::PromptBlocks {
        core: role_core(role).to_owned(),
        overlay: overlay.map(mode_overlay).unwrap_or_default().to_owned(),
        ..Default::default()
    })
}

/// Случаи CI-лимита (D64), лимиты — [`role_limit`], [`ENV_LIMIT`], [`MODE_LIMIT`]:
/// - `top/small/core` — Core top (800); `env/max` — `render_env(max_env())` (200,
///   регрессия: [`render_env`] и так режет блок до 200 байт);
/// - `mode/{coder,researcher,operator,writer}` — overlay (500);
/// - `top/{standard,full}` и `top/{standard,full}/{mode}` — system (Core и overlay),
///   `"\n\n"` и `render_env(max_env())` (2000, 3000);
/// - `orchestrator`, `orchestrator/{mode}` (500), `worker`, `worker/{mode}` (300) — Core
///   вместе с overlay;
/// - `top/small/prefix/{mode}` — Core top, overlay режима и схема `finish`
///   (`render_tools`), лимит 800 + 500 + 550 = 1850 (Core, слот overlay, Tools строки 8k
///   таблицы 2.3). Случай на каждый overlay, а не на самый длинный: самый длинный по
///   токенам у разных токенизаторов может различаться (M-3 ревью задачи 5). Схема
///   `finish` — заглушка слота Tools: top small встроенных инструментов не получает
///   (`finish` — у worker и orchestrator, `ask_user` — со standard), а настоящих
///   инструментов small ещё нет; когда появятся — брать набор реестра
///   `for_run(Top, Small)` (M-2).
pub fn budget_cases() -> Vec<BudgetCase> {
    let case = |name: String, text: String, limit: u32| BudgetCase { name, text, limit };
    let env = render_env(&max_env());
    let mut out = vec![
        case(
            "top/small/core".into(),
            role_core(RunRole::Top).to_owned(),
            role_limit(RunRole::Top, Tier::Small),
        ),
        case("env/max".into(), env.clone(), ENV_LIMIT),
    ];
    for (m, name) in MODES {
        out.push(case(
            format!("mode/{name}"),
            mode_overlay(m).to_owned(),
            MODE_LIMIT,
        ));
    }
    let overlays = std::iter::once((None, String::new()))
        .chain(MODES.map(|(m, name)| (Some(m), format!("/{name}"))))
        .collect::<Vec<_>>();
    for (tier, tier_name) in [(Tier::Standard, "standard"), (Tier::Full, "full")] {
        for (m, suffix) in &overlays {
            out.push(case(
                format!("top/{tier_name}{suffix}"),
                format!("{}\n\n{env}", system_of(RunRole::Top, *m)),
                role_limit(RunRole::Top, tier),
            ));
        }
    }
    for (role, role_name) in [
        (RunRole::Orchestrator, "orchestrator"),
        (RunRole::Worker, "worker"),
    ] {
        for (m, suffix) in &overlays {
            out.push(case(
                format!("{role_name}{suffix}"),
                system_of(role, *m),
                role_limit(role, Tier::Small),
            ));
        }
    }
    let tools = crate::tools::builtin_tools()
        .iter()
        .filter(|t| t.spec().min_tier <= Tier::Small)
        .map(|t| t.spec().schema.clone())
        .collect();
    let tools = za_llm::render_tools(&za_llm::PromptBlocks {
        tools,
        ..Default::default()
    });
    for (m, name) in MODES {
        out.push(case(
            format!("top/small/prefix/{name}"),
            format!("{}\n\n{tools}", system_of(RunRole::Top, Some(m))),
            role_limit(RunRole::Top, Tier::Small) + MODE_LIMIT + SMALL_TOOLS_SLOT,
        ));
    }
    out
}

/// Предел блока `AGENTS.md`, токены: «`AGENTS.md` и резерв ≤ 700» распределения Full
/// (часть 02, §4); на младших тирах — тот же потолок, остаток лимита обычно меньше.
pub const AGENTS_MD_LIMIT: u32 = 700;

/// Бюджет `AGENTS.md` (блок 5), токены: остаток [`role_limit`] после блоков 2–4
/// (`fixed_tokens`) и, если `<env>` входит в лимит, после резерва [`ENV_LIMIT`], но не
/// больше [`AGENTS_MD_LIMIT`]. Резерв, а не фактический `<env>`, — чтобы обрезка
/// `AGENTS.md` (блок 5, общий префикс) не зависела от ветки и пути. Orchestrator и
/// worker `AGENTS.md` не получают (N-07) — 0.
///
/// `fixed_tokens` — счёт фиксированных текстов (CI-счётчик плана B2 или завышающая для
/// английского [`approx_tokens`]); результат расходуется в верхней оценке
/// [`upper_tokens`] ([`fit_agents_md`]), поэтому реальный счёт `AGENTS.md` его не превысит.
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

/// `AGENTS.md` в пределах `budget` токенов по гарантированной верхней оценке
/// [`upper_tokens`] (D22, D23, решение контроллера: недоверенный текст не считается
/// байтами / 4 — на коде и hex это занижает в 1,6–3,8 раза): текст
/// экранируется [`escape_untrusted`]; не помещается — обрезается по строкам с конца,
/// последняя строка `[AGENTS.md truncated: N lines omitted]` входит в бюджет. Не
/// помещается и она (или текст пуст) — `None`, блок опускается целиком.
pub fn fit_agents_md(text: &str, budget: u32) -> Option<String> {
    let text = escape_untrusted(text.trim_end());
    if text.is_empty() {
        return None;
    }
    if upper_tokens(&text) <= budget {
        return Some(text);
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let total = lines.len();
    let marker = |omitted: usize| format!("[AGENTS.md truncated: {omitted} lines omitted]");
    let fits = |len: usize| len <= budget as usize;
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

/// Гарантированная верхняя оценка числа токенов недоверенного или переменного текста:
/// число байт UTF-8. Токен byte-level BPE (cl100k, o200k, Qwen, Llama 3) не короче
/// одного байта, поэтому ни один такой токенизатор не даст больше. Ею считаются
/// `AGENTS.md`, `<env>` и прочие данные, которые CI не видит.
pub fn upper_tokens(s: &str) -> u32 {
    u32::try_from(s.len()).unwrap_or(u32::MAX)
}

/// Оценка рантайма: `ceil(байты UTF-8 / 4)` (D22) — только для планирования с запасом
/// по фиксированным текстам (на английском завышает). Точный счёт — CI (PRM-007).
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
