//! Открытие журнала с ограниченным повтором на `Locked`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use za_store::{ProjectStore, StoreError};
use zero_agent::journal::open_retrying;

#[path = "../../../test-support/unique_dir.rs"]
mod unique_dir;
use unique_dir::unique_dir;

fn dir(name: &str) -> PathBuf {
    unique_dir("zero-agent-journal", name)
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
