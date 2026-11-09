//! Канонический формат сообщений.
//!
//! Внутренний формат не привязан к протоколу провайдера (MDL-001): коннекторы
//! переводят его в Chat Completions, Responses или Anthropic Messages и обратно.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::caps::Wire;
use crate::prefix::ToolSchema;

/// Роль сообщения. Системный промпт — отдельное поле `ChatRequest::system`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    /// Результаты инструментов.
    Tool,
    /// Служебное system-сообщение посреди истории (D10). Первое system — ChatRequest.system.
    ///
    /// Появляется в истории только из [`StreamEvent::ServiceMessage`]: коннектор
    /// Anthropic рендерит его как system-сообщение с `clear_at`, если модель их
    /// принимает, остальные коннекторы (и Anthropic у прочих моделей) его опускают.
    System,
}

/// Сообщение диалога.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

/// Часть сообщения.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Part {
    Text(String),
    /// Рассуждение модели; `replay` — данные провайдера для возврата в следующем запросе.
    Reasoning {
        text: String,
        replay: Option<ReplayBlob>,
    },
    ToolCall(ToolCall),
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

/// Вызов инструмента моделью.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Разобранные аргументы.
    pub arguments: Value,
    /// Исходная строка аргументов от провайдера, если она была.
    pub raw_arguments: Option<String>,
}

/// Непрозрачные данные провайдера, которые нужно вернуть ему без изменений:
/// подпись thinking (Anthropic), `encrypted_content` (Responses), `reasoning_details`
/// (OpenRouter). Действительны только для того же протокола и той же модели.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayBlob {
    pub wire: Wire,
    pub model: String,
    pub data: Value,
}

/// Выбор инструмента.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    /// Принудительный вызов инструмента с этим именем.
    Named(String),
}

/// Уровень усилия рассуждения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    Max,
}

impl Effort {
    /// Все уровни по возрастанию бюджета.
    const ALL: [Effort; 4] = [Effort::Low, Effort::Medium, Effort::High, Effort::Max];

    /// Бюджет токенов рассуждения уровня (D36): 2048, 8192, 16384, 32000.
    pub fn budget_tokens(self) -> u32 {
        match self {
            Self::Low => 2048,
            Self::Medium => 8192,
            Self::High => 16384,
            Self::Max => 32000,
        }
    }

    /// Уровень с ближайшим бюджетом; при равном расстоянии — меньший уровень.
    pub fn nearest(budget: u32) -> Self {
        let mut best = Self::Low;
        for e in Self::ALL {
            // Строгое «меньше»: при равенстве остаётся меньший (раньше в списке) уровень.
            if e.budget_tokens().abs_diff(budget) < best.budget_tokens().abs_diff(budget) {
                best = e;
            }
        }
        best
    }

    /// Имя уровня: `"low"`, `"medium"`, `"high"`, `"max"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}

/// Режим рассуждения запроса.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningMode {
    /// Поведение провайдера по умолчанию, поле не отправляется.
    #[default]
    Default,
    Off,
    Effort(Effort),
    /// Бюджет токенов рассуждения; перевод по провайдерам — D36.
    Budget(u32),
}

/// Параметры сэмплирования. `None` — не отправлять поле.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Sampling {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub seed: Option<u64>,
    pub presence_penalty: Option<f32>,
    pub stop: Vec<String>,
}

/// Запрос к модели в каноническом формате.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ChatRequest {
    /// Единственное system-сообщение (CTX-004).
    pub system: String,
    pub tools: Vec<ToolSchema>,
    pub messages: Vec<Message>,
    pub tool_choice: ToolChoice,
    pub max_output_tokens: Option<u32>,
    pub sampling: Sampling,
    pub reasoning: ReasoningMode,
    /// Ключ кэша промпта провайдера (`prompt_cache_key` и аналоги).
    pub cache_key: Option<String>,
}

/// Причина завершения ответа.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Refusal,
    /// Нераспознанная причина провайдера как есть.
    Other(String),
}

/// Откуда взята стоимость (D16).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostSource {
    /// Сумма из ответа провайдера.
    Provider,
    /// Расчёт по ценам из конфигурации эндпоинта.
    Computed,
    #[default]
    Unknown,
}

/// Расход токенов и стоимость вызова.
///
/// Единицы одинаковы у всех провайдеров (задача 9): `input` — все входные токены
/// запроса, включая прочитанные из кэша (`cached`) и записанные в кэш (`cache_write`),
/// как `prompt_tokens` у OpenAI. Коннектор Anthropic складывает `input_tokens`,
/// `cache_read_input_tokens` и `cache_creation_input_tokens` (у Anthropic
/// `input_tokens` кэш не включает), llama.cpp — `prompt_n + cache_n`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Все входные токены, включая `cached` и `cache_write`.
    pub input: u64,
    /// Входные токены, прочитанные из кэша провайдера.
    pub cached: u64,
    /// Входные токены, записанные в кэш.
    pub cache_write: u64,
    pub output: u64,
    /// Токены рассуждения (часть `output`, если провайдер так считает).
    pub reasoning: u64,
    pub gpu_ms: Option<u64>,
    pub cost_usd: Option<f64>,
    pub cost_source: CostSource,
}

/// Имя вида ресурса: `"ram"`, `"vram"`, `"cpu"`, `"disk"`. Строка, потому что
/// ядро `za-llm` не зависит от `za-res`.
pub type ResourceKindName = String;

/// Состояние заявки в очереди.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitState {
    Queued,
    WaitingResource {
        kind: ResourceKindName,
    },
    Cooldown {
        remaining_ms: u64,
    },
    Granted,
    /// Заявка снята или отклонена очередью; `reason` — код `QueueError::code()`
    /// (`cancelled`, `wait_timeout`, `budget_exhausted`, …).
    Failed {
        reason: String,
    },
}

/// Положение заявки в очереди (RES-006).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueStatus {
    /// Номер среди ожидающих заявок группы в порядке выдачи, с 1. `0` — место выдано,
    /// заявка провалена или ещё не пересчитана очередью.
    pub position: u32,
    pub waited_ms: u64,
    pub state: WaitState,
}

/// Событие стрима ответа модели.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamEvent {
    /// Заявка ждёт в очереди (RES-006).
    Queued(QueueStatus),
    /// Запрос отправлен; `attempt` = 1, один раз на стрим.
    Started {
        endpoint: EndpointId,
        slot: Option<u32>,
        attempt: u32,
    },
    /// Повтор после частичного ответа (D20): накопленное отбрасывается, в том числе
    /// `ServiceMessage` прежней попытки; `attempt` — номер новой попытки (2…6).
    /// Частичный ответ — хотя бы одно событие провайдера; попытка, сорвавшаяся до него,
    /// повторяется без `Restart`.
    Restart {
        attempt: u32,
        reason: String,
    },
    /// Обработка промпта (llama.cpp `return_progress`).
    Progress {
        processed: u32,
        total: u32,
    },
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStart {
        index: u32,
        id: String,
        name: String,
    },
    ToolCallArgs {
        index: u32,
        fragment: String,
    },
    /// Сообщение, которое коннектор добавил в конец истории запроса (эмуляция required у
    /// Anthropic, `clear_at`). Приходит до дельт. Потребитель фиксирует его перед ответом хода.
    ///
    /// Отправляется каждой попыткой, но только вместе с ответом провайдера: повторы
    /// держат его до первого события провайдера, так что сорвавшаяся раньше попытка его
    /// не отдаёт. После `Restart` приходит заново, как и дельты.
    ServiceMessage(Message),
    /// Итог последней попытки.
    Finished {
        message: Message,
        finish: FinishReason,
        usage: Usage,
    },
}

/// Собранный ответ модели.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    pub message: Message,
    pub finish: FinishReason,
    pub usage: Usage,
}

/// Идентификатор эндпоинта модели из конфигурации.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct EndpointId(pub String);

impl std::fmt::Display for EndpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Wire;
    use crate::prefix::ToolSchema;
    use serde_json::json;

    #[test]
    fn request_roundtrips_through_json() {
        let req = ChatRequest {
            system: "Ты агент.".into(),
            tools: vec![ToolSchema {
                name: "read_file".into(),
                description: "Прочитать файл.".into(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }],
            messages: vec![
                Message {
                    role: Role::User,
                    parts: vec![Part::Text("Прочитай README".into())],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![
                        Part::Reasoning {
                            text: "Нужно прочитать файл.".into(),
                            replay: Some(ReplayBlob {
                                wire: Wire::AnthropicMessages,
                                model: "claude".into(),
                                data: json!({"signature": "sig"}),
                            }),
                        },
                        Part::ToolCall(ToolCall {
                            id: "toolu_1".into(),
                            name: "read_file".into(),
                            arguments: json!({"path": "README.md"}),
                            raw_arguments: Some(r#"{"path":"README.md"}"#.into()),
                        }),
                    ],
                },
                Message {
                    role: Role::Tool,
                    parts: vec![Part::ToolResult {
                        call_id: "toolu_1".into(),
                        content: "# zero-agent".into(),
                        is_error: false,
                    }],
                },
            ],
            tool_choice: ToolChoice::Named("read_file".into()),
            max_output_tokens: Some(1024),
            sampling: Sampling {
                temperature: Some(0.5),
                top_p: Some(0.9),
                seed: Some(7),
                presence_penalty: None,
                stop: vec!["</end>".into()],
            },
            reasoning: ReasoningMode::Effort(Effort::High),
            cache_key: Some("sess-1".into()),
        };
        let back: ChatRequest =
            serde_json::from_value(serde_json::to_value(&req).unwrap()).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn effort_budget_table() {
        assert_eq!(Effort::High.budget_tokens(), 16384);
        assert_eq!(Effort::nearest(3000), Effort::Low);
        assert_eq!(Effort::nearest(10000), Effort::Medium);
        assert_eq!(Effort::nearest(20000), Effort::High);
        assert_eq!(Effort::nearest(100_000), Effort::Max);
        assert_eq!(Effort::nearest(5120), Effort::Low); // равное расстояние до 2048 и 8192 → меньший
    }
}
