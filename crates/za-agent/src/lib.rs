//! Цикл агента zero-agent поверх примитивов `za-core` (ADR 0004, D1).
//!
//! Крейт держит код над примитивами: промпты приложения A ([`role_core`],
//! [`render_env`], [`render_task`], [`render_control`], [`escape_untrusted`]), историю
//! диалога ([`Conversation`], атомарная фиксация хода, D10) и порт
//! памяти ([`MemoryPort`], режим L0 — [`NoMemory`]), сборку запроса ([`freeze`],
//! [`first_message`], [`build_request`]: стабильный префикс блоков 1–5), ход модели
//! ([`model_turn`]: эффект `model`, события журнала D8, сигналы цикла [`Signals`]),
//! разбор ответа ([`review`], D27) и отображение ошибок в состояние run
//! ([`exit_for_model_error`], [`exit_for_effect_error`], D11), пакет вызовов
//! инструментов ([`run_tools`]: гейт режима, одобрение, `ask_user`, `finish`, D14–D17). Модель
//! вызывается только через `za_llm::ModelPort`: `za-llm` подключён без HTTP и `Gateway`.

mod conversation;
mod exit;
mod memory;
mod prompt;
mod request;
mod tools;
mod turn;

pub use conversation::{Conversation, HistoryError, LoadError, Persisted};
pub use exit::{LoopExit, exit_for_effect_error, exit_for_model_error};
pub use memory::{MemoryLevel, MemoryPort, NoMemory, Recall, memory_budget};
pub use prompt::{
    AGENTS_MD_LIMIT, BRANCH_MAX_CHARS, ControlAction, ControlFrom, ENV_LIMIT, EnvInfo, LEFT_ANGLES,
    MODE_LIMIT, PROMPT_VERSION, RIGHT_ANGLES, TaskBlock, WorkMode, agents_md_budget, approx_tokens,
    env_in_role_limit, escape_untrusted, fit_agents_md, git_branch, mode_overlay, prompt_files,
    prompt_texts_hash, reasoning_for, render_control, render_env, render_task, role_core,
    role_limit, upper_tokens, utility_rules,
};
pub use request::{
    BlockHashes, CAPABILITY_LIMIT, FrozenPrefix, PrefixError, RunSetup, TurnOptions, build_request,
    first_message, freeze, request_hash,
};
pub use tools::{
    ASK_USER_TOOL, FINISH_TOOL, FinishArgs, ToolEnv, ToolsOutcome, builtin_tools, run_tools,
};
pub use turn::{
    CallVerdict, ControlFlags, DELTA_FLUSH_AFTER, DELTA_FLUSH_BYTES, DeltaBuffer,
    HEARTBEAT_POSITION_EVERY, MODEL_EFFECT, PlannedCall, RequestRefs, Reviewed, Signal, SignalTx,
    Signals, TurnError, TurnOutput, model_turn, output_tps, report_parse_errors, review,
    signal_channel,
};
