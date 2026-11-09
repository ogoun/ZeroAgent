//! Таксономия ошибок вызова модели.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Класс ошибки вызова модели.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Сбой соединения.
    Transport,
    Timeout,
    /// 429: превышен лимит провайдера.
    RateLimited,
    /// Провайдер перегружен (529, 503).
    Overloaded,
    /// 5xx.
    Server,
    ContextOverflow,
    BadRequest,
    /// Ключ отвергнут или не пройдена проверка TLS сервера (сертификат, рукопожатие).
    Auth,
    NotFound,
    ContentFilter,
    /// Стрим оборвался до итога.
    StreamCut,
    /// Ответ провайдера не разобран.
    Malformed,
    /// Предохранитель эндпоинта открыт (MDL-016).
    CircuitOpen,
    /// Эндпоинт выключен или модель не готова.
    Inactive,
    /// Идентичный запрос после ошибки разбора (MDL-018, D21).
    RepeatedAfterParseError,
    Cancelled,
    /// Ошибка очереди (ожидание дольше `max_wait` и т. п.).
    Queue,
    /// Эндпоинт недоступен по зонду (D40): запрос не отправлялся. Не повторяется и не
    /// считается предохранителю; run встаёт на паузу до следующего зонда.
    Unreachable,
    /// Внутренний сбой `Gateway` (паника задачи запроса), не ошибка эндпоинта.
    Internal,
}

impl ErrorClass {
    /// Класс, который лечится повтором того же запроса (MDL-018, D19).
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::Transport
                | Self::Timeout
                | Self::RateLimited
                | Self::Overloaded
                | Self::Server
                | Self::StreamCut
        )
    }

    /// Класс, который считается неудачей для предохранителя эндпоинта (D22).
    /// `RateLimited` не считается: это лимит счёта, а не отказ эндпоинта.
    pub fn counts_for_breaker(self) -> bool {
        matches!(
            self,
            Self::Transport | Self::Timeout | Self::Overloaded | Self::Server | Self::StreamCut
        )
    }
}

/// Ошибка вызова модели. Сообщение не содержит ключей (SEC-044).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{class:?}: {message}")]
pub struct ModelError {
    pub class: ErrorClass,
    pub message: String,
    /// Пауза, которую назвал провайдер (`Retry-After`).
    pub retry_after: Option<Duration>,
    pub http_status: Option<u16>,
}

impl ModelError {
    /// Ошибка без `retry_after` и HTTP-статуса.
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            retry_after: None,
            http_status: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ErrorClass::*;
    use std::collections::HashSet;

    #[test]
    fn retryable_classes() {
        for c in [
            Transport,
            Timeout,
            RateLimited,
            Overloaded,
            Server,
            StreamCut,
        ] {
            assert!(c.retryable(), "{c:?}");
        }
        for c in [
            Auth,
            BadRequest,
            ContextOverflow,
            Malformed,
            NotFound,
            Inactive,
            Unreachable,
            Internal,
        ] {
            assert!(!c.retryable(), "{c:?}");
        }
        assert!(!RateLimited.counts_for_breaker() && Server.counts_for_breaker());
        assert!(!Unreachable.counts_for_breaker() && !Internal.counts_for_breaker());
    }

    #[test]
    fn error_class_is_copy_hash_key() {
        let mut m = HashSet::new();
        let c = ErrorClass::Server;
        m.insert(c);
        assert!(m.contains(&c) && c.retryable());
    }
}
