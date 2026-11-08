//! События журнала: запись и чтение по `seq`.
//!
//! `seq` назначает SQLite при вставке, поэтому порядок чтения определяется только
//! им, а не `ts` (часы могут идти назад).

use jiff::Timestamp;
use rusqlite::{Connection, params};
use serde_json::Value;
use uuid::Uuid;
use za_protocol::{
    Actor, BlobHash, Event, EventId, EventKind, NewEvent, RunId, SCHEMA_V, Seq, canonical_json,
};

use crate::{ProjectStore, StoreError};

impl ProjectStore {
    /// Добавляет событие; `Ok` возвращается после фиксации на диске.
    pub async fn append(&self, e: NewEvent) -> Result<Seq, StoreError> {
        self.write(move |c| insert(c, &e)).await
    }

    /// Добавляет события атомарно, в одной транзакции; `seq` идут подряд по порядку.
    pub async fn append_batch(&self, e: Vec<NewEvent>) -> Result<Vec<Seq>, StoreError> {
        if e.is_empty() {
            return Ok(Vec::new());
        }
        self.write(move |c| e.iter().map(|x| insert(c, x)).collect())
            .await
    }

    /// События с `seq > after` по возрастанию `seq`, не больше `limit`.
    pub async fn read_after(&self, after: Seq, limit: usize) -> Result<Vec<Event>, StoreError> {
        self.read(move |c| {
            query(
                c,
                &format!("{SELECT} WHERE seq > ?1 ORDER BY seq LIMIT ?2"),
                params![seq_sql(after), limit_sql(limit)],
            )
        })
        .await
    }

    /// События run с `seq > after` по возрастанию `seq`, не больше `limit`.
    pub async fn read_run(
        &self,
        run: RunId,
        after: Seq,
        limit: usize,
    ) -> Result<Vec<Event>, StoreError> {
        self.read(move |c| {
            query(
                c,
                &format!("{SELECT} WHERE run_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3"),
                params![
                    run.0.as_bytes().as_slice(),
                    seq_sql(after),
                    limit_sql(limit)
                ],
            )
        })
        .await
    }
}

const SELECT: &str = "SELECT seq, id, run_id, parent_seq, ts, actor, kind, schema_v, \
                      payload, blob_ref, tainted FROM events";

/// Вставляет событие; id и ts назначаются здесь.
fn insert(c: &Connection, e: &NewEvent) -> Result<Seq, StoreError> {
    let id = EventId::new();
    c.execute(
        "INSERT INTO events (id, run_id, parent_seq, ts, actor, kind, schema_v, payload, blob_ref, tainted) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9)",
        params![
            id.0.as_bytes().as_slice(),
            e.run_id.0.as_bytes().as_slice(),
            e.parent_seq.map(seq_sql),
            Timestamp::now().to_string(),
            actor_name(e.actor)?,
            e.kind.as_str(),
            SCHEMA_V,
            canonical_json(&e.payload),
            e.tainted,
        ],
    )?;
    let seq = u64::try_from(c.last_insert_rowid()).map_err(|_| corrupt("отрицательный seq"))?;
    Ok(Seq(seq))
}

/// Строка таблицы `events` в исходном виде.
struct Row {
    seq: i64,
    id: Vec<u8>,
    run_id: Vec<u8>,
    parent_seq: Option<i64>,
    ts: String,
    actor: String,
    kind: String,
    schema_v: i64,
    payload: String,
    blob_ref: Option<String>,
    tainted: bool,
}

fn query(
    c: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Event>, StoreError> {
    let mut stmt = c.prepare(sql)?;
    let rows = stmt.query_map(params, |r| {
        Ok(Row {
            seq: r.get(0)?,
            id: r.get(1)?,
            run_id: r.get(2)?,
            parent_seq: r.get(3)?,
            ts: r.get(4)?,
            actor: r.get(5)?,
            kind: r.get(6)?,
            schema_v: r.get(7)?,
            payload: r.get(8)?,
            blob_ref: r.get(9)?,
            tainted: r.get(10)?,
        })
    })?;
    rows.map(|r| decode(r?)).collect()
}

fn decode(r: Row) -> Result<Event, StoreError> {
    Ok(Event {
        seq: seq_from(r.seq)?,
        id: EventId(uuid(&r.id)?),
        run_id: RunId(uuid(&r.run_id)?),
        parent_seq: r.parent_seq.map(seq_from).transpose()?,
        ts: r
            .ts
            .parse()
            .map_err(|e| corrupt(&format!("ts `{}`: {e}", r.ts)))?,
        actor: serde_json::from_value(Value::String(r.actor))
            .map_err(|e| corrupt(&format!("actor: {e}")))?,
        kind: EventKind::parse(&r.kind),
        schema_v: u16::try_from(r.schema_v).map_err(|_| corrupt("schema_v"))?,
        payload: serde_json::from_str(&r.payload).map_err(|e| corrupt(&format!("payload: {e}")))?,
        blob_ref: r.blob_ref.map(BlobHash),
        tainted: r.tainted,
    })
}

/// Имя актора в snake_case, как в сериализации протокола.
fn actor_name(a: Actor) -> Result<String, StoreError> {
    match serde_json::to_value(a) {
        Ok(Value::String(s)) => Ok(s),
        _ => Err(StoreError::Sqlite(format!("актор {a:?} не сериализуется"))),
    }
}

fn uuid(b: &[u8]) -> Result<Uuid, StoreError> {
    Uuid::from_slice(b).map_err(|e| corrupt(&format!("uuid: {e}")))
}

fn seq_from(v: i64) -> Result<Seq, StoreError> {
    u64::try_from(v)
        .map(Seq)
        .map_err(|_| corrupt("отрицательный seq"))
}

fn seq_sql(s: Seq) -> i64 {
    i64::try_from(s.0).unwrap_or(i64::MAX)
}

fn limit_sql(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn corrupt(what: &str) -> StoreError {
    StoreError::Sqlite(format!("повреждённое событие: {what}"))
}
