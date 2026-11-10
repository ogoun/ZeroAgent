//! Трейт `ModelPort` и стрим ответа модели.
//!
//! Ядро (`za-core`) видит модель только через `ModelPort`; рабочая реализация —
//! `Gateway`, тестовая — `ScriptedModel`.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use za_protocol::{Priority, RunId};

use crate::caps::ProviderCapabilities;
use crate::error::{ErrorClass, ModelError};
use crate::types::{ChatRequest, EndpointId, ModelResponse, StreamEvent};

/// Будущее с владением на время `'a`, без зависимости от `futures`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Служебные данные запроса: чей он и с каким приоритетом.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestMeta {
    pub run_id: RunId,
    pub priority: Priority,
    /// Сессия для привязки к слоту llama.cpp (MDL-011).
    pub session: Option<String>,
    /// Предел ожидания в очереди (D12 R1-A); `None` — без предела (RES-007). Истёк —
    /// ошибка `Queue` с текстом `wait_timeout`.
    #[serde(default)]
    pub max_wait: Option<Duration>,
}

/// Вызов модели: куда, от кого и что.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCall {
    pub endpoint: EndpointId,
    pub meta: RequestMeta,
    pub request: ChatRequest,
}

/// Порт модели для ядра.
pub trait ModelPort: Send + Sync {
    /// Начать вызов. `Err` — отказ до начала стрима; ошибки по ходу приходят в стриме.
    fn stream(&self, call: ModelCall) -> BoxFuture<'_, Result<ModelStream, ModelError>>;
    /// Возможности эндпоинта; `None` — эндпоинт неизвестен.
    fn capabilities(&self, endpoint: &EndpointId) -> Option<ProviderCapabilities>;
    /// Размер контекста модели эндпоинта в токенах (D43, для компакции).
    fn context_tokens(&self, endpoint: &EndpointId) -> Option<u32>;
    /// Цикл сообщает, что вызов инструмента из ответа на `call` не разобран
    /// (D43, MDL-018): идентичный повтор запроса будет отвергнут.
    fn record_parse_error(&self, run: RunId, call: &ModelCall);
    /// Run в конечном состоянии: забыть отпечатки запросов после ошибок разбора
    /// (`ParseGuard`). Цикл вызывает при любом конечном состоянии run. По умолчанию
    /// ничего не делает.
    fn finish_run(&self, _run: RunId) {}
    /// Точное число токенов `text` токенизатором модели эндпоинта (R1-B2 D41); `None` —
    /// эндпоинт так не умеет или счёт не удался: вызывающий берёт оценку по байтам. По
    /// умолчанию — `None`.
    fn count_tokens<'a>(
        &'a self,
        _endpoint: &'a EndpointId,
        _text: &'a str,
    ) -> BoxFuture<'a, Option<u32>> {
        Box::pin(async { None })
    }
    /// Тело запроса провайдеру без заголовков и ключей (R1-B2 D52, OPS-003): то, что
    /// реализация отправила бы на `call` сейчас. `None` — не умеет (эндпоинт неизвестен,
    /// не зондирован или реализация так не умеет). По умолчанию — `None`.
    fn render_request(&self, _call: &ModelCall) -> Option<String> {
        None
    }
}

/// Guard фоновой задачи стрима: `Drop` вызывает `JoinHandle::abort()` (D2).
/// Простой drop `JoinHandle` задачу лишь отсоединяет.
pub struct AbortOnDrop(pub tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Стрим событий ответа. Drop закрывает канал и уничтожает guard, что отменяет
/// фоновую задачу и закрывает соединение (D2).
pub struct ModelStream {
    rx: mpsc::Receiver<Result<StreamEvent, ModelError>>,
    _guard: Box<dyn Send + Sync>,
}

impl std::fmt::Debug for ModelStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelStream").finish_non_exhaustive()
    }
}

impl ModelStream {
    /// Стрим из канала; `guard` живёт, пока жив стрим (обычно `AbortOnDrop`).
    pub fn from_channel(
        rx: mpsc::Receiver<Result<StreamEvent, ModelError>>,
        guard: Box<dyn Send + Sync>,
    ) -> Self {
        Self { rx, _guard: guard }
    }

    /// Следующее событие; `None` — канал закрыт.
    pub async fn next(&mut self) -> Option<Result<StreamEvent, ModelError>> {
        self.rx.recv().await
    }

    /// Дождаться итога. Ответ берётся из `Finished` последней попытки: дельты не
    /// накапливаются, поэтому всё, что пришло до `Restart`, отбрасывается (D20).
    /// `Err` из канала — итог; канал закрыт до `Finished` — `StreamCut`.
    pub async fn collect(mut self) -> Result<ModelResponse, ModelError> {
        while let Some(ev) = self.next().await {
            match ev? {
                StreamEvent::Finished {
                    message,
                    finish,
                    usage,
                } => {
                    return Ok(ModelResponse {
                        message,
                        finish,
                        usage,
                    });
                }
                _ => continue,
            }
        }
        Err(ModelError::new(
            ErrorClass::StreamCut,
            "стрим закрыт до события Finished",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorClass;
    use crate::types::{FinishReason, Message, Part, Role, Usage};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn finished(text: &str) -> StreamEvent {
        StreamEvent::Finished {
            message: Message {
                role: Role::Assistant,
                parts: vec![Part::Text(text.into())],
            },
            finish: FinishReason::Stop,
            usage: Usage::default(),
        }
    }

    fn stream_of(events: Vec<Result<StreamEvent, ModelError>>) -> ModelStream {
        let (tx, rx) = mpsc::channel(events.len().max(1));
        for e in events {
            tx.try_send(e).unwrap();
        }
        ModelStream::from_channel(rx, Box::new(()))
    }

    #[tokio::test]
    async fn collect_discards_partial_output_on_restart() {
        let s = stream_of(vec![
            Ok(StreamEvent::TextDelta("обры".into())),
            Ok(StreamEvent::Restart {
                attempt: 2,
                reason: "обрыв".into(),
            }),
            Ok(StreamEvent::TextDelta("целый".into())),
            Ok(finished("целый")),
        ]);
        let resp = s.collect().await.unwrap();
        assert_eq!(resp.message.parts, vec![Part::Text("целый".into())]);
    }

    #[tokio::test]
    async fn collect_without_finished_is_stream_cut() {
        let s = stream_of(vec![Ok(StreamEvent::TextDelta("обры".into()))]);
        let err = s.collect().await.unwrap_err();
        assert_eq!(err.class, ErrorClass::StreamCut);
    }

    /// Guard, выставляющий флаг в `Drop`.
    struct Flag(Arc<AtomicBool>);
    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn dropping_stream_drops_guard() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = mpsc::channel::<Result<StreamEvent, ModelError>>(1);
        let stream = ModelStream::from_channel(rx, Box::new(Flag(dropped.clone())));
        assert!(!dropped.load(Ordering::SeqCst));
        drop(stream);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn abort_on_drop_cancels_task() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            let _ = tx.send(());
        });
        drop(AbortOnDrop(handle));
        // Ограничение 5 с только страхует тест от зависания: отменённая задача
        // уничтожает Sender сразу.
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("задача не отменена: Sender жив");
        assert!(res.is_err(), "Sender должен быть уничтожен без отправки");
    }
}
