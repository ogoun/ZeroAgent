//! Инструменты-заглушки общих сценариев интеграционных тестов `za-agent` (R1-M, fix-3
//! задачи 3): вместо копий в тестовых крейтах.

use serde_json::Value;
use za_core::{Tool, ToolCx, ToolOutput, ToolSpec};
use za_llm::BoxFuture;

/// Чтение `fail`: результат-ошибка `fail {args}` (как в генераторе журналов R1-C2).
/// Спецификацию задаёт тест: у крейтов свои помощники `tool_spec`.
pub struct Fail {
    pub spec: ToolSpec,
}

impl Tool for Fail {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move { ToolOutput::error(format!("fail {args}")) })
    }
}
