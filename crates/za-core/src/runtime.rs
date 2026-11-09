//! Контекст выполнения run и протокол эффектов (CORE-002, CORE-004).
//!
//! Алгоритм эффекта по ключу `(run, step, effect_seq)`:
//! квитанция есть — вернуть записанный результат, не выполняя; намерение без
//! квитанции — для `NonIdempotent` открыть прерывание `unknown_outcome`, для прочих
//! классов выполнить заново; намерения нет — записать его и выполнить. Итог
//! записывается квитанцией с явным исходом — конвертом
//! `{"outcome":"ok","value":…}` или `{"outcome":"error","message":…}`, поэтому
//! успешное значение любой формы при повторе остаётся успехом. Конверт для
//! хранилища — обычное значение: большой конверт целиком уходит в blob.
//!
//! При повторе имя и аргументы эффекта сверяются с намерением в журнале:
//! расхождение (например, дрейф хэша префикса промпта в аргументах вызова
//! модели) — [`EffectError::ReplayMismatch`], а не тихая выдача записанного
//! результата.
//!
//! Ключи выдаёт [`RunCtx::reserve`] в порядке вызовов, сам протокол выполняет
//! [`EffectSlot`] (D3): слоты одного шага можно запускать одновременно, ключи
//! при реплее от этого не меняются. [`RunCtx::effect`] — резервирование,
//! выполнение и учёт результата одним вызовом.

use std::future::Future;

use serde_json::{Value, json};
use za_protocol::{EffectClass, EffectKey, RunId, canonical_json};
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
    /// Имя или аргументы эффекта расходятся с намерением в журнале под тем же
    /// ключом: код узла недетерминирован или изменился. `field` — `name` или
    /// `args`. Эффект не выполняется, записанный результат не отдаётся.
    #[error(
        "эффект {key}: поле {field} расходится с журналом — повтор run недетерминирован, продолжение остановлено"
    )]
    ReplayMismatch { key: String, field: &'static str },
    /// Контекст отравлен прежней ошибкой журнала, неизвестным исходом или
    /// расхождением с журналом: продолжать можно только из нового контекста
    /// после восстановления.
    #[error("контекст run отравлен прежней ошибкой, нужен новый контекст")]
    Poisoned,
}

/// Откуда взят результат слота (D7, pre-flight 5.2).
///
/// `Journal` — квитанция уже была (успех или отказ) и замыкание не вызывалось:
/// это реплей. `Live` — замыкание выполнено сейчас, а также любая ошибка без
/// разобранной квитанции (`Store`, в том числе повреждённый конверт, `Unknown`,
/// `ReplayMismatch`, `Poisoned`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectSource {
    Live,
    Journal,
}

/// Заранее зарезервированный ключ эффекта (D3).
///
/// Слот владеет клоном [`ProjectStore`] и выполняется независимо от
/// [`RunCtx`], в том числе в `tokio::task::JoinSet`: ключи назначены в порядке
/// резервирования, поэтому при реплее они совпадают, даже если слоты завершились
/// в другом порядке. Слот сам контекст не отравляет — результат передаётся в
/// [`RunCtx::observe`].
#[derive(Debug)]
pub struct EffectSlot {
    store: ProjectStore,
    run: RunId,
    key: EffectKey,
}

impl EffectSlot {
    /// Ключ эффекта слота.
    pub fn key(&self) -> &EffectKey {
        &self.key
    }

    /// Протокол намерение → выполнение → квитанция, как у [`RunCtx::effect`].
    /// Future — `Send + 'static`, если `F` и `Fut` — `Send + 'static`
    /// (годится для `JoinSet`).
    pub async fn run<F, Fut>(self, spec: EffectSpec, f: F) -> Result<Value, EffectError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, String>>,
    {
        self.run_traced(spec, f).await.0
    }

    /// То же, что [`Self::run`], плюс источник результата: [`EffectSource::Journal`],
    /// если квитанция уже была (успех или отказ) и `f` не вызывался.
    pub async fn run_traced<F, Fut>(
        self,
        spec: EffectSpec,
        f: F,
    ) -> (Result<Value, EffectError>, EffectSource)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, String>>,
    {
        let out = match self.prepare(spec).await {
            Ok(Prepared::Recorded(r)) => (r, EffectSource::Journal),
            Ok(Prepared::Execute) => (self.execute(f).await, EffectSource::Live),
            Err(e) => (Err(e), EffectSource::Live),
        };
        // `observe` ключа не знает: ошибки, отравляющие контекст, логируются здесь.
        if let Err(e @ (EffectError::Store(_) | EffectError::ReplayMismatch { .. })) = &out.0 {
            tracing::error!(key = %self.key.0, error = %e, "ошибка эффекта");
        }
        out
    }

    /// Сверяет спецификацию с журналом; если намерения нет, записывает его.
    async fn prepare(&self, spec: EffectSpec) -> Result<Prepared, EffectError> {
        let key = &self.key;
        match self.store.effect(self.run, key).await? {
            Some(row) => {
                // Расхождение с журналом — признак недетерминированного узла.
                if row.class != spec.class {
                    tracing::warn!(key = %key.0, journal = ?row.class, spec = ?spec.class,
                        "класс эффекта расходится с журналом");
                }
                let field = if row.name != spec.name {
                    Some("name")
                } else if row.args != spec.args {
                    Some("args")
                } else {
                    None
                };
                if let Some(field) = field {
                    tracing::error!(key = %key.0, field,
                        journal_name = %row.name, spec_name = %spec.name,
                        journal_args = %canonical_json(&row.args),
                        spec_args = %canonical_json(&spec.args),
                        "эффект расходится с журналом");
                    return Err(EffectError::ReplayMismatch {
                        key: key.0.clone(),
                        field,
                    });
                }
                if let Some(result) = row.result {
                    tracing::debug!(key = %key.0, "эффект завершён ранее, результат из журнала");
                    // Повреждённый конверт — ошибка журнала, а не записанный исход.
                    return match decode(key, result) {
                        Err(e @ EffectError::Store(_)) => Err(e),
                        r => Ok(Prepared::Recorded(r)),
                    };
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
        Ok(Prepared::Execute)
    }

    /// Выполняет замыкание и записывает квитанцию с конвертом исхода.
    async fn execute<F, Fut>(&self, f: F) -> Result<Value, EffectError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, String>>,
    {
        match f().await {
            Ok(v) => {
                self.store
                    .record_receipt(self.run, &self.key, ok_envelope(&v))
                    .await?;
                Ok(v)
            }
            Err(msg) => {
                self.store
                    .record_receipt(self.run, &self.key, error_envelope(&msg))
                    .await?;
                Err(EffectError::Failed(msg))
            }
        }
    }
}

/// Итог сверки слота с журналом.
enum Prepared {
    /// Квитанция уже есть: записанный успех или отказ.
    Recorded(Result<Value, EffectError>),
    /// Намерение записано или повторяемо: выполнить замыкание.
    Execute,
}

/// Контекст выполнения run: хранилище, шаг и номер эффекта в шаге.
///
/// После любой [`EffectError::Store`] контекст отравлен: неизвестно, зафиксирована
/// ли запись, поэтому ни занять следующий ключ, ни повторить текущий нельзя —
/// иначе после перезапуска `NonIdempotent`-действие выполнится повторно или ключи
/// шага сдвинутся. Так же отравляют [`EffectError::Unknown`] (run ждёт решения
/// человека, следующий эффект не должен занять ключ `seq+1`) и
/// [`EffectError::ReplayMismatch`] (дальнейшие ключи уже не соответствуют
/// журналу). Все дальнейшие вызовы [`RunCtx::effect`] и [`RunCtx::reserve`] дают
/// [`EffectError::Poisoned`] без обращения к журналу, `next_step` отравление не
/// снимает; продолжение — только новым контекстом через восстановление.
///
/// Результаты слотов ([`RunCtx::reserve`]) отравляют контекст только через
/// [`RunCtx::observe`]; уже выданные слоты отравление не останавливает — их
/// судьбу решает вызывающий (D3).
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
        Self::resume_at(store, run, 0, 0)
    }

    /// Контекст run с шага `step` и номера эффекта `effect_seq` (из снимка, D4).
    pub fn resume_at(store: ProjectStore, run: RunId, step: u64, effect_seq: u64) -> Self {
        Self {
            store,
            run,
            step,
            effect_seq,
            poisoned: false,
        }
    }

    /// Текущий шаг.
    pub fn step(&self) -> u64 {
        self.step
    }

    /// Номер следующего эффекта в шаге.
    pub fn effect_seq(&self) -> u64 {
        self.effect_seq
    }

    /// Run контекста.
    pub fn run_id(&self) -> RunId {
        self.run
    }

    /// Хранилище run.
    pub fn store(&self) -> &ProjectStore {
        &self.store
    }

    /// Отравлен ли контекст (см. [`RunCtx`]).
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Переходит к следующему шагу; нумерация эффектов начинается заново.
    /// Отравление контекста не снимается.
    pub fn next_step(&mut self) {
        self.step += 1;
        self.effect_seq = 0;
    }

    /// Резервирует следующий ключ шага. Отравленный контекст —
    /// [`EffectError::Poisoned`], ключ не занимается.
    pub fn reserve(&mut self) -> Result<EffectSlot, EffectError> {
        if self.poisoned {
            return Err(EffectError::Poisoned);
        }
        let key = EffectKey::derive(self.run, self.step, self.effect_seq);
        self.effect_seq += 1;
        Ok(EffectSlot {
            store: self.store.clone(),
            run: self.run,
            key,
        })
    }

    /// Учитывает результат слота: [`EffectError::Store`], [`EffectError::Unknown`]
    /// и [`EffectError::ReplayMismatch`] отравляют контекст (правила R0).
    pub fn observe<T>(&mut self, r: &Result<T, EffectError>) {
        if let Err(
            e @ (EffectError::Store(_)
            | EffectError::Unknown { .. }
            | EffectError::ReplayMismatch { .. }),
        ) = r
        {
            tracing::error!(run = %self.run, step = self.step, error = %e, "контекст run отравлен");
            self.poisoned = true;
        }
    }

    /// Выполняет эффект по протоколу намерение → выполнение → квитанция:
    /// [`Self::reserve`], [`EffectSlot::run`], [`Self::observe`].
    ///
    /// Каждый вызов занимает следующий номер эффекта в шаге; ключи совпадают с
    /// исходными при повторе run из нового контекста, если код узла детерминирован.
    /// Ошибка журнала, неизвестный исход и расхождение с журналом отравляют
    /// контекст (см. [`RunCtx`]).
    pub async fn effect<F, Fut>(&mut self, spec: EffectSpec, f: F) -> Result<Value, EffectError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, String>>,
    {
        let slot = self.reserve()?;
        let r = slot.run(spec, f).await;
        self.observe(&r);
        r
    }
}

/// Исход успешного эффекта в конверте квитанции.
const OUTCOME_OK: &str = "ok";
/// Исход отказа в конверте квитанции.
const OUTCOME_ERROR: &str = "error";

/// Конверт квитанции успешного эффекта.
pub(crate) fn ok_envelope(v: &Value) -> Value {
    json!({ "outcome": OUTCOME_OK, "value": v })
}

/// Конверт квитанции отказа.
pub(crate) fn error_envelope(msg: &str) -> Value {
    json!({ "outcome": OUTCOME_ERROR, "message": msg })
}

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
