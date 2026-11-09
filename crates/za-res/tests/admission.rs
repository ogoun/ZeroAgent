//! Допуск заявок (D10, D30, RES-012/013/017/018), гистерезис, резерв диска.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::{broadcast, watch};
use za_protocol::Priority;
use za_res::{
    Admission, CpuSample, DiskKind, DiskReservations, GpuSample, GpuStatus, MemSample,
    MonitorStopped, Need, ResourceKind, ResourceNotice, ResourceSnapshot, ResourcesConfig, Verdict,
    VolumeSample,
};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

fn mount() -> PathBuf {
    PathBuf::from("/data")
}

fn za_home() -> PathBuf {
    mount().join("za")
}

fn volume(available: u64) -> VolumeSample {
    VolumeSample {
        mount: mount(),
        name: "data".into(),
        kind: DiskKind::Ssd,
        total: 1000 * GIB,
        available,
        read_bps: None,
        write_bps: None,
        roles: vec![],
    }
}

fn gpu(total: u64, used: Option<u64>, free: Option<u64>) -> GpuStatus {
    GpuStatus::Available(vec![GpuSample {
        index: 0,
        name: "NVIDIA GeForce RTX 3090".into(),
        uuid: "GPU-1".into(),
        util_percent: Some(10),
        mem_total_mib: total,
        mem_used_mib: used,
        mem_free_mib: free,
        temp_c: Some(50),
    }])
}

/// Снимок «всего вдоволь»: RAM 32 ГиБ, GPU свободен, том 500 ГиБ, CPU не занят.
fn snap() -> ResourceSnapshot {
    let now = jiff::Timestamp::now();
    ResourceSnapshot {
        sampled_at: now,
        cpu: CpuSample {
            usage_percent: 5.0,
            cores: 16,
            busy_ms: 0,
        },
        ram: MemSample {
            total: 64 * GIB,
            available: 32 * GIB,
        },
        gpu_sampled_at: Some(now),
        gpu: gpu(24576, Some(1000), Some(23576)),
        volumes_sampled_at: Some(now),
        volumes: vec![volume(500 * GIB)],
        categories_sampled_at: None,
        categories: vec![],
    }
}

fn with_ram(available: u64) -> ResourceSnapshot {
    let mut s = snap();
    s.ram.available = available;
    s
}

fn cfg() -> ResourcesConfig {
    ResourcesConfig {
        ram_reserve_mib: Some(2048),
        ..ResourcesConfig::default()
    }
}

fn setup(s: ResourceSnapshot) -> (watch::Sender<ResourceSnapshot>, Admission) {
    let (tx, rx) = watch::channel(s);
    (tx, Admission::new(rx, cfg(), za_home()))
}

fn none() -> Need {
    Need {
        vram_mib: 0,
        ram_mib: 0,
        local_gpu: false,
    }
}

fn gpu_need() -> Need {
    Need {
        local_gpu: true,
        ..none()
    }
}

fn kind(v: Verdict) -> Option<ResourceKind> {
    match v {
        Verdict::Admit => None,
        Verdict::Wait { kind, .. } => Some(kind),
    }
}

fn drain(rx: &mut broadcast::Receiver<ResourceNotice>) -> Vec<ResourceNotice> {
    let mut out = vec![];
    while let Ok(n) = rx.try_recv() {
        out.push(n);
    }
    out
}

#[test]
fn kind_names() {
    assert_eq!(ResourceKind::Cpu.as_str(), "cpu");
    assert_eq!(ResourceKind::Ram.as_str(), "ram");
    assert_eq!(ResourceKind::Vram.as_str(), "vram");
    assert_eq!(ResourceKind::Disk.as_str(), "disk");
}

#[test]
fn ram_reserve_vs_floor() {
    let (tx, a) = setup(with_ram(3 * GIB / 2));
    assert_eq!(
        kind(a.check(Priority::Job, &none())),
        Some(ResourceKind::Ram)
    );
    assert_eq!(
        a.check(Priority::Worker, &none()),
        Verdict::Wait {
            kind: ResourceKind::Ram,
            free: 3 * GIB / 2,
            // Порог уже закрыт заявкой Job: виден уровень возобновления.
            required: 2 * GIB + 256 * MIB,
        }
    );
    assert_eq!(a.check(Priority::Interactive, &none()), Verdict::Admit);
    tx.send(with_ram(400 * MIB)).unwrap();
    assert_eq!(
        kind(a.check(Priority::Interactive, &none())),
        Some(ResourceKind::Ram)
    );
}

#[test]
fn ram_need_is_subtracted() {
    let (_tx, a) = setup(with_ram(3 * GIB));
    let need = Need {
        ram_mib: 1536,
        ..none()
    };
    // 3 ГиБ − 1,5 ГиБ < 2 ГиБ.
    assert_eq!(kind(a.check(Priority::Job, &need)), Some(ResourceKind::Ram));
    // Без заявки 3 ГиБ ≥ max(2 ГиБ × 1,1, 2 ГиБ + 256 МиБ) — допуск.
    assert_eq!(a.check(Priority::Job, &none()), Verdict::Admit);
}

#[test]
fn hysteresis_prevents_flapping() {
    let (tx, a) = setup(with_ram(19 * GIB / 10));
    assert_eq!(
        kind(a.check(Priority::Job, &none())),
        Some(ResourceKind::Ram)
    );
    tx.send(with_ram(205 * GIB / 100)).unwrap();
    assert_eq!(
        a.check(Priority::Job, &none()),
        Verdict::Wait {
            kind: ResourceKind::Ram,
            free: 205 * GIB / 100,
            required: 2 * GIB + 256 * MIB,
        }
    );
    tx.send(with_ram(23 * GIB / 10)).unwrap();
    assert_eq!(a.check(Priority::Job, &none()), Verdict::Admit);
    // Пауза снята — снова обычный порог.
    tx.send(with_ram(205 * GIB / 100)).unwrap();
    assert_eq!(a.check(Priority::Job, &none()), Verdict::Admit);
}

#[test]
fn hysteresis_is_shared_between_clones() {
    let (tx, a) = setup(with_ram(19 * GIB / 10));
    let b = a.clone();
    assert_eq!(
        kind(a.check(Priority::Job, &none())),
        Some(ResourceKind::Ram)
    );
    tx.send(with_ram(205 * GIB / 100)).unwrap();
    assert_eq!(
        kind(b.check(Priority::Job, &none())),
        Some(ResourceKind::Ram)
    );
}

#[test]
fn background_hysteresis_does_not_touch_interactive() {
    // RES-013: пауза фоновых по запасу не поднимает пол для интерактива.
    let (tx, a) = setup(with_ram(GIB));
    assert_eq!(
        kind(a.check(Priority::Job, &none())),
        Some(ResourceKind::Ram)
    );
    tx.send(with_ram(600 * MIB)).unwrap();
    assert_eq!(a.check(Priority::Interactive, &none()), Verdict::Admit);
}

#[test]
fn unknown_vram_does_not_block() {
    let mut s = snap();
    s.gpu = GpuStatus::Unavailable {
        reason: "нет nvidia-smi".into(),
    };
    let (tx, a) = setup(s);
    assert_eq!(a.check(Priority::Job, &gpu_need()), Verdict::Admit);
    assert_eq!(a.check(Priority::Worker, &gpu_need()), Verdict::Admit);
    // [N/A] под WDDM: свободная и занятая память неизвестны.
    let mut s = snap();
    s.gpu = gpu(24576, None, None);
    tx.send(s).unwrap();
    assert_eq!(a.check(Priority::Job, &gpu_need()), Verdict::Admit);
    assert_eq!(a.check(Priority::Worker, &gpu_need()), Verdict::Admit);
}

#[test]
fn sysmem_fallback_pauses_background_only() {
    let mut s = snap();
    s.gpu = gpu(24576, Some(24400), Some(176));
    let (_tx, a) = setup(s);
    let mut notices = a.notices();
    assert_eq!(
        kind(a.check(Priority::Job, &gpu_need())),
        Some(ResourceKind::Vram)
    );
    assert_eq!(
        kind(a.check(Priority::Worker, &gpu_need())),
        Some(ResourceKind::Vram)
    );
    assert_eq!(a.check(Priority::Interactive, &gpu_need()), Verdict::Admit);
    assert_eq!(a.check(Priority::Top, &gpu_need()), Verdict::Admit);
    assert_eq!(
        drain(&mut notices),
        vec![ResourceNotice::VramExhausted {
            used_mib: 24400,
            total_mib: 24576
        }]
    );
}

#[test]
fn vram_exhausted_once_per_episode() {
    let mut full = snap();
    full.gpu = gpu(24576, Some(24400), Some(176));
    let (tx, a) = setup(full.clone());
    let mut notices = a.notices();
    a.check(Priority::Job, &gpu_need());
    tx.send(full.clone()).unwrap();
    a.check(Priority::Job, &gpu_need());
    assert_eq!(drain(&mut notices).len(), 1);
    // Эпизод закончился (свободно ≥ 512 МиБ) и начался снова — второе уведомление.
    let mut ok = snap();
    ok.gpu = gpu(24576, Some(20000), Some(4576));
    tx.send(ok).unwrap();
    assert_eq!(a.check(Priority::Job, &gpu_need()), Verdict::Admit);
    tx.send(full).unwrap();
    a.check(Priority::Job, &gpu_need());
    assert_eq!(drain(&mut notices).len(), 1);
}

#[test]
fn vram_floor_applies_to_worker_only() {
    let mut s = snap();
    s.gpu = gpu(24576, Some(23876), Some(700));
    let (_tx, a) = setup(s);
    let mut notices = a.notices();
    assert_eq!(
        a.check(Priority::Job, &gpu_need()),
        Verdict::Wait {
            kind: ResourceKind::Vram,
            free: 700,
            required: 1024,
        }
    );
    assert_eq!(a.check(Priority::Worker, &gpu_need()), Verdict::Admit);
    assert_eq!(a.check(Priority::Interactive, &gpu_need()), Verdict::Admit);
    assert!(drain(&mut notices).is_empty());
}

#[test]
fn vram_ignored_without_local_gpu() {
    let mut s = snap();
    s.gpu = gpu(24576, Some(24400), Some(176));
    let (_tx, a) = setup(s);
    assert_eq!(a.check(Priority::Job, &none()), Verdict::Admit);
}

#[test]
fn cpu_busy_gates_background_only() {
    let mut s = snap();
    s.cpu.busy_ms = 12000;
    let (_tx, a) = setup(s);
    assert_eq!(
        a.check(Priority::Job, &none()),
        Verdict::Wait {
            kind: ResourceKind::Cpu,
            free: 12000,
            required: 10000,
        }
    );
    assert_eq!(a.check(Priority::Worker, &none()), Verdict::Admit);
}

#[test]
fn journal_volume_low_stops_background() {
    let mut s = snap();
    s.volumes = vec![volume(GIB)];
    let (tx, a) = setup(s.clone());
    let mut notices = a.notices();
    assert_eq!(
        kind(a.check(Priority::Arena, &none())),
        Some(ResourceKind::Disk)
    );
    assert_eq!(a.check(Priority::Top, &none()), Verdict::Admit);
    tx.send(s).unwrap();
    assert_eq!(
        kind(a.check(Priority::Job, &none())),
        Some(ResourceKind::Disk)
    );
    assert_eq!(
        drain(&mut notices),
        vec![ResourceNotice::JournalVolumeLow {
            volume: mount(),
            free: GIB
        }]
    );
}

// --- Устаревшие части снимка ---

fn ago(secs: i64) -> jiff::Timestamp {
    jiff::Timestamp::now() - jiff::SignedDuration::from_secs(secs)
}

#[test]
fn stale_cpu_ram_pauses_reserve_classes_only() {
    let mut s = snap();
    s.sampled_at = ago(60);
    let (_tx, a) = setup(s);
    assert_eq!(
        kind(a.check(Priority::Job, &none())),
        Some(ResourceKind::Ram)
    );
    assert_eq!(
        kind(a.check(Priority::Worker, &none())),
        Some(ResourceKind::Ram)
    );
    assert_eq!(a.check(Priority::Interactive, &none()), Verdict::Admit);
}

#[test]
fn stale_volumes_pause_background_only() {
    let mut s = snap();
    s.volumes_sampled_at = Some(ago(60));
    let (tx, a) = setup(s);
    assert_eq!(
        kind(a.check(Priority::Job, &none())),
        Some(ResourceKind::Disk)
    );
    assert_eq!(a.check(Priority::Worker, &none()), Verdict::Admit);
    assert_eq!(a.check(Priority::Interactive, &none()), Verdict::Admit);
    // Томов ещё не замеряли — то же самое.
    let mut s = snap();
    s.volumes_sampled_at = None;
    s.volumes = vec![];
    tx.send(s).unwrap();
    assert_eq!(
        kind(a.check(Priority::Arena, &none())),
        Some(ResourceKind::Disk)
    );
    assert_eq!(a.check(Priority::Top, &none()), Verdict::Admit);
}

#[test]
fn stale_gpu_pauses_reserve_classes_only() {
    let mut s = snap();
    s.gpu_sampled_at = Some(ago(60));
    let (_tx, a) = setup(s);
    assert_eq!(
        kind(a.check(Priority::Job, &gpu_need())),
        Some(ResourceKind::Vram)
    );
    assert_eq!(
        kind(a.check(Priority::Worker, &gpu_need())),
        Some(ResourceKind::Vram)
    );
    assert_eq!(a.check(Priority::Interactive, &gpu_need()), Verdict::Admit);
}

#[test]
fn unknown_journal_volume_does_not_block() {
    // Свежий замер, но тома ZA_HOME среди точек монтирования нет — не блокировать навсегда.
    let mut s = snap();
    s.volumes = vec![VolumeSample {
        mount: PathBuf::from("/other"),
        ..volume(GIB)
    }];
    let (_tx, a) = setup(s);
    assert_eq!(a.check(Priority::Job, &none()), Verdict::Admit);
}

// --- Ожидание ---

#[tokio::test]
async fn waiters_resume_automatically() {
    let (tx, a) = setup(with_ram(GIB));
    let waiter = {
        let a = a.clone();
        tokio::spawn(async move { a.wait_admitted(Priority::Job, &none()).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiter.is_finished());
    // Выше порога, но ниже гистерезиса — всё ещё ждёт.
    tx.send(with_ram(205 * GIB / 100)).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiter.is_finished());
    tx.send(with_ram(3 * GIB)).unwrap();
    let res = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res, Ok(()));
}

#[tokio::test]
async fn wait_ends_when_monitor_stops() {
    let (tx, a) = setup(with_ram(GIB));
    let waiter = {
        let a = a.clone();
        tokio::spawn(async move { a.wait_admitted(Priority::Job, &none()).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(tx);
    let res = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res, Err(MonitorStopped));
}

#[tokio::test]
async fn admitted_wait_returns_immediately() {
    let (_tx, a) = setup(snap());
    assert_eq!(a.wait_admitted(Priority::Arena, &none()).await, Ok(()));
}

// --- Резерв диска (RES-017) ---

fn disk_wait_count(rx: &mut broadcast::Receiver<ResourceNotice>) -> usize {
    drain(rx)
        .into_iter()
        .filter(|n| matches!(n, ResourceNotice::DiskWait { .. }))
        .count()
}

#[tokio::test]
async fn disk_reservation_waits_and_notifies_once() {
    let mut s = snap();
    s.volumes = vec![volume(10 * GIB)];
    // Том журнала тот же, места на нём хватает (10 ГиБ ≥ 2 ГиБ).
    let (tx, a) = setup(s.clone());
    let mut notices = a.notices();
    let res = DiskReservations::new(a);
    let path = mount().join("models").join("big.gguf");

    let first = res.reserve(&path, 4 * GIB, Priority::Worker).await.unwrap();
    assert_eq!(res.reserved_on(&mount()), 4 * GIB);

    let second = {
        let res = res.clone();
        let path = path.clone();
        tokio::spawn(async move { res.reserve(&path, 2 * GIB, Priority::Worker).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!second.is_finished());
    // Новый снимок с тем же местом — заявка всё ещё ждёт, второго уведомления нет.
    tx.send(s).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!second.is_finished());
    let waits: Vec<_> = drain(&mut notices);
    assert_eq!(
        waits
            .iter()
            .filter(|n| matches!(n, ResourceNotice::DiskWait { .. }))
            .count(),
        1
    );
    assert!(waits.contains(&ResourceNotice::DiskWait {
        volume: mount(),
        need: 2 * GIB + 5 * GIB,
        free: 6 * GIB,
        caches: vec![],
    }));

    drop(first);
    let second = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(res.reserved_on(&mount()), 2 * GIB);
    assert_eq!(disk_wait_count(&mut notices), 0);
    drop(second);
    assert_eq!(res.reserved_on(&mount()), 0);
}

#[tokio::test]
async fn disk_reservation_ends_when_monitor_stops() {
    let mut s = snap();
    s.volumes = vec![volume(6 * GIB)];
    let (tx, a) = setup(s);
    let res = DiskReservations::new(a);
    let waiter = {
        let res = res.clone();
        tokio::spawn(async move {
            res.reserve(Path::new("/data/x"), 2 * GIB, Priority::Interactive)
                .await
                .map(|_| ())
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(tx);
    let r = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r, Err(MonitorStopped));
}

#[tokio::test]
async fn background_reservation_respects_journal_volume() {
    // Места под заявку на томе хватает, но том журнала почти полон: фоновая ждёт (RES-018).
    let mut s = snap();
    s.volumes = vec![
        volume(GIB),
        VolumeSample {
            mount: PathBuf::from("/big"),
            ..volume(100 * GIB)
        },
    ];
    let (_tx, a) = setup(s);
    let res = DiskReservations::new(a);
    let fg = res
        .reserve(Path::new("/big/x"), GIB, Priority::Interactive)
        .await
        .unwrap();
    let bg = tokio::time::timeout(
        Duration::from_millis(100),
        res.reserve(Path::new("/big/y"), GIB, Priority::Job),
    )
    .await;
    assert!(bg.is_err(), "фоновая заявка должна ждать");
    assert_eq!(res.reserved_on(Path::new("/big")), GIB);
    drop(fg);
}
