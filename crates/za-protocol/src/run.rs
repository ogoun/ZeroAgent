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

/// Причины паузы run: поле `reason` события `run.status` при переходе в `paused`.
///
/// Значения — часть протокола: их читают UI, статус и автоподъём после старта,
/// поэтому менять строки нельзя.
pub mod pause_reason {
    /// Нужно решение человека: исход `NonIdempotent`-эффекта неизвестен.
    pub const NEEDS_DECISION: &str = "needs_decision";
    /// Записанный в журнале эффект расходится с тем, что run делает сейчас.
    pub const REPLAY_MISMATCH: &str = "replay_mismatch";
    /// Заявка в очереди к модели ждала дольше `max_wait` (RES-007).
    pub const QUEUE: &str = "queue";
    /// Не хватает ресурса узла: RAM, VRAM, места на диске (RES-012).
    pub const RESOURCE: &str = "resource";
    /// Провайдер модели недоступен или отказал, повторы исчерпаны (MDL-016).
    pub const PROVIDER: &str = "provider";
    /// Дневной бюджет облачной группы исчерпан (RES-004, `QueueError::BudgetExhausted`).
    pub const BUDGET: &str = "budget";
    /// Пауза по команде пользователя (`RunControl::pause`, Ctrl+C).
    pub const COMMAND: &str = "command";
    /// Модель спросила пользователя (`ask_user`), ждём ответа.
    pub const NEEDS_INPUT: &str = "needs_input";
    /// Вызов инструмента ждёт одобрения человека.
    pub const NEEDS_APPROVAL: &str = "needs_approval";
    /// Ждём внешнего результата или таймера.
    pub const WAITING: &str = "waiting";
    /// Внутренняя ошибка рантайма или модели, не относящаяся к провайдеру.
    pub const INTERNAL: &str = "internal";
    /// Контекст модели переполнен.
    pub const CONTEXT: &str = "context";
    /// Модель отказалась отвечать (`finish = Refusal` или фильтр содержимого).
    pub const REFUSAL: &str = "refusal";
    /// Подряд ошибки инструментов одного класса (R1-B2 D57).
    pub const TOOL_ERRORS: &str = "tool_errors";
    /// Модель зациклилась: повтор текста или бесконечное рассуждение (R1-B2 D58, D59, D61).
    pub const MODEL_LOOP: &str = "model_loop";
}

/// Причины ошибки run: поле `reason` события `run.status` при переходе в `failed`.
///
/// Короткий машинный код класса ошибки цикла; подробность — в логах. Значения —
/// часть протокола (их читают CLI и UI), менять строки нельзя.
pub mod fail_reason {
    /// Ошибка журнала, после которой статус удалось записать.
    pub const STORE: &str = "store";
    /// Контекст run отравлен прежней ошибкой.
    pub const POISONED: &str = "poisoned";
    /// Прочая ошибка протокола эффектов (квитанция-отказ там, где её быть не должно).
    pub const EFFECT: &str = "effect";
    /// Нарушен инвариант истории диалога.
    pub const HISTORY: &str = "history";
    /// Снимок run не пишется или не читается.
    pub const SNAPSHOT: &str = "snapshot";
    /// Нарушен инвариант графа цикла.
    pub const GRAPH: &str = "graph";
    /// Run в статусе, из которого продолжить нельзя.
    pub const NOT_RESUMABLE: &str = "not_resumable";
    /// Эндпоинт модели неизвестен.
    pub const UNKNOWN_ENDPOINT: &str = "unknown_endpoint";
    /// Смена модели посреди шага.
    pub const CHANGE_MID_STEP: &str = "change_mid_step";
    /// Префикс промпта не собирается (конфигурация run).
    pub const PREFIX: &str = "prefix";
    /// Запрос хода не сериализуется.
    pub const REQUEST: &str = "request";
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
