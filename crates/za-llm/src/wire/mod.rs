//! Коннекторы провайдеров (функция `http`): перевод канонического запроса в протокол
//! провайдера и событий его стрима — в [`StreamEvent`] (D35).
//!
//! Коннекторы — внутренняя часть крейта (D38): рабочий код пользуется ими только через
//! `Gateway`, интеграционные тесты — через `za_llm::__test`.

use std::collections::BTreeMap;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::caps::{ProviderCapabilities, ThinkingSupport};
use crate::error::ModelError;
use crate::http::{HttpTransport, ProviderLimits, Secret};
use crate::types::{ChatRequest, Message, Part, ReplayBlob, Role, StreamEvent, ToolCall};

mod chat;

pub use chat::{ChatDecoder, ChatDialect, ChatWire};

/// Ёмкость канала событий стрима одной попытки.
pub(crate) const STREAM_CHANNEL: usize = 64;

/// Данные эндпоинта для одного запроса, кроме самого запроса.
#[derive(Debug, Clone, Copy)]
pub struct WireCtx<'a> {
    pub caps: &'a ProviderCapabilities,
    /// Слот llama.cpp (`id_slot`).
    pub slot: Option<u32>,
    /// Ключ облачного провайдера; `None` — без авторизации.
    pub secret: Option<&'a Secret>,
    /// Способ мышления из профиля модели (задача 9); `None` — до зонда.
    pub thinking: Option<&'a ThinkingSupport>,
    /// Предел вывода модели из профиля (нужен Anthropic, D36).
    pub max_output_tokens: Option<u32>,
}

/// Открытый стрим одной попытки (D35): лимиты из заголовков успешного ответа и канал
/// событий. Обрыв, idle-таймаут и ошибка внутри стрима — последнее `Err` в канале;
/// drop приёмника закрывает соединение (D2).
#[derive(Debug)]
pub struct OpenedStream {
    pub limits: ProviderLimits,
    pub events: mpsc::Receiver<Result<StreamEvent, ModelError>>,
}

/// Коннектор эндпоинта. Задачи 5 и 6 добавляют варианты Anthropic Messages и Responses.
#[derive(Debug, Clone)]
pub enum Connector {
    Chat(ChatWire),
}

impl Connector {
    /// Тело запроса провайдеру. Запрос, который провайдер заведомо отвергнет, —
    /// `BadRequest` до отправки.
    pub fn build_body(&self, req: &ChatRequest, ctx: &WireCtx<'_>) -> Result<Value, ModelError> {
        match self {
            Self::Chat(w) => w.build_body(req, ctx),
        }
    }

    /// Отправляет запрос и запускает насос стрима. Статус ≠ 2xx и сбой соединения —
    /// `Err` отсюда, канал не создаётся.
    pub async fn open(
        &self,
        http: &HttpTransport,
        req: &ChatRequest,
        ctx: &WireCtx<'_>,
    ) -> Result<OpenedStream, ModelError> {
        match self {
            Self::Chat(w) => w.open(http, req, ctx).await,
        }
    }
}

/// Вызов инструмента, собираемый из фрагментов.
#[derive(Debug, Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

/// Собирает итоговое [`Message`] ассистента из событий стрима; общий для коннекторов
/// задач 4–6 (D35).
#[derive(Debug, Default)]
pub struct MessageAssembler {
    text: String,
    reasoning: String,
    replay: Option<ReplayBlob>,
    tool_calls: BTreeMap<u32, PendingCall>,
}

impl MessageAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Учитывает событие: `TextDelta`, `ReasoningDelta`, `ToolCallStart`,
    /// `ToolCallArgs`; прочие события не меняют сообщение.
    pub fn on_event(&mut self, ev: &StreamEvent) {
        match ev {
            StreamEvent::TextDelta(t) => self.text.push_str(t),
            StreamEvent::ReasoningDelta(t) => self.reasoning.push_str(t),
            StreamEvent::ToolCallStart { index, id, name } => {
                let c = self.tool_calls.entry(*index).or_default();
                c.id.clone_from(id);
                c.name.clone_from(name);
            }
            StreamEvent::ToolCallArgs { index, fragment } => {
                self.tool_calls
                    .entry(*index)
                    .or_default()
                    .arguments
                    .push_str(fragment);
            }
            _ => {}
        }
    }

    /// Данные провайдера для возврата рассуждения в следующем запросе.
    pub fn set_reasoning_replay(&mut self, blob: ReplayBlob) {
        self.replay = Some(blob);
    }

    /// Итоговое сообщение. Порядок частей: рассуждение, текст, вызовы по `index`.
    /// Аргументы, не разобранные как JSON, остаются только в `raw_arguments`, а
    /// `arguments` равно `Null`: ремонт делает нормализация.
    pub fn finish(self) -> Message {
        let mut parts = Vec::new();
        if !self.reasoning.is_empty() || self.replay.is_some() {
            parts.push(Part::Reasoning {
                text: self.reasoning,
                replay: self.replay,
            });
        }
        if !self.text.is_empty() {
            parts.push(Part::Text(self.text));
        }
        for (_, c) in self.tool_calls {
            let arguments = serde_json::from_str(&c.arguments).unwrap_or(Value::Null);
            parts.push(Part::ToolCall(ToolCall {
                id: c.id,
                name: c.name,
                arguments,
                raw_arguments: Some(c.arguments),
            }));
        }
        Message {
            role: Role::Assistant,
            parts,
        }
    }
}
