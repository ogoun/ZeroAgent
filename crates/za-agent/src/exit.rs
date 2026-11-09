//! Выходы цикла и отображение ошибок модели и эффектов в состояние run (D11, D31).
//!
//! Часть 01, 4.3: сбой провайдера, лимит и нехватка ресурса переводят run в
//! `paused` с причиной, а не оставляют его в `running`. `failed` — только ошибки
//! журнала и нарушенные инварианты цикла: их драйвер получает как `LoopError`, а не
//! как [`LoopExit`].

use za_core::EffectError;
use za_llm::{ErrorClass, ModelError, WaitState};
use za_protocol::{BudgetKind, RunStatus, pause_reason};

/// Выход цикла, который узел кладёт в LoopState.exit (D31). `failed` сюда не входит: это LoopError (D11).
#[derive(Debug, Clone, PartialEq)]
pub enum LoopExit {
    /// Конечное состояние: `Done`, `Partial` или `Blocked`.
    Finished(RunStatus),
    /// Чистая пауза с причиной из [`pause_reason`] (D5).
    Paused(&'static str),
    /// Прерывание: approval, ввод, неизвестный исход (D16).
    Suspended {
        reason: &'static str,
        interrupt: String,
    },
    /// Превышен бюджет run (D19).
    BlockedOnBudget(BudgetKind),
    /// Отмена по команде (D26).
    Cancelled,
}

/// Таблица D11. `last_wait` — последний WaitState стрима.
///
/// `Queue` различается по коду `QueueError::code()` в тексте ошибки (`Gateway`
/// кладёт туда код как есть): `wait_timeout` — `resource`, если заявка последней
/// ждала ресурса, иначе `queue`; `budget_exhausted` — `budget`; `monitor_stopped` —
/// `resource`; прочие коды — `internal`.
pub fn exit_for_model_error(e: &ModelError, last_wait: Option<&WaitState>) -> LoopExit {
    use ErrorClass::*;
    let reason = match e.class {
        Transport | Timeout | RateLimited | Overloaded | Server | StreamCut | CircuitOpen
        | Unreachable | Auth | NotFound | BadRequest | Malformed | ContentFilter | Inactive => {
            pause_reason::PROVIDER
        }
        Queue => match e.message.as_str() {
            "wait_timeout" => match last_wait {
                Some(WaitState::WaitingResource { .. }) => pause_reason::RESOURCE,
                _ => pause_reason::QUEUE,
            },
            "budget_exhausted" => pause_reason::BUDGET,
            "monitor_stopped" => pause_reason::RESOURCE,
            _ => pause_reason::INTERNAL,
        },
        ContextOverflow => pause_reason::CONTEXT,
        // `Cancelled` сюда попадает только не по команде цикла: обрыв по сигналу
        // приходит квитанцией `interrupted:*`, а не ошибкой модели.
        Internal | RepeatedAfterParseError | Cancelled => pause_reason::INTERNAL,
    };
    LoopExit::Paused(reason)
}

/// D11, как в демо R0: ReplayMismatch → Paused("replay_mismatch"); Unknown{interrupt_id} →
/// Suspended{reason: "needs_decision", interrupt: interrupt_id}; прочие (Store, Failed, Poisoned) — None,
/// драйвер превращает их в LoopError.
pub fn exit_for_effect_error(e: &EffectError) -> Option<LoopExit> {
    match e {
        EffectError::ReplayMismatch { .. } => Some(LoopExit::Paused(pause_reason::REPLAY_MISMATCH)),
        EffectError::Unknown { interrupt_id } => Some(LoopExit::Suspended {
            reason: pause_reason::NEEDS_DECISION,
            interrupt: interrupt_id.clone(),
        }),
        EffectError::Store(_) | EffectError::Failed(_) | EffectError::Poisoned => None,
    }
}
