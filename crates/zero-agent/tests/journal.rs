//! Открытие журнала с ограниченным повтором на `Locked`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use za_store::{ProjectStore, StoreError};
use zero_agent::journal::open_retrying;

/// Имя каталога теста `<name>-<pid>-<n>`: свой у каждого вызова и процесса, одновременные
/// прогоны одного бинарника не делят каталог (R1-C1, гигиена тестов).
fn unique(name: &str) -> String {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{name}-{}-{n}", std::process::id())
}

fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("zero-agent-journal")
        .join(unique(name));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn open_waits_for_lock_release() {
    let d = dir("released");
    let held = ProjectStore::open(&d).unwrap();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        drop(held);
    });
    let s = open_retrying(&d);
    t.join().unwrap();
    assert!(s.is_ok(), "{s:?}");
}

#[test]
fn open_gives_up_after_bounded_retries() {
    let d = dir("held");
    let _held = ProjectStore::open(&d).unwrap();
    let start = Instant::now();
    assert!(matches!(open_retrying(&d), Err(StoreError::Locked)));
    let spent = start.elapsed();
    assert!(spent >= Duration::from_millis(2900), "{spent:?}");
    assert!(spent < Duration::from_secs(20), "{spent:?}");
}
