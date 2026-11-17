//! Лимитер частоты шлюза (D107): скользящее окно в минуту на процесс и не чаще одного
//! запроса к хосту за интервал. Чистая структура без сети (D121): тесты — под
//! виртуальным временем tokio.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::FetchError;

/// Окно счёта запросов.
const WINDOW: Duration = Duration::from_secs(60);
/// Сколько последних времён по хостам держать без чистки.
const HOSTS_KEPT: usize = 1024;

/// Лимитер частоты: `acquire` ждёт, пока запрос не уложится в оба предела, но не дольше
/// дедлайна вызова (ожидание входит в тайм-аут вызова).
#[derive(Debug)]
pub struct RateLimiter {
    per_minute: usize,
    host_interval: Duration,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// Времена запросов последней минуты, по возрастанию.
    window: VecDeque<Instant>,
    /// Время последнего запроса к хосту.
    last: HashMap<String, Instant>,
}

impl RateLimiter {
    /// `per_minute` меньше 1 считается за 1 (шлюз такую конфигурацию отвергает раньше).
    pub fn new(per_minute: u32, host_interval: Duration) -> Self {
        Self {
            per_minute: per_minute.max(1) as usize,
            host_interval,
            state: Mutex::new(State::default()),
        }
    }

    /// Занять место для запроса к `host` (нормализованный хост). Если раньше дедлайна
    /// места не будет — сразу `Err(FetchError::Timeout)`, без ожидания.
    pub async fn acquire(&self, host: &str, deadline: Instant) -> Result<(), FetchError> {
        loop {
            let at = {
                let mut st = self.state.lock().await;
                let now = Instant::now();
                while st.window.front().is_some_and(|t| *t + WINDOW <= now) {
                    st.window.pop_front();
                }
                let mut at = now;
                if st.window.len() >= self.per_minute
                    && let Some(first) = st.window.front()
                {
                    at = at.max(*first + WINDOW);
                }
                if let Some(last) = st.last.get(host) {
                    at = at.max(*last + self.host_interval);
                }
                if at <= now {
                    st.window.push_back(now);
                    if st.last.len() >= HOSTS_KEPT {
                        let interval = self.host_interval;
                        st.last.retain(|_, t| *t + interval > now);
                    }
                    st.last.insert(host.to_string(), now);
                    return Ok(());
                }
                at
            };
            if at > deadline {
                return Err(FetchError::Timeout);
            }
            tokio::time::sleep_until(at).await;
        }
    }
}
