//! Цикл агента zero-agent поверх примитивов `za-core` (ADR 0004, D1).
//!
//! Крейт держит код над примитивами: промпты приложения A ([`role_core`],
//! [`render_env`], [`render_task`], [`render_control`], [`escape_untrusted`]), историю
//! диалога ([`Conversation`], атомарная фиксация хода, D10), порт памяти
//! ([`MemoryPort`] из `za-core`, режим L0 — [`NoMemory`]) и память в цикле (R1-M D140,
//! D142, D161, D168: дайджест и подсказки эффектами, отложенный повтор, потолок подсказок
//! эпохи, шаги журналов R1-C2 без новых эффектов, [`MemoryState`]), сборку запроса ([`freeze`],
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
//! инспектор запроса хода [`inspect`] и метрики run [`run_metrics`] (D52, D53), детекторы
//! повтора, цикла и застоя [`DetectorState`] (D54–D56), повтора текста и бесконечного
//! рассуждения в стриме [`StreamWatch`] (D58, D59), обещания без действия
//! [`promise_without_action`], ошибок инструмента, порчи JSON и падения кэша (D57,
//! D60–D62), движок политики [`decide`] (правила, гранты [`Grant`], защищённые пути,
//! доверие и метка `tainted`, R1-C1 D77–D83), скиллы (`activate_skill`
//! [`skills_capability`], `/имя`, роутер карточек tier small, защита активных скиллов от
//! удаления результатов и компакции, R1-C2 D117–D119). Модель
//! вызывается только через `za_llm::ModelPort`: `za-llm` подключён без HTTP и `Gateway`.

mod budget;
mod context;
mod control;
mod conversation;
mod detect;
mod driver;
mod exit;
mod exposure;
mod inspect;
mod memory;
mod policy;
mod prompt;
mod request;
mod resume;
mod skills;
mod state;
mod tools;
mod turn;

/// Уникальный каталог unit-тестов — общий помощник workspace.
#[cfg(test)]
#[path = "../../../test-support/unique_dir.rs"]
mod test_unique_dir;

pub use budget::{Spent, check_budget};
pub use context::{
    ANSWER_RESERVE, CATALOG_LIMIT, COMPACT_RUBRIC, CompactInput, Compacted, ContextBudget,
    ContextConfig, ContextConfigError, ContextMeter, Estimate, OutputPlan, SPILL_DIR, SpillCfg,
    SpillResult, UserInput, compact, output_plan, prune, spill, spill_limit_bytes, spill_name,
    summary_block, thinking_budget, truncation_suspected,
};
pub use control::{ControlRx, InputMode, RunControl, control_channel, submit_input};
pub use conversation::{Conversation, HistoryError, LoadError, Persisted, cut_points, split_tail};
pub use detect::{
    CYCLE_NUDGE, Degenerate, DetectorState, PROMISE_NUDGE, REPEAT_MARK, REPEAT_NUDGE,
    REPEAT_REJECT, Reaction, StreamWatch, THINKING_TIMEOUT, call_fingerprint,
    promise_without_action, retry_params, thinking_timeout,
};
pub use driver::{
    CrashHook, CrashPoint, DEFAULT_N_CTX, FaultHook, FaultPoint, LoopDeps, LoopError, LoopOutcome,
    RunSpec, start_run,
};
pub use exit::{LoopExit, exit_for_effect_error, exit_for_model_error};
pub use exposure::{Exposure, RuleOfTwoNotice, rule_of_two_marked};
pub use inspect::{BlockCost, InspectError, Inspection, RunMetrics, inspect, run_metrics};
pub use memory::{
    CommandNote, IdeasSignal, MemoryLevel, MemoryPort, NoMemory, NoteOutcome, POST_TOOL_EVENT,
    PROMPT_EVENT, Recall, RecallReply, SessionNote, memory_block, memory_budget,
};
pub use policy::{
    Decision, Grant, GrantConfig, GrantMode, GrantSource, PolicyConfig, PolicyCx, PolicyError,
    PolicySnapshot, Rule, RuleAction, RuleSource, command_path, covers, decide, grants_text,
    parse_grant_flag, subject_paths,
};
pub use prompt::{
    AGENTS_MD_LIMIT, BRANCH_MAX_CHARS, BudgetCase, ControlAction, ControlFrom, ENV_LIMIT, EnvInfo,
    LEFT_ANGLES, MODE_LIMIT, PROMPT_VERSION, RIGHT_ANGLES, SMALL_PREFIX_LIMIT, SMALL_TOOLS_SLOT,
    STANDARD_MIN_WINDOW, STANDARD_TOOLS_LIMIT, TaskBlock, WorkMode, agents_md_budget,
    approx_tokens, budget_cases, env_in_role_limit, escape_untrusted, fit_agents_md, git_branch,
    max_env, mode_overlay, prompt_files, prompt_texts_hash, prompt_texts_hash_for, reasoning_for,
    render_control, render_env, render_task, role_core, role_limit, runtime_texts,
    runtime_texts_with, small_prefix_cases, standard_tools_cases, upper_tokens, utility_rules,
};
pub use request::{
    BlockHashes, BlockSizes, CAPABILITY_LIMIT, FrozenPrefix, PrefixError, RunSetup, TurnOptions,
    build_request, first_message, first_message_with, freeze, freeze_for, grants_block,
    request_hash,
};
pub use resume::{ResumeChange, resume_run, state_block};
pub use skills::{
    ACTIVATE_SKILL_TOOL, CARD_LIMIT, SKILL_OUTPUT_LIMIT, SKILLS_CAPABILITY, SKILLS_DISABLED,
    activate_skill_tool, catalog_payload, skill_output_limit, skill_recall_limit,
    skills_capability, skills_withdrawn_text,
};
pub use state::{
    ActiveSkill, AtBoundary, AuditHead, Counters, LoopState, MemoryState, Next, PendingExit,
    SNAPSHOT_V, Snapshot,
};
pub use tools::{
    ASK_USER_TOOL, FINISH_TOOL, FinishArgs, REQUEST_GRANT_TOOL, ToolEnv, ToolsOutcome, TrustSetFn,
    UNTRUSTED_MARK, builtin_tools, request_grant_tool, run_tools,
};
pub use turn::{
    COMPACT_EFFECT, COMPACT_KIND, CallVerdict, ControlFlags, DELTA_FLUSH_AFTER, DELTA_FLUSH_BYTES,
    DeltaBuffer, HEARTBEAT_POSITION_EVERY, MODEL_EFFECT, PlannedCall, RequestRefs, Reviewed,
    Signal, SignalTx, Signals, TurnError, TurnOutput, model_turn, model_turn_with, output_tps,
    report_parse_errors, review, signal_channel,
};
