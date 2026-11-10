//! Эффекты (намерение и квитанция, CORE-002) и прерывания.
//!
//! Журнал — источник истины: каждая запись в `effects` или `interrupts`
//! сопровождается событием (`effect.intent`, `effect.receipt`, `interrupt.opened`,
//! `interrupt.resolved`) в той же транзакции. Намерение `NonIdempotent`-эффекта
//! фиксируется с `synchronous=FULL` (CORE-005): после `Ok` оно переживает отключение
//! питания, и необратимое действие не повторится вслепую.
//!
//! Результат хранится в событии квитанции `{key, result}`. Результат длиннее
//! [`BLOB_THRESHOLD`] байт канонического JSON уходит в blob: в событии остаётся
//! `{key, result_blob: <hash>}`, тот же хеш пишется в `effects.blob_ref`.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use za_protocol::{
    Actor, BlobHash, EffectClass, EffectKey, EventKind, NewEvent, RunId, Seq, canonical_json,
};

use crate::blobs::{self, BLOB_THRESHOLD};
use crate::events::{self, corrupt, name_of, parse_name, seq_from, seq_sql, uuid};
use crate::runs::{ensure_run, id_sql, runtime_event};
use crate::{ProjectStore, StoreError};

/// MIME-тип blob с результатом эффекта.
const RESULT_MIME: &str = "application/json";

/// Статус (`InterruptRow::status`) только что открытого прерывания.
pub const INTERRUPT_OPEN: &str = "open";
/// Статус (`InterruptRow::status`) решённого прерывания.
pub const INTERRUPT_RESOLVED: &str = "resolved";

/// Столбцы прерывания в порядке [`interrupt_from`].
const INTERRUPT_COLS: &str = "SELECT id, run_id, kind, status, payload, response FROM interrupts";

/// Эффект, как он записан в журнале.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectRow {
    pub key: EffectKey,
    pub class: EffectClass,
    /// Имя действия из намерения.
    pub name: String,
    /// Аргументы из намерения.
    pub args: Value,
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
    /// Состояние: `open`, после решения `resolved`.
    pub status: String,
    pub payload: Option<Value>,
    /// Решение человека или внешний результат; `None`, пока прерывание открыто.
    pub response: Option<Value>,
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
            let (name, args) = load_intent(c, &dir, raw.intent_seq)?;
            Ok(Some(EffectRow {
                key,
                class: raw.class,
                name,
                args,
                intent_seq: raw.intent_seq,
                receipt_seq: raw.receipt_seq,
                result,
            }))
        })
        .await
    }

    /// Эффекты run с намерением, но без квитанции, по возрастанию `intent_seq`.
    pub async fn pending_effects(&self, run: RunId) -> Result<Vec<EffectRow>, StoreError> {
        let dir = self.dir();
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
                let intent_seq = seq_from(intent)?;
                let (name, args) = load_intent(c, &dir, intent_seq)?;
                Ok(EffectRow {
                    key: EffectKey(key),
                    class: parse_name(class, "class")?,
                    name,
                    args,
                    intent_seq,
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
            let mut stmt = c.prepare(&format!(
                "{INTERRUPT_COLS} WHERE run_id = ?1 ORDER BY rowid"
            ))?;
            let rows = stmt.query_map([id_sql(run)], raw_interrupt)?;
            rows.map(|r| interrupt_from(r?)).collect()
        })
        .await
    }

    /// Прерывание по id; `None`, если его нет.
    pub async fn interrupt(&self, id: &str) -> Result<Option<InterruptRow>, StoreError> {
        let id = id.to_owned();
        self.read(move |c| {
            c.query_row(
                &format!("{INTERRUPT_COLS} WHERE id = ?1"),
                [id],
                raw_interrupt,
            )
            .optional()?
            .map(interrupt_from)
            .transpose()
        })
        .await
    }

    /// Решение пользователя: [`Self::resolve_interrupt_as`] с actor `user`.
    pub async fn resolve_interrupt(&self, id: &str, response: Value) -> Result<bool, StoreError> {
        self.resolve_interrupt_as(id, response, Actor::User).await
    }

    /// Переводит прерывание `open` в `resolved`, сохраняет `response` и пишет событие
    /// `interrupt.resolved {id, kind, response}` от имени `actor` — того, кто решил:
    /// человек (`User`; одобрение, ответ, решение неизвестного исхода), рантайм
    /// (`Runtime`; таймер), инструмент или соседний агент (внешний результат).
    ///
    /// Уже решённое прерывание не меняется: `Ok(false)` без события (решение
    /// принимается один раз). Неизвестный id — [`StoreError::UnknownInterrupt`].
    pub async fn resolve_interrupt_as(
        &self,
        id: &str,
        response: Value,
        actor: Actor,
    ) -> Result<bool, StoreError> {
        let dir = self.dir();
        let id = id.to_owned();
        self.write(move |c| resolve_in(c, &dir, &id, response, actor))
            .await
    }

    /// Есть ли у run эффекты шага `step`, то есть с ключом `{run}:{step}:*`.
    pub async fn step_has_effects(&self, run: RunId, step: u64) -> Result<bool, StoreError> {
        // Двоеточие после номера шага отделяет шаг 3 от шага 30. В ключе только
        // UUID, цифры и двоеточия, поэтому `%` и `_` в шаблоне не встречаются.
        let pattern = format!("{run}:{step}:%");
        self.read(move |c| {
            Ok(c.query_row(
                "SELECT EXISTS (SELECT 1 FROM effects WHERE run_id = ?1 AND effect_key LIKE ?2)",
                params![id_sql(run), pattern],
                |r| r.get::<_, bool>(0),
            )?)
        })
        .await
    }
}

/// Решение прерывания в открытой транзакции писателя: `open` → `resolved`, `response` и
/// событие `interrupt.resolved {id, kind, response}` от имени `actor`. `Ok(false)` —
/// прерывание уже решено (ничего не пишется); неизвестный id —
/// [`StoreError::UnknownInterrupt`]. Общая часть [`ProjectStore::resolve_interrupt_as`]
/// и [`ProjectStore::resolve_with_audit`].
pub(crate) fn resolve_in(
    c: &Connection,
    dir: &std::path::Path,
    id: &str,
    response: Value,
    actor: Actor,
) -> Result<bool, StoreError> {
    let (run_id, kind) = c
        .query_row(
            "SELECT run_id, kind FROM interrupts WHERE id = ?1",
            [id],
            |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
        .ok_or_else(|| StoreError::UnknownInterrupt(id.to_owned()))?;
    let n = c.execute(
        "UPDATE interrupts SET status = ?1, response = ?2 WHERE id = ?3 AND status = ?4",
        params![
            INTERRUPT_RESOLVED,
            canonical_json(&response),
            id,
            INTERRUPT_OPEN
        ],
    )?;
    if n == 0 {
        return Ok(false);
    }
    let run = RunId(uuid(&run_id)?);
    let ev = NewEvent {
        actor,
        ..runtime_event(
            run,
            EventKind::InterruptResolved,
            json!({"id": id, "kind": kind, "response": response}),
        )
    };
    events::insert(c, dir, &ev)?;
    Ok(true)
}

/// Строка `interrupts` в исходном виде: id, run_id, kind, status, payload, response.
type RawInterrupt = (
    String,
    Vec<u8>,
    String,
    String,
    Option<String>,
    Option<String>,
);

fn raw_interrupt(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawInterrupt> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
    ))
}

fn interrupt_from(raw: RawInterrupt) -> Result<InterruptRow, StoreError> {
    let (id, run_id, kind, status, payload, response) = raw;
    let parse = |v: Option<String>, what: &str| {
        v.map(|p| serde_json::from_str(&p))
            .transpose()
            .map_err(|e| corrupt(&format!("{what} прерывания {id}: {e}")))
    };
    Ok(InterruptRow {
        run: RunId(uuid(&run_id)?),
        payload: parse(payload, "payload")?,
        response: parse(response, "response")?,
        id,
        kind,
        status,
    })
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

/// Имя и аргументы из события намерения `seq`.
fn load_intent(
    c: &Connection,
    dir: &std::path::Path,
    seq: Seq,
) -> Result<(String, Value), StoreError> {
    let mut payload = events::payload_at(c, dir, seq)?
        .ok_or_else(|| corrupt(&format!("нет события намерения {}", seq.0)))?;
    let name = match payload.get_mut("name").map(Value::take) {
        Some(Value::String(n)) => n,
        _ => return Err(corrupt(&format!("в намерении {} нет name", seq.0))),
    };
    let args = payload
        .get_mut("args")
        .map(Value::take)
        .ok_or_else(|| corrupt(&format!("в намерении {} нет args", seq.0)))?;
    Ok((name, args))
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
