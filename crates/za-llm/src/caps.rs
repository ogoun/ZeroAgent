//! Возможности провайдера (MDL-002).
//!
//! `default_caps` — значения до зонда; реальные возможности уточняет зонд
//! (MDL-003) и записывает в профиль модели. Размер контекста хранится в профиле.

use serde::{Deserialize, Serialize};

/// Протокол обмена с провайдером.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

/// Где провайдер отдаёт рассуждение.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningField {
    None,
    /// `reasoning_content` (llama.cpp, DeepSeek).
    ReasoningContent,
    /// `reasoning` (vLLM, OpenRouter).
    Reasoning,
    /// Блоки `thinking` (Anthropic).
    ThinkingBlocks,
    /// Элементы `reasoning` (OpenAI Responses).
    ResponsesItems,
}

/// Как модель вызывает инструменты.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFormat {
    /// Нативные вызовы провайдера.
    Native,
    /// Вызовы в тексте ответа в указанном формате.
    Text(TextFormat),
}

/// Текстовые форматы вызова инструментов.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextFormat {
    Hermes,
    QwenXml,
    Harmony,
    Dsml,
    Gemma4,
    MistralToolCalls,
    Pythonic,
    Llama3Json,
}

/// Способ мышления модели (не поле MDL-002): заполняет зонд (задача 9), читают
/// коннекторы Anthropic и Responses. У Anthropic — из Models API (`capabilities`), у
/// OpenAI Responses — по имени модели (API возможностей не сообщает), у прочих —
/// `Default` (коннектор Chat Completions поле не читает).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ThinkingSupport {
    /// Адаптивное мышление (`{"type":"adaptive"}`).
    pub adaptive: bool,
    /// Поддерживается уровень усилия: `output_config.effort` у Anthropic,
    /// `reasoning.effort` у Responses (у Responses — признак рассуждающей модели).
    pub effort: bool,
    /// Мышление с бюджетом `{"type":"enabled","budget_tokens":n}` (Anthropic
    /// `capabilities.thinking.types.enabled`).
    #[serde(default)]
    pub budget: bool,
    /// Уровень усилия `xhigh`.
    #[serde(default)]
    pub effort_xhigh: bool,
    /// Уровень усилия `max`.
    #[serde(default)]
    pub effort_max: bool,
    /// Модель принимает `temperature` и `top_p` (Responses; у Anthropic на
    /// `2026-10-21` — никогда, D15).
    #[serde(default)]
    pub sampling: bool,
}

impl ThinkingSupport {
    /// Значение `effort` для [`crate::Effort::Max`]: `max`, если модель его принимает,
    /// иначе `xhigh`, иначе `high`.
    pub fn max_effort(&self) -> &'static str {
        if self.effort_max {
            "max"
        } else if self.effort_xhigh {
            "xhigh"
        } else {
            "high"
        }
    }
}

/// Где провайдер сообщает число токенов из кэша.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachedTokensField {
    None,
    /// `usage.prompt_tokens_details.cached_tokens`.
    PromptTokensDetails,
    /// `timings.cache_n` llama.cpp.
    LlamaTimings,
    /// `usage.cache_read_input_tokens`.
    AnthropicCacheRead,
    /// `usage.input_tokens_details.cached_tokens`.
    ResponsesInputDetails,
}

/// Откуда известен размер контекста (MDL-004).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    Config,
    /// Из рантайма, `path` — поле ответа.
    Runtime {
        path: String,
    },
    OverflowProbe,
    /// Консервативное значение до зонда.
    Conservative,
}

/// Чем считаются токены промпта до отправки (компакция, допуск по контексту).
///
/// Единый смысл (задача 9): точный подсчёт там, где у провайдера есть метод подсчёта;
/// иначе `Usage`. `Usage.input` прошлых ответов в токенах всего входа (включая кэш,
/// см. [`crate::Usage`]), до первого ответа и для нового хвоста — оценка символы / 4.
/// `CharsDiv4` — только оценка: провайдер заведомо не отдаёт `usage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenCountSource {
    LlamaTokenize,
    AnthropicCountTokens,
    /// По `usage` прошлых ответов; до первого ответа — символы / 4.
    Usage,
    /// Оценка: символы / 4.
    CharsDiv4,
}

/// Требования провайдера к id вызова инструмента.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolIdFormat {
    Any,
    /// Ровно 9 латинских букв и цифр (Mistral).
    Alnum9,
    Prefixed(String),
}

/// Возможности провайдера: ровно 13 полей MDL-002.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub wire: Wire,
    pub reasoning_field: ReasoningField,
    /// Рассуждение нужно возвращать провайдеру в следующих запросах.
    pub reasoning_roundtrip: bool,
    pub tool_format: ToolFormat,
    pub parallel_tools: bool,
    pub tool_choice_required: bool,
    pub strict_schema: bool,
    /// Можно продолжить начатое сообщение ассистента.
    pub prefill: bool,
    pub cached_tokens_field: CachedTokensField,
    pub context_source: ContextSource,
    pub token_count_source: TokenCountSource,
    pub single_system: bool,
    pub tool_id_format: ToolIdFormat,
}

/// Вид бэкенда. Имена serde используются в конфигурации и ключе группы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BackendKind {
    #[serde(rename = "llama_cpp")]
    LlamaCpp,
    #[serde(rename = "vllm")]
    Vllm,
    #[serde(rename = "openrouter")]
    OpenRouter,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
}

/// Пул эндпоинта (RES-001).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolKind {
    Local,
    Cloud,
}

/// Возможности бэкенда до зонда.
pub fn default_caps(kind: BackendKind) -> ProviderCapabilities {
    use BackendKind as B;
    let wire = match kind {
        B::Anthropic => Wire::AnthropicMessages,
        B::OpenAiResponses => Wire::Responses,
        B::LlamaCpp | B::Vllm | B::OpenRouter | B::OpenAiChat | B::OpenAiCompatible => {
            Wire::ChatCompletions
        }
    };
    let reasoning_field = match kind {
        B::LlamaCpp | B::OpenAiCompatible => ReasoningField::ReasoningContent,
        B::Vllm | B::OpenRouter => ReasoningField::Reasoning,
        B::Anthropic => ReasoningField::ThinkingBlocks,
        B::OpenAiResponses => ReasoningField::ResponsesItems,
        B::OpenAiChat => ReasoningField::None,
    };
    // Возврат рассуждения обязателен там, где провайдер отдаёт непрозрачные данные
    // для повтора: подпись thinking, encrypted_content, reasoning_details.
    let reasoning_roundtrip = matches!(kind, B::Anthropic | B::OpenAiResponses | B::OpenRouter);
    // Параллельные вызовы до зонда. llama-server принимает `parallel_tool_calls`, а
    // без поля берёт `chat_template_caps.supports_parallel_tool_calls` шаблона
    // (`server-common.cpp`, master 2026-11): шаблон может их не уметь, поэтому до
    // зонда `false`, зонд берёт значение из `/props`. vLLM: у запроса
    // `parallel_tool_calls` по умолчанию `true` (`chat_completion/protocol.py`), число
    // вызовов определяет парсер модели — `true` совпадает с поведением сервера.
    // Облака умеют параллельные вызовы. О произвольном совместимом сервере ничего не
    // известно — консервативно.
    let parallel_tools = !matches!(kind, B::LlamaCpp | B::OpenAiCompatible);
    let tool_choice_required = !matches!(kind, B::OpenAiCompatible);
    let strict_schema = matches!(kind, B::OpenAiResponses | B::OpenAiChat);
    let prefill = matches!(kind, B::LlamaCpp);
    let cached_tokens_field = match kind {
        B::LlamaCpp => CachedTokensField::LlamaTimings,
        B::Vllm | B::OpenRouter | B::OpenAiChat | B::OpenAiCompatible => {
            CachedTokensField::PromptTokensDetails
        }
        B::Anthropic => CachedTokensField::AnthropicCacheRead,
        B::OpenAiResponses => CachedTokensField::ResponsesInputDetails,
    };
    let token_count_source = match kind {
        B::LlamaCpp => TokenCountSource::LlamaTokenize,
        B::Anthropic => TokenCountSource::AnthropicCountTokens,
        // Коннектор Chat Completions просит `stream_options.include_usage` у всех, в том
        // числе у совместимого сервера: источник один — `Usage` (с оценкой до ответа).
        B::Vllm | B::OpenRouter | B::OpenAiResponses | B::OpenAiChat | B::OpenAiCompatible => {
            TokenCountSource::Usage
        }
    };
    let tool_id_format = match kind {
        B::Anthropic => ToolIdFormat::Prefixed("toolu_".into()),
        B::OpenAiResponses | B::OpenAiChat => ToolIdFormat::Prefixed("call_".into()),
        B::LlamaCpp | B::Vllm | B::OpenRouter | B::OpenAiCompatible => ToolIdFormat::Any,
    };
    ProviderCapabilities {
        wire,
        reasoning_field,
        reasoning_roundtrip,
        tool_format: ToolFormat::Native,
        parallel_tools,
        tool_choice_required,
        strict_schema,
        prefill,
        cached_tokens_field,
        context_source: ContextSource::Conservative,
        token_count_source,
        single_system: true,
        tool_id_format,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_caps_per_backend() {
        assert_eq!(
            default_caps(BackendKind::Anthropic).wire,
            Wire::AnthropicMessages
        );
        assert!(default_caps(BackendKind::Anthropic).single_system);
        assert_eq!(
            default_caps(BackendKind::LlamaCpp).cached_tokens_field,
            CachedTokensField::LlamaTimings
        );
        assert_eq!(
            default_caps(BackendKind::OpenAiResponses).wire,
            Wire::Responses
        );
        assert_eq!(
            default_caps(BackendKind::LlamaCpp).context_source,
            ContextSource::Conservative
        );
    }

    #[test]
    fn backend_kind_serde_names() {
        assert_eq!(
            serde_json::to_value(BackendKind::OpenRouter).unwrap(),
            "openrouter"
        );
        assert_eq!(
            serde_json::to_value(BackendKind::OpenAiResponses).unwrap(),
            "openai_responses"
        );
        assert_eq!(
            serde_json::to_value(BackendKind::LlamaCpp).unwrap(),
            "llama_cpp"
        );
    }
}
