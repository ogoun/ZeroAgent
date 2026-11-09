//! Монитор ресурсов: разбор nvidia-smi, тома, размеры категорий, конфигурация, снимки.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;
use za_res::{
    DataCategory, GPU_RETRY_UNAVAILABLE, GpuSample, GpuStatus, MonitorConfig, ResourceMonitor,
    ResourcesConfig, default_category_paths, dir_size, host_sample_once, measure_categories,
    next_gpu_poll, parse_nvidia_smi_csv, query_nvidia_smi, volume_for,
};

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/nvidia-smi")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Чистый каталог внутри `CARGO_TARGET_TMPDIR` (системный `%TEMP%` запрещён).
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("za-res")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_bytes(path: &Path, n: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![b'x'; n]).unwrap();
}

// --- nvidia-smi ---

#[test]
fn parses_rtx3090_line() {
    let gpus = parse_nvidia_smi_csv(&fixture("rtx3090.csv")).unwrap();
    assert_eq!(
        gpus,
        vec![GpuSample {
            index: 0,
            name: "NVIDIA GeForce RTX 3090".into(),
            uuid: "GPU-1234".into(),
            util_percent: Some(37),
            mem_total_mib: 24576,
            mem_used_mib: Some(18000),
            mem_free_mib: Some(6576),
            temp_c: Some(54),
        }]
    );
}

#[test]
fn two_gpus_with_crlf() {
    let text = fixture("two_gpus_crlf.csv");
    assert!(text.contains("\r\n"), "фикстура должна сохранить CRLF");
    let gpus = parse_nvidia_smi_csv(&text).unwrap();
    assert_eq!(gpus.len(), 2);
    assert_eq!(gpus[0].index, 0);
    assert_eq!(gpus[1].index, 1);
    assert_eq!(gpus[1].name, "NVIDIA RTX A4000");
    assert_eq!(gpus[1].uuid, "GPU-bbbb-1111");
    assert_eq!(gpus[1].temp_c, Some(35));
    assert_eq!(gpus[1].mem_total_mib, 16376);
}

#[test]
fn na_values_are_none() {
    let gpus = parse_nvidia_smi_csv(&fixture("wddm_na.csv")).unwrap();
    assert_eq!(gpus.len(), 1);
    let g = &gpus[0];
    assert_eq!(g.util_percent, None);
    assert_eq!(g.mem_used_mib, None);
    assert_eq!(g.mem_free_mib, None);
    assert_eq!(g.temp_c, None);
    assert_eq!(g.mem_total_mib, 24576);
}

#[test]
fn fractional_temperature_of_driver_621() {
    // Записано с RTX 3090, драйвер 621.01 (Windows, WDDM); uuid обезличен.
    let text = fixture("driver621_fractional_temp.csv");
    assert!(text.contains("\r\n"));
    let gpus = parse_nvidia_smi_csv(&text).unwrap();
    assert_eq!(gpus.len(), 1);
    assert_eq!(gpus[0].temp_c, Some(45));
    assert_eq!(gpus[0].util_percent, Some(0));
    assert_eq!(gpus[0].mem_total_mib, 24576);
    assert_eq!(gpus[0].mem_used_mib, Some(5234));
    assert_eq!(gpus[0].mem_free_mib, Some(19093));
}

#[test]
fn empty_output_has_no_gpus() {
    assert_eq!(parse_nvidia_smi_csv(&fixture("empty.csv")), Ok(vec![]));
    assert_eq!(parse_nvidia_smi_csv("\r\n\n  \n"), Ok(vec![]));
}

#[test]
fn garbage_is_an_error() {
    assert!(parse_nvidia_smi_csv(&fixture("garbage.txt")).is_err());
    // Число полей не то.
    assert!(parse_nvidia_smi_csv("0, NVIDIA GeForce RTX 3090, GPU-1, 37").is_err());
    // Не число там, где ждём число.
    assert!(parse_nvidia_smi_csv("x, RTX, GPU-1, 37, 24576, 1, 2, 3").is_err());
    assert!(parse_nvidia_smi_csv("0, RTX, GPU-1, abc, 24576, 1, 2, 3").is_err());
    // Объём памяти обязателен.
    assert!(parse_nvidia_smi_csv("0, RTX, GPU-1, 37, [N/A], 1, 2, 3").is_err());
    // Одна битая строка среди хороших — ошибка всего вывода.
    let mixed = format!("{}garbage line\n", fixture("rtx3090.csv"));
    assert!(parse_nvidia_smi_csv(&mixed).is_err());
}

#[tokio::test]
async fn missing_nvidia_smi_is_unavailable() {
    let exe = Path::new(env!("CARGO_TARGET_TMPDIR")).join("нет-такого.exe");
    let tmp = scratch("gpu-missing");
    let status = query_nvidia_smi(&exe, Duration::from_secs(5), &tmp).await;
    assert!(
        matches!(status, GpuStatus::Unavailable { .. }),
        "{status:?}"
    );
}

#[tokio::test]
async fn failing_or_slow_exe_is_unavailable() {
    // Тестовый бинарник не знает аргументов nvidia-smi: ненулевой код возврата.
    let exe = std::env::current_exe().unwrap();
    let tmp = scratch("gpu-failing");
    let status = query_nvidia_smi(&exe, Duration::from_secs(5), &tmp).await;
    assert!(
        matches!(status, GpuStatus::Unavailable { .. }),
        "{status:?}"
    );
    // Нулевой таймаут: процесс не успевает ответить, ожидание не вечное.
    let started = std::time::Instant::now();
    let status = query_nvidia_smi(&exe, Duration::ZERO, &tmp).await;
    assert!(
        matches!(status, GpuStatus::Unavailable { .. }),
        "{status:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn unavailable_gpu_is_retried_after_60s() {
    let t = Instant::now();
    let every = Duration::from_secs(2);
    let un = GpuStatus::Unavailable {
        reason: "нет".into(),
    };
    assert_eq!(GPU_RETRY_UNAVAILABLE, Duration::from_secs(60));
    assert_eq!(next_gpu_poll(t, &un, every), t + Duration::from_secs(60));
    assert_eq!(
        next_gpu_poll(t, &GpuStatus::Available(vec![]), every),
        t + Duration::from_secs(2)
    );
}

// --- тома ---

#[test]
fn longest_mount_wins() {
    let mounts: Vec<PathBuf> = ["/", "/home", "/home/u/data"]
        .iter()
        .map(PathBuf::from)
        .collect();
    assert_eq!(
        volume_for(Path::new("/home/u/data/x"), &mounts),
        Some(&PathBuf::from("/home/u/data"))
    );
    assert_eq!(
        volume_for(Path::new("/etc"), &mounts),
        Some(&PathBuf::from("/"))
    );
    // Префикс строки — не префикс пути.
    assert_eq!(
        volume_for(Path::new("/home/u/database"), &mounts),
        Some(&PathBuf::from("/home"))
    );
    assert_eq!(
        volume_for(Path::new("/home/u/data"), &mounts),
        Some(&PathBuf::from("/home/u/data"))
    );
    assert_eq!(volume_for(Path::new("/etc"), &[]), None);
}

#[cfg(windows)]
#[test]
fn longest_mount_wins_windows_prefix() {
    let mounts = vec![PathBuf::from("C:\\"), PathBuf::from("D:\\")];
    assert_eq!(
        volume_for(Path::new("\\\\?\\d:\\proj"), &mounts),
        Some(&PathBuf::from("D:\\"))
    );
    assert_eq!(
        volume_for(Path::new("c:/Users/x"), &mounts),
        Some(&PathBuf::from("C:\\"))
    );
    assert_eq!(volume_for(Path::new("E:\\x"), &mounts), None);
}

// --- размеры категорий ---

#[test]
fn dir_size_counts_and_truncates() {
    let root = scratch("dir-size");
    for i in 0..3 {
        write_bytes(&root.join(format!("f{i}")), 1000);
    }
    assert_eq!(dir_size(&root, 100, &[]), (Some(3000), false));
    let (_, truncated) = dir_size(&root, 2, &[]);
    assert!(truncated);

    let spill = root.join("spill");
    write_bytes(&spill.join("s"), 500);
    assert_eq!(dir_size(&root, 100, &[]), (Some(3500), false));
    assert_eq!(dir_size(&root, 100, &[spill]), (Some(3000), false));

    assert_eq!(dir_size(&root.join("нет"), 100, &[]), (None, false));
}

#[test]
fn project_work_excludes_nested_categories() {
    let base = scratch("categories");
    let home = base.join("home");
    let p = base.join("proj");
    let work = p.join(".zero-agent/work");
    write_bytes(&work.join("a.txt"), 100);
    write_bytes(&work.join("spill/s.bin"), 200);
    write_bytes(&work.join("worktrees/w1/f"), 300);
    write_bytes(&work.join("browser/profile/c"), 400);
    write_bytes(&home.join("config.toml"), 10);

    let paths = default_category_paths(&home, std::slice::from_ref(&p));
    assert!(paths.contains(&(DataCategory::ProjectWork, work.clone())));
    assert!(paths.contains(&(DataCategory::Spill, work.join("spill"))));
    assert!(paths.contains(&(DataCategory::Worktrees, work.join("worktrees"))));
    assert!(paths.contains(&(DataCategory::BrowserProfiles, work.join("browser"))));
    assert!(paths.contains(&(DataCategory::ZaHome, home.clone())));

    let usage = measure_categories(&paths, 1000);
    let project_sum: u64 = usage
        .iter()
        .filter(|u| u.path.starts_with(&work))
        .map(|u| u.bytes.unwrap())
        .sum();
    assert_eq!(dir_size(&work, 1000, &[]), (Some(1000), false));
    assert_eq!(project_sum, 1000);
    let pw = usage
        .iter()
        .find(|u| u.category == DataCategory::ProjectWork)
        .unwrap();
    assert_eq!(pw.bytes, Some(100));
    assert!(!pw.truncated);
}

// --- конфигурация ---

#[test]
fn monitor_config_from_resources() {
    let res = ResourcesConfig {
        cpu_busy_percent: 80.0,
        ..Default::default()
    };
    let cfg = MonitorConfig::from_resources(&res, PathBuf::from("home"), vec![], None);
    assert_eq!(cfg.cpu_busy_percent, 80.0);
    assert_eq!(cfg.nvidia_smi, PathBuf::from("nvidia-smi"));
    assert_eq!(cfg.cpu_ram_every, Duration::from_secs(1));
    assert_eq!(cfg.gpu_every, Duration::from_secs(2));
    assert_eq!(cfg.volumes_every, Duration::from_secs(5));
    assert_eq!(cfg.categories_every, Duration::from_secs(300));
}

#[test]
fn resources_config_defaults_match_d10() {
    let d = ResourcesConfig::default();
    assert_eq!(d.ram_reserve_mib, None);
    assert_eq!(d.ram_floor_mib, 512);
    assert_eq!(d.vram_reserve_mib, 1024);
    assert_eq!(d.vram_floor_mib, 256);
    assert_eq!(d.disk_margin_mib, 5120);
    assert_eq!(d.journal_low_mib, 2048);
    assert_eq!(d.cpu_busy_percent, 95.0);
    assert_eq!(d.cpu_busy_for_s, 10);
    assert_eq!(d.hysteresis, 0.1);
    assert_eq!(d.nvidia_smi, PathBuf::from("nvidia-smi"));
}

#[test]
fn resources_config_partial_toml_uses_defaults() {
    let c: ResourcesConfig = serde_json::from_str("{\"vram_floor_mib\": 512}").unwrap();
    assert_eq!(
        c,
        ResourcesConfig {
            vram_floor_mib: 512,
            ..Default::default()
        }
    );
    let c: ResourcesConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(c, ResourcesConfig::default());
    assert!(serde_json::from_str::<ResourcesConfig>("{\"vram_flor_mib\": 1}").is_err());
}

// --- снимки ---

fn host_cfg(name: &str) -> MonitorConfig {
    let base = scratch(name);
    MonitorConfig::from_resources(&ResourcesConfig::default(), base.join("home"), vec![], None)
}

#[test]
fn host_sample_is_sane() {
    let s = host_sample_once(&host_cfg("host-sample"));
    assert!(s.ram.total > 0);
    assert!(s.ram.available <= s.ram.total);
    assert!(s.cpu.cores >= 1);
    assert!((0.0..=100.0).contains(&s.cpu.usage_percent));
    assert!(!s.volumes.is_empty());
    assert!(matches!(s.gpu, GpuStatus::Unavailable { .. }));
    // Каталог ZA_HOME лежит на одном из томов — у тома есть роль ZaHome.
    assert!(
        s.volumes
            .iter()
            .any(|v| v.roles.contains(&DataCategory::ZaHome)),
        "{:?}",
        s.volumes
    );
}

#[test]
fn snapshot_json_shape() {
    let s = host_sample_once(&host_cfg("json-shape"));
    let v = serde_json::to_value(&s).unwrap();
    let obj = v.as_object().unwrap();
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["categories", "cpu", "gpu", "ram", "sampled_at", "volumes"]
    );
    assert!(obj["gpu"].get("unavailable").is_some(), "{}", obj["gpu"]);
    // Круговая сериализация.
    let back: za_res::ResourceSnapshot = serde_json::from_value(v).unwrap();
    assert_eq!(back, s);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_publishes_and_stops_on_drop() {
    let mut cfg = host_cfg("monitor-run");
    cfg.nvidia_smi = Path::new(env!("CARGO_TARGET_TMPDIR")).join("нет-такого.exe");
    cfg.cpu_ram_every = Duration::from_millis(50);
    cfg.gpu_every = Duration::from_millis(50);
    cfg.volumes_every = Duration::from_millis(50);
    cfg.categories_every = Duration::from_millis(50);
    let home = cfg.za_home.clone();
    let (mon, mut rx) = ResourceMonitor::start(cfg);
    assert!(
        home.join("work/tmp/monitor").is_dir(),
        "D33: каталог монитора"
    );
    // Нет nvidia-smi: GPU недоступен, а CPU и RAM всё равно публикуются.
    let deadline = Duration::from_secs(10);
    let ok = tokio::time::timeout(deadline, async {
        loop {
            rx.changed().await.unwrap();
            let s = rx.borrow_and_update().clone();
            if s.ram.total > 0
                && s.cpu.cores >= 1
                && !s.volumes.is_empty()
                && !s.categories.is_empty()
                && matches!(&s.gpu, GpuStatus::Unavailable { reason } if reason != za_res::GPU_NOT_POLLED)
            {
                return s;
            }
        }
    })
    .await
    .expect("снимок с CPU, RAM, томами и категориями");
    assert_eq!(mon.latest().ram.total, ok.ram.total);
    drop(mon);
    // Опрос отменён: все отправители закрыты, канал сообщает об этом.
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        while rx.changed().await.is_ok() {}
    })
    .await;
    assert!(closed.is_ok(), "после drop монитора канал закрывается");
}

#[tokio::test]
#[ignore = "нужна NVIDIA GPU"]
async fn live_nvidia_smi() {
    let tmp = scratch("gpu-live");
    let status = query_nvidia_smi(Path::new("nvidia-smi"), Duration::from_secs(5), &tmp).await;
    eprintln!("{status:?}");
    match status {
        GpuStatus::Available(gpus) => {
            assert!(!gpus.is_empty());
            assert!(gpus.iter().all(|g| g.mem_total_mib > 0));
        }
        GpuStatus::Unavailable { reason } => panic!("GPU недоступен: {reason}"),
    }
}
