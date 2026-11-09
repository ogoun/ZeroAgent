//! Фоновый опрос ресурсов: три задачи (CPU, RAM и тома; GPU; категории данных)
//! пишут в общий `watch`-канал снимков. Drop монитора отменяет опрос.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::config::MonitorConfig;
use crate::gpu::{GPU_NOT_POLLED, next_gpu_poll, query_nvidia_smi};
use crate::host::HostSampler;
use crate::snapshot::{CpuSample, DataCategory, GpuStatus, ResourceSnapshot};
use crate::volumes::{CATEGORY_MAX_ENTRIES, default_category_paths, measure_categories};

/// Таймаут одного вызова `nvidia-smi` (D6).
pub const GPU_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Монитор ресурсов узла. Drop отменяет все задачи опроса; после этого получатели
/// снимков видят закрытый канал.
pub struct ResourceMonitor {
    tx: watch::Sender<ResourceSnapshot>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for ResourceMonitor {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Пути категорий монитора: умолчания плюс `Models`, если каталог моделей задан.
fn category_paths(cfg: &MonitorConfig) -> Vec<(DataCategory, PathBuf)> {
    let mut paths = default_category_paths(&cfg.za_home, &cfg.projects);
    if let Some(m) = &cfg.models_dir {
        paths.insert(1, (DataCategory::Models, m.clone()));
    }
    paths
}

impl ResourceMonitor {
    /// Запускает опрос. Вызывается внутри рантайма tokio.
    ///
    /// Создаёт временный каталог монитора `ZA_HOME/work/tmp/monitor` (D33) и сразу
    /// публикует начальный снимок с RAM и томами (CPU — 0 % до первого периода,
    /// GPU — `Unavailable { "не опрашивался" }`, категорий ещё нет).
    pub fn start(cfg: MonitorConfig) -> (Self, watch::Receiver<ResourceSnapshot>) {
        let tmp = cfg.tmp_dir();
        if let Err(e) = std::fs::create_dir_all(&tmp) {
            tracing::warn!(dir = %tmp.display(), error = %e, "не создан временный каталог монитора");
        }
        let categories = Arc::new(category_paths(&cfg));

        let mut sampler = HostSampler::new();
        sampler.refresh_cpu();
        let now = Instant::now();
        let initial = ResourceSnapshot {
            sampled_at: jiff::Timestamp::now(),
            cpu: CpuSample {
                usage_percent: 0.0,
                cores: sampler.cores(),
                busy_ms: 0,
            },
            ram: sampler.memory(),
            gpu: GpuStatus::Unavailable {
                reason: GPU_NOT_POLLED.into(),
            },
            volumes: sampler.volumes(&categories, now),
            categories: Vec::new(),
        };
        let (tx, rx) = watch::channel(initial);
        let tasks = vec![
            tokio::spawn(host_loop(
                tx.clone(),
                sampler,
                cfg.clone(),
                categories.clone(),
            )),
            tokio::spawn(gpu_loop(tx.clone(), cfg.clone(), tmp)),
            tokio::spawn(categories_loop(
                tx.clone(),
                cfg.categories_every,
                categories,
            )),
        ];
        (Self { tx, tasks }, rx)
    }

    /// Последний опубликованный снимок.
    pub fn latest(&self) -> ResourceSnapshot {
        self.tx.borrow().clone()
    }
}

fn ticker(every: Duration) -> tokio::time::Interval {
    // Первый замер — через период: базовая точка CPU и томов снята в `start`.
    let every = every.max(Duration::from_millis(1));
    let mut t = tokio::time::interval_at(Instant::now() + every, every);
    t.set_missed_tick_behavior(MissedTickBehavior::Delay);
    t
}

/// CPU и RAM каждые `cpu_ram_every`, тома каждые `volumes_every`.
async fn host_loop(
    tx: watch::Sender<ResourceSnapshot>,
    mut sampler: HostSampler,
    cfg: MonitorConfig,
    categories: Arc<Vec<(DataCategory, PathBuf)>>,
) {
    let mut cpu_tick = ticker(cfg.cpu_ram_every);
    let mut vol_tick = ticker(cfg.volumes_every);
    let threshold = cfg.cpu_busy_percent;
    loop {
        tokio::select! {
            _ = cpu_tick.tick() => {
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
                            snap.cpu = cpu;
                            snap.ram = ram;
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "замер CPU и RAM сорвался");
                        sampler = HostSampler::new();
                    }
                }
            }
            _ = vol_tick.tick() => {
                let cats = categories.clone();
                let res = tokio::task::spawn_blocking(move || {
                    let v = sampler.volumes(&cats, Instant::now());
                    (sampler, v)
                })
                .await;
                match res {
                    Ok((s, volumes)) => {
                        sampler = s;
                        tx.send_modify(|snap| {
                            snap.sampled_at = jiff::Timestamp::now();
                            snap.volumes = volumes;
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "замер томов сорвался");
                        sampler = HostSampler::new();
                    }
                }
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
            snap.sampled_at = jiff::Timestamp::now();
            snap.gpu = status;
        });
        tokio::time::sleep_until(next).await;
    }
}

/// Размеры категорий: первый обход сразу, затем каждые `every` (D11).
async fn categories_loop(
    tx: watch::Sender<ResourceSnapshot>,
    every: Duration,
    categories: Arc<Vec<(DataCategory, PathBuf)>>,
) {
    let mut tick = tokio::time::interval(every.max(Duration::from_millis(1)));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let cats = categories.clone();
        match tokio::task::spawn_blocking(move || measure_categories(&cats, CATEGORY_MAX_ENTRIES))
            .await
        {
            Ok(usage) => tx.send_modify(|snap| {
                snap.sampled_at = jiff::Timestamp::now();
                snap.categories = usage;
            }),
            Err(e) => tracing::warn!(error = %e, "обход категорий сорвался"),
        }
    }
}

/// Один синхронный замер для CLI и `doctor`: CPU (около 200 мс на два замера), RAM,
/// тома и категории. GPU не опрашивается: `Unavailable { "не опрашивался" }`.
pub fn host_sample_once(cfg: &MonitorConfig) -> ResourceSnapshot {
    let categories = category_paths(cfg);
    let mut sampler = HostSampler::new();
    sampler.refresh_cpu();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    sampler.refresh_cpu();
    let now = Instant::now();
    ResourceSnapshot {
        sampled_at: jiff::Timestamp::now(),
        cpu: sampler.cpu(cfg.cpu_busy_percent, now),
        ram: sampler.memory(),
        gpu: GpuStatus::Unavailable {
            reason: GPU_NOT_POLLED.into(),
        },
        volumes: sampler.volumes(&categories, now),
        categories: measure_categories(&categories, CATEGORY_MAX_ENTRIES),
    }
}
