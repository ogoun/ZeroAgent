//! `Gateway` — единственный путь к модели для рабочего кода (RES-001, D38): собирает
//! зонды и профили, коннекторы, повторы, предохранитель, очередь с допуском по ресурсам
//! и слоты llama.cpp в одну реализацию [`ModelPort`].
//!
//! **Старт.** [`Gateway::start`] проверяет конфигурацию, берёт из [`ProfileStore`]
//! последние факты эндпоинтов (для показа и мест D8), настраивает группы очереди и
//! запускает зонды в фоне — не ждёт их. Пока идёт первый зонд, эндпоинт в состоянии
//! `probing`; у каждого зонда общий предел [`GatewayOptions::probe_deadline`], по его
//! истечении эндпоинт `Unreachable`. Дождаться зондов — [`Gateway::wait_probed`].
//!
//! **Зонды** одного эндпоинта идут по одному: запрошенный во время идущего зонда
//! «свежий» зонд (ручной [`Gateway::probe`], повторный зонд D40) выполняется сразу
//! после него, ожидающие заявки присоединяются к идущему. У каждого зонда поколение;
//! результат применяется, только если его поколение новее применённого.
//!
//! **Путь запроса** ([`ModelPort::stream`]):
//! 1. До стрима: эндпоинт `Inactive` (по завершённому зонду) — ошибка `Inactive` без
//!    сети; идентичный запрос после ошибки разбора — `RepeatedAfterParseError` (D21,
//!    D43); у готового эндпоинта пауза открытого предохранителя не истекла —
//!    `CircuitOpen` с `retry_after`, эндпоинт выключен — `Auth` (D22; разрешение здесь
//!    не берётся); заявка встаёт в очередь группы с пределом `RequestMeta::max_wait`
//!    (D12, M1; `None` — без предела).
//! 2. Стрим возвращается сразу; его guard — [`AbortOnDrop`] фоновой задачи (D2). Первое
//!    событие — `Queued` (заявке, снятой очередью уже при постановке, — только ошибка).
//!    Пока эндпоинт не готов, его заявки стоят в очереди приостановленными ([`ModelQueue::hold`] по id эндпоинта): с позицией и старением
//!    D9, но без места (место общей группы не простаивает). Успешный зонд снимает
//!    приостановку синхронно, в шаге установки `Ready`: места раздаются одним
//!    пересчётом в порядке D9. Эндпоинт `Unreachable` — повторный зонд (D40): один на
//!    всех ждущих (зонд новее постановки заявки уже дал ответ — без нового), не чаще
//!    `reprobe_min` от конца прошлого такого зонда с удвоением до `reprobe_max`; не
//!    ответил — `ErrorClass::Unreachable` с `retry_after` до следующего разрешённого
//!    зонда: запрос не отправлялся, предохранителю не считается.
//!    Затем — каждое изменение статуса заявки, пока она ждёт места (D31). Отказ
//!    очереди: `cancelled` → `Cancelled`, прочие — `Queue` с кодом [`QueueError::code`].
//! 3. Место выдано: если эндпоинт за время ожидания стал недоступен — место отпускается,
//!    дальше — как в п. 2; повторная постановка получает остаток `max_wait` от первой.
//!    Иначе в момент выдачи берётся разрешение предохранителя — на обоих путях (готовый
//!    эндпоинт и ожидание зонда), до выдачи заявка его не держит (M6). Переход в
//!    `HalfOpen` запускает повторный зонд; отказ — место отпускается, заявка
//!    кончается `CircuitOpen` (или `Auth`).
//!    У llama.cpp слот (`SlotManager`, ключ `"{bundle_key}:{session}"`), порядок `save`
//!    вытесненной сессии → `restore` → запрос (D29); каждое действие со слотом — до
//!    30 с, вместе до 60 с, место очереди при этом занято, событий потребителю нет.
//!    Затем `Started { attempt: 1 }` и [`drive_with_retry`] (D19, D20) с джиттером от
//!    [`request_fingerprint`] (D44). Каждый 429 ставит cooldown группы до
//!    `now + Retry-After` (без заголовка — на базовую паузу повтора); успешный ответ с
//!    исчерпанным лимитом — cooldown до сброса (MDL-016, RES-004). В `Finished`
//!    дописывается стоимость (D16), она же учитывается в бюджете группы (D14).
//! 4. Итог сообщается предохранителю через его разрешение — для каждой пропущенной
//!    заявки, в том числе отменённой. `Malformed` запоминает отпечаток; итог `Transport`
//!    или `StreamCut` запускает повторный зонд; отказ провайдера по `tool_choice`
//!    снимает `tool_choice_required` в профиле (D15). Паника задачи запроса — `error!`
//!    в лог и `ErrorClass::Internal` потребителю; предохранителю не сообщается.
//!
//! Эндпоинт запроса не меняется ни при каком исходе: автоматического перехода на другую
//! модель, тем более в облако, нет (MDL-016, ТЗ D-24).
//!
//! **Освобождение** (Review Focus 5). Drop стрима отменяет задачу: заявка снимается или
//! освобождаются слот и место очереди (слот раньше места), канал попытки закрывается, и
//! насос коннектора закрывает HTTP-соединение (D2, D35). Соединение закрывается
//! асинхронно, чуть позже освобождения слота: следующий запрос на тот же слот
//! llama-server отложит до конца прежней задачи — задержка, не ошибка.
//! [`Gateway::cancel_run`] снимает ожидающие заявки run и отменяет уже идущие его запросы.
//!
//! **Профили и слоты** (D40). После зонда профиль связки и факты эндпоинта пишутся в
//! хранилище. `SlotManager` сбрасывается ([`SlotManager::reset`]) только при смене
//! связки, числа слотов или контекста сервера (признак перезапуска с другими
//! параметрами; времени старта `/props` не сообщает); иначе карта «сессия → слот» и
//! `slot_save` сохраняются. Сброс сохраняет занятость идущих аренд. Контекст и слоты
//! берутся из фактов эндпоинта; факты из хранилища сверяются с профилем по ключу связки.

use std::collections::{BTreeMap, HashMap};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use za_protocol::{RunId, RunRole};
use za_res::{Admission, ResourceSnapshot};

use crate::caps::{BackendKind, PoolKind, ProviderCapabilities, default_caps};
use crate::config::{
    ConfigError, ConfigWarning, EndpointConfig, ModelsConfig, Price, compute_cost,
};
use crate::error::{ErrorClass, ModelError};
use crate::http::{HttpTransport, ProviderLimits, Secret, SecretSource, Timeouts};
use crate::port::{AbortOnDrop, BoxFuture, ModelCall, ModelPort, ModelStream};
use crate::probe::{probe_endpoint, probe_transport};
use crate::profile::{EndpointFacts, EndpointState, ProbedEndpoint, ProfileStore};
use crate::queue::{
    DEFAULT_CLOUD_PARALLEL, GroupId, GroupLimits, ModelQueue, Permit as PlacePermit, QueueError,
    QueueSnapshot, Ticket,
};
use crate::resilience::{
    BreakerOptions, BreakerState, CircuitBreaker, ParseGuard, Permit as BreakerPermit,
    RetryDecision, RetryPolicy, drive_with_retry, request_fingerprint,
};
use crate::slots::{
    LlamaSlots, SlotLease, SlotManager, SlotPlan, restore_unsupported, save_unsupported,
};
use crate::types::{
    CostSource, EndpointId, QueueStatus, ReasoningMode, StreamEvent, ToolChoice, Usage, WaitState,
};
use crate::wire::{AnthropicWire, ChatDialect, ChatWire, Connector, ResponsesWire, WireCtx};

/// Число мест локального эндпоинта, кроме llama.cpp, без `max_parallel` (D8).
pub const DEFAULT_LOCAL_PARALLEL: u32 = 4;

/// Общий предел одного зонда по умолчанию (несколько запросов зонда вместе).
pub const DEFAULT_PROBE_DEADLINE: Duration = Duration::from_secs(30);

/// Первый интервал повторного зонда по заявкам (D40).
pub const DEFAULT_REPROBE_MIN: Duration = Duration::from_secs(5);
/// Потолок интервала повторного зонда по заявкам.
pub const DEFAULT_REPROBE_MAX: Duration = Duration::from_secs(60);

/// Ёмкость канала стрима для потребителя.
const STREAM_CHANNEL: usize = 64;

type Ev = Result<StreamEvent, ModelError>;

/// Выбор эндпоинта (MDL-021, D24): явный выбор run, затем выбор задания, затем роль,
/// затем `models.default`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Selection {
    pub run_choice: Option<EndpointId>,
    pub job_choice: Option<EndpointId>,
    pub role: Option<RunRole>,
}

/// Отказ выбора эндпоинта. Другой эндпоинт вместо выбранного не подставляется (D24).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SelectError {
    #[error("модель не выбрана, а models.default не задан")]
    NoDefault,
    #[error("эндпоинт {0} не описан в models.endpoints")]
    Unknown(EndpointId),
    #[error("эндпоинт {id} не активен: {reason}")]
    Inactive { id: EndpointId, reason: String },
}

/// Параметры `Gateway` (D41).
#[derive(Debug, Clone)]
pub struct GatewayOptions {
    pub retry: RetryPolicy,
    pub timeouts_local: Timeouts,
    pub timeouts_cloud: Timeouts,
    pub breaker: BreakerOptions,
    /// Общий предел одного зонда эндпоинта; истёк — эндпоинт `Unreachable`.
    pub probe_deadline: Duration,
    /// Первый интервал между повторными зондами по заявкам к недоступному эндпоинту
    /// (D40); удваивается при каждом таком зонде до `reprobe_max`, сбрасывается при
    /// успешном зонде.
    pub reprobe_min: Duration,
    pub reprobe_max: Duration,
}

impl Default for GatewayOptions {
    /// D19 (повторы), D26 (таймауты), D22 (предохранитель), зонд — 30 с, повторный
    /// зонд по заявкам — не чаще 5 с, до 60 с.
    fn default() -> Self {
        Self {
            retry: RetryPolicy::default(),
            timeouts_local: Timeouts::local(),
            timeouts_cloud: Timeouts::cloud(),
            breaker: BreakerOptions::default(),
            probe_deadline: DEFAULT_PROBE_DEADLINE,
            reprobe_min: DEFAULT_REPROBE_MIN,
            reprobe_max: DEFAULT_REPROBE_MAX,
        }
    }
}

/// Эндпоинт для `models`, `doctor` и UI-046.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EndpointView {
    pub id: EndpointId,
    pub backend: BackendKind,
    pub pool: PoolKind,
    /// `probing` (первый зонд ещё идёт), `ready`, `inactive`, `unreachable`, `open`
    /// (предохранитель открыт или идёт проба), `disabled` (ключ отвергнут, до
    /// [`Gateway::reset`]).
    pub state: String,
    pub reason: Option<String>,
    /// Контекст из фактов эндпоинта (последний известный, если эндпоинт не готов).
    pub context_tokens: Option<u32>,
    pub slots: Option<u32>,
    pub model: Option<String>,
}

/// Ресурсы, очередь и модели (UI-046, D32).
#[derive(Debug, Clone, Serialize)]
pub struct ResourcesView {
    /// Последний снимок монитора; `None` — `Gateway` без допуска по ресурсам.
    pub resources: Option<ResourceSnapshot>,
    pub queue: QueueSnapshot,
    pub endpoints: Vec<EndpointView>,
    /// Предупреждения конфигурации текстом (`ConfigWarning`).
    pub warnings: Vec<String>,
}

/// Предел запроса `POST /tokenize` (R1-B2 D41): счётчик контекста не ждёт дольше и
/// берёт оценку по байтам.
pub const TOKENIZE_TIMEOUT: Duration = Duration::from_secs(5);

/// Шлюз к моделям. Clone дешёвый: состояние общее.
#[derive(Clone)]
pub struct Gateway {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field(
                "endpoints",
                &self.inner.endpoints.keys().collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// Эндпоинт с его состоянием.
struct Endpoint {
    cfg: EndpointConfig,
    group: GroupId,
    /// Запросы модели и `/slots`: таймауты по пулу (D26); без прокси для адресов этого
    /// хоста и локальной сети, как у зонда.
    http: HttpTransport,
    probe_http: HttpTransport,
    breaker: Mutex<CircuitBreaker>,
    live: Mutex<Live>,
    probe: Mutex<ProbeCtl>,
    /// Поколение последнего завершённого зонда (применённого или нет).
    probe_done: watch::Sender<u64>,
    /// Предупреждение «сохранение слотов не поддержано» уже записано (D29).
    slot_save_warned: AtomicBool,
}

/// Управление зондами эндпоинта: один за раз.
#[derive(Debug, Default)]
struct ProbeCtl {
    /// Идёт задача зондов.
    running: bool,
    /// После идущего зонда нужен ещё один (запрошен свежий).
    rerun: bool,
    /// Поколение последнего начатого (или назначенного) зонда.
    started: u64,
    /// Раньше этого момента повторный зонд по заявке не запускается (D40).
    next_demand: Option<Instant>,
    /// Поколение идущего зонда, запущенного по заявке.
    demand: Option<u64>,
    /// Текущий интервал повторного зонда по заявке; `ZERO` — ещё не было.
    backoff: Duration,
}

/// Изменяемое состояние эндпоинта.
struct Live {
    /// Итог последнего применённого зонда; до первого — `Unreachable` с `probed == false`.
    state: EndpointState,
    /// Первый зонд применён.
    probed: bool,
    /// Поколение применённого зонда.
    applied: u64,
    /// Последние известные профиль и факты: из зонда или из `ProfileStore` при старте.
    last: Option<ProbedEndpoint>,
    /// Слоты llama.cpp; сбрасывается при смене связки, числа слотов или контекста.
    slots: Option<Arc<Mutex<SlotManager>>>,
}

impl Live {
    fn ready(&self) -> Option<&ProbedEndpoint> {
        match &self.state {
            EndpointState::Ready(p) => Some(p),
            _ => None,
        }
    }

    fn known(&self) -> Option<&ProbedEndpoint> {
        self.ready().or(self.last.as_ref())
    }
}

/// Идущий запрос run: сигнал отмены и признак «запрос вне очереди» (место забрано
/// или ждёт зонда без заявки): такой запрос `cancel_run` считает сам.
struct CallHandle {
    cancel: watch::Sender<bool>,
    outside: Arc<AtomicBool>,
}

struct Inner {
    cfg: ModelsConfig,
    endpoints: BTreeMap<EndpointId, Endpoint>,
    secrets: Arc<dyn SecretSource>,
    admission: Option<Admission>,
    profiles: ProfileStore,
    opts: GatewayOptions,
    queue: ModelQueue,
    parse_guard: Mutex<ParseGuard>,
    calls: Mutex<HashMap<RunId, HashMap<u64, CallHandle>>>,
    next_call: AtomicU64,
    warnings: Vec<String>,
    rt: Handle,
}

/// Замок без паники: изменения под замками `Gateway` целостны, отравление снимается.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Gateway {
    /// Проверяет конфигурацию, загружает сохранённые факты эндпоинтов, настраивает
    /// группы очереди и запускает зонды всех эндпоинтов в фоне; зондов не ждёт.
    /// Облачный эндпоинт без ключа становится `Inactive` без сети (MDL-021).
    pub async fn start(
        cfg: ModelsConfig,
        secrets: Arc<dyn SecretSource>,
        admission: Option<Admission>,
        profiles: ProfileStore,
        opts: GatewayOptions,
    ) -> Result<Self, ConfigError> {
        let warnings: Vec<String> = cfg.validate()?.iter().map(warning_text).collect();
        for w in &warnings {
            tracing::warn!("{w}");
        }
        let rt = Handle::current();
        let queue = ModelQueue::new(rt.clone(), admission.clone());
        let endpoints = cfg
            .endpoints
            .iter()
            .map(|ep| (ep.id.clone(), Endpoint::new(ep, &opts)))
            .collect();
        let inner = Arc::new(Inner {
            cfg,
            endpoints,
            secrets,
            admission,
            profiles,
            opts,
            queue,
            parse_guard: Mutex::new(ParseGuard::new()),
            calls: Mutex::new(HashMap::new()),
            next_call: AtomicU64::new(0),
            warnings,
            rt,
        });
        inner.load_cached().await;
        for (group, limits) in inner.group_limits() {
            inner.queue.configure(group, limits);
        }
        for id in inner.endpoints.keys() {
            // До первого успешного зонда заявки эндпоинта стоят в очереди
            // приостановленными (I6): с порядком и старением, но без места.
            inner.queue.hold(&id.0);
            inner.request_probe(id, true);
        }
        Ok(Self { inner })
    }

    /// Ждёт завершения первого зонда всех эндпоинтов не дольше `limit`; `true` — все
    /// зонды завершены (с любым итогом). Для CLI, `doctor` и тестов.
    pub async fn wait_probed(&self, limit: Duration) -> bool {
        let until = Instant::now() + limit;
        let mut all = true;
        for ep in self.inner.endpoints.values() {
            let mut rx = ep.probe_done.subscribe();
            let done = tokio::time::timeout_at(until, rx.wait_for(|g| *g >= 1)).await;
            all &= matches!(done, Ok(Ok(_)));
        }
        all
    }

    /// Эндпоинт запроса по порядку D24: выбор run, выбор задания, роль, `models.default`.
    /// Выбранный, но выключенный эндпоинт — `Inactive`: замена не подставляется.
    pub fn select(&self, s: &Selection) -> Result<EndpointId, SelectError> {
        let cfg = &self.inner.cfg;
        let id = s
            .run_choice
            .clone()
            .or_else(|| s.job_choice.clone())
            .or_else(|| s.role.and_then(|r| cfg.role(r).cloned()))
            .or_else(|| cfg.default.clone())
            .ok_or(SelectError::NoDefault)?;
        let Some(ep) = self.inner.endpoints.get(&id) else {
            return Err(SelectError::Unknown(id));
        };
        if let BreakerState::Disabled { reason } = lock(&ep.breaker).state() {
            return Err(SelectError::Inactive { id, reason });
        }
        if let EndpointState::Inactive { reason } = &lock(&ep.live).state {
            let reason = reason.clone();
            return Err(SelectError::Inactive { id, reason });
        }
        Ok(id)
    }

    /// Зонд эндпоинта по требованию (MDL-003 «при обновлении»): начатый после вызова
    /// (если идёт другой, этот — сразу после него); итог — состояние после зонда.
    /// Неизвестный id — `Inactive`.
    pub async fn probe(&self, id: &EndpointId) -> EndpointState {
        let Some(ep) = self.inner.endpoints.get(id) else {
            return EndpointState::Inactive {
                reason: format!("эндпоинт {id} не описан в models.endpoints"),
            };
        };
        let target = self.inner.request_probe(id, true);
        wait_probe(ep, target).await;
        lock(&ep.live).state.clone()
    }

    /// Выбор пользователя «повторить» (D22): предохранитель закрыт, счёт с начала.
    /// Неготовый эндпоинт (нет ключа, не отвечает) зондируется заново в фоне.
    pub fn reset(&self, id: &EndpointId) {
        let Some(ep) = self.inner.endpoints.get(id) else {
            return;
        };
        lock(&ep.breaker).reset();
        if lock(&ep.live).ready().is_none() {
            self.inner.request_probe(id, true);
        }
    }

    /// Отмена run (RES-003): ожидающие и выданные, но не забранные заявки снимаются,
    /// идущие запросы run прерываются (слот и место освобождаются); их стримы кончаются
    /// ошибкой `Cancelled`. Возвращает число снятых заявок и прерванных запросов.
    ///
    /// Запрос, который забирает место в этот же миг, прерывается, но может не попасть в
    /// счёт; запрос, уже завершившийся, но ещё отдающий итог, может в него попасть.
    pub fn cancel_run(&self, run: RunId) -> usize {
        let mut n = self.inner.queue.cancel_run(run);
        let calls = lock(&self.inner.calls);
        if let Some(m) = calls.get(&run) {
            for c in m.values() {
                if c.outside.load(Ordering::SeqCst) {
                    n += 1;
                }
                let _ = c.cancel.send(true);
            }
        }
        n
    }

    /// Run завершён: отпечатки запросов после ошибок разбора забываются
    /// (`ParseGuard::clear`). Вызывает цикл R1-B при переходе run в любое конечное
    /// состояние, иначе отпечатки копятся.
    pub fn finish_run(&self, run: RunId) {
        lock(&self.inner.parse_guard).clear(run);
    }

    /// Эндпоинты в порядке id.
    pub fn endpoints(&self) -> Vec<EndpointView> {
        self.inner.endpoints.values().map(Endpoint::view).collect()
    }

    /// Ресурсы, очередь, эндпоинты и предупреждения (UI-046).
    pub fn view(&self) -> ResourcesView {
        ResourcesView {
            resources: self
                .inner
                .admission
                .as_ref()
                .map(|a| a.changed().borrow().clone()),
            queue: self.inner.queue.snapshot(),
            endpoints: self.endpoints(),
            warnings: self.inner.warnings.clone(),
        }
    }
}

impl ModelPort for Gateway {
    fn stream(&self, call: ModelCall) -> BoxFuture<'_, Result<ModelStream, ModelError>> {
        let r = self.inner.open_call(call);
        Box::pin(async move { r })
    }

    /// Возможности готового эндпоинта; у неготового — последние известные или
    /// возможности бэкенда до зонда.
    fn capabilities(&self, endpoint: &EndpointId) -> Option<ProviderCapabilities> {
        let ep = self.inner.endpoints.get(endpoint)?;
        let live = lock(&ep.live);
        Some(
            live.known()
                .map_or_else(|| default_caps(ep.cfg.backend), ProbedEndpoint::caps),
        )
    }

    /// Контекст из фактов эндпоинта (D43); у неготового — последний известный или
    /// `context_tokens` конфигурации.
    fn context_tokens(&self, endpoint: &EndpointId) -> Option<u32> {
        let ep = self.inner.endpoints.get(endpoint)?;
        let live = lock(&ep.live);
        live.known()
            .map(|p| p.facts.context_tokens)
            .or(ep.cfg.context_tokens)
    }

    /// Ошибка разбора вызова инструмента в цикле (D43): идентичный запрос в этом run
    /// будет отвергнут без отправки.
    fn record_parse_error(&self, run: RunId, call: &ModelCall) {
        let fp = request_fingerprint(&call.endpoint, &call.request);
        lock(&self.inner.parse_guard).record(run, fp);
    }

    /// [`Gateway::finish_run`]: отпечатки run после ошибок разбора забываются.
    fn finish_run(&self, run: RunId) {
        Gateway::finish_run(self, run);
    }

    /// R1-B2 D41: у llama.cpp — `POST {base}/tokenize {"content": text}`, число
    /// элементов `tokens` ответа; предел [`TOKENIZE_TIMEOUT`]. У прочих бэкендов, при
    /// ошибке, 404 или ответе без `tokens` — `None` (`debug`).
    fn count_tokens<'a>(
        &'a self,
        endpoint: &'a EndpointId,
        text: &'a str,
    ) -> BoxFuture<'a, Option<u32>> {
        Box::pin(async move {
            let ep = self.inner.endpoints.get(endpoint)?;
            if !ep.is_llama() {
                return None;
            }
            self.inner.tokenize(ep, text).await
        })
    }
}

impl Endpoint {
    fn new(ep: &EndpointConfig, opts: &GatewayOptions) -> Self {
        let t = match ep.pool() {
            PoolKind::Local => opts.timeouts_local,
            PoolKind::Cloud => opts.timeouts_cloud,
        };
        let http = if ep.host_is_local() {
            HttpTransport::local(t)
        } else {
            HttpTransport::cloud(t)
        };
        Self {
            cfg: ep.clone(),
            group: GroupId(ep.group_key()),
            http,
            probe_http: probe_transport(ep),
            breaker: Mutex::new(CircuitBreaker::new(ep.id.clone(), opts.breaker.clone())),
            live: Mutex::new(Live {
                state: EndpointState::Unreachable {
                    reason: "зонд ещё не завершён".to_owned(),
                },
                probed: false,
                applied: 0,
                last: None,
                slots: None,
            }),
            probe: Mutex::new(ProbeCtl::default()),
            probe_done: watch::channel(0).0,
            slot_save_warned: AtomicBool::new(false),
        }
    }

    /// Эндпоинт готов к запросу (по применённому зонду).
    fn ready_now(&self) -> bool {
        let live = lock(&self.live);
        live.probed && live.ready().is_some()
    }

    fn is_llama(&self) -> bool {
        self.cfg.backend == BackendKind::LlamaCpp
    }

    /// Мест у эндпоинта (D8): llama.cpp — `min(total_slots, max_parallel)`, прочие
    /// локальные — `max_parallel` или 4, облачные — `max_parallel` или
    /// [`DEFAULT_CLOUD_PARALLEL`].
    fn places(&self) -> u32 {
        let n = if self.is_llama() {
            let slots = lock(&self.live).known().and_then(|p| p.facts.slots);
            match (slots, self.cfg.max_parallel) {
                (Some(s), Some(m)) => s.min(m),
                (Some(n), None) | (None, Some(n)) => n,
                (None, None) => 1,
            }
        } else {
            match self.cfg.pool() {
                PoolKind::Local => self.cfg.max_parallel.unwrap_or(DEFAULT_LOCAL_PARALLEL),
                PoolKind::Cloud => self.cfg.max_parallel.unwrap_or(DEFAULT_CLOUD_PARALLEL),
            }
        };
        n.max(1)
    }

    fn view(&self) -> EndpointView {
        let breaker = lock(&self.breaker).state();
        let live = lock(&self.live);
        let (state, reason) = match (&breaker, &live.state) {
            (BreakerState::Disabled { reason }, _) => ("disabled", Some(reason.clone())),
            _ if !live.probed => ("probing", None),
            (_, EndpointState::Inactive { reason }) => ("inactive", Some(reason.clone())),
            (_, EndpointState::Unreachable { reason }) => ("unreachable", Some(reason.clone())),
            (BreakerState::Open { .. } | BreakerState::HalfOpen, EndpointState::Ready(_)) => {
                ("open", None)
            }
            (BreakerState::Closed, EndpointState::Ready(_)) => ("ready", None),
        };
        let known = live.known();
        EndpointView {
            id: self.cfg.id.clone(),
            backend: self.cfg.backend,
            pool: self.cfg.pool(),
            state: state.to_owned(),
            reason,
            context_tokens: known
                .map(|p| p.facts.context_tokens)
                .or(self.cfg.context_tokens),
            slots: known.and_then(|p| p.facts.slots),
            model: known
                .map(|p| p.profile.bundle.model.clone())
                .or_else(|| self.cfg.model.clone()),
        }
    }
}

/// Ждёт завершения зонда поколения `target` (или новее).
async fn wait_probe(ep: &Endpoint, target: u64) {
    let mut rx = ep.probe_done.subscribe();
    // Отправитель живёт в `Endpoint`: ошибки (закрытого канала) не бывает.
    let _ = rx.wait_for(|g| *g >= target).await;
}

/// Неудача запроса; `sent` — ошибка уже ушла в канал потребителя.
/// `handled` — исход уже сообщён (до отправки ошибки потребителю).
struct Failure {
    err: ModelError,
    sent: bool,
    handled: bool,
}

impl Failure {
    fn unsent(err: ModelError) -> Self {
        Self {
            err,
            sent: false,
            handled: false,
        }
    }

    fn sent(err: ModelError) -> Self {
        Self {
            err,
            sent: true,
            handled: false,
        }
    }
}

/// Слот и место очереди запроса. Поля дропаются в порядке объявления: слот
/// освобождается раньше места, иначе очередь выдала бы место до освобождения слота
/// (договор `slots`).
struct Held {
    _lease: Option<SlotLease>,
    _permit: PlacePermit,
}

/// Снимает запрос из реестра идущих при завершении или отмене задачи.
struct Registration {
    inner: Arc<Inner>,
    run: RunId,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut calls = lock(&self.inner.calls);
        if let Some(m) = calls.get_mut(&self.run) {
            m.remove(&self.id);
            if m.is_empty() {
                calls.remove(&self.run);
            }
        }
    }
}

/// Задача зондов кончилась (в том числе отменена с остановкой runtime): признак
/// снимается, ожидающие будятся.
struct ProbeLoopGuard {
    inner: Arc<Inner>,
    id: EndpointId,
}

impl Drop for ProbeLoopGuard {
    fn drop(&mut self) {
        let Some(ep) = self.inner.endpoints.get(&self.id) else {
            return;
        };
        let started = {
            let mut c = lock(&ep.probe);
            if !c.running {
                return;
            }
            c.running = false;
            c.rerun = false;
            c.started
        };
        ep.probe_done.send_replace(started);
    }
}

/// Страж паники задачи запроса (M1): `error!` и `Internal` потребителю.
struct PanicGuard {
    tx: mpsc::Sender<Ev>,
    endpoint: EndpointId,
}

impl Drop for PanicGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            tracing::error!(endpoint = %self.endpoint, "паника задачи запроса к модели");
            let _ = self.tx.try_send(Err(ModelError::new(
                ErrorClass::Internal,
                "внутренний сбой Gateway: задача запроса завершилась паникой",
            )));
        }
    }
}

impl Inner {
    /// Пределы групп очереди: места — минимум по эндпоинтам группы (D8), допуск по
    /// ресурсам — эндпоинт на этом хосте (D25), бюджет — минимум группы (D34).
    fn group_limits(&self) -> BTreeMap<GroupId, GroupLimits> {
        let mut out: BTreeMap<GroupId, GroupLimits> = BTreeMap::new();
        for ep in self.endpoints.values() {
            let n = ep.places();
            match out.get_mut(&ep.group) {
                Some(l) => {
                    l.max_parallel = l.max_parallel.min(n);
                    l.resource_gated |= ep.cfg.on_host();
                }
                None => {
                    out.insert(
                        ep.group.clone(),
                        GroupLimits {
                            kind: ep.cfg.pool(),
                            max_parallel: n,
                            resource_gated: ep.cfg.on_host(),
                            daily_budget_usd: self.cfg.group_budget(&ep.group.0),
                        },
                    );
                }
            }
        }
        out
    }

    /// Места группы по текущим фактам её эндпоинтов.
    fn group_parallel(&self, group: &GroupId) -> u32 {
        self.endpoints
            .values()
            .filter(|e| &e.group == group)
            .map(Endpoint::places)
            .min()
            .unwrap_or(1)
    }

    /// Просит зонд эндпоинта; возвращает поколение, которого надо дождаться.
    /// Нет идущего — запускается новый. Идёт — `fresh` назначает ещё один сразу после
    /// него (начатый после этой просьбы), иначе просьба присоединяется к идущему.
    fn request_probe(self: &Arc<Self>, id: &EndpointId, fresh: bool) -> u64 {
        let Some(ep) = self.endpoints.get(id) else {
            return 0;
        };
        let mut c = lock(&ep.probe);
        if !c.running {
            c.running = true;
            c.started += 1;
            let target = c.started;
            drop(c);
            self.spawn_probe_loop(id);
            return target;
        }
        if fresh {
            c.rerun = true;
            c.started + 1
        } else {
            c.started
        }
    }

    /// Повторный зонд по заявке к недоступному эндпоинту (D40) с ограничением частоты.
    /// Идёт зонд — присоединиться (`Ok(поколение)`). Интервал с прошлого такого зонда не
    /// истёк — `Err(остаток)`. Иначе — новый зонд; интервал удваивается от
    /// `reprobe_min` до `reprobe_max` и сбрасывается успешным зондом.
    fn demand_probe(self: &Arc<Self>, ep: &Endpoint) -> Result<u64, Duration> {
        let now = Instant::now();
        let mut c = lock(&ep.probe);
        if c.running {
            return Ok(c.started);
        }
        if let Some(t) = c.next_demand
            && t > now
        {
            return Err(t - now);
        }
        c.backoff = if c.backoff.is_zero() {
            self.opts.reprobe_min
        } else {
            c.backoff.saturating_mul(2).min(self.opts.reprobe_max)
        };
        c.running = true;
        c.started += 1;
        let target = c.started;
        // Интервал отсчитывается от конца этого зонда (задача зондов).
        c.demand = Some(target);
        drop(c);
        self.spawn_probe_loop(&ep.cfg.id);
        Ok(target)
    }

    /// Пауза до следующего разрешённого повторного зонда по заявке.
    fn reprobe_after(&self, ep: &Endpoint) -> Duration {
        lock(&ep.probe).next_demand.map_or(Duration::ZERO, |t| {
            t.saturating_duration_since(Instant::now())
        })
    }

    /// Задача зондов эндпоинта: зонд, затем назначенный следующий, пока назначают.
    fn spawn_probe_loop(self: &Arc<Self>, id: &EndpointId) {
        let guard = ProbeLoopGuard {
            inner: self.clone(),
            id: id.clone(),
        };
        self.rt.spawn(async move {
            let inner = &guard.inner;
            let Some(ep) = inner.endpoints.get(&guard.id) else {
                return;
            };
            loop {
                let generation = lock(&ep.probe).started;
                let state = inner.run_probe(ep).await;
                let ready = matches!(state, EndpointState::Ready(_));
                inner.apply_probe(ep, generation, state).await;
                {
                    // Зонд по заявке кончился неудачей: следующий — не раньше интервала от
                    // конца этого (до пробуждения ждущих: их `retry_after` уже верен).
                    let mut c = lock(&ep.probe);
                    if c.demand == Some(generation) {
                        c.demand = None;
                        if !ready {
                            c.next_demand = Instant::now().checked_add(c.backoff);
                        }
                    }
                }
                ep.probe_done.send_replace(generation);
                let mut c = lock(&ep.probe);
                if c.rerun {
                    c.rerun = false;
                    c.started += 1;
                    continue;
                }
                c.running = false;
                break;
            }
            drop(guard);
        });
    }

    /// Один зонд с общим пределом.
    async fn run_probe(&self, ep: &Endpoint) -> EndpointState {
        let limit = self.opts.probe_deadline;
        let probe = probe_endpoint(&ep.cfg, &ep.probe_http, &*self.secrets);
        match tokio::time::timeout(limit, probe).await {
            Ok(s) => s,
            Err(_) => EndpointState::Unreachable {
                reason: format!("зонд не завершён за {} мс", limit.as_millis()),
            },
        }
    }

    /// Применяет итог зонда поколения `generation`, если оно новее применённого:
    /// профиль и факты в хранилище, слоты (D29), места группы (D8, D40).
    async fn apply_probe(&self, ep: &Endpoint, generation: u64, mut state: EndpointState) {
        let (changed_bundle, saved) = {
            let mut live = lock(&ep.live);
            if generation <= live.applied {
                tracing::debug!(endpoint = %ep.cfg.id, generation, applied = live.applied,
                                "итог устаревшего зонда не применяется");
                return;
            }
            live.applied = generation;
            live.probed = true;
            let mut changed_bundle = None;
            let mut saved = None;
            if let EndpointState::Ready(p) = &mut state {
                let old = live.known().map(|o| o.facts.clone());
                let same = old.as_ref().is_some_and(|o| same_server(o, &p.facts));
                if same
                    && p.facts.slot_save.is_none()
                    && let Some(o) = &old
                {
                    p.facts.slot_save = o.slot_save;
                }
                if let Some(o) = &old
                    && o.bundle_key != p.facts.bundle_key
                {
                    changed_bundle = Some((o.bundle_key.clone(), p.facts.bundle_key.clone()));
                }
                if ep.is_llama() {
                    let total = p.facts.slots.unwrap_or(1);
                    match (&live.slots, same) {
                        // Тот же сервер: карта «сессия → слот» сохраняется.
                        (Some(_), true) => {}
                        (Some(m), false) => {
                            tracing::info!(endpoint = %ep.cfg.id,
                                "связка, число слотов или контекст сервера изменились: слоты сброшены");
                            lock(m).reset(total);
                        }
                        (None, _) => {
                            live.slots = Some(Arc::new(Mutex::new(SlotManager::new(total))));
                        }
                    }
                }
                live.last = Some(p.clone());
                saved = Some(p.clone());
            }
            live.state = state;
            (changed_bundle, saved)
        };
        if let Some((old, new)) = changed_bundle {
            tracing::info!(endpoint = %ep.cfg.id, old = %old, new = %new,
                           "на эндпоинте другая связка модели: профиль заменён");
        }
        // Места группы, затем приостановка — синхронно, до любого `await` (I6): выдача
        // ждавшим идёт одним пересчётом очереди в порядке D9, и заявка, поставленная
        // после `Ready`, их не опережает.
        if ep.is_llama() {
            self.queue
                .set_parallel(&ep.group, self.group_parallel(&ep.group));
        }
        if saved.is_some() {
            // Успешный зонд сбрасывает интервал повторных зондов по заявкам (D40).
            {
                let mut c = lock(&ep.probe);
                c.backoff = Duration::ZERO;
                c.next_demand = None;
            }
            self.queue.release(&ep.cfg.id.0);
        } else {
            self.queue.hold(&ep.cfg.id.0);
        }
        if let Some(p) = saved
            && let Err(e) = self.profiles.save_probed_async(p).await
        {
            tracing::warn!(endpoint = %ep.cfg.id, error = %e, "профиль эндпоинта не сохранён");
        }
    }

    /// Факты и профили из хранилища (до первого зонда: показ и места D8). Профиль
    /// берётся по `bundle_key` фактов, ключ связки профиля сверяется с ним.
    async fn load_cached(&self) {
        let ids: Vec<EndpointId> = self.endpoints.keys().cloned().collect();
        let store = self.profiles.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            ids.into_iter()
                .filter_map(|id| load_probed(&store, &id).map(|p| (id, p)))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        for (id, p) in loaded {
            if let Some(ep) = self.endpoints.get(&id) {
                let mut live = lock(&ep.live);
                if live.known().is_none() {
                    live.last = Some(p);
                }
            }
        }
    }

    /// Проверки до стрима и постановка в очередь; затем фоновая задача запроса.
    fn open_call(self: &Arc<Self>, call: ModelCall) -> Result<ModelStream, ModelError> {
        let Some(ep) = self.endpoints.get(&call.endpoint) else {
            return Err(ModelError::new(
                ErrorClass::NotFound,
                format!("эндпоинт {} не описан в models.endpoints", call.endpoint),
            ));
        };
        let (ready, seen) = {
            let live = lock(&ep.live);
            if live.probed
                && let EndpointState::Inactive { reason } = &live.state
            {
                return Err(inactive(&ep.cfg.id, reason));
            }
            (live.probed && live.ready().is_some(), live.applied)
        };
        let run = call.meta.run_id;
        let fp = request_fingerprint(&call.endpoint, &call.request);
        lock(&self.parse_guard).check(run, &fp)?;
        // Заявка встаёт в очередь сразу, с меткой эндпоинта: пока он не готов, она
        // приостановлена (I6) — сохраняет позицию и старение, но места не занимает.
        // Разрешение предохранителя берётся при выдаче места (M6); здесь — только отказ,
        // который ожидание не изменит: пауза открытого предохранителя или выключенный
        // эндпоинт.
        if ready && let Some(err) = lock(&ep.breaker).refusal(Instant::now()) {
            return Err(err);
        }
        let tag = Some(ep.cfg.id.0.clone());
        let enqueued = Instant::now();
        let ticket = self
            .queue
            .enqueue_tagged(&ep.group, run, call.meta.priority, call.meta.max_wait, tag)
            .map_err(|e| queue_error(&e))?;
        let id = self.next_call.fetch_add(1, Ordering::Relaxed);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let outside = Arc::new(AtomicBool::new(false));
        lock(&self.calls).entry(run).or_default().insert(
            id,
            CallHandle {
                cancel: cancel_tx,
                outside: outside.clone(),
            },
        );
        let reg = Registration {
            inner: self.clone(),
            run,
            id,
        };
        let (tx, rx) = mpsc::channel(STREAM_CHANNEL);
        let task = CallTask {
            inner: self.clone(),
            call,
            fp,
            seen,
            enqueued,
            tx,
            outside,
            _reg: reg,
        };
        let handle = self.rt.spawn(task.run(ticket, cancel_rx));
        Ok(ModelStream::from_channel(rx, Box::new(AbortOnDrop(handle))))
    }

    /// Разрешение предохранителя; проба `HalfOpen` запускает повторный зонд (D40).
    fn allow(self: &Arc<Self>, ep: &Endpoint) -> Result<BreakerPermit, ModelError> {
        let permit = lock(&ep.breaker).allow(Instant::now())?;
        if permit.is_probe() {
            self.request_probe(&ep.cfg.id, true);
        }
        Ok(permit)
    }

    /// Эндпоинт готов к запросу заявки, поставленной при применённом зонде `seen`.
    /// - первый зонд ещё идёт — ждать его;
    /// - зонд новее `seen` сказал «недоступен» — сразу `Unreachable`: один повторный
    ///   зонд отвечает всем ждущим;
    /// - иначе повторный зонд по заявке (D40): идёт — присоединиться; интервал не
    ///   истёк — `Unreachable` без зонда.
    ///
    /// `Unreachable` несёт `retry_after` — паузу до следующего разрешённого повторного
    /// зонда. Запрос модели при отказе не отправлялся.
    async fn ensure_ready(self: &Arc<Self>, ep: &Endpoint, seen: u64) -> Result<(), ModelError> {
        loop {
            let (probed, applied, state) = {
                let live = lock(&ep.live);
                (live.probed, live.applied, live.state.clone())
            };
            let reason = match state {
                EndpointState::Ready(_) if probed => return Ok(()),
                EndpointState::Inactive { reason } if probed => {
                    return Err(inactive(&ep.cfg.id, &reason));
                }
                EndpointState::Unreachable { reason } => reason,
                // Не бывает: до первого зонда состояние — `Unreachable`.
                _ => String::new(),
            };
            if !probed {
                let target = self.request_probe(&ep.cfg.id, false);
                wait_probe(ep, target).await;
                if !lock(&ep.live).probed {
                    // Задача зонда отменена (runtime останавливается).
                    return Err(self.unreachable(ep, &reason));
                }
                continue;
            }
            if applied > seen {
                return Err(self.unreachable(ep, &reason));
            }
            match self.demand_probe(ep) {
                Ok(target) => {
                    wait_probe(ep, target).await;
                    if lock(&ep.live).applied <= seen {
                        return Err(self.unreachable(ep, &reason));
                    }
                }
                Err(_) => return Err(self.unreachable(ep, &reason)),
            }
        }
    }

    /// `Unreachable` с паузой до следующего разрешённого повторного зонда.
    fn unreachable(&self, ep: &Endpoint, reason: &str) -> ModelError {
        ModelError {
            retry_after: Some(self.reprobe_after(ep)),
            ..ModelError::new(
                ErrorClass::Unreachable,
                format!("эндпоинт {} недоступен: {reason}", ep.cfg.id),
            )
        }
    }

    /// `POST {base}/tokenize` llama-server (R1-B2 D41) с ключом запроса модели.
    async fn tokenize(&self, ep: &Endpoint, text: &str) -> Option<u32> {
        let secret = match self.secret(ep) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(endpoint = %ep.cfg.id, class = ?e.class, "счёт токенов: нет ключа");
                return None;
            }
        };
        let url = format!(
            "{}/tokenize",
            ep.cfg.effective_base_url().trim_end_matches('/')
        );
        let mut headers = crate::http::HeaderMap::new();
        if let Some(s) = &secret {
            match s.header_value("Bearer ") {
                Ok(v) => {
                    headers.insert(reqwest::header::AUTHORIZATION, v);
                }
                Err(e) => {
                    tracing::debug!(endpoint = %ep.cfg.id, class = ?e.class, "счёт токенов: ключ");
                    return None;
                }
            }
        }
        let redact: Vec<&Secret> = secret.iter().collect();
        let body = serde_json::json!({ "content": text });
        let fut = ep.http.post_json(&url, headers, &body, &redact);
        match tokio::time::timeout(TOKENIZE_TIMEOUT, fut).await {
            Err(_) => {
                tracing::debug!(endpoint = %ep.cfg.id, "счёт токенов: тайм-аут /tokenize");
                None
            }
            Ok(Err(e)) => {
                tracing::debug!(endpoint = %ep.cfg.id, class = ?e.class, "счёт токенов: /tokenize не ответил");
                None
            }
            Ok(Ok((v, _))) => {
                let n = v
                    .get("tokens")
                    .and_then(serde_json::Value::as_array)
                    .map(Vec::len);
                if n.is_none() {
                    tracing::debug!(endpoint = %ep.cfg.id, "счёт токенов: ответ /tokenize без tokens");
                }
                n.and_then(|n| u32::try_from(n).ok())
            }
        }
    }

    /// Ключ эндпоинта на момент запроса; `key_ref` без ключа — `Inactive`.
    fn secret(&self, ep: &Endpoint) -> Result<Option<Secret>, ModelError> {
        match &ep.cfg.key_ref {
            None => Ok(None),
            Some(k) => self
                .secrets
                .get(k)
                .map(Some)
                .ok_or_else(|| inactive(&ep.cfg.id, &crate::http::missing_key_reason(k))),
        }
    }

    /// `save` вытесненной сессии, затем `restore` своей (D29). Ошибки влияют только на
    /// скорость; «не поддержано» (нет `--slot-save-path` или 401/403 на `/slots`)
    /// выключает сохранение. Ключ — тот же, что у запроса модели.
    async fn prepare_slot(&self, ep: &Endpoint, plan: &SlotPlan, secret: Option<Secret>) {
        let disabled = lock(&ep.live)
            .ready()
            .is_some_and(|p| p.facts.slot_save == Some(false));
        if disabled || (plan.save_evicted.is_none() && plan.restore.is_none()) {
            return;
        }
        let slots =
            LlamaSlots::new(ep.cfg.effective_base_url(), ep.http.clone()).with_secret(secret);
        let mut unsupported = None;
        if let Some((slot, file)) = &plan.save_evicted {
            match slots.save(*slot, file).await {
                Ok(()) => {}
                Err(e) if save_unsupported(&e) => unsupported = Some(e.class),
                Err(e) => tracing::debug!(endpoint = %ep.cfg.id, slot, class = ?e.class,
                                          "слот не сохранён"),
            }
        }
        if unsupported.is_none()
            && let Some(file) = &plan.restore
        {
            match slots.restore(plan.slot, file).await {
                Ok(()) => tracing::debug!(endpoint = %ep.cfg.id, slot = plan.slot,
                                          "слот восстановлен"),
                Err(e) if restore_unsupported(&e) => unsupported = Some(e.class),
                // 400 — файла нет: новая сессия.
                Err(e) => tracing::debug!(endpoint = %ep.cfg.id, slot = plan.slot,
                                          class = ?e.class, "слот не восстановлен"),
            }
        }
        if let Some(class) = unsupported {
            self.disable_slot_save(ep, class == ErrorClass::Auth);
        }
    }

    /// `slot_save = Some(false)` в фактах эндпоинта, одно предупреждение (D29). `auth` —
    /// причина в отказе ключа на `/slots` (401/403), иначе — нет `--slot-save-path`.
    fn disable_slot_save(&self, ep: &Endpoint, auth: bool) {
        let facts = {
            let mut live = lock(&ep.live);
            let mut out = None;
            if let EndpointState::Ready(p) = &mut live.state
                && p.facts.slot_save != Some(false)
            {
                p.facts.slot_save = Some(false);
                out = Some(p.facts.clone());
            }
            if let (Some(f), Some(last)) = (&out, live.last.as_mut()) {
                last.facts = f.clone();
            }
            out
        };
        let Some(facts) = facts else { return };
        if !ep.slot_save_warned.swap(true, Ordering::AcqRel) {
            if auth {
                tracing::warn!(endpoint = %ep.cfg.id,
                    "llama-server отклонил ключ на /slots (401/403): работа без сохранения KV-кэша; проверьте key_ref эндпоинта и --api-key сервера");
            } else {
                tracing::warn!(endpoint = %ep.cfg.id,
                    "llama-server не сохраняет слоты (нет --slot-save-path): работа без сохранения KV-кэша");
            }
        }
        let store = self.profiles.clone();
        self.rt.spawn_blocking(move || {
            if let Err(e) = store.save_endpoint(&facts) {
                tracing::warn!(error = %e, "факты эндпоинта не сохранены");
            }
        });
    }

    /// Провайдер отверг принудительный `tool_choice` (D15): профиль больше его не
    /// предлагает, запрос с тем же значением не повторяется.
    fn disable_forced_tool_choice(&self, ep: &Endpoint) {
        let profile = {
            let mut live = lock(&ep.live);
            let mut out = None;
            if let EndpointState::Ready(p) = &mut live.state
                && p.profile.caps.tool_choice_required
            {
                p.profile.caps.tool_choice_required = false;
                out = Some(p.profile.clone());
            }
            if let (Some(pr), Some(last)) = (&out, live.last.as_mut()) {
                last.profile = pr.clone();
            }
            out
        };
        let Some(profile) = profile else { return };
        tracing::warn!(endpoint = %ep.cfg.id,
            "провайдер отверг принудительный tool_choice: профиль получает tool_choice_required = false");
        let store = self.profiles.clone();
        self.rt.spawn_blocking(move || {
            if let Err(e) = store.save(&profile) {
                tracing::warn!(error = %e, "профиль модели не сохранён");
            }
        });
    }
}

/// Тот же сервер с теми же параметрами: связка, число слотов и контекст рантайма не
/// изменились. Иначе — смена модели или перезапуск с другими параметрами.
fn same_server(old: &EndpointFacts, new: &EndpointFacts) -> bool {
    old.bundle_key == new.bundle_key
        && old.slots == new.slots
        && old.runtime_context == new.runtime_context
}

/// Фоновая задача одного запроса.
struct CallTask {
    inner: Arc<Inner>,
    call: ModelCall,
    fp: String,
    tx: mpsc::Sender<Ev>,
    /// Применённый зонд на момент постановки заявки.
    seen: u64,
    /// Момент первой постановки: отсчёт `max_wait` при повторной постановке (M1).
    enqueued: Instant,
    outside: Arc<AtomicBool>,
    _reg: Registration,
}

impl CallTask {
    async fn run(self, ticket: Ticket, mut cancel: watch::Receiver<bool>) {
        // Разрешение предохранителя берётся при выдаче места и общее с задачей попытки:
        // успех сообщается, когда пришёл `Finished`, до передачи его потребителю.
        let verdict: Verdict = Arc::new(Mutex::new(None));
        let Some(ep) = self.inner.endpoints.get(&self.call.endpoint) else {
            return;
        };
        let _panic = PanicGuard {
            tx: self.tx.clone(),
            endpoint: ep.cfg.id.clone(),
        };
        // Отмена run бросает `serve` целиком: слот, место и попытка освобождаются до
        // отправки ошибки.
        let outcome = tokio::select! {
            biased;
            () = wait_cancel(&mut cancel) => Err(Failure::unsent(ModelError::new(
                ErrorClass::Cancelled,
                "run отменён",
            ))),
            r = self.serve(ep, ticket, &verdict) => r,
        };
        match outcome {
            Ok(()) => {
                let p = lock(&verdict).take();
                if let Some(p) = p {
                    lock(&ep.breaker).on_success(p);
                }
            }
            Err(f) => {
                if !f.handled {
                    self.report_failure(ep, &verdict, &f.err);
                }
                if !f.sent {
                    let _ = self.tx.send(Err(f.err.clone())).await;
                }
            }
        }
    }

    /// Исход-ошибка: предохранителю (если разрешение ещё не отчиталось) и реакции D21,
    /// D40, D15.
    fn report_failure(&self, ep: &Endpoint, verdict: &Verdict, err: &ModelError) {
        let p = lock(verdict).take();
        if let Some(p) = p {
            lock(&ep.breaker).on_failure(p, err, Instant::now());
        }
        self.after_failure(ep, err);
    }

    /// Зонд (если нужен), очередь, слот, повторы, стоимость.
    async fn serve(&self, ep: &Endpoint, ticket: Ticket, verdict: &Verdict) -> Result<(), Failure> {
        let mut ticket = Some(ticket);
        let mut announce = true;
        let place = loop {
            let t = match ticket.as_mut() {
                Some(t) => t,
                None => {
                    // Место было выдано, но эндпоинт успел стать неготовым: место
                    // отпущено, заявка — заново (редкий случай; счёт `cancel_run` — у
                    // очереди).
                    self.outside.store(false, Ordering::SeqCst);
                    announce = true;
                    let meta = &self.call.meta;
                    let tag = Some(ep.cfg.id.0.clone());
                    // Предел ожидания общий для всех постановок заявки (M1).
                    let left = remaining_wait(meta.max_wait, self.enqueued.elapsed());
                    let t = self
                        .inner
                        .queue
                        .enqueue_tagged(&ep.group, meta.run_id, meta.priority, left, tag)
                        .map_err(|e| Failure::unsent(queue_error(&e)))?;
                    ticket.insert(t)
                }
            };
            // Первое событие — `Queued` с настоящей позицией заявки. Заявка, снятая
            // очередью уже при постановке (`max_wait` ноль или исчерпан, бюджет), его не
            // получает: потребителю — только итоговая ошибка.
            let mut status = t.status();
            let st = status.borrow_and_update().clone();
            if announce {
                if !matches!(st.state, WaitState::Failed { .. }) {
                    send(&self.tx, Ok(StreamEvent::Queued(st))).await?;
                }
                announce = false;
            }
            if !ep.ready_now() {
                // Заявка приостановлена в очереди (I6): ждёт зонда, сохраняя позицию и
                // старение, место не занимает (I5). Очередь может снять её и во время
                // зонда (`max_wait`, бюджет) — тогда её отказ уходит сразу, не после зонда.
                tokio::select! {
                    biased;
                    () = queue_failed(t.status()) => {
                        return Err(Failure::unsent(match t.wait_granted().await {
                            Err(e) => queue_error(&e),
                            // Не бывает: снятая заявка места не получает.
                            Ok(_) => ModelError::new(
                                ErrorClass::Internal,
                                "очередь выдала место снятой заявке",
                            ),
                        }));
                    }
                    r = self.inner.ensure_ready(ep, self.seen) => r.map_err(Failure::unsent)?,
                }
            }
            match wait_place(t, status, &self.tx, ep).await? {
                Waited::NotReady => continue,
                Waited::Granted(place) => {
                    ticket = None;
                    self.outside.store(true, Ordering::SeqCst);
                    if ep.ready_now() {
                        // Разрешение предохранителя — в момент выдачи места (M6): до
                        // выдачи заявка ждёт без него и не держит пробу `HalfOpen`.
                        // Отказ — место отпускается сразу, заявка кончается отказом.
                        match self.inner.allow(ep) {
                            Ok(p) => *lock(verdict) = Some(p),
                            Err(e) => {
                                drop(place);
                                return Err(Failure::unsent(e));
                            }
                        }
                        break place;
                    }
                    // Место отпускается, дальше — зонд по правилам `ensure_ready`.
                }
            }
        };
        let (probed, slots) = {
            let live = lock(&ep.live);
            match live.ready() {
                Some(p) => (p.clone(), live.slots.clone()),
                None => {
                    return Err(Failure::unsent(ModelError::new(
                        ErrorClass::Unreachable,
                        format!("эндпоинт {} недоступен", ep.cfg.id),
                    )));
                }
            }
        };
        let secret = self.inner.secret(ep).map_err(Failure::unsent)?;
        let (lease, slot) = match slots {
            Some(manager) => {
                let key = self
                    .call
                    .meta
                    .session
                    .as_ref()
                    .map(|s| format!("{}:{s}", probed.facts.bundle_key));
                let (lease, plan) = SlotManager::lease(&manager, key.as_deref());
                self.inner.prepare_slot(ep, &plan, secret.clone()).await;
                (Some(lease), Some(plan.slot))
            }
            None => (None, None),
        };
        let _held = Held {
            _lease: lease,
            _permit: place,
        };
        send(
            &self.tx,
            Ok(StreamEvent::Started {
                endpoint: ep.cfg.id.clone(),
                slot,
                attempt: 1,
            }),
        )
        .await?;

        let connector = connector(&ep.cfg, &probed);
        let caps = probed.caps();
        let ctx = WireCtx {
            caps: &caps,
            slot,
            secret: secret.as_ref(),
            thinking: Some(&probed.profile.thinking),
            max_output_tokens: probed.profile.max_output_tokens,
        };
        let mut req = self.call.request.clone();
        if req.reasoning == ReasoningMode::Default
            && let Some(r) = &ep.cfg.reasoning
        {
            req.reasoning = r.clone();
        }
        let price = ep.cfg.price.or(probed.profile.price);
        let hook = FinishHook {
            inner: self.inner.clone(),
            endpoint: ep.cfg.id.clone(),
            verdict: verdict.clone(),
        };
        let (queue, group, http, rt) = (&self.inner.queue, &ep.group, &ep.http, &self.inner.rt);
        let (conn, req, hook) = (&connector, &req, &hook);
        let open = move |_attempt: u32| async move {
            let opened = conn.open(http, req, &ctx).await?;
            if let Some(until) = exhausted_until(&opened.limits) {
                tracing::debug!(group = %group, "лимит провайдера исчерпан: cooldown группы");
                queue.cooldown(group, until);
            }
            let (mtx, mrx) = mpsc::channel(STREAM_CHANNEL);
            rt.spawn(price_events(opened.events, mtx, price, hook.clone()));
            Ok(mrx)
        };
        let policy = &self.inner.opts.retry;
        let handled = AtomicBool::new(false);
        let on_attempt_error = |e: &ModelError, decision: RetryDecision| {
            if e.class == ErrorClass::RateLimited {
                // Без `Retry-After` — хотя бы базовая пауза повтора: следующая заявка
                // группы не бьёт сразу в тот же лимит.
                let pause = e.retry_after.unwrap_or(policy.base);
                if let Some(until) = Instant::now().checked_add(pause) {
                    queue.cooldown(group, until);
                }
            }
            // Итоговая ошибка (решение принял `drive_with_retry` и передал сюда до его
            // исполнения): исход сообщается до того, как ошибка уйдёт потребителю —
            // иначе его следующий запрос мог бы опередить предохранитель и `ParseGuard`.
            if decision == RetryDecision::GiveUp {
                self.report_failure(ep, verdict, e);
                handled.store(true, Ordering::SeqCst);
            }
        };
        drive_with_retry(policy, &self.fp, &self.tx, open, on_attempt_error)
            .await
            .map_err(|err| Failure {
                err,
                sent: true,
                handled: handled.load(Ordering::SeqCst),
            })?;
        Ok(())
    }

    /// Реакция на итоговую ошибку (D21, D40, D15).
    fn after_failure(&self, ep: &Endpoint, err: &ModelError) {
        match err.class {
            ErrorClass::Malformed => {
                lock(&self.inner.parse_guard).record(self.call.meta.run_id, self.fp.clone());
            }
            ErrorClass::Transport | ErrorClass::StreamCut => {
                self.inner.request_probe(&ep.cfg.id, true);
            }
            ErrorClass::BadRequest if tool_choice_rejected(err, &self.call.request.tool_choice) => {
                self.inner.disable_forced_tool_choice(ep);
            }
            _ => {}
        }
    }
}

/// Итог ожидания места.
enum Waited {
    Granted(PlacePermit),
    /// Эндпоинт стал неготовым: заявка остаётся в очереди приостановленной.
    NotReady,
}

/// Ждёт места, пересылая каждое изменение статуса заявки, пока она ждёт (первый статус
/// уже отправлен). Эндпоинт стал неготовым (зонд) — `NotReady`, заявка не снимается.
async fn wait_place(
    ticket: &mut Ticket,
    mut status: watch::Receiver<QueueStatus>,
    tx: &mpsc::Sender<Ev>,
    ep: &Endpoint,
) -> Result<Waited, Failure> {
    let mut probes = ep.probe_done.subscribe();
    probes.borrow_and_update();
    if !ep.ready_now() {
        return Ok(Waited::NotReady);
    }
    let mut granted = pin!(ticket.wait_granted());
    let mut watching = true;
    loop {
        tokio::select! {
            biased;
            r = &mut granted => {
                return r
                    .map(Waited::Granted)
                    .map_err(|e| Failure::unsent(queue_error(&e)));
            }
            _ = probes.changed() => {
                if !ep.ready_now() {
                    return Ok(Waited::NotReady);
                }
            }
            ch = status.changed(), if watching => {
                if ch.is_err() {
                    watching = false;
                    continue;
                }
                let st = status.borrow_and_update().clone();
                if !matches!(st.state, WaitState::Granted | WaitState::Failed { .. }) {
                    send(tx, Ok(StreamEvent::Queued(st))).await?;
                }
            }
        }
    }
}

/// Очередь сняла заявку (`WaitState::Failed`: `max_wait`, бюджет, отмена). Закрытый
/// канал статуса — никогда. `Ref` статуса не держится через `await` (см.
/// [`Ticket::status`]).
async fn queue_failed(mut rx: watch::Receiver<QueueStatus>) {
    loop {
        let failed = matches!(rx.borrow_and_update().state, WaitState::Failed { .. });
        if failed {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Отправка потребителю; канал закрыт — отмена.
async fn send(tx: &mpsc::Sender<Ev>, ev: Ev) -> Result<(), Failure> {
    tx.send(ev).await.map_err(|_| {
        Failure::sent(ModelError::new(
            ErrorClass::Cancelled,
            "потребитель стрима закрыл канал",
        ))
    })
}

/// Сигнал отмены run.
async fn wait_cancel(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Разрешение предохранителя запроса; `None` — ещё не взято или исход уже сообщён.
type Verdict = Arc<Mutex<Option<BreakerPermit>>>;

/// Действия при `Finished` попытки — до передачи потребителю: иначе на многопоточном
/// runtime следующий запрос мог бы застать незакрытую пробу предохранителя.
#[derive(Clone)]
struct FinishHook {
    inner: Arc<Inner>,
    endpoint: EndpointId,
    verdict: Verdict,
}

impl FinishHook {
    /// Успех предохранителю и расход в бюджет группы (D14).
    fn finished(&self, usage: &Usage) {
        let Some(ep) = self.inner.endpoints.get(&self.endpoint) else {
            return;
        };
        let p = lock(&self.verdict).take();
        if let Some(p) = p {
            lock(&ep.breaker).on_success(p);
        }
        if let Some(c) = usage.cost_usd {
            self.inner.queue.record_spend(&ep.group, c);
        }
    }
}

/// Пересылает события попытки, дописывая стоимость в `Finished` (D16) и сообщая успех
/// ([`FinishHook`]); кончается по закрытию приёмника (D2): его попытка брошена, канал
/// коннектора закрывается.
async fn price_events(
    mut events: mpsc::Receiver<Ev>,
    out: mpsc::Sender<Ev>,
    price: Option<Price>,
    hook: FinishHook,
) {
    loop {
        let ev = tokio::select! {
            biased;
            () = out.closed() => return,
            ev = events.recv() => ev,
        };
        let Some(mut ev) = ev else { return };
        if let Ok(StreamEvent::Finished { usage, .. }) = &mut ev {
            apply_cost(usage, price.as_ref());
            hook.finished(usage);
        }
        if out.send(ev).await.is_err() {
            return;
        }
    }
}

/// D16: сумма провайдера, иначе расчёт по ценам, иначе неизвестно.
fn apply_cost(u: &mut Usage, price: Option<&Price>) {
    if u.cost_usd.is_some() {
        u.cost_source = CostSource::Provider;
        return;
    }
    match price {
        Some(p) => {
            u.cost_usd = Some(compute_cost(p, u));
            u.cost_source = CostSource::Computed;
        }
        None => u.cost_source = CostSource::Unknown,
    }
}

/// Лимит провайдера исчерпан в успешном ответе: момент сброса (MDL-016, RES-004).
fn exhausted_until(l: &ProviderLimits) -> Option<Instant> {
    let exhausted = l.remaining_requests == Some(0) || l.remaining_tokens == Some(0);
    if !exhausted {
        return None;
    }
    l.reset_after.and_then(|d| Instant::now().checked_add(d))
}

/// 400 с упоминанием `tool_choice` на принудительный выбор (D15).
fn tool_choice_rejected(err: &ModelError, choice: &ToolChoice) -> bool {
    err.http_status == Some(400)
        && matches!(choice, ToolChoice::Required | ToolChoice::Named(_))
        && err.message.to_ascii_lowercase().contains("tool_choice")
}

/// Остаток предела ожидания после `waited` (повторная постановка заявки, M1);
/// `Some(ZERO)` — предел исчерпан, очередь откажет `wait_timeout` сразу.
fn remaining_wait(max_wait: Option<Duration>, waited: Duration) -> Option<Duration> {
    max_wait.map(|m| m.saturating_sub(waited))
}

/// Отказ очереди наружу: `cancelled` → `Cancelled`, прочие → `Queue` с кодом (D12).
fn queue_error(e: &QueueError) -> ModelError {
    match e {
        QueueError::Cancelled => ModelError::new(ErrorClass::Cancelled, e.code()),
        _ => ModelError::new(ErrorClass::Queue, e.code()),
    }
}

fn inactive(id: &EndpointId, reason: &str) -> ModelError {
    ModelError::new(
        ErrorClass::Inactive,
        format!("эндпоинт {id} не активен: {reason}"),
    )
}

/// Коннектор эндпоинта (D35, D37). Модель — из конфигурации, иначе из профиля связки.
fn connector(cfg: &EndpointConfig, p: &ProbedEndpoint) -> Connector {
    let base_url = cfg.effective_base_url();
    let model = cfg
        .model
        .clone()
        .unwrap_or_else(|| p.profile.bundle.model.clone());
    let chat = |dialect| {
        Connector::Chat(ChatWire {
            dialect,
            base_url: base_url.clone(),
            model: model.clone(),
        })
    };
    match cfg.backend {
        BackendKind::LlamaCpp => chat(ChatDialect::LlamaCpp),
        BackendKind::Vllm => chat(ChatDialect::Vllm),
        BackendKind::OpenRouter => chat(ChatDialect::OpenRouter),
        BackendKind::OpenAiChat => chat(ChatDialect::OpenAi),
        BackendKind::OpenAiCompatible => chat(ChatDialect::Generic),
        BackendKind::Anthropic => Connector::Anthropic(AnthropicWire {
            base_url: base_url.clone(),
            model: model.clone(),
        }),
        BackendKind::OpenAiResponses => Connector::Responses(ResponsesWire {
            base_url: base_url.clone(),
            model: model.clone(),
        }),
    }
}

/// Факты эндпоинта и профиль их связки из хранилища; ключ связки сверяется.
fn load_probed(store: &ProfileStore, id: &EndpointId) -> Option<ProbedEndpoint> {
    let facts = store.load_endpoint(id)?;
    let profile = store.load(&facts.bundle_key)?;
    if profile.bundle.key() != facts.bundle_key {
        tracing::warn!(endpoint = %id, "профиль не той связки, что в фактах эндпоинта");
        return None;
    }
    Some(ProbedEndpoint { profile, facts })
}

fn warning_text(w: &ConfigWarning) -> String {
    match w {
        ConfigWarning::BudgetMismatch { group, used } => format!(
            "models: у эндпоинтов облачной группы {group} разные daily_budget_usd, \
             действует минимум {used} USD (D34)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_wait_after_requeue() {
        assert_eq!(
            remaining_wait(Some(Duration::from_millis(100)), Duration::from_millis(60)),
            Some(Duration::from_millis(40))
        );
        assert_eq!(
            remaining_wait(Some(Duration::from_millis(100)), Duration::from_millis(150)),
            Some(Duration::ZERO)
        );
        assert_eq!(remaining_wait(None, Duration::from_secs(5)), None);
    }
}
