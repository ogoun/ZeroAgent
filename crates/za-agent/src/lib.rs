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
//! инструментов ([`run_tools`]: гейт режима, одобрение, `ask_user`, `finish`, D14–D17),
//! бюджеты run ([`check_budget`], [`Spent`], D19),
//! драйвер цикла ([`start_run`]: граница шага, ходы, завершение и пауза, снимки
//! [`Snapshot`], D4, D5, D18, D31–D33) и управление run ([`RunControl`]: пауза,
//! отмена, ввод [`InputMode`] и поправка, D25, D26, D36), продолжение run
//! ([`resume_run`]: реплей после сбоя, выход из паузы и бюджета, блок [`state_block`],
//! D4–D6, CORE-006, CORE-110), контекст модели ([`ContextBudget`], [`output_plan`],
//! [`ContextMeter`], раздел [`ContextConfig`], выгрузка вывода [`spill`] и удаление
//! старых результатов [`prune`], компакция [`compact`], R1-B2 D40–D50, D65, D66),
//! инспектор запроса хода [`inspect`] и метрики run [`run_metrics`] (D52, D53). Модель
//! вызывается только через `za_llm::ModelPort`: `za-llm` подключён без HTTP и `Gateway`.

mod budget;
mod context;
mod control;
mod conversation;
mod driver;
mod exit;
mod inspect;
mod memory;
mod prompt;
mod request;
mod resume;
mod state;
mod tools;
mod turn;

pub use budget::{Spent, check_budget};
pub use context::{
    ANSWER_RESERVE, COMPACT_RUBRIC, CompactInput, Compacted, ContextBudget, ContextConfig,
    ContextConfigError, ContextMeter, Estimate, OutputPlan, SPILL_DIR, SpillCfg, SpillResult,
    UserInput, compact, output_plan, prune, spill, spill_limit_bytes, spill_name, summary_block,
    thinking_budget, truncation_suspected,
};
pub use control::{ControlRx, InputMode, RunControl, control_channel, submit_input};
pub use conversation::{Conversation, HistoryError, LoadError, Persisted, cut_points, split_tail};
pub use driver::{CrashHook, CrashPoint, LoopDeps, LoopError, LoopOutcome, RunSpec, start_run};
pub use exit::{LoopExit, exit_for_effect_error, exit_for_model_error};
pub use inspect::{BlockCost, InspectError, Inspection, RunMetrics, inspect, run_metrics};
pub use memory::{MemoryLevel, MemoryPort, NoMemory, Recall, memory_budget};
pub use prompt::{
    AGENTS_MD_LIMIT, BRANCH_MAX_CHARS, BudgetCase, ControlAction, ControlFrom, ENV_LIMIT, EnvInfo,
    LEFT_ANGLES, MODE_LIMIT, PROMPT_VERSION, RIGHT_ANGLES, TaskBlock, WorkMode, agents_md_budget,
    approx_tokens, budget_cases, env_in_role_limit, escape_untrusted, fit_agents_md, git_branch,
    max_env, mode_overlay, prompt_files, prompt_texts_hash, reasoning_for, render_control,
    render_env, render_task, role_core, role_limit, runtime_texts, upper_tokens, utility_rules,
};
pub use request::{
    BlockHashes, BlockSizes, CAPABILITY_LIMIT, FrozenPrefix, PrefixError, RunSetup, TurnOptions,
    build_request, first_message, freeze, request_hash,
};
pub use resume::{ResumeChange, resume_run, state_block};
pub use state::{AtBoundary, Counters, LoopState, Next, PendingExit, SNAPSHOT_V, Snapshot};
pub use tools::{
    ASK_USER_TOOL, FINISH_TOOL, FinishArgs, ToolEnv, ToolsOutcome, builtin_tools, run_tools,
};
pub use turn::{
    COMPACT_EFFECT, CallVerdict, ControlFlags, DELTA_FLUSH_AFTER, DELTA_FLUSH_BYTES, DeltaBuffer,
    HEARTBEAT_POSITION_EVERY, MODEL_EFFECT, PlannedCall, RequestRefs, Reviewed, Signal, SignalTx,
    Signals, TurnError, TurnOutput, model_turn, output_tps, report_parse_errors, review,
    signal_channel,
};
