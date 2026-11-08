//! Контекст выполнения run и протокол эффектов (CORE-002, CORE-004).
//!
//! Алгоритм [`RunCtx::effect`] по ключу `(run, step, effect_seq)`:
//! квитанция есть — вернуть записанный результат, не выполняя; намерение без
//! квитанции — для `NonIdempotent` открыть прерывание `unknown_outcome`, для прочих
//! классов выполнить заново; намерения нет — записать его и выполнить. Итог
//! записывается квитанцией: значение при успехе, `{"error": сообщение}` при отказе.

use std::future::Future;

use serde_json::{Value, json};
use za_protocol::{EffectClass, EffectKey, RunId};
use za_store::{ProjectStore, StoreError};

use crate::recovery::open_unknown;

/// Описание эффекта для журнала.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectSpec {
    pub class: EffectClass,
    /// Имя действия, например имя инструмента.
    pub name: String,
    pub args: Value,
}

/// Ошибка выполнения эффекта.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EffectError {
    /// Ошибка журнала; если она случилась после выполнения, квитанции нет.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Исход `NonIdempotent`-эффекта неизвестен; открыто прерывание.
    #[error("исход эффекта неизвестен, прерывание {interrupt_id}")]
    Unknown { interrupt_id: String },
    /// Эффект завершился отказом; отказ записан и при повторе не выполняется.
    #[error("эффект завершился отказом: {0}")]
    Failed(String),
}

/// Контекст выполнения run: хранилище, шаг и номер эффекта в шаге.
#[derive(Debug, Clone)]
pub struct RunCtx {
    store: ProjectStore,
    run: RunId,
    step: u64,
    effect_seq: u64,
}

impl RunCtx {
    /// Контекст run с шага 0.
    pub fn new(store: ProjectStore, run: RunId) -> Self {
        Self {
            store,
            run,
            step: 0,
            effect_seq: 0,
        }
    }

    /// Переходит к следующему шагу; нумерация эффектов начинается заново.
    pub fn next_step(&mut self) {
        self.step += 1;
        self.effect_seq = 0;
    }

    /// Выполняет эффект по протоколу намерение → выполнение → квитанция.
    ///
    /// Номер эффекта растёт при каждом вызове, в том числе неудачном, чтобы ключи
    /// при повторе run совпадали с исходными.
    pub async fn effect<F, Fut>(&mut self, spec: EffectSpec, f: F) -> Result<Value, EffectError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, String>>,
    {
        let key = EffectKey::derive(self.run, self.step, self.effect_seq);
        self.effect_seq += 1;

        match self.store.effect(self.run, &key).await? {
            Some(row) => {
                if let Some(result) = row.result {
                    tracing::debug!(key = %key.0, "эффект завершён ранее, результат из журнала");
                    return outcome(result);
                }
                if row.class != spec.class {
                    tracing::warn!(key = %key.0, journal = ?row.class, spec = ?spec.class,
                        "класс эффекта расходится с журналом");
                }
                // Решает более строгий из классов: записанного и текущего.
                if row.class == EffectClass::NonIdempotent
                    || spec.class == EffectClass::NonIdempotent
                {
                    let interrupt_id = open_unknown(&self.store, self.run, &row).await?;
                    tracing::warn!(key = %key.0, %interrupt_id, "исход эффекта неизвестен");
                    return Err(EffectError::Unknown { interrupt_id });
                }
                tracing::info!(key = %key.0, "намерение без квитанции, эффект повторяется");
            }
            None => {
                self.store
                    .record_intent(self.run, &key, spec.class, &spec.name, spec.args)
                    .await?;
            }
        }

        match f().await {
            Ok(v) => {
                self.store.record_receipt(self.run, &key, v.clone()).await?;
                Ok(v)
            }
            Err(msg) => {
                self.store
                    .record_receipt(self.run, &key, json!({ "error": msg }))
                    .await?;
                Err(EffectError::Failed(msg))
            }
        }
    }
}

/// Итог по записанному результату: объект ровно с одним строковым полем `error` —
/// отказ, остальное — успех.
///
/// Успешный результат такой же формы неотличим от отказа; типизированный исход —
/// решение R1 вместе с `effect<T>`.
fn outcome(result: Value) -> Result<Value, EffectError> {
    if let Value::Object(m) = &result
        && m.len() == 1
        && let Some(Value::String(msg)) = m.get("error")
    {
        return Err(EffectError::Failed(msg.clone()));
    }
    Ok(result)
}
