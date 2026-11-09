//! Ход модели (задача 10): эффект `model` (D7), события журнала (D8), сигналы цикла
//! (D9, D26), отображение ошибок в состояние run (D11, D13), разбор ответа (D27).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use za_agent::{
    CallVerdict, LoopExit, RequestRefs, Signal, TurnError, exit_for_effect_error,
    exit_for_model_error, model_turn, report_parse_errors, review, signal_channel,
};
use za_core::{EffectError, EffectSource, EventBus, Journal, RunCtx};
use za_llm::normalize::CallSource;
use za_llm::{
    BackendKind, ChatRequest, EndpointId, ErrorClass, FinishReason, Message, ModelCall, ModelError,
    Part, QueueStatus, ReasoningMode, RequestMeta, Role, Sampling, ScriptedModel, StreamEvent,
    ToolCall, ToolChoice, ToolSchema, Usage, WaitState, default_caps,
};
use za_protocol::{BlobHash, EffectClass, EffectKey, EventKind, Priority, RunId, RunRole, Seq};
use za_store::{NewRun, ProjectStore, UsageTotals};

// ---- Общие помощники ----

/// Чистый каталог теста внутри `CARGO_TARGET_TMPDIR`.
fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("turn")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Хранилище, журнал с шиной и созданный в нём run.
async fn setup(name: &str) -> (ProjectStore, Journal, RunId) {
    let s = ProjectStore::open(&dir(name)).unwrap();
    let run = RunId::new();
    s.create_run(NewRun {
        id: run,
        project_id: "p".into(),
        parent_id: None,
        role: RunRole::Top,
        model_profile: None,
    })
    .await
    .unwrap();
    let j = Journal::new(s.clone(), EventBus::new(EventBus::DEFAULT_CAPACITY));
    (s, j, run)
}

fn model() -> ScriptedModel {
    ScriptedModel::new(default_caps(BackendKind::LlamaCpp))
}

fn call(run: RunId) -> ModelCall {
    ModelCall {
        endpoint: EndpointId("local".into()),
        meta: RequestMeta {
            run_id: run,
            priority: Priority::Top,
            session: None,
            max_wait: None,
        },
        request: ChatRequest {
            system: "s".into(),
            tools: vec![],
            messages: vec![text_msg(Role::User, "привет")],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            sampling: Sampling::default(),
            reasoning: ReasoningMode::Default,
            cache_key: None,
        },
    }
}

fn refs() -> RequestRefs {
    RequestRefs {
        frozen: BlobHash("frozen".into()),
        messages: vec![BlobHash("m1".into())],
        prefix: "prefix-hash".into(),
    }
}

fn text_msg(role: Role, t: &str) -> Message {
    Message {
        role,
        parts: vec![Part::Text(t.into())],
    }
}

fn finished(t: &str, finish: FinishReason, usage: Usage) -> Result<StreamEvent, ModelError> {
    Ok(StreamEvent::Finished {
        message: text_msg(Role::Assistant, t),
        finish,
        usage,
    })
}

fn delta(t: &str) -> Result<StreamEvent, ModelError> {
    Ok(StreamEvent::TextDelta(t.into()))
}

fn queued(position: u32) -> Result<StreamEvent, ModelError> {
    Ok(StreamEvent::Queued(QueueStatus {
        position,
        waited_ms: 0,
        state: WaitState::Queued,
    }))
}

/// События run без протокола эффектов (`effect.*`), по порядку.
async fn loop_events(s: &ProjectStore, run: RunId) -> Vec<(EventKind, Value)> {
    s.read_run(run, Seq(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| !matches!(e.kind, EventKind::EffectIntent | EventKind::EffectReceipt))
        .map(|e| (e.kind, e.payload))
        .collect()
}

/// Сообщение отказа в квитанции эффекта.
async fn receipt_error(s: &ProjectStore, run: RunId, key: &EffectKey) -> String {
    let row = s.effect(run, key).await.unwrap().unwrap();
    let env = row.result.expect("квитанция");
    assert_eq!(env["outcome"], "error", "{env}");
    env["message"].as_str().unwrap().to_owned()
}

// ---- Отображение ошибок (D11) ----

#[test]
fn error_mapping_table() {
    use ErrorClass::*;
    for c in [
        Transport,
        Timeout,
        RateLimited,
        Overloaded,
        Server,
        StreamCut,
        CircuitOpen,
        Unreachable,
        Auth,
        NotFound,
        BadRequest,
        Malformed,
        ContentFilter,
        Inactive,
    ] {
        assert_eq!(
            exit_for_model_error(&ModelError::new(c, "x"), None),
            LoopExit::Paused("provider"),
            "{c:?}"
        );
    }
    let q = |m: &str| ModelError::new(Queue, m);
    assert_eq!(
        exit_for_model_error(&q("wait_timeout"), None),
        LoopExit::Paused("queue")
    );
    let wr = WaitState::WaitingResource {
        kind: "vram".into(),
    };
    assert_eq!(
        exit_for_model_error(&q("wait_timeout"), Some(&wr)),
        LoopExit::Paused("resource")
    );
    assert_eq!(
        exit_for_model_error(&q("budget_exhausted"), None),
        LoopExit::Paused("budget")
    );
    assert_eq!(
        exit_for_model_error(&q("monitor_stopped"), None),
        LoopExit::Paused("resource")
    );
    assert_eq!(
        exit_for_model_error(&q("runtime_stopped"), None),
        LoopExit::Paused("internal")
    );
    assert_eq!(
        exit_for_model_error(&ModelError::new(Internal, "panic"), None),
        LoopExit::Paused("internal")
    );
    assert_eq!(
        exit_for_model_error(&ModelError::new(ContextOverflow, "x"), None),
        LoopExit::Paused("context")
    );
    // Прочие классы D11: RepeatedAfterParseError и Cancelled не по команде цикла.
    for c in [RepeatedAfterParseError, Cancelled] {
        assert_eq!(
            exit_for_model_error(&ModelError::new(c, "x"), None),
            LoopExit::Paused("internal"),
            "{c:?}"
        );
    }
}

#[test]
fn effect_errors_map_like_r0() {
    let mm = EffectError::ReplayMismatch {
        key: "r:1:1".into(),
        field: "args",
    };
    assert_eq!(
        exit_for_effect_error(&mm),
        Some(LoopExit::Paused("replay_mismatch"))
    );
    assert_eq!(
        exit_for_effect_error(&EffectError::Unknown {
            interrupt_id: "unk-r:1:0".into()
        }),
        Some(LoopExit::Suspended {
            reason: "needs_decision",
            interrupt: "unk-r:1:0".into()
        })
    );
    assert_eq!(
        exit_for_effect_error(&EffectError::Failed("x".into())),
        None
    );
    assert_eq!(exit_for_effect_error(&EffectError::Poisoned), None);
}

// ---- Сигналы (D9, D26) ----

#[tokio::test]
async fn signals_keep_last_flag_and_priority() {
    let (tx, mut rx) = signal_channel();
    tx.input();
    tx.input();
    tx.pause();
    tx.pause(); // повтор не теряется и не дублируется
    assert_eq!(rx.recv().await, Signal::Pause);
    assert_eq!(rx.recv().await, Signal::Input);
    tx.cancel();
    assert_eq!(rx.drain(), Some(Signal::Cancel));
    assert_eq!(rx.drain(), None);
    tx.input();
    assert_eq!(rx.drain(), None); // Input на границе выбрасывается
    assert!(
        tokio::time::timeout(Duration::from_millis(20), rx.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn signals_wait_forever_when_senders_dropped() {
    let (tx, mut rx) = signal_channel();
    let flags = rx.flags();
    drop(tx);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), rx.recv())
            .await
            .is_err()
    );
    assert_eq!(*flags.borrow(), za_agent::ControlFlags::default());
}

// ---- Эффект model и события (D7, D8) ----

#[tokio::test]
async fn turn_journals_request_deltas_usage() {
    let (s, j, run) = setup("journal").await;
    let m = model();
    m.push(vec![
        queued(2),
        queued(1),
        Ok(StreamEvent::Started {
            endpoint: EndpointId("local".into()),
            slot: None,
            attempt: 1,
        }),
        delta("при"),
        delta("вет"),
        delta(" мир"),
        finished(
            "привет мир",
            FinishReason::Stop,
            Usage {
                input: 50,
                output: 3,
                ..Usage::default()
            },
        ),
    ]);
    let mut ctx = RunCtx::new(s.clone(), run);
    ctx.next_step();
    let key = EffectKey::derive(run, 1, 0);
    let (_tx, mut sig) = signal_channel();
    let out = model_turn(&mut ctx, &j, &m, call(run), &refs(), 1, &mut sig)
        .await
        .unwrap();
    assert_eq!(out.message, text_msg(Role::Assistant, "привет мир"));
    assert_eq!(out.finish, FinishReason::Stop);
    assert!(out.service.is_empty());

    let ev = loop_events(&s, run).await;
    let kinds: Vec<&str> = ev.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "run.created",
            "model.request",
            "agent.heartbeat",
            "agent.heartbeat",
            "model.delta",
            "model.usage"
        ]
    );
    let req = &ev[1].1;
    assert_eq!(req["turn"], 1);
    assert_eq!(req["endpoint"], "local");
    assert_eq!(req["effect_key"], key.0.as_str());
    assert_eq!(
        req["request"],
        za_agent::request_hash(&call(run).request).unwrap()
    );
    assert_eq!(req["prefix"], "prefix-hash");
    assert_eq!(req["frozen"], "frozen");
    assert_eq!(req["messages"], json!(["m1"]));
    assert_eq!(req["prompt_version"], za_agent::PROMPT_VERSION);
    assert_eq!(ev[2].1["state"], "queued");
    assert_eq!(ev[2].1["position"], 2);
    assert_eq!(ev[2].1["turn"], 1);
    // Позиция 1 пришла быстрее секунды после 2 — без события; смена состояния — сразу.
    assert_eq!(ev[3].1["state"], "generating");
    assert_eq!(
        ev[4].1,
        json!({"turn": 1, "attempt": 1, "text": "привет мир"})
    );
    let usage = &ev[5].1;
    assert_eq!(usage["usage"]["input"], 50);
    assert_eq!(usage["effect_key"], key.0.as_str());
    assert_eq!(usage["endpoint"], "local");
    assert_eq!(usage["finish"], "stop");
    assert_eq!(usage["turn"], 1);

    assert_eq!(
        s.usage_totals(run).await.unwrap(),
        UsageTotals {
            calls: 1,
            input: 50,
            output: 3,
            ..UsageTotals::default()
        }
    );
    // Квитанция — успех с итогом хода.
    let row = s.effect(run, &key).await.unwrap().unwrap();
    assert_eq!(row.name, "model");
    assert_eq!(row.class, EffectClass::Idempotent);
    assert_eq!(row.args["prefix"], "prefix-hash");
    assert_eq!(row.result.unwrap()["outcome"], "ok");
}

#[tokio::test]
async fn restart_drops_partial_and_journals_restart() {
    let (s, j, run) = setup("restart").await;
    let m = model();
    m.push(vec![
        delta("обры"),
        Ok(StreamEvent::ServiceMessage(text_msg(
            Role::System,
            "старое",
        ))),
        Ok(StreamEvent::Restart {
            attempt: 2,
            reason: "StreamCut: x".into(),
        }),
        delta("целый"),
        finished("целый", FinishReason::Stop, Usage::default()),
    ]);
    let mut ctx = RunCtx::new(s.clone(), run);
    ctx.next_step();
    let (_tx, mut sig) = signal_channel();
    let out = model_turn(&mut ctx, &j, &m, call(run), &refs(), 1, &mut sig)
        .await
        .unwrap();
    assert_eq!(out.message, text_msg(Role::Assistant, "целый"));
    // Служебное сообщение прежней попытки сброшено вместе с ней.
    assert!(out.service.is_empty());
    let deltas: Vec<Value> = loop_events(&s, run)
        .await
        .into_iter()
        .filter(|(k, _)| *k == EventKind::ModelDelta)
        .map(|(_, p)| p)
        .collect();
    assert_eq!(
        deltas,
        [
            json!({"turn": 1, "attempt": 1, "text": "обры"}),
            json!({"turn": 1, "attempt": 1, "restart": "StreamCut: x"}),
            json!({"turn": 1, "attempt": 2, "text": "целый"}),
        ]
    );
}

#[tokio::test]
async fn service_message_returned_with_turn() {
    let (s, j, run) = setup("service").await;
    let m = model();
    m.push(vec![
        Ok(StreamEvent::ServiceMessage(text_msg(Role::System, "x"))),
        finished("a", FinishReason::Stop, Usage::default()),
    ]);
    let mut ctx = RunCtx::new(s.clone(), run);
    ctx.next_step();
    let (_tx, mut sig) = signal_channel();
    let out = model_turn(&mut ctx, &j, &m, call(run), &refs(), 1, &mut sig)
        .await
        .unwrap();
    assert_eq!(out.service, vec![text_msg(Role::System, "x")]);
    assert_eq!(out.message, text_msg(Role::Assistant, "a"));
}

#[tokio::test]
async fn stream_error_after_deltas_is_model_error_without_commit() {
    let (s, j, run) = setup("stream-error").await;
    let m = model();
    m.push(vec![
        delta("часть"),
        Err(ModelError::new(ErrorClass::Server, "500")),
    ]);
    let mut ctx = RunCtx::new(s.clone(), run);
    ctx.next_step();
    let key = EffectKey::derive(run, 1, 0);
    let (_tx, mut sig) = signal_channel();
    let err = model_turn(&mut ctx, &j, &m, call(run), &refs(), 1, &mut sig)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, TurnError::Model { error, last_wait: None } if error.class == ErrorClass::Server),
        "{err:?}"
    );
    assert!(!ctx.is_poisoned());
    let deltas: Vec<Value> = loop_events(&s, run)
        .await
        .into_iter()
        .filter(|(k, _)| *k == EventKind::ModelDelta)
        .map(|(_, p)| p)
        .collect();
    assert_eq!(deltas, [json!({"turn": 1, "attempt": 1, "text": "часть"})]);
    assert!(
        receipt_error(&s, run, &key)
            .await
            .starts_with("model_error:")
    );
    // Нет `model.usage`: ход не завершён.
    assert_eq!(s.usage_totals(run).await.unwrap().calls, 0);

    // Реплей: тот же отказ из журнала, модель не вызывается.
    let mut ctx = RunCtx::resume_at(s.clone(), run, 1, 0);
    let again = model_turn(&mut ctx, &j, &m, call(run), &refs(), 1, &mut sig)
        .await
        .unwrap_err();
    assert!(
        matches!(&again, TurnError::Model { error, .. } if error.class == ErrorClass::Server && error.message == "500"),
        "{again:?}"
    );
    assert_eq!(m.calls().len(), 1);
}

#[tokio::test]
async fn queue_error_keeps_last_wait() {
    let (s, j, run) = setup("queue-wait").await;
    let m = model();
    m.push(vec![
        Ok(StreamEvent::Queued(QueueStatus {
            position: 1,
            waited_ms: 10,
            state: WaitState::WaitingResource {
                kind: "vram".into(),
            },
        })),
        Err(ModelError::new(ErrorClass::Queue, "wait_timeout")),
    ]);
    let mut ctx = RunCtx::new(s.clone(), run);
    ctx.next_step();
    let (_tx, mut sig) = signal_channel();
    let err = model_turn(&mut ctx, &j, &m, call(run), &refs(), 1, &mut sig)
        .await
        .unwrap_err();
    let TurnError::Model { error, last_wait } = &err else {
        panic!("{err:?}")
    };
    assert_eq!(
        exit_for_model_error(error, last_wait.as_ref()),
        LoopExit::Paused("resource")
    );
    let hb: Vec<Value> = loop_events(&s, run)
        .await
        .into_iter()
        .filter(|(k, _)| *k == EventKind::AgentHeartbeat)
        .map(|(_, p)| p)
        .collect();
    assert_eq!(
        hb,
        [
            json!({"turn": 1, "state": "waiting_resource", "position": 1, "waited_ms": 10, "kind": "vram"})
        ]
    );
    // Реплей восстанавливает и последний WaitState.
    let mut ctx = RunCtx::resume_at(s.clone(), run, 1, 0);
    let again = model_turn(&mut ctx, &j, &m, call(run), &refs(), 1, &mut sig)
        .await
        .unwrap_err();
    assert!(
        matches!(&again, TurnError::Model { last_wait: Some(WaitState::WaitingResource { kind }), .. } if kind == "vram"),
        "{again:?}"
    );
}

#[tokio::test]
async fn refusal_is_reported_not_committed() {
    let (s, j, run) = setup("refusal").await;
    let m = model();
    m.push(vec![
        delta("частич"),
        finished("частич", FinishReason::Refusal, Usage::default()),
    ]);
    let mut ctx = RunCtx::new(s.clone(), run);
    ctx.next_step();
    let (_tx, mut sig) = signal_channel();
    let out = model_turn(&mut ctx, &j, &m, call(run), &refs(), 1, &mut sig)
        .await
        .unwrap();
    assert_eq!(out.finish, FinishReason::Refusal);
    assert_eq!(out.message, text_msg(Role::Assistant, "частич"));
}

#[tokio::test]
async fn abort_drops_stream_fast() {
    let (s, j, run) = setup("abort").await;
    let m = model();
    m.push_open(vec![delta("a")]);
    let mut ctx = RunCtx::new(s.clone(), run);
    ctx.next_step();
    let key = EffectKey::derive(run, 1, 0);
    let (tx, mut sig) = signal_channel();
    let pauser = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let at = tokio::time::Instant::now();
        tx.pause();
        (tx, at)
    });
    let err = model_turn(&mut ctx, &j, &m, call(run), &refs(), 1, &mut sig)
        .await
        .unwrap_err();
    let done = tokio::time::Instant::now();
    let (_tx, at) = pauser.await.unwrap();
    assert!(
        matches!(
            err,
            TurnError::Aborted {
                signal: Signal::Pause,
                source: EffectSource::Live
            }
        ),
        "{err:?}"
    );
    assert!(done - at <= Duration::from_millis(200), "{:?}", done - at);
    assert_eq!(m.open_streams(), 0);
    assert_eq!(receipt_error(&s, run, &key).await, "interrupted:pause");

    // Реплей: квитанция обрыва из журнала, стрим не открывается.
    let mut ctx = RunCtx::resume_at(s.clone(), run, 1, 0);
    let (_tx2, mut sig2) = signal_channel();
    let again = model_turn(&mut ctx, &j, &m, call(run), &refs(), 1, &mut sig2)
        .await
        .unwrap_err();
    assert!(
        matches!(
            again,
            TurnError::Aborted {
                signal: Signal::Pause,
                source: EffectSource::Journal
            }
        ),
        "{again:?}"
    );
    assert_eq!(m.calls().len(), 1);
}

#[tokio::test]
async fn changed_request_is_replay_mismatch() {
    let (s, j, run) = setup("mismatch").await;
    let m = model();
    m.push(vec![finished("a", FinishReason::Stop, Usage::default())]);
    let mut ctx = RunCtx::new(s.clone(), run);
    ctx.next_step();
    let (_tx, mut sig) = signal_channel();
    model_turn(&mut ctx, &j, &m, call(run), &refs(), 1, &mut sig)
        .await
        .unwrap();
    let mut other = call(run);
    other.request.system = "другой".into();
    let mut ctx = RunCtx::resume_at(s.clone(), run, 1, 0);
    let err = model_turn(&mut ctx, &j, &m, other, &refs(), 1, &mut sig)
        .await
        .unwrap_err();
    let TurnError::Effect(e) = &err else {
        panic!("{err:?}")
    };
    assert_eq!(
        exit_for_effect_error(e),
        Some(LoopExit::Paused("replay_mismatch"))
    );
    assert!(ctx.is_poisoned());
}

// ---- Разбор ответа (D27) ----

fn schema(name: &str, props: Value, required: &[&str]) -> ToolSchema {
    ToolSchema {
        name: name.into(),
        description: format!("tool {name}"),
        parameters: json!({"type": "object", "properties": props, "required": required}),
    }
}

fn tools() -> Vec<ToolSchema> {
    vec![
        schema("read_file", json!({"path": {"type": "string"}}), &["path"]),
        schema(
            "write_file",
            json!({"path": {"type": "string"}, "content": {"type": "string"}}),
            &["path", "content"],
        ),
    ]
}

fn classes() -> BTreeMap<String, EffectClass> {
    BTreeMap::from([
        ("read_file".to_owned(), EffectClass::ReadOnly),
        ("write_file".to_owned(), EffectClass::NonIdempotent),
        ("write".to_owned(), EffectClass::NonIdempotent),
    ])
}

fn output(parts: Vec<Part>, finish: FinishReason) -> za_agent::TurnOutput {
    za_agent::TurnOutput {
        service: vec![],
        message: Message {
            role: Role::Assistant,
            parts,
        },
        finish,
        usage: Usage::default(),
        ttft_ms: None,
        duration_ms: 0,
    }
}

fn native(id: &str, name: &str, args: Value, raw: Option<&str>) -> Part {
    Part::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        raw_arguments: raw.map(Into::into),
    })
}

fn call_ids(m: &Message) -> Vec<String> {
    m.parts
        .iter()
        .filter_map(|p| match p {
            Part::ToolCall(c) => Some(c.id.clone()),
            _ => None,
        })
        .collect()
}

fn texts(m: &Message) -> Vec<String> {
    m.parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

fn reject_text(v: &CallVerdict) -> &str {
    match v {
        CallVerdict::Reject(s) => s,
        CallVerdict::Run => panic!("ожидался отказ"),
    }
}

#[test]
fn review_blocks_extracted_mutation_and_records_ids() {
    let text = "Читаю.\n<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a\"}}\n</tool_call>\n\
                <tool_call>\n{\"name\": \"write_file\", \"arguments\": {\"path\": \"b\", \"content\": \"c\"}}\n</tool_call>";
    let out = output(vec![Part::Text(text.into())], FinishReason::Stop);
    let r = review(&out, &tools(), &classes(), 3);
    assert_eq!(r.calls.len(), 2);
    assert_eq!(r.calls[0].call.name, "read_file");
    assert_eq!(r.calls[0].call.id, "xc-3-0");
    assert_eq!(r.calls[0].source, CallSource::Content);
    assert_eq!(r.calls[0].verdict, CallVerdict::Run);
    assert_eq!(r.calls[1].call.name, "write_file");
    assert_eq!(r.calls[1].call.id, "xc-3-1");
    assert_eq!(
        r.calls[1].verdict,
        CallVerdict::Reject(
            "blocked: calls that change state must use the tool-calling interface; this call was not executed"
                .into()
        )
    );
    assert_eq!(
        r.nudges,
        vec![
            "The tool call in your text was not executed: calls that change state must use the tool-calling interface. Make the call again as a tool call."
                .to_owned()
        ]
    );
    assert_eq!(call_ids(&r.assistant), ["xc-3-0", "xc-3-1"]);
    assert_eq!(texts(&r.assistant), ["Читаю."]);
    assert_eq!(r.text, "Читаю.");
    assert_eq!(r.parse_errors, 0);
    assert_eq!(r.assistant.role, Role::Assistant);
}

#[test]
fn review_matches_rejected_natives_by_order() {
    let out = output(
        vec![
            native("c1", "read_file", json!({"path": "a"}), None),
            native("c2", "read_file", Value::Null, Some("{bad")),
            native("", "zzz", json!({}), None),
        ],
        FinishReason::ToolCalls,
    );
    let tools = vec![schema(
        "read_file",
        json!({"path": {"type": "string"}}),
        &["path"],
    )];
    let r = review(&out, &tools, &classes(), 3);
    assert_eq!(r.calls.len(), 3);
    assert_eq!(r.calls[0].call.id, "c1");
    assert_eq!(r.calls[0].verdict, CallVerdict::Run);
    assert_eq!(r.calls[1].call.id, "c2");
    assert!(
        reject_text(&r.calls[1].verdict).starts_with("could not parse tool call arguments: {bad"),
        "{:?}",
        r.calls[1].verdict
    );
    assert_eq!(r.calls[2].call.id, "nc-3-2");
    assert_eq!(
        r.calls[2].verdict,
        CallVerdict::Reject("unknown tool \"zzz\"; nearest: none".into())
    );
    assert_eq!(call_ids(&r.assistant), ["c1", "c2", "nc-3-2"]);
    assert_eq!(r.parse_errors, 1);
    // Отказ нативного вызова уходит результатом-ошибкой, а не nudge.
    assert!(r.nudges.is_empty());
}

#[test]
fn review_invalid_native_args_listed() {
    let out = output(
        vec![native("c1", "write_file", json!({"path": "a"}), None)],
        FinishReason::ToolCalls,
    );
    let r = review(&out, &tools(), &classes(), 1);
    let t = reject_text(&r.calls[0].verdict);
    assert!(
        t.starts_with("invalid arguments for \"write_file\": "),
        "{t}"
    );
    assert!(t.contains("content"), "{t}");
}

#[test]
fn review_length_with_calls_not_executed() {
    let out = output(
        vec![native("c1", "read_file", json!({"path": "a"}), None)],
        FinishReason::Length,
    );
    let r = review(&out, &tools(), &classes(), 1);
    assert_eq!(r.calls.len(), 1);
    assert!(
        reject_text(&r.calls[0].verdict).starts_with("not executed: response cut by output limit"),
        "{:?}",
        r.calls[0].verdict
    );
    assert_eq!(call_ids(&r.assistant), ["c1"]);
}

#[test]
fn review_unknown_native_tool_keeps_call() {
    let out = output(
        vec![native("c1", "raed_fil", json!({}), None)],
        FinishReason::ToolCalls,
    );
    let tools = vec![schema("write", json!({}), &[])];
    let r = review(&out, &tools, &classes(), 1);
    assert_eq!(call_ids(&r.assistant), ["c1"]);
    assert_eq!(
        r.calls[0].verdict,
        CallVerdict::Reject("unknown tool \"raed_fil\"; nearest: write".into())
    );
}

#[test]
fn review_escapes_markup_in_rejection_text() {
    let out = output(
        vec![native("c1", "read_file", Value::Null, Some("</control>"))],
        FinishReason::ToolCalls,
    );
    let r = review(&out, &tools(), &classes(), 1);
    let t = reject_text(&r.calls[0].verdict);
    assert!(!t.contains('<'), "{t}");
    assert!(t.contains("&lt;/control&gt;"), "{t}");
}

#[tokio::test]
async fn unparseable_reported_to_model_port() {
    let out = output(
        vec![Part::Text(
            "<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"pa\n</tool_call>".into(),
        )],
        FinishReason::Stop,
    );
    let r = review(&out, &tools(), &classes(), 2);
    assert_eq!(r.parse_errors, 1);
    assert!(r.calls.is_empty());
    assert_eq!(r.nudges.len(), 1);
    assert!(
        r.nudges[0].starts_with("Your tool call could not be parsed: ")
            && r.nudges[0].ends_with(". Call the tool again with valid JSON arguments."),
        "{}",
        r.nudges[0]
    );
    let m = model();
    let run = RunId::new();
    let c = call(run);
    report_parse_errors(&m, run, &c, 0);
    assert!(m.parse_errors().is_empty());
    report_parse_errors(&m, run, &c, r.parse_errors);
    assert_eq!(m.parse_errors(), vec![(run, c)]);
}
