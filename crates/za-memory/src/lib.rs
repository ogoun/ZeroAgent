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
//! Модули задачи 4:
//! - [`session`] — сессия run `za-<project_id>-<run>` и её регистрация (D139);
//! - [`recall`] — запросы `/recall` без `output_limit`, запас бюджета, разбор ответа и данные
//!   события `memory.recall` (D140, D142, D168);
//! - [`fit`] — укладка ответа в бюджет на клиенте, чистая функция (D141);
//! - [`ideas`] — режим идей и его выбор по признакам сервера (D148, D169);
//! - [`port`] — `ZamMemory`, порт памяти цикла `za_core::memory::MemoryPort` (D138).
//!
//! Модули задачи 5:
//! - [`vault`] — записи Vault по имени `key_ref`: Vault решения, затем личный; раскрытие с
//!   сессией run или без неё; записи окружения — не ключи (D143, D144);
//! - [`keys`] — ключи, нужные команде: раскрытие, кэш на процесс, маска процесса до возврата
//!   значения (D143, D160).
//!
//! Модули задачи 6 — шлюз записи, единственный писатель модели и рантайма:
//! - [`link`] — связь с сервером (уровень, флаги, проба), общая для порта и шлюза (D136, D165);
//! - [`taxonomy`] — обязательные поля и формы по виду записи до сети (D145, П8);
//! - [`gate`] — одна функция решения по всем путям записи (D163, D170, П6);
//! - [`scope`] — `ScopeGuard`, область решения на всех параметрах, задающих пространство
//!   (D164, П10);
//! - [`gateway`] — `WriteGateway`: доверие цели, доказательство, маска, `Idempotency-Key` из
//!   ключа эффекта, очередь `WriteQueue` (до outbox — `DirectQueue`), разбор идеи мимо очереди
//!   (D145, D165, D171).
//!
//! Outbox (задача 7), инструменты модели и операции идей (задача 8), снимок сессии (задача 9) —
//! следующие задачи плана.

pub mod clock;
pub mod config;
pub mod discovery;
pub mod error;
pub mod fit;
pub mod gate;
pub mod gateway;
pub mod ideas;
pub mod keys;
pub mod link;
pub mod port;
pub mod recall;
pub mod scope;
pub mod session;
pub mod taxonomy;
pub mod transport;
pub mod vault;

#[cfg(feature = "test-hooks")]
pub use clock::ManualClock;
pub use clock::{Clock, TokioClock};
pub use config::{ConfigError, MemoryConfig, TOKEN_ENV, Token};
pub use discovery::{
    Discovery, Level, MemoryStatus, ServerInfo, SolutionState, TokenState, VaultState, discover,
    status,
};
pub use error::{ErrorKind, ZamError, parse_mcp_error};
pub use fit::{Fitted, fit_digest};
pub use gate::{TargetTrust, Verdict, WriteCx, WriteOp, decide};
pub use gateway::{
    DirectQueue, GatewayParts, QueuedWrite, Submit, WriteGateway, WriteOutcome, WriteQueue,
};
pub use ideas::{IdeasMode, ideas_mode};
pub use keys::{KeyState, Revealed, VaultKeys, process_mask};
pub use link::Link;
pub use port::{ZamMemory, ZamParts};
pub use recall::{RecallReport, fetch_budget};
pub use scope::{ScopeAccess, ScopeGuard};
pub use session::{RunBinding, session_id};
pub use taxonomy::{Checked, Flags, TaxonomyError, check_entry};
pub use transport::{Ctx, Method, ZamClient};
pub use vault::{SolutionVault, VaultClient, VaultError, VaultItem, VaultScope};
