//! Допуск заявок по порогам ресурсов (D10, D30, RES-012, RES-013, RES-018).
//!
//! Решение берётся из последнего снимка монитора. Пороги — из [`ResourcesConfig`].
//!
//! **Гистерезис.** У каждого порога (запас RAM, пол RAM, запас VRAM, пол VRAM, признак
//! общей памяти D30, том журнала) своё состояние «открыт/закрыт», общее у клонов
//! [`Admission`]. Состояние меняется только по уровню ресурса (свободно на узле), без учёта
//! потребности заявки: закрывается при уровне ниже порога, открывается при уровне не ниже
//! уровня возобновления `max(порог × (1 + hysteresis), порог + 256 МиБ)`. Заявка проверяется
//! без изменения состояния: `свободно − потребность ≥ порог` при открытом пороге и
//! `≥ уровень возобновления` при закрытом. Поэтому порядок проверок разных заявок на
//! результат не влияет. Пороги разных классов раздельны: пауза фоновых по запасу RAM не
//! поднимает пол интерактива (RES-013).
//!
//! **Устаревшие части снимка.** Возраст части считается по её монотонной отметке
//! (`*_sampled_mono`, `tokio::time::Instant`) — перевод часов на него не влияет. Если
//! монотонной отметки нет (снимок пришёл из сериализации), — по настенной; настенная
//! отметка из будущего считается устаревшей. Часть устарела, если замер старше трёх
//! периодов опроса D11 ([`CPU_RAM_STALE_AFTER`], [`GPU_STALE_AFTER`],
//! [`VOLUMES_STALE_AFTER`]) или замера не было. По устаревшей части классы, к которым
//! применяется её запас (RAM и VRAM — `Job`, `Arena`, `Worker`; CPU и том журнала — `Job`,
//! `Arena`), ждут (`Wait`, `free` = 0 для RAM, VRAM и диска); `Interactive`, `Top`,
//! `Orchestrator` эту часть не проверяют (RES-013). Состояние порогов и уведомления по
//! устаревшим данным не меняются.
//!
//! **Уведомления** (`RamCritical`, `VramExhausted`, `JournalVolumeLow`) — одно на вход
//! порога в паузу. Шлёт их допуск при проверке заявок, а не монитор: без заявок (а для
//! `VramExhausted` — без заявок к локальному GPU) уведомлений нет.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;
use za_protocol::Priority;

use crate::config::ResourcesConfig;
use crate::monitor::GPU_QUERY_TIMEOUT;
use crate::snapshot::{CategoryUsage, GpuSample, GpuStatus, ResourceSnapshot, VolumeSample};
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
/// Единицы `free` и `required`: RAM, VRAM и диск — **байты**; CPU — миллисекунды
/// непрерывной занятости (`free` — текущая `busy_ms`, `required` — предел; для CPU ждут
/// при `free ≥ required`, т. е. сравнение обратное). Для RAM, VRAM и диска `free` —
/// свободно за вычетом потребности заявки, `required` — действующий уровень: порог или,
/// пока порог закрыт, уровень возобновления. Исключения без вычета потребности: признак
/// общей памяти D30 (`free` — запас до предела `total − used` карты; он от заявки не
/// зависит) и том журнала (`free` — свободно на томе `ZA_HOME`).
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
    /// `free` — свободно на томе за вычетом резервов и ожидающих байтов, байты; `caches` —
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

/// Порог со своим гистерезисом.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Gate {
    RamReserve,
    /// Пол RAM; вход в паузу — уведомление `RamCritical`.
    RamFloor,
    VramReserve,
    VramFloor,
    VramShared,
    JournalLow,
}

#[derive(Debug, Default)]
struct HysteresisState {
    /// Закрытые пороги.
    closed: HashSet<Gate>,
    /// Предупреждение «том `ZA_HOME` не найден» уже записано; сбрасывается, когда том найден.
    warned_no_journal_volume: bool,
}

/// Порог с уровнем возобновления.
#[derive(Clone, Copy)]
struct Level {
    open_at: u64,
    resume_at: u64,
}

impl Level {
    /// Обычный порог D10: возобновление при `max(порог × (1 + h), порог + шаг)`.
    fn new(threshold: u64, hysteresis: f32, step: u64) -> Self {
        Self {
            open_at: threshold,
            resume_at: resume_level(threshold, hysteresis, step),
        }
    }
}

/// Состояние порога после обновления.
#[derive(Clone, Copy)]
struct GateState {
    closed: bool,
    /// Порог закрылся этим обновлением.
    entered: bool,
    /// Действующий уровень.
    required: u64,
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

/// Уровень возобновления (D10): `max(⌈порог × (1 + h)⌉, порог + шаг)`.
fn resume_level(threshold: u64, hysteresis: f32, step: u64) -> u64 {
    let h = f64::from(hysteresis.clamp(0.0, 1.0));
    // u64 → f64 точен до 2^53 байт (8 ПиБ) — с запасом для RAM и дисков; приведение насыщается.
    let scaled = (threshold as f64 * (1.0 + h)).ceil() as u64;
    scaled.max(threshold.saturating_add(step))
}

/// Свежесть части снимка: по монотонной отметке, иначе по настенной (из будущего — устарела).
pub(crate) fn is_fresh(
    mono: Option<Instant>,
    wall: Option<jiff::Timestamp>,
    limit: Duration,
) -> bool {
    if let Some(m) = mono {
        return Instant::now().saturating_duration_since(m) <= limit;
    }
    let Some(ts) = wall else { return false };
    let age_ms = jiff::Timestamp::now().as_millisecond() - ts.as_millisecond();
    age_ms >= 0 && u128::from(age_ms.unsigned_abs()) <= limit.as_millis()
}

/// Свободная и занятая память карты, МиБ: неизвестную половину выводим из `total`.
fn gpu_mem(g: &GpuSample) -> (Option<u64>, Option<u64>) {
    let total = g.mem_total_mib;
    let free = g
        .mem_free_mib
        .or_else(|| g.mem_used_mib.map(|u| total.saturating_sub(u)));
    let used = g
        .mem_used_mib
        .or_else(|| g.mem_free_mib.map(|f| total.saturating_sub(f)));
    (free, used)
}

impl Admission {
    /// Недопустимые значения `cfg` ([`ResourcesConfig::validate`]) заменяются умолчаниями
    /// с предупреждением: допуск не должен вставать навсегда из-за опечатки.
    pub fn new(
        rx: watch::Receiver<ResourceSnapshot>,
        cfg: ResourcesConfig,
        za_home: PathBuf,
    ) -> Self {
        let (notices, _) = broadcast::channel(NOTICE_CAPACITY);
        Self {
            rx,
            cfg: Arc::new(cfg.sanitized()),
            za_home: Arc::new(za_home),
            state: Arc::new(Mutex::new(HysteresisState::default())),
            notices,
        }
    }

    /// Решение по последнему снимку. Порядок видов при нескольких нехватках:
    /// RAM, VRAM, CPU, диск; пороги всех видов обновляются всегда.
    pub fn check(&self, p: Priority, need: &Need) -> Verdict {
        let snap = self.rx.borrow();
        let mut st = self.lock();
        let verdicts = [
            self.check_ram(&mut st, &snap, p, need),
            self.check_vram(&mut st, &snap, p, need),
            self.check_cpu(&snap, p),
            self.check_journal(&mut st, &snap, p),
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

    pub(crate) fn snapshot(&self) -> watch::Ref<'_, ResourceSnapshot> {
        self.rx.borrow()
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
        let snap = self.rx.borrow();
        let mut st = self.lock();
        self.check_journal(&mut st, &snap, p)
    }

    fn lock(&self) -> MutexGuard<'_, HysteresisState> {
        // Состояние — набор флагов; после паники держателя оно остаётся согласованным.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Обновляет порог `g` по уровню ресурса (без потребности заявки).
    fn update(st: &mut HysteresisState, g: Gate, level: u64, l: Level) -> GateState {
        let was = st.closed.contains(&g);
        let closed = if was {
            level < l.resume_at.max(l.open_at)
        } else {
            level < l.open_at
        };
        if closed {
            st.closed.insert(g);
        } else {
            st.closed.remove(&g);
        }
        GateState {
            closed,
            entered: closed && !was,
            required: if closed {
                l.resume_at.max(l.open_at)
            } else {
                l.open_at
            },
        }
    }

    fn level(&self, threshold: u64, step: u64) -> Level {
        Level::new(threshold, self.cfg.hysteresis, step)
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
        p: Priority,
        need: &Need,
    ) -> Verdict {
        let reserve_class = matches!(p, Priority::Job | Priority::Arena | Priority::Worker);
        let step = HYSTERESIS_MIN_MIB * MIB;
        let reserve = self.level(self.ram_reserve(snap.ram.total), step);
        let floor = self.level(self.cfg.ram_floor_mib.saturating_mul(MIB), step);
        if !is_fresh(
            snap.sampled_mono,
            Some(snap.sampled_at),
            CPU_RAM_STALE_AFTER,
        ) {
            return if reserve_class {
                Verdict::Wait {
                    kind: ResourceKind::Ram,
                    free: 0,
                    required: reserve.open_at,
                }
            } else {
                Verdict::Admit
            };
        }
        let available = snap.ram.available;
        let r = Self::update(st, Gate::RamReserve, available, reserve);
        let f = Self::update(st, Gate::RamFloor, available, floor);
        if f.entered {
            self.notify(ResourceNotice::RamCritical { free: available });
        }
        let gate = if reserve_class { r } else { f };
        let free = available.saturating_sub(need.ram_mib.saturating_mul(MIB));
        if free >= gate.required {
            Verdict::Admit
        } else {
            Verdict::Wait {
                kind: ResourceKind::Ram,
                free,
                required: gate.required,
            }
        }
    }

    fn check_vram(
        &self,
        st: &mut HysteresisState,
        snap: &ResourceSnapshot,
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
        let wait = |free_mib: u64, required_mib: u64| Verdict::Wait {
            kind: ResourceKind::Vram,
            free: free_mib.saturating_mul(MIB),
            required: required_mib.saturating_mul(MIB),
        };
        // D10: Interactive, Top, Orchestrator по VRAM не проверяются.
        let class_gate = match p {
            Priority::Job | Priority::Arena => Some((Gate::VramReserve, self.cfg.vram_reserve_mib)),
            Priority::Worker => Some((Gate::VramFloor, self.cfg.vram_floor_mib)),
            Priority::Interactive | Priority::Top | Priority::Orchestrator => None,
        };
        if !is_fresh(snap.gpu_sampled_mono, snap.gpu_sampled_at, GPU_STALE_AFTER) {
            return match class_gate {
                Some((_, threshold)) => wait(0, threshold),
                None => Verdict::Admit,
            };
        }
        let mems: Vec<(Option<u64>, Option<u64>, u64)> = gpus
            .iter()
            .map(|g| {
                let (free, used) = gpu_mem(g);
                (free, used, g.mem_total_mib)
            })
            .collect();

        // D30: карта с наименьшим запасом до предела (несколько GPU — консервативно).
        // Занятая память неизвестна и не выводится — признак не срабатывает.
        let mut verdict = Verdict::Admit;
        let shared = mems
            .iter()
            .filter_map(|&(_, used, total)| used.map(|u| (total.saturating_sub(u), u, total)))
            .min_by_key(|(headroom, _, _)| *headroom);
        if let Some((headroom, used_mib, total_mib)) = shared {
            // Пауза при занято ≥ всего − 256 (открыт при запасе ≥ 257); возобновление — как
            // у порога 256 МиБ: max(256 × (1 + h), 512) МиБ.
            let l = Level {
                open_at: SHARED_VRAM_MARGIN_MIB + 1,
                resume_at: resume_level(
                    SHARED_VRAM_MARGIN_MIB,
                    self.cfg.hysteresis,
                    HYSTERESIS_MIN_MIB,
                ),
            };
            let s = Self::update(st, Gate::VramShared, headroom, l);
            if s.entered {
                self.notify(ResourceNotice::VramExhausted {
                    used_mib,
                    total_mib,
                });
            }
            if s.closed && class_gate.is_some() {
                verdict = wait(headroom, s.required);
            }
        }

        // Запас и пол: карта с наименьшим свободным; свободное неизвестно — проверки нет.
        let Some(min_free) = mems.iter().filter_map(|&(free, _, _)| free).min() else {
            return verdict;
        };
        let reserve = self.level(self.cfg.vram_reserve_mib, HYSTERESIS_MIN_MIB);
        let floor = self.level(self.cfg.vram_floor_mib, HYSTERESIS_MIN_MIB);
        let r = Self::update(st, Gate::VramReserve, min_free, reserve);
        let f = Self::update(st, Gate::VramFloor, min_free, floor);
        let Some((g, _)) = class_gate else {
            return Verdict::Admit;
        };
        if verdict != Verdict::Admit {
            return verdict;
        }
        let gate = if g == Gate::VramFloor { f } else { r };
        let free = min_free.saturating_sub(need.vram_mib);
        if free >= gate.required {
            Verdict::Admit
        } else {
            wait(free, gate.required)
        }
    }

    fn check_cpu(&self, snap: &ResourceSnapshot, p: Priority) -> Verdict {
        if !p.is_background() {
            return Verdict::Admit;
        }
        let limit = self.cfg.cpu_busy_for_s.saturating_mul(1000);
        let busy = snap.cpu.busy_ms;
        let fresh = is_fresh(
            snap.sampled_mono,
            Some(snap.sampled_at),
            CPU_RAM_STALE_AFTER,
        );
        // Гистерезис по времени уже в `busy_ms`: сбрасывается в 0 при первом же спаде.
        if busy >= limit || !fresh {
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
        p: Priority,
    ) -> Verdict {
        let low = self.level(
            self.cfg.journal_low_mib.saturating_mul(MIB),
            HYSTERESIS_MIN_MIB * MIB,
        );
        if !is_fresh(
            snap.volumes_sampled_mono,
            snap.volumes_sampled_at,
            VOLUMES_STALE_AFTER,
        ) {
            return if p.is_background() {
                Verdict::Wait {
                    kind: ResourceKind::Disk,
                    free: 0,
                    required: low.open_at,
                }
            } else {
                Verdict::Admit
            };
        }
        let Some(vol) = journal_volume(&self.za_home, &snap.volumes) else {
            // Тома нет среди замеренных: мерить нечем, вечная пауза хуже (как GPU Unavailable).
            if !st.warned_no_journal_volume {
                st.warned_no_journal_volume = true;
                tracing::warn!(za_home = %self.za_home.display(), "том ZA_HOME не найден, проверка места журнала пропущена");
            }
            return Verdict::Admit;
        };
        st.warned_no_journal_volume = false;
        let s = Self::update(st, Gate::JournalLow, vol.available, low);
        if s.entered {
            self.notify(ResourceNotice::JournalVolumeLow {
                volume: vol.mount.clone(),
                free: vol.available,
            });
        }
        if s.closed && p.is_background() {
            Verdict::Wait {
                kind: ResourceKind::Disk,
                free: vol.available,
                required: s.required,
            }
        } else {
            Verdict::Admit
        }
    }
}

/// Том `ZA_HOME` среди замеренных.
fn journal_volume<'a>(za_home: &Path, volumes: &'a [VolumeSample]) -> Option<&'a VolumeSample> {
    let mounts: Vec<PathBuf> = volumes.iter().map(|v| v.mount.clone()).collect();
    let m = volume_for(za_home, &mounts)?;
    volumes.iter().find(|v| &v.mount == m)
}
