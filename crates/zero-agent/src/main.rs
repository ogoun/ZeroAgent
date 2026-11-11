//! Бинарник `zero-agent`: `doctor`, `demo-run`, `replay` (R0), `models`, `ask`,
//! `resources` (R1-A), `run`, `answer` (R1-B1), `inspect`, `stats` (R1-B2), `audit`,
//! `journal export`, `checkpoints`, `secret list`, `trust` (R1-C1).
//!
//! Коды выхода: 0 — готово, 1 — ошибка (в том числе неверные аргументы, ошибка
//! конфигурации, ошибка или неготовность модели), 3 — нужно решение пользователя;
//! у `run` ещё 4 — пауза или `blocked_on_budget`, 5 — `blocked`, 6 — `cancelled` (D28),
//! 2 — удалённый флаг `--agents-md` (D83).
//! Журнал проекта лежит в
//! `ZA_HOME/projects/<project_id>/` (ARC-020), `log.txt` демонстрации — в
//! каталоге проекта.
//!
//! Логи `tracing` ядра и хранилища пишутся в stderr; уровень по умолчанию
//! `warn`, фильтр задаётся `ZA_LOG` (приоритетнее) или `RUST_LOG` в формате
//! `info` или `warn,za_store=debug`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use za_agent::{GrantMode, WorkMode, parse_grant_flag};
use za_core::{CrashPoint, DemoConfig, DemoOutcome, EffectError, replay_hash, run_demo};
use za_protocol::{Priority, RunId, RunMode, Tier};
use za_store::{ProjectStore, StoreError};
use zero_agent::audit::{self as audit_cmd, AuditCmdError};
use zero_agent::checkpoints::{self as checkpoints_cmd, CheckpointsCmdError};
use zero_agent::doctor::{self, LLAMA_URL_ENV};
use zero_agent::home::{self, NoHome};
use zero_agent::inspect::{self as inspector, InspectArgs, InspectCmdError};
use zero_agent::journal::open_retrying;
use zero_agent::models::{self, AskArgs, CommandError};
use zero_agent::resources;
use zero_agent::run::{
    self as agent_run, Answer, BudgetFlags, EXIT_NEEDS_DECISION, JOURNAL_FILE, NewRunArgs,
    ResumeArgs, RunAction, RunArgs, RunCrash, RunError, parse_run_crash, parse_usd,
};
use zero_agent::secret::{StoreSecrets, cmd_secret_list};
use zero_agent::trust::{TrustCmdError, cmd_trust};
/// Переменная окружения с фильтром логов; без неё читается `RUST_LOG`.
const LOG_ENV: &str = "ZA_LOG";
/// Фильтр логов по умолчанию.
const DEFAULT_LOG: &str = "warn";
/// Сколько runtime ждёт фоновые задачи при выходе: зависший замер тома (`statvfs`) или
/// обход каталога отменить нельзя, команда не должна из-за них висеть.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// Код выхода удалённого флага (`run --agents-md`, D83): как ошибка аргументов.
const EXIT_REMOVED_FLAG: u8 = 2;

#[derive(Debug, Parser)]
#[command(
    name = "zero-agent",
    version,
    about = "zero-agent, этапы R0, R1-A и R1-B: журнал, эффекты, модели, ресурсы, цикл агента, контекст"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Проверка окружения: SQLite, llama.cpp (`LLAMA_URL`), GPU, модели, версия Rust.
    Doctor {
        /// Отчёт в JSON.
        #[arg(long)]
        json: bool,
    },
    /// Демонстрационный run; первая строка stdout — id run.
    DemoRun {
        /// Каталог проекта (создаётся при необходимости); в нём пишется `log.txt`.
        #[arg(long)]
        project: PathBuf,
        /// Число шагов.
        #[arg(long, default_value_t = 20)]
        steps: u32,
        /// Продолжить существующий run.
        #[arg(long, value_parser = parse_run)]
        resume: Option<RunId>,
        /// Аварийно завершить процесс: `intent:N` или `execute:N`.
        #[arg(long, value_parser = parse_crash)]
        crash_at: Option<CrashPoint>,
    },
    /// Печатает хэш значимых событий run.
    Replay {
        /// Каталог проекта.
        #[arg(long)]
        project: PathBuf,
        /// id run.
        #[arg(long, value_parser = parse_run)]
        run: RunId,
    },
    /// Модели из `ZA_HOME/config.toml` с состоянием после первого зонда.
    Models {
        /// Вывод в JSON.
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: Option<ModelsCmd>,
    },
    /// Проверочный вопрос модели через Gateway: текст — в stdout, usage — в stderr.
    Ask {
        /// id эндпоинта; без него — `models.default`.
        #[arg(long)]
        model: Option<String>,
        /// Класс приоритета заявки в очереди.
        #[arg(long, value_enum, default_value_t = AskPriority::Interactive)]
        priority: AskPriority,
        /// Текст вопроса.
        text: String,
    },
    /// Ресурсы узла, очередь к моделям и эндпоинты.
    Resources {
        /// Вывод в JSON (с `--watch` — по строке на снимок).
        #[arg(long)]
        json: bool,
        /// Печатать каждые 2 с до прерывания.
        #[arg(long)]
        watch: bool,
    },
    /// Run агента: ответ — в stdout, первая строка stderr — `run <id>`; Ctrl+C — пауза.
    Run(RunCli),
    /// Ответ на прерывание run: вопрос модели, одобрение вызова, доверие к проекту,
    /// неизвестный исход. Одобрение и доверие пишутся в журнал аудита.
    #[command(group(
        ArgGroup::new("reply")
            .required(true)
            .args(["text", "approve", "deny", "done", "not_done"])
    ))]
    Answer {
        /// Каталог проекта.
        #[arg(long)]
        project: PathBuf,
        /// id прерывания из подсказки `run`.
        #[arg(long)]
        interrupt: String,
        /// Ответ на вопрос модели (`input-…`).
        #[arg(long)]
        text: Option<String>,
        /// Одобрить вызов (`appr-…`) или довериться проекту (`trust-…`).
        #[arg(long)]
        approve: bool,
        /// Отказать в вызове (`appr-…`) или в доверии к проекту (`trust-…`).
        #[arg(long)]
        deny: bool,
        /// Заметка к отказу.
        #[arg(long, requires = "deny")]
        note: Option<String>,
        /// Действие выполнено (`unk-…`).
        #[arg(long)]
        done: bool,
        /// Действие не выполнено (`unk-…`).
        #[arg(long)]
        not_done: bool,
    },
    /// Журнал аудита проекта: одобрения, гранты, доверие; `--verify` — проверка
    /// hash-цепочки (разрыв — код 1).
    Audit {
        /// Каталог проекта.
        #[arg(long)]
        project: PathBuf,
        /// Только проверить цепочку.
        #[arg(long)]
        verify: bool,
        /// Вывод в JSON (строка на запись).
        #[arg(long)]
        json: bool,
    },
    /// Чекпоинты рабочей копии проекта: список и восстановление.
    Checkpoints {
        #[command(subcommand)]
        action: CheckpointsCmd,
    },
    /// Журнал проекта.
    Journal {
        #[command(subcommand)]
        action: JournalCmd,
    },
    /// Доверие к проекту заранее (D83): AGENTS.md, CLAUDE.md, .zero-agent/config.toml и
    /// скиллы читаются, команды выполняются по режиму; набор доверенных файлов пишется в
    /// журнал аудита. Изменение этих файлов снимает доверие до нового.
    Trust {
        /// Каталог проекта.
        #[arg(long)]
        project: PathBuf,
        /// Отозвать доверие.
        #[arg(long)]
        revoke: bool,
    },
    /// Секреты агента: имена и задан ли секрет, без значений.
    Secret {
        #[command(subcommand)]
        action: SecretCmd,
    },
    /// Запрос хода к модели по журналу: блоки с байтами, токенами и хэшем, расход,
    /// предсказание следующего хода; `--raw` — тело запроса провайдеру.
    Inspect(InspectCli),
    /// Метрики run по журналу: токены и кэш, время до первого токена, скорость, ошибки,
    /// ремонт вызовов, компакции, стоимость.
    Stats {
        /// Каталог проекта.
        #[arg(long)]
        project: PathBuf,
        /// id run.
        #[arg(long, value_parser = parse_run)]
        run: RunId,
        /// Вывод в JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Аргументы `inspect`.
#[derive(Debug, clap::Args)]
struct InspectCli {
    /// Каталог проекта.
    #[arg(long)]
    project: PathBuf,
    /// id run.
    #[arg(long, value_parser = parse_run)]
    run: RunId,
    /// Номер хода (с 1); без него — последний ход.
    #[arg(long)]
    turn: Option<u32>,
    /// Точное тело запроса провайдеру (без ключей) вместо таблицы блоков.
    #[arg(long, conflicts_with = "json")]
    raw: bool,
    /// Вывод в JSON.
    #[arg(long)]
    json: bool,
}

/// Аргументы `run`.
#[derive(Debug, clap::Args)]
struct RunCli {
    /// Каталог проекта (должен существовать).
    #[arg(long)]
    project: PathBuf,
    /// Продолжить run.
    #[arg(long, value_parser = parse_run)]
    resume: Option<RunId>,
    /// id эндпоинта; без него — `models.default`. С `--resume` — смена модели на границе шага.
    #[arg(long)]
    model: Option<String>,
    /// Режим run; по умолчанию `ask`.
    #[arg(long, value_enum, conflicts_with = "resume")]
    mode: Option<ModeArg>,
    /// Режим работы (overlay промпта).
    #[arg(long, value_enum, conflicts_with = "resume")]
    work_mode: Option<WorkModeArg>,
    /// Тир промпта; без него — по размеру контекста модели.
    #[arg(long, value_enum, conflicts_with = "resume")]
    tier: Option<TierArg>,
    /// Предел ожидания в очереди к модели, с; без него — без предела.
    #[arg(long, value_name = "СЕК", conflicts_with = "resume")]
    max_wait: Option<u64>,
    /// Предел шагов поверх `[budgets.run]`.
    #[arg(long)]
    max_steps: Option<u32>,
    /// Предел токенов поверх `[budgets.run]`.
    #[arg(long)]
    max_tokens: Option<u64>,
    /// Предел стоимости, USD, поверх `[budgets.run]`.
    #[arg(long, value_name = "USD", value_parser = parse_usd)]
    max_usd: Option<f64>,
    /// Предел времени GPU, мс, поверх `[budgets.run]`.
    #[arg(long, value_name = "МС")]
    max_gpu_ms: Option<u64>,
    /// Предел активного времени run, с, поверх `[budgets.run]` (`duration_ms`).
    #[arg(long, value_name = "СЕК")]
    max_duration: Option<u64>,
    /// Удалён (D83): `AGENTS.md` читается у доверенного проекта (`zero-agent trust`).
    #[arg(long, hide = true)]
    agents_md: bool,
    /// Грант доступа вне проекта: `<путь>[=ro|=rw]` (по умолчанию `ro`); можно повторять.
    /// С `--resume` — добавляется к грантам run.
    #[arg(long = "grant", value_name = "ПУТЬ[=ro|=rw]", value_parser = parse_grant_flag)]
    grants: Vec<(PathBuf, GrantMode)>,
    /// Ввод пользователя для продолжаемого run.
    #[arg(long, requires = "resume")]
    input: Option<String>,
    /// Аварийно завершить процесс: `after-delta:N` (тесты, D33).
    #[arg(long, hide = true, value_parser = parse_run_crash)]
    crash_at: Option<RunCrash>,
    /// Текст задачи.
    #[arg(required_unless_present = "resume", conflicts_with = "resume")]
    task: Option<String>,
}

/// Режим run (CORE-106).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModeArg {
    ReadOnly,
    Plan,
    Ask,
    AcceptEdits,
    Auto,
    /// Без вопросов, кроме необратимых и путей вне проекта; только при
    /// `[policy] allow_bypass = true` глобальной конфигурации (D80).
    Bypass,
}

impl From<ModeArg> for RunMode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::ReadOnly => RunMode::ReadOnly,
            ModeArg::Plan => RunMode::Plan,
            ModeArg::Ask => RunMode::Ask,
            ModeArg::AcceptEdits => RunMode::AcceptEdits,
            ModeArg::Auto => RunMode::Auto,
            ModeArg::Bypass => RunMode::Bypass,
        }
    }
}

/// Режим работы (A.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum WorkModeArg {
    Coder,
    Researcher,
    Operator,
    Writer,
}

impl From<WorkModeArg> for WorkMode {
    fn from(m: WorkModeArg) -> Self {
        match m {
            WorkModeArg::Coder => WorkMode::Coder,
            WorkModeArg::Researcher => WorkMode::Researcher,
            WorkModeArg::Operator => WorkMode::Operator,
            WorkModeArg::Writer => WorkMode::Writer,
        }
    }
}

/// Тир промпта (PRM-002, D20).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum TierArg {
    Small,
    Standard,
    Full,
}

impl From<TierArg> for Tier {
    fn from(t: TierArg) -> Self {
        match t {
            TierArg::Small => Tier::Small,
            TierArg::Standard => Tier::Standard,
            TierArg::Full => Tier::Full,
        }
    }
}

fn run_args(r: RunCli) -> RunArgs {
    let action = match r.resume {
        Some(run) => RunAction::Resume(ResumeArgs {
            run,
            model: r.model,
            input: r.input,
        }),
        None => RunAction::New(NewRunArgs {
            model: r.model,
            run_mode: r.mode.map(Into::into).unwrap_or_default(),
            work_mode: r.work_mode.map(Into::into),
            tier: r.tier.map(Into::into),
            max_wait: r.max_wait.map(Duration::from_secs),
            task: r.task.unwrap_or_default(),
        }),
    };
    RunArgs {
        project: r.project,
        action,
        budget: BudgetFlags {
            steps: r.max_steps,
            tokens: r.max_tokens,
            usd: r.max_usd,
            gpu_ms: r.max_gpu_ms,
            duration_secs: r.max_duration,
        },
        grants: r.grants,
        crash: r.crash_at,
    }
}

#[derive(Debug, Subcommand)]
enum JournalCmd {
    /// События run в JSONL по порядку `seq`, секреты замаскированы (значения
    /// `ZA_SECRET_*` и шаблоны ключей).
    Export {
        /// Каталог проекта.
        #[arg(long)]
        project: PathBuf,
        /// id run.
        #[arg(long, value_parser = parse_run)]
        run: RunId,
    },
}

#[derive(Debug, Subcommand)]
enum SecretCmd {
    /// Имена секретов (`key_ref` из config.toml и переменные `ZA_SECRET_*`), отметка
    /// «задан»; значения не печатаются.
    List,
}

#[derive(Debug, Subcommand)]
enum CheckpointsCmd {
    /// Чекпоинты проекта: id, вид, коммит.
    List {
        /// Каталог проекта.
        #[arg(long)]
        project: PathBuf,
        /// Вывод в JSON (строка на чекпоинт).
        #[arg(long)]
        json: bool,
    },
    /// Восстановить рабочую копию по чекпоинту; перед этим делается новый чекпоинт,
    /// восстановление пишется в журнал аудита.
    Restore {
        /// Каталог проекта.
        #[arg(long)]
        project: PathBuf,
        /// id чекпоинта `<run>/<шаг>-<номер>`.
        id: String,
        /// Удалить файлы, которых нет в чекпоинте (кроме игнорируемых).
        #[arg(long)]
        delete_new: bool,
        /// Итог в JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ModelsCmd {
    /// Повторный зонд эндпоинта и сохранение профиля; код 0 — эндпоинт готов.
    Probe {
        /// id эндпоинта.
        id: String,
    },
}

/// Приоритет `ask` (RES-003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AskPriority {
    Interactive,
    Top,
    Worker,
    Job,
}

impl From<AskPriority> for Priority {
    fn from(p: AskPriority) -> Self {
        match p {
            AskPriority::Interactive => Priority::Interactive,
            AskPriority::Top => Priority::Top,
            AskPriority::Worker => Priority::Worker,
            AskPriority::Job => Priority::Job,
        }
    }
}

/// Ошибки команд.
#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error(transparent)]
    NoHome(#[from] NoHome),
    #[error("{what} {}: {source}", path.display())]
    Io {
        what: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("журнал проекта {} не найден: в нём ещё не было run", .0.display())]
    NoJournal(PathBuf),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Effect(#[from] EffectError),
    #[error("асинхронная среда: {0}")]
    Runtime(std::io::Error),
    #[error("отчёт в JSON: {0}")]
    Json(serde_json::Error),
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    Inspect(#[from] InspectCmdError),
    #[error(transparent)]
    Audit(#[from] AuditCmdError),
    #[error(transparent)]
    Checkpoints(#[from] CheckpointsCmdError),
    #[error(transparent)]
    Trust(#[from] TrustCmdError),
}

fn parse_run(s: &str) -> Result<RunId, String> {
    uuid::Uuid::parse_str(s.trim())
        .map(RunId)
        .map_err(|e| format!("неверный id run {s:?}: {e}"))
}

fn parse_crash(s: &str) -> Result<CrashPoint, String> {
    let bad = || format!("ожидается intent:N или execute:N, получено {s:?}");
    let (kind, n) = s.split_once(':').ok_or_else(bad)?;
    let n: u32 = n.trim().parse().map_err(|_| bad())?;
    match kind.trim() {
        "intent" => Ok(CrashPoint::AfterIntent(n)),
        "execute" => Ok(CrashPoint::AfterExecute(n)),
        _ => Err(bad()),
    }
}

/// Ставит подписчик `tracing`: текст в stderr, фильтр из `ZA_LOG` или
/// `RUST_LOG`, иначе [`DEFAULT_LOG`]. Неразборчивый фильтр — предупреждение
/// и уровень по умолчанию.
fn init_logging() {
    let spec = [LOG_ENV, "RUST_LOG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()));
    let filter = match spec.as_deref().map(str::parse::<Targets>) {
        Some(Ok(t)) => t,
        Some(Err(e)) => {
            eprintln!("zero-agent: фильтр логов не разобран ({e}), уровень {DEFAULT_LOG}");
            default_filter()
        }
        None => default_filter(),
    };
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false),
        )
        .with(filter);
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        eprintln!("zero-agent: подписчик логов уже установлен");
    }
}

fn default_filter() -> Targets {
    Targets::new().with_default(tracing::Level::WARN)
}

fn main() -> ExitCode {
    init_logging();
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            // Справка и версия — не ошибка.
            return if e.use_stderr() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("zero-agent: ошибка: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode, AppError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(AppError::Runtime)?;
    let result = rt.block_on(dispatch(cli.cmd));
    // Фоновые задачи (зонды, монитор, обход каталогов) отменяются; блокирующие вызовы
    // ждутся не дольше `SHUTDOWN_TIMEOUT`.
    rt.shutdown_timeout(SHUTDOWN_TIMEOUT);
    result
}

async fn dispatch(cmd: Cmd) -> Result<ExitCode, AppError> {
    match cmd {
        Cmd::Doctor { json } => cmd_doctor(json).await,
        Cmd::DemoRun {
            project,
            steps,
            resume,
            crash_at,
        } => cmd_demo(&project, steps, resume, crash_at).await,
        Cmd::Replay { project, run } => cmd_replay(&project, run).await,
        Cmd::Models { json, action } => {
            let home = home::za_home()?;
            Ok(match action {
                None => models::cmd_models(&home, json).await?,
                Some(ModelsCmd::Probe { id }) => models::cmd_probe(&home, &id, json).await?,
            })
        }
        Cmd::Ask {
            model,
            priority,
            text,
        } => {
            let home = home::za_home()?;
            let args = AskArgs {
                model,
                priority: priority.into(),
                text,
            };
            Ok(models::cmd_ask(&home, args).await?)
        }
        Cmd::Resources { json, watch } => {
            let home = home::za_home()?;
            Ok(resources::cmd_resources(&home, json, watch).await?)
        }
        Cmd::Run(r) => {
            if r.agents_md {
                // D83: флаг удалён, инструкции проекта — по доверию.
                eprintln!(
                    "zero-agent: флаг --agents-md удалён: AGENTS.md (или CLAUDE.md) читается у доверенного проекта; доверие: zero-agent trust --project \"{}\"",
                    r.project.display()
                );
                return Ok(ExitCode::from(EXIT_REMOVED_FLAG));
            }
            let home = home::za_home()?;
            Ok(agent_run::cmd_run(&home, run_args(r)).await?)
        }
        Cmd::Answer {
            project,
            interrupt,
            text,
            approve,
            deny,
            note,
            done,
            not_done: _,
        } => {
            let home = home::za_home()?;
            // Ровно один вид ответа обеспечивает группа `reply`.
            let answer = match text {
                Some(t) => Answer::Text(t),
                None if approve => Answer::Approve,
                None if deny => Answer::Deny(note),
                None if done => Answer::Done,
                None => Answer::NotDone,
            };
            Ok(agent_run::cmd_answer(&home, &project, &interrupt, answer).await?)
        }
        Cmd::Inspect(i) => {
            let home = home::za_home()?;
            let args = InspectArgs {
                project: i.project,
                run: i.run,
                turn: i.turn,
                raw: i.raw,
                json: i.json,
            };
            Ok(inspector::cmd_inspect(&home, args).await?)
        }
        Cmd::Stats { project, run, json } => {
            let home = home::za_home()?;
            Ok(inspector::cmd_stats(&home, &project, run, json).await?)
        }
        Cmd::Audit {
            project,
            verify,
            json,
        } => {
            let home = home::za_home()?;
            let masker = StoreSecrets::from_env().masker();
            Ok(audit_cmd::cmd_audit(&home, &project, verify, json, &masker).await?)
        }
        Cmd::Checkpoints { action } => {
            let home = home::za_home()?;
            Ok(match action {
                CheckpointsCmd::List { project, json } => {
                    checkpoints_cmd::cmd_list(&home, &project, json).await?
                }
                CheckpointsCmd::Restore {
                    project,
                    id,
                    delete_new,
                    json,
                } => checkpoints_cmd::cmd_restore(&home, &project, &id, delete_new, json).await?,
            })
        }
        Cmd::Journal {
            action: JournalCmd::Export { project, run },
        } => {
            let home = home::za_home()?;
            let masker = StoreSecrets::from_env().masker();
            Ok(audit_cmd::cmd_journal_export(&home, &project, run, &masker).await?)
        }
        Cmd::Trust { project, revoke } => {
            let home = home::za_home()?;
            Ok(cmd_trust(&home, &project, revoke).await?)
        }
        Cmd::Secret {
            action: SecretCmd::List,
        } => {
            let home = home::za_home()?;
            Ok(cmd_secret_list(&home))
        }
    }
}

async fn cmd_doctor(json: bool) -> Result<ExitCode, AppError> {
    let url = std::env::var(LLAMA_URL_ENV).ok();
    let report = doctor::run(url.as_deref(), home::za_home().ok().as_deref()).await;
    if json {
        let text = serde_json::to_string_pretty(&report).map_err(AppError::Json)?;
        println!("{text}");
    } else {
        print!("{}", report.render());
    }
    Ok(if report.has_failure() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

async fn cmd_demo(
    project: &Path,
    steps: u32,
    resume: Option<RunId>,
    crash_at: Option<CrashPoint>,
) -> Result<ExitCode, AppError> {
    std::fs::create_dir_all(project).map_err(|source| AppError::Io {
        what: "каталог проекта",
        path: project.to_path_buf(),
        source,
    })?;
    let dir = journal_dir(project)?;
    // Продолжение часто идёт сразу после аварийного завершения: блокировка
    // убитого процесса может сниматься не мгновенно.
    let store = if resume.is_some() {
        open_retrying(&dir)?
    } else {
        ProjectStore::open(&dir)?
    };
    let run = match resume {
        Some(run) => {
            if store.get_run(run).await?.is_none() {
                return Err(StoreError::UnknownRun(run).into());
            }
            run
        }
        None => RunId::new(),
    };
    // id run нужен до шагов: при аварийном завершении по нему продолжают.
    println!("{run}");
    let _ = std::io::stdout().flush();

    let cfg = DemoConfig {
        steps,
        workdir: project.to_path_buf(),
        crash_at,
    };
    match run_demo(&store, run, &cfg).await? {
        DemoOutcome::Done => {
            println!("done");
            Ok(ExitCode::SUCCESS)
        }
        DemoOutcome::NeedsDecision(ids) => {
            println!("needs_decision");
            for id in &ids {
                println!("{id}");
            }
            eprintln!(
                "zero-agent: исход необратимых действий неизвестен, run приостановлен до решения пользователя"
            );
            Ok(ExitCode::from(EXIT_NEEDS_DECISION))
        }
    }
}

async fn cmd_replay(project: &Path, run: RunId) -> Result<ExitCode, AppError> {
    let dir = journal_dir(project)?;
    if !dir.join(JOURNAL_FILE).is_file() {
        return Err(AppError::NoJournal(dir));
    }
    let store = open_retrying(&dir)?;
    println!("{}", replay_hash(&store, run).await?);
    Ok(ExitCode::SUCCESS)
}

/// Каталог журнала проекта в `ZA_HOME`.
fn journal_dir(project: &Path) -> Result<PathBuf, AppError> {
    home::journal_dir(&home::za_home()?, project).map_err(|source| AppError::Io {
        what: "каталог проекта",
        path: project.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_point_parses() {
        assert_eq!(parse_crash("intent:2"), Ok(CrashPoint::AfterIntent(2)));
        assert_eq!(parse_crash("execute:3"), Ok(CrashPoint::AfterExecute(3)));
        assert!(parse_crash("execute").is_err());
        assert!(parse_crash("receipt:1").is_err());
        assert!(parse_crash("intent:-1").is_err());
    }

    /// D78: `--grant <путь>=rw` — режим rw, без режима — ro; повтор флага — несколько
    /// грантов, `=` отделяет режим от пути Windows с `:`.
    #[test]
    fn grant_flag_parsed() {
        let cli = Cli::try_parse_from([
            "zero-agent",
            "run",
            "--project",
            "p",
            "--grant",
            r"D:\data=rw",
            "--grant",
            "/srv/ro",
            "задача",
        ])
        .unwrap();
        let Cmd::Run(r) = cli.cmd else { panic!() };
        let args = run_args(r);
        assert_eq!(args.grants.len(), 2);
        assert_eq!(args.grants[0], (PathBuf::from(r"D:\data"), GrantMode::Rw));
        assert_eq!(args.grants[1].1, GrantMode::Ro);
        // С `--resume` грант допустим (добавляется с границы, D92).
        let id = RunId::new().to_string();
        assert!(
            Cli::try_parse_from([
                "zero-agent",
                "run",
                "--project",
                "p",
                "--resume",
                &id,
                "--grant",
                "/x"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "zero-agent",
                "run",
                "--project",
                "p",
                "--grant",
                "/x=wr",
                "t"
            ])
            .is_err()
        );
    }

    #[test]
    fn bypass_mode_parses() {
        let cli = Cli::try_parse_from([
            "zero-agent",
            "run",
            "--project",
            "p",
            "--mode",
            "bypass",
            "t",
        ])
        .unwrap();
        let Cmd::Run(r) = cli.cmd else { panic!() };
        assert_eq!(r.mode.map(RunMode::from), Some(RunMode::Bypass));
    }

    #[test]
    fn cli_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
