//! Сетевой шлюз чтения (R1-C2 D107, SEC-047): единственный путь агента в веб.
//!
//! - [`WebGateway`] — один объект на процесс (`Arc`): HTTP-клиент, фильтр адресов в
//!   резолвере, редиректы, лимиты, `robots.txt`, журнал обращений ([`Fetched::requests`]).
//!   Клиент `reqwest` строится только в `gateway.rs` (D129).
//! - [`check_ip`] — разрешающий список (D108): только глобально маршрутизируемые адреса
//!   по реестрам IANA special-purpose; всё прочее — отказ `local or private address`.
//! - [`parse_url`] — схема, учётные данные, длина, хост через
//!   `za_core::net::normalize_host` (D124), IP-литерал и `localhost` — сразу.
//! - [`RateLimiter`] — окно в минуту на процесс и интервал по хосту.
//! - [`robots_allows`] — свой разбор `robots.txt` (RFC 9309).
//!
//! Враждебный HTML шлюз не разбирает (D127). Тестовые подмены резолвера существуют только
//! с признаком сборки `test-hooks` (D128).

mod addr;
mod gateway;
mod limit;
mod robots;

use std::time::Duration;

pub use addr::{Block, IANA_SNAPSHOT, IPV4_SPECIAL, IPV6_SPECIAL, check_ip, parse_url};
pub use gateway::WebGateway;
pub use limit::RateLimiter;
pub use robots::{ROBOTS_MAX_BYTES, ROBOTS_MAX_RULES, robots_allows};
use za_core::NetRequest;

/// Типы резолвера `reqwest` для тестовой подмены внутреннего резолвера (D128).
#[cfg(feature = "test-hooks")]
pub mod dns {
    pub use reqwest::dns::{Addrs, Name, Resolve, Resolving};
}

/// Причина отказа фильтра адресов (D108); текст для модели — `blocked: <причина>`.
pub const LOCAL_ADDRESS: &str = "local or private address";

/// Настройки шлюза: раздел `[web]` глобальной конфигурации (D107, D122).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebConfig {
    /// Соблюдать `robots.txt`.
    pub robots: bool,
    /// Предел тела ответа, байт: дальше чтение обрывается (`Body::truncated`).
    pub max_bytes: u64,
    /// Запросов в минуту на процесс (не меньше 1).
    pub requests_per_minute: u32,
    /// Разрешённые порты (межпротокольные запросы к SMTP, Redis и подобным через GET).
    pub ports: Vec<u16>,
    /// Не чаще одного запроса к хосту за этот интервал.
    pub host_interval: Duration,
    /// Предел установки соединения.
    pub connect_timeout: Duration,
    /// Экземпляр поставщика поиска из глобальной конфигурации (D126, задача 9).
    pub search_endpoint: Option<url::Url>,
}

impl Default for WebConfig {
    /// Значения D107.
    fn default() -> Self {
        Self {
            robots: true,
            max_bytes: 5 * 1024 * 1024,
            requests_per_minute: 60,
            ports: vec![80, 443, 8080, 8443],
            host_interval: Duration::from_millis(500),
            connect_timeout: Duration::from_secs(10),
            search_endpoint: None,
        }
    }
}

/// Итог вызова шлюза: ответ или ошибка и журнал обращений вызова (D112) — в том числе
/// при `Timeout`: шлюз сам соблюдает дедлайн (D107).
#[derive(Debug)]
pub struct Fetched {
    pub result: Result<Body, FetchError>,
    pub requests: Vec<NetRequest>,
}

/// Ответ сервера (любой код, кроме исполненного редиректа).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Body {
    /// Адрес последнего запроса (после редиректов на тот же хост).
    pub final_url: url::Url,
    pub status: u16,
    /// Заголовок `Content-Type` как есть (тип определяет задача 7, D109).
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
    /// Тело оборвано на `max_bytes`.
    pub truncated: bool,
}

/// Ошибка шлюза. [`FetchError::text`] — текст для модели (английский, ASCII, D23).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("only http and https URLs are allowed")]
    Scheme,
    #[error("credentials in URL are not allowed")]
    Credentials,
    /// Отказ до соединения: адрес, порт, не тот поставщик.
    #[error("blocked: {0}")]
    Blocked(String),
    /// Редирект на другой хост или на `http` с `https` не исполняется (D107).
    #[error("redirected to {0}; call web_fetch with it if it is the page you need")]
    Redirect(String),
    /// `unavailable` (5xx или сбой) или `disallowed` (правило).
    #[error("blocked by robots.txt: {0}")]
    Robots(&'static str),
    /// `Content-Encoding`, отличный от `identity` (без попытки распаковки).
    #[error("unsupported content encoding {0}")]
    Encoding(String),
    /// Дедлайн вызова истёк.
    #[error("request timed out")]
    Timeout,
    #[error("network error: {0}")]
    Network(String),
}

impl FetchError {
    /// Текст для модели (D107).
    pub fn text(&self) -> String {
        self.to_string()
    }

    /// Причина для журнала обращений (`NetRequest::reason`).
    fn reason(&self) -> String {
        match self {
            Self::Blocked(r) => r.clone(),
            Self::Robots(r) => format!("robots.txt: {r}"),
            Self::Timeout => "timeout".into(),
            Self::Network(r) => r.clone(),
            e => e.text(),
        }
    }
}

/// Ошибка построения шлюза.
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("web config: {0}")]
    Config(String),
    #[error("http client: {0}")]
    Client(String),
}
