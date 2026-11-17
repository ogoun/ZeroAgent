//! Часы клиента памяти (R1-M D165): `now()` и `sleep(d)`.
//!
//! Через них идут все интервалы клиента: повторная проба сервера не чаще раза в 30 с (D136),
//! паузы повторов записи (D165), ожидание Vault бинарником (D143). В продукте — tokio
//! ([`TokioClock`]), в тестах — ручные часы ([`ManualClock`], признак `test-hooks`): время
//! двигает тест, без сокетов под паузой tokio и без ожидания по таймеру.

use std::time::{Duration, Instant};

use za_core::BoxFuture;

/// Источник времени и сна.
pub trait Clock: Send + Sync {
    /// Текущий момент.
    fn now(&self) -> Instant;
    /// Будущее, готовое через `d`.
    fn sleep(&self, d: Duration) -> BoxFuture<'_, ()>;
}

/// Часы tokio: `Instant::now()` и `tokio::time::sleep`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioClock;

impl Clock for TokioClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, d: Duration) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(d))
    }
}

#[cfg(feature = "test-hooks")]
pub use manual::ManualClock;

#[cfg(feature = "test-hooks")]
mod manual {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use tokio::sync::{Notify, oneshot};
    use za_core::BoxFuture;

    use super::Clock;

    /// Ручные часы тестов (D165): время стоит, пока тест не вызовет [`ManualClock::advance`];
    /// сон просыпается, когда его срок наступил. Тест ждёт засыпания по событию
    /// ([`ManualClock::wait_sleepers`]), а не по таймеру.
    #[derive(Debug)]
    pub struct ManualClock {
        state: Mutex<State>,
        slept: Notify,
    }

    #[derive(Debug)]
    struct State {
        now: Instant,
        sleepers: Vec<(Instant, oneshot::Sender<()>)>,
    }

    impl Default for ManualClock {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ManualClock {
        pub fn new() -> Self {
            Self {
                state: Mutex::new(State {
                    now: Instant::now(),
                    sleepers: Vec::new(),
                }),
                slept: Notify::new(),
            }
        }

        fn state(&self) -> std::sync::MutexGuard<'_, State> {
            self.state.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// Сдвигает время на `d` и будит сны, чей срок наступил.
        pub fn advance(&self, d: Duration) {
            let mut s = self.state();
            s.now += d;
            let now = s.now;
            let (due, rest): (Vec<_>, Vec<_>) =
                s.sleepers.drain(..).partition(|(at, _)| *at <= now);
            s.sleepers = rest.into_iter().filter(|(_, tx)| !tx.is_closed()).collect();
            drop(s);
            for (_, tx) in due {
                let _ = tx.send(());
            }
        }

        /// Число спящих (брошенные сны не в счёт).
        pub fn sleepers(&self) -> usize {
            self.state()
                .sleepers
                .iter()
                .filter(|(_, tx)| !tx.is_closed())
                .count()
        }

        /// Ждёт, пока спящих станет не меньше `n`.
        pub async fn wait_sleepers(&self, n: usize) {
            loop {
                let notified = self.slept.notified();
                if self.sleepers() >= n {
                    return;
                }
                notified.await;
            }
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            self.state().now
        }

        fn sleep(&self, d: Duration) -> BoxFuture<'_, ()> {
            if d.is_zero() {
                return Box::pin(std::future::ready(()));
            }
            let (tx, rx) = oneshot::channel();
            {
                let mut s = self.state();
                let at = s.now + d;
                s.sleepers.push((at, tx));
            }
            self.slept.notify_waiters();
            Box::pin(async move {
                let _ = rx.await;
            })
        }
    }
}
