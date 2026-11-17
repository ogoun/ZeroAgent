//! Драйвер цикла (задача 12): граница шага, ходы, завершение (D18), чистая пауза
//! (D5), снимки (D4), ошибки цикла (D11), управление (D26).

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use support::*;

use serde_json::{Value, json};
use za_agent::{
    ControlRx, Conversation, CrashPoint, EnvInfo, FaultHook, FaultPoint, HistoryError, LoopDeps,
    LoopError, LoopOutcome, Next, NoMemory, PendingExit, ResumeChange, RunSpec, Signal, Snapshot,
    builtin_tools, control_channel, resume_run, start_run,
};
use za_core::{
    Approval, EventBus, Journal, RiskClass, Tool, ToolCx, ToolKind, ToolOutput, ToolRegistry,
    ToolSpec,
};
use za_llm::{
    BackendKind, BoxFuture, EndpointId, ErrorClass, FinishReason, Message, ModelError, Part, Role,
    ScriptedModel, StreamEvent, ToolCall, ToolChoice, ToolSchema, Usage, default_caps,
};
use za_protocol::{
    Actor, Budget, EffectClass, EffectKey, Event, EventKind, NewEvent, Priority, RunId, RunMode,
    RunRole, RunStatus, Seq, Tier, pause_reason,
};
use za_store::{ProjectStore, StoreError, StoreOptions};

// ---- Помощники ----

/// Свой каталог теста: `CARGO_TARGET_TMPDIR/driver/<name>-<pid>-<n>`.
fn dir(name: &str) -> PathBuf {
    unique_dir("driver", name)
}

/// Инструмент чтения `echo`: результат — канонический JSON аргументов.
struct Echo {
    spec: ToolSpec,
}

impl Tool for Echo {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move { ToolOutput::ok(format!("echo {args}")) })
    }
}

/// Инструмент, который на вызове заполняет журнал до отказа (`StoreError::Full`).
struct Filler {
    spec: ToolSpec,
    store: ProjectStore,
}

impl Tool for Filler {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, _args: Value, cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let store = self.store.clone();
        Box::pin(async move {
            let ev = || NewEvent {
                run_id: cx.run,
                parent_seq: None,
                actor: Actor::Tool,
                kind: EventKind::Unknown("test.fill".into()),
                payload: json!({}),
                tainted: false,
            };
            // Пачками, пока влезает, затем по одному: место кончается и для
            // маленьких записей.
            while store
                .append_batch((0..200).map(|_| ev()).collect())
                .await
                .is_ok()
            {}
            while store.append(ev()).await.is_ok() {}
            ToolOutput::ok("filled")
        })
    }
}

fn tool_spec(name: &str, kind: ToolKind, class: EffectClass) -> ToolSpec {
    ToolSpec {
        schema: ToolSchema {
            name: name.into(),
            description: "t".into(),
            parameters: json!({"type": "object"}),
        },
        class,
        kind,
        approval: Approval::Mode,
        risk: match kind {
            ToolKind::Edit => RiskClass::LocalWrite,
            ToolKind::Exec => RiskClass::Exec,
            _ => RiskClass::Read,
        },
        roles: vec![RunRole::Top, RunRole::Worker, RunRole::Orchestrator],
        min_tier: Tier::Small,
        timeout: Duration::from_secs(60),
        output_limit: None,
        tainted: false,
    }
}

fn registry(extra: Vec<Arc<dyn Tool>>) -> ToolRegistry {
    let mut r = ToolRegistry::default();
    for t in builtin_tools() {
        r.register(t).unwrap();
    }
    r.register(Arc::new(Echo {
        spec: tool_spec("echo", ToolKind::Read, EffectClass::ReadOnly),
    }))
    .unwrap();
    r.register(Arc::new(Echo {
        spec: tool_spec("edit", ToolKind::Edit, EffectClass::Idempotent),
    }))
    .unwrap();
    for t in extra {
        r.register(t).unwrap();
    }
    r
}

struct Fx {
    store: ProjectStore,
    model: Arc<ScriptedModel>,
    deps: LoopDeps,
    run: RunId,
    ctl_rx: Option<ControlRx>,
    ctl: za_agent::RunControl,
    bus: EventBus,
}

fn fx_with(name: &str, model: ScriptedModel, opts: StoreOptions, extra: Vec<Arc<dyn Tool>>) -> Fx {
    let store = ProjectStore::open_with(&dir(name), opts).unwrap();
    fx_on(store, model, extra)
}

fn fx_on(store: ProjectStore, model: ScriptedModel, extra: Vec<Arc<dyn Tool>>) -> Fx {
    let model = Arc::new(model);
    let bus = EventBus::new(EventBus::DEFAULT_CAPACITY);
    let journal = Journal::new(store.clone(), bus.clone());
    let deps = LoopDeps {
        capabilities: Vec::new(),
        memory: Arc::new(NoMemory),
        crash: None,
        fault: None,
        ..LoopDeps::new(journal, model.clone(), Arc::new(registry(extra)))
    };
    let run = RunId::new();
    let (ctl, rx) = control_channel(store.clone(), run);
    Fx {
        store,
        model,
        deps,
        run,
        ctl_rx: Some(rx),
        ctl,
        bus,
    }
}

fn fx(name: &str, model: ScriptedModel) -> Fx {
    fx_with(name, model, StoreOptions::default(), Vec::new())
}

fn scripted() -> ScriptedModel {
    ScriptedModel::new(default_caps(BackendKind::LlamaCpp))
}

fn spec(role: RunRole) -> RunSpec {
    RunSpec {
        project_id: "p".into(),
        project_dir: PathBuf::from("H:/p"),
        role,
        parent: None,
        endpoint: EndpointId("local".into()),
        run_mode: RunMode::Ask,
        work_mode: None,
        tier: None,
        budget: Budget::default(),
        priority: Priority::Top,
        max_wait: None,
        task: "Столица Франции?".into(),
        agents_md: None,
        env: EnvInfo {
            os: "windows".into(),
            shell: "powershell".into(),
            project_dir: "H:/p".into(),
            date: "2026-11-10".into(),
            tier: Tier::Full,
            n_ctx: 1,
            budget: "x".into(),
            branch: "main".into(),
        },
        context: za_agent::ContextConfig::default(),
        trusted: true,
        grants: Vec::new(),
    }
}

async fn start(f: &mut Fx, s: RunSpec) -> Result<LoopOutcome, LoopError> {
    let rx = f.ctl_rx.take().expect("ControlRx уже взят");
    tokio::time::timeout(limit(), start_run(&f.deps, f.run, s, rx))
        .await
        .expect("start_run не завершился в срок")
}

fn text_msg(role: Role, t: &str) -> Message {
    Message {
        role,
        parts: vec![Part::Text(t.into())],
    }
}

fn answer(t: &str) -> Vec<Result<StreamEvent, ModelError>> {
    vec![
        Ok(StreamEvent::TextDelta(t.into())),
        Ok(StreamEvent::Finished {
            message: text_msg(Role::Assistant, t),
            finish: FinishReason::Stop,
            usage: Usage::default(),
        }),
    ]
}

fn call_part(id: &str, name: &str, args: Value) -> Part {
    Part::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        raw_arguments: None,
    })
}

fn calls(parts: Vec<Part>) -> Result<StreamEvent, ModelError> {
    Ok(StreamEvent::Finished {
        message: Message {
            role: Role::Assistant,
            parts,
        },
        finish: FinishReason::ToolCalls,
        usage: Usage::default(),
    })
}

async fn events(s: &ProjectStore, run: RunId) -> Vec<Event> {
    s.read_run(run, Seq(0), 100_000).await.unwrap()
}

fn of_kind(evs: &[Event], k: EventKind) -> Vec<&Event> {
    evs.iter().filter(|e| e.kind == k).collect()
}

async fn snapshots(s: &ProjectStore, run: RunId) -> Vec<(Seq, Snapshot)> {
    s.snapshots(run)
        .await
        .unwrap()
        .into_iter()
        .map(|(seq, v)| (seq, serde_json::from_value(v).unwrap()))
        .collect()
}

async fn last_snapshot(s: &ProjectStore, run: RunId) -> (Seq, Snapshot) {
    let (seq, v) = s.latest_snapshot(run).await.unwrap().expect("нет снимка");
    (seq, serde_json::from_value(v).unwrap())
}

async fn history(s: &ProjectStore, snap: &Snapshot) -> Conversation {
    Conversation::load(s, &snap.messages).await.unwrap()
}

async fn status(s: &ProjectStore, run: RunId) -> RunStatus {
    s.get_run(run).await.unwrap().unwrap().status
}

fn roles(c: &Conversation) -> Vec<Role> {
    c.messages().iter().map(|m| m.role).collect()
}

fn text_of(m: &Message) -> String {
    m.parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

// ---- Тесты брифа ----

#[tokio::test]
async fn top_answers_and_finishes() {
    let mut f = fx("top_answers", scripted());
    f.model.push(answer("Париж"));
    let out = start(&mut f, spec(RunRole::Top)).await.unwrap();
    assert_eq!(
        out,
        LoopOutcome::Finished {
            status: RunStatus::Done,
            answer: Some("Париж".into()),
            finish: None,
        }
    );
    assert_eq!(status(&f.store, f.run).await, RunStatus::Done);
    let evs = events(&f.store, f.run).await;
    let st: Vec<&str> = of_kind(&evs, EventKind::RunStatus)
        .iter()
        .map(|e| e.payload["to"].as_str().unwrap())
        .collect();
    assert_eq!(st, ["running", "done"]);
    let fin = of_kind(&evs, EventKind::TurnFinished);
    assert_eq!(fin.len(), 1);
    assert_eq!(fin[0].payload["outcome"], "answer");
    assert_eq!(f.model.finished_runs(), vec![f.run]);
}

#[tokio::test]
async fn tool_turn_then_answer() {
    let mut f = fx("tool_turn", scripted());
    f.model
        .push(vec![calls(vec![call_part("c1", "echo", json!({"x": 1}))])]);
    f.model.push(answer("готово"));
    let out = start(&mut f, spec(RunRole::Top)).await.unwrap();
    assert!(matches!(
        out,
        LoopOutcome::Finished {
            status: RunStatus::Done,
            ..
        }
    ));
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    let h = history(&f.store, &snap).await;
    assert_eq!(
        roles(&h),
        [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    let calls = f.model.calls();
    assert_eq!(calls.len(), 2);
    let tool_msg = &calls[1].request.messages[2];
    assert_eq!(tool_msg.role, Role::Tool);
    assert!(matches!(
        &tool_msg.parts[0],
        Part::ToolResult { call_id, content, is_error: false }
            if call_id == "c1" && content == "echo {\"x\":1}"
    ));
    let model_snaps = snapshots(&f.store, f.run)
        .await
        .into_iter()
        .filter(|(_, s)| s.next == Next::Model)
        .count();
    assert!(model_snaps >= 2, "снимков Model: {model_snaps}");
}

#[tokio::test]
async fn worker_finishes_only_by_finish_tool() {
    let mut f = fx("worker_finish", scripted());
    f.model.push(answer("думаю"));
    f.model.push(answer("ещё думаю"));
    f.model.push(vec![calls(vec![call_part(
        "f1",
        "finish",
        json!({"status": "done", "result": "сделано"}),
    )])]);
    let out = start(&mut f, spec(RunRole::Worker)).await.unwrap();
    let LoopOutcome::Finished {
        status,
        finish: Some(fin),
        ..
    } = out
    else {
        panic!("ожидался Finished с finish: {out:?}");
    };
    assert_eq!(status, RunStatus::Done);
    assert_eq!(fin.status, "done");
    let calls = f.model.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].request.tool_choice, ToolChoice::Auto);
    assert_eq!(calls[1].request.tool_choice, ToolChoice::Auto);
    assert_eq!(calls[2].request.tool_choice, ToolChoice::Required);
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    let h = history(&f.store, &snap).await;
    let nudges = h
        .messages()
        .iter()
        .filter(|m| text_of(m).contains("<control from=\"runtime\" action=\"nudge\""))
        .count();
    assert_eq!(nudges, 2);
    let last = h.messages().last().unwrap();
    assert!(matches!(
        &last.parts[..],
        [Part::ToolResult { call_id, content, is_error: false }] if call_id == "f1" && content == "ok"
    ));
    assert_eq!(h.validate(), Ok(()));
    assert_eq!(f.model.finished_runs(), vec![f.run]);
}

fn truncated(t: &str) -> Vec<Result<StreamEvent, ModelError>> {
    vec![
        Ok(StreamEvent::TextDelta(t.into())),
        Ok(StreamEvent::Finished {
            message: text_msg(Role::Assistant, t),
            finish: FinishReason::Length,
            usage: Usage::default(),
        }),
    ]
}

/// I2: текст без вызовов с `finish = Length` — исход `truncated`, nudge про обрезку,
/// без `Required`; ход считается к лимиту пустых ходов.
#[tokio::test]
async fn truncated_reply_nudges_to_continue() {
    const NUDGE: &str = "Your previous reply was cut off by the output limit. Continue more briefly or split the answer into parts.";
    let mut f = fx("truncated", scripted());
    f.model.push(truncated("Столица Франции — Па"));
    f.model.push(answer("Париж"));
    let out = start(&mut f, spec(RunRole::Top)).await.unwrap();
    assert_eq!(
        out,
        LoopOutcome::Finished {
            status: RunStatus::Done,
            answer: Some("Париж".into()),
            finish: None,
        }
    );
    let calls = f.model.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].request.tool_choice, ToolChoice::Auto);
    let last = calls[1].request.messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    let text = text_of(last);
    assert!(
        text.starts_with("<control from=\"runtime\" action=\"nudge\""),
        "{text}"
    );
    assert!(text.contains(NUDGE), "{text}");
    let evs = events(&f.store, f.run).await;
    let outcomes: Vec<&str> = of_kind(&evs, EventKind::TurnFinished)
        .iter()
        .map(|e| e.payload["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes, ["truncated", "answer"]);

    // Три обрезанных подряд — `blocked`, как пустые ходы.
    let mut f = fx("truncated_block", scripted());
    for t in ["а", "б", "в"] {
        f.model.push(truncated(t));
    }
    let out = start(&mut f, spec(RunRole::Top)).await.unwrap();
    assert!(
        matches!(
            out,
            LoopOutcome::Finished {
                status: RunStatus::Blocked,
                ..
            }
        ),
        "{out:?}"
    );
    assert_eq!(f.model.calls().len(), 3);
}

/// Решение контроллера (ревью задачи 12): при `Length` с прозой и обрезанным вызовом
/// в тексте модель получает один nudge — про обрезанный вызов, без nudge обрезанного
/// ответа: он точнее (вызов не выполнен из-за обрезки).
#[tokio::test]
async fn length_cut_call_gets_single_nudge() {
    let mut f = fx("length_cut_single", scripted());
    let t = "Читаю файл.
<tool_call>
{\"name\": \"echo\", \"arguments\": {\"pa
</tool_call>";
    f.model.push(truncated(t));
    f.model.push(answer("ок"));
    let out = start(&mut f, spec(RunRole::Top)).await.unwrap();
    assert!(
        matches!(
            out,
            LoopOutcome::Finished {
                status: RunStatus::Done,
                ..
            }
        ),
        "{out:?}"
    );
    let calls = f.model.calls();
    assert_eq!(calls.len(), 2);
    let msgs = &calls[1].request.messages;
    let at = msgs
        .iter()
        .rposition(|m| m.role == Role::Assistant)
        .expect("ответа ассистента нет");
    let nudges: Vec<String> = msgs[at + 1..]
        .iter()
        .filter(|m| m.role == Role::User)
        .map(text_of)
        .collect();
    assert_eq!(nudges.len(), 1, "{nudges:?}");
    assert!(nudges[0].contains("finish_reason=length"), "{}", nudges[0]);
    assert!(
        !nudges[0].contains("Continue more briefly"),
        "{}",
        nudges[0]
    );
}

/// I1: конечный снимок пишется после конечного статуса. Сбой между статусом и
/// снимком оставляет run `done` (не resumable) с последним снимком `Model`; сбой до
/// статуса оставляет `running` и снимок `Model` — resume реплеит шаг по квитанциям.
#[tokio::test]
async fn final_snapshot_follows_final_status() {
    let points = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut f = fx("final_after_status", scripted());
    f.model.push(answer("Париж"));
    let seen = Arc::clone(&points);
    f.deps.crash = Some(Arc::new(move |p: CrashPoint| {
        seen.lock().unwrap().push(p);
        matches!(p, CrashPoint::AfterFinalStatus { .. })
    }));
    let err = start(&mut f, spec(RunRole::Top)).await.unwrap_err();
    assert!(matches!(err, LoopError::Halted), "{err:?}");
    assert_eq!(
        *points.lock().unwrap(),
        [
            CrashPoint::AfterBoundary { step: 1 },
            CrashPoint::AfterContext { step: 1 },
            CrashPoint::AfterModelEffect { step: 1 },
            CrashPoint::AfterFinalStatus { step: 1 },
        ]
    );
    assert_eq!(status(&f.store, f.run).await, RunStatus::Done);
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!(snap.next, Next::Model);
    assert!(f.model.finished_runs().is_empty());

    // Сбой до статуса: квитанция модели записана, статуса и конечного снимка нет.
    let mut f = fx("final_before_status", scripted());
    f.model.push(answer("Париж"));
    f.deps.crash = Some(Arc::new(|p: CrashPoint| {
        matches!(p, CrashPoint::AfterModelEffect { .. })
    }));
    let err = start(&mut f, spec(RunRole::Top)).await.unwrap_err();
    assert!(matches!(err, LoopError::Halted), "{err:?}");
    assert_eq!(status(&f.store, f.run).await, RunStatus::Running);
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!(snap.next, Next::Model);

    // Без сбоя конечный снимок есть и держит итоговую историю.
    let mut f = fx("final_snapshot", scripted());
    f.model.push(answer("Париж"));
    start(&mut f, spec(RunRole::Top)).await.unwrap();
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!((snap.next, snap.exit.clone()), (Next::Boundary, None));
    assert_eq!(
        roles(&history(&f.store, &snap).await),
        [Role::User, Role::Assistant]
    );
}

#[tokio::test]
async fn three_empty_turns_block() {
    let mut f = fx("three_empty", scripted());
    for t in ["a", "b", "c"] {
        f.model.push(answer(t));
    }
    let out = start(&mut f, spec(RunRole::Worker)).await.unwrap();
    assert!(
        matches!(
            out,
            LoopOutcome::Finished {
                status: RunStatus::Blocked,
                ..
            }
        ),
        "{out:?}"
    );
    assert_eq!(status(&f.store, f.run).await, RunStatus::Blocked);
    assert_eq!(f.model.finished_runs(), vec![f.run]);
    assert_eq!(f.model.calls().len(), 3);
}

#[tokio::test]
async fn provider_error_pauses_not_hangs() {
    let mut f = fx("provider_error", scripted());
    f.model.push(vec![Err(ModelError::new(
        ErrorClass::Unreachable,
        "нет связи",
    ))]);
    let out = start(&mut f, spec(RunRole::Top)).await.unwrap();
    assert_eq!(
        out,
        LoopOutcome::Paused {
            reason: "provider".into(),
            interrupt: None,
        }
    );
    assert_eq!(status(&f.store, f.run).await, RunStatus::Paused);
    let evs = events(&f.store, f.run).await;
    let st = of_kind(&evs, EventKind::RunStatus);
    assert_eq!(st.last().unwrap().payload["reason"], "provider");
    let fin = of_kind(&evs, EventKind::TurnFinished);
    assert_eq!(fin.len(), 1);
    assert_eq!(fin[0].payload["outcome"], "error");
    let (seq, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!(snap.next, Next::Boundary);
    assert_eq!(snap.step, 1);
    assert_eq!(
        snap.exit,
        Some(PendingExit::Paused {
            reason: "provider".into()
        })
    );
    assert_eq!(seq, fin[0].seq);
    assert!(f.model.finished_runs().is_empty());
}

#[tokio::test]
async fn step0_snapshot_and_memory_unavailable() {
    let mut f = fx("step0", scripted());
    f.model.push(answer("Париж"));
    start(&mut f, spec(RunRole::Top)).await.unwrap();
    let snaps = snapshots(&f.store, f.run).await;
    let (seq0, s0) = &snaps[0];
    assert_eq!(s0.step, 0);
    assert_eq!(s0.next, Next::Boundary);
    assert_eq!(s0.exit, None);
    assert_eq!(s0.setup.n_ctx, 8192);
    assert_eq!(s0.setup.tier, Tier::Small);
    let evs = events(&f.store, f.run).await;
    let mem = of_kind(&evs, EventKind::MemoryUnavailable);
    assert_eq!(mem.len(), 1);
    // R1-B2 D43: окно неизвестно — `context.alert` после снимка памяти, снимок шага 0
    // стоит на нём.
    let alert = of_kind(&evs, EventKind::ContextAlert);
    assert_eq!(alert[0].payload["kind"], "n_ctx_unknown");
    assert!(mem[0].seq < alert[0].seq);
    assert_eq!(alert[0].seq, *seq0);
    // До первого хода: снимок шага 0 раньше turn.started.
    let started = of_kind(&evs, EventKind::TurnStarted);
    assert!(*seq0 < started[0].seq);

    let mut f = fx("step0_ctx", scripted().with_context(32768));
    f.model.push(answer("Париж"));
    start(&mut f, spec(RunRole::Top)).await.unwrap();
    let (_, s0) = &snapshots(&f.store, f.run).await[0];
    assert_eq!(s0.setup.n_ctx, 32768);
    assert_eq!(s0.setup.tier, Tier::Standard);
}

#[tokio::test]
async fn refusal_pauses_without_commit() {
    let mut f = fx("refusal", scripted());
    f.model.push(vec![
        Ok(StreamEvent::TextDelta("частич".into())),
        Ok(StreamEvent::Finished {
            message: text_msg(Role::Assistant, "частич"),
            finish: FinishReason::Refusal,
            usage: Usage::default(),
        }),
    ]);
    let out = start(&mut f, spec(RunRole::Top)).await.unwrap();
    assert_eq!(
        out,
        LoopOutcome::Paused {
            reason: "refusal".into(),
            interrupt: None,
        }
    );
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    let h = history(&f.store, &snap).await;
    assert_eq!(roles(&h), [Role::User]);
}

#[tokio::test]
async fn invariant_violation_fails_run() {
    let mut f = fx("invariant", scripted());
    let mut evs = vec![Ok(StreamEvent::ServiceMessage(text_msg(Role::User, "x")))];
    evs.extend(answer("a"));
    f.model.push(evs);
    let err = start(&mut f, spec(RunRole::Top)).await.unwrap_err();
    assert!(
        matches!(
            err,
            LoopError::History(HistoryError::SystemNotBeforeAssistant)
        ),
        "{err:?}"
    );
    assert_eq!(status(&f.store, f.run).await, RunStatus::Failed);
    assert_eq!(f.model.finished_runs(), vec![f.run]);
}

#[tokio::test]
async fn full_journal_returns_store_error() {
    let mut opts = StoreOptions::default();
    opts.max_page_count = Some(256);
    let store = ProjectStore::open_with(&dir("full"), opts).unwrap();
    let filler: Arc<dyn Tool> = Arc::new(Filler {
        spec: tool_spec("fill", ToolKind::Read, EffectClass::ReadOnly),
        store: store.clone(),
    });
    let mut f = fx_on(store, scripted(), vec![filler]);
    f.model
        .push(vec![calls(vec![call_part("c1", "fill", json!({}))])]);
    let err = start(&mut f, spec(RunRole::Top)).await.unwrap_err();
    assert!(matches!(err, LoopError::Store(StoreError::Full)), "{err:?}");
    // Статус записать нельзя: run остаётся `running` для resume.
    assert_eq!(status(&f.store, f.run).await, RunStatus::Running);
    assert!(f.model.finished_runs().is_empty());
}

#[tokio::test]
async fn approval_suspends_run() {
    let mut f = fx("approval", scripted());
    f.model.push(vec![calls(vec![call_part(
        "c1",
        "edit",
        json!({"path": "a"}),
    )])]);
    let out = start(&mut f, spec(RunRole::Top)).await.unwrap();
    let LoopOutcome::Paused {
        reason,
        interrupt: Some(id),
    } = out
    else {
        panic!("ожидалась пауза с прерыванием: {out:?}");
    };
    assert_eq!(reason, "needs_approval");
    assert!(id.starts_with("appr-"), "{id}");
    assert_eq!(status(&f.store, f.run).await, RunStatus::Paused);
    assert!(f.model.finished_runs().is_empty());
}

#[tokio::test]
async fn service_message_committed_with_turn() {
    let mut f = fx("service", scripted());
    f.model.push(vec![
        Ok(StreamEvent::ServiceMessage(text_msg(Role::System, "x"))),
        calls(vec![call_part("c1", "echo", json!({}))]),
    ]);
    f.model.push(answer("готово"));
    start(&mut f, spec(RunRole::Top)).await.unwrap();
    let calls = f.model.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].request.messages[1].role, Role::System);
    assert_eq!(calls[1].request.messages[2].role, Role::Assistant);
}

#[tokio::test]
async fn first_request_matches_frozen_prefix() {
    let mut f = fx("first_request", scripted());
    f.model.push(answer("Париж"));
    let mut s = spec(RunRole::Top);
    s.max_wait = Some(Duration::from_secs(1));
    start(&mut f, s).await.unwrap();
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    let c = &f.model.calls()[0];
    assert_eq!(c.request.system, snap.frozen.system);
    assert!(text_of(&c.request.messages[0]).starts_with("<env>\n"));
    assert_eq!(c.meta.session, Some(f.run.to_string()));
    assert_eq!(c.meta.max_wait, Some(Duration::from_secs(1)));
}

// ---- Управление и решения контроллера ----

/// Пауза до первого шага применяется на границе: `control.applied`, снимок, `paused`.
#[tokio::test]
async fn pause_at_boundary_is_clean_pause() {
    let mut f = fx("pause_boundary", scripted());
    f.ctl.pause();
    let out = start(&mut f, spec(RunRole::Top)).await.unwrap();
    assert_eq!(
        out,
        LoopOutcome::Paused {
            reason: "command".into(),
            interrupt: None,
        }
    );
    assert!(f.model.calls().is_empty());
    let evs = events(&f.store, f.run).await;
    let applied = of_kind(&evs, EventKind::ControlApplied);
    assert_eq!(applied.len(), 1);
    assert_eq!(
        applied[0].payload,
        json!({"action": "pause", "command": ""})
    );
    let (seq, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!(seq, applied[0].seq);
    assert_eq!(snap.step, 0);
    assert_eq!(
        snap.exit,
        Some(PendingExit::Paused {
            reason: "command".into()
        })
    );
    assert_eq!(status(&f.store, f.run).await, RunStatus::Paused);
}

/// Отмена до первого шага: cancelling → cancelled, `finish_run`.
#[tokio::test]
async fn cancel_at_boundary_cancels() {
    let mut f = fx("cancel_boundary", scripted());
    f.ctl.cancel();
    let out = start(&mut f, spec(RunRole::Top)).await.unwrap();
    assert_eq!(out, LoopOutcome::Cancelled);
    assert_eq!(status(&f.store, f.run).await, RunStatus::Cancelled);
    assert_eq!(f.model.finished_runs(), vec![f.run]);
}

/// Carry задачи 10: снятая пауза не мешает следующей паузе в том же процессе.
#[tokio::test]
async fn cleared_pause_signals_again() {
    let store = ProjectStore::open(&dir("clear_pause")).unwrap();
    let (ctl, mut rx) = control_channel(store, RunId::new());
    ctl.pause();
    assert_eq!(rx.signals().drain(), Some(Signal::Pause));
    assert_eq!(rx.signals().drain(), None);
    rx.clear_pause();
    assert_eq!(rx.signals().drain(), None);
    ctl.pause();
    assert_eq!(rx.signals().drain(), Some(Signal::Pause));
    // Пауза сразу после снятия, до чтения, тоже не теряется.
    rx.clear_pause();
    ctl.pause();
    assert_eq!(rx.signals().drain(), Some(Signal::Pause));
}

/// Неизвестный эндпоинт: ошибка до создания run.
#[tokio::test]
async fn unknown_endpoint_creates_no_run() {
    struct NoCaps;
    impl za_llm::ModelPort for NoCaps {
        fn stream(
            &self,
            _call: za_llm::ModelCall,
        ) -> BoxFuture<'_, Result<za_llm::ModelStream, ModelError>> {
            Box::pin(async { Err(ModelError::new(ErrorClass::Inactive, "нет")) })
        }
        fn capabilities(&self, _e: &EndpointId) -> Option<za_llm::ProviderCapabilities> {
            None
        }
        fn context_tokens(&self, _e: &EndpointId) -> Option<u32> {
            None
        }
        fn record_parse_error(&self, _run: RunId, _call: &za_llm::ModelCall) {}
    }
    let mut f = fx("unknown_endpoint", scripted());
    f.deps.model = Arc::new(NoCaps);
    let err = start(&mut f, spec(RunRole::Top)).await.unwrap_err();
    assert!(matches!(err, LoopError::UnknownEndpoint(_)), "{err:?}");
    assert!(f.store.get_run(f.run).await.unwrap().is_none());
}

/// Повтор имени инструмента в префиксе — ошибка конфигурации: `failed`, `finish_run`.
#[tokio::test]
async fn duplicate_tool_fails_run() {
    let mut f = fx("duplicate_tool", scripted());
    f.deps.capabilities = vec![za_core::Capability {
        name: "c".into(),
        tools: vec![Arc::new(Echo {
            spec: tool_spec("echo", ToolKind::Read, EffectClass::ReadOnly),
        })],
        ..Default::default()
    }];
    let err = start(&mut f, spec(RunRole::Top)).await.unwrap_err();
    assert!(matches!(err, LoopError::Prefix(_)), "{err:?}");
    assert_eq!(status(&f.store, f.run).await, RunStatus::Failed);
    assert_eq!(f.model.finished_runs(), vec![f.run]);
}

/// Решение контроллера к задаче 11: пауза, пришедшая во время `finish`, не мешает
/// завершению — `finish` важнее обрыва пакета. Пауза посылается по событию
/// `tool.started` вызова `finish`; попадёт ли она внутрь вызова или после пакета,
/// зависит от планировщика, но исход один: `done`, без `control.applied`.
#[tokio::test]
async fn finish_wins_over_pause_during_finish() {
    let mut f = fx("finish_wins", scripted());
    f.model.push(vec![calls(vec![call_part(
        "f1",
        "finish",
        json!({"status": "done", "result": "r"}),
    )])]);
    let mut bus = f.bus.subscribe();
    let ctl = f.ctl.clone();
    let watcher = tokio::spawn(async move {
        loop {
            let ev = bus.recv().await.expect("шина закрыта");
            if ev.event.kind == EventKind::ToolStarted {
                ctl.pause();
                return;
            }
        }
    });
    let out = start(&mut f, spec(RunRole::Worker)).await.unwrap();
    tokio::time::timeout(limit(), watcher)
        .await
        .expect("tool.started не пришёл")
        .unwrap();
    assert!(
        matches!(
            out,
            LoopOutcome::Finished {
                status: RunStatus::Done,
                ..
            }
        ),
        "{out:?}"
    );
    let evs = events(&f.store, f.run).await;
    assert!(of_kind(&evs, EventKind::ControlApplied).is_empty());
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    let h = history(&f.store, &snap).await;
    assert_eq!(h.validate(), Ok(()));
    assert!(h.pending_calls().is_empty());
}

// ---- Разовый сбой журнала: пауза store статусом (R1-C1 D72) ----

/// Хук, срабатывающий один раз в точке `at`.
fn fault_once(at: FaultPoint) -> FaultHook {
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(true));
    Arc::new(move |p| p == at && armed.swap(false, std::sync::atomic::Ordering::SeqCst))
}

/// Resume run фикстуры без хука сбоя.
async fn resume_clean(f: &Fx) -> Result<LoopOutcome, LoopError> {
    let mut deps = f.deps.clone();
    deps.fault = None;
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    within(
        "resume_run",
        resume_run(&deps, f.run, ResumeChange::default(), rx),
    )
    .await
}

/// Последний `reason` перехода `run.status` в `paused`.
fn pause_reason_of(evs: &[Event]) -> Option<String> {
    of_kind(evs, EventKind::RunStatus)
        .into_iter()
        .rev()
        .find(|e| e.payload["to"] == "paused")
        .and_then(|e| e.payload["reason"].as_str().map(str::to_owned))
}

fn store_paused() -> LoopOutcome {
    LoopOutcome::Paused {
        reason: pause_reason::STORE.into(),
        interrupt: None,
    }
}

#[tokio::test]
async fn boundary_fault_pauses_with_store_then_resumes() {
    let mut f = fx("boundary_fault", scripted());
    f.deps.fault = Some(fault_once(FaultPoint::BeforeReadBoundary { step: 2 }));
    f.model
        .push(vec![calls(vec![call_part("c1", "echo", json!({"x": 1}))])]);
    f.model.push(answer("готово"));
    let out = start(&mut f, spec(RunRole::Top)).await.unwrap();
    assert_eq!(out, store_paused());
    assert_eq!(status(&f.store, f.run).await, RunStatus::Paused);
    let evs = events(&f.store, f.run).await;
    assert_eq!(pause_reason_of(&evs).as_deref(), Some(pause_reason::STORE));
    // Снимка паузы нет: последний — снимок `Model` шага 1, записанный до шага 2.
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!((snap.step, snap.next, &snap.exit), (1, Next::Model, &None));
    // Намерение `boundary` шага 2 без квитанции: resume прочитает ввод заново.
    let row = f
        .store
        .effect(f.run, &EffectKey::derive(f.run, 2, 0))
        .await
        .unwrap()
        .expect("намерение boundary шага 2");
    assert!(row.receipt_seq.is_none(), "квитанции разового сбоя нет");
    assert_eq!(f.model.calls().len(), 1);

    let out = resume_clean(&f).await.unwrap();
    assert!(
        matches!(
            out,
            LoopOutcome::Finished {
                status: RunStatus::Done,
                ..
            }
        ),
        "{out:?}"
    );
    let row = f
        .store
        .effect(f.run, &EffectKey::derive(f.run, 2, 0))
        .await
        .unwrap()
        .unwrap();
    assert!(row.receipt_seq.is_some(), "граница шага 2 прочитана вживую");
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    let h = history(&f.store, &snap).await;
    h.validate().unwrap();
    assert_eq!(
        roles(&h),
        [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    // Ход шага 1 взят из квитанции: модель вызвана ещё раз только для шага 2.
    assert_eq!(f.model.calls().len(), 2);
}

#[tokio::test]
async fn tool_finished_fault_keeps_history_valid() {
    let mut f = fx("tool_finished_fault", scripted());
    f.deps.fault = Some(fault_once(FaultPoint::BeforeToolFinished { step: 1 }));
    f.model
        .push(vec![calls(vec![call_part("c1", "echo", json!({"x": 1}))])]);
    f.model.push(answer("готово"));
    let out = start(&mut f, spec(RunRole::Top)).await.unwrap();
    assert_eq!(out, store_paused());
    assert_eq!(status(&f.store, f.run).await, RunStatus::Paused);
    let evs = events(&f.store, f.run).await;
    assert_eq!(pause_reason_of(&evs).as_deref(), Some(pause_reason::STORE));
    assert!(of_kind(&evs, EventKind::ToolFinished).is_empty());
    // Пауза без снимка: последний снимок — `Model` шага 1, до вызова.
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!((snap.step, snap.next, &snap.exit), (1, Next::Model, &None));
    history(&f.store, &snap).await.validate().unwrap();

    let out = resume_clean(&f).await.unwrap();
    assert!(
        matches!(
            out,
            LoopOutcome::Finished {
                status: RunStatus::Done,
                ..
            }
        ),
        "{out:?}"
    );
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    let h = history(&f.store, &snap).await;
    h.validate().unwrap();
    assert_eq!(
        roles(&h),
        [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    // Квитанция инструмента записана до сбоя: реплей не вызывает его снова.
    let evs = events(&f.store, f.run).await;
    assert_eq!(of_kind(&evs, EventKind::ToolStarted).len(), 1);
}

// ---- Resume с границы шага 1 без вклинивания `state` (R1-C1, Ruling I1) ----

/// Сбой один раз в точке `at`.
fn crash_once(at: CrashPoint) -> za_agent::CrashHook {
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(true));
    Arc::new(move |p| p == at && armed.swap(false, std::sync::atomic::Ordering::SeqCst))
}

/// Ход с вызовом и ответ; после resume история проходит `validate()`, `replay_mismatch` нет.
async fn resume_after_step1(name: &str, crash: Option<CrashPoint>, fault: Option<FaultPoint>) {
    let mut f = fx(name, scripted());
    f.deps.crash = crash.map(crash_once);
    f.deps.fault = fault.map(fault_once);
    f.model
        .push(vec![calls(vec![call_part("c1", "echo", json!({"x": 1}))])]);
    f.model.push(answer("готово"));
    let first = start(&mut f, spec(RunRole::Top)).await;
    match (crash, &first) {
        (Some(_), Err(LoopError::Halted)) => {}
        (None, Ok(o)) if *o == store_paused() => {}
        other => panic!("первая попытка: {other:?}"),
    }
    // Последний снимок — `Boundary` шага 0: resume взводит блок `<state>`.
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!((snap.step, snap.next), (0, Next::Boundary));
    let mut deps = f.deps.clone();
    deps.crash = None;
    deps.fault = None;
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    let out = within(
        "resume_run",
        resume_run(&deps, f.run, ResumeChange::default(), rx),
    )
    .await
    .unwrap();
    assert!(
        matches!(
            out,
            LoopOutcome::Finished {
                status: RunStatus::Done,
                ..
            }
        ),
        "{out:?}"
    );
    let evs = events(&f.store, f.run).await;
    assert_ne!(
        pause_reason_of(&evs).as_deref(),
        Some(pause_reason::REPLAY_MISMATCH)
    );
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    let h = history(&f.store, &snap).await;
    h.validate().unwrap();
    // Блок `<state>` resume есть в истории ровно один раз.
    let notes = h
        .messages()
        .iter()
        .filter(|m| text_of(m).contains("action=\"resume\""))
        .count();
    assert_eq!(notes, 1, "{:?}", roles(&h));
}

#[tokio::test]
async fn step1_crash_after_boundary_resumes() {
    resume_after_step1(
        "s1_crash_boundary",
        Some(CrashPoint::AfterBoundary { step: 1 }),
        None,
    )
    .await;
}

#[tokio::test]
async fn step1_crash_after_context_resumes() {
    resume_after_step1(
        "s1_crash_context",
        Some(CrashPoint::AfterContext { step: 1 }),
        None,
    )
    .await;
}

#[tokio::test]
async fn step1_fault_after_boundary_resumes() {
    resume_after_step1(
        "s1_fault_boundary",
        None,
        Some(FaultPoint::AfterBoundary { step: 1 }),
    )
    .await;
}

#[tokio::test]
async fn step1_fault_after_context_resumes() {
    resume_after_step1(
        "s1_fault_context",
        None,
        Some(FaultPoint::AfterContext { step: 1 }),
    )
    .await;
}

// ---- Повреждение журнала — failed, а не пауза store (Ruling, сомнение 2) ----

/// Инструмент записывает под ключом `boundary` следующего шага квитанцию чужой формы:
/// реплей границы шага 2 разбирает её как `StoreError::Corrupt`.
struct Corrupter {
    spec: ToolSpec,
    store: ProjectStore,
}

impl Tool for Corrupter {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, _args: Value, cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let store = self.store.clone();
        Box::pin(async move {
            let key = EffectKey::derive(cx.run, 2, 0);
            store
                .record_intent(
                    cx.run,
                    &key,
                    EffectClass::ReadOnly,
                    "boundary",
                    json!({ "step": 2 }),
                )
                .await
                .unwrap();
            store
                .record_receipt(
                    cx.run,
                    &key,
                    json!({"outcome": "ok", "value": {"bogus": 1}}),
                )
                .await
                .unwrap();
            ToolOutput::ok("corrupted")
        })
    }
}

#[tokio::test]
async fn corrupt_journal_fails_not_pauses() {
    let store = ProjectStore::open(&dir("corrupt")).unwrap();
    let t: Arc<dyn Tool> = Arc::new(Corrupter {
        spec: tool_spec("corrupt", ToolKind::Read, EffectClass::ReadOnly),
        store: store.clone(),
    });
    let mut f = fx_on(store, scripted(), vec![t]);
    f.model
        .push(vec![calls(vec![call_part("c1", "corrupt", json!({}))])]);
    let err = start(&mut f, spec(RunRole::Top)).await.unwrap_err();
    assert!(
        matches!(err, LoopError::Store(StoreError::Corrupt(_))),
        "{err:?}"
    );
    assert_eq!(status(&f.store, f.run).await, RunStatus::Failed);
    let evs = events(&f.store, f.run).await;
    let last = of_kind(&evs, EventKind::RunStatus)
        .last()
        .unwrap()
        .payload
        .clone();
    assert_eq!(last["reason"], "store");
}

/// m-new1: шаг после снимка `Boundary` начат без блока `<state>` — смена эндпоинта при
/// resume отвергается (`ChangeMidStep`), resume без смены доводит run.
#[tokio::test]
async fn endpoint_change_refused_on_started_step() {
    let mut f = fx("endpoint_started_step", scripted());
    f.deps.crash = Some(crash_once(CrashPoint::AfterContext { step: 1 }));
    f.model.push(answer("готово"));
    assert!(matches!(
        start(&mut f, spec(RunRole::Top)).await,
        Err(LoopError::Halted)
    ));
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    let change = ResumeChange {
        endpoint: Some(EndpointId("local".into())),
        ..ResumeChange::default()
    };
    let r = within("resume_run", resume_run(&f.deps, f.run, change, rx)).await;
    assert!(matches!(r, Err(LoopError::ChangeMidStep)), "{r:?}");
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    let out = within(
        "resume_run",
        resume_run(&f.deps, f.run, ResumeChange::default(), rx),
    )
    .await
    .unwrap();
    assert!(matches!(
        out,
        LoopOutcome::Finished {
            status: RunStatus::Done,
            ..
        }
    ));
}

// ---- R1-C2, задача 1: невидимые символы во вводе (D102, SEC-051) ----

/// Инструмент, который на вызове пишет ввод пользователя в журнал run (как
/// `RunControl::input` в режиме `Queue`): следующая граница его прочитает.
struct Inputter {
    spec: ToolSpec,
    store: ProjectStore,
    text: String,
}

impl Tool for Inputter {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, _args: Value, cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let (store, text) = (self.store.clone(), self.text.clone());
        Box::pin(async move {
            za_agent::submit_input(&store, cx.run, &text, za_agent::InputMode::Queue)
                .await
                .unwrap();
            ToolOutput::ok("sent")
        })
    }
}

#[tokio::test]
async fn task_with_tag_chars_is_cleaned_and_logged() {
    let mut f = fx("task_tag_chars", scripted());
    f.model.push(answer("ок"));
    let mut s = spec(RunRole::Top);
    s.task = "fix\u{E0069}\u{E0067} bug".into();
    let out = start(&mut f, s).await.unwrap();
    assert!(matches!(
        out,
        LoopOutcome::Finished {
            status: RunStatus::Done,
            ..
        }
    ));
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    let conv = history(&f.store, &snap).await;
    let first = text_of(&conv.messages()[0]);
    assert!(first.contains("fix bug"), "{first}");
    assert!(!first.contains('\u{E0069}'), "{first:?}");
    // Модель видит очищенную задачу.
    let sent = text_of(&f.model.calls()[0].request.messages[0]);
    assert!(sent.contains("fix bug"), "{sent}");
    let evs = events(&f.store, f.run).await;
    let cleaned = of_kind(&evs, EventKind::InputCleaned);
    assert_eq!(cleaned.len(), 1, "{cleaned:?}");
    assert_eq!(cleaned[0].payload, json!({"source": "task", "removed": 2}));
    assert_eq!(cleaned[0].actor, Actor::Runtime);
}

/// Память L1 без текста: шаг 0 не пишет `memory.unavailable`.
struct EmptyMemory;

impl za_agent::MemoryPort for EmptyMemory {
    fn level(&self) -> za_agent::MemoryLevel {
        za_agent::MemoryLevel::L1
    }

    fn snapshot(&self, _run: RunId, _budget_tokens: u32) -> BoxFuture<'_, za_agent::Recall> {
        Box::pin(async { za_agent::Recall::Empty })
    }

    fn before_compaction<'a>(&'a self, _run: RunId, _next_step: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn after_compaction(
        &self,
        _run: RunId,
        _budget_tokens: u32,
    ) -> BoxFuture<'_, za_agent::Recall> {
        Box::pin(async { za_agent::Recall::Empty })
    }
}

/// Ревью задачи 1, m1 (D4): `input.cleaned` шага 0 пишется после эффекта памяти,
/// снимок шага 0 стоит на последнем событии до первого хода.
#[tokio::test]
async fn step0_snapshot_on_last_event_after_cleaned_task() {
    let mut f = fx("task_tag_snapshot", scripted().with_context(32768));
    f.deps.memory = Arc::new(EmptyMemory);
    f.model.push(answer("ок"));
    let mut s = spec(RunRole::Top);
    s.task = "fix\u{E0069} bug".into();
    s.agents_md = Some("rules\u{202E}".into());
    start(&mut f, s).await.unwrap();
    let (seq0, s0) = &snapshots(&f.store, f.run).await[0];
    assert_eq!(s0.step, 0);
    let evs = events(&f.store, f.run).await;
    // Шаг 1 начинается с эффекта `boundary`: снимок шага 0 — на событии перед ним.
    let boundary = evs
        .iter()
        .find(|e| e.kind == EventKind::EffectIntent && e.payload["name"] == "boundary")
        .expect("нет эффекта boundary")
        .seq;
    let last_before = evs
        .iter()
        .filter(|e| e.seq < boundary)
        .map(|e| e.seq)
        .max()
        .unwrap();
    assert_eq!(*seq0, last_before, "{evs:#?}");
    // Квитанция памяти раньше `input.cleaned`.
    let mem = evs
        .iter()
        .find(|e| e.kind == EventKind::EffectIntent && e.payload["name"] == "memory.snapshot")
        .expect("нет эффекта памяти")
        .seq;
    let cleaned = of_kind(&evs, EventKind::InputCleaned);
    assert_eq!(cleaned.len(), 2, "{cleaned:?}");
    assert!(cleaned.iter().all(|e| e.seq > mem), "{cleaned:?}");
}

#[tokio::test]
async fn clean_task_logs_nothing() {
    let mut f = fx("task_clean", scripted());
    f.model.push(answer("ок"));
    start(&mut f, spec(RunRole::Top)).await.unwrap();
    let evs = events(&f.store, f.run).await;
    assert!(of_kind(&evs, EventKind::InputCleaned).is_empty());
}

#[tokio::test]
async fn inbox_input_is_cleaned() {
    let store = ProjectStore::open_with(&dir("inbox_cleaned"), StoreOptions::default()).unwrap();
    let inputter: Arc<dyn Tool> = Arc::new(Inputter {
        spec: tool_spec("send_input", ToolKind::Read, EffectClass::ReadOnly),
        store: store.clone(),
        text: "abc\u{202E}def \u{1F468}\u{200D}\u{1F469}".into(),
    });
    let mut f = fx_on(store, scripted(), vec![inputter]);
    f.model
        .push(vec![calls(vec![call_part("c1", "send_input", json!({}))])]);
    f.model.push(answer("ок"));
    let out = start(&mut f, spec(RunRole::Top)).await.unwrap();
    assert!(matches!(
        out,
        LoopOutcome::Finished {
            status: RunStatus::Done,
            ..
        }
    ));
    // Ввод дошёл до модели без U+202E, ZWJ в эмодзи остался.
    let msgs = &f.model.calls()[1].request.messages;
    let user: Vec<String> = msgs
        .iter()
        .filter(|m| m.role == Role::User)
        .map(text_of)
        .collect();
    assert!(
        user.iter()
            .any(|t| t == "abcdef \u{1F468}\u{200D}\u{1F469}"),
        "{user:?}"
    );
    assert!(user.iter().all(|t| !t.contains('\u{202E}')), "{user:?}");
    let evs = events(&f.store, f.run).await;
    let cleaned = of_kind(&evs, EventKind::InputCleaned);
    assert_eq!(cleaned.len(), 1, "{cleaned:?}");
    // Ревью задачи 1, m2: `seq` исходного `turn.input` — повтор после resume
    // отличается от нового ввода.
    let input = of_kind(&evs, EventKind::TurnInput);
    let src = input
        .iter()
        .find(|e| {
            e.payload["text"]
                .as_str()
                .is_some_and(|t| t.starts_with("abc"))
        })
        .expect("нет turn.input");
    assert_eq!(
        cleaned[0].payload,
        json!({"source": "input", "removed": 1, "seq": src.seq.0})
    );
}
