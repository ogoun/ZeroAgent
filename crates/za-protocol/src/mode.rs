//! Режим run (CORE-106) и тир модели по размеру контекста (PRM-002).

use serde::{Deserialize, Serialize};

/// Режим run: меняет политику инструментов рантайма, а не текст промпта.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunMode {
    /// Только чтение: правки и запуск команд запрещены.
    ReadOnly,
    /// Планирование: как `ReadOnly`, результат — план.
    Plan,
    /// Правки и команды — после одобрения (по умолчанию).
    #[default]
    Ask,
    /// Правки без одобрения, команды — с одобрением.
    AcceptEdits,
    /// Без одобрений.
    Auto,
}

/// Тир модели: определяет бюджет промпта и уровень мышления.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Small,
    Standard,
    Full,
}

/// Наименьший контекст тира `Standard`, токены.
const STANDARD_MIN_CTX: u32 = 32_768;
/// Наименьший контекст тира `Full`, токены.
const FULL_MIN_CTX: u32 = 65_536;

impl Tier {
    /// Тир по размеру контекста модели (D20): меньше 32768 — `Small`,
    /// меньше 65536 — `Standard`, иначе `Full`.
    pub fn for_context(n_ctx: u32) -> Tier {
        if n_ctx < STANDARD_MIN_CTX {
            Tier::Small
        } else if n_ctx < FULL_MIN_CTX {
            Tier::Standard
        } else {
            Tier::Full
        }
    }

    /// Имя тира, как в сериализации.
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Small => "small",
            Tier::Standard => "standard",
            Tier::Full => "full",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_mode_kebab_and_default() {
        assert_eq!(
            serde_json::to_value(RunMode::AcceptEdits).unwrap(),
            "accept-edits"
        );
        assert_eq!(
            serde_json::to_value(RunMode::ReadOnly).unwrap(),
            "read-only"
        );
        assert_eq!(RunMode::default(), RunMode::Ask);
    }

    #[test]
    fn tier_for_context() {
        assert_eq!(Tier::for_context(8192), Tier::Small);
        assert_eq!(Tier::for_context(32767), Tier::Small);
        assert_eq!(Tier::for_context(32768), Tier::Standard);
        assert_eq!(Tier::for_context(65535), Tier::Standard);
        assert_eq!(Tier::for_context(65536), Tier::Full);
    }

    #[test]
    fn tier_name_matches_serde() {
        for t in [Tier::Small, Tier::Standard, Tier::Full] {
            assert_eq!(serde_json::to_value(t).unwrap(), t.as_str());
        }
        assert!(Tier::Small < Tier::Standard && Tier::Standard < Tier::Full);
    }
}
