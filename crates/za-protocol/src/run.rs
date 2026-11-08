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
                | (Running, Pausing)
                | (Pausing, Paused)
                | (Paused, Running)
                | (Running, Cancelling)
                | (Paused, Cancelling)
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
    fn status_serde_round_trip_snake_case() {
        let s = serde_json::to_string(&RunStatus::BlockedOnBudget).unwrap();
        assert_eq!(s, r#""blocked_on_budget""#);
        assert_eq!(
            serde_json::from_str::<RunStatus>(&s).unwrap(),
            RunStatus::BlockedOnBudget
        );
    }
}
