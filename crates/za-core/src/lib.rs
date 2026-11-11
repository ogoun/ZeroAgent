//! Ядро zero-agent: контекст выполнения run и протокол эффектов.
//!
//! Каждый побочный эффект проходит протокол намерение → выполнение → квитанция
//! (CORE-002) с ключом из `(run, step, effect_seq)` (CORE-004). Завершённые
//! эффекты при повторе отдаются из журнала; незавершённые после сбоя повторяются
//! или, для `NonIdempotent`, превращаются в прерывание `unknown_outcome` (CORE-003),
//! которое закрывает решение человека [`resolve_unknown`]. Ключи можно
//! резервировать заранее ([`RunCtx::reserve`], [`EffectSlot`]) и выполнять слоты
//! одновременно; [`RunCtx::resume_at`] продолжает нумерацию с шага снимка.
//!
//! [`Graph`] выполняет узлы run с переходами `Goto`, `Suspend`, `Done` (D2);
//! [`Interrupt`] с id из ключа эффекта (CORE-007, D16) останавливает граф до
//! решения, модуль [`interrupt`] открывает прерывания и читает их состояние.
//!
//! Инструменты описываются [`ToolSpec`], режим run решает о вызове через [`gate`]
//! (D14), [`ToolRegistry`] выдаёт инструменты по роли и тиру; [`Capability`]
//! объединяет инструменты, инструкции и хуки границы шага. [`Journal`] пишет
//! события и публикует их после COMMIT в [`EventBus`] (D34, D35).
//!
//! Для границы проекта (R1-C1) модуль [`paths`] разбирает и разрешает пути вызовов
//! (D76, D98), модуль [`shell`] (крейт `za-shell`) разбирает команды оболочки
//! и классифицирует необратимые и сетевые (D77, D81), [`Checkpointer`] — контракт чекпоинта рабочей
//! копии (D89).
//!
//! Модуль [`text`] удаляет невидимые символы Unicode из результатов инструментов,
//! ввода пользователя и `AGENTS.md` (SEC-051, D102).
//!
//! [`run_demo`] — детерминированный демонстрационный run для сквозной проверки
//! журнала и протокола эффектов, [`replay_hash`] — хэш его значимых событий.

mod bus;
mod capability;
mod checkpoint;
mod demo;
mod graph;
pub mod interrupt;
pub mod paths;
mod recovery;
mod runtime;
pub mod text;
mod tool;

pub use bus::{AgentEvent, EventBus, Journal};
pub use capability::{BoundaryInfo, Capability, Hook};
pub use checkpoint::{Checkpoint, CheckpointRequest, Checkpointer};
pub use demo::{CrashPoint, DemoConfig, DemoOutcome, replay_hash, run_demo};
pub use graph::{BoxFuture, Exit, Graph, GraphError, Node, NodeId, Transition};
pub use interrupt::{Interrupt, InterruptKind, InterruptState, pause_reason_for};
pub use recovery::{
    NOT_DONE_MESSAGE, Recovery, UNKNOWN_OUTCOME, UnknownDecision, recover, resolve_unknown,
};
pub use runtime::{EffectError, EffectFailure, EffectSlot, EffectSource, EffectSpec, RunCtx};
pub use tool::{
    Access, Approval, DuplicateTool, Gate, Postcondition, RawPath, RiskClass, Subjects, Tool,
    ToolCx, ToolKind, ToolOutput, ToolRegistry, ToolSpec, gate,
};
/// Схема инструмента для модели (поле [`ToolSpec::schema`]): крейты инструментов
/// (`za-tools`) строят её без зависимости от `za-llm`.
pub use za_llm::ToolSchema;
/// Прежнее имя причины паузы; значение живёт в [`za_protocol::pause_reason`].
pub use za_protocol::pause_reason::NEEDS_DECISION;
/// Разбор команд оболочки (D81, D99) — крейт `za-shell`.
pub use za_shell as shell;
