//! Ресурсные типы протокола: приоритет заявки к модели и ресурсам (RES-003).

use serde::{Deserialize, Serialize};

/// Класс приоритета заявки (RES-003, D9). Меньший ранг обслуживается раньше.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Пользователь ждёт ответа в интерфейсе.
    Interactive,
    /// Верхний run дерева.
    Top,
    Orchestrator,
    Worker,
    /// Фоновое задание по расписанию.
    Job,
    /// Арена: сравнение моделей, самая низкая важность.
    Arena,
}

impl Priority {
    /// Базовый ранг по D9: `Interactive` 0 … `Arena` 5.
    pub fn rank(self) -> u8 {
        match self {
            Self::Interactive => 0,
            Self::Top => 1,
            Self::Orchestrator => 2,
            Self::Worker => 3,
            Self::Job => 4,
            Self::Arena => 5,
        }
    }

    /// Фоновый класс (`Job`, `Arena`): ограничен по слотам и ресурсам строже (D9, D10).
    pub fn is_background(self) -> bool {
        matches!(self, Self::Job | Self::Arena)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_rank_and_background() {
        assert!(
            Priority::Interactive.rank() < Priority::Top.rank()
                && Priority::Job.rank() < Priority::Arena.rank()
        );
        assert!(
            Priority::Job.is_background()
                && Priority::Arena.is_background()
                && !Priority::Worker.is_background()
        );
    }
}
