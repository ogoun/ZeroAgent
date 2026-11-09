//! Бинарник `zero-agent`: `doctor`, `demo-run`, `replay` (R0), `models`, `ask`,
//! `resources` (R1-A).
//!
//! Коды выхода: 0 — готово, 1 — ошибка (в том числе неверные аргументы, ошибка
//! конфигурации, ошибка или неготовность модели), 3 — нужно решение пользователя.
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

use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use za_core::{CrashPoint, DemoConfig, DemoOutcome, EffectError, replay_hash, run_demo};
use za_protocol::{Priority, RunId};
use za_store::{ProjectStore, StoreError};
use zero_agent::doctor::{self, LLAMA_URL_ENV};
use zero_agent::home::{self, NoHome};
use zero_agent::journal::open_retrying;
use zero_agent::models::{self, AskArgs, CommandError};
use zero_agent::resources;

/// Код выхода «нужно решение пользователя».
const EXIT_NEEDS_DECISION: u8 = 3;
/// Имя файла журнала в каталоге журнала проекта.
const JOURNAL_FILE: &str = "journal.db";
/// Переменная окружения с фильтром логов; без неё читается `RUST_LOG`.
const LOG_ENV: &str = "ZA_LOG";
/// Фильтр логов по умолчанию.
const DEFAULT_LOG: &str = "warn";
/// Сколько runtime ждёт фоновые задачи при выходе: зависший замер тома (`statvfs`) или
/// обход каталога отменить нельзя, команда не должна из-за них висеть.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Parser)]
#[command(
    name = "zero-agent",
    version,
    about = "zero-agent, этапы R0 и R1-A: журнал, эффекты, модели, ресурсы"
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

    #[test]
    fn cli_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
