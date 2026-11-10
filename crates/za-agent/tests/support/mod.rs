//! Общие помощники интеграционных тестов `za-agent` (R1-C1, D72): множитель времени
//! [`scale`], ожидание с пределом [`within`] и уникальный каталог теста [`unique_dir`].
//!
//! Каждый тестовый крейт берёт только часть помощников, поэтому неиспользуемые в нём
//! функции не считаются ошибкой.
#![allow(dead_code)]

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

/// Множитель времени тестов: `ZA_TEST_TIME_SCALE` (целое 1–6, по умолчанию 1).
pub fn scale() -> u32 {
    static SCALE: OnceLock<u32> = OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("ZA_TEST_TIME_SCALE")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map_or(1, |n| n.clamp(1, 6))
    })
}

/// Предел ожиданий, которые при регрессии повисли бы: 10 с × [`scale`].
pub fn limit() -> Duration {
    Duration::from_secs(10) * scale()
}

/// Длительность `d` × [`scale`].
pub fn scaled(d: Duration) -> Duration {
    d * scale()
}

/// Ждёт `f` не дольше [`limit`]; иначе — паника с понятным текстом.
pub async fn within<T>(what: &str, f: impl Future<Output = T>) -> T {
    within_for(Duration::from_secs(10), what, f).await
}

/// Ждёт `f` не дольше `base` × [`scale`] (долгие сценарии); иначе — паника.
pub async fn within_for<T>(base: Duration, what: &str, f: impl Future<Output = T>) -> T {
    tokio::time::timeout(scaled(base), f)
        .await
        .unwrap_or_else(|_| panic!("не дождались в срок: {what}"))
}

#[path = "../../../../test-support/unique_dir.rs"]
mod unique;

/// Чистый каталог теста `CARGO_TARGET_TMPDIR/<suite>/<name>-<pid>-<n>` — общий помощник
/// workspace (`test-support/unique_dir.rs`).
pub use unique::unique_dir;
