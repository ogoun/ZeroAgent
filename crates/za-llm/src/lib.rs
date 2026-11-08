//! Крейт za-llm: работа с моделью. Пока — рендер стабильного префикса промпта и его хэш,
//! зонд llama.cpp (`probe_llama`, функция `probe`, включена по умолчанию).

pub mod prefix;
#[cfg(feature = "probe")]
pub mod probe;

pub use prefix::{PromptBlocks, ToolSchema, prefix_hash, render_system, render_tools};
#[cfg(feature = "probe")]
pub use probe::{LlamaProbe, ProbeError, probe_llama};
