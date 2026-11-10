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
//! `blocked_on_budget`; 5 — `blocked`; 6 — `cancelled`. На Windows аварийное завершение
//! (`abort`) тоже даёт код 3: признак настоящего «нужно решение» — строка `ответьте:`.
//!
//! `answer` открывает журнал, записывает решение прерывания и выходит; цикл продолжает
//! `run --resume`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use za_agent::{
    EnvInfo, LoopDeps, LoopError, LoopOutcome, NoMemory, ResumeChange, RunSpec, WorkMode,
    builtin_tools, control_channel, git_branch, resume_run, start_run,
};
use za_core::{
    AgentEvent, EventBus, InterruptKind, Journal, ToolRegistry, UnknownDecision, resolve_unknown,
};
use za_llm::{EndpointId, SelectError, Selection};
use za_protocol::{
    Budget, EventKind, Priority, RunId, RunMode, RunRole, RunStatus, Tier, pause_reason,
};
use za_store::{INTERRUPT_OPEN, ProjectStore, StoreError};

use crate::config::{AgentConfig, load_config};
use crate::home;
use crate::journal::open_retrying;
use crate::models::{CommandError, PROBE_WAIT, start_gateway, state_text, wait_endpoint};

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
/// Файл инструкций проекта (блок 5, D22).
const AGENTS_MD: &str = "AGENTS.md";

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
    #[error("прерывание {id} вида {kind}: {hint}")]
    WrongAnswer {
        id: String,
        kind: String,
        hint: &'static str,
    },
}

/// Точка аварийного завершения `--crash-at` (D33).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunCrash {
    /// `std::process::abort()` на первом `model.delta` хода N (ходы с 1).
    AfterDelta(u32),
}

/// Разбор `--crash-at after-delta:N`.
pub fn parse_run_crash(s: &str) -> Result<RunCrash, String> {
    let bad = || format!("ожидается after-delta:N, получено {s:?}");
    let (kind, n) = s.split_once(':').ok_or_else(bad)?;
    let n: u32 = n.trim().parse().map_err(|_| bad())?;
    match kind.trim() {
        "after-delta" if n >= 1 => Ok(RunCrash::AfterDelta(n)),
        _ => Err(bad()),
    }
}

/// Аргументы нового run.
#[derive(Debug, Clone)]
pub struct NewRunArgs {
    pub model: Option<String>,
    pub run_mode: RunMode,
    pub work_mode: Option<WorkMode>,
    pub tier: Option<Tier>,
    pub max_wait: Option<Duration>,
    pub agents_md: bool,
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
    pub max_steps: Option<u32>,
    pub max_tokens: Option<u64>,
    pub crash: Option<RunCrash>,
}

/// Ответ на прерывание (`answer`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Ответ на `ask_user` (`input-…`).
    Text(String),
    /// Одобрение вызова (`appr-…`).
    Approve,
    /// Отказ в вызове (`appr-…`) с необязательной заметкой.
    Deny(Option<String>),
    /// Неизвестный исход (`unk-…`): действие выполнено.
    Done,
    /// Неизвестный исход (`unk-…`): действие не выполнено.
    NotDone,
}

/// Бюджет run: `[budgets.run]`, поверх него флаги `--max-steps`, `--max-tokens`.
pub fn run_budget(cfg: &AgentConfig, max_steps: Option<u32>, max_tokens: Option<u64>) -> Budget {
    let mut b = cfg.budgets.run.clone();
    if max_steps.is_some() {
        b.steps = max_steps;
    }
    if max_tokens.is_some() {
        b.tokens = max_tokens;
    }
    b
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
fn open_existing(home: &Path, project: &Path) -> Result<ProjectStore, RunError> {
    let dir = journal_dir(home, project)?;
    if !dir.join(JOURNAL_FILE).is_file() {
        return Err(RunError::NoJournal(dir));
    }
    Ok(open_retrying(&dir)?)
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
fn env_info(project: &Path) -> EnvInfo {
    EnvInfo {
        os: std::env::consts::OS.into(),
        shell: shell_name(),
        project_dir: project.display().to_string(),
        date: today_utc(),
        tier: Tier::Small,
        n_ctx: 0,
        budget: String::new(),
        branch: git_branch(project),
    }
}

/// `AGENTS.md` проекта; нет файла — предупреждение и `None`.
fn read_agents_md(project: &Path) -> Result<Option<String>, RunError> {
    let path = project.join(AGENTS_MD);
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "zero-agent: {} не найден, run без инструкций проекта",
                path.display()
            );
            Ok(None)
        }
        Err(source) => Err(RunError::Io {
            what: "файл инструкций проекта",
            path,
            source,
        }),
    }
}

/// Строка аргумента для подсказок: путь в кавычках.
fn quoted(p: &Path) -> String {
    format!("\"{}\"", p.display())
}

/// `zero-agent run`.
pub async fn cmd_run(home: &Path, args: RunArgs) -> Result<ExitCode, RunError> {
    let project = std::fs::canonicalize(&args.project).map_err(|source| RunError::Io {
        what: "каталог проекта",
        path: args.project.clone(),
        source,
    })?;
    // Resume: журнал проверяется раньше конфигурации и моделей, неизвестный run не
    // создаёт журнал проекта (R0 progress:114).
    let resume_store = match &args.action {
        RunAction::Resume(_) => Some(open_existing(home, &project)?),
        RunAction::New(_) => None,
    };
    if let (Some(store), RunAction::Resume(r)) = (&resume_store, &args.action)
        && store.get_run(r.run).await?.is_none()
    {
        return Err(StoreError::UnknownRun(r.run).into());
    }

    let cfg = load_config(home).map_err(CommandError::from)?;
    if cfg.models.as_ref().is_none_or(|m| m.endpoints.is_empty()) {
        return Err(CommandError::NoModels.into());
    }
    let budget = run_budget(&cfg, args.max_steps, args.max_tokens);
    let started = start_gateway(home, &cfg, true).await?;
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
            let dir = journal_dir(home, &project)?;
            let store = ProjectStore::open(&dir)?;
            let project_id = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let agents_md = if n.agents_md {
                read_agents_md(&project)?
            } else {
                None
            };
            let spec = RunSpec {
                project_id,
                project_dir: project.clone(),
                role: RunRole::Top,
                parent: None,
                endpoint,
                run_mode: n.run_mode,
                work_mode: n.work_mode,
                tier: n.tier,
                budget,
                priority: Priority::Top,
                max_wait: n.max_wait,
                task: n.task,
                agents_md,
                env: env_info(&project),
            };
            let run = RunId::new();
            announce(run);
            let (deps, bus) = deps(&store, &gateway, tools);
            let (ctl, rx) = control_channel(store.clone(), run);
            let fut = start_run(&deps, run, spec, rx);
            let outcome = drive(&bus, fut, &ctl, args.crash).await;
            report(&store, &project, run, outcome).await
        }
        RunAction::Resume(r) => {
            let Some(store) = resume_store else {
                return Err(RunError::NoJournal(journal_dir(home, &project)?));
            };
            let endpoint = match r.model {
                Some(m) => Some(select(Some(m)).map_err(CommandError::from)?),
                None => None,
            };
            // Зонды до продолжения: размер контекста и состояние эндпоинта. Неготовый
            // эндпоинт run не останавливает — ошибка модели посреди run ставит паузу (D11).
            gateway.wait_probed(PROBE_WAIT).await;
            announce(r.run);
            let (deps, bus) = deps(&store, &gateway, tools);
            // Новый канал управления на каждый resume: `start_run` и `resume_run` забирают
            // `ControlRx`, а Ctrl+C управляет этим `RunControl` (Carry M2 задачи 12).
            let (ctl, rx) = control_channel(store.clone(), r.run);
            let change = ResumeChange {
                endpoint,
                // Бюджет перечитывается из `[budgets.run]` и флагов: иначе run в
                // `blocked_on_budget` из CLI не продолжить (D19).
                budget: Some(budget),
                input: r.input,
                run_mode: None,
            };
            let fut = resume_run(&deps, r.run, change, rx);
            let outcome = drive(&bus, fut, &ctl, args.crash).await;
            report(&store, &project, r.run, outcome).await
        }
    }
}

/// Первая строка stderr: `run <id>` — по нему продолжают после сбоя.
fn announce(run: RunId) {
    eprintln!("run {run}");
    let _ = std::io::stderr().flush();
}

/// Зависимости цикла и шина его событий (`EventBus::DEFAULT_CAPACITY`, D35).
fn deps(
    store: &ProjectStore,
    gateway: &Arc<za_llm::Gateway>,
    tools: ToolRegistry,
) -> (LoopDeps, EventBus) {
    let bus = EventBus::new(EventBus::DEFAULT_CAPACITY);
    let deps = LoopDeps {
        journal: Journal::new(store.clone(), bus.clone()),
        model: Arc::clone(gateway) as Arc<dyn za_llm::ModelPort>,
        tools: Arc::new(tools),
        capabilities: Vec::new(),
        memory: Arc::new(NoMemory),
        crash: None,
    };
    (deps, bus)
}

/// Печать ответа по `model.delta` шины (D35).
#[derive(Default)]
struct Printer {
    /// Текст текущей попытки хода: с ним сравнивается `answer` после отставания.
    current: String,
    /// Последний напечатанный байт — перевод строки (или ничего не напечатано).
    at_line_start: bool,
    /// Что-то напечатано.
    any: bool,
    /// Подписчик отстал (`Lagged`): дельты больше не печатаются.
    lagged: bool,
    crash: Option<RunCrash>,
}

impl Printer {
    fn new(crash: Option<RunCrash>) -> Self {
        Self {
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
            EventKind::ModelDelta => {
                if let Some(RunCrash::AfterDelta(n)) = self.crash
                    && p.get("turn").and_then(Value::as_u64) == Some(u64::from(n))
                {
                    // D33: эквивалент `kill -9` посреди хода, событие уже в журнале.
                    std::process::abort();
                }
                if self.lagged {
                    return;
                }
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    self.current.push_str(t);
                    self.write(t);
                } else if p.get("restart").is_some() {
                    if !self.at_line_start {
                        self.write("\n");
                    }
                    self.current.clear();
                    eprintln!("zero-agent: повтор запроса к модели, ответ печатается заново");
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
            match a.strip_prefix(self.current.as_str()) {
                Some(rest) => self.write(rest),
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

/// Ведёт цикл: печатает ответ по шине, обрабатывает Ctrl+C.
async fn drive(
    bus: &EventBus,
    fut: impl std::future::Future<Output = Result<LoopOutcome, LoopError>>,
    ctl: &za_agent::RunControl,
    crash: Option<RunCrash>,
) -> Result<LoopOutcome, LoopError> {
    // Подписка до первого опроса цикла: future ленив, ни одно событие не теряется.
    let mut rx = bus.subscribe();
    let mut printer = Printer::new(crash);
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
    outcome
}

/// Причина паузы, требующая решения пользователя (код 3).
fn needs_answer(reason: &str) -> bool {
    [
        pause_reason::NEEDS_INPUT,
        pause_reason::NEEDS_APPROVAL,
        pause_reason::NEEDS_DECISION,
    ]
    .contains(&reason)
}

/// Флаги `answer` по виду прерывания.
fn answer_flags(kind: &str) -> &'static str {
    if kind == InterruptKind::Input.as_str() {
        "--text \"<ответ>\""
    } else if kind == InterruptKind::Approval.as_str() {
        "--approve | --deny [--note \"<текст>\"]"
    } else if kind == InterruptKind::UnknownOutcome.as_str() {
        "--done | --not-done"
    } else {
        "…"
    }
}

/// Итог run: сообщения в stderr и код выхода (D28).
async fn report(
    store: &ProjectStore,
    project: &Path,
    run: RunId,
    outcome: Result<LoopOutcome, LoopError>,
) -> Result<ExitCode, RunError> {
    let resume_hint = || {
        eprintln!(
            "zero-agent: продолжить: zero-agent run --project {} --resume {run}",
            quoted(project)
        );
    };
    let code = match outcome? {
        LoopOutcome::Finished { status, .. } => match status {
            RunStatus::Done | RunStatus::Partial => EXIT_DONE,
            RunStatus::Blocked => {
                eprintln!("zero-agent: run заблокирован (blocked): задача не продвигается");
                EXIT_BLOCKED
            }
            other => {
                eprintln!("zero-agent: run завершён в статусе {other:?}");
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
            let kind = serde_json::to_value(kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            eprintln!(
                "zero-agent: run на паузе: {}: исчерпан предел {kind}",
                pause_reason::BUDGET
            );
            eprintln!(
                "zero-agent: продолжить с новым бюджетом: zero-agent run --project {} --resume {run} --max-steps N | --max-tokens N",
                quoted(project)
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

/// `zero-agent answer`: решение прерывания; цикл не запускается.
pub async fn cmd_answer(
    home: &Path,
    project: &Path,
    interrupt: &str,
    answer: Answer,
) -> Result<ExitCode, RunError> {
    let project = std::fs::canonicalize(project).map_err(|source| RunError::Io {
        what: "каталог проекта",
        path: project.to_path_buf(),
        source,
    })?;
    let store = open_existing(home, &project)?;
    let Some(row) = store.interrupt(interrupt).await? else {
        return Err(RunError::UnknownInterrupt(interrupt.to_owned()));
    };
    let wrong = |hint| RunError::WrongAnswer {
        id: interrupt.to_owned(),
        kind: row.kind.clone(),
        hint,
    };
    let response = match (&answer, row.kind.as_str()) {
        (Answer::Text(t), k) if k == InterruptKind::Input.as_str() => Some(json!({"answer": t})),
        (Answer::Approve, k) if k == InterruptKind::Approval.as_str() => {
            Some(json!({"approved": true, "note": ""}))
        }
        (Answer::Deny(note), k) if k == InterruptKind::Approval.as_str() => {
            Some(json!({"approved": false, "note": note.clone().unwrap_or_default()}))
        }
        (Answer::Done | Answer::NotDone, k) if k == InterruptKind::UnknownOutcome.as_str() => None,
        (_, k) if k == InterruptKind::Input.as_str() => return Err(wrong("ответ — --text")),
        (_, k) if k == InterruptKind::Approval.as_str() => {
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
            was_open && recorded == d
        }
    };
    if !applied {
        eprintln!("zero-agent: прерывание {interrupt} уже решено, ответ не изменён");
        return Ok(ExitCode::from(EXIT_ERROR));
    }
    eprintln!(
        "zero-agent: ответ записан; продолжить: zero-agent run --project {} --resume {}",
        quoted(&project),
        row.run
    );
    Ok(ExitCode::from(EXIT_DONE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_parses() {
        assert_eq!(
            parse_run_crash("after-delta:2"),
            Ok(RunCrash::AfterDelta(2))
        );
        assert!(parse_run_crash("after-delta:0").is_err());
        assert!(parse_run_crash("after-delta").is_err());
        assert!(parse_run_crash("intent:1").is_err());
    }

    #[test]
    fn budget_flags_override_config() {
        let mut cfg = AgentConfig::default();
        cfg.budgets.run.steps = Some(10);
        cfg.budgets.run.usd = Some(1.5);
        let b = run_budget(&cfg, Some(3), None);
        assert_eq!(b.steps, Some(3));
        assert_eq!(b.usd, Some(1.5));
        assert_eq!(b.tokens, None);
        let b = run_budget(&cfg, None, Some(500));
        assert_eq!((b.steps, b.tokens), (Some(10), Some(500)));
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
