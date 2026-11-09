//! `Gateway` (задача 15): единственный путь к модели — зонды, очередь, слоты, повторы,
//! предохранитель, защита от повтора после ошибки разбора, стоимость, выбор модели.
//! Поддельный axum-сервер, реальное время с короткими параметрами `GatewayOptions` (D41),
//! облачных ключей нет.

mod support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use support::{FakeServer, Reply, fixture};
use tokio::sync::watch;
use tokio::time::Instant;
use za_llm::config::{ConfigError, ModelsConfig};
use za_llm::http::{Secret, SecretSource, Timeouts};
use za_llm::profile::{EndpointState, ProfileStore};
use za_llm::resilience::{BreakerOptions, RetryPolicy};
use za_llm::{
    ChatRequest, CostSource, DEFAULT_REPROBE_MAX, DEFAULT_REPROBE_MIN, EndpointId, ErrorClass,
    Gateway, GatewayOptions, Message, ModelCall, ModelError, ModelPort, ModelStream, Part,
    RequestMeta, Role, SelectError, Selection, StreamEvent, WaitState,
};
use za_protocol::{Priority, RunId, RunRole};
use za_res::{Admission, CpuSample, GpuStatus, MemSample, ResourceSnapshot, ResourcesConfig};

type Ev = Result<StreamEvent, ModelError>;

const CHAT: &str = "POST /v1/chat/completions";
const PROPS: &str = "GET /props";
const MODEL_A: &str = "Qwen3.8-27B-Q4_K_M.gguf";
const MODEL_B: &str = "gemma-4-12b-it-IQ3_XXS.gguf";

/// Ключи теста по `key_ref`.
#[derive(Default)]
struct MapSecrets(HashMap<String, String>);

impl SecretSource for MapSecrets {
    fn get(&self, key_ref: &str) -> Option<Secret> {
        self.0.get(key_ref).cloned().map(Secret::new)
    }
}

fn secrets(pairs: &[(&str, &str)]) -> Arc<dyn SecretSource> {
    Arc::new(MapSecrets(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    ))
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// D41: повторы 1–5 мс, таймауты 500 мс, пауза предохранителя 50 мс.
fn opts() -> GatewayOptions {
    let t = Timeouts {
        connect: ms(500),
        first_byte: ms(500),
        idle: ms(500),
    };
    GatewayOptions {
        retry: RetryPolicy {
            base: ms(1),
            cap: ms(5),
            ..RetryPolicy::default()
        },
        timeouts_local: t,
        timeouts_cloud: t,
        breaker: BreakerOptions {
            cooldown: ms(50),
            ..BreakerOptions::default()
        },
        probe_deadline: Duration::from_secs(2),
        reprobe_min: DEFAULT_REPROBE_MIN,
        reprobe_max: DEFAULT_REPROBE_MAX,
    }
}

/// Без повторов.
fn opts_no_retry() -> GatewayOptions {
    let mut o = opts();
    o.retry.max_retries = 0;
    o
}

/// Долгий idle: зависший стрим держит место, пока тест его не отпустит.
fn opts_long_idle() -> GatewayOptions {
    let mut o = opts();
    o.timeouts_local.idle = Duration::from_secs(30);
    o
}

fn store(name: &str) -> ProfileStore {
    let d = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("gateway")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    ProfileStore::open(&d).unwrap()
}

fn models(v: Value) -> ModelsConfig {
    serde_json::from_value(v).unwrap()
}

fn fixture_json(path: &str) -> Value {
    serde_json::from_slice(&fixture(path)).unwrap()
}

fn props(path: &str) -> Reply {
    Reply::Json(200, fixture_json(path))
}

/// Фикстура SSE, разрезанная по событиям (`\n\n`).
fn sse_events(bytes: &[u8]) -> Vec<Vec<u8>> {
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    text.split_inclusive("\n\n")
        .map(|s| s.as_bytes().to_vec())
        .collect()
}

fn llama_text() -> Reply {
    Reply::sse(sse_events(&fixture("chat/llama_text.sse")))
}

/// SSE: первый чанк (роль и первый текст) сразу, остальные — через 10 с каждый. Сервер
/// считает отключением drop тела до последнего чанка.
fn hang_after_first() -> Reply {
    let ev = sse_events(&fixture("chat/llama_text.sse"));
    let mut chunks = vec![[ev[0].clone(), ev[1].clone()].concat()];
    chunks.extend(ev[2..].iter().cloned());
    Reply::Sse {
        chunks,
        delay: Duration::from_secs(10),
        cut_after: None,
        then_hang: false,
    }
}

/// Сервер llama.cpp: `/props` и `/v1/chat/completions`.
async fn llama_server(props_replies: Vec<Reply>, chat: Vec<Reply>) -> FakeServer {
    FakeServer::start(vec![(PROPS, props_replies), (CHAT, chat)]).await
}

fn llama_cfg(base: &str, extra: Value) -> ModelsConfig {
    let mut ep = json!({"id": "local", "backend": "llama_cpp", "base_url": base});
    if let (Some(o), Some(e)) = (ep.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            o.insert(k.clone(), v.clone());
        }
    }
    models(json!({"default": "local", "endpoints": [ep]}))
}

/// `Gateway::start` и ожидание первых зондов (start их не ждёт).
async fn started(
    cfg: ModelsConfig,
    secrets: Arc<dyn SecretSource>,
    admission: Option<Admission>,
    store: ProfileStore,
    o: GatewayOptions,
) -> Result<Gateway, ConfigError> {
    let g = Gateway::start(cfg, secrets, admission, store, o).await?;
    assert!(
        g.wait_probed(Duration::from_secs(5)).await,
        "зонды не завершены"
    );
    Ok(g)
}

async fn start(cfg: ModelsConfig, name: &str, o: GatewayOptions) -> Gateway {
    started(cfg, secrets(&[]), None, store(name), o)
        .await
        .unwrap()
}

fn id(s: &str) -> EndpointId {
    EndpointId(s.to_owned())
}

fn request(text: &str) -> ChatRequest {
    ChatRequest {
        system: "Ты агент.".into(),
        messages: vec![Message {
            role: Role::User,
            parts: vec![Part::Text(text.into())],
        }],
        ..ChatRequest::default()
    }
}

fn call_on(ep: &str, run: RunId, text: &str) -> ModelCall {
    ModelCall {
        endpoint: id(ep),
        meta: RequestMeta {
            run_id: run,
            priority: Priority::Interactive,
            session: None,
        },
        request: request(text),
    }
}

/// Все события стрима до закрытия канала (не дольше 10 с).
async fn drain(mut s: ModelStream) -> Vec<Ev> {
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(ev) = s.next().await {
            out.push(ev);
        }
    })
    .await
    .expect("стрим не закрылся за 10 с");
    out
}

/// Ждёт условия до `limit`, проверяя каждые 10 мс.
async fn eventually(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        if cond() {
            return true;
        }
        tokio::time::sleep(ms(10)).await;
    }
    cond()
}

fn view_of(g: &Gateway, ep: &str) -> za_llm::EndpointView {
    g.endpoints()
        .into_iter()
        .find(|v| v.id == id(ep))
        .expect("эндпоинт в списке")
}

/// Снимок «всего вдоволь» для допуска.
fn fresh_snapshot() -> ResourceSnapshot {
    let now = jiff::Timestamp::now();
    ResourceSnapshot {
        sampled_at: now,
        sampled_mono: Some(Instant::now()),
        cpu: CpuSample {
            usage_percent: 5.0,
            cores: 16,
            busy_ms: 0,
        },
        ram: MemSample {
            total: 64 << 30,
            available: 32 << 30,
        },
        gpu_sampled_at: Some(now),
        gpu_sampled_mono: Some(Instant::now()),
        gpu: GpuStatus::Unavailable {
            reason: "тест".into(),
        },
        volumes_sampled_at: None,
        volumes_sampled_mono: None,
        volumes: vec![],
        categories_sampled_at: None,
        categories_sampled_mono: None,
        categories: vec![],
    }
}

#[tokio::test]
async fn stream_goes_through_queue() {
    let srv = llama_server(vec![props("probe/llama_props.json")], vec![llama_text()]).await;
    let (_snap_tx, snap_rx) = watch::channel(fresh_snapshot());
    let admission = Admission::new(
        snap_rx,
        ResourcesConfig::default(),
        std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")),
    );
    let g = started(
        llama_cfg(&srv.base_url, json!({})),
        secrets(&[]),
        Some(admission),
        store("through_queue"),
        opts(),
    )
    .await
    .unwrap();
    let s = g
        .stream(call_on("local", RunId::new(), "Привет"))
        .await
        .unwrap();
    let evs = drain(s).await;
    assert!(evs.iter().all(Result::is_ok), "ошибки в стриме: {evs:?}");
    let evs: Vec<StreamEvent> = evs.into_iter().map(Result::unwrap).collect();
    let queued = evs
        .iter()
        .take_while(|e| matches!(e, StreamEvent::Queued(_)))
        .count();
    assert!(queued >= 1, "первым событием должен быть Queued: {evs:?}");
    match &evs[queued] {
        StreamEvent::Started {
            endpoint,
            slot,
            attempt,
        } => {
            assert_eq!(endpoint, &id("local"));
            assert!(slot.is_some());
            assert_eq!(*attempt, 1);
        }
        other => panic!("после Queued ожидался Started: {other:?}"),
    }
    assert!(matches!(evs[queued + 1], StreamEvent::TextDelta(_)));
    assert!(matches!(evs.last(), Some(StreamEvent::Finished { .. })));
    // Место освобождено.
    assert!(
        eventually(ms(500), || g.view().queue.groups[0].active == 0).await,
        "место очереди не освобождено"
    );
}

#[tokio::test]
async fn inactive_cloud_is_rejected_without_network() {
    let cloud = FakeServer::start(vec![]).await;
    let local = llama_server(vec![props("probe/llama_props.json")], vec![llama_text()]).await;
    let cfg = models(json!({
        "default": "local",
        "endpoints": [
            {"id": "local", "backend": "llama_cpp", "base_url": local.base_url},
            {"id": "cloud", "backend": "openrouter", "base_url": cloud.base_url,
             "model": "qwen/qwen3.8-235b", "key_ref": "openrouter"}
        ]
    }));
    let g = start(cfg, "inactive_cloud", opts()).await;
    let err = g
        .stream(call_on("cloud", RunId::new(), "Привет"))
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Inactive);
    assert_eq!(cloud.total_requests(), 0);
    assert_eq!(view_of(&g, "cloud").state, "inactive");
}

#[tokio::test]
async fn no_automatic_fallback() {
    let local = llama_server(
        vec![props("probe/llama_props.json")],
        vec![Reply::Json(500, json!({"error": "сбой"}))],
    )
    .await;
    let cloud = FakeServer::start(vec![
        ("GET /models", vec![props("probe/openrouter_models.json")]),
        (
            "POST /chat/completions",
            vec![Reply::sse(sse_events(&fixture("chat/openrouter_cost.sse")))],
        ),
    ])
    .await;
    let cfg = models(json!({
        "default": "local",
        "endpoints": [
            {"id": "local", "backend": "llama_cpp", "base_url": local.base_url},
            {"id": "cloud", "backend": "openrouter", "base_url": cloud.base_url,
             "model": "qwen/qwen3.8-235b", "key_ref": "openrouter"}
        ]
    }));
    let g = started(
        cfg,
        secrets(&[("openrouter", "sk-or-test-key")]),
        None,
        store("no_fallback"),
        opts(),
    )
    .await
    .unwrap();
    // Облако готово (зонд прошёл), но запрос к нему не уходит ни при каком исходе.
    assert_eq!(view_of(&g, "cloud").state, "ready");
    let cloud_before = cloud.total_requests();
    let err = g
        .stream(call_on("local", RunId::new(), "Привет"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Server);
    assert_eq!(local.requests(CHAT).len(), 6);
    assert_eq!(cloud.total_requests() - cloud_before, 0);
    assert_eq!(cloud.requests("POST /chat/completions").len(), 0);
}

#[tokio::test]
async fn stream_cut_midtool_then_success() {
    let ev = sse_events(&fixture("chat/llama_tool_calls.sse"));
    let cut = Reply::Sse {
        chunks: ev.clone(),
        delay: Duration::ZERO,
        cut_after: Some(2),
        then_hang: false,
    };
    let srv = llama_server(
        vec![props("probe/llama_props.json")],
        vec![cut, Reply::sse(ev)],
    )
    .await;
    let g = start(llama_cfg(&srv.base_url, json!({})), "cut_midtool", opts()).await;
    let evs = drain(
        g.stream(call_on("local", RunId::new(), "Прочитай"))
            .await
            .unwrap(),
    )
    .await;
    assert!(evs.iter().all(Result::is_ok), "{evs:?}");
    let evs: Vec<StreamEvent> = evs.into_iter().map(Result::unwrap).collect();
    let restart = evs
        .iter()
        .position(|e| matches!(e, StreamEvent::Restart { attempt: 2, .. }))
        .expect("нет Restart { attempt: 2 }");
    // Частичный вызов ушёл до Restart.
    assert!(
        evs[..restart]
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolCallStart { .. }))
    );
    let Some(StreamEvent::Finished { message, .. }) = evs.last() else {
        panic!("нет Finished: {evs:?}");
    };
    let calls: Vec<_> = message
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::ToolCall(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "read_file");
    assert_eq!(calls[0].arguments, json!({"path": "a.txt"}));
    assert_eq!(srv.requests(CHAT).len(), 2);
}

/// Облако OpenRouter на поддельном сервере.
async fn openrouter_server(chat: Vec<Reply>) -> FakeServer {
    FakeServer::start(vec![
        ("GET /models", vec![props("probe/openrouter_models.json")]),
        ("POST /chat/completions", chat),
    ])
    .await
}

fn openrouter_cfg(base: &str) -> ModelsConfig {
    models(json!({
        "endpoints": [
            {"id": "cloud", "backend": "openrouter", "base_url": base,
             "model": "qwen/qwen3.8-235b", "key_ref": "openrouter"}
        ]
    }))
}

async fn start_cloud(base: &str, name: &str, o: GatewayOptions) -> Gateway {
    started(
        openrouter_cfg(base),
        secrets(&[("openrouter", "sk-or-test-key")]),
        None,
        store(name),
        o,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn rate_limit_sets_group_cooldown() {
    let srv = openrouter_server(vec![Reply::JsonWithHeaders(
        429,
        vec![("retry-after".into(), "3".into())],
        json!({"error": {"message": "rate limited"}}),
    )])
    .await;
    let g = start_cloud(&srv.base_url, "rate_limit", opts_no_retry()).await;
    let err = g
        .stream(call_on("cloud", RunId::new(), "раз"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::RateLimited);
    let mut s = g
        .stream(call_on("cloud", RunId::new(), "два"))
        .await
        .unwrap();
    match s.next().await {
        Some(Ok(StreamEvent::Queued(st))) => match st.state {
            WaitState::Cooldown { remaining_ms } => assert!(remaining_ms > 2000, "{remaining_ms}"),
            other => panic!("ожидался Cooldown: {other:?}"),
        },
        other => panic!("ожидался Queued: {other:?}"),
    }
    tokio::time::sleep(ms(100)).await;
    assert_eq!(srv.requests("POST /chat/completions").len(), 1);
}

#[tokio::test]
async fn exhausted_limits_set_cooldown_after_success() {
    let body: Vec<u8> = fixture("chat/openrouter_cost.sse");
    let srv = openrouter_server(vec![Reply::Raw(
        200,
        vec![
            ("content-type".into(), "text/event-stream".into()),
            ("x-ratelimit-remaining-requests".into(), "0".into()),
            ("x-ratelimit-reset-requests".into(), "2s".into()),
        ],
        body,
    )])
    .await;
    let g = start_cloud(&srv.base_url, "exhausted_limits", opts()).await;
    g.stream(call_on("cloud", RunId::new(), "раз"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let mut s = g
        .stream(call_on("cloud", RunId::new(), "два"))
        .await
        .unwrap();
    match s.next().await {
        Some(Ok(StreamEvent::Queued(st))) => match st.state {
            WaitState::Cooldown { remaining_ms } => assert!(remaining_ms > 1000, "{remaining_ms}"),
            other => panic!("ожидался Cooldown: {other:?}"),
        },
        other => panic!("ожидался Queued: {other:?}"),
    }
}

#[tokio::test]
async fn breaker_opens_and_reset_closes() {
    breaker_opens_and_reset_closes_body("breaker").await;
}

/// То же на многопоточном runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn breaker_opens_and_reset_closes_mt() {
    breaker_opens_and_reset_closes_body("breaker_mt").await;
}

async fn breaker_opens_and_reset_closes_body(name: &str) {
    let mut chat = vec![Reply::Json(500, json!({"error": "сбой"})); 5];
    chat.push(llama_text());
    chat.push(Reply::Json(401, json!({"error": "ключ отвергнут"})));
    let srv = llama_server(vec![props("probe/llama_props.json")], chat).await;
    let g = start(llama_cfg(&srv.base_url, json!({})), name, opts_no_retry()).await;
    for i in 0..5 {
        let err = g
            .stream(call_on("local", RunId::new(), &format!("запрос {i}")))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_err();
        assert_eq!(err.class, ErrorClass::Server);
    }
    let err = g
        .stream(call_on("local", RunId::new(), "шестой"))
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::CircuitOpen);
    assert_eq!(srv.requests(CHAT).len(), 5);
    assert_eq!(view_of(&g, "local").state, "open");

    tokio::time::sleep(ms(60)).await;
    g.stream(call_on("local", RunId::new(), "проба"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(view_of(&g, "local").state, "ready");

    let err = g
        .stream(call_on("local", RunId::new(), "ключ"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Auth);
    assert_eq!(view_of(&g, "local").state, "disabled");
    g.reset(&id("local"));
    assert_eq!(view_of(&g, "local").state, "ready");
}

#[tokio::test]
async fn same_request_after_malformed_is_refused() {
    let bad = Reply::sse(vec![b"data: {not json\n\n".to_vec()]);
    let srv = llama_server(
        vec![props("probe/llama_props.json")],
        vec![bad, llama_text()],
    )
    .await;
    let g = start(llama_cfg(&srv.base_url, json!({})), "malformed", opts()).await;
    let run = RunId::new();
    let err = g
        .stream(call_on("local", run, "Привет"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Malformed);
    assert_eq!(srv.requests(CHAT).len(), 1);
    let err = g.stream(call_on("local", run, "Привет")).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::RepeatedAfterParseError);
    assert_eq!(srv.requests(CHAT).len(), 1);
    // Изменённый запрос уходит.
    g.stream(call_on("local", run, "Привет, исправлено"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(srv.requests(CHAT).len(), 2);
    // После завершения run отпечатки забыты (ParseGuard::clear).
    g.finish_run(run);
    g.stream(call_on("local", run, "Привет"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(srv.requests(CHAT).len(), 3);
}

#[tokio::test]
async fn parse_error_reported_by_loop_blocks_repeat() {
    let srv = llama_server(vec![props("probe/llama_props.json")], vec![llama_text()]).await;
    let g = start(
        llama_cfg(&srv.base_url, json!({})),
        "loop_parse_error",
        opts(),
    )
    .await;
    let run = RunId::new();
    let call = call_on("local", run, "Привет");
    g.stream(call.clone())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(srv.requests(CHAT).len(), 1);
    g.record_parse_error(run, &call);
    let err = g.stream(call).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::RepeatedAfterParseError);
    assert_eq!(srv.requests(CHAT).len(), 1);
}

#[tokio::test]
async fn drop_stream_releases_slot_and_cancels_http() {
    drop_stream_releases_slot_and_cancels_http_body("drop_stream").await;
}

/// То же на многопоточном runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drop_stream_releases_slot_and_cancels_http_mt() {
    drop_stream_releases_slot_and_cancels_http_body("drop_stream_mt").await;
}

async fn drop_stream_releases_slot_and_cancels_http_body(name: &str) {
    let srv = llama_server(
        vec![props("probe/llama_props.json")],
        vec![hang_after_first(), llama_text()],
    )
    .await;
    let g = start(
        llama_cfg(&srv.base_url, json!({"max_parallel": 1})),
        name,
        opts_long_idle(),
    )
    .await;
    assert_eq!(g.view().queue.groups[0].max_parallel, 1);
    let mut s = g
        .stream(call_on("local", RunId::new(), "раз"))
        .await
        .unwrap();
    loop {
        match s.next().await {
            Some(Ok(StreamEvent::TextDelta(_))) => break,
            Some(Ok(_)) => continue,
            other => panic!("ожидался TextDelta: {other:?}"),
        }
    }
    drop(s);
    assert!(
        eventually(Duration::from_secs(2), || srv.disconnects(CHAT) == 1).await,
        "соединение не закрыто"
    );
    let evs = drain(
        g.stream(call_on("local", RunId::new(), "два"))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        evs.iter()
            .any(|e| matches!(e, Ok(StreamEvent::Started { slot: Some(_), .. }))),
        "{evs:?}"
    );
    assert!(matches!(evs.last(), Some(Ok(StreamEvent::Finished { .. }))));
}

#[tokio::test]
async fn cancel_run_drops_waiting_requests() {
    cancel_run_drops_waiting_requests_body("cancel_run").await;
}

/// То же на многопоточном runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_run_drops_waiting_requests_mt() {
    cancel_run_drops_waiting_requests_body("cancel_run_mt").await;
}

async fn cancel_run_drops_waiting_requests_body(name: &str) {
    let srv = llama_server(
        vec![props("probe/llama_props.json")],
        vec![hang_after_first()],
    )
    .await;
    let g = start(
        llama_cfg(&srv.base_url, json!({"max_parallel": 1})),
        name,
        opts_long_idle(),
    )
    .await;
    let mut holder = g
        .stream(call_on("local", RunId::new(), "держит"))
        .await
        .unwrap();
    loop {
        match holder.next().await {
            Some(Ok(StreamEvent::Started { .. })) => break,
            Some(Ok(_)) => continue,
            other => panic!("ожидался Started: {other:?}"),
        }
    }
    let run = RunId::new();
    let mut waiting = Vec::new();
    for i in 0..2 {
        let mut s = g
            .stream(call_on("local", run, &format!("ждёт {i}")))
            .await
            .unwrap();
        assert!(matches!(s.next().await, Some(Ok(StreamEvent::Queued(_)))));
        waiting.push(s);
    }
    assert_eq!(g.cancel_run(run), 2);
    for s in waiting {
        let err = s.collect().await.unwrap_err();
        assert_eq!(err.class, ErrorClass::Cancelled);
    }
    let q = g.view().queue;
    assert!(
        q.groups
            .iter()
            .all(|gr| gr.waiting.iter().all(|w| w.run_id != run))
    );
    // `Started` уходит до отправки запроса: на многопоточном runtime он доходит позже.
    assert!(eventually(Duration::from_secs(2), || srv.requests(CHAT).len() == 1).await);
    tokio::time::sleep(ms(50)).await;
    assert_eq!(srv.requests(CHAT).len(), 1);
    drop(holder);
}

#[tokio::test]
async fn cancel_run_stops_running_request() {
    let srv = llama_server(
        vec![props("probe/llama_props.json")],
        vec![hang_after_first()],
    )
    .await;
    let g = start(
        llama_cfg(&srv.base_url, json!({})),
        "cancel_running",
        opts_long_idle(),
    )
    .await;
    let run = RunId::new();
    let mut s = g.stream(call_on("local", run, "долго")).await.unwrap();
    loop {
        match s.next().await {
            Some(Ok(StreamEvent::TextDelta(_))) => break,
            Some(Ok(_)) => continue,
            other => panic!("ожидался TextDelta: {other:?}"),
        }
    }
    // Забранное место и слот освобождаются при отмене run.
    assert_eq!(g.cancel_run(run), 1);
    let err = s.collect().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Cancelled);
    assert!(
        eventually(Duration::from_secs(2), || srv.disconnects(CHAT) == 1
            && g.view().queue.groups[0].active == 0)
        .await
    );
}

#[tokio::test]
async fn reprobe_after_stream_cut_picks_new_bundle() {
    let ev = sse_events(&fixture("chat/llama_text.sse"));
    let cut = Reply::Sse {
        chunks: ev,
        delay: Duration::ZERO,
        cut_after: Some(2),
        then_hang: false,
    };
    let srv = llama_server(
        vec![
            props("probe/llama_props.json"),
            props("probe/llama_props_other_model.json"),
        ],
        vec![cut],
    )
    .await;
    let st = store("reprobe_bundle");
    let g = started(
        llama_cfg(&srv.base_url, json!({})),
        secrets(&[]),
        None,
        st.clone(),
        opts(),
    )
    .await
    .unwrap();
    assert_eq!(view_of(&g, "local").model.as_deref(), Some(MODEL_A));
    let err = g
        .stream(call_on("local", RunId::new(), "Привет"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::StreamCut);
    assert!(
        eventually(Duration::from_secs(2), || view_of(&g, "local")
            .model
            .as_deref()
            == Some(MODEL_B))
        .await,
        "профиль не обновлён повторным зондом"
    );
    // Хранилище пишется сразу после применения зонда.
    let stored_b = || {
        st.load_endpoint(&id("local"))
            .and_then(|f| st.load(&f.bundle_key))
            .is_some_and(|p| p.bundle.model == MODEL_B)
    };
    assert!(
        eventually(Duration::from_secs(2), stored_b).await,
        "профиль B не сохранён"
    );
    assert_eq!(srv.requests(PROPS).len(), 2);
}

#[tokio::test]
async fn unreachable_endpoint_gets_one_reprobe() {
    let down = || Reply::Json(503, json!({"error": "загрузка модели"}));
    let srv = llama_server(
        vec![down(), props("probe/llama_props.json")],
        vec![llama_text()],
    )
    .await;
    let g = start(
        llama_cfg(&srv.base_url, json!({})),
        "unreachable_ok",
        opts(),
    )
    .await;
    assert_eq!(view_of(&g, "local").state, "unreachable");
    g.stream(call_on("local", RunId::new(), "Привет"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(srv.requests(PROPS).len(), 2);

    let srv = llama_server(vec![down()], vec![llama_text()]).await;
    let g = start(
        llama_cfg(&srv.base_url, json!({})),
        "unreachable_down",
        opts(),
    )
    .await;
    // Стрим возвращается сразу, отказ — в стриме после повторного зонда (I4):
    // типизированная недоступность, а не `Transport`; предохранителю не считается.
    let err = g
        .stream(call_on("local", RunId::new(), "Привет"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Unreachable);
    assert_eq!(srv.requests(PROPS).len(), 2);
    assert_eq!(srv.requests(CHAT).len(), 0);
}

#[tokio::test]
async fn selection_order() {
    let srv = llama_server(vec![props("probe/llama_props.json")], vec![llama_text()]).await;
    let ep = |name: &str| json!({"id": name, "backend": "llama_cpp", "base_url": srv.base_url});
    let cfg = models(json!({
        "default": "d",
        "endpoints": [ep("d"), ep("w"), ep("j"), ep("r")],
        "roles": {"worker": "w"}
    }));
    let g = start(cfg.clone(), "selection", opts()).await;
    let sel = |run: Option<&str>, job: Option<&str>, role: Option<RunRole>| Selection {
        run_choice: run.map(id),
        job_choice: job.map(id),
        role,
    };
    assert_eq!(
        g.select(&sel(Some("r"), Some("j"), Some(RunRole::Worker))),
        Ok(id("r"))
    );
    assert_eq!(
        g.select(&sel(None, Some("j"), Some(RunRole::Worker))),
        Ok(id("j"))
    );
    assert_eq!(
        g.select(&sel(None, None, Some(RunRole::Worker))),
        Ok(id("w"))
    );
    assert_eq!(g.select(&sel(None, None, Some(RunRole::Top))), Ok(id("d")));
    assert_eq!(g.select(&sel(None, None, None)), Ok(id("d")));
    assert_eq!(
        g.select(&sel(Some("nope"), None, None)),
        Err(SelectError::Unknown(id("nope")))
    );

    let mut no_default = cfg.clone();
    no_default.default = None;
    let g = start(no_default, "selection_no_default", opts()).await;
    assert_eq!(
        g.select(&sel(None, None, None)),
        Err(SelectError::NoDefault)
    );

    let cloud = models(json!({
        "default": "c",
        "endpoints": [{"id": "c", "backend": "anthropic", "model": "claude-opus-5-5",
                       "key_ref": "anthropic"}]
    }));
    let r = started(cloud, secrets(&[]), None, store("selection_cloud"), opts()).await;
    assert!(matches!(r, Err(ConfigError::CloudDefault(_))));
}

/// SSE Anthropic: вход 1000, выход 2000 токенов, без кэша.
fn anthropic_sse() -> Vec<u8> {
    let events = [
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1000,"output_tokens":1}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Да."}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2000}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ];
    events
        .iter()
        .map(|(e, d)| format!("event: {e}\ndata: {d}\n\n"))
        .collect::<String>()
        .into_bytes()
}

#[tokio::test]
async fn usage_has_cost() {
    let with_cost = openrouter_server(vec![Reply::sse(sse_events(&fixture(
        "chat/openrouter_cost.sse",
    )))])
    .await;
    let no_cost_body = String::from_utf8(fixture("chat/openrouter_cost.sse"))
        .unwrap()
        .replace("\"cost\":0.00042,", "");
    let without_cost =
        openrouter_server(vec![Reply::sse(sse_events(no_cost_body.as_bytes()))]).await;
    let anthropic = FakeServer::start(vec![
        (
            "GET /models/claude-opus-5-5",
            vec![props("probe/anthropic_model.json")],
        ),
        ("POST /messages", vec![Reply::sse(vec![anthropic_sse()])]),
    ])
    .await;
    let cfg = models(json!({
        "endpoints": [
            {"id": "or", "backend": "openrouter", "base_url": with_cost.base_url,
             "model": "qwen/qwen3.8-235b", "key_ref": "openrouter"},
            {"id": "or2", "backend": "openrouter", "base_url": without_cost.base_url,
             "model": "qwen/qwen3.8-235b", "key_ref": "openrouter"},
            {"id": "an", "backend": "anthropic", "base_url": anthropic.base_url,
             "model": "claude-opus-5-5", "key_ref": "anthropic",
             "price": {"input_per_mtok": 3.0, "output_per_mtok": 15.0}}
        ]
    }));
    let g = started(
        cfg,
        secrets(&[
            ("openrouter", "sk-or-test-key"),
            ("anthropic", "sk-ant-test-key"),
        ]),
        None,
        store("usage_cost"),
        opts(),
    )
    .await
    .unwrap();
    let usage = |ep: &'static str| {
        let g = g.clone();
        async move {
            g.stream(call_on(ep, RunId::new(), "Привет"))
                .await
                .unwrap()
                .collect()
                .await
                .unwrap()
                .usage
        }
    };
    let u = usage("or").await;
    assert_eq!(u.cost_source, CostSource::Provider);
    assert_eq!(u.cost_usd, Some(0.00042));

    let u = usage("an").await;
    assert_eq!((u.input, u.output), (1000, 2000));
    assert_eq!(u.cost_source, CostSource::Computed);
    assert!(
        (u.cost_usd.unwrap() - 0.033).abs() < 1e-12,
        "{:?}",
        u.cost_usd
    );

    // Цены только в профиле (каталог OpenRouter): 20 × 3 + 5 × 15 за миллион.
    let u = usage("or2").await;
    assert_eq!(u.cost_source, CostSource::Computed);
    assert!(
        (u.cost_usd.unwrap() - 0.000135).abs() < 1e-12,
        "{:?}",
        u.cost_usd
    );
    // Расход учтён в группе облака.
    let spent: f64 = g
        .view()
        .queue
        .groups
        .iter()
        .map(|gr| gr.spent_today_usd)
        .sum();
    assert!(
        (spent - (0.00042 + 0.033 + 0.000135)).abs() < 1e-9,
        "{spent}"
    );
}

#[tokio::test]
async fn context_tokens_from_profile() {
    let srv = llama_server(vec![props("probe/llama_props.json")], vec![llama_text()]).await;
    let g = start(
        llama_cfg(&srv.base_url, json!({})),
        "context_tokens",
        opts(),
    )
    .await;
    assert_eq!(g.context_tokens(&id("local")), Some(32768));
    assert!(g.capabilities(&id("local")).is_some());
    assert_eq!(g.context_tokens(&id("nope")), None);
    let v = view_of(&g, "local");
    assert_eq!((v.context_tokens, v.slots), (Some(32768), Some(4)));
}

#[tokio::test]
async fn view_serializes() {
    let srv = llama_server(vec![props("probe/llama_props.json")], vec![llama_text()]).await;
    let g = start(llama_cfg(&srv.base_url, json!({})), "view", opts()).await;
    let v = serde_json::to_value(g.view()).unwrap();
    for key in ["resources", "queue", "endpoints", "warnings"] {
        assert!(v.get(key).is_some(), "нет ключа {key}: {v}");
    }
    assert!(v["queue"]["groups"].is_array());
    assert_eq!(v["endpoints"][0]["state"], "ready");
    assert_eq!(v["endpoints"][0]["backend"], "llama_cpp");
}

#[tokio::test]
async fn slot_save_unsupported_is_recorded() {
    // Маршрута /slots нет (404, как без `--slot-save-path` у старых сборок): restore
    // новой сессии «не поддержан» → `slot_save = Some(false)` в фактах эндпоинта (D29).
    let srv = llama_server(vec![props("probe/llama_props.json")], vec![llama_text()]).await;
    let st = store("slot_save");
    let g = started(
        llama_cfg(&srv.base_url, json!({})),
        secrets(&[]),
        None,
        st.clone(),
        opts(),
    )
    .await
    .unwrap();
    let mut c = call_on("local", RunId::new(), "Привет");
    c.meta.session = Some("sess-1".into());
    let before = srv.total_requests();
    g.stream(c.clone()).await.unwrap().collect().await.unwrap();
    // restore (404) и сам запрос.
    assert_eq!(srv.total_requests() - before, 2);
    assert_eq!(srv.requests(CHAT).len(), 1);
    assert!(
        eventually(Duration::from_secs(2), || st
            .load_endpoint(&id("local"))
            .is_some_and(|f| f.slot_save == Some(false)))
        .await,
        "slot_save не записан"
    );
    // Второй запрос той же сессии идёт без save/restore и проходит.
    let before = srv.total_requests();
    g.stream(c).await.unwrap().collect().await.unwrap();
    assert_eq!(srv.total_requests() - before, 1);
}

#[tokio::test]
async fn forced_tool_choice_rejection_updates_profile() {
    // D15: 400 с упоминанием tool_choice на `Required` → профиль без
    // `tool_choice_required`; тот же выбор дальше отвергается до отправки.
    let srv = llama_server(
        vec![props("probe/llama_props.json")],
        vec![Reply::Json(
            400,
            json!({"error": {"message": "tool_choice=required is not supported"}}),
        )],
    )
    .await;
    let g = start(llama_cfg(&srv.base_url, json!({})), "tool_choice", opts()).await;
    assert!(g.capabilities(&id("local")).unwrap().tool_choice_required);
    let mut c = call_on("local", RunId::new(), "Прочитай");
    c.request.tools = vec![za_llm::ToolSchema {
        name: "read_file".into(),
        description: "Прочитать файл.".into(),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    }];
    c.request.tool_choice = za_llm::ToolChoice::Required;
    let err = g
        .stream(c.clone())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::BadRequest);
    assert_eq!(srv.requests(CHAT).len(), 1);
    assert!(!g.capabilities(&id("local")).unwrap().tool_choice_required);
    c.request.messages[0].parts = vec![Part::Text("Прочитай ещё раз".into())];
    let err = g.stream(c).await.unwrap().collect().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::BadRequest);
    assert_eq!(srv.requests(CHAT).len(), 1, "запрос не должен уйти");
}

/// Слот из события `Started`.
fn started_slot(evs: &[Ev]) -> Option<u32> {
    evs.iter().find_map(|e| match e {
        Ok(StreamEvent::Started { slot, .. }) => *slot,
        _ => None,
    })
}

fn session_call(session: Option<&str>, text: &str) -> ModelCall {
    let mut c = call_on("local", RunId::new(), text);
    c.meta.session = session.map(str::to_owned);
    c
}

/// Ждёт `Started` и возвращает его слот; стрим остаётся открытым.
async fn until_started(s: &mut ModelStream) -> Option<u32> {
    loop {
        match s.next().await {
            Some(Ok(StreamEvent::Started { slot, .. })) => return slot,
            Some(Ok(_)) => continue,
            other => panic!("ожидался Started: {other:?}"),
        }
    }
}

#[tokio::test]
async fn start_returns_without_waiting_for_probes() {
    // I3: облако не отвечает вовсе, локальный сервер отвечает через 1,5 с.
    let cloud = FakeServer::start(vec![("GET /models", vec![Reply::Hang])]).await;
    let local = llama_server(
        vec![Reply::Delayed(
            ms(1500),
            Box::new(props("probe/llama_props.json")),
        )],
        vec![llama_text()],
    )
    .await;
    let cfg = models(json!({
        "default": "local",
        "endpoints": [
            {"id": "local", "backend": "llama_cpp", "base_url": local.base_url},
            {"id": "cloud", "backend": "openrouter", "base_url": cloud.base_url,
             "model": "qwen/qwen3.8-235b", "key_ref": "openrouter"}
        ]
    }));
    let mut o = opts();
    o.probe_deadline = ms(2500);
    let t0 = Instant::now();
    let g = Gateway::start(
        cfg,
        secrets(&[("openrouter", "sk-or-test-key")]),
        None,
        store("start_no_wait"),
        o,
    )
    .await
    .unwrap();
    assert!(
        t0.elapsed() < ms(1000),
        "start ждал зонды: {:?}",
        t0.elapsed()
    );
    assert_eq!(view_of(&g, "local").state, "probing");
    assert_eq!(view_of(&g, "cloud").state, "probing");
    // Заявка к зондируемому эндпоинту ждёт итога зонда и проходит.
    let evs = drain(
        g.stream(call_on("local", RunId::new(), "Привет"))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        matches!(evs.first(), Some(Ok(StreamEvent::Queued(_)))),
        "{evs:?}"
    );
    assert!(evs.iter().all(Result::is_ok), "{evs:?}");
    assert!(matches!(evs.last(), Some(Ok(StreamEvent::Finished { .. }))));
    assert_eq!(local.requests(PROPS).len(), 1);
    // Общий предел зонда облака → Unreachable.
    assert!(
        eventually(Duration::from_secs(5), || view_of(&g, "cloud").state
            == "unreachable")
        .await
    );
    assert!(g.wait_probed(ms(100)).await);
}

#[tokio::test]
async fn reprobe_without_changes_keeps_slot_map() {
    // I1: зонд без изменений не трогает карту «сессия → слот» и `slot_save`.
    let srv = llama_server(vec![props("probe/llama_props.json")], vec![llama_text()]).await;
    let st = store("slot_map");
    let g = started(
        llama_cfg(&srv.base_url, json!({})),
        secrets(&[]),
        None,
        st.clone(),
        opts(),
    )
    .await
    .unwrap();
    let slot_a =
        started_slot(&drain(g.stream(session_call(Some("a"), "раз")).await.unwrap()).await);
    let slot_b =
        started_slot(&drain(g.stream(session_call(Some("b"), "два")).await.unwrap()).await);
    assert!(slot_a.is_some() && slot_b.is_some() && slot_a != slot_b);
    assert!(
        eventually(Duration::from_secs(2), || st
            .load_endpoint(&id("local"))
            .is_some_and(|f| f.slot_save == Some(false)))
        .await
    );
    assert!(matches!(
        g.probe(&id("local")).await,
        EndpointState::Ready(_)
    ));
    assert_eq!(srv.requests(PROPS).len(), 2);
    assert_eq!(
        st.load_endpoint(&id("local")).and_then(|f| f.slot_save),
        Some(false),
        "slot_save потерян после зонда"
    );
    // Сессия b возвращается в свой слот; save/restore не пробуются (slot_save = false).
    let before = srv.total_requests();
    let slot_b2 =
        started_slot(&drain(g.stream(session_call(Some("b"), "три")).await.unwrap()).await);
    assert_eq!(slot_b2, slot_b);
    assert_eq!(srv.total_requests() - before, 1);
}

#[tokio::test]
async fn restart_resets_slots_without_double_occupancy() {
    // I1: сервер перезапущен с другими -c/-np → слоты сброшены, но слот идущего
    // запроса не выдаётся второму.
    let mut restarted = fixture_json("probe/llama_props.json");
    restarted["default_generation_settings"]["n_ctx"] = json!(16384);
    restarted["total_slots"] = json!(2);
    let srv = llama_server(
        vec![props("probe/llama_props.json"), Reply::Json(200, restarted)],
        vec![hang_after_first(), llama_text()],
    )
    .await;
    let g = start(
        llama_cfg(&srv.base_url, json!({})),
        "slot_restart",
        opts_long_idle(),
    )
    .await;
    let mut holder = g.stream(session_call(Some("a"), "держит")).await.unwrap();
    let held = until_started(&mut holder).await;
    assert!(held.is_some());
    assert!(matches!(
        g.probe(&id("local")).await,
        EndpointState::Ready(_)
    ));
    let v = view_of(&g, "local");
    assert_eq!((v.slots, v.context_tokens), (Some(2), Some(16384)));
    assert_eq!(g.view().queue.groups[0].max_parallel, 2);
    let evs = drain(g.stream(session_call(None, "второй")).await.unwrap()).await;
    let second = started_slot(&evs);
    assert!(second.is_some());
    assert_ne!(second, held, "слот идущего запроса выдан второму");
    assert!(matches!(evs.last(), Some(Ok(StreamEvent::Finished { .. }))));
    drop(holder);
}

#[tokio::test]
async fn concurrent_probes_apply_fresh() {
    // I2: зонды одного эндпоинта идут по одному; второй, запрошенный во время первого,
    // выполняется после него, применяется свежий итог.
    let b = "probe/llama_props_other_model.json";
    let srv = llama_server(
        vec![
            props("probe/llama_props.json"),
            props("probe/llama_props.json"),
            props(b),
        ],
        vec![llama_text()],
    )
    .await;
    let g = start(llama_cfg(&srv.base_url, json!({})), "probe_race", opts()).await;
    let local = id("local");
    let (_first, second) = tokio::join!(g.probe(&local), g.probe(&local));
    match second {
        EndpointState::Ready(p) => assert_eq!(p.profile.bundle.model, MODEL_B),
        other => panic!("ожидался Ready: {other:?}"),
    }
    assert_eq!(view_of(&g, "local").model.as_deref(), Some(MODEL_B));
    assert_eq!(srv.requests(PROPS).len(), 3);
}

#[tokio::test]
async fn waiting_requests_after_unreachable_probe_fail_without_breaker() {
    // I4/I5: пока заявки ждут места, зонд нашёл эндпоинт недоступным. Каждая получает
    // `Unreachable` без своего зонда (зонд после постановки уже ответил) и без запроса
    // модели; предохранитель (порог 5) не открывается — следующий запрос после
    // восстановления проходит.
    let down = || Reply::Json(503, json!({"error": "загрузка модели"}));
    let mut props_replies = vec![props("probe/llama_props.json")];
    props_replies.push(down());
    props_replies.push(props("probe/llama_props.json"));
    let srv = llama_server(props_replies, vec![hang_after_first(), llama_text()]).await;
    let g = start(
        llama_cfg(&srv.base_url, json!({"max_parallel": 1})),
        "unreachable_waiting",
        opts_long_idle(),
    )
    .await;
    let mut holder = g
        .stream(call_on("local", RunId::new(), "держит"))
        .await
        .unwrap();
    until_started(&mut holder).await;
    let mut waiting = Vec::new();
    for i in 0..5 {
        let mut s = g
            .stream(call_on("local", RunId::new(), &format!("ждёт {i}")))
            .await
            .unwrap();
        assert!(matches!(s.next().await, Some(Ok(StreamEvent::Queued(_)))));
        waiting.push(s);
    }
    assert!(matches!(
        g.probe(&id("local")).await,
        EndpointState::Unreachable { .. }
    ));
    drop(holder);
    for s in waiting {
        let err = s.collect().await.unwrap_err();
        assert_eq!(err.class, ErrorClass::Unreachable, "{err:?}");
        assert!(err.retry_after.is_some());
    }
    assert_eq!(srv.requests(CHAT).len(), 1);
    assert_eq!(srv.requests(PROPS).len(), 2, "ждущие не зондировали");
    g.stream(call_on("local", RunId::new(), "после"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(srv.requests(CHAT).len(), 2);
    assert_eq!(srv.requests(PROPS).len(), 3);
}

#[tokio::test]
async fn slot_actions_use_endpoint_key() {
    // Финальное ревью I1: сервер с `--api-key` требует ключ на всех маршрутах; restore
    // сессии идёт с тем же `Authorization`, что зонд и запрос, сохранение не отключается.
    let ok = Reply::Json(200, json!({"id_slot": 0, "n_restored": 10}));
    let srv = FakeServer::start_with_bearer(
        vec![
            (PROPS, vec![props("probe/llama_props.json")]),
            (CHAT, vec![llama_text()]),
            ("POST /slots/0", vec![ok]),
        ],
        "k-llama",
    )
    .await;
    let st = store("slot_key");
    let g = started(
        llama_cfg(&srv.base_url, json!({"key_ref": "llama"})),
        secrets(&[("llama", "k-llama")]),
        None,
        st.clone(),
        opts(),
    )
    .await
    .unwrap();
    let mut c = call_on("local", RunId::new(), "Привет");
    c.meta.session = Some("sess-1".into());
    g.stream(c).await.unwrap().collect().await.unwrap();
    let reqs = srv.requests("POST /slots/0");
    assert_eq!(reqs.len(), 1, "restore не дошёл до сервера");
    assert_eq!(reqs[0].header("authorization"), Some("Bearer k-llama"));
    assert_eq!(srv.unauthorized(), 0);
    assert_ne!(
        st.load_endpoint(&id("local")).and_then(|f| f.slot_save),
        Some(false)
    );
}

#[tokio::test]
async fn slot_auth_failure_disables_slot_save() {
    // Ruling I1: 401/403 на `/slots` → `slot_save = Some(false)`, дальше без save/restore.
    let denied = Reply::Json(
        401,
        json!({"error": {"code": 401, "message": "Invalid API Key"}}),
    );
    let srv = FakeServer::start(vec![
        (PROPS, vec![props("probe/llama_props.json")]),
        (CHAT, vec![llama_text()]),
        ("POST /slots/0", vec![denied]),
    ])
    .await;
    let st = store("slot_auth");
    let g = started(
        llama_cfg(&srv.base_url, json!({})),
        secrets(&[]),
        None,
        st.clone(),
        opts(),
    )
    .await
    .unwrap();
    let mut c = call_on("local", RunId::new(), "Привет");
    c.meta.session = Some("sess-1".into());
    g.stream(c.clone()).await.unwrap().collect().await.unwrap();
    assert!(
        eventually(Duration::from_secs(2), || st
            .load_endpoint(&id("local"))
            .is_some_and(|f| f.slot_save == Some(false)))
        .await,
        "slot_save не выключен после 401"
    );
    let before = srv.total_requests();
    g.stream(c).await.unwrap().collect().await.unwrap();
    assert_eq!(srv.total_requests() - before, 1, "save/restore после 401");
}

/// Ключ для зонда, паника — при запросе (сбой кода внутри задачи запроса).
struct PanicOnSecondGet(std::sync::atomic::AtomicUsize);

impl SecretSource for PanicOnSecondGet {
    fn get(&self, _key_ref: &str) -> Option<Secret> {
        if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1 {
            panic!("ожидаемая паника теста: источник ключей");
        }
        Some(Secret::new("k".into()))
    }
}

#[tokio::test]
async fn panic_in_request_task_is_internal_error() {
    // M1: паника задачи запроса — `Internal` потребителю, место освобождено.
    let srv = llama_server(vec![props("probe/llama_props.json")], vec![llama_text()]).await;
    let g = started(
        llama_cfg(&srv.base_url, json!({"key_ref": "k"})),
        Arc::new(PanicOnSecondGet(Default::default())),
        None,
        store("panic_task"),
        opts(),
    )
    .await
    .unwrap();
    let err = g
        .stream(call_on("local", RunId::new(), "Привет"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Internal);
    assert!(
        eventually(ms(500), || g.view().queue.groups[0].active == 0).await,
        "место не освобождено"
    );
    assert_eq!(srv.requests(CHAT).len(), 0);
}

#[tokio::test]
async fn waiting_requests_share_one_reprobe_with_rate_limit() {
    // I5: эндпоинт недоступен; пять заявок → ровно один повторный зонд на всех, каждая —
    // `Unreachable` с `retry_after`. Следующая заявка в пределах интервала — без зонда.
    let down = || Reply::Json(503, json!({"error": "загрузка модели"}));
    let srv = llama_server(vec![down()], vec![llama_text()]).await;
    let g = start(llama_cfg(&srv.base_url, json!({})), "one_reprobe", opts()).await;
    assert_eq!(view_of(&g, "local").state, "unreachable");
    assert_eq!(srv.requests(PROPS).len(), 1);
    let mut streams = Vec::new();
    for i in 0..5 {
        streams.push(
            g.stream(call_on("local", RunId::new(), &format!("заявка {i}")))
                .await
                .unwrap(),
        );
    }
    for s in streams {
        let err = s.collect().await.unwrap_err();
        assert_eq!(err.class, ErrorClass::Unreachable, "{err:?}");
        let after = err.retry_after.expect("retry_after");
        assert!(
            after > Duration::ZERO && after <= DEFAULT_REPROBE_MIN,
            "{after:?}"
        );
    }
    assert_eq!(srv.requests(PROPS).len(), 2, "повторный зонд не один");
    // Интервал не истёк: без зонда, `retry_after` до следующего разрешённого.
    let err = g
        .stream(call_on("local", RunId::new(), "ещё"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Unreachable);
    assert!(err.retry_after.is_some_and(|d| d > Duration::ZERO));
    assert_eq!(srv.requests(PROPS).len(), 2);
    assert_eq!(srv.requests(CHAT).len(), 0);
}

#[tokio::test]
async fn reprobe_interval_doubles_and_resets_on_success() {
    // D40: интервал повторного зонда по заявкам удваивается и сбрасывается успехом.
    let down = || Reply::Json(503, json!({"error": "загрузка модели"}));
    let srv = llama_server(
        vec![down(), down(), down(), props("probe/llama_props.json")],
        vec![llama_text()],
    )
    .await;
    let mut o = opts();
    o.reprobe_min = ms(100);
    o.reprobe_max = ms(250);
    let g = start(llama_cfg(&srv.base_url, json!({})), "reprobe_backoff", o).await;
    let fail = |g: &Gateway| {
        let g = g.clone();
        async move {
            g.stream(call_on("local", RunId::new(), "x"))
                .await
                .unwrap()
                .collect()
                .await
                .unwrap_err()
        }
    };
    let e1 = fail(&g).await; // зонд №2, интервал 100 мс
    assert!(e1.retry_after.unwrap() <= ms(100));
    tokio::time::sleep(ms(120)).await;
    let e2 = fail(&g).await; // зонд №3, интервал 200 мс
    assert!(e2.retry_after.unwrap() > ms(100), "{:?}", e2.retry_after);
    assert_eq!(srv.requests(PROPS).len(), 3);
    tokio::time::sleep(ms(220)).await;
    // Зонд №4 успешен: запрос проходит, интервал сброшен.
    g.stream(call_on("local", RunId::new(), "ok"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(srv.requests(PROPS).len(), 4);
}

/// Стрим до конца с моментом события `Started`.
async fn drain_timed(s: ModelStream) -> (Option<Instant>, Vec<Ev>) {
    let mut s = s;
    let mut started = None;
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(ev) = s.next().await {
            if started.is_none() && matches!(ev, Ok(StreamEvent::Started { .. })) {
                started = Some(Instant::now());
            }
            out.push(ev);
        }
    })
    .await
    .expect("стрим не закрылся за 10 с");
    (started, out)
}

#[tokio::test]
async fn probe_wait_keeps_priority_and_aging() {
    // I6: эндпоинт с одним местом ещё зондируется; первым поставлен Job, затем
    // Interactive. Обе стоят в очереди приостановленными (видны, со старением), после
    // зонда первым место получает Interactive.
    let chunks = sse_events(&fixture("chat/llama_text.sse"));
    let slow = Reply::Sse {
        chunks,
        delay: ms(50),
        cut_after: None,
        then_hang: false,
    };
    let srv = llama_server(
        vec![Reply::Delayed(
            ms(600),
            Box::new(props("probe/llama_props.json")),
        )],
        vec![slow],
    )
    .await;
    let g = Gateway::start(
        llama_cfg(&srv.base_url, json!({"max_parallel": 1})),
        secrets(&[]),
        None,
        store("probe_priority"),
        opts(),
    )
    .await
    .unwrap();
    let mut job_call = call_on("local", RunId::new(), "фон");
    job_call.meta.priority = Priority::Job;
    let job = g.stream(job_call).await.unwrap();
    tokio::time::sleep(ms(20)).await;
    let inter = g
        .stream(call_on("local", RunId::new(), "пользователь"))
        .await
        .unwrap();
    tokio::time::sleep(ms(100)).await;
    assert_eq!(view_of(&g, "local").state, "probing");
    let snap = g.view().queue;
    let waiting = &snap.groups[0].waiting;
    assert_eq!(waiting.len(), 2, "заявки не видны в очереди во время зонда");
    assert_eq!(snap.groups[0].active, 0, "место занято во время зонда");
    assert_eq!(waiting[0].priority, Priority::Interactive);
    let (job, inter) = tokio::join!(drain_timed(job), drain_timed(inter));
    let (job_started, job_evs) = job;
    let (inter_started, inter_evs) = inter;
    assert!(job_evs.iter().all(Result::is_ok), "{job_evs:?}");
    assert!(inter_evs.iter().all(Result::is_ok), "{inter_evs:?}");
    assert!(
        inter_started.unwrap() < job_started.unwrap(),
        "Job обогнал Interactive"
    );
    // Старение и время ожидания не сбросились: Job ждал и зонд, и запрос Interactive.
    let waited = job_evs
        .iter()
        .filter_map(|e| match e {
            Ok(StreamEvent::Queued(st)) => Some(st.waited_ms),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    assert!(waited >= 500, "waited_ms Job: {waited}");
}

#[tokio::test]
async fn reprobe_interval_counts_from_probe_end() {
    // Раунд 3: долгий зонд (300 мс) при интервале 100 мс — `retry_after` после него
    // около 100 мс, а не ноль (отсчёт от конца зонда, не от запуска).
    let slow_down = || {
        Reply::Delayed(
            ms(300),
            Box::new(Reply::Json(503, json!({"error": "загрузка модели"}))),
        )
    };
    let srv = llama_server(vec![slow_down()], vec![llama_text()]).await;
    let mut o = opts();
    o.reprobe_min = ms(100);
    o.reprobe_max = ms(400);
    let g = start(llama_cfg(&srv.base_url, json!({})), "reprobe_from_end", o).await;
    let err = g
        .stream(call_on("local", RunId::new(), "x"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Unreachable);
    let after = err.retry_after.unwrap();
    assert!(after > ms(50) && after <= ms(100), "{after:?}");
    assert_eq!(srv.requests(PROPS).len(), 2);
}
