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

/// Способ мышления модели (не поле MDL-002): заполняет зонд, читают коннекторы.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ThinkingSupport {
    /// Адаптивное мышление (`{"type":"adaptive"}`).
    pub adaptive: bool,
    /// Поддерживается уровень усилия (`effort`).
    pub effort: bool,
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

/// Чем считаются токены.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenCountSource {
    LlamaTokenize,
    AnthropicCountTokens,
    /// По `usage` прошлых ответов.
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
    // llama-server по умолчанию не даёт параллельных вызовов; о произвольном
    // совместимом сервере ничего не известно — консервативно.
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
        B::Vllm | B::OpenRouter | B::OpenAiResponses | B::OpenAiChat => TokenCountSource::Usage,
        B::OpenAiCompatible => TokenCountSource::CharsDiv4,
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
