//! Бинарник `zero-agent` этапа R0: `doctor`, `demo-run`, `replay`.
//!
//! Коды выхода: 0 — готово, 1 — ошибка (в том числе неверные аргументы),
//! 3 — нужно решение пользователя. Журнал проекта лежит в
//! `ZA_HOME/projects/<project_id>/` (ARC-020), `log.txt` демонстрации — в
//! каталоге проекта.
//!
//! Логи `tracing` ядра и хранилища пишутся в stderr; уровень по умолчанию
//! `warn`, фильтр задаётся `ZA_LOG` (приоритетнее) или `RUST_LOG` в формате
//! `info` или `warn,za_store=debug`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use za_core::{CrashPoint, DemoConfig, DemoOutcome, EffectError, replay_hash, run_demo};
use za_protocol::RunId;
use za_store::{ProjectStore, StoreError};
use zero_agent::doctor::{self, LLAMA_URL_ENV};
use zero_agent::home::{self, NoHome};
use zero_agent::journal::open_retrying;

/// Код выхода «нужно решение пользователя».
const EXIT_NEEDS_DECISION: u8 = 3;
/// Имя файла журнала в каталоге журнала проекта.
const JOURNAL_FILE: &str = "journal.db";
/// Переменная окружения с фильтром логов; без неё читается `RUST_LOG`.
const LOG_ENV: &str = "ZA_LOG";
/// Фильтр логов по умолчанию.
const DEFAULT_LOG: &str = "warn";

#[derive(Debug, Parser)]
#[command(
    name = "zero-agent",
    version,
    about = "zero-agent, этап R0: журнал, эффекты, зонды"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Проверка окружения: SQLite, llama.cpp (`LLAMA_URL`), версия Rust.
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
    match cli.cmd {
        Cmd::Doctor { json } => rt.block_on(cmd_doctor(json)),
        Cmd::DemoRun {
            project,
            steps,
            resume,
            crash_at,
        } => rt.block_on(cmd_demo(&project, steps, resume, crash_at)),
        Cmd::Replay { project, run } => rt.block_on(cmd_replay(&project, run)),
    }
}

async fn cmd_doctor(json: bool) -> Result<ExitCode, AppError> {
    let url = std::env::var(LLAMA_URL_ENV).ok();
    let report = doctor::run(url.as_deref()).await;
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
