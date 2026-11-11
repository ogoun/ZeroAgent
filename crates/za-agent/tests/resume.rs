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
    ContextConfig, CrashPoint, EnvInfo, LoopDeps, LoopError, LoopOutcome, Next, NoMemory,
    PendingExit, ResumeChange, RunSpec, Snapshot, builtin_tools, control_channel, resume_run,
    start_run, state_block,
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
        // Эффекты шага 4: boundary, state, context (R1-B2 D42), model.
        EffectKey::derive(f.run, 4, 3).0
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
        .save_snapshot(f.run, last, json!({"v": 4}))
        .await
        .unwrap();
    let err = resume(&f, ResumeChange::default()).await.unwrap_err();
    assert!(
        matches!(&err, LoopError::Snapshot(s) if s == "версия снимка 4"),
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
    assert_eq!(snap.v, 3);
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
    assert_eq!(after.v, 3);
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
    assert_eq!(after.v, 3);
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
    assert_eq!(after.v, 3);
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
