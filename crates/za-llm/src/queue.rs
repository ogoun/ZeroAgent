//! Общая очередь к моделям (RES-001…RES-007, RES-012, RES-013; D8, D9, D12, D14, D34).
//!
//! Заявки стоят в **группах**: локальная группа — один эндпоинт, облачная —
//! `cloud:<backend>:<host>` (D34). Группы друг друга не блокируют (пулы локальных и облачных
//! раздельны). У группы `max_parallel` мест (D8: `min(слоты сервера, max_parallel)` считает
//! вызывающий и передаёт в [`GroupLimits`] или [`ModelQueue::set_parallel`]).
//!
//! **Порядок выдачи** (D9). Эффективный ранг заявки — `max(базовый − ⌊ожидание / 30 с⌋,
//! min(базовый, 1))`: состарившийся фон поднимается до ранга 1, но `Interactive` (ранг 0)
//! никто не обгоняет (RES-013). При равенстве — FIFO. Выдача запускается синхронно при
//! `enqueue`, освобождении места, `configure`, `set_parallel`, `cooldown`, `record_spend`,
//! `cancel_run`, а фоновая задача очереди — по таймерам (конец cooldown, `max_wait`, границы
//! старения для пересчёта позиций) и на каждом новом снимке ресурсов. Для каждой группы,
//! пока есть свободные места, берётся первая по порядку заявка, которая проходит:
//! 1. дневной бюджет (D14, UTC-сутки) — исчерпан: все ожидающие заявки группы сразу
//!    завершаются [`QueueError::BudgetExhausted`], не дожидаясь свободного места;
//! 2. cooldown группы (429, исчерпанный лимит провайдера) — пока он идёт, выдачи нет,
//!    состояние ожидающих `Cooldown { remaining_ms }`;
//! 3. допуск по ресурсам при `resource_gated` (`Need { local_gpu: true, .. }`) — проверяется у
//!    каждой ожидающей независимо от занятости мест, поэтому `WaitingResource { kind }`
//!    отражает ресурс, а не слоты; не прошедшая не занимает места и не блокирует следующих
//!    (RES-012);
//! 4. лимит фоновых (`Job`, `Arena`) в локальной группе — не больше `max(1, n − 1)` из `n`
//!    мест: последнее место — под интерактивную работу. При `n = 1` фон может занять
//!    единственное место, пока нет ожидающего интерактива; уже выданное место не
//!    вытесняется (D9) — пользователь ждёт конца фоновой генерации.
//!
//! **Голодание фона** допустимо (RES-013): пол ранга не-интерактивных — 1, поэтому при
//! непрерывном потоке `Interactive` заявки прочих классов ждут без предела. Среди остальных
//! классов ожидание ограничено старением. Выход для run — `max_wait` (D12) и показ `waited_ms`.
//!
//! **Ожидание** (D12). Без `max_wait` заявка по времени не снимается (RES-007); с `max_wait` —
//! [`QueueError::WaitTimeout`]. Позиция — номер в отсортированном списке ожидающих группы
//! с 1; у выданной и проваленной — 0. Провал виден и в статусе: `WaitState::Failed { reason }`
//! с кодом [`QueueError::code`].
//!
//! **Освобождение** (Review Focus 5). Drop [`Ticket`] до выдачи снимает заявку; выданное, но
//! не забранное `granted()` место возвращается. Drop [`Permit`] (в том числе при панике
//! задачи и при брошенном стриме) освобождает место. [`ModelQueue::cancel_run`] снимает все
//! незабранные заявки run (RES-003): ждущие `granted()` получают [`QueueError::Cancelled`].
//!
//! **Фоновая задача** (таймеры и снимки ресурсов) запускается в runtime, переданном в
//! [`ModelQueue::new`], и находится под надзором: если она завершилась (паника, отмена), её
//! перезапускает следующее обращение к очереди (`enqueue`, `configure`, `set_parallel`,
//! `cooldown`, `record_spend`, `cancel_run`, `granted()`) с одним `warn` на перезапуск. Если
//! runtime остановлен и перезапуск невозможен, заявки, которым нужен таймер или снимок
//! (`max_wait`, cooldown группы, ожидание ресурса), получают [`QueueError::RuntimeStopped`];
//! заявки без таймеров продолжают выдаваться синхронно. Время — `tokio::time::Instant`.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use serde::Serialize;
use tokio::runtime::Handle;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use za_protocol::{Priority, RunId};
use za_res::{Admission, Need, ResourceSnapshot, Verdict};

use crate::caps::PoolKind;
use crate::types::{QueueStatus, WaitState};

/// Число мест облачной группы по умолчанию (RES-004); конфигурация может переопределить
/// `max_parallel` эндпоинта.
pub const DEFAULT_CLOUD_PARALLEL: u32 = 4;
/// Шаг старения (D9): каждые 30 с ожидания ранг заявки уменьшается на 1.
pub const AGING_STEP: Duration = Duration::from_secs(30);

/// Группа очереди: `EndpointConfig::group_key()` — у локального эндпоинта его id, у
/// облачного `cloud:<backend>:<host>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct GroupId(pub String);

impl fmt::Display for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Пределы группы.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupLimits {
    pub kind: PoolKind,
    /// Число мест (D8).
    pub max_parallel: u32,
    /// Выдача проходит допуск по ресурсам узла (эндпоинт на этом узле).
    pub resource_gated: bool,
    /// Дневной бюджет, доллары (D14, D34); `None` — без предела.
    pub daily_budget_usd: Option<f64>,
}

/// Отказ очереди. Текст — код варианта (`cancelled`, `wait_timeout`, …): по нему R1-B ставит
/// причину паузы run (D12).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum QueueError {
    /// Run отменён ([`ModelQueue::cancel_run`]).
    #[error("cancelled")]
    Cancelled,
    /// Истёк `max_wait` заявки (D12).
    #[error("wait_timeout")]
    WaitTimeout,
    /// Исчерпан дневной бюджет группы (D14).
    #[error("budget_exhausted: {group}")]
    BudgetExhausted { group: GroupId },
    /// Группа не настроена.
    #[error("unknown_group: {0}")]
    UnknownGroup(GroupId),
    /// Монитор ресурсов остановлен, а заявка ждёт ресурса.
    #[error("monitor_stopped")]
    MonitorStopped,
    /// Runtime очереди остановлен, а заявке нужен таймер или снимок ресурсов.
    #[error("runtime_stopped")]
    RuntimeStopped,
}

impl QueueError {
    /// Код варианта без подробностей.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::WaitTimeout => "wait_timeout",
            Self::BudgetExhausted { .. } => "budget_exhausted",
            Self::UnknownGroup(_) => "unknown_group",
            Self::MonitorStopped => "monitor_stopped",
            Self::RuntimeStopped => "runtime_stopped",
        }
    }
}

/// Снимок очереди для `status`, UI и `doctor`.
#[derive(Debug, Clone, Serialize)]
pub struct QueueSnapshot {
    /// Группы по возрастанию id.
    pub groups: Vec<GroupView>,
}

/// Группа в снимке.
#[derive(Debug, Clone, Serialize)]
pub struct GroupView {
    pub id: GroupId,
    pub kind: PoolKind,
    pub max_parallel: u32,
    /// Выданные места, включая ещё не забранные `granted()`.
    pub active: u32,
    /// Остаток cooldown, мс; `None` — нет.
    pub cooldown_ms: Option<u64>,
    /// Потрачено за текущие UTC-сутки.
    pub spent_today_usd: f64,
    /// Ожидающие в порядке выдачи.
    pub waiting: Vec<WaitingView>,
}

/// Ожидающая заявка в снимке.
#[derive(Debug, Clone, Serialize)]
pub struct WaitingView {
    pub run_id: RunId,
    pub priority: Priority,
    pub position: u32,
    pub waited_ms: u64,
    pub state: WaitState,
}

type Today = Arc<dyn Fn() -> jiff::civil::Date + Send + Sync>;

/// Текущие UTC-сутки (D14).
fn utc_today() -> jiff::civil::Date {
    jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Эффективный ранг по D9.
fn effective_rank(p: Priority, waited: Duration) -> u64 {
    let base = u64::from(p.rank());
    let steps = waited.as_secs() / AGING_STEP.as_secs();
    base.saturating_sub(steps).max(base.min(1))
}

#[derive(Debug)]
enum Phase {
    Waiting,
    /// Место выдано и учтено в `active`, `granted()` ещё не забрал его.
    Granted,
    Failed(QueueError),
}

struct Entry {
    group: GroupId,
    run: RunId,
    priority: Priority,
    enqueued: Instant,
    deadline: Option<Instant>,
    phase: Phase,
    /// Последнее состояние статуса: снимок читает его, а не `watch` (без чтения `watch`
    /// под замком очереди).
    state: WaitState,
    status: watch::Sender<QueueStatus>,
    wake: Arc<Notify>,
}

impl Entry {
    fn background(&self) -> bool {
        self.priority.is_background()
    }

    /// Обновляет статус; получатели уведомляются только при смене позиции или состояния,
    /// `waited_ms` обновляется молча.
    fn set_status(&mut self, position: u32, state: WaitState, now: Instant) {
        let waited_ms = millis(now.saturating_duration_since(self.enqueued));
        self.state = state.clone();
        self.status.send_if_modified(|s| {
            let changed = s.position != position || s.state != state;
            s.position = position;
            s.state = state;
            s.waited_ms = waited_ms;
            changed
        });
    }

    fn fail(&mut self, e: QueueError, now: Instant) {
        let reason = e.code().to_owned();
        self.phase = Phase::Failed(e);
        self.set_status(0, WaitState::Failed { reason }, now);
        self.wake.notify_one();
    }
}

struct Group {
    limits: GroupLimits,
    active: u32,
    /// Выданные фоновым (`Job`, `Arena`) места.
    active_bg: u32,
    cooldown_until: Option<Instant>,
    spent: f64,
    spent_day: Option<jiff::civil::Date>,
    /// Ожидающие в порядке постановки (id растут).
    waiting: Vec<u64>,
}

impl Group {
    fn new(limits: GroupLimits) -> Self {
        Self {
            limits,
            active: 0,
            active_bg: 0,
            cooldown_until: None,
            spent: 0.0,
            spent_day: None,
            waiting: Vec::new(),
        }
    }

    fn spent_today(&self, today: jiff::civil::Date) -> f64 {
        if self.spent_day == Some(today) {
            self.spent
        } else {
            0.0
        }
    }

    fn roll_day(&mut self, today: jiff::civil::Date) {
        if self.spent_day != Some(today) {
            self.spent_day = Some(today);
            self.spent = 0.0;
        }
    }

    fn exhausted(&self, today: jiff::civil::Date) -> bool {
        self.limits
            .daily_budget_usd
            .is_some_and(|b| self.spent_today(today) >= b)
    }

    fn cooldown_left(&self, now: Instant) -> Option<Duration> {
        self.cooldown_until
            .filter(|u| *u > now)
            .map(|u| u.saturating_duration_since(now))
    }

    fn release(&mut self, background: bool) {
        self.active = self.active.saturating_sub(1);
        if background {
            self.active_bg = self.active_bg.saturating_sub(1);
        }
    }
}

struct State {
    groups: BTreeMap<GroupId, Group>,
    entries: HashMap<u64, Entry>,
    next_id: u64,
    monitor_stopped: bool,
    /// Runtime остановлен: фоновой задачи нет и не будет.
    driver_dead: bool,
    today: Today,
}

impl State {
    /// Ожидающие группы в порядке выдачи: `(эффективный ранг, seq)`.
    fn sorted_waiting(&self, g: &Group, now: Instant) -> Vec<u64> {
        let mut ids: Vec<(u64, u64)> = g
            .waiting
            .iter()
            .filter_map(|id| {
                self.entries.get(id).map(|e| {
                    let waited = now.saturating_duration_since(e.enqueued);
                    (effective_rank(e.priority, waited), *id)
                })
            })
            .collect();
        ids.sort_unstable();
        ids.into_iter().map(|(_, id)| id).collect()
    }

    /// Выдача в одной группе (алгоритм — в документации модуля).
    fn dispatch(&mut self, gid: &GroupId, admission: Option<&Admission>, now: Instant) {
        let today = (self.today)();
        let Some(mut g) = self.groups.remove(gid) else {
            return;
        };
        g.roll_day(today);
        if g.cooldown_left(now).is_none() {
            g.cooldown_until = None;
        }

        // Истёкший `max_wait` (D12) и исчерпанный бюджет (D14) — отказ сразу. Без фоновой
        // задачи `max_wait` не сработает — отказ `RuntimeStopped`.
        let exhausted = g.exhausted(today);
        let dead = self.driver_dead;
        let entries = &mut self.entries;
        g.waiting.retain(|id| {
            let Some(e) = entries.get_mut(id) else {
                return false;
            };
            let err = if exhausted {
                Some(QueueError::BudgetExhausted { group: gid.clone() })
            } else if e.deadline.is_some_and(|d| d <= now) {
                Some(QueueError::WaitTimeout)
            } else if dead && e.deadline.is_some() {
                Some(QueueError::RuntimeStopped)
            } else {
                None
            };
            match err {
                Some(err) => {
                    e.fail(err, now);
                    false
                }
                None => true,
            }
        });

        let order = self.sorted_waiting(&g, now);
        let cooldown = g.cooldown_left(now);
        let n = g.limits.max_parallel;
        let bg_limit = n.saturating_sub(1).max(1);
        let need = Need {
            local_gpu: true,
            vram_mib: 0,
            ram_mib: 0,
        };
        // Решение допуска зависит только от класса: одна проверка на класс за пересчёт.
        let mut verdicts: HashMap<Priority, Verdict> = HashMap::new();
        let mut granted = Vec::new();
        let mut failed: Vec<(u64, QueueError)> = Vec::new();
        let mut states: HashMap<u64, WaitState> = HashMap::new();
        for &id in &order {
            let Some(e) = self.entries.get(&id) else {
                continue;
            };
            if let Some(left) = cooldown {
                if dead {
                    failed.push((id, QueueError::RuntimeStopped));
                } else {
                    let remaining_ms = millis(left);
                    states.insert(id, WaitState::Cooldown { remaining_ms });
                }
                continue;
            }
            if g.limits.resource_gated
                && let Some(adm) = admission
            {
                let v = *verdicts
                    .entry(e.priority)
                    .or_insert_with(|| adm.check(e.priority, &need));
                if let Verdict::Wait { kind, .. } = v {
                    if self.monitor_stopped {
                        failed.push((id, QueueError::MonitorStopped));
                    } else if dead {
                        failed.push((id, QueueError::RuntimeStopped));
                    } else {
                        let kind = kind.as_str().into();
                        states.insert(id, WaitState::WaitingResource { kind });
                    }
                    continue;
                }
            }
            if g.active >= n
                || (g.limits.kind == PoolKind::Local && e.background() && g.active_bg >= bg_limit)
            {
                states.insert(id, WaitState::Queued);
                continue;
            }
            g.active += 1;
            if e.background() {
                g.active_bg += 1;
            }
            granted.push(id);
        }

        for id in &granted {
            if let Some(e) = self.entries.get_mut(id) {
                e.phase = Phase::Granted;
                e.set_status(0, WaitState::Granted, now);
                e.wake.notify_one();
                tracing::debug!(group = %gid, run = %e.run, priority = ?e.priority, "место выдано");
            }
        }
        for (id, err) in &failed {
            if let Some(e) = self.entries.get_mut(id) {
                e.fail(err.clone(), now);
            }
        }
        g.waiting
            .retain(|id| !granted.contains(id) && !failed.iter().any(|(f, _)| f == id));

        let mut position = 0u32;
        for id in order {
            let Some(state) = states.remove(&id) else {
                continue;
            };
            if let Some(e) = self.entries.get_mut(&id) {
                position += 1;
                e.set_status(position, state, now);
            }
        }
        self.groups.insert(gid.clone(), g);
    }

    fn dispatch_all(&mut self, admission: Option<&Admission>, now: Instant) {
        let ids: Vec<GroupId> = self.groups.keys().cloned().collect();
        for gid in ids {
            self.dispatch(&gid, admission, now);
        }
    }

    /// Пересчёт по новому снимку ресурсов: только группы с допуском и ожидающими.
    fn dispatch_gated(&mut self, admission: Option<&Admission>, now: Instant) {
        let ids: Vec<GroupId> = self
            .groups
            .iter()
            .filter(|(_, g)| g.limits.resource_gated && !g.waiting.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        for gid in ids {
            self.dispatch(&gid, admission, now);
        }
    }

    /// Ближайший момент, когда нужна выдача по времени: конец cooldown, `max_wait`,
    /// следующая граница старения ожидающих (позиции).
    fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let cooldowns = self.groups.values().filter_map(|g| g.cooldown_until);
        let waits = self
            .entries
            .values()
            .filter(|e| matches!(e.phase, Phase::Waiting))
            .flat_map(|e| {
                let waited = now.saturating_duration_since(e.enqueued);
                let aging = (effective_rank(e.priority, waited)
                    > u64::from(e.priority.rank().min(1)))
                .then(|| {
                    let steps = waited.as_secs() / AGING_STEP.as_secs() + 1;
                    let step = u32::try_from(steps).unwrap_or(u32::MAX);
                    e.enqueued + AGING_STEP.saturating_mul(step)
                });
                [e.deadline, aging]
            })
            .flatten();
        cooldowns.chain(waits).min()
    }
}

struct Shared {
    state: Mutex<State>,
    admission: Option<Admission>,
    /// Будит фоновую задачу очереди: изменились сроки.
    driver_wake: Arc<Notify>,
    /// Runtime фоновой задачи.
    rt: Handle,
    /// Фоновая задача; `None` — не запущена или runtime остановлен.
    driver: Mutex<Option<JoinHandle<()>>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Изменения состояния не паникуют посередине; после паники держателя оно согласовано.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn dispatch(&self, st: &mut State, gid: &GroupId) {
        st.dispatch(gid, self.admission.as_ref(), Instant::now());
    }

    /// Надзор за фоновой задачей: запускает её, если она не запущена или завершилась.
    /// Порядок замков: `driver`, затем `state` (обратного нигде нет).
    fn ensure_driver(self: &Arc<Self>) {
        let mut d = self.driver.lock().unwrap_or_else(|e| e.into_inner());
        if d.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        let (dead, monitor_stopped) = {
            let st = self.lock();
            (st.driver_dead, st.monitor_stopped)
        };
        if dead {
            return;
        }
        let restart = d.take().is_some();
        let snapshots = if monitor_stopped {
            None
        } else {
            self.admission.as_ref().map(Admission::changed)
        };
        let h = self.rt.spawn(drive(
            Arc::downgrade(self),
            self.driver_wake.clone(),
            snapshots,
        ));
        if h.is_finished() {
            // Задача, порождённая в остановленном runtime, сразу отменена.
            drop(d);
            tracing::warn!(
                "runtime очереди остановлен: заявки с таймерами и ожиданием ресурса завершаются"
            );
            let mut st = self.lock();
            st.driver_dead = true;
            st.dispatch_all(self.admission.as_ref(), Instant::now());
            return;
        }
        if restart {
            tracing::warn!("фоновая задача очереди завершилась, перезапущена");
        }
        *d = Some(h);
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Фоновая задача держит `Weak` и завершится, не получив `Arc`.
        self.driver_wake.notify_one();
    }
}

/// Причина пробуждения фоновой задачи.
enum Wake {
    /// Событие очереди или срок таймера: пересчёт всех групп.
    All,
    /// Новый снимок ресурсов: пересчёт групп с допуском и ожидающими.
    Snapshot,
    /// Монитор ресурсов остановлен.
    MonitorStopped,
}

/// Фоновая задача очереди: таймеры и снимки ресурсов. Первый проход — сразу (после
/// перезапуска нужно догнать пропущенные сроки и снимки).
async fn drive(
    weak: Weak<Shared>,
    wake: Arc<Notify>,
    mut snapshots: Option<watch::Receiver<ResourceSnapshot>>,
) {
    let mut reason = Wake::All;
    loop {
        let next = {
            let Some(s) = weak.upgrade() else { return };
            let mut st = s.lock();
            let now = Instant::now();
            match reason {
                Wake::All => st.dispatch_all(s.admission.as_ref(), now),
                Wake::Snapshot => st.dispatch_gated(s.admission.as_ref(), now),
                Wake::MonitorStopped => {
                    tracing::warn!(
                        "монитор ресурсов остановлен: заявки, ждущие ресурса, завершаются"
                    );
                    st.monitor_stopped = true;
                    st.dispatch_all(s.admission.as_ref(), now);
                }
            }
            st.next_deadline(Instant::now())
        };
        reason = tokio::select! {
            _ = wake.notified() => Wake::All,
            _ = tokio::time::sleep_until(next.unwrap_or_else(Instant::now)), if next.is_some() => Wake::All,
            r = async {
                match snapshots.as_mut() {
                    Some(rx) => rx.changed().await,
                    None => std::future::pending().await,
                }
            }, if snapshots.is_some() => match r {
                Ok(()) => Wake::Snapshot,
                Err(_) => {
                    snapshots = None;
                    Wake::MonitorStopped
                }
            },
        };
    }
}

/// Очередь к моделям. Clone дешёвый: состояние общее.
#[derive(Clone)]
pub struct ModelQueue {
    shared: Arc<Shared>,
}

impl ModelQueue {
    /// `rt` — runtime фоновой задачи очереди (таймеры, снимки ресурсов); `admission` —
    /// допуск для групп с `resource_gated`, `None` — без проверки ресурсов.
    ///
    /// Runtime `rt` должен опрашиваться (работать `block_on` или рабочие потоки): фоновая
    /// задача только порождается в нём. Если runtime жив, но не опрашивается (например,
    /// `current_thread` без `block_on`), cooldown, `max_wait`, старение позиций и
    /// возобновление по ресурсам не срабатывают, хотя синхронная выдача работает.
    pub fn new(rt: Handle, admission: Option<Admission>) -> Self {
        let q = Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    groups: BTreeMap::new(),
                    entries: HashMap::new(),
                    next_id: 0,
                    monitor_stopped: false,
                    driver_dead: false,
                    today: Arc::new(utc_today),
                }),
                admission,
                driver_wake: Arc::new(Notify::new()),
                rt,
                driver: Mutex::new(None),
            }),
        };
        q.shared.ensure_driver();
        q
    }

    /// Источник текущей даты для дневного бюджета (тесты); по умолчанию — UTC-сутки (D14).
    pub fn with_today(self, today: Arc<dyn Fn() -> jiff::civil::Date + Send + Sync>) -> Self {
        self.shared.lock().today = today;
        self
    }

    fn wake_driver(&self) {
        self.shared.ensure_driver();
        self.shared.driver_wake.notify_one();
    }

    /// Создаёт группу или меняет её пределы (места, ожидающие и траты сохраняются).
    pub fn configure(&self, g: GroupId, limits: GroupLimits) {
        let mut st = self.shared.lock();
        match st.groups.get_mut(&g) {
            Some(group) => group.limits = limits,
            None => {
                st.groups.insert(g.clone(), Group::new(limits));
            }
        }
        self.shared.dispatch(&mut st, &g);
        drop(st);
        self.wake_driver();
    }

    /// Новое число мест группы (D8: слоты сервера после зонда). Уже выданные места не
    /// отзываются.
    pub fn set_parallel(&self, g: &GroupId, n: u32) {
        self.shared.ensure_driver();
        let mut st = self.shared.lock();
        let Some(group) = st.groups.get_mut(g) else {
            tracing::warn!(group = %g, "set_parallel: группа не настроена");
            return;
        };
        group.limits.max_parallel = n;
        self.shared.dispatch(&mut st, g);
    }

    /// Ставит заявку run в очередь группы. Выдача может произойти сразу.
    pub fn enqueue(
        &self,
        g: &GroupId,
        run: RunId,
        p: Priority,
        max_wait: Option<Duration>,
    ) -> Result<Ticket, QueueError> {
        self.shared.ensure_driver();
        let now = Instant::now();
        let mut st = self.shared.lock();
        if !st.groups.contains_key(g) {
            return Err(QueueError::UnknownGroup(g.clone()));
        }
        let id = st.next_id;
        st.next_id += 1;
        let (tx, rx) = watch::channel(QueueStatus {
            position: 0,
            waited_ms: 0,
            state: WaitState::Queued,
        });
        let wake = Arc::new(Notify::new());
        st.entries.insert(
            id,
            Entry {
                group: g.clone(),
                run,
                priority: p,
                enqueued: now,
                deadline: max_wait.map(|w| now + w),
                phase: Phase::Waiting,
                state: WaitState::Queued,
                status: tx,
                wake: wake.clone(),
            },
        );
        if let Some(group) = st.groups.get_mut(g) {
            group.waiting.push(id);
        }
        self.shared.dispatch(&mut st, g);
        drop(st);
        self.shared.driver_wake.notify_one();
        Ok(Ticket {
            shared: self.shared.clone(),
            id,
            status: rx,
            wake,
            done: false,
        })
    }

    /// Снимает все незабранные заявки run (ожидающие и выданные, но не забранные
    /// `granted()`): они получают [`QueueError::Cancelled`] (RES-003). Возвращает их число.
    pub fn cancel_run(&self, run: RunId) -> usize {
        self.shared.ensure_driver();
        let now = Instant::now();
        let mut st = self.shared.lock();
        let mut touched: Vec<GroupId> = Vec::new();
        let mut count = 0;
        let State {
            groups, entries, ..
        } = &mut *st;
        for (id, e) in entries.iter_mut() {
            if e.run != run {
                continue;
            }
            let Some(group) = groups.get_mut(&e.group) else {
                continue;
            };
            match e.phase {
                Phase::Waiting => group.waiting.retain(|w| w != id),
                Phase::Granted => group.release(e.background()),
                Phase::Failed(_) => continue,
            }
            e.fail(QueueError::Cancelled, now);
            count += 1;
            if !touched.contains(&e.group) {
                touched.push(e.group.clone());
            }
        }
        for g in &touched {
            self.shared.dispatch(&mut st, g);
        }
        count
    }

    /// Пауза выдачи в группе до `until` (429, исчерпанный лимит). Более ранний срок не
    /// сокращает текущий.
    pub fn cooldown(&self, g: &GroupId, until: Instant) {
        let mut st = self.shared.lock();
        let Some(group) = st.groups.get_mut(g) else {
            tracing::warn!(group = %g, "cooldown: группа не настроена");
            return;
        };
        group.cooldown_until = Some(group.cooldown_until.map_or(until, |u| u.max(until)));
        self.shared.dispatch(&mut st, g);
        drop(st);
        self.wake_driver();
    }

    /// Учитывает расход группы за текущие UTC-сутки (D14). Отрицательное и нечисловое
    /// значение игнорируется с предупреждением.
    pub fn record_spend(&self, g: &GroupId, usd: f64) {
        if !usd.is_finite() || usd < 0.0 {
            tracing::warn!(group = %g, usd, "record_spend: недопустимая сумма");
            return;
        }
        self.shared.ensure_driver();
        let mut st = self.shared.lock();
        let today = (st.today)();
        let Some(group) = st.groups.get_mut(g) else {
            tracing::warn!(group = %g, "record_spend: группа не настроена");
            return;
        };
        group.roll_day(today);
        group.spent += usd;
        self.shared.dispatch(&mut st, g);
    }

    /// Снимок групп и ожидающих (позиции и ожидание — на момент вызова).
    pub fn snapshot(&self) -> QueueSnapshot {
        let now = Instant::now();
        let st = self.shared.lock();
        let today = (st.today)();
        let groups = st
            .groups
            .iter()
            .map(|(id, g)| {
                let waiting = st
                    .sorted_waiting(g, now)
                    .into_iter()
                    .filter_map(|wid| st.entries.get(&wid))
                    .enumerate()
                    .map(|(i, e)| WaitingView {
                        run_id: e.run,
                        priority: e.priority,
                        position: u32::try_from(i + 1).unwrap_or(u32::MAX),
                        waited_ms: millis(now.saturating_duration_since(e.enqueued)),
                        state: e.state.clone(),
                    })
                    .collect();
                GroupView {
                    id: id.clone(),
                    kind: g.limits.kind,
                    max_parallel: g.limits.max_parallel,
                    active: g.active,
                    cooldown_ms: g.cooldown_left(now).map(millis),
                    spent_today_usd: g.spent_today(today),
                    waiting,
                }
            })
            .collect();
        QueueSnapshot { groups }
    }
}

/// Заявка в очереди. Drop до выдачи снимает её; выданное, но не забранное место
/// возвращается.
pub struct Ticket {
    shared: Arc<Shared>,
    id: u64,
    status: watch::Receiver<QueueStatus>,
    wake: Arc<Notify>,
    /// Заявка забрана `granted()`; Drop ничего не делает.
    done: bool,
}

impl Ticket {
    /// Положение заявки (RES-006): уведомление при смене позиции или состояния;
    /// `waited_ms` обновляется при каждом пересчёте очереди молча. Провал заявки —
    /// `WaitState::Failed { reason }`.
    ///
    /// Очередь пишет в этот `watch` под своим замком. Не держите `borrow()` (`watch::Ref`)
    /// во время вызовов очереди и долгих операций: писатель ждёт освобождения `Ref`, а
    /// вызов очереди — её замка (взаимоблокировка). Копируйте значение: `borrow().clone()`.
    pub fn status(&self) -> watch::Receiver<QueueStatus> {
        self.status.clone()
    }

    /// Ждёт места. Брошенный future снимает заявку (Drop билета).
    pub async fn granted(mut self) -> Result<Permit, QueueError> {
        loop {
            self.shared.ensure_driver();
            {
                let mut st = self.shared.lock();
                let phase = st.entries.get(&self.id).map(|e| &e.phase);
                match phase {
                    Some(Phase::Waiting) => {}
                    Some(Phase::Granted) | Some(Phase::Failed(_)) | None => {
                        let entry = st.entries.remove(&self.id);
                        drop(st);
                        self.done = true;
                        return match entry {
                            Some(Entry {
                                phase: Phase::Granted,
                                group,
                                priority,
                                ..
                            }) => Ok(Permit {
                                shared: self.shared.clone(),
                                group,
                                background: priority.is_background(),
                            }),
                            Some(Entry {
                                phase: Phase::Failed(e),
                                ..
                            }) => Err(e),
                            _ => Err(QueueError::Cancelled),
                        };
                    }
                }
            }
            self.wake.notified().await;
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut st = self.shared.lock();
        let Some(e) = st.entries.remove(&self.id) else {
            return;
        };
        let Some(group) = st.groups.get_mut(&e.group) else {
            return;
        };
        match e.phase {
            Phase::Waiting => group.waiting.retain(|w| *w != self.id),
            Phase::Granted => group.release(e.background()),
            Phase::Failed(_) => return,
        }
        self.shared.dispatch(&mut st, &e.group);
    }
}

/// Выданное место. Drop освобождает его, в том числе при панике держателя.
pub struct Permit {
    shared: Arc<Shared>,
    group: GroupId,
    background: bool,
}

impl Permit {
    pub fn group(&self) -> &GroupId {
        &self.group
    }
}

impl fmt::Debug for Permit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Permit")
            .field("group", &self.group)
            .field("background", &self.background)
            .finish()
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut st = self.shared.lock();
        if let Some(g) = st.groups.get_mut(&self.group) {
            g.release(self.background);
        }
        self.shared.dispatch(&mut st, &self.group);
        drop(st);
        // Ждущие в `granted()` не должны застревать после падения фоновой задачи до
        // следующего вызова очереди; замок `state` уже отпущен (порядок `driver` → `state`).
        self.shared.ensure_driver();
    }
}

impl fmt::Debug for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ticket").field("id", &self.id).finish()
    }
}
