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

/// Куда префикс run кладёт инструкции capability (R1-C2 D117).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InstructionsTarget {
    /// Блок 2 system после Core (D21); на top small — описание первого инструмента
    /// capability (R1-C1 D93).
    #[default]
    System,
    /// Описание первого по имени инструмента capability на любом тире, в своём бюджете
    /// окна, а не в system: так идёт каталог скиллов — недоверенные описания из файлов
    /// скиллов (MCP-007). Инструмента в наборе run нет (например, на tier small) —
    /// инструкции молча опускаются.
    FirstTool,
}

/// Набор возможностей, подключаемый к run.
#[derive(Clone, Default)]
pub struct Capability {
    pub name: String,
    pub tools: Vec<Arc<dyn Tool>>,
    /// Инструкции; куда они идут — [`Capability::instructions_target`].
    pub instructions: Option<String>,
    pub hooks: Vec<Arc<dyn Hook>>,
    /// Настройки capability как есть.
    pub settings: Value,
    /// Место инструкций в префиксе; по умолчанию — system (D21).
    pub instructions_target: InstructionsTarget,
}
