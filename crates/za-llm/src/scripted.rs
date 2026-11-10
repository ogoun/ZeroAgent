//! Детерминированная модель для тестов R1-B, без сети.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::mpsc;
use za_protocol::RunId;

use crate::caps::ProviderCapabilities;
use crate::error::{ErrorClass, ModelError};
use crate::port::{AbortOnDrop, BoxFuture, ModelCall, ModelPort, ModelStream};
use crate::types::{EndpointId, StreamEvent};

type Ev = Result<StreamEvent, ModelError>;

/// Как сценарий отдаёт события.
enum Mode {
    /// Все события сразу, затем канал закрыт.
    Closed,
    /// Все события сразу, канал открыт, пока стрим не брошен.
    Open,
    /// Первое событие сразу, каждое следующее — через `gap` после предыдущего.
    Paced(Duration),
}

/// Сценарий одного вызова: события стрима по порядку и способ их отдачи.
struct Script {
    events: Vec<Ev>,
    mode: Mode,
}

/// Модель, отвечающая заранее заданными сценариями по порядку вызовов.
/// Возможности одинаковы для любого эндпоинта.
pub struct ScriptedModel {
    caps: ProviderCapabilities,
    context: Option<u32>,
    /// Счётчик токенов для `count_tokens`; `None` — счёта нет (как у облачных бэкендов).
    tokens: Option<fn(&str) -> u32>,
    scripts: Mutex<VecDeque<Script>>,
    calls: Mutex<Vec<ModelCall>>,
    parse_errors: Mutex<Vec<(RunId, ModelCall)>>,
    finished: Mutex<Vec<RunId>>,
    /// Число живых стримов (guard ещё не уничтожен).
    open: Arc<AtomicUsize>,
}

/// Отравленный мьютекс не мешает тестовой модели: данные — простые списки.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Guard стрима: считает живые стримы и держит то, что живёт вместе со стримом
/// (отправитель открытого канала, задача размеренной отдачи).
struct StreamGuard {
    open: Arc<AtomicUsize>,
    _keep: Box<dyn Send + Sync>,
}

impl StreamGuard {
    fn new(open: &Arc<AtomicUsize>, keep: Box<dyn Send + Sync>) -> Self {
        open.fetch_add(1, Ordering::SeqCst);
        Self {
            open: open.clone(),
            _keep: keep,
        }
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ScriptedModel {
    pub fn new(caps: ProviderCapabilities) -> Self {
        Self {
            caps,
            context: None,
            tokens: None,
            scripts: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            parse_errors: Mutex::new(Vec::new()),
            finished: Mutex::new(Vec::new()),
            open: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Размер контекста для `context_tokens()`; без вызова — `None`.
    pub fn with_context(mut self, tokens: u32) -> Self {
        self.context = Some(tokens);
        self
    }

    /// Счётчик токенов для `count_tokens()`; без вызова — `None` (счёта нет).
    pub fn with_token_counter(mut self, f: fn(&str) -> u32) -> Self {
        self.tokens = Some(f);
        self
    }

    fn push_script(&self, events: Vec<Ev>, mode: Mode) {
        lock(&self.scripts).push_back(Script { events, mode });
    }

    /// Добавить сценарий в конец очереди: события сразу, затем канал закрыт.
    pub fn push(&self, events: Vec<Result<StreamEvent, ModelError>>) {
        self.push_script(events, Mode::Closed);
    }

    /// Сценарий, канал которого остаётся открытым после событий, пока стрим не брошен
    /// (модель «молчит» после последнего события).
    pub fn push_open(&self, events: Vec<Result<StreamEvent, ModelError>>) {
        self.push_script(events, Mode::Open);
    }

    /// Первое событие сразу, следующие — через `gap` каждое (`tokio::time::sleep`,
    /// поэтому `pause`/`advance` им управляют); после последнего канал закрыт. Задача
    /// отдачи — [`AbortOnDrop`]: drop стрима её отменяет.
    pub fn push_paced(&self, events: Vec<Result<StreamEvent, ModelError>>, gap: Duration) {
        self.push_script(events, Mode::Paced(gap));
    }

    /// Все вызовы `stream` по порядку, включая те, на которые сценария не нашлось.
    pub fn calls(&self) -> Vec<ModelCall> {
        lock(&self.calls).clone()
    }

    /// Вызовы `record_parse_error` по порядку.
    pub fn parse_errors(&self) -> Vec<(RunId, ModelCall)> {
        lock(&self.parse_errors).clone()
    }

    /// Живые стримы (guard ещё не уничтожен).
    pub fn open_streams(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    /// Вызовы `finish_run` по порядку.
    pub fn finished_runs(&self) -> Vec<RunId> {
        lock(&self.finished).clone()
    }
}

/// Стрим сценария. Ёмкость канала вмещает весь сценарий: отправка не ждёт получателя,
/// а получатель жив (он в стриме), поэтому `try_send` не падает.
fn open_script(
    script: Script,
    open: &Arc<AtomicUsize>,
    start: tokio::time::Instant,
) -> ModelStream {
    let Script { events, mode } = script;
    let (tx, rx) = mpsc::channel(events.len().max(1));
    let keep: Box<dyn Send + Sync> = match mode {
        Mode::Closed | Mode::Open => {
            for ev in events {
                let _ = tx.try_send(ev);
            }
            match mode {
                Mode::Open => Box::new(tx),
                _ => Box::new(()),
            }
        }
        Mode::Paced(gap) => {
            // Моменты отсчитываются от синхронного вызова `stream` (`start`), а не от
            // первого опроса возвращённого им будущего или задачи: на паузе времени они не
            // зависят от планировщика.
            let mut at = start;
            let mut rest = events.into_iter();
            if let Some(first) = rest.next() {
                let _ = tx.try_send(first);
            }
            let handle = tokio::spawn(async move {
                for ev in rest {
                    at += gap;
                    tokio::time::sleep_until(at).await;
                    if tx.send(ev).await.is_err() {
                        return;
                    }
                }
            });
            Box::new(AbortOnDrop(handle))
        }
    };
    ModelStream::from_channel(rx, Box::new(StreamGuard::new(open, keep)))
}

impl ModelPort for ScriptedModel {
    fn stream(&self, call: ModelCall) -> BoxFuture<'_, Result<ModelStream, ModelError>> {
        lock(&self.calls).push(call);
        let script = lock(&self.scripts).pop_front();
        let start = tokio::time::Instant::now();
        Box::pin(async move {
            let script =
                script.ok_or_else(|| ModelError::new(ErrorClass::Inactive, "сценарий исчерпан"))?;
            Ok(open_script(script, &self.open, start))
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

    fn finish_run(&self, run: RunId) {
        lock(&self.finished).push(run);
    }

    fn count_tokens<'a>(
        &'a self,
        _endpoint: &'a EndpointId,
        text: &'a str,
    ) -> BoxFuture<'a, Option<u32>> {
        let n = self.tokens.map(|f| f(text));
        Box::pin(async move { n })
    }

    /// Канонический JSON запроса: у тестовой модели нет провайдера и его тела (R1-B2 D52).
    fn render_request(&self, call: &ModelCall) -> Option<String> {
        serde_json::to_value(&call.request)
            .ok()
            .map(|v| za_protocol::canonical_json(&v))
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
                max_wait: None,
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
        assert_eq!(m.count_tokens(&ep, "abcd").await, None);
        let m = m.with_token_counter(|s| s.len() as u32);
        assert_eq!(m.count_tokens(&ep, "abcd").await, Some(4));
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

    #[tokio::test]
    async fn push_open_keeps_stream_until_drop() {
        let m = ScriptedModel::new(default_caps(BackendKind::LlamaCpp));
        m.push_open(vec![Ok(StreamEvent::TextDelta("a".into()))]);
        let mut s = m.stream(call("local", "q")).await.unwrap();
        assert!(matches!(s.next().await, Some(Ok(StreamEvent::TextDelta(t))) if t == "a"));
        let more = tokio::time::timeout(Duration::from_millis(50), s.next()).await;
        assert!(more.is_err(), "канал должен оставаться открытым: {more:?}");
        assert_eq!(m.open_streams(), 1);
        drop(s);
        assert_eq!(m.open_streams(), 0);
    }

    /// Уже пришедшие события стрима без ожидания: нулевой предел опрашивает стрим один
    /// раз, прежде чем истечь.
    async fn ready_events(s: &mut ModelStream) -> usize {
        let mut n = 0;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::ZERO, s.next()).await {
            n += 1;
        }
        n
    }

    #[tokio::test(start_paused = true)]
    async fn push_paced_spaces_events() {
        let m = ScriptedModel::new(default_caps(BackendKind::LlamaCpp));
        m.push_paced(
            vec![
                Ok(StreamEvent::TextDelta("a".into())),
                Ok(StreamEvent::TextDelta("b".into())),
            ],
            Duration::from_secs(1),
        );
        let mut s = m.stream(call("local", "q")).await.unwrap();
        tokio::time::advance(Duration::from_millis(500)).await;
        let mut got = ready_events(&mut s).await;
        assert_eq!(got, 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        got += ready_events(&mut s).await;
        assert_eq!(got, 2);
        assert_eq!(m.open_streams(), 1);
        drop(s);
        assert_eq!(m.open_streams(), 0);
    }

    #[test]
    fn finish_run_recorded() {
        let m = ScriptedModel::new(default_caps(BackendKind::LlamaCpp));
        let r = RunId::new();
        m.finish_run(r);
        assert_eq!(m.finished_runs(), vec![r]);
    }
}
