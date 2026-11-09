//! Цикл агента zero-agent поверх примитивов `za-core` (ADR 0004, D1).
//!
//! Крейт держит код над примитивами: промпты приложения A ([`role_core`],
//! [`render_env`], [`render_task`], [`render_control`], [`escape_untrusted`]) и порт
//! памяти ([`MemoryPort`], режим L0 — [`NoMemory`]). Модель вызывается только через
//! `za_llm::ModelPort`: `za-llm` подключён без HTTP и `Gateway`.

mod memory;
mod prompt;

pub use memory::{MemoryLevel, MemoryPort, NoMemory, memory_budget};
pub use prompt::{
    ControlAction, ControlFrom, ENV_LIMIT, EnvInfo, MODE_LIMIT, PROMPT_VERSION, TaskBlock,
    WorkMode, approx_tokens, escape_untrusted, git_branch, mode_overlay, prompt_files,
    prompt_texts_hash, reasoning_for, render_control, render_env, render_task, role_core,
    role_limit, utility_rules,
};
