//! CPU, RAM и тома через `sysinfo` (D7). Вызовы блокирующие: монитор зовёт их
//! из `spawn_blocking`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sysinfo::{Disks, System};
use tokio::time::Instant;

use crate::snapshot::{CpuSample, DataCategory, DiskKind, MemSample, VolumeSample};
use crate::volumes::volume_for;

/// Счётчик «сколько подряд CPU занят».
#[derive(Debug, Default)]
pub(crate) struct BusyTracker {
    since: Option<Instant>,
}

impl BusyTracker {
    /// Миллисекунды подряд с загрузкой не ниже `threshold` на момент `now`; ниже — 0.
    pub(crate) fn observe(&mut self, usage: f32, threshold: f32, now: Instant) -> u64 {
        if usage >= threshold {
            let since = *self.since.get_or_insert(now);
            u64::try_from(now.duration_since(since).as_millis()).unwrap_or(u64::MAX)
        } else {
            self.since = None;
            0
        }
    }
}

/// Накопленные счётчики ввода-вывода тома на момент замера.
#[derive(Debug, Clone, Copy)]
struct IoMark {
    at: Instant,
    read: u64,
    written: u64,
}

/// Состояние `sysinfo` для CPU и RAM между замерами.
///
/// Тома — отдельный [`VolumeSampler`] в отдельной задаче: медленный `statvfs` или
/// запрос к съёмному носителю не задерживает CPU и RAM.
pub(crate) struct HostSampler {
    /// `None` — платформа не поддерживается или `sysinfo` не инициализировался.
    system: Option<System>,
    busy: BusyTracker,
}

impl HostSampler {
    pub(crate) fn new() -> Self {
        let system = System::new()
            .inspect_err(|e| tracing::warn!(error = %e, "sysinfo: CPU и RAM недоступны"))
            .ok();
        Self {
            system,
            busy: BusyTracker::default(),
        }
    }

    /// Обновляет загрузку CPU. Первое значение после создания осмысленно только через
    /// `sysinfo::MINIMUM_CPU_UPDATE_INTERVAL` после предыдущего обновления.
    pub(crate) fn refresh_cpu(&mut self) {
        if let Some(s) = &mut self.system {
            s.refresh_cpu_usage();
        }
    }

    /// CPU после `refresh_cpu`.
    pub(crate) fn cpu(&mut self, busy_threshold: f32, now: Instant) -> CpuSample {
        let usage = self.system.as_ref().map_or(0.0, System::global_cpu_usage);
        let usage = if usage.is_finite() {
            usage.clamp(0.0, 100.0)
        } else {
            0.0
        };
        CpuSample {
            usage_percent: usage,
            cores: self.cores(),
            busy_ms: self.busy.observe(usage, busy_threshold, now),
        }
    }

    /// Число логических ядер; если `sysinfo` их не дал — из `available_parallelism`.
    pub(crate) fn cores(&self) -> u32 {
        let n = self.system.as_ref().map_or(0, |s| s.cpus().len());
        let n = if n == 0 {
            std::thread::available_parallelism().map_or(1, |n| n.get())
        } else {
            n
        };
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    pub(crate) fn memory(&mut self) -> MemSample {
        match &mut self.system {
            Some(s) => {
                s.refresh_memory();
                let total = s.total_memory();
                MemSample {
                    total,
                    available: s.available_memory().min(total),
                }
            }
            None => MemSample {
                total: 0,
                available: 0,
            },
        }
    }
}

/// Источник замеров томов; в рабочем коде — [`VolumeSampler`], в тестах — подмена.
pub(crate) trait VolumeProbe: Send + 'static {
    fn sample(&mut self, categories: &[(DataCategory, PathBuf)], now: Instant)
    -> Vec<VolumeSample>;
}

/// Состояние `sysinfo` для томов: список дисков и накопленные счётчики ввода-вывода.
pub(crate) struct VolumeSampler {
    disks: Option<Disks>,
    io: HashMap<PathBuf, IoMark>,
}

impl VolumeSampler {
    pub(crate) fn new() -> Self {
        let disks = Disks::new()
            .inspect_err(|e| tracing::warn!(error = %e, "sysinfo: тома недоступны"))
            .ok();
        Self {
            disks,
            io: HashMap::new(),
        }
    }
}

impl VolumeProbe for VolumeSampler {
    fn sample(
        &mut self,
        categories: &[(DataCategory, PathBuf)],
        now: Instant,
    ) -> Vec<VolumeSample> {
        self.volumes(categories, now)
    }
}

impl VolumeSampler {
    /// Тома со скоростью ввода-вывода — разность накопленных счётчиков за прошедшее время.
    /// `roles` — категории данных, лежащие на томе (по самой длинной точке монтирования).
    pub(crate) fn volumes(
        &mut self,
        categories: &[(DataCategory, PathBuf)],
        now: Instant,
    ) -> Vec<VolumeSample> {
        let Some(disks) = &mut self.disks else {
            return Vec::new();
        };
        disks.refresh(true);
        let mounts: Vec<PathBuf> = disks
            .iter()
            .filter(|d| d.total_space() > 0)
            .map(|d| d.mount_point().to_path_buf())
            .collect();
        let mut roles: HashMap<&Path, Vec<DataCategory>> = HashMap::new();
        for (cat, path) in categories {
            // Ссылка или junction (модели на другом диске) — том цели, а не ссылки.
            let abs = std::fs::canonicalize(path)
                .or_else(|_| std::path::absolute(path))
                .unwrap_or_else(|_| path.clone());
            if let Some(m) = volume_for(&abs, &mounts) {
                let r = roles.entry(m.as_path()).or_default();
                if !r.contains(cat) {
                    r.push(*cat);
                }
            }
        }
        let mut out = Vec::new();
        let mut io = HashMap::new();
        for d in disks.iter().filter(|d| d.total_space() > 0) {
            let mount = d.mount_point().to_path_buf();
            let usage = d.usage();
            let mark = IoMark {
                at: now,
                read: usage.total_read_bytes,
                written: usage.total_written_bytes,
            };
            // Нули накопленных счётчиков — платформа их не даёт (D7).
            let supported = mark.read > 0 || mark.written > 0;
            let (read_bps, write_bps) = match self.io.get(&mount) {
                Some(prev) if supported => (
                    rate(prev.read, mark.read, prev.at, now),
                    rate(prev.written, mark.written, prev.at, now),
                ),
                _ => (None, None),
            };
            if supported {
                io.insert(mount.clone(), mark);
            }
            out.push(VolumeSample {
                name: d.name().to_string_lossy().into_owned(),
                kind: match d.kind() {
                    sysinfo::DiskKind::HDD => DiskKind::Hdd,
                    sysinfo::DiskKind::SSD => DiskKind::Ssd,
                    sysinfo::DiskKind::Unknown(_) => DiskKind::Unknown,
                },
                total: d.total_space(),
                available: d.available_space().min(d.total_space()),
                read_bps,
                write_bps,
                roles: roles.get(mount.as_path()).cloned().unwrap_or_default(),
                mount,
            });
        }
        self.io = io;
        out
    }
}

/// Байт/с между двумя накопленными значениями; счётчик, пошедший назад, — `None`.
fn rate(prev: u64, cur: u64, from: Instant, to: Instant) -> Option<u64> {
    let dt = to.duration_since(from);
    if dt < Duration::from_millis(1) {
        return None;
    }
    let delta = cur.checked_sub(prev)?;
    let bps = u128::from(delta) * 1000 / dt.as_millis();
    Some(u64::try_from(bps).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_counts_consecutive_time() {
        let t = Instant::now();
        let mut b = BusyTracker::default();
        assert_eq!(b.observe(96.0, 95.0, t), 0);
        assert_eq!(b.observe(99.0, 95.0, t + Duration::from_secs(3)), 3000);
        assert_eq!(b.observe(50.0, 95.0, t + Duration::from_secs(4)), 0);
        assert_eq!(b.observe(95.0, 95.0, t + Duration::from_secs(5)), 0);
        assert_eq!(b.observe(95.0, 95.0, t + Duration::from_secs(6)), 1000);
    }

    #[test]
    fn rate_is_delta_over_time() {
        let t = Instant::now();
        assert_eq!(rate(1000, 3000, t, t + Duration::from_secs(2)), Some(1000));
        assert_eq!(rate(3000, 1000, t, t + Duration::from_secs(2)), None);
        assert_eq!(rate(0, 10, t, t), None);
    }
}
