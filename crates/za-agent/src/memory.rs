//! Порт памяти цикла (часть 04 ТЗ) и режим L0 — память недоступна (N-16).
//!
//! Интерфейс без реализации ZeroAIMemory: цикл зависит только от [`MemoryPort`];
//! подключение памяти L1/L2 — отдельный план.

use serde::{Deserialize, Serialize};
use za_llm::BoxFuture;
use za_protocol::RunId;

/// Уровень доступной памяти: L0 — недоступна, L1 и L2 — по части 04 ТЗ.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryLevel {
    L0,
    L1,
    L2,
}

/// Порт памяти для цикла.
pub trait MemoryPort: Send + Sync {
    /// Уровень памяти; L0 — цикл пишет `memory.unavailable` на шаге 0 (D24).
    fn level(&self) -> MemoryLevel;
    /// Замороженный снимок для первого сообщения (блок 6), не больше `budget_tokens`.
    fn snapshot(&self, run: RunId, budget_tokens: u32) -> BoxFuture<'_, Option<String>>;
    /// CTX-013 перед компакцией (план B2).
    fn before_compaction(&self, run: RunId, next_step: &str) -> BoxFuture<'_, ()>;
    /// MEM-050: recovery после компакции (план B2).
    fn after_compaction(&self, run: RunId) -> BoxFuture<'_, Option<String>>;
}

/// L0: память недоступна (N-16). `snapshot` и `after_compaction` — `None`.
#[derive(Copy, Clone, Debug, Default)]
pub struct NoMemory;

impl MemoryPort for NoMemory {
    fn level(&self) -> MemoryLevel {
        MemoryLevel::L0
    }

    fn snapshot(&self, _run: RunId, _budget_tokens: u32) -> BoxFuture<'_, Option<String>> {
        Box::pin(async { None })
    }

    fn before_compaction(&self, _run: RunId, _next_step: &str) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn after_compaction(&self, _run: RunId) -> BoxFuture<'_, Option<String>> {
        Box::pin(async { None })
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
