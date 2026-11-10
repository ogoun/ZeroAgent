//! Resume run (задача 15): реплей после сбоя (Review Focus 1), отложенный выход
//! чистой паузы (D5), смена модели только на границе (D6), `cancelling` и `pausing`
//! после сбоя (D26), бюджет и ввод при resume, блок `<state>`.
//!
//! «Сбой» — либо брошенный future цикла на ожидании, которое не держит запись журнала
//! (стрим `push_open`, инструмент на семафоре; D33), либо крючок `LoopDeps.crash`
//! (`Err(Halted)` без единой записи). Сигналы посылаются по событию шины, не по таймеру.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore, broadcast};
use tokio::task::JoinHandle;
use za_agent::{
    CrashPoint, EnvInfo, LoopDeps, LoopError, LoopOutcome, Next, NoMemory, PendingExit,
    ResumeChange, RunSpec, Snapshot, builtin_tools, control_channel, resume_run, start_run,
    state_block,
};
use za_core::{
    AgentEvent, Approval, EventBus, Journal, Tool, ToolCx, ToolKind, ToolOutput, ToolRegistry,
    ToolSpec, UnknownDecision, resolve_unknown,
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

/// Множитель времени тестов: `ZA_TEST_TIME_SCALE` (целое 1–6, по умолчанию 1).
fn time_scale() -> u32 {
    static SCALE: OnceLock<u32> = OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("ZA_TEST_TIME_SCALE")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map_or(1, |n| n.clamp(1, 6))
    })
}

/// Предел ожиданий, которые при регрессии повисли бы.
fn limit() -> Duration {
    Duration::from_secs(10) * time_scale()
}

async fn within<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(limit(), f)
        .await
        .unwrap_or_else(|_| panic!("не дождались в срок: {what}"))
}

fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("resume")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
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
        Box::pin(async move {
            ToolOutput {
                content: format!("echo {args}"),
                is_error: false,
            }
        })
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
        Box::pin(async move {
            ToolOutput {
                content: format!("incr {n}"),
                is_error: false,
            }
        })
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
            ToolOutput {
                content: "hung".into(),
                is_error: false,
            }
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
    let store = ProjectStore::open(&dir(name)).unwrap();
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
        journal: Journal::new(store.clone(), bus.clone()),
        model: model.clone(),
        tools: Arc::new(reg),
        capabilities: Vec::new(),
        memory: Arc::new(NoMemory),
        crash: None,
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
        EffectKey::derive(f.run, 2, 1).0
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
        EffectKey::derive(f.run, 4, 2).0
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

/// Ревью задачи 14, M2: флаг паузы, поглощённый прежним выходом, не ставит resume на паузу.
#[tokio::test]
async fn consumed_pause_flag_does_not_repause() {
    let f = fx("consumed_pause");
    f.model.push(server_error());
    assert_eq!(start(&f, ask()).await.unwrap(), paused("provider"));
    let before = statuses(&f.store, f.run).await.len();
    let (ctl, rx) = control_channel(f.store.clone(), f.run);
    // Флаг поднят до resume (команда уже исполнена выходом в `paused`).
    ctl.pause();
    f.model.push(answer("ок"));
    let out = within(
        "resume_run",
        resume_run(&f.deps, f.run, ResumeChange::default(), rx),
    )
    .await
    .unwrap();
    assert_eq!(out, done("ок"));
    let st = statuses(&f.store, f.run).await;
    assert_eq!(st[before..], [RunStatus::Running, RunStatus::Done]);
}
