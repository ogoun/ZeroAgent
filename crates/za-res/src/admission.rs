//! Допуск заявок по порогам ресурсов (D10, D30, RES-012, RES-013, RES-018).
//!
//! Решение берётся из последнего снимка монитора. Пороги — из [`ResourcesConfig`].
//! Гистерезис и отправленные уведомления хранятся в общем состоянии клонов [`Admission`].
//!
//! **Устаревшие части снимка.** У каждой части снимка своя отметка времени. Часть считается
//! устаревшей, если её замер старше трёх периодов опроса D11 ([`CPU_RAM_STALE_AFTER`],
//! [`GPU_STALE_AFTER`], [`VOLUMES_STALE_AFTER`]) или замера ещё не было. По устаревшей
//! части классы, к которым применяется её запас (RAM и VRAM — `Job`, `Arena`, `Worker`;
//! CPU и том журнала — `Job`, `Arena`), ждут (`Wait`, `free` = 0 для RAM, VRAM и диска);
//! `Interactive`, `Top`, `Orchestrator` эту часть не проверяют (RES-013). Гистерезис
//! и уведомления по устаревшим данным не меняются. Возраст считается по часам
//! (`jiff::Timestamp::now()` против отметки снимка): отметки снимка — настенное время;
//! отметка из будущего (часы перевели назад) — возраст 0.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{broadcast, watch};
use za_protocol::Priority;

use crate::config::ResourcesConfig;
use crate::monitor::GPU_QUERY_TIMEOUT;
use crate::snapshot::{CategoryUsage, GpuStatus, ResourceSnapshot};
use crate::volumes::volume_for;

const MIB: u64 = 1 << 20;

/// Признак ухода Windows в общую память (D30): занято VRAM ≥ всего − 256 МиБ.
const SHARED_VRAM_MARGIN_MIB: u64 = 256;
/// Минимальный шаг гистерезиса (D10): возобновление не раньше, чем при пороге + 256 МиБ.
const HYSTERESIS_MIN_MIB: u64 = 256;
/// Ёмкость канала уведомлений: отстающий получатель теряет старые (`Lagged`).
const NOTICE_CAPACITY: usize = 64;

/// CPU и RAM устарели: 3 периода опроса по 1 с (D11).
pub const CPU_RAM_STALE_AFTER: Duration = Duration::from_secs(3);
/// GPU устарел: 3 периода по 2 с (D11) плюс таймаут одного вызова `nvidia-smi` (D6) —
/// зависший вызов сам превращается в `Unavailable` через 5 с.
pub const GPU_STALE_AFTER: Duration = Duration::from_secs(6 + GPU_QUERY_TIMEOUT.as_secs());
/// Тома устарели: 3 периода по 5 с (D11).
pub const VOLUMES_STALE_AFTER: Duration = Duration::from_secs(15);

/// Вид ресурса, по которому заявка ждёт.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Cpu,
    Ram,
    Vram,
    Disk,
}

impl ResourceKind {
    /// Имя для статуса ожидания: `"cpu"`, `"ram"`, `"vram"`, `"disk"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Ram => "ram",
            Self::Vram => "vram",
            Self::Disk => "disk",
        }
    }
}

/// Потребность заявки.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Need {
    /// VRAM, МиБ (учитывается при `local_gpu`).
    pub vram_mib: u64,
    /// RAM, МиБ.
    pub ram_mib: u64,
    /// Заявка к локальному эндпоинту на GPU узла.
    pub local_gpu: bool,
}

/// Решение допуска.
///
/// Единицы `free` и `required`: RAM и диск — байты, VRAM — МиБ, CPU — миллисекунды
/// непрерывной занятости (`free` — текущая `busy_ms`, `required` — предел; ждут при
/// `free ≥ required`). `required` для RAM, VRAM и диска учитывает гистерезис: после
/// паузы это уровень возобновления, а не исходный порог.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Admit,
    Wait {
        kind: ResourceKind,
        free: u64,
        required: u64,
    },
}

/// Уведомление пользователю о ресурсах. Каждое — одно на эпизод (вход в паузу).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceNotice {
    /// Заявке на запись не хватает места (RES-017). `need` — объём заявки плюс запас, байты;
    /// `free` — свободно на томе за вычетом уже зарезервированного, байты; `caches` —
    /// категории данных агента на этом томе (что можно очистить), по убыванию размера.
    DiskWait {
        volume: PathBuf,
        need: u64,
        free: u64,
        caches: Vec<CategoryUsage>,
    },
    /// Мало места на томе `ZA_HOME` (RES-018), байты.
    JournalVolumeLow { volume: PathBuf, free: u64 },
    /// VRAM на пределе, драйвер уходит в общую память (D30).
    VramExhausted { used_mib: u64, total_mib: u64 },
    /// Доступной RAM меньше аварийного пола, байты.
    RamCritical { free: u64 },
}

/// Ожидание прервано: отправитель снимков (монитор) закрыт.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("монитор ресурсов остановлен")]
pub struct MonitorStopped;

/// Порог со своим гистерезисом. У разных порогов одного ресурса состояние раздельное:
/// пауза фоновых по запасу RAM не поднимает пол для интерактива (RES-013).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Gate {
    RamReserve,
    RamFloor,
    RamCritical,
    VramReserve,
    VramFloor,
    VramShared,
    JournalLow,
}

#[derive(Debug, Default)]
struct HysteresisState {
    /// Пороги в паузе: возобновление только при свободном ≥ уровня возобновления.
    closed: HashSet<Gate>,
    /// Предупреждение «том `ZA_HOME` не найден» уже записано.
    warned_no_journal_volume: bool,
}

/// Итог проверки порога.
enum GateOutcome {
    Open,
    /// `required` — нужный сейчас уровень; `entered` — пауза началась этой проверкой.
    Closed {
        required: u64,
        entered: bool,
    },
}

/// Допуск заявок. Clone дешёвый: всё изменяемое состояние (гистерезис по порогам,
/// отправленные уведомления) — в общем `Arc<Mutex<…>>`, поэтому `check(&self)` из разных
/// клонов видит одно состояние.
#[derive(Clone)]
pub struct Admission {
    rx: watch::Receiver<ResourceSnapshot>,
    cfg: Arc<ResourcesConfig>,
    za_home: Arc<PathBuf>,
    state: Arc<Mutex<HysteresisState>>,
    notices: broadcast::Sender<ResourceNotice>,
}

/// Уровень возобновления (D10): `max(порог × (1 + h), порог + шаг)`.
fn resume_level(threshold: u64, hysteresis: f32, step: u64) -> u64 {
    let h = f64::from(hysteresis.max(0.0));
    // u64 → f64 точен до 2^53 байт (8 ПиБ) — с запасом для RAM и дисков.
    let scaled = (threshold as f64 * (1.0 + h)).ceil() as u64;
    scaled.max(threshold.saturating_add(step))
}

/// Возраст отметки по настенным часам; `None` (замера не было) — бесконечно стар.
pub(crate) fn is_fresh(ts: Option<jiff::Timestamp>, now: jiff::Timestamp, limit: Duration) -> bool {
    let Some(ts) = ts else { return false };
    let age_ms = now
        .as_millisecond()
        .saturating_sub(ts.as_millisecond())
        .max(0);
    u128::from(age_ms.unsigned_abs()) <= limit.as_millis()
}

impl Admission {
    pub fn new(
        rx: watch::Receiver<ResourceSnapshot>,
        cfg: ResourcesConfig,
        za_home: PathBuf,
    ) -> Self {
        let (notices, _) = broadcast::channel(NOTICE_CAPACITY);
        Self {
            rx,
            cfg: Arc::new(cfg),
            za_home: Arc::new(za_home),
            state: Arc::new(Mutex::new(HysteresisState::default())),
            notices,
        }
    }

    /// Решение по последнему снимку. Порядок видов при нескольких нехватках:
    /// RAM, VRAM, CPU, диск; пороги всех видов оцениваются всегда (гистерезис и
    /// уведомления не зависят от порядка).
    pub fn check(&self, p: Priority, need: &Need) -> Verdict {
        let snap = self.snapshot();
        let now = jiff::Timestamp::now();
        let mut st = self.lock();
        let verdicts = [
            self.check_ram(&mut st, &snap, now, p, need),
            self.check_vram(&mut st, &snap, now, p, need),
            self.check_cpu(&snap, now, p),
            self.check_journal(&mut st, &snap, now, p),
        ];
        verdicts
            .into_iter()
            .find(|v| *v != Verdict::Admit)
            .unwrap_or(Verdict::Admit)
    }

    /// Ждёт допуска: проверка на каждом новом снимке, без внешних вызовов (RES-012).
    pub async fn wait_admitted(&self, p: Priority, need: &Need) -> Result<(), MonitorStopped> {
        let mut rx = self.rx.clone();
        loop {
            rx.borrow_and_update();
            if self.check(p, need) == Verdict::Admit {
                return Ok(());
            }
            rx.changed().await.map_err(|_| MonitorStopped)?;
        }
    }

    /// Получатель снимков (для ожидания изменений снаружи, например в очереди).
    pub fn changed(&self) -> watch::Receiver<ResourceSnapshot> {
        self.rx.clone()
    }

    /// Подписка на уведомления.
    pub fn notices(&self) -> broadcast::Receiver<ResourceNotice> {
        self.notices.subscribe()
    }

    pub(crate) fn snapshot(&self) -> ResourceSnapshot {
        self.rx.borrow().clone()
    }

    pub(crate) fn config(&self) -> &ResourcesConfig {
        &self.cfg
    }

    pub(crate) fn notify(&self, n: ResourceNotice) {
        // Нет получателей — не ошибка.
        let _ = self.notices.send(n);
    }

    /// Решение только по тому журнала (для резерва диска фоновыми, RES-018).
    pub(crate) fn journal_verdict(&self, p: Priority) -> Verdict {
        let snap = self.snapshot();
        let now = jiff::Timestamp::now();
        let mut st = self.lock();
        self.check_journal(&mut st, &snap, now, p)
    }

    fn lock(&self) -> MutexGuard<'_, HysteresisState> {
        // Состояние — набор флагов; после паники держателя оно остаётся согласованным.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Порог `g`: открыт при `free ≥ open_at`; закрывшись, открывается при `free ≥ resume_at`.
    fn gate(
        st: &mut HysteresisState,
        g: Gate,
        free: u64,
        open_at: u64,
        resume_at: u64,
    ) -> GateOutcome {
        if st.closed.contains(&g) {
            let resume = resume_at.max(open_at);
            if free >= resume {
                st.closed.remove(&g);
                GateOutcome::Open
            } else {
                GateOutcome::Closed {
                    required: resume,
                    entered: false,
                }
            }
        } else if free >= open_at {
            GateOutcome::Open
        } else {
            st.closed.insert(g);
            GateOutcome::Closed {
                required: open_at,
                entered: true,
            }
        }
    }

    fn resume(&self, threshold: u64, step: u64) -> u64 {
        resume_level(threshold, self.cfg.hysteresis, step)
    }

    fn ram_reserve(&self, total: u64) -> u64 {
        match self.cfg.ram_reserve_mib {
            Some(m) => m.saturating_mul(MIB),
            None => (2 * 1024 * MIB).max(total / 10),
        }
    }

    fn check_ram(
        &self,
        st: &mut HysteresisState,
        snap: &ResourceSnapshot,
        now: jiff::Timestamp,
        p: Priority,
        need: &Need,
    ) -> Verdict {
        let reserve_class = matches!(p, Priority::Job | Priority::Arena | Priority::Worker);
        let reserve = self.ram_reserve(snap.ram.total);
        let floor = self.cfg.ram_floor_mib.saturating_mul(MIB);
        let step = HYSTERESIS_MIN_MIB * MIB;
        if !is_fresh(Some(snap.sampled_at), now, CPU_RAM_STALE_AFTER) {
            return if reserve_class {
                Verdict::Wait {
                    kind: ResourceKind::Ram,
                    free: 0,
                    required: reserve,
                }
            } else {
                Verdict::Admit
            };
        }
        let available = snap.ram.available;
        if let GateOutcome::Closed { entered: true, .. } = Self::gate(
            st,
            Gate::RamCritical,
            available,
            floor,
            self.resume(floor, step),
        ) {
            self.notify(ResourceNotice::RamCritical { free: available });
        }
        let free = available.saturating_sub(need.ram_mib.saturating_mul(MIB));
        let (g, threshold) = if reserve_class {
            (Gate::RamReserve, reserve)
        } else {
            (Gate::RamFloor, floor)
        };
        match Self::gate(st, g, free, threshold, self.resume(threshold, step)) {
            GateOutcome::Open => Verdict::Admit,
            GateOutcome::Closed { required, .. } => Verdict::Wait {
                kind: ResourceKind::Ram,
                free,
                required,
            },
        }
    }

    fn check_vram(
        &self,
        st: &mut HysteresisState,
        snap: &ResourceSnapshot,
        now: jiff::Timestamp,
        p: Priority,
        need: &Need,
    ) -> Verdict {
        if !need.local_gpu {
            return Verdict::Admit;
        }
        // Review Focus 4: GPU недоступен — проверки нет, допуск не блокируется.
        let GpuStatus::Available(gpus) = &snap.gpu else {
            return Verdict::Admit;
        };
        let threshold = match p {
            Priority::Job | Priority::Arena => Some(self.cfg.vram_reserve_mib),
            Priority::Worker => Some(self.cfg.vram_floor_mib),
            // D10: Interactive, Top, Orchestrator по VRAM не проверяются.
            Priority::Interactive | Priority::Top | Priority::Orchestrator => None,
        };
        if !is_fresh(snap.gpu_sampled_at, now, GPU_STALE_AFTER) {
            return match threshold {
                Some(required) => Verdict::Wait {
                    kind: ResourceKind::Vram,
                    free: 0,
                    required,
                },
                None => Verdict::Admit,
            };
        }

        // D30: карта с наименьшим запасом до предела (несколько GPU — консервативно).
        let shared = gpus
            .iter()
            .filter_map(|g| {
                g.mem_used_mib
                    .map(|u| (g.mem_total_mib.saturating_sub(u), u, g.mem_total_mib))
            })
            .min_by_key(|(headroom, _, _)| *headroom);
        let mut verdict = Verdict::Admit;
        if let Some((headroom, used_mib, total_mib)) = shared {
            // Пауза при занято ≥ всего − 256, т. е. открыт при запасе ≥ 257.
            // Возобновление — как у порога 256 МиБ: max(256 × 1,1, 512) МиБ.
            match Self::gate(
                st,
                Gate::VramShared,
                headroom,
                SHARED_VRAM_MARGIN_MIB + 1,
                self.resume(SHARED_VRAM_MARGIN_MIB, HYSTERESIS_MIN_MIB),
            ) {
                GateOutcome::Open => {}
                GateOutcome::Closed { required, entered } => {
                    if entered {
                        self.notify(ResourceNotice::VramExhausted {
                            used_mib,
                            total_mib,
                        });
                    }
                    if threshold.is_some() {
                        verdict = Verdict::Wait {
                            kind: ResourceKind::Vram,
                            free: headroom,
                            required,
                        };
                    }
                }
            }
        }

        let Some(threshold) = threshold else {
            return Verdict::Admit;
        };
        // `mem_free` = None ([N/A] под WDDM) — проверки запаса нет.
        let Some(min_free) = gpus.iter().filter_map(|g| g.mem_free_mib).min() else {
            return verdict;
        };
        let free = min_free.saturating_sub(need.vram_mib);
        let g = if p == Priority::Worker {
            Gate::VramFloor
        } else {
            Gate::VramReserve
        };
        match Self::gate(
            st,
            g,
            free,
            threshold,
            self.resume(threshold, HYSTERESIS_MIN_MIB),
        ) {
            GateOutcome::Open => verdict,
            GateOutcome::Closed { required, .. } => {
                if verdict == Verdict::Admit {
                    verdict = Verdict::Wait {
                        kind: ResourceKind::Vram,
                        free,
                        required,
                    };
                }
                verdict
            }
        }
    }

    fn check_cpu(&self, snap: &ResourceSnapshot, now: jiff::Timestamp, p: Priority) -> Verdict {
        if !p.is_background() {
            return Verdict::Admit;
        }
        let limit = self.cfg.cpu_busy_for_s.saturating_mul(1000);
        let busy = snap.cpu.busy_ms;
        // Гистерезис по времени уже в `busy_ms`: сбрасывается в 0 при первом же спаде.
        if busy >= limit || !is_fresh(Some(snap.sampled_at), now, CPU_RAM_STALE_AFTER) {
            Verdict::Wait {
                kind: ResourceKind::Cpu,
                free: busy,
                required: limit,
            }
        } else {
            Verdict::Admit
        }
    }

    fn check_journal(
        &self,
        st: &mut HysteresisState,
        snap: &ResourceSnapshot,
        now: jiff::Timestamp,
        p: Priority,
    ) -> Verdict {
        let low = self.cfg.journal_low_mib.saturating_mul(MIB);
        if !is_fresh(snap.volumes_sampled_at, now, VOLUMES_STALE_AFTER) {
            return if p.is_background() {
                Verdict::Wait {
                    kind: ResourceKind::Disk,
                    free: 0,
                    required: low,
                }
            } else {
                Verdict::Admit
            };
        }
        let mounts: Vec<PathBuf> = snap.volumes.iter().map(|v| v.mount.clone()).collect();
        let Some(vol) = journal_volume(&self.za_home, &snap.volumes, &mounts) else {
            // Тома нет среди замеренных: мерить нечем, вечная пауза хуже (как GPU Unavailable).
            if !st.warned_no_journal_volume {
                st.warned_no_journal_volume = true;
                tracing::warn!(za_home = %self.za_home.display(), "том ZA_HOME не найден, проверка места журнала пропущена");
            }
            return Verdict::Admit;
        };
        let resume = self.resume(low, HYSTERESIS_MIN_MIB * MIB);
        match Self::gate(st, Gate::JournalLow, vol.1, low, resume) {
            GateOutcome::Open => Verdict::Admit,
            GateOutcome::Closed { required, entered } => {
                if entered {
                    self.notify(ResourceNotice::JournalVolumeLow {
                        volume: vol.0.clone(),
                        free: vol.1,
                    });
                }
                if p.is_background() {
                    Verdict::Wait {
                        kind: ResourceKind::Disk,
                        free: vol.1,
                        required,
                    }
                } else {
                    Verdict::Admit
                }
            }
        }
    }
}

/// Том `ZA_HOME`: точка монтирования и свободное место.
fn journal_volume<'a>(
    za_home: &Path,
    volumes: &'a [crate::snapshot::VolumeSample],
    mounts: &[PathBuf],
) -> Option<(&'a PathBuf, u64)> {
    let m = volume_for(za_home, mounts)?;
    volumes
        .iter()
        .find(|v| &v.mount == m)
        .map(|v| (&v.mount, v.available))
}
