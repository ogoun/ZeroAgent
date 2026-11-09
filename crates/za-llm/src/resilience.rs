//! Устойчивость вызова модели: повторы стрима (D19, D20), предохранитель эндпоинта
//! (D22), запрет идентичного повтора после ошибки разбора (D21) и детерминированный
//! джиттер (D44).
//!
//! Время — `tokio::time::Instant` и `tokio::time::sleep`: тесты двигают его через
//! `tokio::time::pause`.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;
use za_protocol::{RunId, canonical_json};

use crate::error::{ErrorClass, ModelError};
use crate::types::{ChatRequest, EndpointId, StreamEvent};

/// Политика повторов одного вызова (D19).
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    /// Повторов сверх первой попытки.
    pub max_retries: u32,
    /// Задержка перед первым повтором.
    pub base: Duration,
    /// Множитель задержки на каждый следующий повтор.
    pub factor: u32,
    /// Потолок задержки до джиттера.
    pub cap: Duration,
    /// Самая долгая пауза `Retry-After`, которую стоит ждать; дольше — без повторов.
    pub max_retry_after: Duration,
    /// Доля джиттера: задержка умножается на `1 ± jitter`.
    pub jitter: f32,
}

impl Default for RetryPolicy {
    /// D19: 5 повторов, 1 с × 2^n, потолок 30 с, `Retry-After` до 120 с, джиттер ±20 %.
    fn default() -> Self {
        Self {
            max_retries: 5,
            base: Duration::from_secs(1),
            factor: 2,
            cap: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(120),
            jitter: 0.2,
        }
    }
}

/// Решение после неудачной попытки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Повторить после паузы.
    Retry(Duration),
    /// Больше не повторять: ошибка уходит вызывающему.
    GiveUp,
}

impl RetryPolicy {
    /// Решение после `retries_done` уже сделанных повторов и ошибки `err`.
    ///
    /// Неповторяемый класс или исчерпанный лимит — `GiveUp`. Пауза провайдера
    /// (`retry_after`, у 429 и 503/529) берётся как есть, без джиттера, если она не
    /// дольше `max_retry_after`, иначе `GiveUp` (D19: run встаёт на паузу). Без неё —
    /// `min(base × factor^retries_done, cap) × (1 + jitter × (2 × jitter_unit − 1))`.
    pub fn decide(&self, retries_done: u32, err: &ModelError, jitter_unit: f32) -> RetryDecision {
        if retries_done >= self.max_retries || !err.class.retryable() {
            return RetryDecision::GiveUp;
        }
        if let Some(after) = err.retry_after {
            return if after <= self.max_retry_after {
                RetryDecision::Retry(after)
            } else {
                RetryDecision::GiveUp
            };
        }
        let step = self
            .base
            .saturating_mul(self.factor.saturating_pow(retries_done))
            .min(self.cap);
        let unit = if jitter_unit.is_finite() {
            jitter_unit.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let jitter = if self.jitter.is_finite() {
            self.jitter.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let k = 1.0 + f64::from(jitter) * (2.0 * f64::from(unit) - 1.0);
        // Множитель в [0, 2]; переполнение при огромном `cap` — без джиттера.
        let delay = Duration::try_from_secs_f64(step.as_secs_f64() * k).unwrap_or(step);
        RetryDecision::Retry(round_to_micros(delay))
    }
}

/// Задержка до микросекунд: убирает шум двоичной дроби (0.8 × 1 с = 800 мс ровно).
fn round_to_micros(d: Duration) -> Duration {
    let micros = (d.as_nanos() + 500) / 1000;
    Duration::from_micros(u64::try_from(micros).unwrap_or(u64::MAX))
}

/// Детерминированное число в `[0, 1)` для джиттера (D44): первые байты
/// `blake3(seed ‖ attempt)`. Берутся старшие 24 бита первых 4 байт (`u32 >> 8`) —
/// столько точно представимо в `f32`, и результат не округляется до 1.0.
pub fn jitter_unit(seed: &str, attempt: u32) -> f32 {
    let mut h = blake3::Hasher::new();
    h.update(seed.as_bytes());
    h.update(&attempt.to_le_bytes());
    let b = h.finalize();
    let bytes = b.as_bytes();
    let x = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) >> 8;
    // `x < 2^24`: деление точное, результат строго меньше 1.
    x as f32 / (1u32 << 24) as f32
}

/// Параметры предохранителя эндпоинта (D22).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakerOptions {
    /// Неудачных запросов подряд до открытия.
    pub threshold: u32,
    /// Первая пауза открытого предохранителя.
    pub cooldown: Duration,
    /// Потолок паузы при повторных отказах пробы.
    pub max_cooldown: Duration,
}

impl Default for BreakerOptions {
    /// D22: 5 отказов подряд, пауза 30 с, потолок 300 с.
    fn default() -> Self {
        Self {
            threshold: 5,
            cooldown: Duration::from_secs(30),
            max_cooldown: Duration::from_secs(300),
        }
    }
}

/// Состояние предохранителя.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakerState {
    /// Заявки идут.
    Closed,
    /// Заявки отвергаются до `until`.
    Open { until: Instant },
    /// Пауза истекла: идёт одна пробная заявка.
    HalfOpen,
    /// Эндпоинт выключен до `reset` (ключ отвергнут, D22).
    Disabled { reason: String },
}

/// Предохранитель эндпоинта (MDL-016, D22).
///
/// Считает запросы, а не попытки: Gateway вызывает `on_failure` после исчерпания
/// повторов. На каждую заявку, пропущенную `allow`, Gateway обязан сообщить исход —
/// `on_success` или `on_failure` (в том числе с классом вне предохранителя, например
/// `Cancelled`), иначе пробная заявка полуоткрытого состояния не освобождается.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    opts: BreakerOptions,
    state: BreakerState,
    /// Отказы подряд в `Closed`.
    failures: u32,
    /// Пауза следующего открытия из `HalfOpen`; после успеха — `opts.cooldown`.
    cooldown: Duration,
    /// Пробная заявка `HalfOpen` уже выдана.
    probe_out: bool,
}

impl CircuitBreaker {
    pub fn new(opts: BreakerOptions) -> Self {
        Self {
            cooldown: opts.cooldown,
            opts,
            state: BreakerState::Closed,
            failures: 0,
            probe_out: false,
        }
    }

    /// Можно ли отправить заявку в момент `now`. Открытый предохранитель — `CircuitOpen`
    /// с `retry_after = until − now`; по истечении паузы — переход в `HalfOpen` и одна
    /// пробная заявка, остальные — `CircuitOpen` до её исхода. `Disabled` — `Auth`.
    pub fn allow(&mut self, now: Instant) -> Result<(), ModelError> {
        match &self.state {
            BreakerState::Closed => Ok(()),
            BreakerState::Open { until } if now < *until => Err(ModelError {
                retry_after: Some(*until - now),
                ..ModelError::new(ErrorClass::CircuitOpen, "предохранитель эндпоинта открыт")
            }),
            BreakerState::Open { .. } => {
                self.state = BreakerState::HalfOpen;
                self.probe_out = true;
                Ok(())
            }
            BreakerState::HalfOpen if self.probe_out => Err(ModelError::new(
                ErrorClass::CircuitOpen,
                "предохранитель эндпоинта: идёт пробная заявка",
            )),
            BreakerState::HalfOpen => {
                self.probe_out = true;
                Ok(())
            }
            BreakerState::Disabled { reason } => Err(ModelError::new(
                ErrorClass::Auth,
                format!("эндпоинт выключен до сброса: {reason}"),
            )),
        }
    }

    /// Запрос завершился успешно: предохранитель закрывается, пауза — базовая.
    /// `Disabled` не снимается: это делает только `reset`.
    pub fn on_success(&mut self) {
        if matches!(self.state, BreakerState::Disabled { .. }) {
            return;
        }
        self.state = BreakerState::Closed;
        self.failures = 0;
        self.cooldown = self.opts.cooldown;
        self.probe_out = false;
    }

    /// Запрос завершился ошибкой `err`. `Auth` выключает эндпоинт (`Disabled`);
    /// считаются только классы `counts_for_breaker()`, прочие (429, отмена, 400)
    /// счёт не меняют, а в `HalfOpen` лишь освобождают пробу. Отказ пробы открывает
    /// предохранитель с удвоенной паузой (до `max_cooldown`); отказы заявок, начатых до
    /// открытия, паузу не продлевают.
    pub fn on_failure(&mut self, err: &ModelError, now: Instant) {
        if matches!(self.state, BreakerState::Disabled { .. }) {
            return;
        }
        if err.class == ErrorClass::Auth {
            self.state = BreakerState::Disabled {
                reason: err.message.clone(),
            };
            self.probe_out = false;
            return;
        }
        if !err.class.counts_for_breaker() {
            if self.state == BreakerState::HalfOpen {
                self.probe_out = false;
            }
            return;
        }
        match self.state {
            BreakerState::Closed => {
                self.failures = self.failures.saturating_add(1);
                if self.failures >= self.opts.threshold {
                    self.open(now, self.opts.cooldown);
                }
            }
            BreakerState::HalfOpen => {
                let next = self
                    .cooldown
                    .saturating_mul(2)
                    .min(self.opts.max_cooldown)
                    .max(self.opts.cooldown);
                self.open(now, next);
            }
            BreakerState::Open { .. } | BreakerState::Disabled { .. } => {}
        }
    }

    fn open(&mut self, now: Instant, cooldown: Duration) {
        self.cooldown = cooldown;
        self.failures = 0;
        self.probe_out = false;
        self.state = BreakerState::Open {
            until: now + cooldown,
        };
        tracing::warn!(
            cooldown_ms = cooldown.as_millis() as u64,
            "предохранитель эндпоинта открыт"
        );
    }

    /// Сброс пользователем («повторить», D22): `Closed`, счёт и пауза — с начала.
    pub fn reset(&mut self) {
        self.state = BreakerState::Closed;
        self.failures = 0;
        self.cooldown = self.opts.cooldown;
        self.probe_out = false;
    }

    pub fn state(&self) -> BreakerState {
        self.state.clone()
    }
}

/// Отпечаток запроса для [`ParseGuard`] и джиттера (D21, D44): BLAKE3 от id эндпоинта
/// и канонического JSON запроса. Инструменты сортируются по имени — порядок
/// объявления не делает запрос другим; всё прочее (сообщения, сэмплирование,
/// рассуждение, `tool_choice`) входит в отпечаток. Результат — hex.
pub fn request_fingerprint(endpoint: &EndpointId, req: &ChatRequest) -> String {
    let mut tools: Vec<&crate::prefix::ToolSchema> = req.tools.iter().collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let mut h = blake3::Hasher::new();
    h.update(endpoint.0.as_bytes());
    h.update(&[0]);
    match (serde_json::to_value(&tools), serde_json::to_value(req)) {
        (Ok(tools), Ok(Value::Object(mut obj))) => {
            obj.insert("tools".into(), tools);
            h.update(canonical_json(&Value::Object(obj)).as_bytes());
        }
        // Сериализация канонических типов не падает; на всякий случай — `Debug`
        // запроса, чтобы разные запросы не слились в один отпечаток.
        _ => {
            h.update(format!("{tools:?}\0{req:?}").as_bytes());
        }
    }
    h.finalize().to_hex().to_string()
}

/// Отпечатки запросов, закончившихся ошибкой разбора, по run (D21, MDL-018).
#[derive(Debug, Clone, Default)]
pub struct ParseGuard {
    seen: HashMap<RunId, HashSet<String>>,
}

impl ParseGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Тот же отпечаток в том же run после ошибки разбора — `RepeatedAfterParseError`.
    pub fn check(&self, run: RunId, fp: &str) -> Result<(), ModelError> {
        if self.seen.get(&run).is_some_and(|s| s.contains(fp)) {
            return Err(ModelError::new(
                ErrorClass::RepeatedAfterParseError,
                "идентичный запрос после ошибки разбора не повторяется без изменения",
            ));
        }
        Ok(())
    }

    pub fn record(&mut self, run: RunId, fp: String) {
        self.seen.entry(run).or_default().insert(fp);
    }

    /// Забыть отпечатки run (run завершён).
    pub fn clear(&mut self, run: RunId) {
        self.seen.remove(&run);
    }
}

type Ev = Result<StreamEvent, ModelError>;

/// Открывает попытки через `open`, пересылает события в `tx`, повторяет по политике.
/// `open(attempt)` получает номер попытки начиная с 1. Перед попыткой n ≥ 2, если события
/// попытки n−1 уже ушли в `tx`, шлёт `Restart { attempt: n, reason }` (D20) — сразу
/// после решения о повторе, до паузы.
/// `on_attempt_error` вызывается на каждой неудачной попытке до решения о повторе
/// (Gateway ставит по нему cooldown группы на каждый 429). Итог — Ok после Finished
/// или последняя ошибка; ошибка также уходит в `tx` последним событием.
///
/// Канал попытки, закрытый до `Finished`, — `StreamCut`. Закрытый `tx` (потребитель
/// ушёл) — `Cancelled` без повторов и без `on_attempt_error`; канал попытки при этом
/// уничтожается, что отменяет её задачу (D2). Джиттер — `jitter_unit(seed, attempt)`.
pub async fn drive_with_retry<F, Fut, E>(
    policy: &RetryPolicy,
    seed: &str,
    tx: &mpsc::Sender<Ev>,
    mut open: F,
    mut on_attempt_error: E,
) -> Result<(), ModelError>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<mpsc::Receiver<Ev>, ModelError>>,
    E: FnMut(&ModelError),
{
    let cancelled = || ModelError::new(ErrorClass::Cancelled, "потребитель стрима закрыл канал");
    let mut retries_done: u32 = 0;
    loop {
        let attempt = retries_done.saturating_add(1);
        let mut forwarded = false;
        let outcome = tokio::select! {
            biased;
            _ = tx.closed() => return Err(cancelled()),
            r = run_attempt(open(attempt), tx, &mut forwarded) => r,
        };
        let err = match outcome {
            Ok(()) => return Ok(()),
            Err(AttemptError::Cancelled) => return Err(cancelled()),
            Err(AttemptError::Model(e)) => e,
        };
        on_attempt_error(&err);
        match policy.decide(retries_done, &err, jitter_unit(seed, attempt)) {
            RetryDecision::GiveUp => {
                tracing::debug!(attempt, class = ?err.class, "повторы прекращены");
                // Потребитель мог уйти: итог всё равно возвращается.
                let _ = tx.send(Err(err.clone())).await;
                return Err(err);
            }
            RetryDecision::Retry(delay) => {
                tracing::debug!(
                    attempt,
                    class = ?err.class,
                    delay_ms = delay.as_millis() as u64,
                    "повтор вызова модели"
                );
                if forwarded {
                    let restart = StreamEvent::Restart {
                        attempt: attempt.saturating_add(1),
                        reason: err.to_string(),
                    };
                    if tx.send(Ok(restart)).await.is_err() {
                        return Err(cancelled());
                    }
                }
                tokio::select! {
                    biased;
                    _ = tx.closed() => return Err(cancelled()),
                    _ = tokio::time::sleep(delay) => {}
                }
                retries_done = retries_done.saturating_add(1);
            }
        }
    }
}

/// Неудача одной попытки.
enum AttemptError {
    Model(ModelError),
    /// `tx` закрыт при пересылке.
    Cancelled,
}

/// Одна попытка: открыть и пересылать события до `Finished` или ошибки.
/// `forwarded` — хотя бы одно событие попытки ушло в `tx`.
async fn run_attempt<Fut>(
    opening: Fut,
    tx: &mpsc::Sender<Ev>,
    forwarded: &mut bool,
) -> Result<(), AttemptError>
where
    Fut: Future<Output = Result<mpsc::Receiver<Ev>, ModelError>>,
{
    let mut rx = opening.await.map_err(AttemptError::Model)?;
    loop {
        match rx.recv().await {
            None => {
                return Err(AttemptError::Model(ModelError::new(
                    ErrorClass::StreamCut,
                    "стрим попытки закрыт до события Finished",
                )));
            }
            Some(Err(e)) => return Err(AttemptError::Model(e)),
            Some(Ok(ev)) => {
                let done = matches!(ev, StreamEvent::Finished { .. });
                if tx.send(Ok(ev)).await.is_err() {
                    return Err(AttemptError::Cancelled);
                }
                *forwarded = true;
                if done {
                    return Ok(());
                }
            }
        }
    }
}
