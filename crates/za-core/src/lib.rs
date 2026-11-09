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
//! [`run_demo`] — детерминированный демонстрационный run для сквозной проверки
//! журнала и протокола эффектов, [`replay_hash`] — хэш его значимых событий.

mod demo;
mod recovery;
mod runtime;

pub use demo::{CrashPoint, DemoConfig, DemoOutcome, replay_hash, run_demo};
pub use recovery::{
    NOT_DONE_MESSAGE, Recovery, UNKNOWN_OUTCOME, UnknownDecision, recover, resolve_unknown,
};
pub use runtime::{EffectError, EffectSlot, EffectSource, EffectSpec, RunCtx};
/// Прежнее имя причины паузы; значение живёт в [`za_protocol::pause_reason`].
pub use za_protocol::pause_reason::NEEDS_DECISION;
