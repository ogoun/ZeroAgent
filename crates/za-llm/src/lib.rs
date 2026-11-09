//! Крейт za-llm: работа с моделью.
//!
//! Без функций — чистые части: канонический формат сообщений (`types`), возможности
//! провайдера (`caps`), таксономия ошибок (`error`), трейт `ModelPort` и стрим (`port`),
//! детерминированная `ScriptedModel` для тестов (`scripted`), стабильный префикс
//! промпта (`prefix`). Функция `http` добавляет зонды (`probe`), `gateway` — очередь
//! и `Gateway` (включена по умолчанию).

pub mod caps;
pub mod error;
pub mod port;
pub mod prefix;
#[cfg(feature = "http")]
pub mod probe;
pub mod scripted;
pub mod types;

pub use caps::{
    BackendKind, CachedTokensField, ContextSource, PoolKind, ProviderCapabilities, ReasoningField,
    TextFormat, ThinkingSupport, TokenCountSource, ToolFormat, ToolIdFormat, Wire, default_caps,
};
pub use error::{ErrorClass, ModelError};
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
