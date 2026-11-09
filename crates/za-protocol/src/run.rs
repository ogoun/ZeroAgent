//! Роли и статусы run.

use serde::{Deserialize, Serialize};

/// Роль run в дереве.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunRole {
    Top,
    Orchestrator,
    Worker,
}

/// Статус run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Pausing,
    Paused,
    Cancelling,
    BlockedOnBudget,
    Done,
    Partial,
    Blocked,
    Failed,
    Cancelled,
}

impl RunStatus {
    /// Допустим ли переход `self -> to` (схема ТЗ, часть 01, раздел 4.3).
    pub fn can_transition(self, to: RunStatus) -> bool {
        use RunStatus::*;
        matches!(
            (self, to),
            (Queued, Running)
                | (Queued, Cancelling)
                | (Queued, Cancelled)
                | (Running, Pausing)
                // Немедленная пауза в безопасной точке, выбранной самим рантаймом:
                // ожидание решения человека, отказ провайдера.
                | (Running, Paused)
                | (Pausing, Paused)
                | (Pausing, Running)
                | (Pausing, Failed)
                | (Pausing, Cancelling)
                | (Paused, Running)
                | (Running, Cancelling)
                | (Paused, Cancelling)
                | (BlockedOnBudget, Cancelling)
                | (Cancelling, Cancelled)
                | (Running, BlockedOnBudget)
                | (BlockedOnBudget, Running)
                | (Running, Done | Partial | Blocked | Failed)
        )
    }

    /// Терминальный ли статус (выходов нет).
    pub fn is_terminal(self) -> bool {
        use RunStatus::*;
        matches!(self, Done | Partial | Blocked | Failed | Cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_status_has_no_exits() {
        assert!(!RunStatus::Cancelled.can_transition(RunStatus::Running));
        assert!(RunStatus::Running.can_transition(RunStatus::Pausing));
        assert!(RunStatus::Paused.can_transition(RunStatus::Running));
    }

    #[test]
    fn terminal_statuses_never_transition() {
        use RunStatus::*;
        let all = [
            Queued,
            Running,
            Pausing,
            Paused,
            Cancelling,
            BlockedOnBudget,
            Done,
            Partial,
            Blocked,
            Failed,
            Cancelled,
        ];
        for from in all.into_iter().filter(|s| s.is_terminal()) {
            assert!(all.into_iter().all(|to| !from.can_transition(to)));
        }
    }

    #[test]
    fn new_transitions_allowed() {
        use RunStatus::*;
        for (a, b) in [
            (Queued, Cancelling),
            (Queued, Cancelled),
            (Pausing, Cancelling),
            (BlockedOnBudget, Cancelling),
            (Pausing, Running),
            (Pausing, Paused),
            (Pausing, Failed),
        ] {
            assert!(a.can_transition(b), "{a:?} -> {b:?}");
        }
    }

    #[test]
    fn running_can_pause_immediately() {
        // Немедленная пауза в безопасной точке, выбранной рантаймом
        // (ожидание решения, отказ провайдера), минуя `pausing`.
        assert!(RunStatus::Running.can_transition(RunStatus::Paused));
        assert!(!RunStatus::Paused.can_transition(RunStatus::Paused));
    }

    #[test]
    fn every_non_terminal_can_reach_cancelling() {
        use RunStatus::*;
        for s in [Queued, Running, Pausing, Paused, BlockedOnBudget] {
            assert!(s.can_transition(Cancelling), "{s:?}");
        }
        assert!(!Cancelling.can_transition(Cancelling));
    }

    #[test]
    fn status_serde_round_trip_snake_case() {
        let s = serde_json::to_string(&RunStatus::BlockedOnBudget).unwrap();
        assert_eq!(s, r#""blocked_on_budget""#);
        assert_eq!(
            serde_json::from_str::<RunStatus>(&s).unwrap(),
            RunStatus::BlockedOnBudget
        );
    }
}
