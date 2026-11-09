//! Цикл агента zero-agent поверх примитивов `za-core` (ADR 0004, D1).
//!
//! Крейт держит код над примитивами: промпты приложения A ([`role_core`],
//! [`render_env`], [`render_task`], [`render_control`], [`escape_untrusted`]), историю
//! диалога ([`Conversation`], атомарная фиксация хода, D10) и порт
//! памяти ([`MemoryPort`], режим L0 — [`NoMemory`]), сборку запроса ([`freeze`],
//! [`first_message`], [`build_request`]: стабильный префикс блоков 1–5). Модель
//! вызывается только через `za_llm::ModelPort`: `za-llm` подключён без HTTP и `Gateway`.

mod conversation;
mod memory;
mod prompt;
mod request;

pub use conversation::{Conversation, HistoryError, LoadError, Persisted};
pub use memory::{MemoryLevel, MemoryPort, NoMemory, Recall, memory_budget};
pub use prompt::{
    AGENTS_MD_LIMIT, BRANCH_MAX_CHARS, ControlAction, ControlFrom, ENV_LIMIT, EnvInfo, LEFT_ANGLES,
    MODE_LIMIT, PROMPT_VERSION, RIGHT_ANGLES, TaskBlock, WorkMode, agents_md_budget, approx_tokens,
    env_in_role_limit, escape_untrusted, fit_agents_md, git_branch, mode_overlay, prompt_files,
    prompt_texts_hash, reasoning_for, render_control, render_env, render_task, role_core,
    role_limit, upper_tokens, utility_rules,
};
pub use request::{
    BlockHashes, FrozenPrefix, RunSetup, TurnOptions, build_request, first_message, freeze,
    request_hash,
};
