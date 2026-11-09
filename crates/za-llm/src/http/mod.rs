//! HTTP-слой коннекторов (функция `http`): транспорт с таймаутами D26, разбор SSE,
//! классификация ошибок по статусу, заголовки лимитов, секреты.

mod limits;
mod secret;
mod sse;
mod transport;

pub use limits::{
    MAX_PROVIDER_PAUSE, ProviderLimits, classify_status, parse_rate_limits, parse_retry_after,
};
pub use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
pub use secret::{EnvSecrets, Secret, SecretSource, missing_key_reason, redact, secret_env_name};
pub use sse::{SseEvent, SseParser};
pub use transport::{HttpTransport, SseResponse, Timeouts};
