//! Крейт za-llm: работа с моделью. Пока — рендер стабильного префикса промпта и его хэш.

pub mod prefix;

pub use prefix::{PromptBlocks, ToolSchema, prefix_hash, render_system, render_tools};
