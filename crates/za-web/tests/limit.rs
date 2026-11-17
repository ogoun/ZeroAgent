//! Лимитер частоты (D107, D121) — чистая структура без сети, виртуальное время tokio.

use std::time::Duration;

use tokio::time::Instant;
use za_web::{FetchError, RateLimiter};

const FAR: Duration = Duration::from_secs(3600);

#[tokio::test(start_paused = true)]
async fn host_interval_respected() {
    let l = RateLimiter::new(60, Duration::from_millis(500));
    let t0 = Instant::now();
    l.acquire("a.test", t0 + FAR).await.unwrap();
    assert_eq!(Instant::now(), t0, "первый запрос к хосту не ждёт");
    // Другой хост интервалом первого не задерживается.
    l.acquire("b.test", t0 + FAR).await.unwrap();
    assert_eq!(Instant::now(), t0);
    l.acquire("a.test", t0 + FAR).await.unwrap();
    assert!(
        Instant::now() >= t0 + Duration::from_millis(500),
        "второй запрос к хосту — не раньше +500 мс"
    );
    // Дедлайн раньше интервала — отказ сразу, без ожидания.
    let t1 = Instant::now();
    let r = l.acquire("a.test", t1 + Duration::from_millis(100)).await;
    assert_eq!(r, Err(FetchError::Timeout));
    assert_eq!(Instant::now(), t1);
}

#[tokio::test(start_paused = true)]
async fn per_minute_window() {
    let l = RateLimiter::new(60, Duration::ZERO);
    let t0 = Instant::now();
    for i in 0..60 {
        l.acquire(&format!("h{i}.test"), t0 + FAR).await.unwrap();
    }
    assert_eq!(Instant::now(), t0, "60 запросов в окне не ждут");
    // 61-й: дедлайн раньше конца окна — Timeout.
    assert_eq!(
        l.acquire("x.test", t0 + Duration::from_secs(30)).await,
        Err(FetchError::Timeout)
    );
    // 61-й с далёким дедлайном ждёт, пока первый запрос не выйдет из окна.
    l.acquire("x.test", t0 + FAR).await.unwrap();
    assert!(Instant::now() >= t0 + Duration::from_secs(60));
    // Окно скользящее: все 60 первых вышли из него разом, следующий не ждёт.
    let t1 = Instant::now();
    l.acquire("y.test", t0 + FAR).await.unwrap();
    assert_eq!(Instant::now(), t1);
}

#[tokio::test(start_paused = true)]
async fn zero_per_minute_is_one() {
    // 0 в конфигурации шлюз отвергает; у чистой структуры — не меньше одного запроса.
    let l = RateLimiter::new(0, Duration::ZERO);
    let t0 = Instant::now();
    l.acquire("a.test", t0 + FAR).await.unwrap();
    assert_eq!(
        l.acquire("a.test", t0 + Duration::from_secs(1)).await,
        Err(FetchError::Timeout)
    );
}
