//! Связь с сервером памяти, общая для порта чтения и шлюза записи (R1-M D136, D137, D165).
//!
//! Один объект на клиента: уровень памяти (`watch`), флаги сервера по последнему удачному
//! обнаружению, повторная проба после L0 и счёт «мёртвого хоста». Порт
//! ([`crate::port::ZamMemory`]) и шлюз записи ([`crate::gateway::WriteGateway`]) видят один и
//! тот же уровень: сбой соединения у записи переводит в L0 и чтения, удачная проба у чтения
//! возвращает запись (D165: «при известном L0 — без попыток»).
//!
//! - **L0** — нет соединения, обрыв, «не ZAM» ([`Link::drop_to_l0`]); три `Timeout` подряд без
//!   ответа сервера — тоже L0 ([`Link::note_outcome`], Minor 7 ревью задачи 4).
//! - **Проба** после L0 — `/api/v1/health` не чаще раза в [`PROBE_INTERVAL`] при следующем
//!   обращении ([`Link::available`]); время — [`Clock`] (D165).
//! - Ошибка со статусом уровень не меняет.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::clock::Clock;
use crate::discovery::{Discovery, Level, discover};
use crate::error::{ErrorKind, ZamError};
use crate::transport::ZamClient;

/// Повторная проба после перехода в L0 — не чаще (D136).
pub const PROBE_INTERVAL: Duration = Duration::from_secs(30);
/// `Timeout` подряд без ответа сервера до перехода в L0 («мёртвый хост», Minor 7 ревью
/// задачи 4).
pub const DEAD_HOST_TIMEOUTS: u32 = 3;

/// Изменяемое состояние связи.
#[derive(Debug)]
struct State {
    /// Флаги сервера по последнему удачному обнаружению.
    features: BTreeSet<String>,
    /// Последняя проба или переход в L0 (D136).
    probed_at: Instant,
    /// `Timeout` подряд без ответа сервера.
    timeouts: u32,
}

/// Связь с сервером памяти: уровень, флаги, проба.
pub struct Link {
    client: Arc<ZamClient>,
    clock: Arc<dyn Clock>,
    level: watch::Sender<Level>,
    state: Mutex<State>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("client", &self.client)
            .field("level", &self.level())
            .finish_non_exhaustive()
    }
}

impl Link {
    /// Связь по итогу обнаружения на старте команды (D136).
    pub fn new(client: Arc<ZamClient>, discovery: &Discovery, clock: Arc<dyn Clock>) -> Self {
        let features = discovery
            .info
            .as_ref()
            .map(|i| i.features.clone())
            .unwrap_or_default();
        let state = State {
            features,
            probed_at: clock.now(),
            timeouts: 0,
        };
        let (level, _) = watch::channel(discovery.level);
        Self {
            client,
            clock,
            level,
            state: Mutex::new(state),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Клиент сервера.
    pub fn client(&self) -> &Arc<ZamClient> {
        &self.client
    }

    /// Часы связи (D165).
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Текущий уровень.
    pub fn level(&self) -> Level {
        *self.level.borrow()
    }

    /// Подписка на уровень памяти: смену видит бинарник (D137).
    pub fn subscribe(&self) -> watch::Receiver<Level> {
        self.level.subscribe()
    }

    /// Флаги сервера по последнему удачному обнаружению.
    pub fn features(&self) -> BTreeSet<String> {
        self.state().features.clone()
    }

    /// Есть ли у сервера флаг `name`.
    pub fn has(&self, name: &str) -> bool {
        self.state().features.contains(name)
    }

    /// Можно ли обращаться к серверу: при L0 — проба не чаще раза в [`PROBE_INTERVAL`].
    pub async fn available(&self) -> bool {
        if self.level() != Level::L0 {
            return true;
        }
        let now = self.clock.now();
        {
            let mut st = self.state();
            if now.saturating_duration_since(st.probed_at) < PROBE_INTERVAL {
                return false;
            }
            // Слот пробы занят сразу: параллельный вызов не пробует второй раз.
            st.probed_at = now;
        }
        let d = discover(&self.client).await;
        let Some(info) = d.info.filter(|_| d.level != Level::L0) else {
            self.state().probed_at = self.clock.now();
            return false;
        };
        {
            let mut st = self.state();
            st.features = info.features;
            st.timeouts = 0;
        }
        self.level.send_replace(d.level);
        tracing::info!(level = d.level.as_str(), "память: сервер снова доступен");
        true
    }

    /// Нет соединения, обрыв, «не ZAM» или мёртвый хост — L0 с повторной пробой через
    /// [`PROBE_INTERVAL`].
    pub fn drop_to_l0(&self, e: &ZamError) {
        {
            let mut st = self.state();
            st.probed_at = self.clock.now();
            st.timeouts = 0;
        }
        if self.level.send_replace(Level::L0) != Level::L0 {
            tracing::warn!(error = %e, "zero-agent: память недоступна; записи — в очередь");
        }
    }

    /// Счёт мёртвого хоста по итогу запроса: `Timeout` без ответа сервера — +1, на
    /// [`DEAD_HOST_TIMEOUTS`] подряд — L0; любой ответ сервера (успех или ошибка со статусом)
    /// счёт сбрасывает. Нет соединения, обрыв, «не ZAM» переводят в L0 сами
    /// ([`Link::drop_to_l0`]).
    pub fn note_outcome(&self, e: Option<&ZamError>) {
        let dead = {
            let mut st = self.state();
            match e {
                Some(e) if e.kind == ErrorKind::Timeout => {
                    st.timeouts += 1;
                    st.timeouts >= DEAD_HOST_TIMEOUTS
                }
                _ => {
                    st.timeouts = 0;
                    false
                }
            }
        };
        if let Some(e) = e.filter(|_| dead) {
            tracing::warn!(
                timeouts = DEAD_HOST_TIMEOUTS,
                "память: сервер не отвечает в срок подряд — уровень L0"
            );
            self.drop_to_l0(e);
        }
    }
}
