//! Ядро zero-agent: контекст выполнения run и протокол эффектов.
//!
//! Каждый побочный эффект проходит протокол намерение → выполнение → квитанция
//! (CORE-002) с ключом из `(run, step, effect_seq)` (CORE-004). Завершённые
//! эффекты при повторе отдаются из журнала; незавершённые после сбоя повторяются
//! или, для `NonIdempotent`, превращаются в прерывание `unknown_outcome` (CORE-003).

mod recovery;
mod runtime;

pub use recovery::{Recovery, UNKNOWN_OUTCOME, recover};
pub use runtime::{EffectError, EffectSpec, RunCtx};
