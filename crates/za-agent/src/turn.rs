//! Ход модели (D7, D8, D13, D27): эффект `model`, события журнала хода, сигналы
//! цикла и разбор ответа перед фиксацией в истории.
//!
//! [`model_turn`] выполняет один вызов модели как эффект класса `Idempotent` с
//! аргументами `{endpoint, request, prefix}`. Вживую замыкание слота пишет
//! `model.request` (с `options` хода и `turn_attempt`, R1-B2 D67), читает стрим, склеивает дельты ([`DeltaBuffer`]) в `model.delta`,
//! пишет `agent.heartbeat` и, на `Finished`, `model.usage` вместе со строкой `usage`.
//! Итог хода — квитанция-успех с [`TurnOutput`]; ошибка модели — квитанция-отказ
//! `model_error:{json}`; обрыв сигналом — `interrupted:pause|cancel|input`, обрыв
//! детектором стрима ([`StreamWatch`], R1-B2 D58, D59) — `interrupted:repetition|thinking`. Реплей
//! получает тот же итог из журнала без вызова модели и без событий хода. Тот же путь
//! (`model_call`) у запроса сводки компакции (R1-B2 D46): эффект `compact` с
//! аргументами `{request}`, `model.request` и `model.usage` с `kind: "compact"`.
//!
//! История в этом модуле не меняется: ход фиксирует драйвер, и только по
//! завершённому ответу (D10, D13). [`review`] готовит ответ к фиксации:
//! нормализация вызовов (MDL-005), id, отказы и nudge для модели (D27).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use za_core::{EffectError, EffectFailure, EffectSource, EffectSpec, Journal, RunCtx};
use za_llm::normalize::{
    CallRejection, CallSource, NormalizedCall, extracted_mutation_blocked, normalize,
};
use za_llm::{
    ErrorClass, FinishReason, Message, ModelCall, ModelError, ModelPort, Part, QueueStatus, Role,
    StreamEvent, ToolCall, ToolSchema, Usage, WaitState,
};
use za_protocol::{Actor, BlobHash, EffectClass, EventKind, NewEvent, RunId};
use za_secrets::SecretMasker;
use za_store::{StoreError, UsageRow};

use crate::detect::{Degenerate, StreamWatch};
use crate::prompt::escape_untrusted;
use crate::request::request_hash;

// ---- Сигналы цикла (D9, D25, D26) ----

/// Живой сигнал циклу (D9, D25, D26): пауза, отмена, ввод в режиме interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Pause,
    Cancel,
    Input,
}

impl Signal {
    /// Имя в квитанции обрыва `interrupted:{name}` (D7).
    fn as_str(self) -> &'static str {
        match self {
            Signal::Pause => "pause",
            Signal::Cancel => "cancel",
            Signal::Input => "input",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "pause" => Some(Signal::Pause),
            "cancel" => Some(Signal::Cancel),
            "input" => Some(Signal::Input),
            _ => None,
        }
    }
}

/// Флаги управления (D26): последнее значение не теряется.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ControlFlags {
    pub pause: bool,
    pub cancel: bool,
}

/// Отправляющая сторона: её держит RunControl (задача 12).
#[derive(Clone)]
pub struct SignalTx {
    flags: Arc<watch::Sender<ControlFlags>>,
    input: mpsc::Sender<()>,
}

impl fmt::Debug for SignalTx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalTx")
            .field("flags", &*self.flags.borrow())
            .finish_non_exhaustive()
    }
}

impl SignalTx {
    /// `flags.pause = true`; повтор безвреден.
    pub fn pause(&self) {
        self.flags.send_modify(|f| f.pause = true);
    }

    /// `flags.cancel = true`; повтор безвреден.
    pub fn cancel(&self) {
        self.flags.send_modify(|f| f.cancel = true);
    }

    /// Сигнал ввода. Полный канал — сигнал уже ждёт, не ошибка; закрытый — цикла
    /// уже нет.
    pub fn input(&self) {
        let _ = self.input.try_send(());
    }
}

/// Принимающая сторона цикла.
#[derive(Debug)]
pub struct Signals {
    flags: watch::Receiver<ControlFlags>,
    input: mpsc::Receiver<()>,
    /// Флаги, уже отданные циклу.
    seen: ControlFlags,
    /// Отправитель тех же флагов: цикл снимает применённую паузу ([`Signals::clear_pause`]).
    flags_tx: Arc<watch::Sender<ControlFlags>>,
    /// Сигнал ввода, пришедший во время компакции (R1-B2, Ruling I-2): компакция его не
    /// ждёт, но он не теряется — следующий `recv` отдаёт его сразу, граница снимает.
    deferred_input: bool,
}

impl Signals {
    /// Непрочитанный Cancel или Pause по текущим флагам; помечает его прочитанным.
    /// Снятый флаг снимает и отметку: повторная пауза после снятия снова сигнал.
    fn unseen_flag(&mut self) -> Option<Signal> {
        let cur = *self.flags.borrow_and_update();
        if !cur.cancel {
            self.seen.cancel = false;
        }
        if !cur.pause {
            self.seen.pause = false;
        }
        if cur.cancel && !self.seen.cancel {
            self.seen.cancel = true;
            return Some(Signal::Cancel);
        }
        if cur.pause && !self.seen.pause {
            self.seen.pause = true;
            return Some(Signal::Pause);
        }
        None
    }

    /// Следующий ещё не прочитанный сигнал; приоритет Cancel, Pause, Input. Отменобезопасен (для select!).
    /// Все отправители брошены — ждёт вечно (std::future::pending).
    pub async fn recv(&mut self) -> Signal {
        let mut flags_open = true;
        let mut input_open = true;
        loop {
            if let Some(s) = self.unseen_flag() {
                return s;
            }
            if std::mem::take(&mut self.deferred_input) {
                return Signal::Input;
            }
            if !flags_open && !input_open {
                return std::future::pending().await;
            }
            // Обе ветви отменобезопасны: `changed` и `recv` ничего не теряют при drop.
            tokio::select! {
                biased;
                r = self.flags.changed(), if flags_open => {
                    if r.is_err() {
                        flags_open = false;
                    }
                }
                r = self.input.recv(), if input_open => match r {
                    Some(()) => return Signal::Input,
                    None => input_open = false,
                },
            }
        }
    }

    /// Без ожидания, на границе (D9): непрочитанный Cancel или Pause (помечается прочитанным);
    /// накопленный Input выбрасывается; иначе None.
    pub fn drain(&mut self) -> Option<Signal> {
        // Ввод прочитает сама граница, сигнал о нём устарел.
        while self.input.try_recv().is_ok() {}
        self.deferred_input = false;
        self.unseen_flag()
    }

    /// Ввод уже прочитан границей (повторное чтение после компакции): отложенный и
    /// накопленный сигнал ввода устарели. Флаги не трогаются.
    pub(crate) fn discard_input(&mut self) {
        while self.input.try_recv().is_ok() {}
        self.deferred_input = false;
    }

    /// Сигнал ввода пришёл во время компакции: отдать его следующему `recv`.
    fn defer_input(&mut self) {
        self.deferred_input = true;
    }

    /// Клон watch::Receiver — для наблюдения флагов рядом с future пакета инструментов (D26).
    pub fn flags(&self) -> watch::Receiver<ControlFlags> {
        self.flags.clone()
    }

    /// Снимает флаг паузы и отметку о нём: пауза применена (или run продолжен), и
    /// следующая `pause()` в том же процессе снова сигнал. Флаг отмены не трогается.
    /// `pause()`, пришедшая после этого вызова, не теряется: флаг снова поднят, а
    /// отметки нет.
    pub fn clear_pause(&mut self) {
        self.flags_tx.send_modify(|f| f.pause = false);
        self.seen.pause = false;
    }
}

/// Канал сигналов цикла: флаги в `watch` (D26), ввод — `mpsc` ёмкостью 1.
pub fn signal_channel() -> (SignalTx, Signals) {
    let (flags_tx, flags_rx) = watch::channel(ControlFlags::default());
    let flags_tx = Arc::new(flags_tx);
    let (input_tx, input_rx) = mpsc::channel(1);
    (
        SignalTx {
            flags: Arc::clone(&flags_tx),
            input: input_tx,
        },
        Signals {
            flags: flags_rx,
            input: input_rx,
            seen: ControlFlags::default(),
            flags_tx,
            deferred_input: false,
        },
    )
}

// ---- Склейка дельт (D8, D38) ----

/// Размер склейки дельт: при достижении буфер записывается (D8).
pub const DELTA_FLUSH_BYTES: usize = 2048;
/// Возраст склейки дельт от первой дельты буфера (D8).
pub const DELTA_FLUSH_AFTER: Duration = Duration::from_millis(200);
/// Смена только позиции в очереди пишется в `agent.heartbeat` не чаще этого (D8).
pub const HEARTBEAT_POSITION_EVERY: Duration = Duration::from_secs(1);

/// Склейка дельт D8, D38: до 2048 байт или 200 мс от первой дельты буфера. Instant — tokio::time::Instant.
#[derive(Debug, Default)]
pub struct DeltaBuffer {
    text: String,
    since: Option<Instant>,
}

impl DeltaBuffer {
    /// Some(текст) — пора записать: после добавления ≥ 2048 байт или now − since ≥ 200 мс.
    pub fn push(&mut self, s: &str, now: Instant) -> Option<String> {
        if s.is_empty() && self.text.is_empty() {
            return None;
        }
        let since = *self.since.get_or_insert(now);
        self.text.push_str(s);
        if self.text.len() >= DELTA_FLUSH_BYTES
            || now.saturating_duration_since(since) >= DELTA_FLUSH_AFTER
        {
            self.take()
        } else {
            None
        }
    }

    /// since + 200 мс — момент сброса по таймеру; None — буфер пуст.
    pub fn deadline(&self) -> Option<Instant> {
        self.since.map(|t| t + DELTA_FLUSH_AFTER)
    }

    /// Всё накопленное (по таймеру, перед Restart, Finished и ошибкой); None — пусто.
    pub fn take(&mut self) -> Option<String> {
        self.since = None;
        if self.text.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.text))
        }
    }
}

/// output × 1000 / (duration_ms − ttft_ms); None, если ttft_ms нет или duration_ms <= ttft_ms (D8).
pub fn output_tps(output: u64, ttft_ms: Option<u64>, duration_ms: u64) -> Option<f64> {
    let ttft = ttft_ms?;
    if duration_ms <= ttft {
        return None;
    }
    Some(output as f64 * 1000.0 / (duration_ms - ttft) as f64)
}

// ---- Эффект model (D7) ----

/// Имя эффекта вызова модели (D7).
pub const MODEL_EFFECT: &str = "model";
/// Имя эффекта запроса сводки компакции (R1-B2 D46).
pub const COMPACT_EFFECT: &str = "compact";
/// Значение `kind` в `model.request` и `model.usage` запроса компакции (D46, D67).
pub const COMPACT_KIND: &str = "compact";
/// Префикс квитанции-отказа с ошибкой модели (D7).
pub(crate) const MODEL_ERROR: &str = "model_error:";
/// Префикс квитанции обрыва сигналом (D7).
const INTERRUPTED: &str = "interrupted:";

/// Итог хода модели — значение квитанции эффекта `model` (D7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnOutput {
    /// Служебные сообщения последней попытки (`Role::System`, D10).
    pub service: Vec<Message>,
    pub message: Message,
    pub finish: FinishReason,
    pub usage: Usage,
    /// Время до первого события ответа последней попытки от начала хода, мс.
    pub ttft_ms: Option<u64>,
    /// Длительность хода от запроса до `Finished`, мс.
    pub duration_ms: u64,
}

/// Ошибка хода модели.
#[derive(Debug)]
pub enum TurnError {
    /// Ошибка модели или очереди; `last_wait` — последний `WaitState` стрима
    /// (для причины паузы D11).
    Model {
        error: ModelError,
        last_wait: Option<WaitState>,
    },
    /// Обрыв сигналом; source == Journal — квитанция `interrupted:*` прочитана при реплее (D7).
    Aborted {
        signal: Signal,
        source: EffectSource,
    },
    /// Обрыв детектором стрима (R1-B2 D58, D59): квитанция `interrupted:repetition` или
    /// `interrupted:thinking`; вживую и при реплее одинаково.
    Degenerate(Degenerate),
    /// Ошибка протокола эффектов (журнал, неизвестный исход, расхождение с журналом).
    Effect(EffectError),
    /// Запрос хода не сериализуется для `request_hash`: ключ эффекта не занят,
    /// драйвер превращает ошибку в `LoopError`.
    Request(serde_json::Error),
}

impl fmt::Display for TurnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TurnError::Model { error, .. } => write!(f, "ошибка модели: {error}"),
            TurnError::Aborted { signal, source } => {
                write!(f, "ход прерван сигналом {signal:?} ({source:?})")
            }
            TurnError::Degenerate(d) => {
                write!(f, "стрим хода оборван детектором {}", d.as_str())
            }
            TurnError::Effect(e) => write!(f, "{e}"),
            TurnError::Request(e) => write!(f, "запрос хода не сериализуется: {e}"),
        }
    }
}

impl std::error::Error for TurnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TurnError::Model { error, .. } => Some(error),
            TurnError::Effect(e) => Some(e),
            TurnError::Request(e) => Some(e),
            TurnError::Aborted { .. } | TurnError::Degenerate(_) => None,
        }
    }
}

impl From<EffectError> for TurnError {
    fn from(e: EffectError) -> Self {
        TurnError::Effect(e)
    }
}

/// Хэши блобов для model.request (D8): FrozenPrefix (пишет драйвер при старте run) и сообщения истории.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestRefs {
    pub frozen: BlobHash,
    pub messages: Vec<BlobHash>,
    /// Хэш стабильного префикса (аргумент эффекта `prefix`).
    pub prefix: String,
    /// Версия промптов замороженного префикса run (`FrozenPrefix::prompt_version`,
    /// PRM-010): run, начатый до обновления, продолжает со своей версией, а не с
    /// текущей [`crate::PROMPT_VERSION`].
    pub prompt_version: String,
}

/// Тело квитанции `model_error:` (D7).
#[derive(Serialize, Deserialize)]
struct ModelFailure {
    error: ModelError,
    last_wait: Option<WaitState>,
}

/// Эффект `model` (D7): стрим, события D8 (дельты, restart, heartbeat, usage — только вживую), итог.
/// `sig` — сигналы цикла; первый пришедший обрывает стрим (drop) и записывает `interrupted:*`.
/// Первая попытка хода без детекторов стрима; с ними — [`model_turn_with`].
pub async fn model_turn(
    ctx: &mut RunCtx,
    j: &Journal,
    model: &dyn ModelPort,
    call: ModelCall,
    refs: &RequestRefs,
    turn: u32,
    sig: &mut Signals,
) -> Result<TurnOutput, TurnError> {
    model_turn_with(ctx, j, model, call, refs, turn, sig, None, 0).await
}

/// [`model_turn`] с детекторами стрима и номером попытки хода (R1-B2 D58, D59, D67).
///
/// `watch` смотрит дельты живого стрима: срабатывание обрывает стрим (drop), буферы
/// дельт записываются, квитанция — `interrupted:repetition|thinking`, итог —
/// [`TurnError::Degenerate`]. Дедлайн рассуждения ([`StreamWatch::deadline`]) — ветка
/// того же `select!`, что сигналы и сброс дельт. Реплей отдаёт тот же обрыв из
/// квитанции без модели. `turn_attempt` — поле `model.request` (0 — первая попытка,
/// повторы D49, D58, D59 — с 1).
#[allow(clippy::too_many_arguments)]
pub async fn model_turn_with(
    ctx: &mut RunCtx,
    j: &Journal,
    model: &dyn ModelPort,
    call: ModelCall,
    refs: &RequestRefs,
    turn: u32,
    sig: &mut Signals,
    watch: Option<&mut StreamWatch>,
    turn_attempt: u32,
) -> Result<TurnOutput, TurnError> {
    model_call(
        ctx,
        j,
        model,
        call,
        refs,
        turn,
        sig,
        CallKind::Turn {
            attempt: turn_attempt,
        },
        watch,
        None,
    )
    .await
}

/// [`model_turn_with`] с маскировкой секретов ответа до квитанции (R1-C1 D100).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn model_turn_masked(
    ctx: &mut RunCtx,
    j: &Journal,
    model: &dyn ModelPort,
    call: ModelCall,
    refs: &RequestRefs,
    turn: u32,
    sig: &mut Signals,
    watch: Option<&mut StreamWatch>,
    turn_attempt: u32,
    masker: Option<&SecretMasker>,
) -> Result<TurnOutput, TurnError> {
    model_call(
        ctx,
        j,
        model,
        call,
        refs,
        turn,
        sig,
        CallKind::Turn {
            attempt: turn_attempt,
        },
        watch,
        masker,
    )
    .await
}

/// Маска ответа модели (D100): текст, рассуждение, аргументы вызовов (`arguments` и
/// `raw_arguments`). Всё, что строится из истории, дальше уже маскировано.
pub(crate) fn mask_message(m: &SecretMasker, msg: &mut Message) {
    let text = |s: &str| m.mask(s).into_owned();
    for p in &mut msg.parts {
        match p {
            Part::Text(t) => *t = text(t),
            Part::Reasoning { text: t, .. } => *t = text(t),
            Part::ToolCall(c) => {
                c.arguments = za_protocol::mask_json_with(&text, &c.arguments);
                if let Some(raw) = &c.raw_arguments {
                    c.raw_arguments = Some(za_protocol::mask_str_with(&text, raw));
                }
            }
            Part::ToolResult { content, .. } => *content = text(content),
        }
    }
}

/// Вид вызова модели (R1-B2 D46, D67).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CallKind {
    /// Ход модели: эффект `model`; `attempt` — `turn_attempt` в `model.request` (0 —
    /// первая попытка, 1 — повтор после аварийной компакции D49).
    Turn { attempt: u32 },
    /// Компакция (D46): эффект `compact` с аргументами `{request}`, `model.request` и
    /// `model.usage` с `kind: "compact"`. Ввод пользователя стрим компакции не обрывает:
    /// его прочитает следующая граница.
    Compact,
}

/// Вызов модели как эффект `Idempotent`: ход ([`model_turn_with`]) или компакция (D46).
/// `watch` — детекторы стрима живого хода (D58, D59); у компакции их нет.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn model_call(
    ctx: &mut RunCtx,
    j: &Journal,
    model: &dyn ModelPort,
    call: ModelCall,
    refs: &RequestRefs,
    turn: u32,
    sig: &mut Signals,
    kind: CallKind,
    watch: Option<&mut StreamWatch>,
    masker: Option<&SecretMasker>,
) -> Result<TurnOutput, TurnError> {
    // До резервирования: ошибка хэша не занимает ключ эффекта.
    let request = request_hash(&call.request).map_err(TurnError::Request)?;
    let slot = ctx.reserve()?;
    let key = slot.key().0.clone();
    let (spec, turn_attempt, compact) = match kind {
        CallKind::Turn { attempt } => (
            EffectSpec {
                class: EffectClass::Idempotent,
                name: MODEL_EFFECT.into(),
                args: json!({
                    "endpoint": call.endpoint.0,
                    "request": request,
                    "prefix": refs.prefix,
                }),
            },
            attempt,
            false,
        ),
        // D46: реплей с другой историей даёт `ReplayMismatch`, а не чужую сводку.
        CallKind::Compact => (
            EffectSpec {
                class: EffectClass::Idempotent,
                name: COMPACT_EFFECT.into(),
                args: json!({ "request": request }),
            },
            0,
            true,
        ),
    };
    let live = LiveTurn {
        j,
        run: ctx.run_id(),
        turn,
        turn_attempt,
        compact,
        key: &key,
        request: &request,
        refs,
        masker,
    };
    let mut store_failure: Option<StoreError> = None;
    let failure = &mut store_failure;
    // Ошибка журнала и несериализуемый итог — разовый сбой (R1-C1 D72): квитанции нет,
    // resume повторяет ход (класс `Idempotent`), а не отдаёт вечный отказ.
    let (r, source) = slot
        .run_fallible(spec, move || async move {
            match live.run(model, call, sig, watch).await {
                Ok(mut out) => {
                    // D100: квитанция хранит маскированный ответ, реплей от набора
                    // секретов не зависит.
                    if let Some(m) = masker {
                        mask_message(m, &mut out.message);
                    }
                    serde_json::to_value(&out).map_err(|e| {
                        EffectFailure::Transient(format!("итог хода не сериализуется: {e}"))
                    })
                }
                Err(Live::Model { error, last_wait }) => {
                    let body = ModelFailure { error, last_wait };
                    match serde_json::to_string(&body) {
                        Ok(s) => Err(EffectFailure::Recorded(format!("{MODEL_ERROR}{s}"))),
                        Err(e) => Err(EffectFailure::Transient(format!(
                            "ошибка модели не сериализуется: {e}"
                        ))),
                    }
                }
                Err(Live::Signal(s)) => Err(EffectFailure::Recorded(format!(
                    "{INTERRUPTED}{}",
                    s.as_str()
                ))),
                Err(Live::Degenerate(d)) => Err(EffectFailure::Recorded(format!(
                    "{INTERRUPTED}{}",
                    d.as_str()
                ))),
                Err(Live::Store(e)) => {
                    let msg = e.to_string();
                    *failure = Some(e);
                    Err(EffectFailure::Transient(msg))
                }
            }
        })
        .await;
    ctx.observe(&r);
    // Ошибка журнала посреди хода: квитанции нет, контекст отравлен; драйвер ставит
    // run на паузу `store` статусом.
    if let Some(e) = store_failure {
        let err = EffectError::Store(e);
        ctx.observe::<()>(&Err(err.clone()));
        return Err(err.into());
    }
    match r {
        Ok(v) => serde_json::from_value::<TurnOutput>(v).map_err(|e| {
            let err = EffectError::Store(StoreError::Corrupt(format!(
                "итог хода {key} не разбирается: {e}"
            )));
            ctx.observe::<()>(&Err(err.clone()));
            err.into()
        }),
        Err(EffectError::Failed(msg)) => Err(decode_failure(msg, source)),
        Err(e) => Err(e.into()),
    }
}

/// Квитанция-отказ эффекта `model` обратно в ошибку хода (D7). Обрыв детектором
/// (`interrupted:repetition|thinking`, R1-B2 Ruling 1.9) разбирается до сигналов.
fn decode_failure(msg: String, source: EffectSource) -> TurnError {
    if let Some(body) = msg.strip_prefix(MODEL_ERROR) {
        match serde_json::from_str::<ModelFailure>(body) {
            Ok(f) => {
                return TurnError::Model {
                    error: f.error,
                    last_wait: f.last_wait,
                };
            }
            Err(e) => tracing::warn!(error = %e, "квитанция model_error не разбирается"),
        }
    } else if let Some(tag) = msg.strip_prefix(INTERRUPTED) {
        if let Some(d) = Degenerate::parse(tag) {
            return TurnError::Degenerate(d);
        }
        if let Some(signal) = Signal::parse(tag) {
            return TurnError::Aborted { signal, source };
        }
    }
    TurnError::Effect(EffectError::Failed(msg))
}

/// Исход живого хода внутри замыкания слота.
enum Live {
    Model {
        error: ModelError,
        last_wait: Option<WaitState>,
    },
    Signal(Signal),
    /// Обрыв детектором стрима (D58, D59).
    Degenerate(Degenerate),
    Store(StoreError),
}

impl From<StoreError> for Live {
    fn from(e: StoreError) -> Self {
        Live::Store(e)
    }
}

/// Что пришло первым в цикле стрима.
enum Step {
    Signal(Signal),
    Event(Option<Result<StreamEvent, ModelError>>),
    Timer,
    /// Дедлайн рассуждения [`StreamWatch::deadline`] (D59).
    ThinkingDeadline,
}

/// Последний записанный heartbeat.
struct Beat {
    state: &'static str,
    position: Option<u32>,
    at: Instant,
}

/// Живой ход: события журнала и стрим.
#[derive(Clone, Copy)]
struct LiveTurn<'a> {
    j: &'a Journal,
    run: RunId,
    turn: u32,
    /// Попытка хода (D67): 0 — первая; повторы D49, D58, D59 — с 1.
    turn_attempt: u32,
    /// Запрос компакции (D46): `kind: "compact"`, ввод не обрывает стрим.
    compact: bool,
    key: &'a str,
    request: &'a str,
    refs: &'a RequestRefs,
    /// Маска дельт `model.delta` по фрагментам (D100, best effort).
    masker: Option<&'a SecretMasker>,
}

/// Состояние стрима одного хода.
struct StreamState<'w> {
    attempt: u32,
    text: DeltaBuffer,
    reasoning: DeltaBuffer,
    service: Vec<Message>,
    last_wait: Option<WaitState>,
    beat: Option<Beat>,
    ttft_ms: Option<u64>,
    /// Детекторы стрима (D58, D59); `None` — без них (компакция, [`model_turn`]).
    watch: Option<&'w mut StreamWatch>,
    /// Срабатывание детектора на последнем шаге цикла стрима: стрим обрывается.
    degenerate: Option<Degenerate>,
    /// Удержанные хвосты текста и рассуждения маски потока (D100, I3 ревью задачи 12):
    /// секрет на стыке дельт маскируется целиком.
    held_text: String,
    held_reasoning: String,
}

impl LiveTurn<'_> {
    /// `kind: "compact"` у событий запроса компакции (D46, D67).
    fn mark_kind(&self, payload: &mut Value) {
        if self.compact {
            payload["kind"] = Value::String(COMPACT_KIND.into());
        }
    }

    async fn append(&self, actor: Actor, kind: EventKind, payload: Value) -> Result<(), Live> {
        self.j
            .append(NewEvent {
                run_id: self.run,
                parent_seq: None,
                actor,
                kind,
                payload,
                tainted: false,
            })
            .await?;
        Ok(())
    }

    async fn run(
        self,
        model: &dyn ModelPort,
        call: ModelCall,
        sig: &mut Signals,
        watch: Option<&mut StreamWatch>,
    ) -> Result<TurnOutput, Live> {
        // Ввод во время компакции откладывается после её конца: внутри стрима
        // отложенный сигнал отдавался бы тому же `recv` снова и снова.
        let mut input_seen = false;
        let r = self.stream(model, call, sig, &mut input_seen, watch).await;
        if input_seen {
            sig.defer_input();
        }
        r
    }

    async fn stream(
        self,
        model: &dyn ModelPort,
        call: ModelCall,
        sig: &mut Signals,
        input_seen: &mut bool,
        watch: Option<&mut StreamWatch>,
    ) -> Result<TurnOutput, Live> {
        let endpoint = call.endpoint.0.clone();
        // D67: параметры хода вне истории — по ним инспектор восстанавливает запрос точно.
        let r = &call.request;
        let options = json!({
            "tool_choice": r.tool_choice,
            "sampling": r.sampling,
            "max_output_tokens": r.max_output_tokens,
            "reasoning": r.reasoning,
        });
        let mut payload = json!({
            "turn": self.turn,
            "turn_attempt": self.turn_attempt,
            "endpoint": endpoint,
            "effect_key": self.key,
            "request": self.request,
            "prefix": self.refs.prefix,
            "frozen": self.refs.frozen,
            "messages": self.refs.messages,
            "options": options,
            "prompt_version": self.refs.prompt_version,
        });
        self.mark_kind(&mut payload);
        self.append(Actor::Runtime, EventKind::ModelRequest, payload)
            .await?;
        let started = Instant::now();
        let mut st = StreamState {
            attempt: 1,
            text: DeltaBuffer::default(),
            reasoning: DeltaBuffer::default(),
            service: Vec::new(),
            last_wait: None,
            beat: None,
            ttft_ms: None,
            watch,
            degenerate: None,
            held_text: String::new(),
            held_reasoning: String::new(),
        };
        let mut stream = match model.stream(call).await {
            Ok(s) => s,
            Err(error) => {
                return Err(Live::Model {
                    error,
                    last_wait: None,
                });
            }
        };
        loop {
            let deadline = min_deadline(st.text.deadline(), st.reasoning.deadline());
            // D59: замолчавший стрим рассуждения обрывается по дедлайну, а не только по
            // приходу следующей дельты.
            let thinking = st.watch.as_ref().and_then(|w| w.deadline());
            let step = tokio::select! {
                // Сигналы первыми: поток событий стрима не задерживает обрыв (NFR-03).
                biased;
                s = sig.recv() => Step::Signal(s),
                ev = stream.next() => Step::Event(ev),
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)),
                    if deadline.is_some() => Step::Timer,
                _ = tokio::time::sleep_until(thinking.unwrap_or_else(Instant::now)),
                    if thinking.is_some() => Step::ThinkingDeadline,
            };
            match step {
                // Компакцию ввод не обрывает, но сигнал не теряется (Ruling I-2): граница
                // перечитывает ввод после компакции, ход после аварийной — обрывается.
                Step::Signal(Signal::Input) if self.compact => *input_seen = true,
                Step::Signal(s) => {
                    // Drop закрывает соединение (D2 R1-A) до записи квитанции.
                    drop(stream);
                    tracing::info!(run = %self.run, turn = self.turn, signal = ?s, "ход модели прерван сигналом");
                    self.flush(&mut st).await?;
                    return Err(Live::Signal(s));
                }
                Step::Timer => self.flush_due(&mut st, Instant::now()).await?,
                Step::ThinkingDeadline => st.degenerate = Some(Degenerate::Thinking),
                Step::Event(None) => {
                    self.flush(&mut st).await?;
                    return Err(Live::Model {
                        error: ModelError::new(
                            ErrorClass::StreamCut,
                            "стрим закрыт до события Finished",
                        ),
                        last_wait: st.last_wait,
                    });
                }
                Step::Event(Some(Err(error))) => {
                    self.flush(&mut st).await?;
                    return Err(Live::Model {
                        error,
                        last_wait: st.last_wait,
                    });
                }
                Step::Event(Some(Ok(ev))) => {
                    if let Some(out) = self.on_event(&mut st, ev, &endpoint, started).await? {
                        return Ok(out);
                    }
                }
            }
            if let Some(d) = st.degenerate {
                // Как у сигнала: drop закрывает соединение до записи квитанции.
                drop(stream);
                tracing::warn!(run = %self.run, turn = self.turn, detector = d.as_str(),
                    "стрим хода оборван детектором");
                self.flush(&mut st).await?;
                return Err(Live::Degenerate(d));
            }
        }
    }

    /// Одно событие стрима; `Some` — итог хода.
    async fn on_event(
        &self,
        st: &mut StreamState<'_>,
        ev: StreamEvent,
        endpoint: &str,
        started: Instant,
    ) -> Result<Option<TurnOutput>, Live> {
        let now = Instant::now();
        match ev {
            StreamEvent::Queued(q) => {
                st.last_wait = Some(q.state.clone());
                self.heartbeat_queued(st, &q, now).await?;
            }
            StreamEvent::Started { .. } => self.heartbeat_generating(st, now).await?,
            StreamEvent::Restart { attempt, reason } => {
                self.flush(st).await?;
                self.append(
                    Actor::Model,
                    EventKind::ModelDelta,
                    json!({"turn": self.turn, "effect_key": self.key, "attempt": st.attempt, "restart": reason}),
                )
                .await?;
                // Всё накопленное прежней попыткой отбрасывается (D20 R1-A).
                st.attempt = attempt;
                st.service.clear();
                st.ttft_ms = None;
                if let Some(w) = st.watch.as_deref_mut() {
                    w.on_restart();
                }
            }
            StreamEvent::Progress { .. } => {}
            StreamEvent::TextDelta(t) => {
                mark_ttft(st, started, now);
                st.degenerate = st.watch.as_deref_mut().and_then(|w| w.on_text(&t));
                if let Some(chunk) = st.text.push(&t, now) {
                    self.write_delta(st, "text", chunk, false).await?;
                }
            }
            StreamEvent::ReasoningDelta(t) => {
                mark_ttft(st, started, now);
                st.degenerate = st
                    .watch
                    .as_deref_mut()
                    .and_then(|w| w.on_reasoning(&t, now));
                if let Some(chunk) = st.reasoning.push(&t, now) {
                    self.write_delta(st, "reasoning", chunk, false).await?;
                }
            }
            StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallArgs { .. } => {
                mark_ttft(st, started, now);
                if let Some(w) = st.watch.as_deref_mut() {
                    w.on_calls();
                }
            }
            StreamEvent::ServiceMessage(m) => st.service.push(m),
            StreamEvent::Finished {
                message,
                finish,
                usage,
            } => {
                self.flush(st).await?;
                let duration_ms = millis(now.saturating_duration_since(started));
                let tps = output_tps(usage.output, st.ttft_ms, duration_ms);
                let row = UsageRow {
                    model: endpoint.to_owned(),
                    input: usage.input,
                    cached: usage.cached,
                    output: usage.output,
                    reasoning: usage.reasoning,
                    gpu_ms: usage.gpu_ms,
                    cost_usd: usage.cost_usd,
                };
                let mut payload = json!({
                    "turn": self.turn,
                    "endpoint": endpoint,
                    "effect_key": self.key,
                    "usage": usage,
                    "finish": finish,
                    "ttft_ms": st.ttft_ms,
                    "duration_ms": duration_ms,
                    "output_tps": tps,
                });
                self.mark_kind(&mut payload);
                self.j.record_usage(self.run, row, payload).await?;
                return Ok(Some(TurnOutput {
                    service: std::mem::take(&mut st.service),
                    message,
                    finish,
                    usage,
                    ttft_ms: st.ttft_ms,
                    duration_ms,
                }));
            }
        }
        Ok(None)
    }

    /// Дельта `field` (`text` | `reasoning`) в журнал. С маской (D100) — окно перекрытия:
    /// хвост [`SecretMasker::window`] держится до следующей дельты, `last` (конец стрима
    /// или попытки) отдаёт всё; пустая часть не пишется.
    async fn write_delta(
        &self,
        st: &mut StreamState<'_>,
        field: &'static str,
        chunk: String,
        last: bool,
    ) -> Result<(), Live> {
        let chunk = match self.masker {
            Some(m) => {
                let held = if field == "text" {
                    &mut st.held_text
                } else {
                    &mut st.held_reasoning
                };
                let mut all = std::mem::take(held);
                all.push_str(&chunk);
                let (out, used) = m.mask_stream(&all, last);
                *held = all[used..].to_owned();
                out
            }
            None => chunk,
        };
        if chunk.is_empty() {
            return Ok(());
        }
        let mut payload = json!({"turn": self.turn, "effect_key": self.key, "attempt": st.attempt});
        payload[field] = Value::String(chunk);
        self.append(Actor::Model, EventKind::ModelDelta, payload)
            .await
    }

    /// Сброс обоих буферов (конец стрима или попытки): рассуждение, затем текст; маска
    /// потока отдаёт и удержанный хвост.
    async fn flush(&self, st: &mut StreamState<'_>) -> Result<(), Live> {
        let r = st.reasoning.take();
        if r.is_some() || !st.held_reasoning.is_empty() {
            self.write_delta(st, "reasoning", r.unwrap_or_default(), true)
                .await?;
        }
        let t = st.text.take();
        if t.is_some() || !st.held_text.is_empty() {
            self.write_delta(st, "text", t.unwrap_or_default(), true)
                .await?;
        }
        Ok(())
    }

    /// Сброс буферов, чей срок истёк к `now`.
    async fn flush_due(&self, st: &mut StreamState<'_>, now: Instant) -> Result<(), Live> {
        if st.reasoning.deadline().is_some_and(|d| d <= now)
            && let Some(chunk) = st.reasoning.take()
        {
            self.write_delta(st, "reasoning", chunk, false).await?;
        }
        if st.text.deadline().is_some_and(|d| d <= now)
            && let Some(chunk) = st.text.take()
        {
            self.write_delta(st, "text", chunk, false).await?;
        }
        Ok(())
    }

    async fn heartbeat_queued(
        &self,
        st: &mut StreamState<'_>,
        q: &QueueStatus,
        now: Instant,
    ) -> Result<(), Live> {
        let state = wait_state_name(&q.state);
        let kind = match &q.state {
            WaitState::WaitingResource { kind } => Value::String(kind.clone()),
            _ => Value::Null,
        };
        let payload = json!({
            "turn": self.turn,
            "state": state,
            "position": q.position,
            "waited_ms": q.waited_ms,
            "kind": kind,
        });
        self.heartbeat(st, state, Some(q.position), payload, now)
            .await
    }

    async fn heartbeat_generating(
        &self,
        st: &mut StreamState<'_>,
        now: Instant,
    ) -> Result<(), Live> {
        let payload = json!({
            "turn": self.turn,
            "state": GENERATING,
            "position": null,
            "waited_ms": null,
            "kind": null,
        });
        self.heartbeat(st, GENERATING, None, payload, now).await
    }

    /// D8: смена `state` пишется сразу, смена только позиции — не чаще раза в секунду.
    async fn heartbeat(
        &self,
        st: &mut StreamState<'_>,
        state: &'static str,
        position: Option<u32>,
        payload: Value,
        now: Instant,
    ) -> Result<(), Live> {
        let due = match &st.beat {
            None => true,
            Some(b) => {
                b.state != state
                    || (b.position != position
                        && now.saturating_duration_since(b.at) >= HEARTBEAT_POSITION_EVERY)
            }
        };
        if due {
            self.append(Actor::Runtime, EventKind::AgentHeartbeat, payload)
                .await?;
            st.beat = Some(Beat {
                state,
                position,
                at: now,
            });
        }
        Ok(())
    }
}

/// Состояние heartbeat при `Started` (D8).
const GENERATING: &str = "generating";

/// Имя `WaitState` в snake_case (D8).
fn wait_state_name(s: &WaitState) -> &'static str {
    match s {
        WaitState::Queued => "queued",
        WaitState::WaitingResource { .. } => "waiting_resource",
        WaitState::Cooldown { .. } => "cooldown",
        WaitState::Granted => "granted",
        WaitState::Failed { .. } => "failed",
    }
}

fn mark_ttft(st: &mut StreamState<'_>, started: Instant, now: Instant) {
    if st.ttft_ms.is_none() {
        st.ttft_ms = Some(millis(now.saturating_duration_since(started)));
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn min_deadline(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

// ---- Разбор ответа (D27) ----

/// Решение по вызову: исполнить или ответить моделью результатом-ошибкой.
#[derive(Debug, Clone, PartialEq)]
pub enum CallVerdict {
    Run,
    /// Не исполнять; текст — результат-ошибка для модели (английский, D23).
    Reject(String),
    /// Не исполнять; текст — результат без ошибки: прошлый результат того же вызова с
    /// пометкой `REPEAT_MARK` (R1-B2 D54). Слот резервируется, как у всех вызовов (D15).
    Reuse(String),
}

/// Вызов из ответа модели с решением.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedCall {
    pub call: ToolCall,
    pub source: CallSource,
    pub verdict: CallVerdict,
}

/// Ответ модели, подготовленный к фиксации в истории (D27).
#[derive(Debug, Clone, PartialEq)]
pub struct Reviewed {
    /// Сообщение ассистента для истории: текст и рассуждение без разметки вызовов,
    /// вызовы — все из `calls` (каждому нужен результат).
    pub assistant: Message,
    pub calls: Vec<PlannedCall>,
    /// Управляющие сообщения для модели (оборачивает драйвер, `ControlAction::Nudge`).
    pub nudges: Vec<String>,
    /// Неразобранные вызовы (MDL-018): для [`report_parse_errors`].
    pub parse_errors: u32,
    /// Текст ответа без разметки вызовов.
    pub text: String,
    /// Исправления нормализации у принятых вызовов (`turn.finished.repairs`, D8).
    pub repairs: u32,
    /// Отказы нормализации (`turn.finished.rejected`, D8).
    pub rejected: u32,
}

/// Результат извлечённой из текста мутации (D27).
pub(crate) const BLOCKED: &str = "blocked: calls that change state must use the tool-calling interface; this call was not executed";
/// Nudge извлечённой мутации (D23, D27).
pub(crate) const MUTATION_NUDGE: &str = "The tool call in your text was not executed: calls that change state must use the tool-calling interface. Make the call again as a tool call.";
/// Nudge при `Length` для извлечённого вызова, который не разобран: JSON обрезан
/// пределом вывода, а не испорчен, поэтому просьба о валидном JSON неуместна.
pub(crate) const LENGTH_CUT_NUDGE: &str = "Your tool call was cut by the output limit (finish_reason=length) and was not executed. Repeat the call with shorter arguments or split the work.";
/// Результат вызова в ответе, обрезанном по длине (D27).
pub(crate) const LENGTH_CUT: &str = "not executed: response cut by output limit (finish_reason=length). Repeat the call with shorter arguments or split the work.";
/// Сколько символов исходной разметки показывается модели.
const RAW_SHOWN: usize = 200;

/// D27. `classes` — класс эффекта по имени инструмента. Нативные с пустым id получают `nc-{turn}-{i}` до normalize.
/// `calls`: нативные в порядке сообщения (принятые и отвергнутые), затем извлечённые из текста и рассуждения.
///
/// Повтор id нативного вызова в одном ответе тоже заменяется: иначе сопоставление
/// отказов по id и фиксация хода (`DuplicateCallId`) ломаются. Замещающие id
/// (`nc-{turn}-{k}`, `xc-{turn}-{k}`) уникальны в ответе: номер растёт от `i` (`n`),
/// пока id занят исходным id ответа или уже выданным.
/// Инструмент без класса в `classes` считается `NonIdempotent` (строже).
/// При `finish = Length` неразобранные аргументы не считаются в `parse_errors`:
/// их обрезал предел вывода, а не формат модели (MDL-018).
pub fn review(
    out: &TurnOutput,
    tools: &[ToolSchema],
    classes: &BTreeMap<String, EffectClass>,
    turn: u32,
) -> Reviewed {
    let mut msg = out.message.clone();
    // Исходные id ответа: замещающий id не должен совпасть ни с одним из них.
    let original: HashSet<String> = msg
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::ToolCall(c) if !c.id.is_empty() => Some(c.id.clone()),
            _ => None,
        })
        .collect();
    let mut used: HashSet<String> = HashSet::new();
    let mut i = 0usize;
    for p in &mut msg.parts {
        if let Part::ToolCall(c) = p {
            if c.id.is_empty() || used.contains(&c.id) {
                c.id = fresh_id("nc", turn, i, &original, &used);
            }
            used.insert(c.id.clone());
            i += 1;
        }
    }
    let length = out.finish == FinishReason::Length;

    let norm = normalize(&msg, tools, &[]);
    let accepted: HashMap<&str, &NormalizedCall> = norm
        .calls
        .iter()
        .filter(|c| c.source == CallSource::Native)
        .map(|c| (c.call.id.as_str(), c))
        .collect();
    let mut rejected = norm.rejected.iter();
    let mut calls = Vec::new();
    let mut nudges = Vec::new();
    let mut parse_errors = 0u32;
    let mut repairs = 0u32;

    // Нативные: по порядку сообщения; отвергнутым — первые отказы по порядку (D27).
    for p in &msg.parts {
        let Part::ToolCall(c) = p else { continue };
        if let Some(nc) = accepted.get(c.id.as_str()) {
            repairs += count(nc.repairs.len());
            calls.push(PlannedCall {
                call: nc.call.clone(),
                source: CallSource::Native,
                verdict: CallVerdict::Run,
            });
            continue;
        }
        let text = match rejected.next() {
            Some(r) => {
                if !length && matches!(r, CallRejection::Unparseable { .. }) {
                    parse_errors += 1;
                }
                rejection_text(r)
            }
            None => {
                tracing::warn!(id = %c.id, "нативный вызов не принят, но отказа для него нет");
                "not executed: the tool call was rejected".to_owned()
            }
        };
        calls.push(PlannedCall {
            call: c.clone(),
            source: CallSource::Native,
            verdict: CallVerdict::Reject(text),
        });
    }

    // Извлечённые из текста и рассуждения: id `xc-{turn}-{n}`, мутации не исполняются.
    let mut blocked = false;
    for (n, nc) in norm
        .calls
        .iter()
        .filter(|c| c.source != CallSource::Native)
        .enumerate()
    {
        repairs += count(nc.repairs.len());
        let mut call = nc.call.clone();
        call.id = fresh_id("xc", turn, n, &original, &used);
        used.insert(call.id.clone());
        let class = classes
            .get(&call.name)
            .copied()
            .unwrap_or(EffectClass::NonIdempotent);
        let verdict = if extracted_mutation_blocked(nc.source, class) {
            blocked = true;
            CallVerdict::Reject(BLOCKED.to_owned())
        } else {
            CallVerdict::Run
        };
        calls.push(PlannedCall {
            call,
            source: nc.source,
            verdict,
        });
    }
    if blocked {
        nudges.push(MUTATION_NUDGE.to_owned());
    }

    // Остальные отказы относятся к извлечённым (D27). При `Length` — один nudge про
    // обрезку вместо nudge о JSON для каждого вызова.
    let mut cut = false;
    for r in rejected {
        match r {
            CallRejection::Unparseable { .. } if length => cut = true,
            CallRejection::Unparseable { raw } => {
                parse_errors += 1;
                nudges.push(unparseable_nudge(raw));
            }
            other => tracing::debug!(rejection = ?other, "отклонён вызов, извлечённый из текста"),
        }
    }

    if cut {
        nudges.push(LENGTH_CUT_NUDGE.to_owned());
    }
    if length {
        for c in &mut calls {
            c.verdict = CallVerdict::Reject(LENGTH_CUT.to_owned());
        }
    }

    let assistant = assistant_message(&out.message, &norm.text, &norm.reasoning, &calls);
    Reviewed {
        assistant,
        calls,
        nudges,
        parse_errors,
        text: norm.text,
        repairs,
        rejected: count(norm.rejected.len()),
    }
}

/// Сообщение ассистента для истории: рассуждение, текст, вызовы.
///
/// Рассуждение с данными провайдера (`replay`: подпись thinking и т. п.) остаётся
/// как пришло: изменённый текст сделал бы подпись недействительной. Иначе —
/// одной частью без разметки вызовов.
fn assistant_message(
    original: &Message,
    text: &str,
    reasoning: &str,
    calls: &[PlannedCall],
) -> Message {
    let mut parts = Vec::new();
    let has_replay = original.parts.iter().any(|p| {
        matches!(
            p,
            Part::Reasoning {
                replay: Some(_),
                ..
            }
        )
    });
    if has_replay {
        parts.extend(
            original
                .parts
                .iter()
                .filter(|p| matches!(p, Part::Reasoning { .. }))
                .cloned(),
        );
    } else if !reasoning.is_empty() {
        parts.push(Part::Reasoning {
            text: reasoning.to_owned(),
            replay: None,
        });
    }
    if !text.is_empty() {
        parts.push(Part::Text(text.to_owned()));
    }
    parts.extend(calls.iter().map(|c| Part::ToolCall(c.call.clone())));
    Message {
        role: Role::Assistant,
        parts,
    }
}

/// Результат-ошибка отвергнутого нативного вызова (D23, английский). Текст модели
/// экранируется ([`escape_untrusted`]).
fn rejection_text(r: &CallRejection) -> String {
    match r {
        CallRejection::UnknownTool { name, nearest } => {
            // Порог подсказки — в нормализации (D28, единое правило `za-llm`).
            let near = if nearest.is_empty() {
                "none".to_owned()
            } else {
                escape_untrusted(&nearest.join(", "))
            };
            format!(
                "unknown tool \"{}\"; nearest: {}",
                escape_untrusted(name),
                near
            )
        }
        CallRejection::InvalidArgs { name, errors } => {
            let errs: Vec<String> = errors
                .iter()
                .map(|e| {
                    if e.path.is_empty() {
                        e.message.clone()
                    } else {
                        format!("{}: {}", e.path, e.message)
                    }
                })
                .collect();
            format!(
                "invalid arguments for \"{}\": {}",
                escape_untrusted(name),
                escape_untrusted(&errs.join("; "))
            )
        }
        CallRejection::Unparseable { raw } => {
            format!("could not parse tool call arguments: {}", shown(raw))
        }
        CallRejection::TooManyCalls { limit, skipped } => {
            format!("not executed: too many tool calls (limit {limit}, skipped {skipped})")
        }
    }
}

/// Nudge неразобранного вызова из текста (D27): начало разметки экранируется.
fn unparseable_nudge(raw: &str) -> String {
    format!(
        "Your tool call could not be parsed: {}. Call the tool again with valid JSON arguments.",
        shown(raw)
    )
}

/// Образцы текстов для модели, собираемых `format!`, с фиксированными аргументами
/// (PRM-010, M-1 ревью задачи 5): правка шаблона меняет образец и хэш промптов.
pub(crate) fn text_samples() -> Vec<(&'static str, String)> {
    let invalid = CallRejection::InvalidArgs {
        name: "t".into(),
        errors: vec![
            za_llm::normalize::SchemaError {
                path: String::new(),
                message: "m".into(),
            },
            za_llm::normalize::SchemaError {
                path: "/p".into(),
                message: "m".into(),
            },
        ],
    };
    vec![
        ("runtime/turn.invalid_args", rejection_text(&invalid)),
        (
            "runtime/turn.too_many_calls",
            rejection_text(&CallRejection::TooManyCalls {
                limit: 1,
                skipped: 2,
            }),
        ),
        (
            "runtime/turn.unknown_tool",
            rejection_text(&CallRejection::UnknownTool {
                name: "t".into(),
                nearest: vec!["a".into(), "b".into()],
            }),
        ),
        (
            "runtime/turn.unknown_tool_none",
            rejection_text(&CallRejection::UnknownTool {
                name: "t".into(),
                nearest: Vec::new(),
            }),
        ),
        (
            "runtime/turn.unparseable",
            rejection_text(&CallRejection::Unparseable { raw: "r".into() }),
        ),
        ("runtime/turn.unparseable_nudge", unparseable_nudge("r")),
    ]
}

/// Замещающий id `{prefix}-{turn}-{k}` с наименьшим `k ≥ from`, не занятый ни
/// исходными id ответа, ни уже выданными.
fn fresh_id(
    prefix: &str,
    turn: u32,
    from: usize,
    original: &HashSet<String>,
    used: &HashSet<String>,
) -> String {
    let mut k = from;
    loop {
        let id = format!("{prefix}-{turn}-{k}");
        if !original.contains(&id) && !used.contains(&id) {
            return id;
        }
        k += 1;
    }
}

/// Первые [`RAW_SHOWN`] символов разметки модели, экранированные.
fn shown(raw: &str) -> String {
    let head: String = raw.chars().take(RAW_SHOWN).collect();
    escape_untrusted(&head)
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// MDL-018, R1-A D43: при n > 0 один вызов model.record_parse_error(run, call).
pub fn report_parse_errors(model: &dyn ModelPort, run: RunId, call: &ModelCall, n: u32) {
    if n > 0 {
        model.record_parse_error(run, call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_buffer_flushes_by_size_and_time() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut b = DeltaBuffer::default();
        assert_eq!(b.push("a", ms(0)), None);
        assert_eq!(b.deadline(), Some(ms(200)));
        assert_eq!(b.push("a", ms(150)), None);
        assert_eq!(b.take(), Some("aa".into())); // таймер в 200 мс
        assert_eq!(b.deadline(), None);
        assert_eq!(b.push("a", ms(300)), None);
        assert_eq!(b.push("a", ms(520)), Some("aa".into())); // таймер опоздал
        assert_eq!(b.push(&"x".repeat(2048), ms(600)), Some("x".repeat(2048)));
        assert_eq!(b.push("a", ms(610)), None);
        assert_eq!(b.take(), Some("a".into())); // перед Finished
        assert_eq!(b.take(), None);
    }

    #[test]
    fn output_tps_guards() {
        assert_eq!(output_tps(100, Some(500), 1500), Some(100.0));
        assert_eq!(output_tps(100, None, 1500), None);
        assert_eq!(output_tps(100, Some(1500), 1500), None);
    }

    #[test]
    fn fresh_id_skips_taken() {
        let original = HashSet::from(["nc-3-1".to_owned(), "nc-3-2".to_owned()]);
        let used = HashSet::from(["nc-3-3".to_owned()]);
        assert_eq!(fresh_id("nc", 3, 1, &original, &used), "nc-3-4");
        assert_eq!(fresh_id("xc", 3, 0, &original, &used), "xc-3-0");
    }

    #[test]
    fn decode_failure_round_trips_signals_and_errors() {
        for s in [Signal::Pause, Signal::Cancel, Signal::Input] {
            let msg = format!("{INTERRUPTED}{}", s.as_str());
            assert!(matches!(
                decode_failure(msg, EffectSource::Journal),
                TurnError::Aborted { signal, source: EffectSource::Journal } if signal == s
            ));
        }
        let other = decode_failure("boom".into(), EffectSource::Live);
        assert!(matches!(other, TurnError::Effect(EffectError::Failed(m)) if m == "boom"));
        // Квитанция `store_error:` журналов R1-B (D72: новые не пишутся) — по-прежнему отказ.
        let store = decode_failure("store_error: x".into(), EffectSource::Journal);
        assert!(matches!(store, TurnError::Effect(EffectError::Failed(_))));
    }

    #[test]
    fn decode_failure_parses_degenerate_before_signals() {
        for (tag, d) in [
            ("repetition", Degenerate::Repetition),
            ("thinking", Degenerate::Thinking),
        ] {
            for source in [EffectSource::Live, EffectSource::Journal] {
                let e = decode_failure(format!("{INTERRUPTED}{tag}"), source);
                assert!(matches!(e, TurnError::Degenerate(x) if x == d), "{e:?}");
            }
        }
        let unknown = decode_failure(format!("{INTERRUPTED}other"), EffectSource::Journal);
        assert!(matches!(unknown, TurnError::Effect(EffectError::Failed(_))));
    }
}
