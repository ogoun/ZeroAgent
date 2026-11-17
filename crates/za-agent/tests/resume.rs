//! Resume run (задача 15): реплей после сбоя (Review Focus 1), отложенный выход
//! чистой паузы (D5), смена модели только на границе (D6), `cancelling` и `pausing`
//! после сбоя (D26), бюджет и ввод при resume, блок `<state>`.
//!
//! «Сбой» — либо брошенный future цикла на ожидании, которое не держит запись журнала
//! (стрим `push_open`, инструмент на семафоре; D33), либо крючок `LoopDeps.crash`
//! (`Err(Halted)` без единой записи). Сигналы посылаются по событию шины, не по таймеру.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use support::*;

use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore, broadcast};
use tokio::task::JoinHandle;
use za_agent::{
    ContextConfig, Conversation, CrashPoint, EnvInfo, LoopDeps, LoopError, LoopOutcome, Next,
    NoMemory, PendingExit, ResumeChange, RunSpec, Snapshot, builtin_tools, control_channel,
    resume_run, start_run, state_block,
};
use za_core::{
    AgentEvent, Approval, EventBus, Journal, RiskClass, Tool, ToolCx, ToolKind, ToolOutput,
    ToolRegistry, ToolSpec, UnknownDecision, resolve_unknown,
};
use za_llm::{
    BackendKind, BoxFuture, EndpointId, ErrorClass, FinishReason, Message, ModelError, Part, Role,
    ScriptedModel, StreamEvent, ToolCall, ToolSchema, Usage, default_caps,
};
use za_protocol::{
    Budget, BudgetKind, EffectClass, EffectKey, Event, EventKind, Priority, RunId, RunMode,
    RunRole, RunStatus, Seq, Tier,
};
use za_store::{NewRun, ProjectStore};

// ---- Помощники ----

/// Свой каталог теста: `CARGO_TARGET_TMPDIR/resume/<name>-<pid>-<n>`.
fn dir(name: &str) -> PathBuf {
    unique_dir("resume", name)
}

/// Чтение `echo`: результат `echo {args}`.
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

/// Мутация `incr`: счётчик вызовов, результат `incr {n}`.
struct Incr {
    spec: ToolSpec,
    count: Arc<AtomicUsize>,
}

impl Tool for Incr {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, _args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move { ToolOutput::ok(format!("incr {n}")) })
    }
}

/// Чтение `hang`: ждёт разрешения теста, результат `hung`.
struct Hang {
    spec: ToolSpec,
    release: Arc<Semaphore>,
}

impl Tool for Hang {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, _args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            let permit = tokio::time::timeout(limit(), release.acquire())
                .await
                .expect("инструмент hang не получил разрешения в срок")
                .expect("семафор закрыт");
            permit.forget();
            ToolOutput::ok("hung")
        })
    }
}

/// Мутация `charge`: увеличивает счётчик, сообщает тесту и ждёт вечно (намерение без квитанции).
struct Charge {
    spec: ToolSpec,
    count: Arc<AtomicUsize>,
    entered: Arc<Notify>,
}

impl Tool for Charge {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, _args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        Box::pin(async move {
            std::future::pending::<()>().await;
            unreachable!()
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
        timeout: Duration::from_secs(600),
        output_limit: None,
        tainted: false,
    }
}

struct Fx {
    store: ProjectStore,
    model: Arc<ScriptedModel>,
    deps: LoopDeps,
    run: RunId,
    bus: EventBus,
    incr: Arc<AtomicUsize>,
    charge: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    release: Arc<Semaphore>,
}

fn fx(name: &str) -> Fx {
    fx_in(&dir(name))
}

fn fx_in(d: &std::path::Path) -> Fx {
    let store = ProjectStore::open(d).unwrap();
    let model = Arc::new(ScriptedModel::new(default_caps(BackendKind::LlamaCpp)));
    let bus = EventBus::new(EventBus::DEFAULT_CAPACITY);
    let incr = Arc::new(AtomicUsize::new(0));
    let charge = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let mut reg = ToolRegistry::default();
    for t in builtin_tools() {
        reg.register(t).unwrap();
    }
    reg.register(Arc::new(Echo {
        spec: tool_spec("echo", ToolKind::Read, EffectClass::ReadOnly),
    }))
    .unwrap();
    reg.register(Arc::new(Incr {
        spec: tool_spec("incr", ToolKind::Exec, EffectClass::NonIdempotent),
        count: Arc::clone(&incr),
    }))
    .unwrap();
    reg.register(Arc::new(Hang {
        spec: tool_spec("hang", ToolKind::Read, EffectClass::ReadOnly),
        release: Arc::clone(&release),
    }))
    .unwrap();
    reg.register(Arc::new(Charge {
        spec: tool_spec("charge", ToolKind::Exec, EffectClass::NonIdempotent),
        count: Arc::clone(&charge),
        entered: Arc::clone(&entered),
    }))
    .unwrap();
    let deps = LoopDeps {
        capabilities: Vec::new(),
        memory: Arc::new(NoMemory),
        crash: None,
        fault: None,
        ..LoopDeps::new(
            Journal::new(store.clone(), bus.clone()),
            model.clone(),
            Arc::new(reg),
        )
    };
    Fx {
        store,
        model,
        deps,
        run: RunId::new(),
        bus,
        incr,
        charge,
        entered,
        release,
    }
}

fn spec(mode: RunMode, budget: Budget) -> RunSpec {
    RunSpec {
        project_id: "p".into(),
        project_dir: PathBuf::from("H:/p"),
        role: RunRole::Top,
        parent: None,
        endpoint: EndpointId("a".into()),
        run_mode: mode,
        work_mode: None,
        tier: None,
        budget,
        priority: Priority::Top,
        max_wait: None,
        task: "Задача".into(),
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
        context: ContextConfig::default(),
        trusted: true,
        grants: Vec::new(),
    }
}

fn ask() -> RunSpec {
    spec(RunMode::Ask, Budget::default())
}

/// Цикл в отдельной задаче: её можно бросить (имитация `kill -9` на ожидании).
fn spawn_start(
    f: &Fx,
    spec: RunSpec,
) -> (
    JoinHandle<Result<LoopOutcome, LoopError>>,
    za_agent::RunControl,
) {
    let (ctl, rx) = control_channel(f.store.clone(), f.run);
    let (deps, run) = (f.deps.clone(), f.run);
    (
        tokio::spawn(async move { start_run(&deps, run, spec, rx).await }),
        ctl,
    )
}

async fn start(f: &Fx, spec: RunSpec) -> Result<LoopOutcome, LoopError> {
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    within("start_run", start_run(&f.deps, f.run, spec, rx)).await
}

/// Бросает future цикла и дожидается, что задача действительно снята.
async fn crash(h: JoinHandle<Result<LoopOutcome, LoopError>>) {
    h.abort();
    let r = within("снятие задачи цикла", h).await;
    assert!(
        r.as_ref().is_err_and(|e| e.is_cancelled()),
        "цикл завершился сам до сбоя: {r:?}"
    );
}

async fn resume_with(
    f: &Fx,
    deps: &LoopDeps,
    change: ResumeChange,
) -> Result<LoopOutcome, LoopError> {
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    within("resume_run", resume_run(deps, f.run, change, rx)).await
}

async fn resume(f: &Fx, change: ResumeChange) -> Result<LoopOutcome, LoopError> {
    resume_with(f, &f.deps, change).await
}

fn answer(t: &str) -> Vec<Result<StreamEvent, ModelError>> {
    vec![
        Ok(StreamEvent::TextDelta(t.into())),
        Ok(StreamEvent::Finished {
            message: Message {
                role: Role::Assistant,
                parts: vec![Part::Text(t.into())],
            },
            finish: FinishReason::Stop,
            usage: Usage::default(),
        }),
    ]
}

fn calls(cs: &[(&str, &str)]) -> Vec<Result<StreamEvent, ModelError>> {
    vec![Ok(StreamEvent::Finished {
        message: Message {
            role: Role::Assistant,
            parts: cs
                .iter()
                .map(|(id, name)| {
                    Part::ToolCall(ToolCall {
                        id: (*id).into(),
                        name: (*name).into(),
                        arguments: json!({"n": 1}),
                        raw_arguments: None,
                    })
                })
                .collect(),
        },
        finish: FinishReason::ToolCalls,
        usage: Usage::default(),
    })]
}

fn server_error() -> Vec<Result<StreamEvent, ModelError>> {
    vec![Err(ModelError::new(ErrorClass::Server, "500"))]
}

fn done(t: &str) -> LoopOutcome {
    LoopOutcome::Finished {
        status: RunStatus::Done,
        answer: Some(t.into()),
        finish: None,
    }
}

fn paused(reason: &str) -> LoopOutcome {
    LoopOutcome::Paused {
        reason: reason.into(),
        interrupt: None,
    }
}

fn crash_at(at: CrashPoint) -> Option<za_agent::CrashHook> {
    Some(Arc::new(move |p: CrashPoint| p == at))
}

/// Ждёт на шине `n`-е событие вида `kind` (с 1), подходящее под `pred`.
async fn nth_of(
    bus: &mut broadcast::Receiver<AgentEvent>,
    kind: EventKind,
    n: usize,
    pred: impl Fn(&Value) -> bool,
) -> AgentEvent {
    within(&format!("событие {kind:?} № {n}"), async {
        let mut seen = 0;
        loop {
            let ev = bus.recv().await.expect("шина закрыта");
            if ev.event.kind == kind && pred(&ev.event.payload) {
                seen += 1;
                if seen == n {
                    return ev;
                }
            }
        }
    })
    .await
}

async fn events(s: &ProjectStore, run: RunId) -> Vec<Event> {
    s.read_run(run, Seq(0), 100_000).await.unwrap()
}

fn of_kind(evs: &[Event], k: EventKind) -> Vec<&Event> {
    evs.iter().filter(|e| e.kind == k).collect()
}

/// Цепочка статусов run по событиям `run.status` (без начального `running`).
async fn statuses(s: &ProjectStore, run: RunId) -> Vec<RunStatus> {
    of_kind(&events(s, run).await, EventKind::RunStatus)
        .into_iter()
        .map(|e| serde_json::from_value(e.payload["to"].clone()).unwrap())
        .collect()
}

async fn status(s: &ProjectStore, run: RunId) -> RunStatus {
    s.get_run(run).await.unwrap().unwrap().status
}

async fn last_snapshot(s: &ProjectStore, run: RunId) -> (Seq, Snapshot) {
    let (seq, v) = s.latest_snapshot(run).await.unwrap().expect("нет снимка");
    (seq, serde_json::from_value(v).unwrap())
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

fn result_of(msgs: &[Message], id: &str) -> Option<(String, bool)> {
    msgs.iter().flat_map(|m| &m.parts).find_map(|p| match p {
        Part::ToolResult {
            call_id,
            content,
            is_error,
        } if call_id == id => Some((content.clone(), *is_error)),
        _ => None,
    })
}

fn user_texts(msgs: &[Message]) -> Vec<String> {
    msgs.iter()
        .filter(|m| m.role == Role::User)
        .map(text_of)
        .collect()
}

// ---- Тесты брифа ----

/// Review Focus 1: сбой посреди стрима — повторный запрос байт-в-байт тот же.
#[tokio::test]
async fn crash_mid_stream_replays_same_request() {
    let f = fx("mid_stream");
    f.model.push(calls(&[("c1", "echo")]));
    f.model.push_open(Vec::new());
    let mut bus = f.bus.subscribe();
    let (h, _ctl) = spawn_start(&f, ask());
    nth_of(&mut bus, EventKind::ModelRequest, 2, |_| true).await;
    crash(h).await;
    assert_eq!(f.model.open_streams(), 0);
    f.model.push(answer("ок"));
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    let cs = f.model.calls();
    assert_eq!(cs.len(), 3);
    assert_eq!(cs[2], cs[1]);
}

/// Review Focus 1: `NonIdempotent` с квитанцией не повторяется, прерванное чтение — да.
#[tokio::test]
async fn crash_after_nonidempotent_tool_does_not_repeat() {
    let f = fx("after_nonidempotent");
    f.model.push(calls(&[("c1", "incr"), ("c2", "hang")]));
    let mut bus = f.bus.subscribe();
    let (h, _ctl) = spawn_start(&f, spec(RunMode::Auto, Budget::default()));
    nth_of(&mut bus, EventKind::ToolStarted, 1, |p| p["name"] == "hang").await;
    assert_eq!(f.incr.load(Ordering::SeqCst), 1);
    crash(h).await;
    f.release.add_permits(1);
    f.model.push(answer("ок"));
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    assert_eq!(f.incr.load(Ordering::SeqCst), 1, "incr вызван повторно");
    let cs = f.model.calls();
    assert_eq!(cs.len(), 2);
    let msgs = &cs[1].request.messages;
    assert_eq!(result_of(msgs, "c1"), Some(("incr 1".into(), false)));
    assert_eq!(result_of(msgs, "c2"), Some(("hung".into(), false)));
}

/// Review Focus 1: намерение без квитанции — решение человека; `done` — без повтора.
#[tokio::test]
async fn crash_between_intent_and_receipt_asks_user() {
    let f = fx("intent_no_receipt");
    f.model.push(calls(&[("c1", "charge")]));
    let (h, _ctl) = spawn_start(&f, spec(RunMode::Auto, Budget::default()));
    within("вход в charge", f.entered.notified()).await;
    assert_eq!(f.charge.load(Ordering::SeqCst), 1);
    crash(h).await;
    let out = resume(&f, ResumeChange::default()).await.unwrap();
    let LoopOutcome::Paused {
        reason,
        interrupt: Some(id),
    } = out
    else {
        panic!("ожидалась пауза needs_decision: {out:?}");
    };
    assert_eq!(reason, "needs_decision");
    assert!(id.starts_with("unk-"), "{id}");
    assert_eq!(status(&f.store, f.run).await, RunStatus::Paused);
    resolve_unknown(&f.store, &id, UnknownDecision::Done)
        .await
        .unwrap();
    f.model.push(answer("ок"));
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    assert_eq!(f.charge.load(Ordering::SeqCst), 1, "charge вызван повторно");
    let cs = f.model.calls();
    assert_eq!(cs.len(), 2);
    assert_eq!(
        result_of(&cs[1].request.messages, "c1"),
        Some((r#"{"resolved":"done"}"#.into(), false))
    );
    // Блок <state>: решённый неизвестный исход и выполненная мутация.
    let key = id.trim_start_matches("unk-");
    let text = state_block(&f.store, f.run).await.unwrap();
    assert!(
        text.contains(&format!("unknown outcomes: {key} → done\n")),
        "{text}"
    );
    assert!(text.contains(r#"changes made: charge({"n":1})"#), "{text}");
}

/// Review Focus 1, окно «квитанция — снимок»: реплей читает ошибку из квитанции.
#[tokio::test]
async fn receipt_without_snapshot_repauses() {
    let mut f = fx("receipt_no_snapshot");
    f.model.push(server_error());
    let plain = f.deps.clone();
    f.deps.crash = crash_at(CrashPoint::AfterModelEffect { step: 1 });
    let err = start(&f, ask()).await.unwrap_err();
    assert!(matches!(err, LoopError::Halted), "{err:?}");
    assert_eq!(status(&f.store, f.run).await, RunStatus::Running);
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!((snap.next, snap.exit), (Next::Model, None));
    let out = resume_with(&f, &plain, ResumeChange::default())
        .await
        .unwrap();
    assert_eq!(out, paused("provider"));
    assert_eq!(f.model.calls().len(), 1, "модель вызвана при реплее");
    f.model.push(answer("ок"));
    let out = resume_with(&f, &plain, ResumeChange::default())
        .await
        .unwrap();
    assert_eq!(out, done("ок"));
}

/// 5.2: квитанция `interrupted:pause` без снимка — реплей паузу не повторяет.
#[tokio::test]
async fn pause_receipt_without_snapshot_resumes_once() {
    let mut f = fx("pause_receipt");
    f.model
        .push_open(vec![Ok(StreamEvent::TextDelta("a".into()))]);
    let plain = f.deps.clone();
    f.deps.crash = crash_at(CrashPoint::AfterModelEffect { step: 1 });
    let mut bus = f.bus.subscribe();
    let (h, ctl) = spawn_start(&f, ask());
    nth_of(&mut bus, EventKind::ModelDelta, 1, |_| true).await;
    ctl.pause();
    let r = within("цикл после паузы", h).await.unwrap();
    assert!(matches!(r, Err(LoopError::Halted)), "{r:?}");
    assert_eq!(status(&f.store, f.run).await, RunStatus::Running);
    let evs = events(&f.store, f.run).await;
    assert!(
        of_kind(&evs, EventKind::EffectReceipt)
            .iter()
            .any(|e| e.payload.to_string().contains("interrupted:pause")),
        "нет квитанции interrupted:pause"
    );
    f.model.push(answer("ок"));
    let out = resume_with(&f, &plain, ResumeChange::default())
        .await
        .unwrap();
    assert_eq!(out, done("ок"));
    let st = statuses(&f.store, f.run).await;
    assert!(
        !st.contains(&RunStatus::Paused) && !st.contains(&RunStatus::Pausing),
        "{st:?}"
    );
    assert_eq!(f.model.calls().len(), 2);
    let evs = events(&f.store, f.run).await;
    let reqs = of_kind(&evs, EventKind::ModelRequest);
    assert_eq!(
        reqs.last().unwrap().payload["effect_key"],
        // Эффекты шага 2: boundary, context (R1-B2 D42), model.
        EffectKey::derive(f.run, 2, 2).0
    );
}

/// 5.3: снимок выхода записан, статус нет — resume дописывает статус без шагов.
#[tokio::test]
async fn exit_snapshot_without_status() {
    let mut f = fx("exit_no_status");
    f.model.push(server_error());
    let plain = f.deps.clone();
    f.deps.crash = crash_at(CrashPoint::AfterExitSnapshot { step: 1 });
    let err = start(&f, ask()).await.unwrap_err();
    assert!(matches!(err, LoopError::Halted), "{err:?}");
    assert_eq!(status(&f.store, f.run).await, RunStatus::Running);
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!(
        snap.exit,
        Some(PendingExit::Paused {
            reason: "provider".into()
        })
    );
    let out = resume_with(&f, &plain, ResumeChange::default())
        .await
        .unwrap();
    assert_eq!(out, paused("provider"));
    assert_eq!(status(&f.store, f.run).await, RunStatus::Paused);
    assert_eq!(f.model.calls().len(), 1);
    f.model.push(answer("ок"));
    let out = resume_with(&f, &plain, ResumeChange::default())
        .await
        .unwrap();
    assert_eq!(out, done("ок"));
}

/// D5: граница `step + 1` начата — `exit` снимка устарел и не применяется.
#[tokio::test]
async fn stale_exit_ignored_after_lifted_pause() {
    let f = fx("stale_exit");
    f.model.push(server_error());
    assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
    let mut crashing = f.deps.clone();
    crashing.crash = crash_at(CrashPoint::AfterBoundary { step: 2 });
    let err = resume_with(&f, &crashing, ResumeChange::default())
        .await
        .unwrap_err();
    assert!(matches!(err, LoopError::Halted), "{err:?}");
    assert_eq!(status(&f.store, f.run).await, RunStatus::Running);
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!((snap.step, snap.next), (1, Next::Boundary));
    assert!(snap.exit.is_some());
    assert!(f.store.step_has_effects(f.run, 2).await.unwrap());
    f.model.push(answer("ок"));
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    assert_eq!(f.model.calls().len(), 2);
}

/// D26: упавший в `cancelling` run доводится до `cancelled` без шагов.
#[tokio::test]
async fn cancelling_after_crash_finishes() {
    let f = fx("cancelling");
    f.model.push_open(Vec::new());
    let mut bus = f.bus.subscribe();
    let (h, _ctl) = spawn_start(&f, ask());
    nth_of(&mut bus, EventKind::ModelRequest, 1, |_| true).await;
    crash(h).await;
    let g = f.store.get_run(f.run).await.unwrap().unwrap().generation;
    f.store
        .set_status(f.run, RunStatus::Cancelling, g)
        .await
        .unwrap();
    let out = resume(&f, ResumeChange::default()).await.unwrap();
    assert_eq!(out, LoopOutcome::Cancelled);
    assert_eq!(
        statuses(&f.store, f.run).await.last(),
        Some(&RunStatus::Cancelled)
    );
    assert_eq!(f.model.finished_runs(), vec![f.run]);
    assert_eq!(f.model.calls().len(), 1);
}

/// D11: другой префикс посреди шага (новый бинарник) — пауза, а не `failed`.
#[tokio::test]
async fn replay_mismatch_pauses_not_fails() {
    let f = fx("replay_mismatch");
    f.model.push_open(Vec::new());
    let mut bus = f.bus.subscribe();
    let (h, _ctl) = spawn_start(&f, ask());
    nth_of(&mut bus, EventKind::ModelRequest, 1, |_| true).await;
    crash(h).await;
    let (seq, mut snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!(snap.next, Next::Model);
    snap.frozen.system.push_str("\nnew binary");
    let last = events(&f.store, f.run).await.last().unwrap().seq;
    assert!(last > seq);
    f.store
        .save_snapshot(f.run, last, serde_json::to_value(&snap).unwrap())
        .await
        .unwrap();
    let out = resume(&f, ResumeChange::default()).await.unwrap();
    assert_eq!(out, paused("replay_mismatch"));
    assert_eq!(status(&f.store, f.run).await, RunStatus::Paused);
    assert_eq!(f.model.calls().len(), 1);
}

/// D4: resume без снимка — ошибка и `failed`.
#[tokio::test]
async fn resume_without_snapshot_fails() {
    let f = fx("no_snapshot");
    f.store
        .create_run(NewRun {
            id: f.run,
            project_id: "p".into(),
            parent_id: None,
            role: RunRole::Top,
            model_profile: None,
        })
        .await
        .unwrap();
    f.store
        .set_status(f.run, RunStatus::Running, 0)
        .await
        .unwrap();
    let err = resume(&f, ResumeChange::default()).await.unwrap_err();
    assert!(
        matches!(&err, LoopError::Snapshot(s) if s == "нет снимка"),
        "{err:?}"
    );
    assert_eq!(status(&f.store, f.run).await, RunStatus::Failed);
}

/// D6: после паузы провайдера — другая модель; префикс и тир прежние.
#[tokio::test]
async fn resume_after_provider_pause_switches_model() {
    let f = fx("switch_model");
    f.model.push(server_error());
    assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
    let (_, before) = last_snapshot(&f.store, f.run).await;
    f.model.push(answer("ок"));
    let change = ResumeChange {
        endpoint: Some(EndpointId("b".into())),
        ..ResumeChange::default()
    };
    assert_eq!(resume(&f, change).await.unwrap(), done("ок"));
    let cs = f.model.calls();
    assert_eq!(cs.len(), 2);
    assert_eq!(cs[1].endpoint, EndpointId("b".into()));
    assert!(
        user_texts(&cs[1].request.messages)
            .iter()
            .any(|t| t.starts_with("<control from=\"runtime\" action=\"resume\"")),
        "нет сообщения resume"
    );
    let (_, after) = last_snapshot(&f.store, f.run).await;
    assert_eq!(after.setup.endpoint, EndpointId("b".into()));
    assert_eq!(after.frozen, before.frozen);
    assert_eq!(after.frozen_ref, before.frozen_ref);
    assert_eq!(after.setup.tier, before.setup.tier);
}

/// D6: смена модели посреди шага отвергается.
#[tokio::test]
async fn model_change_mid_step_rejected() {
    let f = fx("change_mid_step");
    f.model.push_open(Vec::new());
    let mut bus = f.bus.subscribe();
    let (h, _ctl) = spawn_start(&f, ask());
    nth_of(&mut bus, EventKind::ModelRequest, 1, |_| true).await;
    crash(h).await;
    let change = ResumeChange {
        endpoint: Some(EndpointId("b".into())),
        ..ResumeChange::default()
    };
    let err = resume(&f, change).await.unwrap_err();
    assert!(matches!(err, LoopError::ChangeMidStep), "{err:?}");
    assert_eq!(status(&f.store, f.run).await, RunStatus::Running);
    assert_eq!(f.model.calls().len(), 1);
}

/// D16: одобренный вызов исполняется при resume ровно один раз.
#[tokio::test]
async fn resume_after_approval_executes_call() {
    let f = fx("approval");
    f.model.push(calls(&[("c1", "incr")]));
    let out = start(&f, ask()).await.unwrap();
    let LoopOutcome::Paused {
        reason,
        interrupt: Some(id),
    } = out
    else {
        panic!("ожидалась пауза needs_approval: {out:?}");
    };
    assert_eq!(reason, "needs_approval");
    assert_eq!(f.incr.load(Ordering::SeqCst), 0);
    assert!(
        f.store
            .resolve_interrupt(&id, json!({"approved": true}))
            .await
            .unwrap()
    );
    f.model.push(answer("ок"));
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    assert_eq!(f.incr.load(Ordering::SeqCst), 1);
}

/// D26: упавший в `pausing` run — `paused (command)`, затем `running`.
#[tokio::test]
async fn resume_from_pausing_after_crash() {
    let mut f = fx("pausing");
    f.model
        .push_open(vec![Ok(StreamEvent::TextDelta("a".into()))]);
    let plain = f.deps.clone();
    f.deps.crash = crash_at(CrashPoint::AfterExitSnapshot { step: 1 });
    let mut bus = f.bus.subscribe();
    let (h, ctl) = spawn_start(&f, ask());
    nth_of(&mut bus, EventKind::ModelDelta, 1, |_| true).await;
    ctl.pause();
    let r = within("цикл после паузы", h).await.unwrap();
    assert!(matches!(r, Err(LoopError::Halted)), "{r:?}");
    assert_eq!(status(&f.store, f.run).await, RunStatus::Pausing);
    let before = statuses(&f.store, f.run).await.len();
    f.model.push(answer("ок"));
    let out = resume_with(&f, &plain, ResumeChange::default())
        .await
        .unwrap();
    assert_eq!(out, done("ок"));
    let evs = events(&f.store, f.run).await;
    let st = of_kind(&evs, EventKind::RunStatus);
    let tail: Vec<(Value, Value)> = st[before..]
        .iter()
        .map(|e| (e.payload["to"].clone(), e.payload["reason"].clone()))
        .collect();
    assert_eq!(
        tail,
        [
            (json!("paused"), json!("command")),
            (json!("running"), Value::Null),
            (json!("done"), Value::Null),
        ]
    );
}

#[tokio::test]
async fn done_run_is_not_resumable() {
    let f = fx("done");
    f.model.push(answer("ок"));
    assert_eq!(start(&f, ask()).await.unwrap(), done("ок"));
    let err = resume(&f, ResumeChange::default()).await.unwrap_err();
    assert!(
        matches!(err, LoopError::NotResumable(RunStatus::Done)),
        "{err:?}"
    );
}

/// D19, D32: новый бюджет; ввод, записанный во время хода 2, читается ровно один раз.
#[tokio::test]
async fn resume_with_more_budget() {
    let f = fx("more_budget");
    f.model.push(calls(&[("c1", "echo")]));
    f.model.push(calls(&[("c2", "hang")]));
    let budget = Budget {
        steps: Some(2),
        ..Budget::default()
    };
    let mut bus = f.bus.subscribe();
    let (h, _ctl) = spawn_start(&f, spec(RunMode::Ask, budget));
    nth_of(&mut bus, EventKind::ToolStarted, 1, |p| p["name"] == "hang").await;
    za_agent::submit_input(&f.store, f.run, "ещё", za_agent::InputMode::Queue)
        .await
        .unwrap();
    f.release.add_permits(1);
    let out = within("цикл до бюджета", h).await.unwrap().unwrap();
    assert_eq!(
        out,
        LoopOutcome::BlockedOnBudget {
            kind: BudgetKind::Steps
        }
    );
    // Снимок выхода — на границе 3 (k + 1, D32), шаг 3 начат только эффектом boundary.
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!((snap.step, snap.next), (3, Next::Boundary));
    let before = statuses(&f.store, f.run).await.len();
    f.model.push(answer("ок"));
    let change = ResumeChange {
        budget: Some(Budget {
            steps: Some(10),
            ..Budget::default()
        }),
        ..ResumeChange::default()
    };
    assert_eq!(resume(&f, change).await.unwrap(), done("ок"));
    let st = statuses(&f.store, f.run).await;
    assert_eq!(
        st[before - 1..],
        [
            RunStatus::BlockedOnBudget,
            RunStatus::Running,
            RunStatus::Done
        ]
    );
    let cs = f.model.calls();
    assert_eq!(cs.len(), 3);
    let n = user_texts(&cs[2].request.messages)
        .iter()
        .filter(|t| *t == "ещё")
        .count();
    assert_eq!(n, 1);
    // Шаг 3 не повторён и не пропущен: следующий запрос — шаг 4.
    let evs = events(&f.store, f.run).await;
    assert_eq!(
        of_kind(&evs, EventKind::ModelRequest)
            .last()
            .unwrap()
            .payload["effect_key"],
        // Эффекты шага 4: boundary, memory.prompt ввода (R1-M D142), state, context (R1-B2
        // D42), model.
        EffectKey::derive(f.run, 4, 4).0
    );
}

/// A.6: ввод при resume и блок `<state>` после него.
#[tokio::test]
async fn resume_with_input_continues_paused_run() {
    let f = fx("with_input");
    f.model
        .push_open(vec![Ok(StreamEvent::TextDelta("a".into()))]);
    let mut bus = f.bus.subscribe();
    let (h, ctl) = spawn_start(&f, ask());
    nth_of(&mut bus, EventKind::ModelDelta, 1, |_| true).await;
    ctl.pause();
    let out = within("цикл после паузы", h).await.unwrap().unwrap();
    assert_eq!(out, paused("command"));
    f.model.push(answer("ок"));
    let change = ResumeChange {
        input: Some("продолжи".into()),
        ..ResumeChange::default()
    };
    assert_eq!(resume(&f, change).await.unwrap(), done("ок"));
    let cs = f.model.calls();
    assert_eq!(cs.len(), 2);
    let users = user_texts(&cs[1].request.messages);
    let input = users
        .iter()
        .position(|t| t == "продолжи")
        .expect("ввода нет в запросе");
    let state = users
        .iter()
        .position(|t| t.contains("<state>\n"))
        .expect("блока <state> нет в запросе");
    assert!(state > input, "{users:?}");
    assert_eq!(
        users[state],
        "<control from=\"runtime\" action=\"resume\" command=\"\">\n<state>\nsteps done: 1; \
         last pause: command\nunknown outcomes: none\nchanges made: none\n</state>\n</control>"
    );
}

// ---- Дополнительно ----

/// I2 ревью задачи 15: пауза, посланная на канал до `resume_run`, — новая команда:
/// первая граница ставит run в `paused (command)`.
#[tokio::test]
async fn pause_before_resume_pauses() {
    let f = fx("pause_before_resume");
    f.model.push(server_error());
    assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
    let before = statuses(&f.store, f.run).await.len();
    let (ctl, rx) = control_channel(f.store.clone(), f.run);
    ctl.pause();
    let out = within(
        "resume_run",
        resume_run(&f.deps, f.run, ResumeChange::default(), rx),
    )
    .await
    .unwrap();
    assert_eq!(out, paused("command"));
    let st = statuses(&f.store, f.run).await;
    assert_eq!(
        st[before..],
        [RunStatus::Running, RunStatus::Pausing, RunStatus::Paused]
    );
    assert_eq!(f.model.calls().len(), 1, "модель вызвана после паузы");
}

/// I1 ревью задачи 15: реплей границы из журнала не берёт `now_ms` прошлого процесса.
#[tokio::test]
async fn replayed_boundary_does_not_count_idle_time() {
    let mut f = fx("replayed_boundary");
    f.model.push(calls(&[("c1", "echo")]));
    let plain = f.deps.clone();
    f.deps.crash = crash_at(CrashPoint::AfterBoundary { step: 2 });
    let err = start(&f, ask()).await.unwrap_err();
    assert!(matches!(err, LoopError::Halted), "{err:?}");
    assert!(f.store.step_has_effects(f.run, 2).await.unwrap());
    f.model.push(answer("ок"));
    let out = resume_with(&f, &plain, ResumeChange::default())
        .await
        .unwrap();
    assert_eq!(out, done("ок"));
    let snaps = f.store.snapshots(f.run).await.unwrap();
    let model2: Snapshot = snaps
        .into_iter()
        .map(|(_, v)| serde_json::from_value::<Snapshot>(v).unwrap())
        .find(|s| s.step == 2 && s.next == Next::Model)
        .expect("нет снимка Model шага 2");
    assert_eq!(model2.counters.last_boundary_ms, None);
    assert_eq!(model2.counters.active_ms, 0);
}

/// M6 ревью задачи 15: resume со снимка `Model` обнуляет `last_boundary_ms` — время
/// между процессами не идёт в бюджет `duration_ms`.
#[tokio::test]
async fn resume_mid_step_resets_last_boundary() {
    let f = fx("reset_last_boundary");
    f.model.push_open(Vec::new());
    let mut bus = f.bus.subscribe();
    let (h, _ctl) = spawn_start(&f, ask());
    nth_of(&mut bus, EventKind::ModelRequest, 1, |_| true).await;
    crash(h).await;
    let (seq, mut snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!(snap.next, Next::Model);
    // Граница «в далёком прошлом»: без обнуления следующая граница засчитала бы всё
    // время с 1 мс эпохи.
    snap.counters.last_boundary_ms = Some(1);
    snap.counters.active_ms = 0;
    snap.budget = Budget {
        duration_ms: Some(60_000),
        ..Budget::default()
    };
    let last = events(&f.store, f.run).await.last().unwrap().seq;
    assert!(last > seq);
    f.store
        .save_snapshot(f.run, last, serde_json::to_value(&snap).unwrap())
        .await
        .unwrap();
    f.model.push(calls(&[("c1", "echo")]));
    f.model.push(answer("ок"));
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert!(snap.counters.active_ms < 60_000, "{:?}", snap.counters);
}

/// M2 ревью задачи 15: испорченный снимок или его история — `failed (snapshot)`;
/// снимок другой версии статус не меняет.
#[tokio::test]
async fn broken_snapshot_fails_other_version_does_not() {
    async fn paused_run(name: &str) -> (Fx, Seq, Snapshot) {
        let f = fx(name);
        f.model.push(server_error());
        assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
        let (seq, snap) = last_snapshot(&f.store, f.run).await;
        let last = events(&f.store, f.run).await.last().unwrap().seq;
        assert!(last > seq);
        (f, last, snap)
    }
    async fn last_reason(f: &Fx) -> Value {
        let evs = events(&f.store, f.run).await;
        of_kind(&evs, EventKind::RunStatus).last().unwrap().payload["reason"].clone()
    }

    // Снимок версии 1, который не разбирается.
    let (f, last, _) = paused_run("broken_snapshot").await;
    f.store
        .save_snapshot(f.run, last, json!({"v": 1, "step": "x"}))
        .await
        .unwrap();
    let err = resume(&f, ResumeChange::default()).await.unwrap_err();
    assert!(matches!(err, LoopError::Snapshot(_)), "{err:?}");
    assert_eq!(status(&f.store, f.run).await, RunStatus::Failed);
    assert_eq!(last_reason(&f).await, json!("snapshot"));

    // История ссылается на блоб, которого нет.
    let (f, last, mut snap) = paused_run("missing_blob").await;
    snap.messages.push(za_protocol::BlobHash("0".repeat(64)));
    f.store
        .save_snapshot(f.run, last, serde_json::to_value(&snap).unwrap())
        .await
        .unwrap();
    let err = resume(&f, ResumeChange::default()).await.unwrap_err();
    assert!(matches!(err, LoopError::Snapshot(_)), "{err:?}");
    assert_eq!(status(&f.store, f.run).await, RunStatus::Failed);
    assert_eq!(last_reason(&f).await, json!("snapshot"));

    // Снимок другой версии: статус не меняется, его может прочитать новая версия.
    let (f, last, _) = paused_run("other_version").await;
    f.store
        .save_snapshot(f.run, last, json!({"v": 6}))
        .await
        .unwrap();
    let err = resume(&f, ResumeChange::default()).await.unwrap_err();
    assert!(
        matches!(&err, LoopError::Snapshot(s) if s == "версия снимка 6"),
        "{err:?}"
    );
    assert_eq!(status(&f.store, f.run).await, RunStatus::Paused);
}

// ---- R1-B2, задача 1: снимок версии 2 (D66) и `[context]` на run (D65) ----

/// Снимок версии 2 → снимок R1-B1: `v = 1`, без полей D66 и `setup.context`.
fn as_v1(snap: &Snapshot) -> Value {
    let mut v = serde_json::to_value(snap).unwrap();
    let o = v.as_object_mut().unwrap();
    o.insert("v".into(), json!(1));
    for k in ["meter", "task", "env_text", "user_inputs"] {
        assert!(o.remove(k).is_some(), "в снимке v2 нет поля {k}");
    }
    o["setup"].as_object_mut().unwrap().remove("context");
    v
}

/// D66: снимок v1 с `next = boundary` продолжается; `task`, `env_text` и
/// `user_inputs` восстанавливаются разбором истории.
#[tokio::test]
async fn snapshot_v1_boundary_continues() {
    let f = fx("v1_boundary");
    f.model.push(server_error());
    assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
    // Ввод пользователя в истории: resume с вводом, снова ошибка провайдера.
    f.model.push(server_error());
    let change = ResumeChange {
        input: Some("ещё <раз>".into()),
        ..ResumeChange::default()
    };
    assert_eq!(resume(&f, change).await.unwrap(), paused("provider"));
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!(snap.v, za_agent::SNAPSHOT_V);
    assert_eq!(snap.next, Next::Boundary);
    assert_eq!(snap.user_inputs.len(), 1);
    let v1 = as_v1(&snap);
    let last = events(&f.store, f.run).await.last().unwrap().seq;
    f.store.save_snapshot(f.run, last, v1).await.unwrap();

    f.model.push(answer("ок"));
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    let (_, after) = last_snapshot(&f.store, f.run).await;
    assert_eq!(after.v, za_agent::SNAPSHOT_V);
    assert_eq!(after.task, "Задача");
    assert!(after.env_text.starts_with("<env>\n"), "{}", after.env_text);
    assert!(after.env_text.ends_with("\n</env>"), "{}", after.env_text);
    assert_eq!(after.setup.context, ContextConfig::default());
    // Ввод восстановлен из истории: экранированный текст, индекс сообщения.
    assert_eq!(after.user_inputs.len(), 1, "{:?}", after.user_inputs);
    let u = &after.user_inputs[0];
    assert!(!u.correct);
    assert_eq!(u.at, snap.user_inputs[0].at);
    let text = f.store.get_blob(&u.text).await.unwrap().unwrap();
    assert_eq!(String::from_utf8(text).unwrap(), "ещё &lt;раз&gt;");
    assert_eq!(u, &snap.user_inputs[0]);
}

/// Журнал run, записанный кодом R1-B1 (2b30f84, генератор — раунд 1 задачи 1): копия
/// фикстуры `tests/golden/r1b1_v1/<name>` в каталог теста, run и снимок v1.
async fn golden_v1(name: &str) -> (Fx, Value) {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/r1b1_v1")
        .join(name);
    let d = dir(&format!("golden_{name}"));
    std::fs::copy(src.join("store/journal.db"), d.join("journal.db")).unwrap();
    let mut f = fx_in(&d);
    let run = std::fs::read_to_string(src.join("run.txt")).unwrap();
    f.run = serde_json::from_value(json!(run.trim())).unwrap();
    let golden: Value =
        serde_json::from_str(&std::fs::read_to_string(src.join("snapshot.json")).unwrap()).unwrap();
    // Фикстура — настоящий снимок R1-B1: версия 1, посреди шага, без полей R1-B2.
    let (_, v) = f.store.latest_snapshot(f.run).await.unwrap().unwrap();
    assert_eq!(v, golden, "снимок журнала не совпадает с golden");
    assert_eq!(golden["v"], 1);
    assert_eq!(golden["next"], "model");
    assert!(golden.get("meter").is_none() && golden["setup"].get("context").is_none());
    (f, golden)
}

/// I2 ревью задачи 1 (D66): run R1-B1, упавший посреди хода, продолжается: шаг
/// доигрывается с параметрами хода R1-B1 (тот же `request_hash`, тот же ключ эффекта).
#[tokio::test]
async fn snapshot_v1_mid_step_continues() {
    let (f, _) = golden_v1("mid_step").await;
    assert_eq!(status(&f.store, f.run).await, RunStatus::Running);
    f.model.push(answer("ок"));
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    let cs = f.model.calls();
    assert_eq!(cs.len(), 1);
    // Параметры хода R1-B1: без лимита вывода.
    assert_eq!(cs[0].request.max_output_tokens, None);
    let evs = events(&f.store, f.run).await;
    let reqs = of_kind(&evs, EventKind::ModelRequest);
    // Запрос R1-B1 и повтор после сбоя — один эффект, один `request_hash`.
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].payload["effect_key"], reqs[1].payload["effect_key"]);
    assert_eq!(reqs[0].payload["request"], reqs[1].payload["request"]);
    assert_eq!(
        reqs[1].payload["effect_key"],
        EffectKey::derive(f.run, 1, 1).0
    );
    let (_, after) = last_snapshot(&f.store, f.run).await;
    assert_eq!(after.v, za_agent::SNAPSHOT_V);
    assert_eq!(after.task, "Задача");
    // PRM-010 (ревью задачи 5): run R1-B1 продолжает с версией своего префикса.
    for r in &reqs {
        assert_eq!(r.payload["prompt_version"], "r1b.1");
    }
}

/// I2 ревью задачи 1 (D66): run R1-B1 на паузе одобрения после обновления продолжается
/// после ответа: квитанция хода R1-B1 реплеится, вызов исполняется один раз, следующий
/// ход — с параметрами R1-B2.
#[tokio::test]
async fn snapshot_v1_approval_continues() {
    let (f, _) = golden_v1("approval").await;
    assert_eq!(status(&f.store, f.run).await, RunStatus::Paused);
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/r1b1_v1/approval");
    let id = std::fs::read_to_string(src.join("interrupt.txt")).unwrap();
    assert!(
        f.store
            .resolve_interrupt(id.trim(), json!({"approved": true}))
            .await
            .unwrap()
    );
    f.model.push(answer("ок"));
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    assert_eq!(f.incr.load(Ordering::SeqCst), 1);
    let cs = f.model.calls();
    assert_eq!(cs.len(), 1, "ход R1-B1 вызван заново");
    assert!(cs[0].request.max_output_tokens.is_some());
    let (_, after) = last_snapshot(&f.store, f.run).await;
    assert_eq!(after.v, za_agent::SNAPSHOT_V);
    // Новый ход после обновления — с версией замороженного префикса R1-B1 (PRM-010).
    let evs = events(&f.store, f.run).await;
    let reqs = of_kind(&evs, EventKind::ModelRequest);
    assert!(!reqs.is_empty());
    for r in &reqs {
        assert_eq!(r.payload["prompt_version"], "r1b.1");
    }
}

/// D65: `[context]` замораживается на run: снимок хранит `setup.context`, resume
/// берёт её из снимка.
#[tokio::test]
async fn context_config_frozen_on_run() {
    let f = fx("context_frozen");
    let mut s = ask();
    s.context.compact_at = 0.70;
    f.model.push(server_error());
    assert_eq!(start(&f, s).await.unwrap(), paused("provider"));
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!(snap.setup.context.compact_at, 0.70);
    f.model.push(answer("ок"));
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    let (_, after) = last_snapshot(&f.store, f.run).await;
    assert_eq!(after.setup.context.compact_at, 0.70);
}

/// D43: resume на эндпоинт без размера контекста — 8192 и `context.alert`.
#[tokio::test]
async fn resume_to_unknown_window_alerts() {
    let f = fx("resume_unknown_window");
    f.model.push(server_error());
    assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
    let before = of_kind(&events(&f.store, f.run).await, EventKind::ContextAlert)
        .iter()
        .filter(|e| e.payload["kind"] == "n_ctx_unknown")
        .count();
    f.model.push(answer("ок"));
    let change = ResumeChange {
        endpoint: Some(EndpointId("b".into())),
        ..ResumeChange::default()
    };
    assert_eq!(resume(&f, change).await.unwrap(), done("ок"));
    let evs = events(&f.store, f.run).await;
    let alerts: Vec<_> = of_kind(&evs, EventKind::ContextAlert)
        .into_iter()
        .filter(|e| e.payload["kind"] == "n_ctx_unknown")
        .collect();
    assert_eq!(alerts.len(), before + 1);
    assert_eq!(alerts.last().unwrap().payload["endpoint"], "b");
    let (_, after) = last_snapshot(&f.store, f.run).await;
    assert_eq!(after.setup.n_ctx, 8192);
}

// ---- Финальное ревью R1-B2, I1: `turn.finished` при реплее после паузы посреди шага ----

/// Ход с одним вызовом `name`; аргументы — JSON строкой: нормализация исправляет их
/// (`ArgsParsedFromString`), у хода `repairs = 1`.
fn repaired_call(id: &str, name: &str, args: &str) -> Vec<Result<StreamEvent, ModelError>> {
    vec![Ok(StreamEvent::Finished {
        message: Message {
            role: Role::Assistant,
            parts: vec![Part::ToolCall(ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: Value::String(args.into()),
                raw_arguments: None,
            })],
        },
        finish: FinishReason::ToolCalls,
        usage: Usage::default(),
    })]
}

/// Число шагов в `<state>` и `repairs` в метриках — как у прогона без паузы: два хода,
/// одно исправление.
async fn assert_counted_once(f: &Fx) {
    let state = state_block(&f.store, f.run).await.unwrap();
    assert!(state.contains("steps done: 2;"), "{state}");
    let m = za_agent::run_metrics(&f.store, f.run).await.unwrap();
    assert_eq!(m.repairs, 1, "{m:?}");
    assert_eq!(m.turns, 2, "{m:?}");
    // Реплей пишет `turn.finished` повторно (at-least-once), ключ события тот же.
    let evs = events(&f.store, f.run).await;
    let fin = of_kind(&evs, EventKind::TurnFinished);
    assert!(fin.len() >= 3, "реплей не переписал событие: {fin:?}");
    let first = &fin[0].payload;
    assert_eq!(first["turn"], 1);
    assert_eq!(first["turn_attempt"], 0);
    assert!(first["step"].as_u64().is_some(), "{first}");
    assert_eq!(fin[1].payload["turn"], 1);
    assert_eq!(fin[1].payload["step"], first["step"]);
}

/// I1: `ask_user` → ответ → resume: ход 1 не считается дважды.
#[tokio::test]
async fn ask_user_resume_counts_turn_once() {
    let f = fx("i1_ask_user");
    f.model.push(repaired_call(
        "q1",
        "ask_user",
        "{\"question\": \"Какой файл?\"}",
    ));
    f.model.push(answer("ок"));
    let mut s = ask();
    s.tier = Some(Tier::Standard);
    let out = start(&f, s).await.unwrap();
    let LoopOutcome::Paused {
        interrupt: Some(id),
        ..
    } = out
    else {
        panic!("ожидалась пауза на ask_user: {out:?}");
    };
    assert!(
        f.store
            .resolve_interrupt(&id, json!({"answer": "a.rs"}))
            .await
            .unwrap()
    );
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    assert_counted_once(&f).await;
}

/// I1: одобрение → resume: ход 1 не считается дважды.
#[tokio::test]
async fn approval_resume_counts_turn_once() {
    let f = fx("i1_approval");
    f.model.push(repaired_call("c1", "incr", "{\"n\": 1}"));
    f.model.push(answer("ок"));
    let out = start(&f, ask()).await.unwrap();
    let LoopOutcome::Paused {
        interrupt: Some(id),
        ..
    } = out
    else {
        panic!("ожидалась пауза needs_approval: {out:?}");
    };
    assert!(
        f.store
            .resolve_interrupt(&id, json!({"approved": true}))
            .await
            .unwrap()
    );
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    assert_eq!(f.incr.load(Ordering::SeqCst), 1);
    assert_counted_once(&f).await;
}

// ---- R1-C2, задача 5: снимок v4 (D130) ----

/// Фикстура `golden/r1c1_v3.json` — снимок v3, записанный кодом R1-C1 до правок задачи 5
/// (генератор — `golden/r1c1_v3.generator.rs.txt`).
fn golden_v3() -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/r1c1_v3.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// Ключи объекта по порядку.
fn keys(v: &Value) -> Vec<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

/// D130: снимок v3 читается, экспозиция и скиллы — по умолчанию, прочие поля — как
/// были; resume снимка v3 продолжает run и пишет снимок v4.
#[tokio::test]
async fn snapshot_v3_reads_with_default_exposure() {
    // (1) Настоящий снимок R1-C1: версия 3, без полей R1-C2, поля R1-C1 заданы.
    let golden = golden_v3();
    assert_eq!(golden["v"], 3);
    assert!(golden.get("exposure").is_none() && golden.get("skills").is_none());
    assert_eq!(golden["taint"], json!(["untrusted_project"]));
    assert_eq!(golden["trusted"], json!(false));
    assert_eq!(golden["grants"].as_array().unwrap().len(), 1);
    assert!(golden["audit_head"].is_object());
    // (2) Разбор: новые части по умолчанию, остальное без потерь.
    let snap: Snapshot = serde_json::from_value(golden.clone()).unwrap();
    assert_eq!(snap.v, 3);
    assert_eq!(snap.exposure, za_agent::Exposure::default());
    assert!(snap.skills.is_empty());
    let mut back = serde_json::to_value(&snap).unwrap();
    let o = back.as_object_mut().unwrap();
    assert!(o.remove("exposure").is_some() && o.remove("skills").is_some());
    assert!(o.remove("memory").is_some());
    assert_eq!(back, golden, "поля снимка v3 потеряны при чтении");

    // (3) Resume: снимок той же формы, что фикстура, — живой run на паузе, сведённый к v3
    // (голова аудита задана, как в фикстуре: поле необязательное).
    let mut f = fx("v3_resume");
    f.deps.audit_head = Some(za_agent::AuditHead {
        seq: 7,
        hash: "ab".repeat(32),
    });
    f.model.push(server_error());
    assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
    let (seq, mut v) = f.store.latest_snapshot(f.run).await.unwrap().unwrap();
    assert_eq!(v["v"], 5);
    let o = v.as_object_mut().unwrap();
    o.insert("v".into(), json!(3));
    assert!(o.remove("exposure").is_some() && o.remove("skills").is_some());
    assert!(o.remove("memory").is_some());
    assert_eq!(
        keys(&v),
        keys(&golden),
        "форма снимка v3 разошлась с фикстурой"
    );
    let last = events(&f.store, f.run).await.last().unwrap().seq;
    assert!(last >= seq);
    f.store.save_snapshot(f.run, last, v).await.unwrap();
    f.model.push(answer("ок"));
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    let (_, after) = last_snapshot(&f.store, f.run).await;
    assert_eq!(after.v, za_agent::SNAPSHOT_V);
    assert_eq!(after.exposure, za_agent::Exposure::default());
    assert!(after.skills.is_empty());
}

/// m6 ревью задачи 5: снимок v3 недоверенного run (метка `untrusted_project`, флагов
/// экспозиции в нём нет) — после resume снимок начала пакета берёт `untrusted` по меткам
/// (`LoopState::exposure_now`), и после пакета флаг run установлен.
#[tokio::test]
async fn snapshot_v3_untrusted_label_resumes_untrusted() {
    let f = fx("v3_untrusted");
    f.model.push(server_error());
    assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
    let (_, mut v) = f.store.latest_snapshot(f.run).await.unwrap().unwrap();
    let o = v.as_object_mut().unwrap();
    o.insert("v".into(), json!(3));
    assert!(o.remove("exposure").is_some() && o.remove("skills").is_some());
    assert!(o.remove("memory").is_some());
    o.insert("taint".into(), json!(["untrusted_project"]));
    let last = events(&f.store, f.run).await.last().unwrap().seq;
    f.store.save_snapshot(f.run, last, v).await.unwrap();
    f.model.push(calls(&[("c1", "echo")]));
    f.model.push(answer("ок"));
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    let (_, after) = last_snapshot(&f.store, f.run).await;
    assert_eq!(after.v, za_agent::SNAPSHOT_V);
    assert_eq!(
        after.exposure,
        za_agent::Exposure {
            untrusted: true,
            ..za_agent::Exposure::default()
        }
    );
    // Снимок начала пакета уже нёс `untrusted` по меткам: флаги пакетом не изменены,
    // события `exposure.changed` нет (без `exposure_now` оно было бы).
    let evs = events(&f.store, f.run).await;
    assert!(
        !evs.iter().any(|e| e.kind == EventKind::ExposureChanged),
        "{:?}",
        evs.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>()
    );
}

// ---- R1-C2, задача 13: скиллы и снимок v4 (D118, D130) ----

/// Скилл источника `user` в каталоге теста: (каталог скиллов, каталог).
fn user_skill_catalog(name: &str, body: &str) -> (PathBuf, za_skills::Catalog) {
    let root = dir(name).join("home-skills");
    let p = root.join("tdd/SKILL.md");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(
        &p,
        format!("---\nname: tdd\ndescription: Use when testing.\n---\n{body}\n"),
    )
    .unwrap();
    let cat = za_skills::discover(&za_skills::DiscoverInput {
        user_dir: Some(root.clone()),
        ..za_skills::DiscoverInput::default()
    });
    assert_eq!(cat.skills.len(), 1, "{:?}", cat.skipped);
    (root, cat)
}

fn with_skills(deps: &mut LoopDeps, cat: za_skills::Catalog) {
    let cat = Arc::new(cat);
    deps.capabilities = vec![za_agent::skills_capability(Arc::clone(&cat))];
    deps.skills = Some(cat);
}

fn activate_call(id: &str) -> Vec<Result<StreamEvent, ModelError>> {
    vec![Ok(StreamEvent::Finished {
        message: Message {
            role: Role::Assistant,
            parts: vec![Part::ToolCall(ToolCall {
                id: id.into(),
                name: za_agent::ACTIVATE_SKILL_TOOL.into(),
                arguments: json!({"name": "tdd"}),
                raw_arguments: None,
            })],
        },
        finish: FinishReason::ToolCalls,
        usage: Usage::default(),
    })]
}

/// D118, D92: сбой после квитанции `skill.load` (граница следующего шага записана, снимка
/// `Model` нет); файл скилла удалён; resume реплеит шаг: тот же текст из квитанции, без
/// второго эффекта и второго `skill.activated`.
#[tokio::test]
async fn resume_replays_skill_load_from_receipt() {
    let (root, cat) = user_skill_catalog("skill_replay", "Original method.");
    let mut f = fx("skill_replay");
    with_skills(&mut f.deps, cat);
    f.deps.crash = crash_at(CrashPoint::AfterBoundary { step: 2 });
    let spec = RunSpec {
        tier: Some(Tier::Standard),
        ..ask()
    };
    f.model.push(activate_call("c1"));
    f.model.push(answer("ок"));
    assert!(matches!(start(&f, spec).await, Err(LoopError::Halted)));
    let evs = events(&f.store, f.run).await;
    let loads: Vec<&Event> = evs
        .iter()
        .filter(|e| e.kind == EventKind::EffectIntent && e.payload["name"] == "skill.load")
        .collect();
    assert_eq!(loads.len(), 1);
    // Текст первой попытки — квитанция эффекта (ход 2 до сбоя не начат).
    let receipt = evs
        .iter()
        .find(|e| e.kind == EventKind::EffectReceipt && e.payload["key"] == loads[0].payload["key"])
        .expect("нет квитанции skill.load");
    let value = &receipt.payload["result"]["value"];
    let first = (
        value["content"].as_str().unwrap().to_owned(),
        value["is_error"].as_bool().unwrap(),
    );
    assert!(!first.1);
    assert!(first.0.contains("Original method."), "{}", first.0);
    // Скилл удалён с диска: перечитать его нельзя, а каталог нового процесса (бинарник
    // обнаруживает скиллы заново) его уже не содержит — решает записанный эффект (D92).
    std::fs::remove_dir_all(root.join("tdd")).unwrap();
    let fresh = za_skills::discover(&za_skills::DiscoverInput {
        user_dir: Some(root.clone()),
        ..za_skills::DiscoverInput::default()
    });
    assert!(fresh.skills.is_empty());
    let mut deps = f.deps.clone();
    deps.crash = None;
    deps.skills = Some(Arc::new(fresh));
    f.model.push(answer("ок"));
    assert_eq!(
        resume_with(&f, &deps, ResumeChange::default())
            .await
            .unwrap(),
        done("ок")
    );
    let again = result_of(&f.model.calls().last().unwrap().request.messages, "c1").unwrap();
    assert_eq!(again, first, "реплей дал другой текст скилла");
    let evs = events(&f.store, f.run).await;
    assert_eq!(
        evs.iter()
            .filter(|e| e.kind == EventKind::EffectIntent && e.payload["name"] == "skill.load")
            .count(),
        1,
        "эффект skill.load повторён"
    );
    assert_eq!(of_kind(&evs, EventKind::SkillActivated).len(), 1);
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!(snap.skills.len(), 1);
    assert_eq!(snap.skills[0].call_id.as_deref(), Some("c1"));
}

/// Minor 4 финального ревью скиллов: скиллы выключены во время паузы (`[skills] enabled =
/// false` — бинарник не подключает capability и снимает `activate_skill` с исполнения,
/// `ToolRegistry::withdraw`), а описание `activate_skill` осталось в замороженном префиксе.
/// Записанный `skill.load` реплеится по квитанции до проверки реестра (D92): тот же текст,
/// без второго эффекта. Новый вызов получает понятный текст, а не `unknown tool`.
#[tokio::test]
async fn resume_with_skills_disabled_replays_and_withdraws() {
    let (_root, cat) = user_skill_catalog("skill_withdrawn", "Original method.");
    let mut f = fx("skill_withdrawn");
    with_skills(&mut f.deps, cat);
    f.deps.crash = crash_at(CrashPoint::AfterBoundary { step: 2 });
    let spec = RunSpec {
        tier: Some(Tier::Standard),
        ..ask()
    };
    // Сбой — на границе шага 2, до вызова модели: второго ответа в сценарии нет.
    f.model.push(activate_call("c1"));
    assert!(matches!(start(&f, spec).await, Err(LoopError::Halted)));
    let evs = events(&f.store, f.run).await;
    let key = evs
        .iter()
        .find(|e| e.kind == EventKind::EffectIntent && e.payload["name"] == "skill.load")
        .map(|e| e.payload["key"].clone())
        .expect("нет намерения skill.load");
    let receipt = evs
        .iter()
        .find(|e| e.kind == EventKind::EffectReceipt && e.payload["key"] == key)
        .expect("нет квитанции skill.load");
    let first = receipt.payload["result"]["value"]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(first.contains("Original method."), "{first}");
    // Скиллы выключены: capability нет, каталога исполнения нет, имя снято с исполнения.
    let mut deps = f.deps.clone();
    deps.crash = None;
    deps.capabilities = Vec::new();
    deps.skills = None;
    let mut reg = (*deps.tools).clone();
    reg.withdraw(
        za_agent::ACTIVATE_SKILL_TOOL,
        za_agent::skills_withdrawn_text(),
    );
    deps.tools = Arc::new(reg);
    // Другие аргументы: повтор того же вызова был бы повтором (D54), а не вызовом.
    let mut other = activate_call("c2");
    if let Ok(StreamEvent::Finished { message, .. }) = &mut other[0]
        && let Part::ToolCall(c) = &mut message.parts[0]
    {
        c.arguments = json!({"name": "review"});
    }
    f.model.push(other);
    f.model.push(answer("ок"));
    assert_eq!(
        resume_with(&f, &deps, ResumeChange::default())
            .await
            .unwrap(),
        done("ок")
    );
    let msgs = f.model.calls().last().unwrap().request.messages.clone();
    assert_eq!(
        result_of(&msgs, "c1"),
        Some((first, false)),
        "реплей квитанции"
    );
    assert_eq!(
        result_of(&msgs, "c2"),
        Some((
            "activate_skill: skills are disabled by configuration since this run started"
                .to_owned(),
            true
        ))
    );
    let evs = events(&f.store, f.run).await;
    assert_eq!(
        evs.iter()
            .filter(|e| e.kind == EventKind::EffectIntent && e.payload["name"] == "skill.load")
            .count(),
        1,
        "эффект skill.load повторён"
    );
}

/// Golden снимка v4 (D130): снимок живого run R1-C2 с активным скиллом и экспозицией.
/// С R1-M (снимок v5) — фикстура чтения v4: код ветки v4 больше не пишет.
fn golden_v4() -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/r1c2_v4/snapshot.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// D130: golden снимка v4 с активным скиллом и экспозицией читается без потерь полей;
/// состояние памяти R1-M (D140) — по умолчанию.
#[tokio::test]
async fn snapshot_v4_golden() {
    let golden = golden_v4();
    assert_eq!(golden["v"], 4);
    assert!(golden.get("memory").is_none());
    let snap: Snapshot = serde_json::from_value(golden.clone()).unwrap();
    assert_eq!(snap.v, 4);
    assert_eq!(snap.memory, za_agent::MemoryState::default());
    assert_eq!(snap.skills.len(), 1);
    let s = &snap.skills[0];
    assert_eq!(s.name, "tdd");
    assert_eq!(s.source, "bundled");
    assert_eq!(s.hash.len(), 64);
    assert_eq!(s.call_id.as_deref(), Some("c1"));
    assert_eq!(s.by, "model");
    assert_eq!(s.content.0.len(), 64);
    assert!(snap.exposure.untrusted);
    assert_eq!(snap.taint.iter().collect::<Vec<_>>(), ["untrusted_project"]);
    // Без потерь: разбор и запись дают тот же JSON и поле памяти R1-M.
    let mut back = serde_json::to_value(&snap).unwrap();
    assert!(back.as_object_mut().unwrap().remove("memory").is_some());
    assert_eq!(back, golden);
}

// ---- R1-M, задача 3: снимок v5 и шаги журналов R1-C2 (D140, D161) ----

/// Чтение `note`: результат `ok` (как в генераторе журналов R1-C2).
struct Note {
    spec: ToolSpec,
}

impl Tool for Note {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, _args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async { ToolOutput::ok("ok") })
    }
}

/// Чтение `fail`: результат-ошибка `fail {args}` (как в генераторе журналов R1-C2).
struct Fail {
    spec: ToolSpec,
}

impl Tool for Fail {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move { ToolOutput::error(format!("fail {args}")) })
    }
}

/// Фикстура над журналом в каталоге `d` с инструментами генератора журналов R1-C2:
/// `note`, `fail` и `incr` (мутация, счётчик — `Fx::incr`); окно 8192.
fn fx_r1c2(d: &std::path::Path) -> Fx {
    let mut f = fx_in(d);
    let model =
        Arc::new(ScriptedModel::new(default_caps(BackendKind::LlamaCpp)).with_context(8192));
    let mut reg = ToolRegistry::default();
    for t in builtin_tools() {
        reg.register(t).unwrap();
    }
    reg.register(Arc::new(Note {
        spec: tool_spec("note", ToolKind::Read, EffectClass::ReadOnly),
    }))
    .unwrap();
    reg.register(Arc::new(Fail {
        spec: tool_spec("fail", ToolKind::Read, EffectClass::ReadOnly),
    }))
    .unwrap();
    reg.register(Arc::new(Incr {
        spec: tool_spec("incr", ToolKind::Exec, EffectClass::NonIdempotent),
        count: Arc::clone(&f.incr),
    }))
    .unwrap();
    f.deps = LoopDeps {
        memory: Arc::new(NoMemory),
        ..LoopDeps::new(
            Journal::new(f.store.clone(), f.bus.clone()),
            model.clone(),
            Arc::new(reg),
        )
    };
    f.model = model;
    f
}

/// Журнал run, записанный кодом R1-C2 (генератор `golden/r1c2_journal/generator.rs.txt`,
/// D161): копия `store/journal.db` в каталог теста, run и снимок v4.
async fn golden_r1c2(name: &str) -> (Fx, Value) {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/r1c2_journal")
        .join(name);
    let d = dir(&format!("r1c2_{name}"));
    std::fs::copy(src.join("store/journal.db"), d.join("journal.db")).unwrap();
    let mut f = fx_r1c2(&d);
    let run = std::fs::read_to_string(src.join("run.txt")).unwrap();
    f.run = serde_json::from_value(json!(run.trim())).unwrap();
    let golden: Value =
        serde_json::from_str(&std::fs::read_to_string(src.join("snapshot.json")).unwrap()).unwrap();
    let (_, v) = f.store.latest_snapshot(f.run).await.unwrap().unwrap();
    assert_eq!(v, golden, "снимок журнала не совпадает с golden");
    assert_eq!(golden["v"], 4);
    assert!(golden.get("memory").is_none());
    (f, golden)
}

/// Ход с вызовами `fail {n}` и `note {n}`: ошибка чтения и успешное чтение (успех сбрасывает
/// счёт ошибок чтения, D57 — без пауз в тесте).
fn fail_note(n: u32) -> Vec<Result<StreamEvent, ModelError>> {
    vec![Ok(StreamEvent::Finished {
        message: Message {
            role: Role::Assistant,
            parts: ["fail", "note"]
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    Part::ToolCall(ToolCall {
                        id: format!("n{n}c{i}"),
                        name: (*name).into(),
                        arguments: json!({"n": n}),
                        raw_arguments: None,
                    })
                })
                .collect(),
        },
        finish: FinishReason::ToolCalls,
        usage: Usage::default(),
    })]
}

/// `(шаг, номер, имя)` намерений эффектов по порядку.
fn effect_names(evs: &[Event]) -> Vec<(u64, u64, String)> {
    evs.iter()
        .filter(|e| e.kind == EventKind::EffectIntent)
        .map(|e| {
            let key = e.payload["key"].as_str().unwrap();
            let mut it = key.rsplit(':');
            let seq = it.next().unwrap().parse().unwrap();
            let step = it.next().unwrap().parse().unwrap();
            (step, seq, e.payload["name"].as_str().unwrap().to_owned())
        })
        .collect()
}

/// Эффекты памяти R1-M, которых нет в шагах кода R1-C2 (D161).
const R1M_MEMORY_EFFECTS: [&str; 3] = ["memory.prompt", "memory.retry", "memory.tool_error"];

/// Шаги эффектов памяти R1-M в журнале.
fn r1m_memory_steps(names: &[(u64, u64, String)]) -> Vec<u64> {
    names
        .iter()
        .filter(|(_, _, n)| R1M_MEMORY_EFFECTS.contains(&n.as_str()))
        .map(|(s, _, _)| *s)
        .collect()
}

/// D161, Review Focus 3: каждый журнал R1-C2 продолжается под R1-M до конца без
/// `ReplayMismatch`; эффекты R1-C2 не переписаны (каждый ключ — одно намерение, имена
/// прежние); эффекты памяти R1-M появляются только с шага `legacy_until + 1`.
#[tokio::test]
async fn r1c2_journals_resume_without_mismatch() {
    // (журнал, legacy_until, живых ходов с ошибкой инструмента до ответа): у `compact_mid` и
    // `inbox_mid` граница шага 2 записана кодом R1-C2 при снимке `Model` шага 1, ход шага 2
    // идёт вживую; у `batch_error` ход шага 2 — из квитанции; у `approval` шаг 2 не начат.
    for (name, until, live) in [
        ("compact_mid", 2, 2),
        ("inbox_mid", 2, 2),
        ("batch_error", 2, 1),
        ("approval", 1, 1),
    ] {
        let (f, _) = golden_r1c2(name).await;
        let before = effect_names(&events(&f.store, f.run).await);
        if name == "approval" {
            let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/golden/r1c2_journal/approval");
            let id = std::fs::read_to_string(src.join("interrupt.txt")).unwrap();
            assert!(
                f.store
                    .resolve_interrupt(id.trim(), json!({"approved": true}))
                    .await
                    .unwrap()
            );
        }
        // Ходы после реплея: пакеты шагов до `until` — без новых эффектов, пакет шага
        // `until + 1` — с подсказкой `memory.tool_error`.
        for n in 0..live {
            f.model.push(fail_note(10 + n));
        }
        f.model.push(answer("готово"));
        let out = resume(&f, ResumeChange::default()).await;
        assert_eq!(out.unwrap(), done("готово"), "{name}");
        let evs = events(&f.store, f.run).await;
        let after = effect_names(&evs);
        // Эффекты R1-C2 на месте, ни один ключ не записан дважды.
        assert_eq!(&after[..before.len()], &before[..], "{name}");
        let mut keys: Vec<(u64, u64)> = after.iter().map(|(s, q, _)| (*s, *q)).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), after.len(), "{name}: ключ записан дважды");
        let steps = r1m_memory_steps(&after);
        assert!(steps.iter().all(|s| *s > until), "{name}: {after:?}");
        assert!(steps.contains(&(until + 1)), "{name}: {after:?}");
        let (_, snap) = last_snapshot(&f.store, f.run).await;
        assert_eq!(snap.v, za_agent::SNAPSHOT_V);
        assert_eq!(snap.memory.legacy_until, Some(until), "{name}");
        if name == "approval" {
            assert_eq!(f.incr.load(Ordering::SeqCst), 1);
        }
    }
}

/// D161: признак шагов R1-C2 живёт в снимке v5 — повторный сбой внутри того же шага его не
/// теряет: после второго resume пакет шага 2 по-прежнему без эффектов памяти.
#[tokio::test]
async fn legacy_step_survives_second_crash() {
    let (mut f, _) = golden_r1c2("inbox_mid").await;
    f.model.push(fail_note(5));
    let mut deps = f.deps.clone();
    deps.crash = crash_at(CrashPoint::AfterModelEffect { step: 2 });
    let err = resume_with(&f, &deps, ResumeChange::default()).await;
    assert!(matches!(err, Err(LoopError::Halted)), "{err:?}");
    // Снимок v5 шага 2 записан кодом R1-M внутри шага R1-C2.
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!((snap.v, snap.step, snap.next), (5, 2, Next::Model));
    assert_eq!(snap.memory.legacy_until, Some(2));
    f.deps.crash = None;
    f.model.push(answer("готово"));
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("готово")
    );
    let after = effect_names(&events(&f.store, f.run).await);
    assert!(r1m_memory_steps(&after).iter().all(|s| *s > 2), "{after:?}");
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!(snap.memory.legacy_until, Some(2));
}

/// D161: снимок v4 (форма golden `r1c2_v4`) продолжается: память — по умолчанию. У снимка
/// `Boundary`, следующий шаг которого не начат, `legacy_until = step` (Minor 3 ревью задачи
/// 3: слагаемое `next == Boundary` снято) — шаг `step + 1` целиком проходит код R1-M, и ввод
/// resume на нём получает подсказку памяти.
#[tokio::test]
async fn r1c2_v4_snapshot_resumes() {
    let golden = golden_v4();
    let snap: Snapshot = serde_json::from_value(golden.clone()).unwrap();
    assert_eq!(snap.memory, za_agent::MemoryState::default());
    // Живой run на паузе, сведённый к v4 (без поля памяти R1-M).
    let f = fx("v4_resume");
    f.model.push(server_error());
    assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
    let (seq, mut v) = f.store.latest_snapshot(f.run).await.unwrap().unwrap();
    assert_eq!(v["v"], 5);
    let step = v["step"].as_u64().unwrap();
    assert_eq!(v["next"], "boundary");
    let o = v.as_object_mut().unwrap();
    o.insert("v".into(), json!(4));
    assert!(o.remove("memory").is_some());
    assert_eq!(
        keys(&v),
        keys(&golden),
        "форма снимка v4 разошлась с golden"
    );
    let last = events(&f.store, f.run).await.last().unwrap().seq;
    assert!(last >= seq);
    f.store.save_snapshot(f.run, last, v).await.unwrap();
    f.model.push(answer("ок"));
    let change = ResumeChange {
        input: Some("ещё".into()),
        ..ResumeChange::default()
    };
    assert_eq!(resume(&f, change).await.unwrap(), done("ок"));
    let (_, after) = last_snapshot(&f.store, f.run).await;
    assert_eq!(after.v, 5);
    assert_eq!(after.memory.legacy_until, Some(step));
    let names = effect_names(&events(&f.store, f.run).await);
    let prompts: Vec<u64> = names
        .iter()
        .filter(|(_, _, n)| n == "memory.prompt")
        .map(|(s, _, _)| *s)
        .collect();
    // Подсказка к задаче шага 0 и к вводу шага `step + 1`.
    assert_eq!(prompts, [0, step + 1]);
}

/// Golden снимка v5 (`golden/r1m_v5.json`): состояние памяти run R1-M.
fn golden_v5_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/r1m_v5.json")
}

/// Снимок для golden v5: частичный дайджест с признаком идей `review` (повтор ждёт),
/// подсказка к задаче, пауза провайдера на шаге 1.
async fn write_golden_v5() {
    let mut f = fx("golden_v5_write");
    let mem = Arc::new(support::memory::ScriptedMemory::new(
        za_agent::MemoryLevel::L2,
    ));
    mem.push_snapshot(za_agent::RecallReply {
        recall: za_agent::Recall::Partial("### Задачи\n[in_progress] A — k:1".into()),
        ideas: Some(za_agent::IdeasSignal::Review),
    });
    mem.push_prompt(za_agent::Recall::Hits("hint i:2".into()));
    f.deps.memory = mem;
    f.model.push(server_error());
    assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
    let (_, v) = f.store.latest_snapshot(f.run).await.unwrap().unwrap();
    let mut text = serde_json::to_string_pretty(&v).unwrap();
    text.push('\n');
    std::fs::write(golden_v5_path(), text).unwrap();
}

/// D140, D169: golden снимка v5 читается без потерь; `ZA_UPDATE_GOLDEN=1` переписывает
/// его живым run.
#[tokio::test]
async fn snapshot_v5_golden() {
    if std::env::var("ZA_UPDATE_GOLDEN").as_deref() == Ok("1") {
        write_golden_v5().await;
    }
    let text = std::fs::read_to_string(golden_v5_path()).unwrap_or_else(|e| {
        panic!(
            "нет {}: {e}; эталон пишется командой ZA_UPDATE_GOLDEN=1 cargo test -p za-agent --test resume snapshot_v5_golden",
            golden_v5_path().display()
        )
    });
    let golden: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(golden["v"], 5);
    let snap: Snapshot = serde_json::from_value(golden.clone()).unwrap();
    assert_eq!(snap.v, za_agent::SNAPSHOT_V);
    let m = &snap.memory;
    assert!(m.retry_digest);
    assert_eq!(m.injected, 2);
    assert_eq!(m.ideas, Some(za_agent::IdeasSignal::Review));
    assert_eq!(m.legacy_until, None);
    assert_eq!(golden["memory"]["ideas"], "review");
    assert_eq!(serde_json::to_value(&snap).unwrap(), golden);
}

/// Блок `<state>` resume после ввода с подсказкой памяти (Ruling I1 R1-C1): сбой после
/// `context` той же границы и второй resume с того же снимка — без `ReplayMismatch`
/// (подсказка и `skill.load` ввода стоят раньше блока, блок не считается поздним).
#[tokio::test]
async fn resume_note_after_input_hint_replays() {
    let mut f = fx("note_after_hint");
    f.model.push(server_error());
    assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    let step = snap.step + 1;
    f.deps.crash = crash_at(CrashPoint::AfterContext { step });
    let change = ResumeChange {
        input: Some("ещё".into()),
        ..ResumeChange::default()
    };
    let err = resume(&f, change).await;
    assert!(matches!(err, Err(LoopError::Halted)), "{err:?}");
    let names = effect_names(&events(&f.store, f.run).await);
    let at: Vec<&str> = names
        .iter()
        .filter(|(s, _, _)| *s == step)
        .map(|(_, _, n)| n.as_str())
        .collect();
    assert_eq!(at, ["boundary", "memory.prompt", "state", "context"]);
    f.deps.crash = None;
    f.model.push(answer("ок"));
    assert_eq!(
        resume(&f, ResumeChange::default()).await.unwrap(),
        done("ок")
    );
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    let h = Conversation::load(&f.store, &snap.messages).await.unwrap();
    let notes = h
        .messages()
        .iter()
        .filter(|m| text_of(m).contains("action=\"resume\""))
        .count();
    assert_eq!(notes, 1);
}

// ---- R1-M, fix-3 задачи 3: повторный resume снимка v4, смена эндпоинта в начатом шаге ----

/// Имена эффектов шага `step` по порядку ключей.
fn names_at(names: &[(u64, u64, String)], step: u64) -> Vec<&str> {
    names
        .iter()
        .filter(|(s, _, _)| *s == step)
        .map(|(_, _, n)| n.as_str())
        .collect()
}

/// I1 ревью задачи 3, D161 (уточнение 2026-11-17): `legacy_until` снимка v4 вычисляется один
/// раз, при первом resume, и пишется в журнал событием. Повторный resume того же снимка (снимка
/// v5 ещё нет) берёт записанное значение, а не пересчитывает его по журналу, который уже
/// дописал код R1-M: шаг, начатый кодом R1-M, остаётся шагом R1-M.
#[tokio::test]
async fn second_resume_of_v4_snapshot_keeps_legacy_until() {
    // (журнал, шаг сбоя после `context`, legacy_until): у `approval` снимок `Model` шага 1 и
    // шаг 2 не начат; у `batch_error` снимок `Model` шага 2, шаг 3 не начат.
    for (name, crashed, until) in [("approval", 2, 1), ("batch_error", 3, 2)] {
        let (mut f, golden) = golden_r1c2(name).await;
        if name == "approval" {
            let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/golden/r1c2_journal/approval");
            let id = std::fs::read_to_string(src.join("interrupt.txt")).unwrap();
            assert!(
                f.store
                    .resolve_interrupt(id.trim(), json!({"approved": true}))
                    .await
                    .unwrap()
            );
        }
        let mut deps = f.deps.clone();
        deps.crash = crash_at(CrashPoint::AfterContext { step: crashed });
        let change = ResumeChange {
            input: Some("ещё".into()),
            ..ResumeChange::default()
        };
        let err = resume_with(&f, &deps, change).await;
        assert!(matches!(err, Err(LoopError::Halted)), "{name}: {err:?}");
        // Снимка v5 нет: последний снимок — прежний v4.
        let (_, v) = f.store.latest_snapshot(f.run).await.unwrap().unwrap();
        assert_eq!(v, golden, "{name}");
        // Шаг сбоя начат кодом R1-M: подсказка к вводу стоит до `context`.
        let names = effect_names(&events(&f.store, f.run).await);
        assert_eq!(
            names_at(&names, crashed),
            ["boundary", "memory.prompt", "context"],
            "{name}"
        );
        f.deps.crash = None;
        f.model.push(answer("готово"));
        assert_eq!(
            resume(&f, ResumeChange::default()).await.unwrap(),
            done("готово"),
            "{name}"
        );
        let evs = events(&f.store, f.run).await;
        let after = effect_names(&evs);
        assert_eq!(&after[..names.len()], &names[..], "{name}");
        let (_, snap) = last_snapshot(&f.store, f.run).await;
        assert_eq!(snap.v, za_agent::SNAPSHOT_V);
        assert_eq!(snap.memory.legacy_until, Some(until), "{name}");
        // Значение записано одним событием — первым resume, до первого нового эффекта.
        let marks: Vec<&Event> = evs
            .iter()
            .filter(|e| e.kind.as_str() == "memory.legacy")
            .collect();
        assert_eq!(marks.len(), 1, "{name}: {marks:?}");
        assert_eq!(marks[0].payload["until"], until, "{name}");
        if name == "approval" {
            assert_eq!(f.incr.load(Ordering::SeqCst), 1);
        }
    }
}

/// I2 ревью задачи 3, D161: смена эндпоинта при resume со снимка `Boundary` отвергается
/// (`ChangeMidStep`), если следующий шаг уже начат: аргумент `budget` эффектов памяти
/// (`memory.prompt`, `memory.retry`) зависит от окна, реплей с другим окном разошёлся бы с
/// журналом. Resume без смены доводит run.
#[tokio::test]
async fn endpoint_change_in_started_step_rejected() {
    // Без повтора дайджеста — ввод с подсказкой `memory.prompt`; с повтором (частичный
    // дайджест шага 0) — `memory.retry` без ввода.
    for retry in [false, true] {
        let mut f = fx(if retry {
            "started_step_retry"
        } else {
            "started_step_prompt"
        });
        if retry {
            let mem = Arc::new(support::memory::ScriptedMemory::new(
                za_agent::MemoryLevel::L2,
            ));
            mem.push_snapshot(za_agent::Recall::Partial(
                "### Задачи\n[in_progress] A — k:1".into(),
            ));
            f.deps.memory = mem;
        }
        // Окно 8192 (по умолчанию): бюджет памяти 200.
        f.model.push(server_error());
        assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
        let (_, snap) = last_snapshot(&f.store, f.run).await;
        assert_eq!(snap.next, Next::Boundary);
        assert_eq!(snap.memory.retry_digest, retry);
        let step = snap.step + 1;
        let mut deps = f.deps.clone();
        deps.crash = crash_at(CrashPoint::AfterContext { step });
        let change = ResumeChange {
            input: (!retry).then(|| "ещё".to_owned()),
            ..ResumeChange::default()
        };
        let err = resume_with(&f, &deps, change).await;
        assert!(matches!(err, Err(LoopError::Halted)), "{retry}: {err:?}");
        let names = effect_names(&events(&f.store, f.run).await);
        let expected: [&str; 4] = if retry {
            ["boundary", "state", "memory.retry", "context"]
        } else {
            ["boundary", "memory.prompt", "state", "context"]
        };
        assert_eq!(names_at(&names, step), expected);
        // Второй resume на эндпоинт с окном 32768 (бюджет памяти 800) — отказ до шага.
        let wide =
            Arc::new(ScriptedModel::new(default_caps(BackendKind::LlamaCpp)).with_context(32768));
        wide.push(answer("ок"));
        let mut deps = f.deps.clone();
        deps.model = wide.clone();
        let change = ResumeChange {
            endpoint: Some(EndpointId("b".into())),
            ..ResumeChange::default()
        };
        let r = resume_with(&f, &deps, change).await;
        assert!(matches!(r, Err(LoopError::ChangeMidStep)), "{retry}: {r:?}");
        assert_eq!(status(&f.store, f.run).await, RunStatus::Running);
        assert!(wide.calls().is_empty());
        assert_eq!(effect_names(&events(&f.store, f.run).await), names);
        // Без смены эндпоинта шаг доигрывается по журналу.
        f.model.push(answer("ок"));
        assert_eq!(
            resume(&f, ResumeChange::default()).await.unwrap(),
            done("ок"),
            "{retry}"
        );
    }
}

/// Снимок run `f` (последний, v5) переписан на том же `seq` функцией `edit`.
async fn rewrite_snapshot(f: &Fx, edit: impl FnOnce(&mut serde_json::Map<String, Value>)) {
    let (seq, mut v) = f.store.latest_snapshot(f.run).await.unwrap().unwrap();
    assert_eq!(v["v"], 5);
    edit(v.as_object_mut().unwrap());
    f.store.save_snapshot(f.run, seq, v).await.unwrap();
}

/// Minor 1 ревью задачи 3 (исправление `state_late`, CHANGELOG): ввод `/tdd …` при resume
/// со снимка `Boundary` — эффект `skill.load` стоит до блока `<state>`; сбой после `context`
/// той же границы и второй resume с того же снимка проходят без `ReplayMismatch`.
///
/// Два варианта:
/// - шаг R1-M: `boundary`, `skill.load`, `memory.prompt`, `state`, `context`;
/// - шаг legacy — ровно путь R1-C2: снимок v4 `Boundary`, шаг после него начат кодом
///   прежней версии (`boundary`, `skill.load`, `state`, `context`, без эффектов памяти).
///   Первая попытка шага идёт под снимком с `legacy_until = step` (так код R1-M проходит шаг
///   кодом R1-C2), затем снимок сводится к v4 — журнал той формы, что пишет R1-C2.
///
/// Прежний `state_late` (`{step}:1` не `state` — блок поздний) роняет оба варианта.
#[tokio::test]
async fn skill_load_before_state_replays() {
    for legacy in [false, true] {
        let name = if legacy {
            "skill_state_legacy"
        } else {
            "skill_state_r1m"
        };
        let (_root, cat) = user_skill_catalog(name, "Test first.");
        let mut f = fx(name);
        with_skills(&mut f.deps, cat);
        f.model.push(server_error());
        assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
        let (_, snap) = last_snapshot(&f.store, f.run).await;
        assert_eq!(snap.next, Next::Boundary);
        let step = snap.step + 1;
        if legacy {
            rewrite_snapshot(&f, |o| {
                o["memory"]["legacy_until"] = json!(step);
            })
            .await;
        }
        let mut deps = f.deps.clone();
        deps.crash = crash_at(CrashPoint::AfterContext { step });
        let change = ResumeChange {
            input: Some("/tdd проверь".into()),
            ..ResumeChange::default()
        };
        let err = resume_with(&f, &deps, change).await;
        assert!(matches!(err, Err(LoopError::Halted)), "{legacy}: {err:?}");
        let names = effect_names(&events(&f.store, f.run).await);
        let expected: &[&str] = if legacy {
            &["boundary", "skill.load", "state", "context"]
        } else {
            &[
                "boundary",
                "skill.load",
                "memory.prompt",
                "state",
                "context",
            ]
        };
        assert_eq!(names_at(&names, step), expected, "{legacy}");
        if legacy {
            // Снимок той формы, что пишет R1-C2: v4 без состояния памяти.
            rewrite_snapshot(&f, |o| {
                o.insert("v".into(), json!(4));
                assert!(o.remove("memory").is_some());
            })
            .await;
        }
        f.model.push(answer("ок"));
        assert_eq!(
            resume(&f, ResumeChange::default()).await.unwrap(),
            done("ок"),
            "{legacy}"
        );
        let evs = events(&f.store, f.run).await;
        let after = effect_names(&evs);
        assert_eq!(&after[..names.len()], &names[..], "{legacy}");
        assert_eq!(
            after.iter().filter(|(_, _, n)| n == "skill.load").count(),
            1,
            "{legacy}: эффект skill.load повторён"
        );
        let (_, snap) = last_snapshot(&f.store, f.run).await;
        assert_eq!(snap.memory.legacy_until, legacy.then_some(step), "{legacy}");
        // Блок `<state>` — один, после текста ввода со скиллом.
        let h = Conversation::load(&f.store, &snap.messages).await.unwrap();
        let texts: Vec<String> = h.messages().iter().map(text_of).collect();
        let notes: Vec<usize> = texts
            .iter()
            .enumerate()
            .filter(|(_, t)| t.contains("action=\"resume\""))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(notes.len(), 1, "{legacy}");
        let input = texts
            .iter()
            .position(|t| t.starts_with("/tdd проверь"))
            .expect("нет ввода /tdd");
        assert!(input < notes[0], "{legacy}: {texts:?}");
    }
}

/// Minor 2 ревью задачи 3, D140: на границе resume со снимка `Boundary` шага 1 отложенный
/// повтор дайджеста стоит после блока `<state>` и до `context`: эффекты ввода →
/// `state` → `memory.retry` → `context`; в истории текст повтора — после сообщения `resume`.
#[tokio::test]
async fn retry_digest_after_state_block() {
    let mut f = fx("retry_after_state");
    let mem = Arc::new(support::memory::ScriptedMemory::new(
        za_agent::MemoryLevel::L2,
    ));
    mem.push_snapshot(za_agent::Recall::Timeout);
    mem.push_retry(za_agent::Recall::Hits("### Задачи\n[open] B — k:2".into()));
    f.deps.memory = mem.clone();
    f.model.push(server_error());
    assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!((snap.step, snap.next), (1, Next::Boundary));
    assert!(snap.memory.retry_digest);
    f.model.push(answer("ок"));
    let change = ResumeChange {
        input: Some("ещё".into()),
        ..ResumeChange::default()
    };
    assert_eq!(resume(&f, change).await.unwrap(), done("ок"));
    let names = effect_names(&events(&f.store, f.run).await);
    let at = names_at(&names, 2);
    assert_eq!(
        &at[..5],
        [
            "boundary",
            "memory.prompt",
            "state",
            "memory.retry",
            "context"
        ]
    );
    let texts = user_texts(&f.model.calls().last().unwrap().request.messages);
    let note = texts
        .iter()
        .position(|t| t.contains("action=\"resume\""))
        .expect("нет сообщения resume");
    let digest = texts
        .iter()
        .position(|t| t.starts_with("<memory source=\"zam\" kind=\"digest\">"))
        .expect("нет текста повтора");
    assert_eq!(digest, note + 1, "{texts:?}");
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert!(!snap.memory.retry_digest);
}
