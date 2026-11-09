//! Расход модели (таблица `usage`, миграция 0002): строка на каждый вызов и суммы по run.
//!
//! Каждая строка пишется в одной транзакции с событием `model.usage` и хранит его
//! `seq`: журнал остаётся источником истины, таблица — индекс для бюджетов (CORE-113).

use jiff::Timestamp;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use za_protocol::{Actor, EventKind, NewEvent, RunId, Seq};

use crate::events::{self, corrupt, seq_sql};
use crate::runs::{ensure_run, id_sql};
use crate::{ProjectStore, StoreError};

/// Расход одного вызова модели.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageRow {
    /// Идентификатор эндпоинта модели.
    pub model: String,
    /// Входные токены, включая кэшированные.
    pub input: u64,
    /// Из них прочитано из кэша.
    pub cached: u64,
    pub output: u64,
    pub reasoning: u64,
    /// Время GPU, мс; `None`, если неизвестно.
    pub gpu_ms: Option<u64>,
    /// Стоимость, доллары США; `None`, если неизвестна.
    pub cost_usd: Option<f64>,
}

/// Суммы расхода run; неизвестные `gpu_ms` и `cost_usd` считаются нулём.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    /// Число вызовов модели.
    pub calls: u64,
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub reasoning: u64,
    pub gpu_ms: u64,
    pub cost_usd: f64,
}

impl ProjectStore {
    /// Событие `model.usage` (actor `model`, payload — как передан) и строка `usage`
    /// в одной транзакции. Неизвестный run — [`StoreError::UnknownRun`].
    pub async fn record_usage(
        &self,
        run: RunId,
        row: UsageRow,
        payload: Value,
    ) -> Result<Seq, StoreError> {
        let dir = self.dir();
        self.write(move |c| {
            ensure_run(c, run)?;
            let ev = NewEvent {
                run_id: run,
                parent_seq: None,
                actor: Actor::Model,
                kind: EventKind::ModelUsage,
                payload,
                tainted: false,
            };
            let seq = events::insert(c, &dir, &ev)?;
            c.execute(
                "INSERT INTO usage (run_id, seq, model, input, cached, output, reasoning, \
                 gpu_ms, cost_eq, ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    id_sql(run),
                    seq_sql(seq),
                    row.model,
                    count_sql(row.input)?,
                    count_sql(row.cached)?,
                    count_sql(row.output)?,
                    count_sql(row.reasoning)?,
                    row.gpu_ms.map(count_sql).transpose()?,
                    row.cost_usd,
                    Timestamp::now().to_string(),
                ],
            )?;
            Ok(seq)
        })
        .await
    }

    /// Суммы расхода run; у run без вызовов (в том числе неизвестного) — нули.
    pub async fn usage_totals(&self, run: RunId) -> Result<UsageTotals, StoreError> {
        self.read(move |c| totals(c, run)).await
    }
}

fn totals(c: &Connection, run: RunId) -> Result<UsageTotals, StoreError> {
    let (calls, input, cached, output, reasoning, gpu_ms, cost_usd) = c.query_row(
        "SELECT count(*), COALESCE(SUM(input), 0), COALESCE(SUM(cached), 0), \
         COALESCE(SUM(output), 0), COALESCE(SUM(reasoning), 0), COALESCE(SUM(gpu_ms), 0), \
         COALESCE(SUM(cost_eq), 0.0) FROM usage WHERE run_id = ?1",
        [id_sql(run)],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, f64>(6)?,
            ))
        },
    )?;
    Ok(UsageTotals {
        calls: count_from(calls)?,
        input: count_from(input)?,
        cached: count_from(cached)?,
        output: count_from(output)?,
        reasoning: count_from(reasoning)?,
        gpu_ms: count_from(gpu_ms)?,
        cost_usd,
    })
}

/// Счётчик для столбца INTEGER; больше `i64::MAX` не помещается в SQLite.
fn count_sql(v: u64) -> Result<i64, StoreError> {
    i64::try_from(v).map_err(|_| StoreError::Serde(format!("счётчик расхода {v} вне диапазона")))
}

fn count_from(v: i64) -> Result<u64, StoreError> {
    u64::try_from(v).map_err(|_| corrupt("отрицательный счётчик расхода"))
}
