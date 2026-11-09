//! События журнала: запись и чтение по `seq`.
//!
//! `seq` назначает SQLite при вставке, поэтому порядок чтения определяется только
//! им, а не `ts` (часы могут идти назад).
//!
//! Payload длиннее [`BLOB_THRESHOLD`] байт канонического JSON уходит в blob
//! (ARC-032): в строке события остаётся `null` и ссылка `blob_ref`, при чтении
//! payload подставляется обратно, `blob_ref` в событии сохраняется.

use std::path::Path;

use jiff::Timestamp;
use rusqlite::{Connection, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use uuid::Uuid;
use za_protocol::{
    BlobHash, Event, EventId, EventKind, NewEvent, RunId, SCHEMA_V, Seq, canonical_json,
};

use crate::blobs::{self, BLOB_THRESHOLD};
use crate::{ProjectStore, StoreError};

/// MIME-тип blob с вынесенным payload.
const PAYLOAD_MIME: &str = "application/json";

impl ProjectStore {
    /// Добавляет событие; `Ok` возвращается после COMMIT с надёжностью
    /// [`StoreOptions::durability`](crate::StoreOptions::durability): при `Normal`
    /// событие переживает падение процесса, но может пропасть при отключении питания.
    pub async fn append(&self, e: NewEvent) -> Result<Seq, StoreError> {
        let dir = self.dir();
        self.write(move |c| insert(c, &dir, &e)).await
    }

    /// Добавляет событие с фиксацией `synchronous=FULL`: `Ok` означает, что событие
    /// переживает и отключение питания. Для намерений `NonIdempotent`-эффектов.
    pub async fn append_durable(&self, e: NewEvent) -> Result<Seq, StoreError> {
        let dir = self.dir();
        self.write_durable(move |c| insert(c, &dir, &e)).await
    }

    /// Добавляет события атомарно, в одной транзакции; `seq` идут подряд по порядку.
    pub async fn append_batch(&self, e: Vec<NewEvent>) -> Result<Vec<Seq>, StoreError> {
        if e.is_empty() {
            return Ok(Vec::new());
        }
        let dir = self.dir();
        self.write(move |c| e.iter().map(|x| insert(c, &dir, x)).collect())
            .await
    }

    /// События с `seq > after` по возрастанию `seq`, не больше `limit`.
    pub async fn read_after(&self, after: Seq, limit: usize) -> Result<Vec<Event>, StoreError> {
        let dir = self.dir();
        self.read(move |c| {
            query(
                c,
                &dir,
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
        let dir = self.dir();
        self.read(move |c| {
            query(
                c,
                &dir,
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

/// Вставляет событие в открытой транзакции писателя; id и ts назначаются здесь.
/// Большой payload сначала сохраняется blob в той же транзакции.
pub(crate) fn insert(c: &Connection, dir: &Path, e: &NewEvent) -> Result<Seq, StoreError> {
    insert_at(c, dir, e, Timestamp::now())
}

/// Как [`insert`], но с отметкой времени вызывающего: так строка проекции
/// (например, `usage`) получает тот же `ts`, что и её событие.
pub(crate) fn insert_at(
    c: &Connection,
    dir: &Path,
    e: &NewEvent,
    ts: Timestamp,
) -> Result<Seq, StoreError> {
    let id = EventId::new();
    let mut payload = canonical_json(&e.payload);
    let mut blob_ref = None;
    if payload.len() > BLOB_THRESHOLD {
        blob_ref = Some(blobs::put(c, dir, payload.as_bytes(), Some(PAYLOAD_MIME))?.0);
        payload = canonical_json(&Value::Null);
    }
    c.execute(
        "INSERT INTO events (id, run_id, parent_seq, ts, actor, kind, schema_v, payload, blob_ref, tainted) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            id.0.as_bytes().as_slice(),
            e.run_id.0.as_bytes().as_slice(),
            e.parent_seq.map(seq_sql),
            ts.to_string(),
            name_of(e.actor)?,
            e.kind.as_str(),
            SCHEMA_V,
            payload,
            blob_ref,
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
    dir: &Path,
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
    rows.map(|r| decode(c, dir, r?)).collect()
}

fn decode(c: &Connection, dir: &Path, r: Row) -> Result<Event, StoreError> {
    let blob_ref = r.blob_ref.map(BlobHash);
    let mut payload: Value =
        serde_json::from_str(&r.payload).map_err(|e| corrupt(&format!("payload: {e}")))?;
    // `null` со ссылкой на blob — вынесенный payload; подставляем его обратно.
    if let (Some(h), Value::Null) = (&blob_ref, &payload) {
        let data = blobs::get(c, dir, h)?
            .ok_or_else(|| corrupt(&format!("нет blob {} для payload", h.0)))?;
        payload = serde_json::from_slice(&data)
            .map_err(|e| corrupt(&format!("payload из blob {}: {e}", h.0)))?;
    }
    Ok(Event {
        seq: seq_from(r.seq)?,
        id: EventId(uuid(&r.id)?),
        run_id: RunId(uuid(&r.run_id)?),
        parent_seq: r.parent_seq.map(seq_from).transpose()?,
        ts: r
            .ts
            .parse()
            .map_err(|e| corrupt(&format!("ts `{}`: {e}", r.ts)))?,
        actor: parse_name(r.actor, "actor")?,
        kind: EventKind::parse(&r.kind),
        schema_v: u16::try_from(r.schema_v).map_err(|_| corrupt("schema_v"))?,
        payload,
        blob_ref,
        tainted: r.tainted,
    })
}

/// Payload события `seq` (вынесенный в blob подставляется); `None`, если события нет.
pub(crate) fn payload_at(
    c: &Connection,
    dir: &Path,
    seq: Seq,
) -> Result<Option<Value>, StoreError> {
    let mut found = query(
        c,
        dir,
        &format!("{SELECT} WHERE seq = ?1"),
        params![seq_sql(seq)],
    )?;
    Ok(found.pop().map(|e| e.payload))
}

/// Имя варианта перечисления протокола в snake_case, как в его сериализации.
pub(crate) fn name_of<T: Serialize + std::fmt::Debug>(v: T) -> Result<String, StoreError> {
    match serde_json::to_value(&v) {
        Ok(Value::String(s)) => Ok(s),
        _ => Err(StoreError::Serde(format!("{v:?} не сериализуется в имя"))),
    }
}

/// Обратное к [`name_of`]; `what` называет поле в сообщении об ошибке.
pub(crate) fn parse_name<T: DeserializeOwned>(s: String, what: &str) -> Result<T, StoreError> {
    serde_json::from_value(Value::String(s)).map_err(|e| corrupt(&format!("{what}: {e}")))
}

pub(crate) fn uuid(b: &[u8]) -> Result<Uuid, StoreError> {
    Uuid::from_slice(b).map_err(|e| corrupt(&format!("uuid: {e}")))
}

pub(crate) fn seq_from(v: i64) -> Result<Seq, StoreError> {
    u64::try_from(v)
        .map(Seq)
        .map_err(|_| corrupt("отрицательный seq"))
}

pub(crate) fn seq_sql(s: Seq) -> i64 {
    i64::try_from(s.0).unwrap_or(i64::MAX)
}

fn limit_sql(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

pub(crate) fn corrupt(what: &str) -> StoreError {
    StoreError::Corrupt(what.to_owned())
}
