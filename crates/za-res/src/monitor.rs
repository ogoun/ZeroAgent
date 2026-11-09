//! Фоновый опрос ресурсов: четыре независимые задачи (CPU и RAM; тома; GPU; категории
//! данных) пишут в общий `watch`-канал снимков, каждая со своей отметкой времени.
//! Зависший замер одной части не задерживает остальные. Drop монитора отменяет опрос.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::config::MonitorConfig;
use crate::gpu::{GPU_NOT_POLLED, next_gpu_poll, query_nvidia_smi};
use crate::host::{HostSampler, VolumeProbe, VolumeSampler};
use crate::snapshot::{CpuSample, DataCategory, GpuStatus, ResourceSnapshot};
use crate::volumes::{
    CATEGORY_MAX_ENTRIES, dedup_paths, default_category_paths, measure_categories,
    measure_categories_cancellable,
};

/// Таймаут одного вызова `nvidia-smi` (D6).
pub const GPU_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

type Categories = Arc<Vec<(DataCategory, PathBuf)>>;

/// Монитор ресурсов узла. Drop отменяет все задачи опроса и идущий обход категорий;
/// после этого получатели снимков видят закрытый канал.
pub struct ResourceMonitor {
    tx: watch::Sender<ResourceSnapshot>,
    tasks: Vec<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
}

impl Drop for ResourceMonitor {
    fn drop(&mut self) {
        // Блокирующий обход категорий `abort` не прерывает — он проверяет флаг.
        self.cancel.store(true, Ordering::Relaxed);
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Пути категорий монитора: умолчания плюс `Models`, если каталог моделей задан;
/// повторные пути убраны.
fn category_paths(cfg: &MonitorConfig) -> Vec<(DataCategory, PathBuf)> {
    let mut paths = default_category_paths(&cfg.za_home, &cfg.projects);
    if let Some(m) = &cfg.models_dir {
        paths.insert(1, (DataCategory::Models, m.clone()));
    }
    dedup_paths(&mut paths);
    paths
}

impl ResourceMonitor {
    /// Запускает опрос. Вызывается внутри рантайма tokio.
    ///
    /// Синхронно (на потоке вызывающего) создаёт временный каталог монитора
    /// `ZA_HOME/work/tmp/monitor` (D33) и снимает RAM — это быстрые вызовы; тома, GPU и
    /// категории опрашиваются уже в фоне. Начальный снимок: CPU 0 % до первого периода,
    /// GPU `Unavailable { "не опрашивался" }`, томов и категорий нет (их отметки — `None`).
    pub fn start(cfg: MonitorConfig) -> (Self, watch::Receiver<ResourceSnapshot>) {
        Self::start_with(cfg, Box::new(VolumeSampler::new()))
    }

    pub(crate) fn start_with(
        cfg: MonitorConfig,
        volumes: Box<dyn VolumeProbe>,
    ) -> (Self, watch::Receiver<ResourceSnapshot>) {
        let tmp = cfg.tmp_dir();
        if let Err(e) = std::fs::create_dir_all(&tmp) {
            tracing::warn!(dir = %tmp.display(), error = %e, "не создан временный каталог монитора");
        }
        let categories: Categories = Arc::new(category_paths(&cfg));
        let cancel = Arc::new(AtomicBool::new(false));

        let mut sampler = HostSampler::new();
        sampler.refresh_cpu();
        let initial = ResourceSnapshot {
            sampled_at: jiff::Timestamp::now(),
            sampled_mono: Some(Instant::now()),
            cpu: CpuSample {
                usage_percent: 0.0,
                cores: sampler.cores(),
                busy_ms: 0,
            },
            ram: sampler.memory(),
            gpu_sampled_at: None,
            gpu_sampled_mono: None,
            gpu: GpuStatus::Unavailable {
                reason: GPU_NOT_POLLED.into(),
            },
            volumes_sampled_at: None,
            volumes_sampled_mono: None,
            volumes: Vec::new(),
            categories_sampled_at: None,
            categories_sampled_mono: None,
            categories: Vec::new(),
        };
        let (tx, rx) = watch::channel(initial);
        let tasks = vec![
            tokio::spawn(cpu_ram_loop(tx.clone(), sampler, cfg.clone())),
            tokio::spawn(volumes_loop(
                tx.clone(),
                volumes,
                cfg.volumes_every,
                categories.clone(),
            )),
            tokio::spawn(gpu_loop(tx.clone(), cfg.clone(), tmp)),
            tokio::spawn(categories_loop(
                tx.clone(),
                cfg.categories_every,
                categories,
                cancel.clone(),
            )),
        ];
        (Self { tx, tasks, cancel }, rx)
    }

    /// Последний опубликованный снимок.
    pub fn latest(&self) -> ResourceSnapshot {
        self.tx.borrow().clone()
    }
}

/// Интервал без тиков-догонялок; `first_now` — первый тик сразу, иначе через период.
fn ticker(every: Duration, first_now: bool) -> tokio::time::Interval {
    let every = every.max(Duration::from_millis(1));
    let start = if first_now {
        Instant::now()
    } else {
        Instant::now() + every
    };
    let mut t = tokio::time::interval_at(start, every);
    t.set_missed_tick_behavior(MissedTickBehavior::Delay);
    t
}

/// CPU и RAM каждые `cpu_ram_every`. Первый замер — через период: базовая точка CPU
/// снята в `start`.
async fn cpu_ram_loop(
    tx: watch::Sender<ResourceSnapshot>,
    mut sampler: HostSampler,
    cfg: MonitorConfig,
) {
    let mut tick = ticker(cfg.cpu_ram_every, false);
    let threshold = cfg.cpu_busy_percent;
    loop {
        tick.tick().await;
        let res = tokio::task::spawn_blocking(move || {
            sampler.refresh_cpu();
            let cpu = sampler.cpu(threshold, Instant::now());
            let ram = sampler.memory();
            (sampler, cpu, ram)
        })
        .await;
        match res {
            Ok((s, cpu, ram)) => {
                sampler = s;
                tx.send_modify(|snap| {
                    snap.sampled_at = jiff::Timestamp::now();
                    snap.sampled_mono = Some(Instant::now());
                    snap.cpu = cpu;
                    snap.ram = ram;
                });
            }
            Err(e) => {
                // Сэмплер ушёл с паникой: новый теряет базу CPU (первый замер ≈ 0 %)
                // и сбрасывает `busy_ms`.
                tracing::warn!(error = %e, "замер CPU и RAM сорвался");
                sampler = HostSampler::new();
            }
        }
    }
}

/// Тома: первый замер сразу, затем каждые `every`. Отдельная задача: медленный
/// `statvfs` (зависшее FUSE-монтирование, пробуждение HDD) задерживает только тома,
/// а устаревание видно по `volumes_sampled_at`.
async fn volumes_loop(
    tx: watch::Sender<ResourceSnapshot>,
    mut probe: Box<dyn VolumeProbe>,
    every: Duration,
    categories: Categories,
) {
    let mut tick = ticker(every, true);
    loop {
        tick.tick().await;
        let cats = categories.clone();
        // Монотонная отметка — начало замера: замер отражает только то, что записано до него
        // (по ней резерв диска снимает ожидающие байты).
        let started = Instant::now();
        let res = tokio::task::spawn_blocking(move || {
            let v = probe.sample(&cats, Instant::now());
            (probe, v)
        })
        .await;
        match res {
            Ok((p, volumes)) => {
                probe = p;
                tx.send_modify(|snap| {
                    snap.volumes_sampled_at = Some(jiff::Timestamp::now());
                    snap.volumes_sampled_mono = Some(started);
                    snap.volumes = volumes;
                });
            }
            Err(e) => {
                // Сэмплер ушёл с паникой: новый начинает счётчики ввода-вывода заново.
                tracing::warn!(error = %e, "замер томов сорвался");
                probe = Box::new(VolumeSampler::new());
            }
        }
    }
}

/// GPU: следующий замер по `next_gpu_poll` (доступен — `gpu_every`, нет — 60 с).
/// Смена доступности пишется в лог один раз, а не на каждом замере.
async fn gpu_loop(tx: watch::Sender<ResourceSnapshot>, cfg: MonitorConfig, tmp: PathBuf) {
    let mut was_available: Option<bool> = None;
    loop {
        let started = Instant::now();
        let status = query_nvidia_smi(&cfg.nvidia_smi, GPU_QUERY_TIMEOUT, &tmp).await;
        let available = matches!(status, GpuStatus::Available(_));
        if was_available != Some(available) {
            match &status {
                GpuStatus::Available(g) => tracing::info!(gpus = g.len(), "GPU доступен"),
                GpuStatus::Unavailable { reason } => {
                    tracing::info!(%reason, "GPU недоступен, повтор через 60 с");
                }
            }
            was_available = Some(available);
        }
        let next = next_gpu_poll(started, &status, cfg.gpu_every);
        tx.send_modify(|snap| {
            snap.gpu_sampled_at = Some(jiff::Timestamp::now());
            snap.gpu_sampled_mono = Some(Instant::now());
            snap.gpu = status;
        });
        tokio::time::sleep_until(next).await;
    }
}

/// Размеры категорий: первый обход сразу, затем каждые `every` (D11). Обход
/// прерывается флагом `cancel` (drop монитора).
async fn categories_loop(
    tx: watch::Sender<ResourceSnapshot>,
    every: Duration,
    categories: Categories,
    cancel: Arc<AtomicBool>,
) {
    let mut tick = ticker(every, true);
    loop {
        tick.tick().await;
        let cats = categories.clone();
        let flag = cancel.clone();
        let res = tokio::task::spawn_blocking(move || {
            measure_categories_cancellable(&cats, CATEGORY_MAX_ENTRIES, &flag)
        })
        .await;
        match res {
            Ok(Some(usage)) => tx.send_modify(|snap| {
                snap.categories_sampled_at = Some(jiff::Timestamp::now());
                snap.categories_sampled_mono = Some(Instant::now());
                snap.categories = usage;
            }),
            Ok(None) => return,
            Err(e) => tracing::warn!(error = %e, "обход категорий сорвался"),
        }
    }
}

/// Один синхронный замер для CLI и `doctor`: CPU (около 200 мс на два замера), RAM,
/// тома и категории (полный обход до 200 000 записей на категорию). GPU не опрашивается:
/// `Unavailable { "не опрашивался" }`. Блокирует поток: из async-кода — через
/// `tokio::task::spawn_blocking`.
pub fn host_sample_once(cfg: &MonitorConfig) -> ResourceSnapshot {
    let categories = category_paths(cfg);
    let mut sampler = HostSampler::new();
    sampler.refresh_cpu();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    sampler.refresh_cpu();
    let now = Instant::now();
    let cpu = sampler.cpu(cfg.cpu_busy_percent, now);
    let ram = sampler.memory();
    let sampled_at = jiff::Timestamp::now();
    let sampled_mono = Some(Instant::now());
    let volumes_sampled_mono = Some(Instant::now());
    let volumes = VolumeSampler::new().sample(&categories, now);
    let volumes_sampled_at = Some(jiff::Timestamp::now());
    let categories = measure_categories(&categories, CATEGORY_MAX_ENTRIES);
    ResourceSnapshot {
        sampled_at,
        sampled_mono,
        cpu,
        ram,
        gpu_sampled_at: None,
        gpu_sampled_mono: None,
        gpu: GpuStatus::Unavailable {
            reason: GPU_NOT_POLLED.into(),
        },
        volumes_sampled_at,
        volumes_sampled_mono,
        volumes,
        categories_sampled_at: Some(jiff::Timestamp::now()),
        categories_sampled_mono: Some(Instant::now()),
        categories,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::config::ResourcesConfig;
    use crate::snapshot::VolumeSample;

    /// Первый замер томов отвечает сразу, следующие висят, пока жив отправитель `release`.
    struct HangingProbe {
        calls: usize,
        release: mpsc::Receiver<()>,
    }

    impl VolumeProbe for HangingProbe {
        fn sample(&mut self, _: &[(DataCategory, PathBuf)], _: Instant) -> Vec<VolumeSample> {
            self.calls += 1;
            if self.calls > 1 {
                let _ = self.release.recv();
            }
            Vec::new()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hung_volume_probe_does_not_stop_cpu_ram() {
        // Модульным тестам `CARGO_TARGET_TMPDIR` не задан: каталог `.tmp/` проекта.
        let home =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.tmp/za-res-hung-volumes");
        let mut cfg =
            MonitorConfig::from_resources(&ResourcesConfig::default(), home, vec![], None);
        cfg.nvidia_smi = PathBuf::from("нет-такого-nvidia-smi");
        cfg.cpu_ram_every = Duration::from_millis(30);
        cfg.volumes_every = Duration::from_millis(30);
        let (release_tx, release) = mpsc::channel();
        let (mon, mut rx) =
            ResourceMonitor::start_with(cfg, Box::new(HangingProbe { calls: 0, release }));

        let wait = |rx: &mut watch::Receiver<ResourceSnapshot>| {
            let mut rx = rx.clone();
            async move {
                loop {
                    let s = rx.borrow_and_update().clone();
                    if s.volumes_sampled_at.is_some() {
                        return s;
                    }
                    rx.changed().await.unwrap();
                }
            }
        };
        let first = tokio::time::timeout(Duration::from_secs(10), wait(&mut rx))
            .await
            .expect("первый замер томов");
        let vol_at = first.volumes_sampled_at;
        // Второй замер томов висит, а CPU и RAM продолжают обновляться.
        let mut cpu_updates = 0;
        let mut last = first.sampled_at;
        tokio::time::timeout(Duration::from_secs(10), async {
            while cpu_updates < 5 {
                rx.changed().await.unwrap();
                let s = rx.borrow_and_update().clone();
                assert_eq!(s.volumes_sampled_at, vol_at, "тома висят");
                if s.sampled_at != last {
                    last = s.sampled_at;
                    cpu_updates += 1;
                }
            }
        })
        .await
        .expect("CPU и RAM обновляются при зависшем замере томов");
        assert_eq!(mon.latest().volumes_sampled_at, vol_at);
        drop(release_tx);
        drop(mon);
    }
}
