//! Детерминированная модель для тестов R1-B, без сети.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::mpsc;
use za_protocol::RunId;

use crate::caps::ProviderCapabilities;
use crate::error::{ErrorClass, ModelError};
use crate::port::{BoxFuture, ModelCall, ModelPort, ModelStream};
use crate::types::{EndpointId, StreamEvent};

/// Сценарий одного вызова: события стрима по порядку.
type Script = Vec<Result<StreamEvent, ModelError>>;

/// Модель, отвечающая заранее заданными сценариями по порядку вызовов.
/// Возможности одинаковы для любого эндпоинта.
pub struct ScriptedModel {
    caps: ProviderCapabilities,
    context: Option<u32>,
    scripts: Mutex<VecDeque<Script>>,
    calls: Mutex<Vec<ModelCall>>,
    parse_errors: Mutex<Vec<(RunId, ModelCall)>>,
}

/// Отравленный мьютекс не мешает тестовой модели: данные — простые списки.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ScriptedModel {
    pub fn new(caps: ProviderCapabilities) -> Self {
        Self {
            caps,
            context: None,
            scripts: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            parse_errors: Mutex::new(Vec::new()),
        }
    }

    /// Размер контекста для `context_tokens()`; без вызова — `None`.
    pub fn with_context(mut self, tokens: u32) -> Self {
        self.context = Some(tokens);
        self
    }

    /// Добавить сценарий в конец очереди.
    pub fn push(&self, events: Vec<Result<StreamEvent, ModelError>>) {
        lock(&self.scripts).push_back(events);
    }

    /// Все вызовы `stream` по порядку, включая те, на которые сценария не нашлось.
    pub fn calls(&self) -> Vec<ModelCall> {
        lock(&self.calls).clone()
    }

    /// Вызовы `record_parse_error` по порядку.
    pub fn parse_errors(&self) -> Vec<(RunId, ModelCall)> {
        lock(&self.parse_errors).clone()
    }
}

impl ModelPort for ScriptedModel {
    fn stream(&self, call: ModelCall) -> BoxFuture<'_, Result<ModelStream, ModelError>> {
        lock(&self.calls).push(call);
        let script = lock(&self.scripts).pop_front();
        Box::pin(async move {
            let events =
                script.ok_or_else(|| ModelError::new(ErrorClass::Inactive, "сценарий исчерпан"))?;
            // Ёмкость канала вмещает весь сценарий: отправка не ждёт получателя.
            let (tx, rx) = mpsc::channel(events.len().max(1));
            for ev in events {
                // Получатель жив (он ниже), а места хватает, поэтому отправка не падает.
                let _ = tx.try_send(ev);
            }
            Ok(ModelStream::from_channel(rx, Box::new(())))
        })
    }

    fn capabilities(&self, _endpoint: &EndpointId) -> Option<ProviderCapabilities> {
        Some(self.caps.clone())
    }

    fn context_tokens(&self, _endpoint: &EndpointId) -> Option<u32> {
        self.context
    }

    fn record_parse_error(&self, run: RunId, call: &ModelCall) {
        lock(&self.parse_errors).push((run, call.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{BackendKind, default_caps};
    use crate::error::ErrorClass;
    use crate::port::RequestMeta;
    use crate::types::{
        ChatRequest, EndpointId, FinishReason, Message, Part, ReasoningMode, Role, Sampling,
        ToolChoice, Usage,
    };
    use za_protocol::Priority;

    fn call(ep: &str, text: &str) -> ModelCall {
        ModelCall {
            endpoint: EndpointId(ep.into()),
            meta: RequestMeta {
                run_id: RunId::new(),
                priority: Priority::Worker,
                session: None,
            },
            request: ChatRequest {
                system: "s".into(),
                tools: vec![],
                messages: vec![Message {
                    role: Role::User,
                    parts: vec![Part::Text(text.into())],
                }],
                tool_choice: ToolChoice::Auto,
                max_output_tokens: None,
                sampling: Sampling::default(),
                reasoning: ReasoningMode::Default,
                cache_key: None,
            },
        }
    }

    fn answer(text: &str) -> Vec<Result<StreamEvent, ModelError>> {
        vec![
            Ok(StreamEvent::TextDelta(text.into())),
            Ok(StreamEvent::Finished {
                message: Message {
                    role: Role::Assistant,
                    parts: vec![Part::Text(text.into())],
                },
                finish: FinishReason::Stop,
                usage: Usage::default(),
            }),
        ]
    }

    #[tokio::test]
    async fn scripted_model_records_calls() {
        let m = ScriptedModel::new(default_caps(BackendKind::LlamaCpp));
        m.push(answer("первый"));
        m.push(answer("второй"));
        let call_a = call("local", "a");
        let call_b = call("local", "b");
        let ra = m
            .stream(call_a.clone())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let rb = m
            .stream(call_b.clone())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(ra.message.parts, vec![Part::Text("первый".into())]);
        assert_eq!(rb.message.parts, vec![Part::Text("второй".into())]);
        assert_eq!(m.calls(), vec![call_a.clone(), call_b.clone()]);
        let err = m.stream(call("local", "c")).await.unwrap_err();
        assert_eq!(err.class, ErrorClass::Inactive);

        let run = call_a.meta.run_id;
        m.record_parse_error(run, &call_a);
        assert_eq!(m.parse_errors(), vec![(run, call_a.clone())]);

        let ep = EndpointId("local".into());
        assert_eq!(m.context_tokens(&ep), None);
        assert_eq!(
            m.capabilities(&ep),
            Some(default_caps(BackendKind::LlamaCpp))
        );
        let m = m.with_context(32768);
        assert_eq!(m.context_tokens(&ep), Some(32768));
    }

    #[tokio::test]
    async fn collect_ignores_service_message() {
        let m = ScriptedModel::new(default_caps(BackendKind::LlamaCpp));
        let mut events = vec![Ok(StreamEvent::ServiceMessage(Message {
            role: Role::System,
            parts: vec![Part::Text("x".into())],
        }))];
        events.extend(answer("a"));
        m.push(events);
        let r = m
            .stream(call("local", "q"))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(r.message.parts, vec![Part::Text("a".into())]);
    }
}
