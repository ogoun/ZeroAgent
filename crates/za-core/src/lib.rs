//! Ядро zero-agent: контекст выполнения run и протокол эффектов.
//!
//! Каждый побочный эффект проходит протокол намерение → выполнение → квитанция
//! (CORE-002) с ключом из `(run, step, effect_seq)` (CORE-004). Завершённые
//! эффекты при повторе отдаются из журнала; незавершённые после сбоя повторяются
//! или, для `NonIdempotent`, превращаются в прерывание `unknown_outcome` (CORE-003).
//!
//! [`run_demo`] — детерминированный демонстрационный run для сквозной проверки
//! журнала и протокола эффектов, [`replay_hash`] — хэш его значимых событий.

mod demo;
mod recovery;
mod runtime;

pub use demo::{CrashPoint, DemoConfig, DemoOutcome, replay_hash, run_demo};
pub use recovery::{Recovery, UNKNOWN_OUTCOME, recover};
pub use runtime::{EffectError, EffectSpec, RunCtx};
/// Прежнее имя причины паузы; значение живёт в [`za_protocol::pause_reason`].
pub use za_protocol::pause_reason::NEEDS_DECISION;
