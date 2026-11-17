//! Режим идей (R1-M D148, D169; решение владельца 3 «идеи — по режиму памяти»).
//!
//! Здесь — тип режима и выбор по признакам сервера: его ждёт укладка дайджеста
//! ([`crate::fit`], раздел `### Идеи на разбор` и строка `План из идей`). Операции над идеями
//! (разбор, предложение, отказы шлюза) — задача 8.
//!
//! Прямого источника режима у токена нет (CR-26: `GET /auth/settings` по токену — 403).
//! Признак `review` — `items[].reason: "idea_review"` в ответе `/recall` `session_start` или
//! `prompt` (сервер ставит его только в режиме `review`) или 409 `/problems/idea-mode` на
//! предложение идеи человека; признак `propose` — 409 на разбор (задача 8). Последний признак
//! run хранит снимок (`MemoryState.ideas`, задача 3) — так реплей детерминирован. Без
//! признака и без флага `idea_review` в `/health.features` — [`IdeasMode::Wait`].

use std::collections::BTreeSet;

/// Флаг выпуска M6.5 в `features` (`0.0.12-m6.5`): разбор идей агентом.
pub const FEATURE_IDEA_REVIEW: &str = "idea_review";
/// `reason` элемента `items[]` ответа `/recall` у идеи очереди разбора (строки дайджеста и
/// подсказки к `prompt`, `api/recall.rs@v0.0.12-m6.5`) — признак режима `review`.
pub const REASON_IDEA_REVIEW: &str = "idea_review";
/// Видимый признак режима `review` (значение `IdeasSignal::Review`).
pub const VISIBLE_REVIEW: &str = "review";

/// Режим идей агента (D148).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum IdeasMode {
    /// Разбор идей агентом выключен или не виден: свои мысли — `idea origin: agent`, работа
    /// по идее — только после «начать» человека; раздел очереди разбора дайджеста снимается.
    #[default]
    Wait,
    /// Разбор идей агентом включён (ZAM `0.0.12-m6.5`, режим `review` владельца базы):
    /// раздел `### Идеи на разбор` идёт в укладке пятым (D141).
    Review,
}

impl IdeasMode {
    /// Машинное имя режима (строка `memory status`, журнал).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Review => "review",
        }
    }
}

/// Режим по флагам сервера и видимому признаку (D169): [`IdeasMode::Review`] — только если в
/// `features` есть [`FEATURE_IDEA_REVIEW`] и признак — `Some("review")`; иначе
/// [`IdeasMode::Wait`] (в том числе при признаке `propose`).
pub fn ideas_mode(features: &BTreeSet<String>, visible: Option<&str>) -> IdeasMode {
    if features.contains(FEATURE_IDEA_REVIEW) && visible == Some(VISIBLE_REVIEW) {
        IdeasMode::Review
    } else {
        IdeasMode::Wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_needs_flag_and_signal() {
        let with: BTreeSet<String> = ["m6", FEATURE_IDEA_REVIEW]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let without: BTreeSet<String> = ["m6".to_owned()].into();
        assert_eq!(ideas_mode(&with, Some("review")), IdeasMode::Review);
        assert_eq!(ideas_mode(&with, Some("propose")), IdeasMode::Wait);
        assert_eq!(ideas_mode(&with, None), IdeasMode::Wait);
        assert_eq!(ideas_mode(&without, Some("review")), IdeasMode::Wait);
        assert_eq!(IdeasMode::default(), IdeasMode::Wait);
    }
}
