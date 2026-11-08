//! Крейт za-llm: работа с моделью. Пока — рендер стабильного префикса промпта и его хэш,
//! зонд llama.cpp (`probe_llama`).

pub mod prefix;
pub mod probe;

pub use prefix::{PromptBlocks, ToolSchema, prefix_hash, render_system, render_tools};
pub use probe::{LlamaProbe, ProbeError, probe_llama};
