//! Крейт za-llm: работа с моделью.
//!
//! Без функций — чистые части: канонический формат сообщений (`types`), возможности
//! провайдера (`caps`), таксономия ошибок (`error`), трейт `ModelPort` и стрим (`port`),
//! детерминированная `ScriptedModel` для тестов (`scripted`), стабильный префикс
//! промпта (`prefix`), нормализация вызовов инструментов (`normalize`). Функция
//! `http` добавляет транспорт, SSE и секреты (`http`) и зонды (`probe`), коннекторы
//! (`wire`, внутренние, D38), `gateway` — очередь и `Gateway` (включена по умолчанию).

pub mod caps;
pub mod error;
#[cfg(feature = "http")]
pub mod http;
pub mod normalize;
pub mod port;
pub mod prefix;
#[cfg(feature = "http")]
pub mod probe;
pub mod scripted;
pub mod types;
#[cfg(feature = "http")]
pub(crate) mod wire;

/// Коннекторы для интеграционных тестов крейта (D38). Не API: рабочий код пользуется
/// моделью только через `Gateway`.
#[cfg(feature = "http")]
#[doc(hidden)]
pub mod __test {
    pub use crate::wire::*;
}

pub use caps::{
    BackendKind, CachedTokensField, ContextSource, PoolKind, ProviderCapabilities, ReasoningField,
    TextFormat, ThinkingSupport, TokenCountSource, ToolFormat, ToolIdFormat, Wire, default_caps,
};
pub use error::{ErrorClass, ModelError};
#[cfg(feature = "http")]
pub use http::{EnvSecrets, Secret, SecretSource};
pub use port::{AbortOnDrop, BoxFuture, ModelCall, ModelPort, ModelStream, RequestMeta};
pub use prefix::{PromptBlocks, ToolSchema, prefix_hash, render_system, render_tools};
#[cfg(feature = "http")]
pub use probe::{LlamaProbe, ProbeError, probe_llama};
pub use scripted::ScriptedModel;
pub use types::{
    ChatRequest, CostSource, Effort, EndpointId, FinishReason, Message, ModelResponse, Part,
    QueueStatus, ReasoningMode, ReplayBlob, ResourceKindName, Role, Sampling, StreamEvent,
    ToolCall, ToolChoice, Usage, WaitState,
};
