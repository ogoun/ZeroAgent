//! `Gateway` — единственный путь к модели для рабочего кода (RES-001, D38): собирает
//! зонды и профили, коннекторы, повторы, предохранитель, очередь с допуском по ресурсам
//! и слоты llama.cpp в одну реализацию [`ModelPort`].
//!
//! **Путь запроса** ([`ModelPort::stream`]):
//! 1. До стрима: эндпоинт `Inactive` — ошибка `Inactive` без сети; `Unreachable` — один
//!    повторный зонд, не ответил — `Transport` (D40); идентичный запрос после ошибки
//!    разбора — `RepeatedAfterParseError` (D21, D43); предохранитель не пускает —
//!    `CircuitOpen` (D22, переход в `HalfOpen` запускает повторный зонд в фоне); заявка
//!    встаёт в очередь своей группы.
//! 2. Стрим возвращается сразу; его guard — [`AbortOnDrop`] фоновой задачи (D2). Первое
//!    событие — всегда `Queued`, затем каждое изменение статуса заявки, пока она ждёт
//!    (D31). Отказ очереди: `cancelled` → `ErrorClass::Cancelled`, прочие — `Queue` с
//!    кодом [`QueueError::code`] в тексте (D12).
//! 3. Место выдано: у llama.cpp слот (`SlotManager`, ключ `"{bundle_key}:{session}"`),
//!    порядок `save` вытесненной сессии → `restore` → запрос (D29); затем
//!    `Started { attempt: 1 }` и [`drive_with_retry`] (D19, D20) с джиттером от
//!    [`request_fingerprint`] (D44). Каждый 429 ставит cooldown группы до
//!    `now + Retry-After`; успешный ответ с исчерпанным лимитом — cooldown до сброса
//!    (MDL-016, RES-004). В `Finished` дописывается стоимость (D16), она же учитывается
//!    в бюджете группы (D14).
//! 4. Итог сообщается предохранителю через его разрешение — для каждой пропущенной
//!    заявки, в том числе отменённой. `Malformed` запоминает отпечаток; итог `Transport`
//!    или `StreamCut` запускает повторный зонд; отказ провайдера по `tool_choice`
//!    снимает `tool_choice_required` в профиле (D15).
//!
//! Эндпоинт запроса не меняется ни при каком исходе: автоматического перехода на другую
//! модель, тем более в облако, нет (MDL-016, ТЗ D-24).
//!
//! **Освобождение** (Review Focus 5). Drop стрима отменяет задачу: заявка снимается или
//! освобождаются слот и место очереди (слот раньше места), канал попытки закрывается, и
//! насос коннектора закрывает HTTP-соединение (D2, D35). [`Gateway::cancel_run`] снимает
//! ожидающие заявки run и отменяет уже идущие его запросы.
//!
//! **Профили** (D40). После каждого зонда профиль связки и факты эндпоинта пишутся в
//! [`ProfileStore`], а `SlotManager` создаётся заново (новая связка, другое число слотов,
//! перезапуск сервера). Контекст и слоты берутся из фактов эндпоинта, а не из профиля
//! связки. Если зонд при старте не удался, для показа берутся последние сохранённые
//! факты и профиль их связки (ключи сверяются).

use std::collections::{BTreeMap, HashMap};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

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
use crate::profile::{EndpointState, ProbedEndpoint, ProfileStore};
use crate::queue::{
    DEFAULT_CLOUD_PARALLEL, GroupId, GroupLimits, ModelQueue, Permit as PlacePermit, QueueError,
    QueueSnapshot, Ticket,
};
use crate::resilience::{
    BreakerOptions, BreakerState, CircuitBreaker, ParseGuard, Permit as BreakerPermit, RetryPolicy,
    drive_with_retry, request_fingerprint,
};
use crate::slots::{
    LlamaSlots, SlotLease, SlotManager, SlotPlan, restore_unsupported, save_unsupported,
};
use crate::types::{
    CostSource, EndpointId, ReasoningMode, StreamEvent, ToolChoice, Usage, WaitState,
};
use crate::wire::{AnthropicWire, ChatDialect, ChatWire, Connector, ResponsesWire, WireCtx};

/// Число мест локального эндпоинта, кроме llama.cpp, без `max_parallel` (D8).
pub const DEFAULT_LOCAL_PARALLEL: u32 = 4;

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
}

impl Default for GatewayOptions {
    /// D19 (повторы), D26 (таймауты), D22 (предохранитель).
    fn default() -> Self {
        Self {
            retry: RetryPolicy::default(),
            timeouts_local: Timeouts::local(),
            timeouts_cloud: Timeouts::cloud(),
            breaker: BreakerOptions::default(),
        }
    }
}

/// Эндпоинт для `models`, `doctor` и UI-046.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EndpointView {
    pub id: EndpointId,
    pub backend: BackendKind,
    pub pool: PoolKind,
    /// `ready`, `inactive`, `unreachable`, `open` (предохранитель открыт или идёт проба),
    /// `disabled` (ключ отвергнут, до [`Gateway::reset`]).
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
    /// Идёт фоновый повторный зонд: второй не запускается.
    probing: AtomicBool,
    /// Предупреждение «сохранение слотов не поддержано» уже записано (D29).
    slot_save_warned: AtomicBool,
}

/// Изменяемое состояние эндпоинта.
struct Live {
    state: EndpointState,
    /// Последние известные профиль и факты: из зонда или из `ProfileStore` при старте.
    last: Option<ProbedEndpoint>,
    /// Слоты llama.cpp; новый менеджер после каждого зонда.
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

/// Идущий запрос run: сигнал отмены и признак забранного места очереди.
struct CallHandle {
    cancel: watch::Sender<bool>,
    claimed: Arc<AtomicBool>,
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
    /// Проверяет конфигурацию, настраивает группы очереди и зондирует все эндпоинты
    /// параллельно. Облачный эндпоинт без ключа становится `Inactive` без сети
    /// (MDL-021). Недоступный эндпоинт не мешает старту: он `Unreachable`, первый
    /// запрос к нему зондирует его ещё раз (D40).
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
        for (group, limits) in inner.group_limits() {
            inner.queue.configure(group, limits);
        }
        let probes: Vec<_> = inner
            .endpoints
            .keys()
            .cloned()
            .map(|id| {
                let inner = inner.clone();
                inner.rt.clone().spawn(async move {
                    inner.probe_now(&id).await;
                })
            })
            .collect();
        for p in probes {
            if let Err(e) = p.await {
                tracing::warn!(error = %e, "зонд эндпоинта при старте не завершён");
            }
        }
        inner.load_cached().await;
        Ok(Self { inner })
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

    /// Зонд эндпоинта по требованию (MDL-003 «при обновлении»): новое состояние,
    /// профиль и слоты. Неизвестный id — `Inactive`.
    pub async fn probe(&self, id: &EndpointId) -> EndpointState {
        self.inner.probe_now(id).await
    }

    /// Выбор пользователя «повторить» (D22): предохранитель закрыт, счёт с начала.
    /// Неготовый эндпоинт (нет ключа, не отвечает) зондируется заново в фоне.
    pub fn reset(&self, id: &EndpointId) {
        let Some(ep) = self.inner.endpoints.get(id) else {
            return;
        };
        lock(&ep.breaker).reset();
        if lock(&ep.live).ready().is_none() {
            self.inner.spawn_reprobe(id);
        }
    }

    /// Отмена run (RES-003): ожидающие и выданные, но не забранные заявки снимаются,
    /// идущие запросы run прерываются (слот и место освобождаются); их стримы кончаются
    /// ошибкой `Cancelled`. Возвращает число снятых заявок и прерванных запросов.
    ///
    /// Запрос, который забирает место в этот же миг, прерывается, но может не попасть в
    /// счёт.
    pub fn cancel_run(&self, run: RunId) -> usize {
        let mut n = self.inner.queue.cancel_run(run);
        let calls = lock(&self.inner.calls);
        if let Some(m) = calls.get(&run) {
            for c in m.values() {
                if c.claimed.load(Ordering::SeqCst) {
                    n += 1;
                }
                let _ = c.cancel.send(true);
            }
        }
        n
    }

    /// Run завершён: отпечатки запросов после ошибок разбора забываются
    /// (`ParseGuard::clear`). Вызывает цикл R1-B при переходе run в конечное состояние.
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
        Box::pin(self.inner.open_call(call))
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
                    reason: "зонд ещё не выполнен".to_owned(),
                },
                last: None,
                slots: None,
            }),
            probing: AtomicBool::new(false),
            slot_save_warned: AtomicBool::new(false),
        }
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

/// Неудача запроса; `sent` — ошибка уже ушла в канал потребителя.
struct Failure {
    err: ModelError,
    sent: bool,
}

impl Failure {
    fn unsent(err: ModelError) -> Self {
        Self { err, sent: false }
    }

    fn sent(err: ModelError) -> Self {
        Self { err, sent: true }
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

/// Снимает признак фонового зонда, в том числе если задача не запустилась.
struct ProbeFlag {
    inner: Arc<Inner>,
    id: EndpointId,
}

impl Drop for ProbeFlag {
    fn drop(&mut self) {
        if let Some(ep) = self.inner.endpoints.get(&self.id) {
            ep.probing.store(false, Ordering::Release);
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

    /// Зонд эндпоинта и применение результата.
    async fn probe_now(&self, id: &EndpointId) -> EndpointState {
        let Some(ep) = self.endpoints.get(id) else {
            return EndpointState::Inactive {
                reason: format!("эндпоинт {id} не описан в models.endpoints"),
            };
        };
        let state = probe_endpoint(&ep.cfg, &ep.probe_http, &*self.secrets).await;
        self.apply_probe(ep, state.clone()).await;
        state
    }

    /// Новое состояние после зонда: профиль и факты в хранилище, новый `SlotManager`,
    /// места группы по слотам сервера (D8, D40).
    async fn apply_probe(&self, ep: &Endpoint, state: EndpointState) {
        if let EndpointState::Ready(p) = &state
            && let Err(e) = self.profiles.save_probed_async(p.clone()).await
        {
            tracing::warn!(endpoint = %ep.cfg.id, error = %e, "профиль эндпоинта не сохранён");
        }
        let (old_key, new_key) = {
            let mut live = lock(&ep.live);
            let old_key = live.known().map(|p| p.facts.bundle_key.clone());
            let new_key = match &state {
                EndpointState::Ready(p) => {
                    live.last = Some(p.clone());
                    live.slots = ep.is_llama().then(|| {
                        Arc::new(Mutex::new(SlotManager::new(p.facts.slots.unwrap_or(1))))
                    });
                    Some(p.facts.bundle_key.clone())
                }
                _ => {
                    live.slots = None;
                    None
                }
            };
            live.state = state;
            (old_key, new_key)
        };
        if let (Some(old), Some(new)) = (&old_key, &new_key)
            && old != new
        {
            tracing::info!(endpoint = %ep.cfg.id, old = %old, new = %new,
                           "на эндпоинте другая связка модели: профиль заменён");
        }
        if ep.is_llama() {
            self.queue
                .set_parallel(&ep.group, self.group_parallel(&ep.group));
        }
    }

    /// Факты и профили из хранилища для эндпоинтов, чей зонд при старте не удался.
    /// Профиль берётся по `bundle_key` фактов, ключ связки профиля сверяется с ним.
    async fn load_cached(&self) {
        let missing: Vec<EndpointId> = self
            .endpoints
            .values()
            .filter(|e| lock(&e.live).known().is_none())
            .map(|e| e.cfg.id.clone())
            .collect();
        if missing.is_empty() {
            return;
        }
        let store = self.profiles.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            missing
                .into_iter()
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

    /// Повторный зонд в фоне (D40); один на эндпоинт одновременно.
    fn spawn_reprobe(self: &Arc<Self>, id: &EndpointId) {
        let Some(ep) = self.endpoints.get(id) else {
            return;
        };
        if ep.probing.swap(true, Ordering::AcqRel) {
            return;
        }
        let flag = ProbeFlag {
            inner: self.clone(),
            id: id.clone(),
        };
        self.rt.spawn(async move {
            let state = flag.inner.probe_now(&flag.id).await;
            tracing::debug!(endpoint = %flag.id, ready = matches!(state, EndpointState::Ready(_)),
                            "повторный зонд эндпоинта");
            drop(flag);
        });
    }

    /// Проверки до стрима и постановка в очередь; затем фоновая задача запроса.
    async fn open_call(self: &Arc<Self>, call: ModelCall) -> Result<ModelStream, ModelError> {
        let Some(ep) = self.endpoints.get(&call.endpoint) else {
            return Err(ModelError::new(
                ErrorClass::NotFound,
                format!("эндпоинт {} не описан в models.endpoints", call.endpoint),
            ));
        };
        let unreachable = match &lock(&ep.live).state {
            EndpointState::Inactive { reason } => return Err(inactive(&ep.cfg.id, reason)),
            EndpointState::Unreachable { .. } => true,
            EndpointState::Ready(_) => false,
        };
        if unreachable {
            match self.probe_now(&call.endpoint).await {
                EndpointState::Ready(_) => {}
                EndpointState::Inactive { reason } => return Err(inactive(&ep.cfg.id, &reason)),
                EndpointState::Unreachable { reason } => {
                    return Err(ModelError::new(
                        ErrorClass::Transport,
                        format!("эндпоинт {} не отвечает: {reason}", ep.cfg.id),
                    ));
                }
            }
        }
        let run = call.meta.run_id;
        let fp = request_fingerprint(&call.endpoint, &call.request);
        lock(&self.parse_guard).check(run, &fp)?;
        let permit = lock(&ep.breaker).allow(Instant::now())?;
        if permit.is_probe() {
            self.spawn_reprobe(&call.endpoint);
        }
        let ticket = match self.queue.enqueue(&ep.group, run, call.meta.priority, None) {
            Ok(t) => t,
            Err(e) => {
                let err = queue_error(&e);
                lock(&ep.breaker).on_failure(permit, &err, Instant::now());
                return Err(err);
            }
        };
        let id = self.next_call.fetch_add(1, Ordering::Relaxed);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let claimed = Arc::new(AtomicBool::new(false));
        lock(&self.calls).entry(run).or_default().insert(
            id,
            CallHandle {
                cancel: cancel_tx,
                claimed: claimed.clone(),
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
            tx,
            claimed,
            _reg: reg,
        };
        let handle = tokio::spawn(task.run(ticket, permit, cancel_rx));
        Ok(ModelStream::from_channel(rx, Box::new(AbortOnDrop(handle))))
    }

    /// Ключ эндпоинта на момент запроса; `key_ref` без ключа — `Inactive`.
    fn secret(&self, ep: &Endpoint) -> Result<Option<Secret>, ModelError> {
        match &ep.cfg.key_ref {
            None => Ok(None),
            Some(k) => self
                .secrets
                .get(k)
                .map(Some)
                .ok_or_else(|| inactive(&ep.cfg.id, &format!("ключ {k} не настроен"))),
        }
    }

    /// `save` вытесненной сессии, затем `restore` своей (D29). Ошибки влияют только на
    /// скорость; «не поддержано» (нет `--slot-save-path`) выключает сохранение.
    async fn prepare_slot(&self, ep: &Endpoint, plan: &SlotPlan) {
        let disabled = lock(&ep.live)
            .ready()
            .is_some_and(|p| p.facts.slot_save == Some(false));
        if disabled || (plan.save_evicted.is_none() && plan.restore.is_none()) {
            return;
        }
        let slots = LlamaSlots::new(ep.cfg.effective_base_url(), ep.http.clone());
        let mut unsupported = false;
        if let Some((slot, file)) = &plan.save_evicted {
            match slots.save(*slot, file).await {
                Ok(()) => {}
                Err(e) if save_unsupported(&e) => unsupported = true,
                Err(e) => tracing::debug!(endpoint = %ep.cfg.id, slot, class = ?e.class,
                                          "слот не сохранён"),
            }
        }
        if !unsupported && let Some(file) = &plan.restore {
            match slots.restore(plan.slot, file).await {
                Ok(()) => tracing::debug!(endpoint = %ep.cfg.id, slot = plan.slot,
                                          "слот восстановлен"),
                Err(e) if restore_unsupported(&e) => unsupported = true,
                // 400 — файла нет: новая сессия.
                Err(e) => tracing::debug!(endpoint = %ep.cfg.id, slot = plan.slot,
                                          class = ?e.class, "слот не восстановлен"),
            }
        }
        if unsupported {
            self.disable_slot_save(ep);
        }
    }

    /// `slot_save = Some(false)` в фактах эндпоинта, одно предупреждение (D29).
    fn disable_slot_save(&self, ep: &Endpoint) {
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
            tracing::warn!(endpoint = %ep.cfg.id,
                "llama-server не сохраняет слоты (нет --slot-save-path): работа без сохранения KV-кэша");
        }
        let store = self.profiles.clone();
        tokio::task::spawn_blocking(move || {
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
        tokio::task::spawn_blocking(move || {
            if let Err(e) = store.save(&profile) {
                tracing::warn!(error = %e, "профиль модели не сохранён");
            }
        });
    }
}

/// Фоновая задача одного запроса.
struct CallTask {
    inner: Arc<Inner>,
    call: ModelCall,
    fp: String,
    tx: mpsc::Sender<Ev>,
    claimed: Arc<AtomicBool>,
    _reg: Registration,
}

impl CallTask {
    async fn run(self, ticket: Ticket, permit: BreakerPermit, mut cancel: watch::Receiver<bool>) {
        let Some(ep) = self.inner.endpoints.get(&self.call.endpoint) else {
            return;
        };
        // Отмена run бросает `serve` целиком: слот, место и попытка освобождаются до
        // отправки ошибки.
        let outcome = tokio::select! {
            biased;
            () = wait_cancel(&mut cancel) => Err(Failure::unsent(ModelError::new(
                ErrorClass::Cancelled,
                "run отменён",
            ))),
            r = self.serve(ep, ticket) => r,
        };
        match outcome {
            Ok(()) => lock(&ep.breaker).on_success(permit),
            Err(f) => {
                lock(&ep.breaker).on_failure(permit, &f.err, Instant::now());
                if !f.sent {
                    let _ = self.tx.send(Err(f.err.clone())).await;
                }
                self.after_failure(ep, &f.err);
            }
        }
    }

    /// Очередь, слот, повторы, стоимость.
    async fn serve(&self, ep: &Endpoint, ticket: Ticket) -> Result<(), Failure> {
        let place = wait_place(ticket, &self.tx).await?;
        self.claimed.store(true, Ordering::SeqCst);
        let (probed, slots) = {
            let live = lock(&ep.live);
            match &live.state {
                EndpointState::Ready(p) => (p.clone(), live.slots.clone()),
                EndpointState::Inactive { reason } => {
                    return Err(Failure::unsent(inactive(&ep.cfg.id, reason)));
                }
                EndpointState::Unreachable { reason } => {
                    return Err(Failure::unsent(ModelError::new(
                        ErrorClass::Transport,
                        format!("эндпоинт {} не отвечает: {reason}", ep.cfg.id),
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
                self.inner.prepare_slot(ep, &plan).await;
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
        let usage: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));
        let (queue, group, http) = (&self.inner.queue, &ep.group, &ep.http);
        let (conn, req, sink) = (&connector, &req, &usage);
        let open = move |_attempt: u32| async move {
            let opened = conn.open(http, req, &ctx).await?;
            if let Some(until) = exhausted_until(&opened.limits) {
                tracing::debug!(group = %group, "лимит провайдера исчерпан: cooldown группы");
                queue.cooldown(group, until);
            }
            let (mtx, mrx) = mpsc::channel(STREAM_CHANNEL);
            tokio::spawn(price_events(opened.events, mtx, price, sink.clone()));
            Ok(mrx)
        };
        let on_attempt_error = |e: &ModelError| {
            if e.class == ErrorClass::RateLimited
                && let Some(until) = e.retry_after.and_then(|d| Instant::now().checked_add(d))
            {
                queue.cooldown(group, until);
            }
        };
        drive_with_retry(
            &self.inner.opts.retry,
            &self.fp,
            &self.tx,
            open,
            on_attempt_error,
        )
        .await
        .map_err(Failure::sent)?;
        let cost = lock(&usage).as_ref().and_then(|u| u.cost_usd);
        if let Some(c) = cost {
            queue.record_spend(group, c);
        }
        Ok(())
    }

    /// Реакция на итоговую ошибку (D21, D40, D15).
    fn after_failure(&self, ep: &Endpoint, err: &ModelError) {
        match err.class {
            ErrorClass::Malformed => {
                lock(&self.inner.parse_guard).record(self.call.meta.run_id, self.fp.clone());
            }
            ErrorClass::Transport | ErrorClass::StreamCut => {
                self.inner.spawn_reprobe(&ep.cfg.id);
            }
            ErrorClass::BadRequest if tool_choice_rejected(err, &self.call.request.tool_choice) => {
                self.inner.disable_forced_tool_choice(ep);
            }
            _ => {}
        }
    }
}

/// Ждёт места, пересылая статус заявки: первым — текущий (`Queued` всегда, даже при
/// свободном месте), затем каждое изменение, пока заявка ждёт.
async fn wait_place(ticket: Ticket, tx: &mpsc::Sender<Ev>) -> Result<PlacePermit, Failure> {
    let mut status = ticket.status();
    let first = status.borrow_and_update().clone();
    send(tx, Ok(StreamEvent::Queued(first))).await?;
    let mut granted = pin!(ticket.granted());
    let mut watching = true;
    loop {
        tokio::select! {
            biased;
            r = &mut granted => return r.map_err(|e| Failure::unsent(queue_error(&e))),
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

/// Пересылает события попытки, дописывая стоимость в `Finished` (D16); кончается по
/// закрытию приёмника (D2): его попытка брошена, канал коннектора закрывается.
async fn price_events(
    mut events: mpsc::Receiver<Ev>,
    out: mpsc::Sender<Ev>,
    price: Option<Price>,
    sink: Arc<Mutex<Option<Usage>>>,
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
            *lock(&sink) = Some(usage.clone());
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
