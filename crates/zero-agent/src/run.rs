//! Команды `run` и `answer` (R1-B1): цикл агента поверх `Gateway` и журнала проекта.
//!
//! `run` создаёт run (`za_agent::start_run`) или продолжает его (`--resume`,
//! `za_agent::resume_run`). Первая строка stderr — `run <id>`; текст ответа идёт в stdout
//! по событиям `model.delta` шины (D35), в конце — перевод строки. Ctrl+C — пауза run
//! (`RunControl::pause`, D26), второй Ctrl+C — выход с кодом 4 без ожидания паузы.
//!
//! Коды выхода (D28): 0 — `done` или `partial`; 1 — ошибка или `failed`; 3 — нужно
//! решение (`needs_input`, `needs_approval`, `needs_decision`), в stderr строка
//! `zero-agent: ответьте: zero-agent answer --interrupt <id> …`; 4 — прочая пауза или
//! `blocked_on_budget`; 5 — `blocked`; 6 — `cancelled`. Аварийное завершение (`abort`)
//! даёт на Windows `0xC0000409` (`__fastfail`), на Unix — SIGABRT; признак настоящего
//! «нужно решение» — строка `ответьте:`.
//!
//! `answer` открывает журнал, записывает решение прерывания и выходит; цикл продолжает
//! `run --resume`. Ответ на одобрение (`appr-…`) и на доверие (`trust-…`) закрывает
//! прерывание и пишет аудит одной транзакцией (`resolve_with_audit`, R1-C1 D91):
//! `approval.resolved`, а при доверии ещё `trust.granted` с набором доверенных файлов на
//! момент ответа (D83).
//!
//! R1-C1 (задача 13): `run` подключает нативные инструменты (`za_tools::native_capability`),
//! чекпоинты, политику (глобальная, у доверенного проекта — и проектная), корни проекта,
//! маску секретов и набор доверия. До цикла: проверка доверия ([`crate::trust::trust_check`]),
//! уборка остатков прошлых сбоев, `policy.loaded`, гранты (`--grant`, `[[policy.grants]]`);
//! у доверенного проекта — `AGENTS.md` или `CLAUDE.md`. Resume сверяет якорь аудита из
//! снимка и передаёт отличия доверия, политики и грантов через `ResumeChange`. Пауза
//! `needs_trust` — код 3.
//!
//! R1-C2 (задача 14): `run` строит сетевой шлюз один раз на процесс и регистрирует
//! `web_fetch` и `web_search` ([`crate::web`]; веб не выключен глобально или проектом,
//! поиск — только при `[web.search] url`), подключает извлекатель в подпроцессе
//! ([`crate::extract::RunExtractor`]: PDF в `read` и `web_fetch`, HTML у `web_fetch`) и
//! каталог скиллов ([`crate::skills::catalog`] после проверки доверия; пустой каталог —
//! без capability `skills` и `activate_skill`, m7 ревью задачи 13). Resume регистрирует
//! веб-инструменты, только если они есть в замороженном префиксе run (новых инструментов
//! посреди run нет, а выключенный сейчас веб не включается); веб-инструмент префикса,
//! выключенный конфигурацией после старта, снимается с исполнения: вызов отвечает модели
//! `<имя>: web tools are disabled by configuration since this run started` (Ruling «Задача
//! 15»), в stderr — строка о выключенном вебе. `policy.loaded` получает
//! `web_disabled` и `search_host` и сверяется с действующим снимком у нового run и у
//! каждого resume (I1 ревью задачи 14). Печать: строки `web.request` (D112), `skill.activated`
//! (D122) и предупреждение о тире standard на окне меньше 32k (D169).
//!
//! R1-M (задача 1): корни run знают домашний каталог пользователя (`Roots::with_home` по
//! [`crate::home::user_home`]) — `ZA_HOME` и `~/.zeroaimemory/` защищены от любого доступа
//! инструментов модели (D157); проект внутри защищённого места — [`RunError::SealedProject`].

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use za_agent::{
    AuditHead, EnvInfo, Grant, GrantMode, GrantSource, LoopDeps, LoopError, LoopOutcome,
    PolicyError, PolicySnapshot, ResumeChange, RuleOfTwoNotice, RunSpec, TrustSetFn, WorkMode,
    builtin_tools, control_channel, git_branch, resume_run, skills_capability, start_run,
};
use za_core::extract::Extractor;
use za_core::paths::{self, Roots};
use za_core::{
    Access, AgentEvent, EventBus, InterruptKind, Journal, Tool, ToolRegistry, UnknownDecision,
    resolve_unknown,
};
use za_jail::DirectRunner;
use za_llm::{EndpointId, SelectError, Selection};
use za_protocol::{
    Actor, Budget, BudgetKind, EventKind, Priority, RunId, RunMode, RunRole, RunStatus, Seq, Tier,
    audit_action, pause_reason,
};
use za_secrets::SecretMasker;
use za_store::{AuditNew, INTERRUPT_OPEN, InterruptRow, ProjectStore, StoreError};
use za_tools::{
    AgentHome, GitCheckpointer, NativeDeps, ReadTracker, SecretFilter, Shell, native_capability,
};

use crate::config::{
    AgentConfig, PROJECT_CONFIG, ProjectConfig, load_config, parse_project_config,
};
use crate::extract::RunExtractor;
use crate::home;
use crate::journal::open_retrying;
use crate::models::{CommandError, PROBE_WAIT, start_gateway, state_text, wait_endpoint};
use crate::trust::{
    TrustSet, current_set, project_instructions, read_trusted, trust_check, trust_set,
};

/// Код выхода: `done` или `partial`.
pub const EXIT_DONE: u8 = 0;
/// Код выхода: ошибка или `failed`.
pub const EXIT_ERROR: u8 = 1;
/// Код выхода: нужно решение пользователя (`needs_input`, `needs_approval`, `needs_decision`).
pub const EXIT_NEEDS_DECISION: u8 = 3;
/// Код выхода: прочая пауза или `blocked_on_budget`.
pub const EXIT_PAUSED: u8 = 4;
/// Код выхода: `blocked`.
pub const EXIT_BLOCKED: u8 = 5;
/// Код выхода: `cancelled`.
pub const EXIT_CANCELLED: u8 = 6;
/// Имя файла журнала в каталоге журнала проекта.
pub const JOURNAL_FILE: &str = "journal.db";
/// Поле payload прерывания доверия с хэшем показанного набора (`TrustSet::hash`).
pub const TRUST_SET_HASH: &str = "set_hash";
/// Разделитель в id повторного прерывания доверия (`<база>-set<хэш>`).
const TRUST_REASK_MARK: &str = "-set";
/// Actor записей аудита, которые пишет решение пользователя в `answer`.
const AUDIT_ACTOR_USER: &str = "user";
/// Actor записей аудита, которые пишет бинарник сам (`policy.loaded`, гранты `--grant`).
const AUDIT_ACTOR_RUNTIME: &str = "runtime";
/// Длина аргументов вызова в подсказке одобрения, символы (D96).
const ARGS_SHOWN_CHARS: usize = 400;

/// Ошибка команд `run` и `answer`.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Loop(#[from] LoopError),
    #[error("{what} {}: {source}", path.display())]
    Io {
        what: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("журнал проекта {} не найден: в нём ещё не было run", .0.display())]
    NoJournal(PathBuf),
    #[error("прерывание {0} не найдено")]
    UnknownInterrupt(String),
    #[error("набор доверенных файлов проекта {}: {source}", path.display())]
    TrustSet {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("прерывание {id} вида {kind}: {hint}")]
    WrongAnswer {
        id: String,
        kind: String,
        hint: &'static str,
    },
    /// Политика: режим `bypass` без `allow_bypass`, неверное правило (D80).
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// Путь гранта (`--grant`, `[[policy.grants]]`) не разрешается (D78).
    #[error("грант {path}: {reason}")]
    Grant { path: String, reason: String },
    /// Запись аудита из снимка run не найдена в цепочке (D91, m1 ревью задачи 4).
    #[error(
        "журнал аудита проекта изменён: записи {seq}, сохранённой при прошлом запуске run, нет в цепочке; проверка: zero-agent audit --verify"
    )]
    AuditAnchor { seq: u64 },
    /// Сетевой шлюз не построен (D107): неверные настройки `[web]`.
    #[error("веб: {0}")]
    Web(#[from] za_web::WebError),
    /// Каталог проекта внутри защищённого места (R1-M D157): `ZA_HOME` или
    /// `~/.zeroaimemory/` — там инструменты агента не работают.
    #[error(
        "каталог проекта {} внутри защищённого места (ZA_HOME или ~/.zeroaimemory): инструменты агента туда не обращаются; запустите run в другом каталоге",
        display_path(.0)
    )]
    SealedProject(PathBuf),
}

/// Точка аварийного завершения `--crash-at` (D33; только для тестов).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunCrash {
    /// `std::process::abort()` на первом `model.delta` хода N (ходы с 1).
    AfterDelta(u32),
    /// `std::process::abort()` внутри слота вызова инструмента шага N (шаги с 1) после
    /// самого вызова: `tool.started` и чекпоинт записаны, инструмент отработал,
    /// квитанции и `tool.finished` нет (R1-C1 D92, `CrashPoint::InTool`). Resume видит
    /// намерение без квитанции: класс `NonIdempotent` даёт прерывание `unk-…`.
    InTool(u64),
    /// `std::process::abort()` после записи эффекта границы шага N (шаги с 1), до снимка
    /// шага (`CrashPoint::AfterBoundary`): квитанции вызовов шага N − 1 записаны, а снимка
    /// после них нет. Resume реплеит шаг N − 1 по квитанциям — без повторного вызова
    /// (R1-C2, приёмка: реплей `web_fetch` без сети, D112).
    AfterBoundary(u64),
}

/// Разбор `--crash-at after-delta:N`, `in-tool:N` и `after-boundary:N`.
pub fn parse_run_crash(s: &str) -> Result<RunCrash, String> {
    let bad = || format!("ожидается after-delta:N, in-tool:N или after-boundary:N, получено {s:?}");
    let (kind, n) = s.split_once(':').ok_or_else(bad)?;
    let n: u32 = n.trim().parse().map_err(|_| bad())?;
    match kind.trim() {
        "after-delta" if n >= 1 => Ok(RunCrash::AfterDelta(n)),
        "in-tool" if n >= 1 => Ok(RunCrash::InTool(u64::from(n))),
        "after-boundary" if n >= 1 => Ok(RunCrash::AfterBoundary(u64::from(n))),
        _ => Err(bad()),
    }
}

/// Крюк сбоя цикла для `--crash-at in-tool:N` и `after-boundary:N`: настоящее аварийное
/// завершение процесса (эквивалент `kill -9`) в точке `CrashPoint::InTool { step: N,
/// after_call: true }` или `CrashPoint::AfterBoundary { step: N }`.
fn crash_hook(crash: Option<RunCrash>) -> Option<za_agent::CrashHook> {
    let at = match crash? {
        RunCrash::InTool(step) => za_agent::CrashPoint::InTool {
            step,
            after_call: true,
        },
        RunCrash::AfterBoundary(step) => za_agent::CrashPoint::AfterBoundary { step },
        RunCrash::AfterDelta(_) => return None,
    };
    Some(Arc::new(move |p| {
        if p == at {
            std::process::abort();
        }
        false
    }))
}

/// Аргументы нового run.
#[derive(Debug, Clone)]
pub struct NewRunArgs {
    pub model: Option<String>,
    pub run_mode: RunMode,
    pub work_mode: Option<WorkMode>,
    pub tier: Option<Tier>,
    pub max_wait: Option<Duration>,
    pub task: String,
}

/// Аргументы `run --resume`.
#[derive(Debug, Clone)]
pub struct ResumeArgs {
    pub run: RunId,
    pub model: Option<String>,
    pub input: Option<String>,
}

/// Что делает `run`.
#[derive(Debug, Clone)]
pub enum RunAction {
    New(NewRunArgs),
    Resume(ResumeArgs),
}

/// Аргументы `run`.
#[derive(Debug, Clone)]
pub struct RunArgs {
    pub project: PathBuf,
    pub action: RunAction,
    pub budget: BudgetFlags,
    /// `--grant <путь>[=ro|=rw]` (D78): у нового run и у resume (добавляются с границы).
    pub grants: Vec<(PathBuf, GrantMode)>,
    pub crash: Option<RunCrash>,
    /// Предел чекпоинта git при вызове инструмента вместо `CHECKPOINT_TIMEOUT` (скрытый
    /// `--checkpoint-timeout`, только для тестов под нагрузкой: × `ZA_TEST_TIME_SCALE`).
    pub checkpoint_timeout: Option<Duration>,
    /// Тестовые подмены (D128); без признака `test-hooks` — пусто.
    pub hooks: RunHooks,
}

/// Тестовые подмены `run` (D128): поля существуют только с признаком `test-hooks`, обычная
/// и release-сборка флага `--web-resolve` не знают.
#[derive(Debug, Clone, Default)]
pub struct RunHooks {
    /// `--web-resolve host=addr`: имя разрешается шлюзом в заданный адрес мимо фильтра
    /// (`WebGateway::with_test_resolve`).
    #[cfg(feature = "test-hooks")]
    pub web_resolve: Vec<(String, std::net::SocketAddr)>,
}

/// Разбор `--web-resolve host=addr` (D128): `addr` — `ip:port`.
#[cfg(feature = "test-hooks")]
pub fn parse_web_resolve(s: &str) -> Result<(String, std::net::SocketAddr), String> {
    let (host, addr) = s
        .split_once('=')
        .ok_or_else(|| format!("ожидается host=ip:port, получено {s:?}"))?;
    let addr = addr
        .trim()
        .parse()
        .map_err(|e| format!("адрес {addr:?}: {e}"))?;
    Ok((host.trim().to_owned(), addr))
}

/// Флаги бюджета `run`: каждый задаёт своё поле поверх базового бюджета.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BudgetFlags {
    /// `--max-steps`.
    pub steps: Option<u32>,
    /// `--max-tokens`.
    pub tokens: Option<u64>,
    /// `--max-usd`.
    pub usd: Option<f64>,
    /// `--max-gpu-ms`.
    pub gpu_ms: Option<u64>,
    /// `--max-duration` в секундах; в бюджете — `duration_ms`.
    pub duration_secs: Option<u64>,
}

impl BudgetFlags {
    /// Задан хотя бы один флаг.
    pub fn any(&self) -> bool {
        self.steps.is_some()
            || self.tokens.is_some()
            || self.usd.is_some()
            || self.gpu_ms.is_some()
            || self.duration_secs.is_some()
    }
}

/// Флаг `run`, меняющий предел вида `kind` (подсказка при `blocked_on_budget`).
pub fn budget_flag(kind: BudgetKind) -> &'static str {
    match kind {
        BudgetKind::Steps => "--max-steps N",
        BudgetKind::Tokens => "--max-tokens N",
        BudgetKind::Usd => "--max-usd USD",
        BudgetKind::GpuMs => "--max-gpu-ms МС",
        BudgetKind::Duration => "--max-duration СЕК",
    }
}

/// Разбор `--max-usd`: конечное неотрицательное число (NaN снял бы предел молча, как в
/// проверке `[budgets.run]`).
pub fn parse_usd(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("ожидается число долларов, получено {s:?}"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(format!(
            "предел usd должен быть конечным и не отрицательным: {s:?}"
        ));
    }
    Ok(v)
}

/// Ответ на прерывание (`answer`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Ответ на `ask_user` (`input-…`).
    Text(String),
    /// Одобрение вызова (`appr-…`) или доверие к проекту (`trust-…`).
    Approve,
    /// Отказ в вызове (`appr-…`) или в доверии (`trust-…`) с необязательной заметкой.
    Deny(Option<String>),
    /// Неизвестный исход (`unk-…`): действие выполнено.
    Done,
    /// Неизвестный исход (`unk-…`): действие не выполнено.
    NotDone,
}

/// Бюджет `base` (у нового run — `[budgets.run]`, у resume — текущий бюджет run), поверх
/// него заданные флаги; незаданные поля остаются из `base`.
pub fn run_budget(base: &Budget, f: &BudgetFlags) -> Budget {
    let mut b = base.clone();
    if f.steps.is_some() {
        b.steps = f.steps;
    }
    if f.tokens.is_some() {
        b.tokens = f.tokens;
    }
    if f.usd.is_some() {
        b.usd = f.usd;
    }
    if f.gpu_ms.is_some() {
        b.gpu_ms = f.gpu_ms;
    }
    if let Some(s) = f.duration_secs {
        b.duration_ms = Some(s.saturating_mul(1000));
    }
    b
}

/// Текущий бюджет run — поле `budget` последнего снимка; снимка нет — без пределов
/// (`resume_run` такой run всё равно не продолжит).
async fn current_budget(store: &ProjectStore, run: RunId) -> Result<Budget, RunError> {
    let Some((_, v)) = store.latest_snapshot(run).await? else {
        return Ok(Budget::default());
    };
    match v.get("budget") {
        Some(b) => serde_json::from_value(b.clone())
            .map_err(|e| StoreError::Corrupt(format!("бюджет в снимке run {run}: {e}")).into()),
        None => Ok(Budget::default()),
    }
}

/// Путь для человека и модели: без префикса `\\?\` канонического пути Windows
/// (`\\?\C:\x` → `C:\x`, `\\?\UNC\srv\share` → `\\srv\share`).
pub fn display_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        s.into_owned()
    }
}

/// Канонический путь проекта без префикса `\\?\` (`display_path`): им пользуются
/// `<env>`, подсказки и `RunSpec.project_dir`; `project_id` канонизирует путь сам.
pub(crate) fn canonical_project(p: &Path) -> Result<PathBuf, RunError> {
    std::fs::canonicalize(p)
        .map(|c| PathBuf::from(display_path(&c)))
        .map_err(|source| RunError::Io {
            what: "каталог проекта",
            path: p.to_path_buf(),
            source,
        })
}

/// Каталог журнала проекта в `ZA_HOME`.
fn journal_dir(home: &Path, project: &Path) -> Result<PathBuf, RunError> {
    home::journal_dir(home, project).map_err(|source| RunError::Io {
        what: "каталог проекта",
        path: project.to_path_buf(),
        source,
    })
}

/// Журнал существующего проекта: наличие `journal.db` проверяется до открытия, иначе
/// `ProjectStore::open` создал бы пустой журнал. Открытие — с повтором на `Locked`
/// (Windows снимает блокировку убитого процесса не сразу).
pub(crate) fn open_existing(home: &Path, project: &Path) -> Result<ProjectStore, RunError> {
    Ok(open_retrying(&existing_journal(home, project)?)?)
}

/// Каталог журнала проекта, если `journal.db` в нём есть; иначе [`RunError::NoJournal`].
fn existing_journal(home: &Path, project: &Path) -> Result<PathBuf, RunError> {
    let dir = journal_dir(home, project)?;
    if !dir.join(JOURNAL_FILE).is_file() {
        return Err(RunError::NoJournal(dir));
    }
    Ok(dir)
}

/// Оболочка для `<env>`: `powershell` на Windows, иначе имя из `$SHELL` без пути или `sh`.
fn shell_name() -> String {
    if cfg!(windows) {
        return "powershell".into();
    }
    std::env::var("SHELL")
        .ok()
        .and_then(|s| {
            Path::new(s.trim())
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sh".into())
}

/// Сегодняшняя дата по UTC, `YYYY-MM-DD` (D24).
fn today_utc() -> String {
    jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .strftime("%Y-%m-%d")
        .to_string()
}

/// `<env>` нового run. `tier`, `n_ctx` и `budget` перезаписывает `start_run` (D37).
/// Блокирующий ввод-вывод (`git_branch`): вызывать через [`blocking`].
fn env_info(project: &Path) -> EnvInfo {
    EnvInfo {
        os: std::env::consts::OS.into(),
        shell: shell_name(),
        project_dir: display_path(project),
        date: today_utc(),
        tier: Tier::Small,
        n_ctx: 0,
        budget: String::new(),
        branch: git_branch(project),
    }
}

/// Строка аргумента для подсказок: путь в кавычках.
fn quoted(p: &Path) -> String {
    format!("\"{}\"", display_path(p))
}

/// Что `run` знает о проекте до цикла (D78, D80, D83, D84): доверие, политика,
/// инструкции, гранты и корни.
struct Prepared {
    trusted: bool,
    /// Последнее решение о доверии — явный отзыв (`trust --revoke`).
    revoked: bool,
    set: TrustSet,
    policy: PolicySnapshot,
    grants: Vec<Grant>,
    roots: Roots,
    project_id: String,
    /// Проектная конфигурация доверенного проекта (`[web] enabled = false`, D105).
    project_cfg: Option<ProjectConfig>,
}

/// Подготовка run (новый и resume). Порядок: доверие (D83: набор сейчас против
/// последней записи аудита доверия; не доверен — сообщение и отличия), уборка остатков
/// прошлых сбоев (`reap_leftovers` реестра групп в `ZA_HOME`, временные файлы правок,
/// D84), проектная конфигурация — только у доверенного проекта, снимок политики
/// (глобальная и проектная, ARC-041), гранты (`--grant` и глобальные
/// `[[policy.grants]]`, пути канонизируются), корни (D76). Записи бинарника в аудит
/// (`policy.loaded`, `grant.added`) и голова аудита — в [`record_start`], прямо перед
/// циклом (m3, m4 ревью задачи 13).
async fn prepare(
    za_home: &Path,
    project: &Path,
    store: &ProjectStore,
    cfg: &AgentConfig,
    cli_grants: &[(PathBuf, GrantMode)],
) -> Result<Prepared, RunError> {
    let check = trust_check(store, project).await?;
    if !check.trusted {
        untrusted_message(project, &check);
    }
    let roots = blocking("корни проекта", project, {
        let (p, h) = (project.to_path_buf(), za_home.to_path_buf());
        let user = home::user_home();
        move || {
            let roots = Roots::canonical(&p, &h)?;
            match user {
                Some(u) => roots.with_home(&u),
                None => {
                    tracing::warn!("домашний каталог пользователя не задан: ~/.zeroaimemory не защищён по пути (D157)");
                    Ok(roots)
                }
            }
        }
    })
    .await?;
    // Проект внутри защищённого места (D157): любой вызов был бы отказом.
    if paths::sealed_place(&roots, &roots.project) {
        return Err(RunError::SealedProject(roots.project.clone()));
    }
    let project_id = home::project_id(project).map_err(|source| RunError::Io {
        what: "каталог проекта",
        path: project.to_path_buf(),
        source,
    })?;
    cleanup_leftovers(&roots, &project_id).await;
    let project_cfg = if check.trusted {
        let (p, set) = (project.to_path_buf(), check.set.clone());
        match blocking(
            "проектная конфигурация",
            project,
            move || read_trusted(&p, &set, PROJECT_CONFIG),
        )
        .await?
        {
            Some(src) => Some(parse_project_config(&src).map_err(CommandError::from)?),
            None => None,
        }
    } else {
        None
    };
    let policy = PolicySnapshot::build(&cfg.policy, project_cfg.as_ref().map(|c| &c.policy))?;
    if project_cfg
        .as_ref()
        .is_some_and(|c| c.web_enabled == Some(false))
    {
        eprintln!(
            "zero-agent: {PROJECT_CONFIG}: веб выключен проектом ([web] enabled = false): web_fetch и web_search в run нет"
        );
    }
    for _ in &policy.ignored {
        eprintln!(
            "zero-agent: {PROJECT_CONFIG}: правило allow проекта не действует (только ask и deny, ARC-041)"
        );
    }
    let mut grants = Vec::new();
    for (path, mode) in cli_grants {
        grants.push(grant(&roots, path, *mode, GrantSource::Cli, String::new()).await?);
    }
    for g in &cfg.policy.grants {
        grants.push(
            grant(
                &roots,
                &g.path,
                g.mode,
                GrantSource::Config,
                g.reason.clone(),
            )
            .await?,
        );
    }
    Ok(Prepared {
        trusted: check.trusted,
        revoked: check.revoked,
        set: check.set,
        policy,
        grants,
        roots,
        project_id,
        project_cfg,
    })
}

/// Подключения run R1-C2 (задача 14): веб-инструменты, извлекатель, каталог скиллов и
/// что из этого пишется в `policy.loaded`.
struct Wiring {
    web: Vec<Arc<dyn Tool>>,
    extractor: Option<Arc<dyn Extractor>>,
    /// `None` — `[skills] enabled = false`.
    catalog: Option<Arc<za_skills::Catalog>>,
    /// Кто выключил веб (`crate::web::DISABLED_BY_*`); `None` — включён.
    web_disabled: Option<&'static str>,
    /// Хост поставщика поиска, если `web_search` зарегистрирован (D105).
    search_host: Option<String>,
    /// Resume: веб-инструменты и `activate_skill` замороженного префикса, которые
    /// конфигурация выключила после старта run, — имя и текст вызова
    /// (`ToolRegistry::withdraw`, Ruling «Задача 15»; [`withdrawn_skills`]).
    withdrawn: Vec<(String, String)>,
}

/// Веб-инструменты замороженного префикса `frozen`, которых нет среди зарегистрированных
/// `web`, с текстом вызова (Ruling «Задача 15», находка 1): веб выключен —
/// [`za_web::WEB_DISABLED`], убран `[web.search] url` — [`za_web::SEARCH_UNCONFIGURED`].
/// Префикс не меняется (кэш), поэтому модель видит описание и может позвать инструмент.
fn withdrawn_web(
    frozen: &BTreeSet<String>,
    web: &[Arc<dyn Tool>],
    web_disabled: Option<&str>,
) -> Vec<(String, String)> {
    [za_web::WEB_FETCH, za_web::WEB_SEARCH]
        .into_iter()
        .filter(|n| frozen.contains(*n) && !web.iter().any(|t| t.spec().schema.name == *n))
        .filter_map(|n| {
            let reason = match (web_disabled, n) {
                (Some(_), _) => za_web::WEB_DISABLED,
                (None, za_web::WEB_SEARCH) => za_web::SEARCH_UNCONFIGURED,
                // `web_fetch` при включённом вебе регистрируется всегда.
                _ => return None,
            };
            Some((n.to_owned(), za_web::withdrawn_text(n, reason)))
        })
        .collect()
}

/// `activate_skill` замороженного префикса `frozen`, когда скиллы выключены после старта
/// run (`[skills] enabled = false`, каталога нет), — имя и текст вызова (Minor 4 финального
/// ревью скиллов, по образцу веба). Записанный `skill.load` пакет реплеит до реестра (D92).
fn withdrawn_skills(
    frozen: &BTreeSet<String>,
    catalog: Option<&Arc<za_skills::Catalog>>,
) -> Option<(String, String)> {
    (catalog.is_none() && frozen.contains(za_agent::ACTIVATE_SKILL_TOOL)).then(|| {
        (
            za_agent::ACTIVATE_SKILL_TOOL.to_owned(),
            za_agent::skills_withdrawn_text(),
        )
    })
}

/// Строка stderr resume о снятых веб-инструментах (`withdrawn` из [`withdrawn_web`]).
fn withdrawn_line(withdrawn: &[(String, String)], web_disabled: Option<&str>) -> Option<String> {
    if withdrawn.is_empty() {
        return None;
    }
    let names: Vec<&str> = withdrawn.iter().map(|(n, _)| n.as_str()).collect();
    let why = match web_disabled {
        Some(crate::web::DISABLED_BY_PROJECT) => {
            format!("веб выключен после старта run ({PROJECT_CONFIG}: [web] enabled = false)")
        }
        Some(_) => "веб выключен после старта run ([web] enabled = false)".to_owned(),
        None => "поиск не настроен после старта run ([web.search] url)".to_owned(),
    };
    Some(format!(
        "zero-agent: {why}: вызовы {} этого run не исполняются",
        names.join(", ")
    ))
}

/// Имена инструментов замороженного префикса из снимка run (`frozen.tools[].name`).
fn frozen_tools(snap: &Value) -> BTreeSet<String> {
    snap.get("frozen")
        .and_then(|f| f.get("tools"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// Сетевой шлюз процесса (D107) из `[web]` глобальной конфигурации; с признаком
/// `test-hooks` — подмена имён `--web-resolve` (D128).
fn gateway(
    cfg: &AgentConfig,
    #[cfg_attr(not(feature = "test-hooks"), allow(unused_variables))] hooks: &RunHooks,
) -> Result<za_web::WebGateway, RunError> {
    let gw = za_web::WebGateway::new(crate::web::web_config(&cfg.web))?;
    #[cfg(feature = "test-hooks")]
    if !hooks.web_resolve.is_empty() {
        return Ok(gw.with_test_resolve(hooks.web_resolve.clone()));
    }
    Ok(gw)
}

/// Извлекатели run по пути бинарника `exe` (`current_exe`): нативным инструментам (`read`) и
/// `web_fetch`. Путь известен — у обоих `RunExtractor` (подпроцесс `extract`, каталог
/// `work_dir("tmp/<run>")` по первому вызову). Путь не определён (m2 ревью задачи 14):
/// `read` без извлекателя (PDF — `binary file`), а `web_fetch` остаётся в наборе с
/// [`za_core::extract::Unavailable`] — текст, JSON и XML читает, на HTML и PDF отвечает
/// `html: …`/`pdf: extraction is unavailable in this session`; `web_search` извлекатель не
/// нужен вовсе. Строка stderr называет, что не работает.
fn extractors(
    exe: std::io::Result<PathBuf>,
    project: &Path,
    run: RunId,
) -> (Option<Arc<dyn Extractor>>, Arc<dyn Extractor>) {
    match exe {
        Ok(exe) => {
            let ex: Arc<dyn Extractor> = Arc::new(RunExtractor::new(
                Arc::new(DirectRunner),
                exe,
                project.to_path_buf(),
                run,
            ));
            (Some(Arc::clone(&ex)), ex)
        }
        Err(e) => {
            tracing::warn!(error = %e, "путь бинарника не определён: PDF и HTML не извлекаются");
            eprintln!(
                "zero-agent: путь бинарника не определён ({e}): PDF и HTML не извлекаются — read отвечает на PDF «binary file», web_fetch читает текст, JSON и XML, а на HTML и PDF отвечает отказом"
            );
            (None, Arc::new(za_core::extract::Unavailable))
        }
    }
}

/// Подключения run: извлекатели ([`extractors`]), шлюз и веб-инструменты, если веб не выключен
/// (`allowed` — имена из замороженного префикса при resume: только они), каталог скиллов
/// после проверки доверия (проектные — у доверенного проекта по сверенному обходу, D116).
async fn wire(
    home: &Path,
    project: &Path,
    run: RunId,
    cfg: &AgentConfig,
    prep: &Prepared,
    hooks: &RunHooks,
    allowed: Option<&BTreeSet<String>>,
) -> Result<Wiring, RunError> {
    let (extractor, web_extractor) = extractors(std::env::current_exe(), project, run);
    let web_disabled = crate::web::web_disabled(cfg, prep.project_cfg.as_ref());
    let mut web = Vec::new();
    if web_disabled.is_none() {
        web = crate::web::web_tools(&Arc::new(gateway(cfg, hooks)?), web_extractor);
        if let Some(names) = allowed {
            web.retain(|t| names.contains(&t.spec().schema.name));
        }
    }
    let withdrawn = allowed.map_or_else(Vec::new, |names| withdrawn_web(names, &web, web_disabled));
    if let Some(line) = withdrawn_line(&withdrawn, web_disabled) {
        eprintln!("{line}");
    }
    let search_host = web
        .iter()
        .any(|t| t.spec().schema.name == za_web::WEB_SEARCH)
        .then(|| crate::web::search_host(&cfg.web))
        .flatten();
    let catalog = if cfg.skills.enabled {
        let trusted = prep.trusted.then_some((project, &prep.set));
        let cat = crate::skills::catalog(home, &cfg.skills, trusted)
            .await
            .map_err(|source| RunError::Io {
                what: "каталог скиллов",
                path: project.to_path_buf(),
                source,
            })?;
        for k in cat
            .skipped
            .iter()
            .filter(|k| k.reason == crate::skills::TREE_CHANGED)
        {
            eprintln!(
                "zero-agent: дерево скиллов {} изменилось после проверки доверия: его скиллы в run не читаются",
                k.path
            );
        }
        for line in crate::skills::limit_lines(&cat, &cfg.skills)
            .into_iter()
            .chain(crate::skills::linked_lines(&cat))
        {
            tracing::warn!("{line}");
        }
        Some(Arc::new(cat))
    } else {
        None
    };
    let mut withdrawn = withdrawn;
    if let Some(off) = allowed.and_then(|names| withdrawn_skills(names, catalog.as_ref())) {
        eprintln!(
            "zero-agent: скиллы выключены после старта run ([skills] enabled = false): вызовы {} этого run не исполняются",
            off.0
        );
        withdrawn.push(off);
    }
    Ok(Wiring {
        web,
        extractor,
        catalog,
        web_disabled,
        search_host,
        withdrawn,
    })
}

/// Записи бинарника в аудит прямо перед циклом (D78, D91): `policy.loaded` с действующим
/// снимком политики run — у нового run и у каждого resume (I1 ревью задачи 14: смена
/// `web_disabled` или `search_host` при том же хэше видна в аудите; дубликат последней
/// записи [`policy_loaded`] не пишет), `grant.added` на каждый новый грант run (источник
/// `cli` — actor `user`, `config` — `runtime`); затем голова цепочки — якорь снимка (после
/// всех записей, m3 ревью задачи 13).
async fn record_start(
    store: &ProjectStore,
    project: &Path,
    run: RunId,
    policy: &PolicySnapshot,
    web: &Wiring,
    grants: &[Grant],
) -> Result<Option<AuditHead>, RunError> {
    policy_loaded(store, project, policy, web).await?;
    grants_audit(store, run, grants).await?;
    Ok(store
        .audit_head()
        .await?
        .map(|(seq, hash)| AuditHead { seq, hash }))
}

/// Блокирующая работа с ФС вне потоков tokio; ошибка — `RunError::Io` с `what` и путём.
async fn blocking<T: Send + 'static>(
    what: &'static str,
    path: &Path,
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, RunError> {
    let io = |source| RunError::Io {
        what,
        path: path.to_path_buf(),
        source,
    };
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| io(std::io::Error::other(e.to_string())))?
        .map_err(io)
}

/// Сообщение недоверенному проекту (D83, D96): что не читается и как дать доверие;
/// изменённые доверенные файлы и разрыв цепочки аудита называются.
fn untrusted_message(project: &Path, check: &crate::trust::TrustCheck) {
    if check.chain_broken {
        eprintln!(
            "zero-agent: цепочка журнала аудита проекта не сходится (zero-agent audit --verify): прежнее доверие не действует"
        );
    }
    if let Some(d) = check.diff.as_ref().filter(|d| !d.is_empty()) {
        eprintln!(
            "zero-agent: доверенные файлы проекта изменились после доверия: {}",
            d.describe()
        );
    }
    eprintln!(
        "zero-agent: проект не доверен: AGENTS.md, CLAUDE.md, {PROJECT_CONFIG} и скиллы не читаются, команды — после разового доверия (вопрос при первой команде или zero-agent trust --project {})",
        quoted(project)
    );
}

/// Уборка остатков прошлых сбоев (D84; заметка 3 задачи 13): деревья процессов из
/// реестра групп `ZA_HOME/projects/<id>/procs` (Linux) и временные файлы правок в
/// `.zero-agent/work/edits` — только если каталог уже есть (в чужом проекте каталоги
/// агента не создаются). Сбой уборки run не останавливает: `warn`.
async fn cleanup_leftovers(roots: &Roots, project_id: &str) {
    let procs = roots
        .za_home
        .join("projects")
        .join(project_id)
        .join("procs");
    let project = roots.project.clone();
    let r = tokio::task::spawn_blocking(move || {
        let killed = if procs.is_dir() {
            DirectRunner::reap_leftovers(&procs)
        } else {
            0
        };
        let edits = project.join(".zero-agent").join("work").join("edits");
        let cleaned = match std::fs::symlink_metadata(&edits) {
            Ok(m) if m.is_dir() => za_tools::clean_edit_leftovers(&project),
            _ => Ok(0),
        };
        (killed, cleaned)
    })
    .await;
    match r {
        Ok((killed, cleaned)) => {
            if killed > 0 {
                tracing::warn!(killed, "убиты процессы, пережившие прошлый запуск агента");
            }
            if let Err(e) = cleaned {
                tracing::warn!(error = %e, "временные файлы правок не убраны");
            }
        }
        Err(e) => tracing::warn!(error = %e, "уборка остатков не выполнена"),
    }
}

/// Убирает `.zero-agent/work/tmp/<run>` (рабочие каталоги процессов run, D84) после
/// выхода из цикла; каталога нет — ничего. Путь проверяется по компонентам без ссылок
/// (`work_dir`), внутри — `remove_dir_all` (по ссылкам не переходит).
async fn cleanup_run_tmp(project: &Path, run: RunId) {
    let tmp = project
        .join(".zero-agent")
        .join("work")
        .join("tmp")
        .join(run.to_string());
    let root = project.to_path_buf();
    let r = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        match std::fs::symlink_metadata(&tmp) {
            Ok(m) if m.is_dir() => {}
            _ => return Ok(()),
        }
        paths::work_dir(&root, &format!("tmp/{run}"))?;
        std::fs::remove_dir_all(&tmp)
    })
    .await;
    match r {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "рабочий каталог процессов run не убран"),
        Err(e) => tracing::warn!(error = %e, "рабочий каталог процессов run не убран"),
    }
}

/// `policy.loaded` (D91): вызывается у нового run и у каждого resume с действующим снимком
/// политики, а пишется, только если хэш снимка, `web_disabled` (кто выключил веб: `config`,
/// `project` или `null`, D105) или `search_host` (хост поставщика поиска, если `web_search`
/// зарегистрирован: правила по домену к поиску не применяются, D126) отличаются от
/// последней такой записи аудита — дубликатов нет. Цепочка аудита не сходится — запись не
/// пишется (`warn`): её сверит `audit --verify`.
async fn policy_loaded(
    store: &ProjectStore,
    project: &Path,
    policy: &PolicySnapshot,
    web: &Wiring,
) -> Result<(), RunError> {
    let last = match store.last_audit(audit_action::POLICY_LOADED).await {
        Ok(l) => l,
        Err(StoreError::Corrupt(e)) => {
            tracing::warn!(error = %e, "цепочка аудита не сходится: policy.loaded не записан");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let same = last.as_ref().is_some_and(|e| {
        let p = &e.payload;
        p.get("hash").and_then(Value::as_str) == Some(policy.hash.as_str())
            && p.get("web_disabled").and_then(Value::as_str) == web.web_disabled
            && p.get("search_host").and_then(Value::as_str) == web.search_host.as_deref()
    });
    if same {
        return Ok(());
    }
    let project_rules = policy
        .rules
        .iter()
        .filter(|(s, _)| *s == za_agent::RuleSource::Project)
        .count();
    store
        .append_audit(AuditNew {
            actor: AUDIT_ACTOR_RUNTIME.to_owned(),
            action: audit_action::POLICY_LOADED.to_owned(),
            target: display_path(project),
            payload: json!({
                "hash": policy.hash,
                "allow_bypass": policy.allow_bypass,
                "rules": policy.rules.len(),
                "project_rules": project_rules,
                "ignored": policy.ignored,
                "web_disabled": web.web_disabled,
                "search_host": web.search_host,
            }),
        })
        .await?;
    Ok(())
}

/// Грант с каноническим путём (D78): относительный путь `--grant` — от текущего
/// каталога; сетевые и device-пути отвергаются до обращения к ФС (D76), несуществующий
/// хвост дописывается к каноническому префиксу.
async fn grant(
    roots: &Roots,
    path: &Path,
    mode: GrantMode,
    source: GrantSource,
    reason: String,
) -> Result<Grant, RunError> {
    let shown = path.display().to_string();
    let bad = |reason: String| RunError::Grant {
        path: shown.clone(),
        reason,
    };
    let abs = std::path::absolute(path).map_err(|e| bad(e.to_string()))?;
    let arg = abs
        .to_str()
        .ok_or_else(|| bad("путь не в UTF-8".to_owned()))?
        .to_owned();
    let roots_c = roots.clone();
    let resolved = tokio::task::spawn_blocking(move || {
        let lex = paths::classify(&roots_c.project, &arg)?;
        paths::resolve(&roots_c, &lex, Access::Read)
    })
    .await
    .map_err(|e| bad(e.to_string()))?
    .map_err(|e| bad(e.to_string()))?;
    Ok(Grant {
        path: resolved.path,
        mode,
        source,
        reason,
    })
}

/// Сверка якоря аудита (D91, m1 ревью задачи 4): запись `seq` из снимка run должна быть
/// в цепочке с тем же хэшем; иначе хвост аудита удалён или переписан —
/// [`RunError::AuditAnchor`]. Снимок без якоря (run до задачи 13) — без сверки.
async fn check_audit_anchor(store: &ProjectStore, snap: &Value) -> Result<(), RunError> {
    let Some(head) = snap
        .get("audit_head")
        .and_then(|v| serde_json::from_value::<AuditHead>(v.clone()).ok())
    else {
        return Ok(());
    };
    let found = store
        .audit_entries(head.seq.saturating_sub(1), 1)
        .await?
        .into_iter()
        .next()
        .is_some_and(|e| e.seq == head.seq && e.hash == head.hash);
    if !found {
        return Err(RunError::AuditAnchor { seq: head.seq });
    }
    Ok(())
}

/// Отличия resume от снимка run (D92): доверие, политика, гранты — и отдельно действующий
/// снимок политики, с которым run продолжается (для `policy.loaded` при каждом resume, I1
/// ревью задачи 14; в `ResumeChange.policy` он попадает, только если хэш другой). Доверие
/// снимается только явным отзывом (`trust --revoke`): правка доверенных файлов посреди run
/// (тем же агентом) доверие этого run не отменяет, предупреждение — в stderr.
///
/// Проектные `deny` и `ask` не теряются (I1 ревью задачи 13): если снимок доверенный, а
/// проект сейчас не доверен (доверенные файлы изменились или доверие отозвано), проектная
/// часть политики берётся из снимка run — она только ограничивает; глобальная — из
/// нынешней конфигурации.
fn resume_changes(
    p: &Prepared,
    cfg: &AgentConfig,
    snap: &Value,
) -> Result<(ResumeChange, PolicySnapshot), RunError> {
    let snap_trusted = snap
        .get("trusted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let trusted = if p.trusted && !snap_trusted {
        Some(true)
    } else if snap_trusted && p.revoked {
        eprintln!("zero-agent: доверие к проекту отозвано: команды run больше не выполняются");
        Some(false)
    } else {
        if snap_trusted && !p.trusted {
            eprintln!(
                "zero-agent: доверие run к проекту сохраняется до его конца; следующий run спросит доверие заново"
            );
        }
        None
    };
    let snap_hash = snap
        .get("policy")
        .and_then(|v| v.get("hash"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let effective = if snap_trusted && !p.trusted {
        let kept: PolicySnapshot = snap
            .get("policy")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let project = za_agent::PolicyConfig {
            rules: kept
                .rules
                .into_iter()
                .filter(|(src, _)| *src == za_agent::RuleSource::Project)
                .map(|(_, r)| r)
                .collect(),
            ..za_agent::PolicyConfig::default()
        };
        PolicySnapshot::build(&cfg.policy, Some(&project))?
    } else {
        p.policy.clone()
    };
    let policy = (effective.hash != snap_hash).then(|| effective.clone());
    let had: Vec<Grant> = snap
        .get("grants")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let grants_add = p
        .grants
        .iter()
        .filter(|g| !had.contains(g))
        .cloned()
        .collect();
    // Run в bypass продолжается, только пока bypass разрешён глобально (D80).
    if snap
        .get("setup")
        .and_then(|s| s.get("run_mode"))
        .and_then(Value::as_str)
        == Some("bypass")
    {
        p.policy.check_mode(RunMode::Bypass)?;
    }
    Ok((
        ResumeChange {
            policy,
            trusted,
            grants_add,
            ..ResumeChange::default()
        },
        effective,
    ))
}

/// Предупреждение D169 на resume с `--model` (`model_changed`, m4 ревью задачи 14): тир run
/// заморожен в снимке (`setup.tier`), а окно — у новой модели `n_ctx` (неизвестно — run
/// считает его 8192, D43). Без `--model` окно то же, что при старте, — без предупреждения.
fn resume_window_warning(snap: &Value, model_changed: bool, n_ctx: Option<u32>) -> Option<String> {
    if !model_changed {
        return None;
    }
    let tier = snap
        .get("setup")
        .and_then(|s| s.get("tier"))
        .and_then(|t| serde_json::from_value::<Tier>(t.clone()).ok())?;
    crate::doctor::tier_window_warning(Some(tier), n_ctx)
}

/// Оболочка `bash` (D85): `pwsh`/`powershell` на Windows, `bash`/`sh` иначе; не найдена —
/// имя по умолчанию: вызов вернёт `failed to start`.
fn shell() -> Shell {
    Shell::detect().unwrap_or_else(|| {
        tracing::warn!("оболочка для bash не найдена в PATH");
        if cfg!(windows) {
            Shell {
                program: PathBuf::from("powershell.exe"),
                dialect: za_core::shell::Dialect::PowerShell,
            }
        } else {
            Shell {
                program: PathBuf::from("/bin/sh"),
                dialect: za_core::shell::Dialect::Bash,
            }
        }
    })
}

/// Набор доверенных файлов для прерывания `Trust` (D83): `{files, set_hash}`; ошибка
/// обхода — пустой набор (доверие к нему `answer` не запишет: набор пересчитывается).
fn trust_set_fn() -> TrustSetFn {
    Arc::new(|p: &Path| match trust_set(p) {
        Ok(s) => json!({"files": s.files, "set_hash": s.hash()}),
        Err(e) => {
            tracing::warn!(error = %e, "набор доверенных файлов не посчитан");
            json!({})
        }
    })
}

/// `zero-agent run`.
pub async fn cmd_run(home: &Path, args: RunArgs) -> Result<ExitCode, RunError> {
    let project = canonical_project(&args.project)?;
    // Первая строка stderr — `run <id>` (D28), раньше любых предупреждений и логов
    // (открытие журнала уже пишет `info`). Resume: наличие журнала проверяется до его
    // открытия, конфигурации и моделей; нет журнала — код 1 без `run` и без создания
    // каталога (R0 progress:114).
    let run = match &args.action {
        RunAction::Resume(r) => {
            existing_journal(home, &project)?;
            r.run
        }
        RunAction::New(_) => RunId::new(),
    };
    announce(run);
    let resume_store = match &args.action {
        RunAction::Resume(r) => {
            let store = open_existing(home, &project)?;
            if store.get_run(r.run).await?.is_none() {
                return Err(StoreError::UnknownRun(r.run).into());
            }
            Some(store)
        }
        RunAction::New(_) => None,
    };

    let cfg = load_config(home).map_err(CommandError::from)?;
    // D80: `bypass` — только при `[policy] allow_bypass = true` глобальной конфигурации.
    if let RunAction::New(n) = &args.action {
        PolicySnapshot::build(&cfg.policy, None)?.check_mode(n.run_mode)?;
    }
    if cfg.models.as_ref().is_none_or(|m| m.endpoints.is_empty()) {
        return Err(CommandError::NoModels.into());
    }
    let started = start_gateway(home, &cfg, true).await?;
    // Маска печати ответа — по секретам, прочитанным при старте вместе с ключами (D90).
    let masker = started.secrets.masker();
    let secret_filter = started.secrets.checkpoint_filter();
    let gateway = Arc::new(started.gateway);
    let select = |model: Option<String>| -> Result<EndpointId, SelectError> {
        gateway.select(&Selection {
            run_choice: model.map(EndpointId),
            role: Some(RunRole::Top),
            ..Selection::default()
        })
    };

    let mut tools = ToolRegistry::default();
    for t in builtin_tools() {
        // Встроенные инструменты драйвер не добавляет сам: их кладёт вызывающий.
        if let Err(e) = tools.register(t) {
            tracing::warn!(error = %e, "встроенный инструмент уже зарегистрирован");
        }
    }

    match args.action {
        RunAction::New(n) => {
            let endpoint = select(n.model.clone()).map_err(CommandError::from)?;
            let Some(view) = wait_endpoint(&gateway, &endpoint, PROBE_WAIT).await else {
                return Err(CommandError::from(SelectError::Unknown(endpoint)).into());
            };
            if !matches!(view.state.as_str(), "ready" | "open") {
                // Новый run к неготовому эндпоинту не создаётся (как `ask`).
                eprintln!(
                    "zero-agent: модель {endpoint} не готова: {}",
                    state_text(&view, Some(PROBE_WAIT))
                );
                return Ok(ExitCode::from(EXIT_ERROR));
            }
            // D169: тир standard (и full) на окне меньше 32k или на неизвестном окне (run считает
            // его 8192, D43; m4 ревью задачи 14) — только явно, с предупреждением.
            if let Some(line) =
                crate::doctor::tier_window_warning(n.tier.or(cfg.context.tier), view.context_tokens)
            {
                eprintln!("zero-agent: {line}");
            }
            let dir = journal_dir(home, &project)?;
            let store = ProjectStore::open(&dir)?;
            let prep = prepare(home, &project, &store, &cfg, &args.grants).await?;
            let wiring = wire(home, &project, run, &cfg, &prep, &args.hooks, None).await?;
            register(&mut tools, &wiring);
            // Бюджет нового run: `[budgets.run]`, поверх него флаги.
            let budget = run_budget(&cfg.budgets.run, &args.budget);
            // D83, CAP-084: инструкции — только у доверенного проекта.
            let agents_md = if prep.trusted {
                let (p, set) = (project.clone(), prep.set.clone());
                blocking(
                    "файл инструкций проекта",
                    &project,
                    move || project_instructions(&p, &set),
                )
                .await?
                .map(|(_, text)| text)
            } else {
                None
            };
            // Ветка git — чтение файлов `.git` проекта: вне потоков tokio (ревью R1-C1).
            let p = project.clone();
            let env = blocking("ветка git проекта", &project, move || {
                Ok(env_info(&p))
            })
            .await?;
            let spec = RunSpec {
                project_id: prep.project_id.clone(),
                project_dir: project.clone(),
                role: RunRole::Top,
                parent: None,
                endpoint,
                run_mode: n.run_mode,
                work_mode: n.work_mode,
                // Порядок тира (D65): `--tier` > `[context].tier` > по окну; выбирает
                // `start_run` по `tier` и `context.tier`.
                tier: n.tier,
                budget,
                priority: Priority::Top,
                max_wait: n.max_wait,
                task: n.task,
                agents_md,
                env,
                context: cfg.context.clone(),
                trusted: prep.trusted,
                grants: prep.grants.clone(),
            };
            let (mut deps, bus) = deps(
                &store,
                &gateway,
                tools,
                &masker,
                secret_filter.clone(),
                &prep,
                &wiring,
                args.checkpoint_timeout,
            );
            deps.crash = crash_hook(args.crash);
            deps.audit_head =
                record_start(&store, &project, run, &prep.policy, &wiring, &prep.grants).await?;
            let (ctl, rx) = control_channel(store.clone(), run);
            let fut = start_run(&deps, run, spec, rx);
            let outcome = drive(&store, &bus, fut, &ctl, args.crash, masker.clone()).await;
            cleanup_run_tmp(&project, run).await;
            report(&store, &project, run, outcome, &masker).await
        }
        RunAction::Resume(r) => {
            let Some(store) = resume_store else {
                return Err(RunError::NoJournal(journal_dir(home, &project)?));
            };
            // D91: якорь аудита прошлого запуска — до любых новых записей.
            let snap = store
                .latest_snapshot(r.run)
                .await?
                .map(|(_, v)| v)
                .unwrap_or(Value::Null);
            check_audit_anchor(&store, &snap).await?;
            let prep = prepare(home, &project, &store, &cfg, &args.grants).await?;
            let (mut change, policy) = resume_changes(&prep, &cfg, &snap)?;
            // Веб-инструменты — только из замороженного префикса run: новых посреди run нет.
            let frozen = frozen_tools(&snap);
            let wiring = wire(
                home,
                &project,
                r.run,
                &cfg,
                &prep,
                &args.hooks,
                Some(&frozen),
            )
            .await?;
            register(&mut tools, &wiring);
            let endpoint = match r.model {
                Some(m) => Some(select(Some(m)).map_err(CommandError::from)?),
                None => None,
            };
            // Зонды до продолжения: размер контекста и состояние эндпоинта. Неготовый
            // эндпоинт run не останавливает — ошибка модели посреди run ставит паузу (D11).
            gateway.wait_probed(PROBE_WAIT).await;
            // D169 (m4 ревью задачи 14): `--model` на модель с окном меньше 32k или с
            // неизвестным окном при тире run standard или full — предупреждение.
            let window = endpoint
                .as_ref()
                .and_then(|ep| za_llm::ModelPort::context_tokens(gateway.as_ref(), ep));
            if let Some(line) = resume_window_warning(&snap, endpoint.is_some(), window) {
                eprintln!("zero-agent: {line}");
            }
            // Бюджет resume: без флагов — бюджет run сохраняется; с флагами — флаги поверх
            // текущего бюджета run из последнего снимка. `[budgets.run]` применён при старте
            // и не перечитывается (решение контроллера, I2 ревью задачи 16).
            let budget = if args.budget.any() {
                let current = current_budget(&store, r.run).await?;
                Some(run_budget(&current, &args.budget))
            } else {
                None
            };
            let (mut deps, bus) = deps(
                &store,
                &gateway,
                tools,
                &masker,
                secret_filter.clone(),
                &prep,
                &wiring,
                args.checkpoint_timeout,
            );
            deps.crash = crash_hook(args.crash);
            // I1 ревью задачи 14: `policy.loaded` — с действующим снимком при каждом resume.
            deps.audit_head = record_start(
                &store,
                &project,
                r.run,
                &policy,
                &wiring,
                &change.grants_add,
            )
            .await?;
            // Новый канал управления на каждый resume: `start_run` и `resume_run` забирают
            // `ControlRx`, а Ctrl+C управляет этим `RunControl` (Carry M2 задачи 12).
            let (ctl, rx) = control_channel(store.clone(), r.run);
            change.endpoint = endpoint;
            change.budget = budget;
            change.input = r.input;
            let fut = resume_run(&deps, r.run, change, rx);
            let outcome = drive(&store, &bus, fut, &ctl, args.crash, masker.clone()).await;
            cleanup_run_tmp(&project, r.run).await;
            report(&store, &project, r.run, outcome, &masker).await
        }
    }
}

/// Аудит грантов run (D78, I2 ревью задачи 13): запись `grant.added` на каждый грант
/// (флаг `--grant` — actor `user`, глобальный `[[policy.grants]]` — `runtime`). Событие
/// `grant.added` журнала run пишет цикл (`za-agent`, шаг 0 и resume). Гранты проектной
/// конфигурации не действуют (ARC-041) и сюда не попадают.
async fn grants_audit(store: &ProjectStore, run: RunId, grants: &[Grant]) -> Result<(), RunError> {
    for g in grants {
        let actor = if g.source == GrantSource::Cli {
            AUDIT_ACTOR_USER
        } else {
            AUDIT_ACTOR_RUNTIME
        };
        store
            .append_audit(AuditNew {
                actor: actor.to_owned(),
                action: audit_action::GRANT_ADDED.to_owned(),
                target: g.path.to_string_lossy().into_owned(),
                payload: json!({
                    "run": run.to_string(),
                    "path": g.path.to_string_lossy(),
                    "mode": g.mode,
                    "source": g.source,
                }),
            })
            .await?;
    }
    Ok(())
}

/// Capability `skills` (`activate_skill` с каталогом, D117) — только у непустого каталога
/// (m7 ревью задачи 13): описание `activate_skill` ссылается на каталог ниже, а без скиллов
/// звать его незачем. Пустой каталог run по-прежнему получает: `skill.catalog` с пропусками.
fn skills_capability_of(cat: &Arc<za_skills::Catalog>) -> Option<za_core::Capability> {
    (!cat.skills.is_empty()).then(|| skills_capability(Arc::clone(cat)))
}

/// Веб-инструменты подключений в реестр run (повтор имени — `warn`, как у встроенных) и
/// снятые с исполнения имена замороженного префикса (`Wiring::withdrawn`).
fn register(reg: &mut ToolRegistry, wiring: &Wiring) {
    for t in &wiring.web {
        if let Err(e) = reg.register(Arc::clone(t)) {
            tracing::warn!(error = %e, "инструмент уже зарегистрирован");
        }
    }
    for (name, text) in &wiring.withdrawn {
        reg.withdraw(name.as_str(), text.as_str());
    }
}

/// Первая строка stderr: `run <id>` — по нему продолжают после сбоя.
fn announce(run: RunId) {
    eprintln!("run {run}");
    let _ = std::io::stderr().flush();
}

/// Зависимости цикла и шина его событий (`EventBus::DEFAULT_CAPACITY`, D35): нативные
/// инструменты (D74) с извлекателем PDF (D113), чекпоинты (D89), политика, корни, маска,
/// набор доверия и голова аудита (задача 13 R1-C1), каталог скиллов (D114–D119): capability
/// `skills` — только у непустого каталога (m7 ревью задачи 13: описание `activate_skill`
/// ссылается на каталог ниже), сам каталог — у любого (событие `skill.catalog` с пропусками).
#[allow(clippy::too_many_arguments)]
fn deps(
    store: &ProjectStore,
    gateway: &Arc<za_llm::Gateway>,
    tools: ToolRegistry,
    masker: &SecretMasker,
    secret_filter: SecretFilter,
    prep: &Prepared,
    wiring: &Wiring,
    checkpoint_timeout: Option<Duration>,
) -> (LoopDeps, EventBus) {
    let bus = EventBus::new(EventBus::DEFAULT_CAPACITY);
    let mut deps = LoopDeps::new(
        Journal::new(store.clone(), bus.clone()),
        Arc::clone(gateway) as Arc<dyn za_llm::ModelPort>,
        Arc::new(tools),
    );
    let agent_home = AgentHome {
        za_home: prep.roots.za_home.clone(),
        project_id: prep.project_id.clone(),
    };
    let runner: Arc<dyn za_jail::Runner> = Arc::new(DirectRunner);
    deps.capabilities = vec![native_capability(&NativeDeps {
        runner: Arc::clone(&runner),
        tracker: ReadTracker::new(),
        shell: shell(),
        home: agent_home.clone(),
        // Маркер повтора правки в `ZA_HOME` — с маскированным diff (ревью задачи 2 R1-C2).
        mask: Some({
            let m = masker.clone();
            Arc::new(move |s: &str| m.mask(s).into_owned())
        }),
        // Извлекатель PDF в подпроцессе (D113): без него `read` отвечает на PDF
        // `binary file …`.
        extractor: wiring.extractor.clone(),
    })];
    if let Some(cat) = &wiring.catalog {
        deps.capabilities.extend(skills_capability_of(cat));
        deps.skills = Some(Arc::clone(cat));
    }
    // D103: секретные файлы (имя, известные значения, PEM) в чекпоинты не входят.
    let mut checkpointer =
        GitCheckpointer::new(runner, agent_home).with_secret_filter(secret_filter);
    if let Some(t) = checkpoint_timeout {
        checkpointer = checkpointer.with_create_timeout_for_tests(t);
    }
    deps.checkpointer = Some(Arc::new(checkpointer));
    deps.policy = prep.policy.clone();
    deps.roots = prep.roots.clone();
    // D100: ответ модели и результаты инструментов маскируются до квитанции.
    deps.masker = Some(Arc::new(masker.clone()));
    deps.trust_set = Some(trust_set_fn());
    (deps, bus)
}

/// Печать ответа по `model.delta` шины (D35). Текст проходит маску секретов (D90):
/// каждая дельта отдельно (секрет на границе двух дельт может проскочить, D100), ответ
/// после отставания — целиком.
#[derive(Default)]
struct Printer {
    masker: SecretMasker,
    /// Текст текущей попытки хода: с ним сравнивается `answer` после отставания.
    current: String,
    /// Последний напечатанный байт — перевод строки (или ничего не напечатано).
    at_line_start: bool,
    /// Что-то напечатано.
    any: bool,
    /// Подписчик отстал (`Lagged`): дельты больше не печатаются.
    lagged: bool,
    /// Идёт запрос компакции (`model.request` с `kind: "compact"`, D46): его дельты —
    /// сводка для истории, а не ответ, и не печатаются.
    compacting: bool,
    crash: Option<RunCrash>,
    /// Строка Rule of Two (D106): одна на run, по первому вызову с внешней связью при всех
    /// трёх свойствах.
    rule_of_two: RuleOfTwoNotice,
}

/// Строка D106 (SEC-052): вызов исполнен при всех трёх свойствах по решению заказчика
/// «не спрашивать». Модели не уходит.
const RULE_OF_TWO_LINE: &str = "zero-agent: в этом запуске сошлись недоверенное содержимое, ваши файлы вне проекта и обращение в сеть (SEC-052); вызов выполнен, запись в аудите";

impl Printer {
    fn new(crash: Option<RunCrash>, masker: SecretMasker) -> Self {
        Self {
            masker,
            at_line_start: true,
            crash,
            ..Self::default()
        }
    }

    fn write(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let mut out = std::io::stdout();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
        self.any = true;
        self.at_line_start = s.ends_with('\n');
    }

    fn event(&mut self, ev: &AgentEvent) {
        let p = &ev.event.payload;
        match ev.event.kind {
            EventKind::TurnStarted => self.current.clear(),
            EventKind::ModelRequest => {
                self.compacting =
                    p.get("kind").and_then(Value::as_str) == Some(za_agent::COMPACT_KIND);
            }
            EventKind::ModelDelta => {
                if let Some(RunCrash::AfterDelta(n)) = self.crash
                    && p.get("turn").and_then(Value::as_u64) == Some(u64::from(n))
                {
                    // D33: эквивалент `kill -9` посреди хода, событие уже в журнале.
                    std::process::abort();
                }
                if self.lagged || self.compacting {
                    return;
                }
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    let t = self.masker.mask(t).into_owned();
                    self.current.push_str(&t);
                    self.write(&t);
                } else if p.get("restart").is_some() {
                    if !self.at_line_start {
                        self.write("\n");
                    }
                    self.current.clear();
                    eprintln!("zero-agent: повтор запроса к модели, ответ печатается заново");
                }
            }
            // D106, Ruling I1 ревью задачи 5: отметка `rule_of_two` вызова.
            EventKind::ToolStarted | EventKind::ToolFinished => {
                if self.rule_of_two.on_event(&ev.event.kind, p) {
                    eprintln!("{RULE_OF_TWO_LINE}");
                }
            }
            // D112: сетевое обращение нативного инструмента — строка на запрос.
            EventKind::WebRequest => eprintln!("{}", crate::web::request_line(p)),
            // D122: загруженный скилл.
            EventKind::SkillActivated => {
                let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("?");
                let by = if s("by") == za_protocol::skill_by::ROUTER {
                    ", карточка роутера"
                } else {
                    ""
                };
                eprintln!("zero-agent: скилл {} ({}){by}", s("name"), s("source"));
            }
            // D96: исполненный вызов с путями вне проекта — строка на путь.
            EventKind::BoundaryOutside => {
                let tool = p.get("tool").and_then(Value::as_str).unwrap_or("?");
                for path in p
                    .get("paths")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    outside_line(tool, path);
                }
            }
            _ => {}
        }
    }

    fn lag(&mut self, n: u64) {
        if !self.lagged {
            tracing::warn!(
                skipped = n,
                "печать отстала от шины событий, ответ будет выведен целиком в конце"
            );
        }
        self.lagged = true;
    }

    /// Конец run: после отставания — остаток ответа (D35); перевод строки в конце.
    fn finish(&mut self, answer: Option<&str>) {
        if self.lagged
            && let Some(a) = answer
        {
            let a = self.masker.mask(a).into_owned();
            let a = a.as_str();
            match a.strip_prefix(self.current.as_str()) {
                Some(rest) => {
                    let rest = rest.to_owned();
                    self.write(&rest);
                }
                None => {
                    if !self.at_line_start {
                        self.write("\n");
                    }
                    self.write(a);
                }
            }
        }
        if self.any && !self.at_line_start {
            self.write("\n");
        }
    }
}

/// Ctrl+C; если обработчик не ставится — никогда.
async fn ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::warn!(error = %e, "обработчик Ctrl+C не установлен");
        std::future::pending::<()>().await;
    }
}

/// У run есть запись аудита `rule_of_two` (D106): строка SEC-052 о нём уже положена.
/// Ошибка чтения — `false` с предупреждением: лишняя строка лучше пропущенной.
async fn rule_of_two_recorded(store: &ProjectStore, run: RunId) -> bool {
    match store
        .audit_by(audit_action::RULE_OF_TWO, &run.to_string())
        .await
    {
        Ok(v) => !v.is_empty(),
        Err(e) => {
            tracing::warn!(error = %e, "записи аудита rule_of_two не прочитаны");
            false
        }
    }
}

/// Ведёт цикл: печатает ответ по шине, обрабатывает Ctrl+C. Строка SEC-052 — одна на run
/// (Ruling I1, m4 ревью задачи 5): resume run с записью `rule_of_two` её не повторяет, а
/// событие, потерянное при отставании шины, восполняет проверка аудита в конце.
async fn drive(
    store: &ProjectStore,
    bus: &EventBus,
    fut: impl std::future::Future<Output = Result<LoopOutcome, LoopError>>,
    ctl: &za_agent::RunControl,
    crash: Option<RunCrash>,
    masker: SecretMasker,
) -> Result<LoopOutcome, LoopError> {
    // Подписка до первого опроса цикла: future ленив, ни одно событие не теряется.
    let mut rx = bus.subscribe();
    let mut printer = Printer::new(crash, masker);
    printer.rule_of_two = RuleOfTwoNotice::new(rule_of_two_recorded(store, ctl.run_id()).await);
    let mut fut = std::pin::pin!(fut);
    let mut interrupt = Box::pin(ctrl_c());
    let mut presses = 0u32;
    let mut bus_open = true;
    let outcome = loop {
        tokio::select! {
            biased;
            ev = rx.recv(), if bus_open => match ev {
                Ok(ev) => printer.event(&ev),
                Err(RecvError::Lagged(n)) => printer.lag(n),
                Err(RecvError::Closed) => bus_open = false,
            },
            () = &mut interrupt => {
                presses += 1;
                if presses == 1 {
                    ctl.pause();
                    eprintln!("\nzero-agent: пауза run (второй Ctrl+C — выход без ожидания)");
                    interrupt = Box::pin(ctrl_c());
                } else {
                    eprintln!("\nzero-agent: выход без ожидания паузы; продолжить: run --resume {}", ctl.run_id());
                    std::process::exit(i32::from(EXIT_PAUSED));
                }
            },
            out = &mut fut => break out,
        }
    };
    // Дочитать опубликованное до конца цикла.
    loop {
        match rx.try_recv() {
            Ok(ev) => printer.event(&ev),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => printer.lag(n),
            Err(_) => break,
        }
    }
    let answer = match &outcome {
        Ok(LoopOutcome::Finished { answer, .. }) => answer.as_deref(),
        _ => None,
    };
    printer.finish(answer);
    if printer.rule_of_two.pending()
        && printer
            .rule_of_two
            .at_end(rule_of_two_recorded(store, ctl.run_id()).await)
    {
        eprintln!("{RULE_OF_TWO_LINE}");
    }
    outcome
}

/// Строка `zero-agent: вне проекта: <tool> <access> <path>` (D96) по элементу `paths`
/// события `boundary.outside` или payload прерывания (`{path, access, …}`).
fn outside_line(tool: &str, path: &Value) {
    let access = path.get("access").and_then(Value::as_str).unwrap_or("?");
    let shown = path
        .get("path")
        .and_then(Value::as_str)
        .map(|p| display_path(Path::new(p)))
        .unwrap_or_default();
    eprintln!("zero-agent: вне проекта: {tool} {access} {shown}");
}

/// Текст не длиннее `n` символов: длиннее — обрезка и `…`.
fn cut_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// Подробности решения в stderr (D96): у одобрения — инструмент, аргументы (до
/// [`ARGS_SHOWN_CHARS`] символов, с маской секретов), причины, метки необратимости и
/// пути вне проекта; у запроса гранта — путь, режим и причина; у доверия — проект и
/// доверенные файлы.
fn decision_lines(kind: &str, p: &Value, masker: &SecretMasker) {
    let list = |key: &str| -> Vec<String> {
        p.get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    if kind == InterruptKind::Trust.as_str() {
        let project = p.get("project").and_then(Value::as_str).unwrap_or_default();
        let files: Vec<String> = p
            .get("files")
            .and_then(Value::as_object)
            .map(|f| f.keys().cloned().collect())
            .unwrap_or_default();
        eprintln!(
            "zero-agent: нужна команда в недоверенном проекте {}: доверить проект один раз? доверенные файлы: {}",
            display_path(Path::new(project)),
            if files.is_empty() {
                "нет".to_owned()
            } else {
                files.join(", ")
            }
        );
        return;
    }
    if kind != InterruptKind::Approval.as_str() {
        return;
    }
    let name = p.get("name").and_then(Value::as_str).unwrap_or("?");
    if let Some(g) = p.get("grant") {
        let path = g.get("path").and_then(Value::as_str).unwrap_or_default();
        let mode = g.get("mode").and_then(Value::as_str).unwrap_or("?");
        let reason = g.get("reason").and_then(Value::as_str).unwrap_or_default();
        eprintln!(
            "zero-agent: модель просит грант: {mode} {}: {}",
            display_path(Path::new(path)),
            cut_chars(&masker.mask(reason), ARGS_SHOWN_CHARS)
        );
    } else {
        let args = p
            .get("arguments")
            .map(za_protocol::canonical_json)
            .unwrap_or_default();
        eprintln!(
            "zero-agent: вызов: {name} {}",
            cut_chars(&masker.mask(&args), ARGS_SHOWN_CHARS)
        );
    }
    let reasons = list("reasons");
    if !reasons.is_empty() {
        eprintln!("zero-agent: причины: {}", reasons.join(", "));
    }
    let irreversible = list("irreversible");
    if !irreversible.is_empty() {
        eprintln!("zero-agent: необратимо: {}", irreversible.join(", "));
    }
    for path in p
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if path.get("inside").and_then(Value::as_bool) == Some(false) {
            outside_line(name, path);
        }
    }
}

/// Причина паузы, требующая решения пользователя (код 3).
fn needs_answer(reason: &str) -> bool {
    [
        pause_reason::NEEDS_INPUT,
        pause_reason::NEEDS_APPROVAL,
        pause_reason::NEEDS_DECISION,
        pause_reason::NEEDS_TRUST,
    ]
    .contains(&reason)
}

/// Флаги `answer` по виду прерывания.
fn answer_flags(kind: &str) -> &'static str {
    if kind == InterruptKind::Input.as_str() {
        "--text \"<ответ>\""
    } else if kind == InterruptKind::Approval.as_str() {
        "--approve | --deny [--note \"<текст>\"]"
    } else if kind == InterruptKind::Trust.as_str() {
        "--approve | --deny"
    } else if kind == InterruptKind::UnknownOutcome.as_str() {
        "--done | --not-done"
    } else {
        "…"
    }
}

/// Имя статуса, как в журнале (`snake_case`).
fn status_name(s: RunStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{s:?}"))
}

/// Причина `failed`: `reason` последнего `run.status` в `failed`; run не в `failed` — `None`.
async fn failed_reason(store: &ProjectStore, run: RunId) -> Result<Option<String>, RunError> {
    match store.get_run(run).await? {
        Some(r) if r.status == RunStatus::Failed => {}
        _ => return Ok(None),
    }
    let events = store.read_run(run, Seq(0), usize::MAX).await?;
    let reason = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::RunStatus && e.payload.get("to") == Some(&json!("failed")))
        .and_then(|e| e.payload.get("reason").and_then(Value::as_str))
        .unwrap_or("-")
        .to_owned();
    Ok(Some(reason))
}

/// Итог run: сообщения в stderr и код выхода (D28).
async fn report(
    store: &ProjectStore,
    project: &Path,
    run: RunId,
    outcome: Result<LoopOutcome, LoopError>,
    masker: &SecretMasker,
) -> Result<ExitCode, RunError> {
    let resume_hint = || {
        eprintln!(
            "zero-agent: продолжить: zero-agent run --project {} --resume {run}",
            quoted(project)
        );
    };
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            // Цикл записал `failed` с кодом причины (`fail_reason`): он и показывается.
            if let Some(reason) = failed_reason(store, run).await? {
                eprintln!("zero-agent: run завершился ошибкой: {reason}");
                eprintln!("zero-agent: подробность: {e}");
                return Ok(ExitCode::from(EXIT_ERROR));
            }
            // Статус не записался (сбой журнала, R1-C1 D72): run не конечный, resume
            // дорабатывает шаг.
            if let Ok(Some(r)) = store.get_run(run).await
                && !r.status.is_terminal()
            {
                resume_hint();
            }
            return Err(e.into());
        }
    };
    let code = match outcome {
        LoopOutcome::Finished { status, .. } => match status {
            RunStatus::Done | RunStatus::Partial => EXIT_DONE,
            RunStatus::Blocked => {
                eprintln!("zero-agent: run заблокирован (blocked): задача не продвигается");
                EXIT_BLOCKED
            }
            other => {
                eprintln!("zero-agent: run завершён в статусе {}", status_name(other));
                EXIT_ERROR
            }
        },
        LoopOutcome::Paused { reason, interrupt } => {
            eprintln!("zero-agent: run на паузе: {reason}");
            if needs_answer(&reason) {
                let ids = match interrupt {
                    Some(id) => vec![id],
                    None => store
                        .interrupts(run)
                        .await?
                        .into_iter()
                        .filter(|i| i.status == INTERRUPT_OPEN)
                        .map(|i| i.id)
                        .collect(),
                };
                for id in ids {
                    let row = store.interrupt(&id).await?;
                    let kind = row.as_ref().map(|r| r.kind.clone()).unwrap_or_default();
                    if let Some(q) = row
                        .as_ref()
                        .and_then(|r| r.payload.as_ref())
                        .and_then(|p| p.get("question"))
                        .and_then(Value::as_str)
                    {
                        eprintln!("zero-agent: вопрос модели: {q}");
                    }
                    if let Some(p) = row.as_ref().and_then(|r| r.payload.as_ref()) {
                        decision_lines(&kind, p, masker);
                    }
                    eprintln!(
                        "zero-agent: ответьте: zero-agent answer --interrupt {id} --project {} {}",
                        quoted(project),
                        answer_flags(&kind)
                    );
                }
                resume_hint();
                EXIT_NEEDS_DECISION
            } else {
                resume_hint();
                EXIT_PAUSED
            }
        }
        LoopOutcome::BlockedOnBudget { kind } => {
            let name = serde_json::to_value(kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            eprintln!(
                "zero-agent: run на паузе: {}: исчерпан предел {name}",
                pause_reason::BUDGET
            );
            eprintln!(
                "zero-agent: продолжить с новым бюджетом: zero-agent run --project {} --resume {run} {}",
                quoted(project),
                budget_flag(kind)
            );
            EXIT_PAUSED
        }
        LoopOutcome::Cancelled => {
            eprintln!("zero-agent: run отменён");
            EXIT_CANCELLED
        }
    };
    Ok(ExitCode::from(code))
}

/// Решение неизвестного исхода именем флага `answer`.
fn decision_flag(d: UnknownDecision) -> &'static str {
    match d {
        UnknownDecision::Done => "--done",
        UnknownDecision::NotDone => "--not-done",
    }
}

/// `zero-agent answer`: решение прерывания; цикл не запускается.
pub async fn cmd_answer(
    home: &Path,
    project: &Path,
    interrupt: &str,
    answer: Answer,
) -> Result<ExitCode, RunError> {
    let project = canonical_project(project)?;
    let store = open_existing(home, &project)?;
    let Some(row) = store.interrupt(interrupt).await? else {
        return Err(RunError::UnknownInterrupt(interrupt.to_owned()));
    };
    let wrong = |hint| RunError::WrongAnswer {
        id: interrupt.to_owned(),
        kind: row.kind.clone(),
        hint,
    };
    let approval = InterruptKind::Approval.as_str();
    let trust = InterruptKind::Trust.as_str();
    let response = match (&answer, row.kind.as_str()) {
        (Answer::Text(t), k) if k == InterruptKind::Input.as_str() => Some(json!({"answer": t})),
        (Answer::Approve | Answer::Deny(_), k) if k == approval || k == trust => {
            let mut set = None;
            if k == trust && answer == Answer::Approve && row.status == INTERRUPT_OPEN {
                let now = current_set(&project).await?;
                if !shown_trust_matches(row.payload.as_ref(), &now) {
                    return reask_trust(&store, &row, &project, now).await;
                }
                set = Some(now);
            }
            let audit = decision_audit(&row, &answer, &project, set.as_ref())?;
            let response = match &answer {
                Answer::Approve if k == trust => json!({"trusted": true}),
                Answer::Approve => json!({"approved": true, "note": ""}),
                a if k == trust => json!({"trusted": false, "note": deny_note(a)}),
                a => json!({"approved": false, "note": deny_note(a)}),
            };
            let applied = store.resolve_with_audit(interrupt, response, audit).await?;
            return Ok(answered(applied, interrupt, &project, row.run));
        }
        (Answer::Done | Answer::NotDone, k) if k == InterruptKind::UnknownOutcome.as_str() => None,
        (_, k) if k == InterruptKind::Input.as_str() => return Err(wrong("ответ — --text")),
        (_, k) if k == approval || k == trust => {
            return Err(wrong("ответ — --approve или --deny"));
        }
        (_, k) if k == InterruptKind::UnknownOutcome.as_str() => {
            return Err(wrong("ответ — --done или --not-done"));
        }
        _ => return Err(wrong("из CLI не решается")),
    };
    let applied = match response {
        Some(v) => store.resolve_interrupt(interrupt, v).await?,
        None => {
            let d = if answer == Answer::Done {
                UnknownDecision::Done
            } else {
                UnknownDecision::NotDone
            };
            let was_open = row.status == INTERRUPT_OPEN;
            let recorded = resolve_unknown(&store, interrupt, d).await?;
            if recorded != d {
                // Действует уже записанное решение (квитанция эффекта или прежний ответ).
                eprintln!(
                    "zero-agent: прерывание {interrupt} уже решено: действует решение {}, ответ не изменён",
                    decision_flag(recorded)
                );
                return Ok(ExitCode::from(EXIT_ERROR));
            }
            was_open
        }
    };
    Ok(answered(applied, interrupt, &project, row.run))
}

/// Итог `answer`: подсказка продолжения или «уже решено» (код 1).
fn answered(applied: bool, interrupt: &str, project: &Path, run: RunId) -> ExitCode {
    if !applied {
        eprintln!("zero-agent: прерывание {interrupt} уже решено, ответ не изменён");
        return ExitCode::from(EXIT_ERROR);
    }
    eprintln!(
        "zero-agent: ответ записан; продолжить: zero-agent run --project {} --resume {}",
        quoted(project),
        run
    );
    ExitCode::from(EXIT_DONE)
}

/// Записи аудита решения пользователя (D91): `approval.resolved` с id прерывания и
/// заметкой к отказу, а при доверии (`set` — набор, пересчитанный в момент ответа и
/// совпавший с показанным, D83) ещё `trust.granted` с путём проекта и набором.
fn decision_audit(
    row: &InterruptRow,
    answer: &Answer,
    project: &Path,
    set: Option<&TrustSet>,
) -> Result<Vec<AuditNew>, RunError> {
    let approved = *answer == Answer::Approve;
    let mut audit = vec![AuditNew {
        actor: AUDIT_ACTOR_USER.to_owned(),
        action: audit_action::APPROVAL_RESOLVED.to_owned(),
        target: row.id.clone(),
        payload: json!({
            "run": row.run.to_string(),
            "kind": row.kind,
            "approved": approved,
            "note": deny_note(answer),
        }),
    }];
    if let Some(set) = set {
        let payload = serde_json::to_value(set)
            .map_err(|e| StoreError::Serde(format!("набор доверенных файлов: {e}")))?;
        audit.push(AuditNew {
            actor: AUDIT_ACTOR_USER.to_owned(),
            action: audit_action::TRUST_GRANTED.to_owned(),
            target: display_path(project),
            payload,
        });
    }
    Ok(audit)
}

/// Заметка к отказу; у одобрения — пустая.
fn deny_note(answer: &Answer) -> String {
    match answer {
        Answer::Deny(Some(n)) => n.clone(),
        _ => String::new(),
    }
}

/// Совпадает ли набор `set` с показанным в payload прерывания доверия (ревью задачи 4,
/// m2): по `set_hash`, а без него — по `files`. Payload без того и другого — не
/// совпадает: доверие даётся только тому, что показано.
fn shown_trust_matches(payload: Option<&Value>, set: &TrustSet) -> bool {
    let Some(p) = payload else { return false };
    if let Some(h) = p.get(TRUST_SET_HASH).and_then(Value::as_str) {
        return h == set.hash();
    }
    p.get("files")
        .and_then(|f| serde_json::from_value::<BTreeMap<String, String>>(f.clone()).ok())
        .is_some_and(|f| f == set.files)
}

/// Набор изменился после вопроса: новое прерывание доверия с текущим набором, старое
/// закрывается ответом `{"superseded_by": <новый id>}` от рантайма (доверие не
/// записывается). id нового — `<база>-set<16 hex хэша набора>`: повтор после сбоя
/// откроет то же прерывание. Код 3, подсказка ответить на новое.
async fn reask_trust(
    store: &ProjectStore,
    row: &InterruptRow,
    project: &Path,
    set: TrustSet,
) -> Result<ExitCode, RunError> {
    let hash = set.hash();
    let base = row.id.split(TRUST_REASK_MARK).next().unwrap_or(&row.id);
    let new_id = format!("{base}{TRUST_REASK_MARK}{}", &hash[..16]);
    let payload = json!({
        "project": display_path(project),
        "files": set.files,
        TRUST_SET_HASH: hash,
    });
    store
        .open_interrupt(&new_id, row.run, InterruptKind::Trust.as_str(), payload)
        .await?;
    let closed = store
        .resolve_interrupt_as(&row.id, json!({"superseded_by": new_id}), Actor::Runtime)
        .await?;
    if !closed {
        eprintln!(
            "zero-agent: прерывание {} уже решено, ответ не изменён",
            row.id
        );
        return Ok(ExitCode::from(EXIT_ERROR));
    }
    eprintln!("zero-agent: доверенные файлы проекта изменились после вопроса, доверие не записано");
    eprintln!(
        "zero-agent: ответьте: zero-agent answer --project {} --interrupt {new_id} --approve|--deny",
        quoted(project)
    );
    Ok(ExitCode::from(EXIT_NEEDS_DECISION))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D106, Ruling I1 ревью задачи 5: строка Rule of Two — одна на run, по отметке
    /// `rule_of_two` вызова (`tool.started`), а не по `exposure.changed`; два вызова при
    /// трёх свойствах — одна строка; resume run с записью — без строки.
    #[test]
    fn rule_of_two_line_once_per_run() {
        let mut p = Printer::default();
        assert!(p.rule_of_two.pending());
        let marked = |id: &str| json!({"call_id": id, "rule_of_two": true});
        assert!(!p.rule_of_two.on_event(
            &EventKind::ExposureChanged,
            &json!({"untrusted": true, "private": true, "external": true,
                "calls": ["w1"], "rule_of_two": ["w1"]})
        ));
        assert!(
            p.rule_of_two
                .on_event(&EventKind::ToolStarted, &marked("w1"))
        );
        assert!(
            !p.rule_of_two
                .on_event(&EventKind::ToolStarted, &marked("w2"))
        );
        assert!(!p.rule_of_two.pending());
        let mut resumed = RuleOfTwoNotice::new(true);
        assert!(!resumed.on_event(&EventKind::ToolStarted, &marked("w3")));
        assert!(RULE_OF_TWO_LINE.contains("SEC-052"));
    }

    /// m7 ревью задачи 13: пустой каталог — без capability `skills` и `activate_skill`.
    #[test]
    fn empty_catalog_has_no_skills_capability() {
        assert!(skills_capability_of(&Arc::new(za_skills::Catalog::default())).is_none());
        let full = za_skills::discover(&za_skills::DiscoverInput {
            bundled: za_skills::bundled(),
            ..za_skills::DiscoverInput::default()
        });
        let cap = skills_capability_of(&Arc::new(full)).expect("capability у поставки");
        assert_eq!(cap.tools.len(), 1);
        assert_eq!(
            cap.tools[0].spec().schema.name,
            za_agent::ACTIVATE_SKILL_TOOL
        );
    }

    /// Имена инструментов замороженного префикса из снимка (resume, задача 14).
    #[test]
    fn frozen_tool_names() {
        let snap = json!({"frozen": {"tools": [{"name": "read"}, {"name": "web_fetch"}]}});
        let names = frozen_tools(&snap);
        assert!(names.contains("web_fetch") && names.contains("read"));
        assert!(frozen_tools(&json!({})).is_empty());
    }

    /// m4 ревью задачи 14: resume с `--model` сверяет тир run (из снимка) с окном новой
    /// модели; без `--model` и у small — без предупреждения.
    #[test]
    fn resume_model_window_warning() {
        let snap = json!({"setup": {"tier": "standard", "n_ctx": 32768}});
        let w = resume_window_warning(&snap, true, Some(16_384)).unwrap();
        assert!(w.contains("тир standard выбран на окне 16384"), "{w}");
        assert!(
            resume_window_warning(&snap, true, None)
                .unwrap()
                .contains("неизвестно")
        );
        assert!(resume_window_warning(&snap, false, Some(16_384)).is_none());
        assert!(resume_window_warning(&snap, true, Some(32_768)).is_none());
        let small = json!({"setup": {"tier": "small"}});
        assert!(resume_window_warning(&small, true, Some(8192)).is_none());
        assert!(resume_window_warning(&json!({}), true, Some(8192)).is_none());
    }

    /// m2 ревью задачи 14: путь бинарника не определён — у `read` извлекателя нет, а
    /// `web_fetch` остаётся с `Unavailable` (честный отказ на HTML и PDF), `web_search` тоже.
    #[test]
    fn extractors_without_exe_keep_web_tools() {
        let (native, web) = extractors(
            Err(std::io::Error::other("no path")),
            Path::new("."),
            RunId::new(),
        );
        assert!(native.is_none());
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let r = rt.block_on(web.html(za_core::extract::HtmlRequest {
            html: "<p>x</p>".into(),
            base_url: "https://a.test/".into(),
        }));
        assert_eq!(r.unwrap_err(), za_core::extract::ExtractError::Unavailable);
        let cfg =
            crate::config::parse_config("[web.search]\nurl = \"http://127.0.0.1:8888\"\n").unwrap();
        let gw = Arc::new(za_web::WebGateway::new(crate::web::web_config(&cfg.web)).unwrap());
        let names: Vec<String> = crate::web::web_tools(&gw, web)
            .iter()
            .map(|t| t.spec().schema.name.clone())
            .collect();
        assert_eq!(names, ["web_fetch", "web_search"]);
        let (native, _) = extractors(
            Ok(PathBuf::from("zero-agent")),
            Path::new("."),
            RunId::new(),
        );
        assert!(native.is_some());
    }

    /// Minor 4 финального ревью скиллов: `activate_skill` префикса при выключенных скиллах
    /// снимается с исполнения с текстом причины; при включённых или без него в префиксе —
    /// нет.
    #[test]
    fn withdrawn_activate_skill_of_frozen_prefix() {
        let frozen: BTreeSet<String> = ["read", "activate_skill"].map(str::to_owned).into();
        assert_eq!(
            withdrawn_skills(&frozen, None),
            Some((
                "activate_skill".to_owned(),
                "activate_skill: skills are disabled by configuration since this run started"
                    .to_owned()
            ))
        );
        let cat = Arc::new(za_skills::Catalog::default());
        assert_eq!(withdrawn_skills(&frozen, Some(&cat)), None);
        let without: BTreeSet<String> = ["read"].map(str::to_owned).into();
        assert_eq!(withdrawn_skills(&without, None), None);
    }

    /// Ruling «Задача 15», находка 1: веб-инструменты префикса, которых нет в реестре resume,
    /// снимаются с исполнения с текстом причины; строка stderr называет, кто выключил.
    #[test]
    fn withdrawn_web_tools_of_frozen_prefix() {
        let frozen: BTreeSet<String> = ["read", "web_fetch", "web_search"]
            .map(str::to_owned)
            .into();
        let off = withdrawn_web(&frozen, &[], Some(crate::web::DISABLED_BY_PROJECT));
        assert_eq!(
            off,
            [
                (
                    "web_fetch".to_owned(),
                    "web_fetch: web tools are disabled by configuration since this run started"
                        .to_owned()
                ),
                (
                    "web_search".to_owned(),
                    "web_search: web tools are disabled by configuration since this run started"
                        .to_owned()
                ),
            ]
        );
        let line = withdrawn_line(&off, Some(crate::web::DISABLED_BY_PROJECT)).unwrap();
        assert!(line.contains(PROJECT_CONFIG), "{line}");
        assert!(line.ends_with("вызовы web_fetch, web_search этого run не исполняются"));
        // Веб включён, поиска нет: снят только `web_search`.
        let only: BTreeSet<String> = ["web_search"].map(str::to_owned).into();
        let search = withdrawn_web(&only, &[], None);
        assert_eq!(search.len(), 1);
        assert_eq!(
            search[0].1,
            "web_search: web search is not configured since this run started"
        );
        // Префикс без веба — ничего не снято, строки нет.
        let none: BTreeSet<String> = ["read"].map(str::to_owned).into();
        assert!(withdrawn_web(&none, &[], Some("config")).is_empty());
        assert!(withdrawn_line(&[], Some("config")).is_none());
    }

    #[test]
    fn crash_parses() {
        assert_eq!(
            parse_run_crash("after-delta:2"),
            Ok(RunCrash::AfterDelta(2))
        );
        assert!(parse_run_crash("after-delta:0").is_err());
        assert!(parse_run_crash("after-delta").is_err());
        assert!(parse_run_crash("intent:1").is_err());
        assert_eq!(parse_run_crash("in-tool:3"), Ok(RunCrash::InTool(3)));
        assert!(parse_run_crash("in-tool:0").is_err());
        assert_eq!(
            parse_run_crash("after-boundary:2"),
            Ok(RunCrash::AfterBoundary(2))
        );
        assert!(parse_run_crash("after-boundary:0").is_err());
    }

    #[test]
    fn budget_flags_override_base() {
        let base = Budget {
            steps: Some(10),
            usd: Some(1.5),
            ..Budget::default()
        };
        let flags = |f: BudgetFlags| run_budget(&base, &f);
        let b = flags(BudgetFlags {
            steps: Some(3),
            ..BudgetFlags::default()
        });
        assert_eq!(b.steps, Some(3));
        assert_eq!(b.usd, Some(1.5));
        assert_eq!(b.tokens, None);
        let b = flags(BudgetFlags {
            tokens: Some(500),
            ..BudgetFlags::default()
        });
        assert_eq!((b.steps, b.tokens), (Some(10), Some(500)));
        let b = flags(BudgetFlags {
            usd: Some(4.0),
            gpu_ms: Some(7),
            duration_secs: Some(600),
            ..BudgetFlags::default()
        });
        assert_eq!(b.usd, Some(4.0));
        assert_eq!((b.gpu_ms, b.duration_ms), (Some(7), Some(600_000)));
        assert_eq!(b.steps, Some(10));
        assert!(!BudgetFlags::default().any());
    }

    #[test]
    fn budget_hint_names_flag_of_kind() {
        assert_eq!(budget_flag(BudgetKind::Duration), "--max-duration СЕК");
        assert_eq!(budget_flag(BudgetKind::Usd), "--max-usd USD");
        assert_eq!(budget_flag(BudgetKind::GpuMs), "--max-gpu-ms МС");
        assert_eq!(budget_flag(BudgetKind::Steps), "--max-steps N");
        assert_eq!(budget_flag(BudgetKind::Tokens), "--max-tokens N");
    }

    #[test]
    fn usd_flag_is_finite_and_non_negative() {
        assert_eq!(parse_usd("2.5"), Ok(2.5));
        assert_eq!(parse_usd("0"), Ok(0.0));
        for bad in ["nan", "inf", "-1", "x"] {
            assert!(parse_usd(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn display_path_strips_verbatim_prefix() {
        assert_eq!(
            display_path(Path::new(r"\\?\H:\Проект\a b")),
            r"H:\Проект\a b"
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\srv\share\x")),
            r"\\srv\share\x"
        );
        assert_eq!(display_path(Path::new(r"\\srv\share")), r"\\srv\share");
        assert_eq!(display_path(Path::new("/home/u/p")), "/home/u/p");
    }

    #[test]
    fn date_is_iso() {
        let d = today_utc();
        assert_eq!(d.len(), 10, "{d}");
        assert_eq!(&d[4..5], "-");
        assert_eq!(&d[7..8], "-");
    }

    #[test]
    fn shell_has_no_path() {
        let s = shell_name();
        assert!(!s.contains('/') && !s.contains('\\'), "{s}");
        assert!(!s.is_empty());
    }

    #[test]
    fn decision_reasons_are_code_3() {
        assert!(needs_answer(pause_reason::NEEDS_INPUT));
        assert!(needs_answer(pause_reason::NEEDS_APPROVAL));
        assert!(needs_answer(pause_reason::NEEDS_DECISION));
        assert!(!needs_answer(pause_reason::PROVIDER));
        assert!(!needs_answer(pause_reason::COMMAND));
    }
}
