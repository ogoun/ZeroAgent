//! Клиент памяти ZeroAIMemory (R1-M, D133; ADR 0007).
//!
//! Модули задачи 2 плана R1-M:
//! - [`config`] — `[memory]` глобальной конфигурации, токен `ZA_MEMORY_TOKEN` (D134);
//! - [`clock`] — часы и сон для интервалов клиента, ручные часы тестов (D165);
//! - [`error`] — вид ошибки по коду и `type` problem+json, формы ошибок MCP (D137);
//! - [`transport`] — HTTP-клиент без прокси и редиректов, заголовки, сроки, REST и MCP
//!   (D133, D137): единственное место, где строится `reqwest::Client`;
//! - [`discovery`] — `/api/v1/health`, уровни L0–L2 по флагам, статус токена, решения,
//!   Vault и подсказок (D136).
//!
//! Сессия, `/recall`, укладка в бюджет и порт памяти (задача 4), Vault и ключи (задача 5),
//! шлюз записи (задача 6), outbox (задача 7), инструменты модели (задача 8), снимок сессии
//! (задача 9) — следующие задачи плана.

pub mod clock;
pub mod config;
pub mod discovery;
pub mod error;
pub mod transport;

#[cfg(feature = "test-hooks")]
pub use clock::ManualClock;
pub use clock::{Clock, TokioClock};
pub use config::{ConfigError, MemoryConfig, TOKEN_ENV, Token};
pub use discovery::{
    Discovery, Level, MemoryStatus, ServerInfo, SolutionState, TokenState, VaultState, discover,
    status,
};
pub use error::{ErrorKind, ZamError, parse_mcp_error};
pub use transport::{Ctx, Method, ZamClient};
