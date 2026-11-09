//! Восстановление run после сбоя (CORE-003, CORE-007).
//!
//! Незавершённые эффекты классов `Pure`, `ReadOnly`, `Idempotent` безопасно
//! повторяются при следующем выполнении узла и решения не требуют. Для
//! `NonIdempotent` исход неизвестен: открывается прерывание `unknown_outcome`
//! с id `unk-{effect_key}`, одинаковым при любом числе повторов. Решение человека
//! ([`resolve_unknown`]) закрывает такой эффект квитанцией (D16), поэтому
//! следующий `recover` его уже не видит.

use serde_json::{Value, json};
use za_protocol::{EffectClass, EffectKey, RunId};
use za_store::{EffectRow, ProjectStore, StoreError};

use crate::runtime::{error_envelope, ok_envelope};

/// Вид прерывания для намерения без квитанции у `NonIdempotent`-эффекта.
pub const UNKNOWN_OUTCOME: &str = "unknown_outcome";

/// Итог восстановления run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// Решений не требуется: run можно продолжать.
    Clean,
    /// Есть эффекты с неизвестным исходом; id открытых прерываний по порядку намерений.
    NeedsDecision { interrupt_ids: Vec<String> },
}

/// Проверяет незавершённые эффекты run и открывает прерывания для `NonIdempotent`.
/// Повторный вызов возвращает те же id и новых прерываний не создаёт.
/// Неизвестный run даёт [`StoreError::UnknownRun`].
pub async fn recover(store: &ProjectStore, run: RunId) -> Result<Recovery, StoreError> {
    if store.get_run(run).await?.is_none() {
        return Err(StoreError::UnknownRun(run));
    }
    let mut interrupt_ids = Vec::new();
    for row in store.pending_effects(run).await? {
        if row.class == EffectClass::NonIdempotent {
            interrupt_ids.push(open_unknown(store, run, &row).await?);
        }
    }
    tracing::info!(%run, unknown = interrupt_ids.len(), "восстановление run");
    if interrupt_ids.is_empty() {
        Ok(Recovery::Clean)
    } else {
        Ok(Recovery::NeedsDecision { interrupt_ids })
    }
}

/// Открывает (идемпотентно) прерывание неизвестного исхода и возвращает его id
/// `unk-{effect_key}` (CORE-007).
pub(crate) async fn open_unknown(
    store: &ProjectStore,
    run: RunId,
    row: &EffectRow,
) -> Result<String, StoreError> {
    let id = format!("unk-{}", row.key.0);
    let payload = json!({ "effect_key": row.key.0, "intent_seq": row.intent_seq.0 });
    store
        .open_interrupt(&id, run, UNKNOWN_OUTCOME, payload)
        .await?;
    Ok(id)
}

/// Решение человека по прерыванию неизвестного исхода (D16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownDecision {
    /// Действие выполнено: эффект закрывается успехом `{"resolved": "done"}`.
    Done,
    /// Действие не выполнено: эффект закрывается отказом [`NOT_DONE_MESSAGE`].
    NotDone,
}

impl UnknownDecision {
    /// Значение поля `decision` в ответе прерывания.
    fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::NotDone => "not_done",
        }
    }

    /// Разбор поля `decision` ответа прерывания.
    fn parse(response: Option<&Value>) -> Option<Self> {
        match response?.get("decision")?.as_str()? {
            "done" => Some(Self::Done),
            "not_done" => Some(Self::NotDone),
            _ => None,
        }
    }

    /// Решение, которое выражает уже записанная квитанция эффекта: успех — `Done`,
    /// отказ — `NotDone`. Квитанция действует, а не новый ответ человека: её
    /// получит run при реплее.
    fn from_receipt(envelope: &Value) -> Option<Self> {
        match envelope.get("outcome")?.as_str()? {
            "ok" => Some(Self::Done),
            "error" => Some(Self::NotDone),
            _ => None,
        }
    }
}

/// Текст отказа эффекта, который человек признал невыполненным; читает модель.
pub const NOT_DONE_MESSAGE: &str = "not executed: the user decided the action did not happen";

/// Состояние открытого прерывания в журнале.
const INTERRUPT_OPEN: &str = "open";

/// Закрывает эффект прерывания `unk-…` квитанцией (D16) и решает прерывание
/// ответом `{"decision": "done" | "not_done"}`. Возвращает действующее решение.
///
/// Порядок — квитанция, затем решение: сбой между ними оставляет закрытый эффект
/// и открытое прерывание. Если квитанция уже есть (повтор после такого сбоя или
/// одновременный вызов), действует она: решение выводится из её исхода, даже если
/// `d` другое (`warn`), — аудит прерывания совпадает с тем, что run получит при
/// реплее. Уже решённое прерывание не меняется; проигравший гонку
/// `resolve_interrupt` вызов возвращает записанное решение.
///
/// Id не `unk-…`, прерывание другого вида, неизвестное прерывание или нет
/// намерения под его ключом — [`StoreError::UnknownInterrupt`]. Нет
/// `payload.effect_key`, ключ чужого run, эффект не `NonIdempotent`, квитанция
/// или ответ без разбираемого исхода — [`StoreError::Corrupt`].
pub async fn resolve_unknown(
    store: &ProjectStore,
    interrupt_id: &str,
    d: UnknownDecision,
) -> Result<UnknownDecision, StoreError> {
    let unknown = || StoreError::UnknownInterrupt(interrupt_id.to_owned());
    let corrupt = |what: &str| StoreError::Corrupt(format!("прерывание {interrupt_id}: {what}"));
    if !interrupt_id.starts_with("unk-") {
        return Err(unknown());
    }
    let int = match store.interrupt(interrupt_id).await? {
        Some(int) if int.kind == UNKNOWN_OUTCOME => int,
        _ => return Err(unknown()),
    };
    if int.status != INTERRUPT_OPEN {
        let recorded = UnknownDecision::parse(int.response.as_ref())
            .ok_or_else(|| corrupt("ответ без decision"))?;
        if recorded != d {
            tracing::warn!(
                id = interrupt_id,
                requested = d.as_str(),
                recorded = recorded.as_str(),
                "прерывание уже решено иначе, решение не меняется"
            );
        }
        return Ok(recorded);
    }
    let key = int
        .payload
        .as_ref()
        .and_then(|p| p.get("effect_key"))
        .and_then(Value::as_str)
        .map(|k| EffectKey(k.to_owned()))
        .ok_or_else(|| corrupt("нет payload.effect_key"))?;
    if !key.0.starts_with(&format!("{}:", int.run)) {
        return Err(corrupt("ключ эффекта другого run"));
    }
    let Some(row) = store.effect(int.run, &key).await? else {
        return Err(unknown());
    };
    if row.class != EffectClass::NonIdempotent {
        return Err(corrupt("эффект не NonIdempotent"));
    }
    let actual = match row.result {
        Some(envelope) => UnknownDecision::from_receipt(&envelope)
            .ok_or_else(|| corrupt("квитанция без исхода"))?,
        None => {
            let envelope = match d {
                UnknownDecision::Done => ok_envelope(&json!({ "resolved": "done" })),
                UnknownDecision::NotDone => error_envelope(NOT_DONE_MESSAGE),
            };
            match store.record_receipt(int.run, &key, envelope).await {
                Ok(_) => d,
                // Квитанцию успел записать одновременный вызов: действует она.
                Err(StoreError::EffectConflict(_)) => {
                    let envelope = store
                        .effect(int.run, &key)
                        .await?
                        .and_then(|r| r.result)
                        .ok_or_else(|| corrupt("квитанция пропала"))?;
                    UnknownDecision::from_receipt(&envelope)
                        .ok_or_else(|| corrupt("квитанция без исхода"))?
                }
                Err(e) => return Err(e),
            }
        }
    };
    if actual != d {
        tracing::warn!(key = %key.0, requested = d.as_str(), receipt = actual.as_str(),
            "квитанция уже записана с другим исходом, действует она");
    }
    let won = store
        .resolve_interrupt(interrupt_id, json!({ "decision": actual.as_str() }))
        .await?;
    if !won {
        // Решение записал одновременный вызов; оно выведено из той же квитанции.
        let recorded = store
            .interrupt(interrupt_id)
            .await?
            .and_then(|i| UnknownDecision::parse(i.response.as_ref()))
            .ok_or_else(|| corrupt("ответ без decision"))?;
        return Ok(recorded);
    }
    tracing::info!(
        id = interrupt_id,
        decision = actual.as_str(),
        "неизвестный исход решён"
    );
    Ok(actual)
}
