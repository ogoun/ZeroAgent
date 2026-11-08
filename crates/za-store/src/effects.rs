//! Эффекты (намерение и квитанция, CORE-002) и прерывания.
//!
//! Журнал — источник истины: каждая запись в `effects` или `interrupts`
//! сопровождается событием (`effect.intent`, `effect.receipt`, `interrupt.opened`)
//! в той же транзакции. Намерение `NonIdempotent`-эффекта фиксируется с
//! `synchronous=FULL` (CORE-005): после `Ok` оно переживает отключение питания, и
//! необратимое действие не повторится вслепую.
//!
//! Результат хранится в событии квитанции `{key, result}`. Результат длиннее
//! [`BLOB_THRESHOLD`] байт канонического JSON уходит в blob: в событии остаётся
//! `{key, result_blob: <hash>}`, тот же хеш пишется в `effects.blob_ref`.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use za_protocol::{BlobHash, EffectClass, EffectKey, EventKind, RunId, Seq, canonical_json};

use crate::blobs::{self, BLOB_THRESHOLD};
use crate::events::{self, corrupt, name_of, parse_name, seq_from, seq_sql, uuid};
use crate::runs::{ensure_run, id_sql, runtime_event};
use crate::{ProjectStore, StoreError};

/// MIME-тип blob с результатом эффекта.
const RESULT_MIME: &str = "application/json";

/// Статус только что открытого прерывания.
const INTERRUPT_OPEN: &str = "open";

/// Эффект, как он записан в журнале.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectRow {
    pub key: EffectKey,
    pub class: EffectClass,
    /// `seq` события `effect.intent`.
    pub intent_seq: Seq,
    /// `seq` события `effect.receipt`; `None`, пока эффект не завершён.
    pub receipt_seq: Option<Seq>,
    /// Записанный результат; `Some` ровно тогда, когда есть квитанция.
    pub result: Option<Value>,
}

/// Прерывание run: ждёт решения человека или внешнего события.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptRow {
    /// Детерминированный идентификатор (CORE-007).
    pub id: String,
    pub run: RunId,
    /// Вид, например `unknown_outcome`.
    pub kind: String,
    /// Состояние; при открытии `open`.
    pub status: String,
    pub payload: Option<Value>,
}

impl ProjectStore {
    /// Записывает намерение эффекта и событие `effect.intent {key, class, name, args}`.
    ///
    /// Класс `NonIdempotent` фиксируется с `synchronous=FULL`, прочие — с надёжностью
    /// по умолчанию. Неизвестный run даёт [`StoreError::UnknownRun`], повторное
    /// намерение с тем же ключом — [`StoreError::EffectConflict`].
    pub async fn record_intent(
        &self,
        run: RunId,
        key: &EffectKey,
        class: EffectClass,
        name: &str,
        args: Value,
    ) -> Result<Seq, StoreError> {
        let dir = self.dir();
        let key = key.clone();
        let name = name.to_owned();
        let f = move |c: &Connection| {
            ensure_run(c, run)?;
            if load_row(c, run, &key)?.is_some() {
                return Err(StoreError::EffectConflict(key));
            }
            let payload = json!({
                "key": key.0,
                "class": class,
                "name": name,
                "args": args,
            });
            let seq = events::insert(
                c,
                &dir,
                &runtime_event(run, EventKind::EffectIntent, payload),
            )?;
            c.execute(
                "INSERT INTO effects (run_id, effect_key, class, intent_seq) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![id_sql(run), key.0, name_of(class)?, seq_sql(seq)],
            )?;
            Ok(seq)
        };
        if class == EffectClass::NonIdempotent {
            self.write_durable(f).await
        } else {
            self.write(f).await
        }
    }

    /// Записывает квитанцию эффекта и событие `effect.receipt`.
    ///
    /// Без намерения — [`StoreError::UnknownEffect`], повторная квитанция —
    /// [`StoreError::EffectConflict`]; отказы ничего не пишут.
    pub async fn record_receipt(
        &self,
        run: RunId,
        key: &EffectKey,
        result: Value,
    ) -> Result<Seq, StoreError> {
        let dir = self.dir();
        let key = key.clone();
        self.write(move |c| {
            match load_row(c, run, &key)? {
                None => return Err(StoreError::UnknownEffect(key)),
                Some(r) if r.receipt_seq.is_some() => return Err(StoreError::EffectConflict(key)),
                Some(_) => {}
            }
            let text = canonical_json(&result);
            let (payload, blob_ref) = if text.len() > BLOB_THRESHOLD {
                let h = blobs::put(c, &dir, text.as_bytes(), Some(RESULT_MIME))?;
                (json!({"key": key.0, "result_blob": h.0}), Some(h.0))
            } else {
                (json!({"key": key.0, "result": result}), None)
            };
            let seq = events::insert(
                c,
                &dir,
                &runtime_event(run, EventKind::EffectReceipt, payload),
            )?;
            c.execute(
                "UPDATE effects SET receipt_seq = ?1, blob_ref = ?2 \
                 WHERE run_id = ?3 AND effect_key = ?4",
                params![seq_sql(seq), blob_ref, id_sql(run), key.0],
            )?;
            Ok(seq)
        })
        .await
    }

    /// Эффект по ключу вместе с результатом; `None`, если намерения не было.
    pub async fn effect(
        &self,
        run: RunId,
        key: &EffectKey,
    ) -> Result<Option<EffectRow>, StoreError> {
        let dir = self.dir();
        let key = key.clone();
        self.read(move |c| {
            let Some(raw) = load_row(c, run, &key)? else {
                return Ok(None);
            };
            let result = match raw.receipt_seq {
                None => None,
                Some(seq) => Some(load_result(c, &dir, seq, raw.blob_ref.as_ref())?),
            };
            Ok(Some(EffectRow {
                key,
                class: raw.class,
                intent_seq: raw.intent_seq,
                receipt_seq: raw.receipt_seq,
                result,
            }))
        })
        .await
    }

    /// Эффекты run с намерением, но без квитанции, по возрастанию `intent_seq`.
    pub async fn pending_effects(&self, run: RunId) -> Result<Vec<EffectRow>, StoreError> {
        self.read(move |c| {
            let mut stmt = c.prepare(
                "SELECT effect_key, class, intent_seq FROM effects \
                 WHERE run_id = ?1 AND receipt_seq IS NULL ORDER BY intent_seq",
            )?;
            let rows = stmt.query_map([id_sql(run)], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            rows.map(|r| {
                let (key, class, intent) = r?;
                Ok(EffectRow {
                    key: EffectKey(key),
                    class: parse_name(class, "class")?,
                    intent_seq: seq_from(intent)?,
                    receipt_seq: None,
                    result: None,
                })
            })
            .collect()
        })
        .await
    }

    /// Открывает прерывание в статусе `open` и пишет событие
    /// `interrupt.opened {id, kind, payload}`.
    ///
    /// Повторное открытие того же `id` ничего не меняет и событие не пишет: id
    /// выводятся детерминированно (CORE-007), повтор после сбоя безопасен.
    /// Неизвестный run даёт [`StoreError::UnknownRun`].
    pub async fn open_interrupt(
        &self,
        id: &str,
        run: RunId,
        kind: &str,
        payload: Value,
    ) -> Result<(), StoreError> {
        let dir = self.dir();
        let id = id.to_owned();
        let kind = kind.to_owned();
        self.write(move |c| {
            ensure_run(c, run)?;
            let n = c.execute(
                "INSERT INTO interrupts (id, run_id, kind, status, payload) \
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (id) DO NOTHING",
                params![
                    id,
                    id_sql(run),
                    kind,
                    INTERRUPT_OPEN,
                    canonical_json(&payload)
                ],
            )?;
            if n == 1 {
                let ev = json!({"id": id, "kind": kind, "payload": payload});
                events::insert(c, &dir, &runtime_event(run, EventKind::InterruptOpened, ev))?;
            }
            Ok(())
        })
        .await
    }

    /// Прерывания run в порядке открытия.
    pub async fn interrupts(&self, run: RunId) -> Result<Vec<InterruptRow>, StoreError> {
        self.read(move |c| {
            let mut stmt = c.prepare(
                "SELECT id, run_id, kind, status, payload FROM interrupts \
                 WHERE run_id = ?1 ORDER BY rowid",
            )?;
            let rows = stmt.query_map([id_sql(run)], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })?;
            rows.map(|r| {
                let (id, run_id, kind, status, payload) = r?;
                let payload = payload
                    .map(|p| serde_json::from_str(&p))
                    .transpose()
                    .map_err(|e| corrupt(&format!("payload прерывания {id}: {e}")))?;
                Ok(InterruptRow {
                    run: RunId(uuid(&run_id)?),
                    id,
                    kind,
                    status,
                    payload,
                })
            })
            .collect()
        })
        .await
    }
}

/// Строка `effects` без результата.
struct RawRow {
    class: EffectClass,
    intent_seq: Seq,
    receipt_seq: Option<Seq>,
    blob_ref: Option<BlobHash>,
}

fn load_row(c: &Connection, run: RunId, key: &EffectKey) -> Result<Option<RawRow>, StoreError> {
    c.query_row(
        "SELECT class, intent_seq, receipt_seq, blob_ref FROM effects \
         WHERE run_id = ?1 AND effect_key = ?2",
        params![id_sql(run), key.0],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        },
    )
    .optional()?
    .map(|(class, intent, receipt, blob)| {
        Ok(RawRow {
            class: parse_name(class, "class")?,
            intent_seq: seq_from(intent)?,
            receipt_seq: receipt.map(seq_from).transpose()?,
            blob_ref: blob.map(BlobHash),
        })
    })
    .transpose()
}

/// Результат из blob `effects.blob_ref` или из события квитанции `seq`.
fn load_result(
    c: &Connection,
    dir: &std::path::Path,
    seq: Seq,
    blob: Option<&BlobHash>,
) -> Result<Value, StoreError> {
    if let Some(h) = blob {
        let data = blobs::get(c, dir, h)?
            .ok_or_else(|| corrupt(&format!("нет blob {} с результатом", h.0)))?;
        return serde_json::from_slice(&data)
            .map_err(|e| corrupt(&format!("результат из blob {}: {e}", h.0)));
    }
    let mut payload = events::payload_at(c, dir, seq)?
        .ok_or_else(|| corrupt(&format!("нет события квитанции {}", seq.0)))?;
    payload
        .get_mut("result")
        .map(Value::take)
        .ok_or_else(|| corrupt(&format!("в квитанции {} нет result", seq.0)))
}
