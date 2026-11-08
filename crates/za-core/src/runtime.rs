//! Контекст выполнения run и протокол эффектов (CORE-002, CORE-004).
//!
//! Алгоритм [`RunCtx::effect`] по ключу `(run, step, effect_seq)`:
//! квитанция есть — вернуть записанный результат, не выполняя; намерение без
//! квитанции — для `NonIdempotent` открыть прерывание `unknown_outcome`, для прочих
//! классов выполнить заново; намерения нет — записать его и выполнить. Итог
//! записывается квитанцией с явным исходом — конвертом
//! `{"outcome":"ok","value":…}` или `{"outcome":"error","message":…}`, поэтому
//! успешное значение любой формы при повторе остаётся успехом. Конверт для
//! хранилища — обычное значение: большой конверт целиком уходит в blob.

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
    /// Контекст отравлен прежней ошибкой журнала: продолжать можно только из
    /// нового контекста после восстановления.
    #[error("контекст run отравлен ошибкой журнала, нужен новый контекст")]
    Poisoned,
}

/// Контекст выполнения run: хранилище, шаг и номер эффекта в шаге.
///
/// После любой [`EffectError::Store`] контекст отравлен: неизвестно, зафиксирована
/// ли запись, поэтому ни занять следующий ключ, ни повторить текущий нельзя —
/// иначе после перезапуска `NonIdempotent`-действие выполнится повторно или ключи
/// шага сдвинутся. Все дальнейшие вызовы [`RunCtx::effect`] дают
/// [`EffectError::Poisoned`] без обращения к журналу, `next_step` отравление не
/// снимает; продолжение — только новым контекстом через восстановление.
/// `Clone` намеренно не реализован: два контекста одного run выдавали бы одни ключи.
#[derive(Debug)]
pub struct RunCtx {
    store: ProjectStore,
    run: RunId,
    step: u64,
    effect_seq: u64,
    poisoned: bool,
}

impl RunCtx {
    /// Контекст run с шага 0.
    pub fn new(store: ProjectStore, run: RunId) -> Self {
        Self {
            store,
            run,
            step: 0,
            effect_seq: 0,
            poisoned: false,
        }
    }

    /// Переходит к следующему шагу; нумерация эффектов начинается заново.
    /// Отравление контекста не снимается.
    pub fn next_step(&mut self) {
        self.step += 1;
        self.effect_seq = 0;
    }

    /// Выполняет эффект по протоколу намерение → выполнение → квитанция.
    ///
    /// Каждый вызов занимает следующий номер эффекта в шаге; ключи совпадают с
    /// исходными при повторе run из нового контекста, если код узла детерминирован.
    /// Ошибка журнала отравляет контекст (см. [`RunCtx`]).
    pub async fn effect<F, Fut>(&mut self, spec: EffectSpec, f: F) -> Result<Value, EffectError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, String>>,
    {
        if self.poisoned {
            return Err(EffectError::Poisoned);
        }
        let key = EffectKey::derive(self.run, self.step, self.effect_seq);
        self.effect_seq += 1;
        let r = self.run_effect(&key, spec, f).await;
        if let Err(EffectError::Store(e)) = &r {
            tracing::error!(key = %key.0, error = %e, "ошибка журнала, контекст run отравлен");
            self.poisoned = true;
        }
        r
    }

    async fn run_effect<F, Fut>(
        &self,
        key: &EffectKey,
        spec: EffectSpec,
        f: F,
    ) -> Result<Value, EffectError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, String>>,
    {
        match self.store.effect(self.run, key).await? {
            Some(row) => {
                // Расхождение с журналом — признак недетерминированного узла.
                if row.class != spec.class {
                    tracing::warn!(key = %key.0, journal = ?row.class, spec = ?spec.class,
                        "класс эффекта расходится с журналом");
                }
                if row.name != spec.name || row.args != spec.args {
                    tracing::warn!(key = %key.0, journal = %row.name, spec = %spec.name,
                        "имя или аргументы эффекта расходятся с журналом");
                }
                if let Some(result) = row.result {
                    tracing::debug!(key = %key.0, "эффект завершён ранее, результат из журнала");
                    return decode(key, result);
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
                    .record_intent(self.run, key, spec.class, &spec.name, spec.args)
                    .await?;
            }
        }

        match f().await {
            Ok(v) => {
                let envelope = json!({ "outcome": OUTCOME_OK, "value": v });
                self.store.record_receipt(self.run, key, envelope).await?;
                Ok(v)
            }
            Err(msg) => {
                self.store
                    .record_receipt(
                        self.run,
                        key,
                        json!({ "outcome": OUTCOME_ERROR, "message": msg }),
                    )
                    .await?;
                Err(EffectError::Failed(msg))
            }
        }
    }
}

/// Исход успешного эффекта в конверте квитанции.
const OUTCOME_OK: &str = "ok";
/// Исход отказа в конверте квитанции.
const OUTCOME_ERROR: &str = "error";

/// Разбирает конверт квитанции; иная форма — повреждение журнала.
fn decode(key: &EffectKey, mut envelope: Value) -> Result<Value, EffectError> {
    let outcome = envelope.get("outcome").and_then(Value::as_str);
    match outcome {
        Some(OUTCOME_OK) => {
            if let Some(v) = envelope.get_mut("value") {
                return Ok(v.take());
            }
        }
        Some(OUTCOME_ERROR) => {
            if let Some(Value::String(msg)) = envelope.get_mut("message") {
                return Err(EffectError::Failed(std::mem::take(msg)));
            }
        }
        _ => {}
    }
    Err(EffectError::Store(StoreError::Corrupt(format!(
        "квитанция {} без конверта исхода",
        key.0
    ))))
}
