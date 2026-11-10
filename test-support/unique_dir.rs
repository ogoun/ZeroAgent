//! Уникальный каталог теста — один помощник на весь workspace (R1-C1, гигиена тестов).
//!
//! Подключается в тестовый крейт как модуль, без отдельного крейта и зависимостей:
//!
//! ```ignore
//! #[path = "../../../test-support/unique_dir.rs"]
//! mod unique_dir;
//! ```
//!
//! Каталог — `<root>/<suite>/<name>-<pid>-<n>`: свой у каждого вызова и процесса, поэтому
//! одновременные прогоны одного тестового бинарника не делят и не стирают каталоги друг
//! друга. Первый вызов процесса для `<root>/<suite>` удаляет каталоги других процессов
//! старше [`STALE_RUN`] (брошенные прошлыми прогонами), свежие чужие не трогаются.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Каталог прогона старше этого считается брошенным: прогон бинарника длится минуты.
pub const STALE_RUN: Duration = Duration::from_secs(30 * 60);

/// Чистый каталог `CARGO_TARGET_TMPDIR/<suite>/<name>-<pid>-<n>` интеграционного теста.
pub fn unique_dir(suite: &str, name: &str) -> PathBuf {
    // `env!` не годится: модуль подключают и unit-тесты, где переменной нет.
    let Some(root) = option_env!("CARGO_TARGET_TMPDIR") else {
        panic!("CARGO_TARGET_TMPDIR задан только интеграционным тестам: unique_dir_in");
    };
    unique_dir_in(&Path::new(root).join(suite), name)
}

/// Чистый каталог `<root>/<name>-<pid>-<n>` (unit-тесты: `.tmp/<crate>-unit`).
pub fn unique_dir_in(root: &Path, name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    static CLEANED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let pid = std::process::id();
    {
        let mut done = CLEANED.lock().unwrap_or_else(|e| e.into_inner());
        if !done.iter().any(|r| r == root) {
            done.push(root.to_path_buf());
            remove_stale(root, &format!("-{pid}-"));
        }
    }
    let n = N.fetch_add(1, Ordering::Relaxed);
    let d = root.join(format!("{name}-{pid}-{n}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Удаляет подкаталоги `root` старше [`STALE_RUN`], в имени которых нет `own`.
fn remove_stale(root: &Path, own: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_RUN);
        if stale && !e.file_name().to_string_lossy().contains(own) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}
