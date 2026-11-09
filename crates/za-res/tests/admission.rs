//! Допуск заявок (D10, D30, RES-012/013/017/018), гистерезис, резерв диска.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::{broadcast, watch};
use tokio::time::Instant;
use za_protocol::Priority;
use za_res::{
    Admission, CpuSample, DiskKind, DiskReservations, GpuSample, GpuStatus, MemSample,
    MonitorStopped, Need, ResourceKind, ResourceNotice, ResourceSnapshot, ResourcesConfig,
    ResourcesConfigError, Verdict, VolumeSample,
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
        sampled_mono: Some(Instant::now()),
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
        gpu_sampled_mono: Some(Instant::now()),
        gpu: gpu(24576, Some(1000), Some(23576)),
        volumes_sampled_at: Some(now),
        volumes_sampled_mono: Some(Instant::now()),
        volumes: vec![volume(500 * GIB)],
        categories_sampled_at: None,
        categories_sampled_mono: None,
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
    setup_with(s, cfg())
}

fn setup_with(
    s: ResourceSnapshot,
    c: ResourcesConfig,
) -> (watch::Sender<ResourceSnapshot>, Admission) {
    let (tx, rx) = watch::channel(s);
    (tx, Admission::new(rx, c, za_home()))
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
            free: 700 * MIB,
            // Уровень 700 < 1024 закрыл порог: нужен уровень возобновления max(1126,4, 1280).
            required: 1280 * MIB,
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
    s.sampled_mono = None;
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
    s.volumes_sampled_mono = None;
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
    s.volumes_sampled_mono = None;
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
    s.gpu_sampled_mono = None;
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

// --- Раунд исправлений 1 ---

/// Решения двух заявок с разной потребностью при заданной последовательности уровней RAM.
fn run_sequence(order_ab: bool) -> Vec<(Verdict, Verdict)> {
    let big = Need {
        ram_mib: 4096,
        ..none()
    };
    let (tx, a) = setup_with(
        with_ram(10 * GIB),
        ResourcesConfig {
            ram_reserve_mib: None, // 64 ГиБ × 10 % = 6,4 ГиБ, возобновление 7,04 ГиБ
            ..ResourcesConfig::default()
        },
    );
    let mut out = vec![];
    for avail in [10 * GIB, 6 * GIB, 7 * GIB, 10 * GIB + GIB / 2, 8 * GIB] {
        tx.send(with_ram(avail)).unwrap();
        let (va, vb) = if order_ab {
            let va = a.check(Priority::Job, &big);
            (va, a.check(Priority::Job, &none()))
        } else {
            let vb = a.check(Priority::Job, &none());
            (a.check(Priority::Job, &big), vb)
        };
        out.push((va, vb));
    }
    out
}

#[test]
fn check_order_does_not_change_verdicts() {
    // I1: состояние порога зависит только от уровня ресурса, а не от потребности заявки.
    assert_eq!(run_sequence(true), run_sequence(false));
}

#[test]
fn hysteresis_is_not_bypassed_by_request_order() {
    let big = Need {
        ram_mib: 4096,
        ..none()
    };
    let (tx, a) = setup_with(
        with_ram(10 * GIB),
        ResourcesConfig {
            ram_reserve_mib: None,
            ..ResourcesConfig::default()
        },
    );
    // Уровень 10 ГиБ ≥ 6,4: порог открыт; большой заявке не хватает (6 < 6,4), порог не закрыт.
    assert_eq!(kind(a.check(Priority::Job, &big)), Some(ResourceKind::Ram));
    assert_eq!(a.check(Priority::Job, &none()), Verdict::Admit);
    // Уровень упал ниже запаса — порог закрыт.
    tx.send(with_ram(6 * GIB)).unwrap();
    assert_eq!(
        kind(a.check(Priority::Job, &none())),
        Some(ResourceKind::Ram)
    );
    // 7 ГиБ < 7,04: и маленькая заявка ждёт, в каком бы порядке ни шли проверки.
    tx.send(with_ram(7 * GIB)).unwrap();
    assert_eq!(kind(a.check(Priority::Job, &big)), Some(ResourceKind::Ram));
    assert_eq!(
        kind(a.check(Priority::Job, &none())),
        Some(ResourceKind::Ram)
    );
    // 10,5 ГиБ ≥ 7,04: порог открыт; большая — 6,5 ≥ 6,4 — допуск.
    tx.send(with_ram(10 * GIB + GIB / 2)).unwrap();
    assert_eq!(a.check(Priority::Job, &big), Verdict::Admit);
    assert_eq!(a.check(Priority::Job, &none()), Verdict::Admit);
}

#[test]
fn closed_gate_uses_resume_level_with_need() {
    let (tx, a) = setup(with_ram(19 * GIB / 10));
    assert_eq!(
        kind(a.check(Priority::Job, &none())),
        Some(ResourceKind::Ram)
    );
    // 2,2 ГиБ < 2,25: порог закрыт; заявка 256 МиБ сравнивается с уровнем возобновления.
    tx.send(with_ram(22 * GIB / 10)).unwrap();
    let need = Need {
        ram_mib: 256,
        ..none()
    };
    assert_eq!(
        a.check(Priority::Job, &need),
        Verdict::Wait {
            kind: ResourceKind::Ram,
            free: 22 * GIB / 10 - 256 * MIB,
            required: 2 * GIB + 256 * MIB,
        }
    );
    // 2,5 ГиБ ≥ 2,25 — порог открывается; заявке 768 МиБ: 1,75 < 2 — ждёт по порогу.
    tx.send(with_ram(5 * GIB / 2)).unwrap();
    let need = Need {
        ram_mib: 768,
        ..none()
    };
    assert_eq!(
        a.check(Priority::Job, &need),
        Verdict::Wait {
            kind: ResourceKind::Ram,
            free: 5 * GIB / 2 - 768 * MIB,
            required: 2 * GIB,
        }
    );
}

// Монотонные отметки (I2).

#[test]
fn monotonic_stamp_ignores_wall_clock_jumps() {
    // Часы ушли вперёд: настенная отметка «час назад», монотонная — свежая.
    let mut s = snap();
    s.sampled_at = ago(3600);
    s.volumes_sampled_at = Some(ago(3600));
    s.gpu_sampled_at = Some(ago(3600));
    let (tx, a) = setup(s);
    assert_eq!(a.check(Priority::Job, &gpu_need()), Verdict::Admit);
    // Часы ушли назад: настенная отметка «через час», монотонная — свежая.
    let mut s = snap();
    s.sampled_at = ago(-3600);
    s.volumes_sampled_at = Some(ago(-3600));
    s.gpu_sampled_at = Some(ago(-3600));
    tx.send(s).unwrap();
    assert_eq!(a.check(Priority::Job, &gpu_need()), Verdict::Admit);
}

#[test]
fn future_wall_stamp_without_monotonic_is_stale() {
    // Снимок из сериализации (монотонных нет), отметки из будущего — устаревшие.
    let mut s = snap();
    s.sampled_at = ago(-3600);
    s.sampled_mono = None;
    s.volumes_sampled_at = Some(ago(-3600));
    s.volumes_sampled_mono = None;
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
fn fresh_wall_stamp_without_monotonic_is_fresh() {
    let mut s = snap();
    s.sampled_mono = None;
    s.volumes_sampled_mono = None;
    s.gpu_sampled_mono = None;
    let (_tx, a) = setup(s);
    assert_eq!(a.check(Priority::Job, &gpu_need()), Verdict::Admit);
}

#[test]
fn deserialized_snapshot_has_no_monotonic_stamps() {
    let json = serde_json::to_string(&snap()).unwrap();
    let back: ResourceSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(back.sampled_mono, None);
    assert_eq!(back.gpu_sampled_mono, None);
    assert_eq!(back.volumes_sampled_mono, None);
    assert_eq!(back.categories_sampled_mono, None);
}

#[tokio::test(start_paused = true)]
async fn monotonic_age_makes_part_stale() {
    let (_tx, a) = setup(snap());
    assert_eq!(a.check(Priority::Job, &none()), Verdict::Admit);
    // Настенные отметки свежие, но по монотонным CPU и RAM старше 3 с.
    tokio::time::advance(Duration::from_secs(4)).await;
    assert_eq!(
        kind(a.check(Priority::Job, &none())),
        Some(ResourceKind::Ram)
    );
    assert_eq!(a.check(Priority::Interactive, &none()), Verdict::Admit);
}

// Уведомления и гистерезис остальных порогов.

#[test]
fn ram_critical_once_per_episode() {
    let (tx, a) = setup(with_ram(400 * MIB));
    let mut notices = a.notices();
    a.check(Priority::Interactive, &none());
    a.check(Priority::Job, &none());
    assert_eq!(
        drain(&mut notices),
        vec![ResourceNotice::RamCritical { free: 400 * MIB }]
    );
    // 600 МиБ < max(512 × 1,1, 768) — эпизод продолжается.
    tx.send(with_ram(600 * MIB)).unwrap();
    assert_eq!(
        kind(a.check(Priority::Interactive, &none())),
        Some(ResourceKind::Ram)
    );
    assert!(drain(&mut notices).is_empty());
    tx.send(with_ram(GIB)).unwrap();
    assert_eq!(a.check(Priority::Interactive, &none()), Verdict::Admit);
    tx.send(with_ram(300 * MIB)).unwrap();
    a.check(Priority::Interactive, &none());
    assert_eq!(drain(&mut notices).len(), 1);
}

fn with_vram_free(free: u64) -> ResourceSnapshot {
    let mut s = snap();
    s.gpu = gpu(24576, Some(24576 - free), Some(free));
    s
}

#[test]
fn vram_reserve_hysteresis() {
    // Запас 1024: закрыт при 900; 1200 < max(1126,4, 1280) — ждёт; 1300 — допуск.
    let (tx, a) = setup(with_vram_free(900));
    assert_eq!(
        kind(a.check(Priority::Job, &gpu_need())),
        Some(ResourceKind::Vram)
    );
    tx.send(with_vram_free(1200)).unwrap();
    assert_eq!(
        a.check(Priority::Job, &gpu_need()),
        Verdict::Wait {
            kind: ResourceKind::Vram,
            free: 1200 * MIB,
            required: 1280 * MIB,
        }
    );
    tx.send(with_vram_free(1300)).unwrap();
    assert_eq!(a.check(Priority::Job, &gpu_need()), Verdict::Admit);
}

#[test]
fn vram_shared_hysteresis() {
    // D30: пауза при запасе ≤ 256; 400 < 512 — пауза продолжается; 600 — снята.
    let (tx, a) = setup(with_vram_free(176));
    assert_eq!(
        kind(a.check(Priority::Worker, &gpu_need())),
        Some(ResourceKind::Vram)
    );
    tx.send(with_vram_free(400)).unwrap();
    assert_eq!(
        a.check(Priority::Worker, &gpu_need()),
        Verdict::Wait {
            kind: ResourceKind::Vram,
            free: 400 * MIB,
            required: 512 * MIB,
        }
    );
    tx.send(with_vram_free(600)).unwrap();
    assert_eq!(a.check(Priority::Worker, &gpu_need()), Verdict::Admit);
}

#[test]
fn journal_volume_hysteresis() {
    let mut s = snap();
    s.volumes = vec![volume(GIB)];
    let (tx, a) = setup(s);
    assert_eq!(
        kind(a.check(Priority::Job, &none())),
        Some(ResourceKind::Disk)
    );
    // 2,1 ГиБ ≥ 2 ГиБ, но < max(2,2, 2,25) ГиБ — ждёт.
    let mut s = snap();
    s.volumes = vec![volume(21 * GIB / 10)];
    tx.send(s).unwrap();
    assert_eq!(
        a.check(Priority::Job, &none()),
        Verdict::Wait {
            kind: ResourceKind::Disk,
            free: 21 * GIB / 10,
            required: 2 * GIB + 256 * MIB,
        }
    );
    let mut s = snap();
    s.volumes = vec![volume(23 * GIB / 10)];
    tx.send(s).unwrap();
    assert_eq!(a.check(Priority::Job, &none()), Verdict::Admit);
}

fn card(index: u32, total: u64, used: Option<u64>, free: Option<u64>) -> GpuSample {
    GpuSample {
        index,
        name: format!("GPU {index}"),
        uuid: format!("GPU-{index}"),
        util_percent: None,
        mem_total_mib: total,
        mem_used_mib: used,
        mem_free_mib: free,
        temp_c: None,
    }
}

#[test]
fn multi_gpu_uses_tightest_card() {
    let mut s = snap();
    s.gpu = GpuStatus::Available(vec![
        card(0, 24576, Some(1000), Some(23576)),
        card(1, 16384, Some(15684), Some(700)),
    ]);
    let (_tx, a) = setup(s);
    assert_eq!(
        a.check(Priority::Job, &gpu_need()),
        Verdict::Wait {
            kind: ResourceKind::Vram,
            free: 700 * MIB,
            // Уровень 700 < 1024 закрыл порог: нужен уровень возобновления max(1126,4, 1280).
            required: 1280 * MIB,
        }
    );
    assert_eq!(a.check(Priority::Worker, &gpu_need()), Verdict::Admit);
}

#[test]
fn vram_used_known_free_unknown() {
    // mem_free = None, used известен: свободное = total − used.
    let mut s = snap();
    s.gpu = gpu(24576, Some(23876), None);
    let (tx, a) = setup(s);
    assert_eq!(
        a.check(Priority::Job, &gpu_need()),
        Verdict::Wait {
            kind: ResourceKind::Vram,
            free: 700 * MIB,
            // Уровень 700 < 1024 закрыл порог: нужен уровень возобновления max(1126,4, 1280).
            required: 1280 * MIB,
        }
    );
    // D30 по used.
    let mut notices = a.notices();
    let mut s = snap();
    s.gpu = gpu(24576, Some(24400), None);
    tx.send(s).unwrap();
    assert_eq!(
        kind(a.check(Priority::Worker, &gpu_need())),
        Some(ResourceKind::Vram)
    );
    assert_eq!(drain(&mut notices).len(), 1);
}

#[test]
fn vram_need_is_subtracted() {
    let (_tx, a) = setup(with_vram_free(4096));
    let need = Need {
        vram_mib: 3500,
        local_gpu: true,
        ..none()
    };
    assert_eq!(
        a.check(Priority::Job, &need),
        Verdict::Wait {
            kind: ResourceKind::Vram,
            free: 596 * MIB,
            required: 1024 * MIB,
        }
    );
    assert_eq!(a.check(Priority::Worker, &need), Verdict::Admit);
}

// Конфигурация.

#[test]
fn config_validation() {
    assert_eq!(ResourcesConfig::default().validate(), Ok(()));
    let bad = |c: ResourcesConfig| c.validate().unwrap_err();
    assert_eq!(
        bad(ResourcesConfig {
            hysteresis: f32::INFINITY,
            ..ResourcesConfig::default()
        }),
        ResourcesConfigError::Hysteresis(f32::INFINITY)
    );
    assert!(matches!(
        bad(ResourcesConfig {
            hysteresis: f32::NAN,
            ..ResourcesConfig::default()
        }),
        ResourcesConfigError::Hysteresis(_)
    ));
    assert_eq!(
        bad(ResourcesConfig {
            hysteresis: -0.1,
            ..ResourcesConfig::default()
        }),
        ResourcesConfigError::Hysteresis(-0.1)
    );
    assert_eq!(
        bad(ResourcesConfig {
            cpu_busy_for_s: 0,
            ..ResourcesConfig::default()
        }),
        ResourcesConfigError::CpuBusyFor
    );
    assert_eq!(
        bad(ResourcesConfig {
            cpu_busy_percent: 0.0,
            ..ResourcesConfig::default()
        }),
        ResourcesConfigError::CpuBusyPercent(0.0)
    );
}

#[test]
fn invalid_config_falls_back_to_defaults() {
    // inf дал бы вечную паузу по RAM, cpu_busy_for_s = 0 — вечную паузу по CPU.
    let c = ResourcesConfig {
        ram_reserve_mib: Some(2048),
        hysteresis: f32::INFINITY,
        cpu_busy_for_s: 0,
        ..ResourcesConfig::default()
    };
    let (tx, a) = setup_with(with_ram(19 * GIB / 10), c);
    assert_eq!(
        kind(a.check(Priority::Job, &none())),
        Some(ResourceKind::Ram)
    );
    tx.send(with_ram(23 * GIB / 10)).unwrap();
    assert_eq!(a.check(Priority::Job, &none()), Verdict::Admit);
}

// Резерв: учёт записанного.

#[tokio::test]
async fn consumed_bytes_release_reservation() {
    let mut s = snap();
    s.volumes = vec![volume(10 * GIB)];
    let (_tx, a) = setup(s);
    let res = DiskReservations::new(a);
    let path = mount().join("x");
    let mut first = res.reserve(&path, 4 * GIB, Priority::Worker).await.unwrap();
    let second = {
        let res = res.clone();
        let path = path.clone();
        tokio::spawn(async move { res.reserve(&path, 2 * GIB, Priority::Worker).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!second.is_finished());
    // Записано 3 ГиБ: резерв первой — 1 ГиБ, свободно 10 − 1 = 9 ≥ 2 + 5.
    first.consume(3 * GIB);
    assert_eq!(first.bytes(), GIB);
    let second = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(res.reserved_on(&mount()), 3 * GIB);
    first.consume(10 * GIB);
    assert_eq!(first.bytes(), 0);
    drop(first);
    assert_eq!(res.reserved_on(&mount()), 2 * GIB);
    drop(second);
    assert_eq!(res.reserved_on(&mount()), 0);
}
