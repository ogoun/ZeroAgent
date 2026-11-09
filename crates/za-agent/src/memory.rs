//! Порт памяти цикла (часть 04 ТЗ) и режим L0 — память недоступна (N-16).
//!
//! Интерфейс без реализации ZeroAIMemory: цикл зависит только от [`MemoryPort`];
//! подключение памяти L1–L3 — отдельный план. Остальные моменты обмена с памятью
//! (подсказки `prompt` и `post_tool`, `brief` субагента, конец сессии; часть 04, §6)
//! добавляются новыми методами с реализацией по умолчанию — без ломки реализаций.

use serde::{Deserialize, Serialize};
use za_llm::BoxFuture;
use za_protocol::RunId;

/// Уровень доступной памяти (часть 04, §3): L0 — офлайн, L1 — базовый, L2 — экономия
/// (эпохи, честный бюджет), L3 — M6 полный (`focus`, снимок сессии).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MemoryLevel {
    L0,
    L1,
    L2,
    L3,
}

/// Итог чтения памяти: «пусто», «недоступна» и «не успели» различаются (MEM-002,
/// MEM-034, MEM-122).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Recall {
    /// Текст в пределах переданного бюджета.
    Hits(String),
    /// Память ответила, но нечего показать: текста в промпте нет (MEM-030).
    Empty,
    /// Память недоступна (L0, ошибка соединения, отозванный токен): явный отказ.
    Unavailable,
    /// Ответ не пришёл в срок (MEM-024) или пришёл с `recall timeout`: один отложенный
    /// повтор вне горячего пути (MEM-034).
    Timeout,
}

impl Recall {
    /// Текст для промпта: только у `Hits` с непустым текстом.
    pub fn text(&self) -> Option<&str> {
        match self {
            Recall::Hits(t) if !t.is_empty() => Some(t),
            _ => None,
        }
    }
}

/// Порт памяти для цикла.
pub trait MemoryPort: Send + Sync {
    /// Уровень памяти; L0 — цикл пишет `memory.unavailable` на шаге 0 (D24).
    fn level(&self) -> MemoryLevel;
    /// Замороженный снимок для первого сообщения (блок 6, MEM-031): `recall` события
    /// `session_start`, не больше `budget_tokens` ([`memory_budget`], MEM-060).
    fn snapshot(&self, run: RunId, budget_tokens: u32) -> BoxFuture<'_, Recall>;
    /// CTX-013 перед компакцией: `next_step`, очередь файлов, снимок сессии (план B2).
    fn before_compaction<'a>(&'a self, run: RunId, next_step: &'a str) -> BoxFuture<'a, ()>;
    /// MEM-050: `recall` события `recovery` после компакции и при resume, не больше
    /// `budget_tokens` (MEM-060: бюджет передаётся всегда).
    fn after_compaction(&self, run: RunId, budget_tokens: u32) -> BoxFuture<'_, Recall>;
}

/// L0: память недоступна (N-16). Чтения — [`Recall::Unavailable`] (MEM-122).
#[derive(Copy, Clone, Debug, Default)]
pub struct NoMemory;

impl MemoryPort for NoMemory {
    fn level(&self) -> MemoryLevel {
        MemoryLevel::L0
    }

    fn snapshot(&self, _run: RunId, _budget_tokens: u32) -> BoxFuture<'_, Recall> {
        Box::pin(async { Recall::Unavailable })
    }

    fn before_compaction<'a>(&'a self, _run: RunId, _next_step: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn after_compaction(&self, _run: RunId, _budget_tokens: u32) -> BoxFuture<'_, Recall> {
        Box::pin(async { Recall::Unavailable })
    }
}

/// Бюджет снимка памяти по окну контекста (MEM-030), токены:
/// меньше 16384 — 200, меньше 32768 — 400, меньше 65536 — 800, иначе 1500.
pub fn memory_budget(n_ctx: u32) -> u32 {
    if n_ctx < 16_384 {
        200
    } else if n_ctx < 32_768 {
        400
    } else if n_ctx < 65_536 {
        800
    } else {
        1500
    }
}
