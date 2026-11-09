//! Восстановление run после сбоя (CORE-003, CORE-007).
//!
//! Незавершённые эффекты классов `Pure`, `ReadOnly`, `Idempotent` безопасно
//! повторяются при следующем выполнении узла и решения не требуют. Для
//! `NonIdempotent` исход неизвестен: открывается прерывание `unknown_outcome`
//! с id `unk-{effect_key}`, одинаковым при любом числе повторов.

use serde_json::json;
use za_protocol::{EffectClass, RunId};
use za_store::{EffectRow, ProjectStore, StoreError};

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
