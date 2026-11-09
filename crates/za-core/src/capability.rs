//! Capability: именованный набор инструментов, инструкций и хуков границы шага.

use std::sync::Arc;

use serde_json::Value;
use za_protocol::RunId;

use crate::tool::Tool;

/// Позиция run на границе шага.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundaryInfo {
    pub run: RunId,
    pub step: u64,
    pub turn: u32,
}

/// Хук границы шага. Обязан быть чистой функцией BoundaryInfo: реплей вызывает его снова.
///
/// `Some(text)` — служебное сообщение для истории, `None` — ничего.
pub trait Hook: Send + Sync {
    fn at_boundary(&self, b: &BoundaryInfo) -> Option<String>;
}

/// Набор возможностей, подключаемый к run.
#[derive(Clone, Default)]
pub struct Capability {
    pub name: String,
    pub tools: Vec<Arc<dyn Tool>>,
    /// Инструкции; входят в блок 2 промпта после Core (D21).
    pub instructions: Option<String>,
    pub hooks: Vec<Arc<dyn Hook>>,
    /// Настройки capability как есть.
    pub settings: Value,
}
