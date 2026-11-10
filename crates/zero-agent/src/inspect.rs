//! Команды `inspect` и `stats` (R1-B2 D52, D53; CTX-018, OPS-002, OPS-003).
//!
//! `inspect` показывает запрос хода, восстановленный по журналу (`za_agent::inspect`):
//! таблицу блоков с байтами, оценкой токенов и хэшем, оценку запроса, расход провайдера и
//! предсказание следующего хода; `--raw` — точное тело запроса провайдеру
//! (`ModelPort::render_request` запущенного по конфигурации `Gateway`), без ключей.
//! `stats` — метрики run по журналу (`za_agent::run_metrics`).
//!
//! Обе команды только читают журнал: наличие `journal.db` проверяется до открытия (иначе
//! `ProjectStore::open` создал бы пустой журнал), открытие — с повтором на `Locked`.
//! Коды выхода: 0 — готово; 1 — ошибка (нет журнала, run или хода, запрос не
//! восстанавливается, тело провайдеру не построено).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use za_agent::{InspectError, Inspection, RunMetrics, inspect, run_metrics};
use za_llm::{ModelCall, ModelPort, RequestMeta};
use za_protocol::{Priority, RunId};
use za_store::{ProjectStore, StoreError};

use crate::config::load_config;
use crate::models::{CommandError, PROBE_WAIT, start_gateway, wait_endpoint};
use crate::run::{EXIT_DONE, EXIT_ERROR, RunError, open_existing};

/// Символов хэша блока в таблице.
const HASH_CHARS: usize = 12;

/// Аргументы `inspect`.
#[derive(Debug, Clone)]
pub struct InspectArgs {
    pub project: PathBuf,
    pub run: RunId,
    /// Номер хода; `None` — последний.
    pub turn: Option<u32>,
    /// Тело запроса провайдеру вместо таблицы.
    pub raw: bool,
    pub json: bool,
}

/// Ошибка команд `inspect` и `stats`.
#[derive(Debug, thiserror::Error)]
pub enum InspectCmdError {
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Inspect(#[from] InspectError),
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error("отчёт в JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Журнал проекта и проверка, что run в нём есть.
async fn open_run(
    home: &Path,
    project: &Path,
    run: RunId,
) -> Result<ProjectStore, InspectCmdError> {
    let store = open_existing(home, project)?;
    if store.get_run(run).await?.is_none() {
        return Err(StoreError::UnknownRun(run).into());
    }
    Ok(store)
}

/// `zero-agent inspect`.
pub async fn cmd_inspect(home: &Path, args: InspectArgs) -> Result<ExitCode, InspectCmdError> {
    let store = open_run(home, &args.project, args.run).await?;
    let Some(i) = inspect(&store, args.run, args.turn).await? else {
        match args.turn {
            Some(n) => eprintln!("zero-agent: у run {} нет хода {n}", args.run),
            None => eprintln!("zero-agent: у run {} ещё нет ходов", args.run),
        }
        return Ok(ExitCode::from(EXIT_ERROR));
    };
    if args.raw {
        return raw_body(home, args.run, &i).await;
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&i)?);
    } else {
        print!("{}", render_inspection(args.run, &i));
    }
    Ok(ExitCode::from(EXIT_DONE))
}

/// `inspect --raw`: тело запроса провайдеру тем же кодом `Gateway`, что и в `run`.
async fn raw_body(home: &Path, run: RunId, i: &Inspection) -> Result<ExitCode, InspectCmdError> {
    let cfg = load_config(home).map_err(CommandError::from)?;
    let started = start_gateway(home, &cfg, false).await?;
    let gateway = started.gateway;
    // Тело строится по фактам зонда: идущий зонд дожидается.
    let _ = wait_endpoint(&gateway, &i.endpoint, PROBE_WAIT).await;
    let call = ModelCall {
        endpoint: i.endpoint.clone(),
        meta: RequestMeta {
            run_id: run,
            priority: Priority::Top,
            session: None,
            max_wait: None,
        },
        request: i.request.clone(),
    };
    match gateway.render_request(&call) {
        Some(body) => {
            println!("{body}");
            Ok(ExitCode::from(EXIT_DONE))
        }
        None => {
            eprintln!(
                "zero-agent: тело запроса к модели {} не построено: модель не настроена или ещё ни разу не зондирована",
                i.endpoint
            );
            Ok(ExitCode::from(EXIT_ERROR))
        }
    }
}

/// `zero-agent stats`.
pub async fn cmd_stats(
    home: &Path,
    project: &Path,
    run: RunId,
    json: bool,
) -> Result<ExitCode, InspectCmdError> {
    let store = open_run(home, project, run).await?;
    let m = run_metrics(&store, run).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&m)?);
    } else {
        print!("{}", render_metrics(run, &m));
    }
    Ok(ExitCode::from(EXIT_DONE))
}

/// Таблица блоков запроса хода.
pub fn render_inspection(run: RunId, i: &Inspection) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "run {run}, ход {}, модель {}, окно {} токенов",
        i.turn, i.endpoint, i.n_ctx
    );
    let _ = writeln!(out, "{:<14} {:>9} {:>8}  хэш", "блок", "байт", "токенов");
    for b in &i.blocks {
        let hash = b
            .hash
            .as_deref()
            .map_or("-", |h| h.get(..HASH_CHARS).unwrap_or(h));
        let _ = writeln!(out, "{:<14} {:>9} {:>8}  {hash}", b.name, b.bytes, b.tokens);
    }
    let input = i.input.map_or_else(|| "нет".to_owned(), |n| n.to_string());
    let _ = writeln!(
        out,
        "оценка запроса {} токенов, расход провайдера {input}, следующий ход около {}",
        i.estimate, i.next_turn
    );
    out
}

/// Метрики run текстом.
pub fn render_metrics(run: RunId, m: &RunMetrics) -> String {
    let mut out = String::new();
    let ms = |v: Option<u64>| v.map_or_else(|| "нет".to_owned(), |n| format!("{n} мс"));
    let _ = writeln!(out, "run {run}: ходов {}, шагов {}", m.turns, m.steps);
    let _ = writeln!(
        out,
        "токены: вход {} (из кэша {}, {:.1} %), выход {}",
        m.input,
        m.cached,
        m.cache_share * 100.0,
        m.output
    );
    let _ = writeln!(
        out,
        "время до первого токена: p50 {}, p95 {}",
        ms(m.ttft_p50_ms),
        ms(m.ttft_p95_ms)
    );
    let tps = m
        .output_tps
        .map_or_else(|| "нет".to_owned(), |v| format!("{v:.1} токенов/с"));
    let _ = writeln!(out, "скорость вывода: {tps}");
    let errors = if m.errors.is_empty() {
        "нет".to_owned()
    } else {
        m.errors
            .iter()
            .map(|(k, n)| format!("{k} {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let _ = writeln!(out, "ошибки модели: {errors}");
    let _ = writeln!(
        out,
        "ремонт вызовов: {}, компакций: {}",
        m.repairs, m.compactions
    );
    let _ = writeln!(out, "стоимость: ${:.4}", m.cost_usd);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use za_agent::BlockCost;
    use za_llm::{ChatRequest, EndpointId};

    #[test]
    fn inspection_table() {
        let i = Inspection {
            turn: 2,
            endpoint: EndpointId("local".into()),
            blocks: vec![
                BlockCost {
                    name: "tools",
                    bytes: 400,
                    tokens: 100,
                    hash: Some("0123456789abcdef".into()),
                },
                BlockCost {
                    name: "dialog",
                    bytes: 8,
                    tokens: 6,
                    hash: None,
                },
            ],
            estimate: 106,
            input: None,
            n_ctx: 8192,
            next_turn: 120,
            request: ChatRequest::default(),
        };
        let run = RunId::new();
        let t = render_inspection(run, &i);
        assert!(t.starts_with(&format!("run {run}, ход 2, модель local, окно 8192")));
        assert!(
            t.contains("tools                400      100  0123456789ab\n"),
            "{t}"
        );
        assert!(t.contains("dialog                 8        6  -\n"), "{t}");
        assert!(
            t.ends_with("расход провайдера нет, следующий ход около 120\n"),
            "{t}"
        );
    }

    #[test]
    fn metrics_text() {
        let m = RunMetrics {
            turns: 2,
            steps: 3,
            input: 220,
            cached: 180,
            cache_share: 180.0 / 220.0,
            output: 30,
            ttft_p50_ms: Some(200),
            ttft_p95_ms: None,
            output_tps: Some(75.0),
            errors: [("server".to_owned(), 1)].into_iter().collect(),
            repairs: 1,
            compactions: 0,
            cost_usd: 0.5,
        };
        let t = render_metrics(RunId::new(), &m);
        assert!(t.contains("ходов 2, шагов 3"), "{t}");
        assert!(t.contains("из кэша 180, 81.8 %"), "{t}");
        assert!(t.contains("p50 200 мс, p95 нет"), "{t}");
        assert!(t.contains("ошибки модели: server 1"), "{t}");
        assert!(t.contains("стоимость: $0.5000"), "{t}");
    }
}
