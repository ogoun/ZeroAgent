//! Детекторы повтора A,A,A (D54), цикла ABAB (D55) и застоя (D56): реакции
//! [`DetectorState`] по ходам и их применение драйвером — повтор без исполнения,
//! отказ без блоба, состояние детекторов в снимке после сбоя (D66, `CrashPoint`).

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use support::*;

use tokio::sync::Semaphore;

use serde_json::{Value, json};
use za_agent::CYCLE_NUDGE;
use za_agent::{
    ContextConfig, Conversation, CrashPoint, DetectorState, EnvInfo, LoopDeps, LoopError,
    LoopOutcome, NoMemory, REPEAT_MARK, REPEAT_NUDGE, REPEAT_REJECT, Reaction, ResumeChange,
    RunSpec, Snapshot, builtin_tools, call_fingerprint, control_channel, resume_run, start_run,
};
use za_core::{
    Approval, EventBus, Journal, RiskClass, Tool, ToolCx, ToolKind, ToolOutput, ToolRegistry,
    ToolSpec,
};
use za_llm::{
    BackendKind, BoxFuture, EndpointId, FinishReason, Message, ModelError, Part, Role,
    ScriptedModel, StreamEvent, ToolCall, ToolSchema, Usage, default_caps,
};
use za_protocol::{
    BlobHash, Budget, EffectClass, Event, EventKind, Priority, RunId, RunMode, RunRole, RunStatus,
    Seq, Tier,
};
use za_store::ProjectStore;

// ---- Помощники ----

fn h(s: &str) -> BlobHash {
    BlobHash(blake3::hash(s.as_bytes()).to_hex().to_string())
}

type Call = (String, String, Value, EffectClass);
type Fact = (String, ToolKind, bool, BlobHash);

fn call(id: &str, name: &str, args: Value, class: EffectClass) -> Call {
    (id.to_owned(), name.to_owned(), args, class)
}

fn read(id: &str, p: &str) -> Call {
    call(id, "read", json!({ "p": p }), EffectClass::ReadOnly)
}

fn exec(id: &str, cmd: &str) -> Call {
    call(
        id,
        "bash",
        json!({ "cmd": cmd }),
        EffectClass::NonIdempotent,
    )
}

/// Факт вызова с настоящим результатом. Драйвер передаёт и перехваченные детектором
/// вызовы (повтор, отказ): они — появления, но не новые факты.
fn fact(id: &str, kind: ToolKind, content: &str) -> Fact {
    (id.to_owned(), kind, false, h(content))
}

fn failed(id: &str, kind: ToolKind, content: &str) -> Fact {
    (id.to_owned(), kind, true, h(content))
}

fn reject() -> Reaction {
    Reaction::Reject(REPEAT_REJECT.to_owned())
}

fn reuse(call_id: &str, content: &str) -> Reaction {
    Reaction::ReusePrevious {
        call_id: call_id.into(),
        result: h(content),
        is_error: false,
    }
}

/// Ход: реакции на вызовы и реакция после результатов.
fn turn(
    d: &mut DetectorState,
    n: u32,
    calls: &[Call],
    facts: &[Fact],
) -> (Vec<Reaction>, Reaction) {
    let on_calls = d.on_calls(n, calls);
    let on_results = d.on_results(n, facts, false);
    (on_calls, on_results)
}

// ---- DetectorState ----

#[test]
fn fingerprint_is_16_hex_of_canonical_args() {
    let a = call_fingerprint("read", &json!({"p": "a", "q": 1}));
    let b = call_fingerprint("read", &json!({"q": 1, "p": "a"}));
    assert_eq!(a, b, "порядок ключей не важен (canonical_json)");
    assert_eq!(a.len(), 16);
    let mut raw = b"read\0".to_vec();
    raw.extend_from_slice(br#"{"p":"a","q":1}"#);
    assert_eq!(a, blake3::hash(&raw).to_hex()[..16]);
    assert_ne!(a, call_fingerprint("edit", &json!({"p": "a", "q": 1})));
}

#[test]
fn aaa_reactions_in_order() {
    let mut d = DetectorState::default();
    let expected = [
        vec![Reaction::None],
        vec![reuse("c1", "A")],
        vec![reject()],
        vec![reject(), Reaction::Nudge(REPEAT_NUDGE.to_owned())],
        vec![Reaction::Block("repeat")],
    ];
    for (i, want) in expected.iter().enumerate() {
        let n = i as u32 + 1;
        let id = format!("c{n}");
        let r = d.on_calls(n, &[read(&id, "a")]);
        assert_eq!(&r, want, "ход {n}");
        let after = d.on_results(n, &[fact(&id, ToolKind::Read, "A")], false);
        assert_eq!(after, Reaction::None, "ход {n}");
    }
}

#[test]
fn mutation_resets_repeat_counter() {
    let mut d = DetectorState::default();
    let (r, _) = turn(
        &mut d,
        1,
        &[read("c1", "a")],
        &[fact("c1", ToolKind::Read, "A")],
    );
    assert_eq!(r, vec![Reaction::None]);
    let (r, _) = turn(
        &mut d,
        2,
        &[read("c2", "a")],
        &[fact("c2", ToolKind::Read, "A")],
    );
    assert_eq!(r, vec![reuse("c1", "A")]);
    let (r, _) = turn(
        &mut d,
        3,
        &[exec("c3", "cargo test")],
        &[fact("c3", ToolKind::Exec, "ok")],
    );
    assert_eq!(r, vec![Reaction::None]);
    let (r, _) = turn(
        &mut d,
        4,
        &[read("c4", "a")],
        &[fact("c4", ToolKind::Read, "A2")],
    );
    assert_eq!(
        r,
        vec![Reaction::None],
        "третий read после мутации исполняется"
    );
    // Ruling I-3: упавший вызов, меняющий состояние, тоже сбрасывает окно.
    turn(
        &mut d,
        5,
        &[exec("c5", "x")],
        &[failed("c5", ToolKind::Exec, "err")],
    );
    let (r, _) = turn(
        &mut d,
        6,
        &[read("c6", "a")],
        &[fact("c6", ToolKind::Read, "A3")],
    );
    assert_eq!(
        r,
        vec![Reaction::None],
        "после упавшего Exec read исполняется"
    );
}

#[test]
fn mutation_in_same_batch_resets_for_later_calls() {
    // Ruling I-2: [правка a, чтение a] после прежнего чтения a — чтение исполняется.
    let mut d = DetectorState::default();
    turn(
        &mut d,
        1,
        &[read("c1", "a")],
        &[fact("c1", ToolKind::Read, "A")],
    );
    let edit = call("c2", "edit", json!({"p": "a"}), EffectClass::Idempotent);
    let (r, _) = turn(
        &mut d,
        2,
        &[edit, read("c3", "a")],
        &[
            fact("c2", ToolKind::Edit, "ok"),
            fact("c3", ToolKind::Read, "A'"),
        ],
    );
    assert_eq!(r, vec![Reaction::None, Reaction::None]);
    // Окно после хода 2 — только чтение после правки: следующий read a — повтор его.
    let (r, _) = turn(
        &mut d,
        3,
        &[read("c4", "a")],
        &[fact("c4", ToolKind::Read, "A'")],
    );
    assert_eq!(r, vec![reuse("c3", "A'")]);
    // Чтение до правки в том же пакете — повтор, правка затем сбрасывает окно.
    let edit = call("c6", "edit", json!({"p": "a"}), EffectClass::Idempotent);
    let (r, _) = turn(
        &mut d,
        4,
        &[read("c5", "a"), edit],
        &[
            fact("c5", ToolKind::Read, "A'"),
            fact("c6", ToolKind::Edit, "ok"),
        ],
    );
    assert_eq!(r, vec![reject(), Reaction::None]);
    let (r, _) = turn(
        &mut d,
        5,
        &[read("c7", "a")],
        &[fact("c7", ToolKind::Read, "A''")],
    );
    assert_eq!(r, vec![Reaction::None]);
}

#[test]
fn interrupted_call_is_not_an_appearance() {
    // Ruling I-1: вызов без результата (оборван, отклонён) в окно не идёт.
    let mut d = DetectorState::default();
    let (r, _) = turn(
        &mut d,
        1,
        &[read("c1", "a"), read("c2", "b")],
        &[fact("c1", ToolKind::Read, "A")],
    );
    assert_eq!(r, vec![Reaction::None, Reaction::None]);
    let (r, _) = turn(
        &mut d,
        2,
        &[read("c3", "b")],
        &[fact("c3", ToolKind::Read, "B")],
    );
    assert_eq!(r, vec![Reaction::None], "b не исполнялся: повтора нет");
    let (r, _) = turn(
        &mut d,
        3,
        &[read("c4", "b")],
        &[fact("c4", ToolKind::Read, "B")],
    );
    assert_eq!(r, vec![reuse("c3", "B")]);
}

#[test]
fn user_input_resets_repeat_window() {
    let mut d = DetectorState::default();
    turn(
        &mut d,
        1,
        &[read("c1", "a")],
        &[fact("c1", ToolKind::Read, "A")],
    );
    d.on_user_input();
    let (r, _) = turn(
        &mut d,
        2,
        &[read("c2", "a")],
        &[fact("c2", ToolKind::Read, "A")],
    );
    assert_eq!(r, vec![Reaction::None]);
}

#[test]
fn reused_error_stays_error() {
    // Ruling m-1: повтор прошлой ошибки — ошибка.
    let mut d = DetectorState::default();
    turn(
        &mut d,
        1,
        &[read("c1", "a")],
        &[failed("c1", ToolKind::Read, "no such file")],
    );
    let (r, _) = turn(
        &mut d,
        2,
        &[read("c2", "a")],
        &[failed("c2", ToolKind::Read, "x")],
    );
    assert_eq!(
        r,
        vec![Reaction::ReusePrevious {
            call_id: "c1".into(),
            result: h("no such file"),
            is_error: true,
        }]
    );
}

#[test]
fn parallel_set_is_order_independent() {
    // Отпечаток хода {a, b} не зависит от порядка: [a, b], [c], [b, a], [c] — цикл ABAB.
    let mut d = DetectorState::default();
    let ab = [read("c1", "a"), read("c2", "b")];
    let ab_facts = [
        fact("c1", ToolKind::Read, "A"),
        fact("c2", ToolKind::Read, "B"),
    ];
    turn(&mut d, 1, &ab, &ab_facts);
    turn(
        &mut d,
        2,
        &[read("c3", "c")],
        &[fact("c3", ToolKind::Read, "C")],
    );
    let (r, after) = turn(
        &mut d,
        3,
        &[read("c4", "b"), read("c5", "a")],
        &[
            fact("c4", ToolKind::Read, "B"),
            fact("c5", ToolKind::Read, "A"),
        ],
    );
    assert_eq!(
        r,
        vec![reuse("c2", "B"), reuse("c1", "A")],
        "реакции — по порядку вызовов"
    );
    assert_eq!(after, Reaction::None);
    let (_, after) = turn(
        &mut d,
        4,
        &[read("c6", "c")],
        &[fact("c6", ToolKind::Read, "C")],
    );
    assert_eq!(after, Reaction::Nudge(CYCLE_NUDGE.to_owned()));

    // Контроль: другой набор в третьем ходу — не цикл.
    let mut d = DetectorState::default();
    turn(&mut d, 1, &ab, &ab_facts);
    turn(
        &mut d,
        2,
        &[read("c3", "c")],
        &[fact("c3", ToolKind::Read, "C")],
    );
    turn(
        &mut d,
        3,
        &[read("c4", "b"), read("c5", "d")],
        &[
            fact("c4", ToolKind::Read, "B"),
            fact("c5", ToolKind::Read, "D"),
        ],
    );
    let (_, after) = turn(
        &mut d,
        4,
        &[read("c6", "c")],
        &[fact("c6", ToolKind::Read, "C")],
    );
    assert_eq!(after, Reaction::None);
}

#[test]
fn abab_reflects_then_blocks() {
    // Как в драйвере: X и Y с хода 3 перехвачены повтором (повтор, отказ) — новых
    // фактов нет; результаты передаются, это появления.
    let mut d = DetectorState::default();
    let plan: [(u32, bool, Reaction); 8] = [
        (1, true, Reaction::None),
        (2, false, Reaction::None),
        (3, true, Reaction::None),
        (4, false, Reaction::Nudge(CYCLE_NUDGE.to_owned())),
        // Окно очищено: скользящее `Y,X,Y,X` на ходу 5 не срабатывает.
        (5, true, Reaction::None),
        (6, false, Reaction::None),
        (7, true, Reaction::None),
        (8, false, Reaction::Block("cycle")),
    ];
    for (n, is_x, want) in plan {
        let id = format!("c{n}");
        let p = if is_x { "x" } else { "y" };
        let (r, after) = turn(&mut d, n, &[read(&id, p)], &[fact(&id, ToolKind::Read, p)]);
        if n >= 3 {
            assert_ne!(
                r,
                vec![Reaction::None],
                "ход {n}: вызов перехвачен повтором"
            );
        }
        assert_eq!(after, want, "ход {n}");
    }
}

#[test]
fn abab_drift_of_three_reflects() {
    // X,Y,Z,X,Y,Z без новых фактов — nudge на шестом ходу.
    let mut d = DetectorState::default();
    for n in 1..=6u32 {
        let p = ["x", "y", "z"][(n as usize - 1) % 3];
        let id = format!("c{n}");
        let (_, after) = turn(&mut d, n, &[read(&id, p)], &[fact(&id, ToolKind::Read, p)]);
        let want = if n == 6 {
            Reaction::Nudge(CYCLE_NUDGE.to_owned())
        } else {
            Reaction::None
        };
        assert_eq!(after, want, "ход {n}");
    }
}

#[test]
fn abab_with_new_facts_is_fine() {
    // X — команда (исполняется каждый раз, сбрасывает окно повтора), Y — чтение её
    // лога; содержимое каждый раз новое — цикла нет.
    let mut d = DetectorState::default();
    for n in 1..=8u32 {
        let id = format!("c{n}");
        let (c, kind) = if n % 2 == 1 {
            (exec(&id, "make"), ToolKind::Exec)
        } else {
            (read(&id, "build.log"), ToolKind::Read)
        };
        let mut f = failed(&id, kind, &format!("content {n}"));
        f.2 = n % 2 == 1; // команда падает: не прогресс по D56, но факт новый
        let (r, after) = turn(&mut d, n, &[c], &[f]);
        assert_eq!(r, vec![Reaction::None], "ход {n}: исполняется");
        assert_eq!(after, Reaction::None, "ход {n}");
    }
}

#[test]
fn stall_after_8_turns() {
    let mut d = DetectorState::default();
    for n in 1..=9u32 {
        let id = format!("c{n}");
        let (r, after) = turn(
            &mut d,
            n,
            &[read(&id, &format!("f{n}"))],
            &[fact(&id, ToolKind::Read, "same")],
        );
        assert_eq!(r, vec![Reaction::None], "ход {n}: не повтор");
        let want = if n == 9 {
            Reaction::Block("stall")
        } else {
            Reaction::None
        };
        assert_eq!(after, want, "ход {n}");
    }
}

#[test]
fn state_roundtrips_and_defaults() {
    let mut d = DetectorState::default();
    turn(
        &mut d,
        1,
        &[read("c1", "a")],
        &[fact("c1", ToolKind::Read, "A")],
    );
    let v = serde_json::to_value(&d).unwrap();
    let back: DetectorState = serde_json::from_value(v).unwrap();
    assert_eq!(back, d);
    let empty: DetectorState = serde_json::from_value(json!({})).unwrap();
    assert_eq!(empty, DetectorState::default());
}

// ---- Драйвер ----

/// Инструмент: считает исполнения `read`; ответ — по имени (см. [`reply`]).
struct Probe {
    spec: ToolSpec,
    count: Option<Arc<AtomicUsize>>,
}

/// `read` — `content of <p>`; `peek` — всегда одно и то же; `fail` — ошибка;
/// `edit` — `edited <p>`.
fn reply(name: &str, args: &Value) -> ToolOutput {
    let (content, is_error) = match name {
        "read" => (format!("content of {}", args["p"]), false),
        "peek" => ("same".to_owned(), false),
        "fail" => (format!("no such file {}", args["p"]), true),
        _ => (format!("edited {}", args["p"]), false),
    };
    ToolOutput {
        content,
        is_error,
        ..ToolOutput::default()
    }
}

impl Tool for Probe {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        if let Some(c) = &self.count {
            c.fetch_add(1, Ordering::SeqCst);
        }
        let out = reply(&self.spec.schema.name, &args);
        Box::pin(async move { out })
    }
}

/// Инструмент чтения `hold`: ждёт разрешения семафора.
struct Hold {
    spec: ToolSpec,
    release: Arc<Semaphore>,
}

impl Tool for Hold {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            within("разрешение hold", release.acquire())
                .await
                .expect("семафор закрыт")
                .forget();
            ToolOutput::ok(format!("held {args}"))
        })
    }
}

fn spec_of(name: &str, kind: ToolKind, class: EffectClass) -> ToolSpec {
    ToolSpec {
        schema: ToolSchema {
            name: name.into(),
            description: "t".into(),
            parameters: json!({"type": "object"}),
        },
        class,
        kind,
        // Правка без одобрения в режиме Ask.
        approval: Approval::Never,
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

struct Fx {
    store: ProjectStore,
    model: Arc<ScriptedModel>,
    deps: LoopDeps,
    run: RunId,
    count: Arc<AtomicUsize>,
    bus: EventBus,
    release: Arc<Semaphore>,
}

fn fx(name: &str) -> Fx {
    let d = unique_dir("detect_repeat", name);
    let store = ProjectStore::open(&d).unwrap();
    let model = Arc::new(ScriptedModel::new(default_caps(BackendKind::LlamaCpp)));
    let count = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let mut reg = ToolRegistry::default();
    for t in builtin_tools() {
        reg.register(t).unwrap();
    }
    reg.register(Arc::new(Probe {
        spec: spec_of("read", ToolKind::Read, EffectClass::ReadOnly),
        count: Some(Arc::clone(&count)),
    }))
    .unwrap();
    for (name, kind, class) in [
        ("peek", ToolKind::Read, EffectClass::ReadOnly),
        ("fail", ToolKind::Read, EffectClass::ReadOnly),
        ("edit", ToolKind::Edit, EffectClass::Idempotent),
    ] {
        reg.register(Arc::new(Probe {
            spec: spec_of(name, kind, class),
            count: None,
        }))
        .unwrap();
    }
    reg.register(Arc::new(Hold {
        spec: spec_of("hold", ToolKind::Read, EffectClass::ReadOnly),
        release: Arc::clone(&release),
    }))
    .unwrap();
    let bus = EventBus::new(EventBus::DEFAULT_CAPACITY);
    let deps = LoopDeps {
        journal: Journal::new(store.clone(), bus.clone()),
        model: model.clone(),
        tools: Arc::new(reg),
        capabilities: Vec::new(),
        memory: Arc::new(NoMemory),
        crash: None,
        fault: None,
    };
    Fx {
        store,
        model,
        deps,
        run: RunId::new(),
        count,
        bus,
        release,
    }
}

fn spec() -> RunSpec {
    RunSpec {
        project_id: "p".into(),
        project_dir: PathBuf::from("H:/p"),
        role: RunRole::Top,
        parent: None,
        endpoint: EndpointId("local".into()),
        run_mode: RunMode::Ask,
        work_mode: None,
        tier: None,
        budget: Budget::default(),
        priority: Priority::Top,
        max_wait: None,
        task: "Прочитай файл a".into(),
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
    }
}

async fn start(f: &Fx, deps: &LoopDeps) -> Result<LoopOutcome, LoopError> {
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    within("start_run", start_run(deps, f.run, spec(), rx)).await
}

async fn resume(f: &Fx) -> Result<LoopOutcome, LoopError> {
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    within(
        "resume_run",
        resume_run(&f.deps, f.run, ResumeChange::default(), rx),
    )
    .await
}

fn crash_at(at: CrashPoint) -> Option<za_agent::CrashHook> {
    Some(Arc::new(move |p: CrashPoint| p == at))
}

fn read_turn(id: &str, p: &str) -> Vec<Result<StreamEvent, ModelError>> {
    vec![Ok(StreamEvent::Finished {
        message: Message {
            role: Role::Assistant,
            parts: vec![Part::ToolCall(ToolCall {
                id: id.into(),
                name: "read".into(),
                arguments: json!({ "p": p }),
                raw_arguments: None,
            })],
        },
        finish: FinishReason::ToolCalls,
        usage: Usage::default(),
    })]
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

fn done(t: &str) -> LoopOutcome {
    LoopOutcome::Finished {
        status: RunStatus::Done,
        answer: Some(t.into()),
        finish: None,
    }
}

async fn last_snapshot(s: &ProjectStore, run: RunId) -> (Seq, Snapshot) {
    let (seq, v) = s.latest_snapshot(run).await.unwrap().expect("нет снимка");
    (seq, serde_json::from_value(v).unwrap())
}

async fn history(f: &Fx) -> Conversation {
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    Conversation::load(&f.store, &snap.messages).await.unwrap()
}

fn result_of(c: &Conversation, id: &str) -> (String, bool) {
    c.messages()
        .iter()
        .flat_map(|m| &m.parts)
        .find_map(|p| match p {
            Part::ToolResult {
                call_id,
                content,
                is_error,
            } if call_id == id => Some((content.clone(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("нет результата {id}"))
}

async fn events(s: &ProjectStore, run: RunId) -> Vec<Event> {
    s.read_run(run, Seq(0), 100_000).await.unwrap()
}

/// (detector, action, turn) событий `loop.detected`.
async fn detected(f: &Fx) -> Vec<(String, String, u64)> {
    events(&f.store, f.run)
        .await
        .into_iter()
        .filter(|e| e.kind == EventKind::LoopDetected)
        .map(|e| {
            (
                e.payload["detector"].as_str().unwrap().to_owned(),
                e.payload["action"].as_str().unwrap().to_owned(),
                e.payload["turn"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn reused_result_not_executed() {
    let f = fx("reused");
    f.model.push(read_turn("c1", "a"));
    f.model.push(read_turn("c2", "a"));
    f.model.push(answer("готово"));
    assert_eq!(start(&f, &f.deps).await.unwrap(), done("готово"));
    assert_eq!(f.count.load(Ordering::SeqCst), 1, "второй read не исполнен");
    let conv = history(&f).await;
    let (first, _) = result_of(&conv, "c1");
    let (second, is_error) = result_of(&conv, "c2");
    assert!(
        second.starts_with("[repeated call: identical to an earlier call;"),
        "{second}"
    );
    assert_eq!(second, format!("{REPEAT_MARK}\n{first}"));
    assert!(!is_error, "повтор — результат без ошибки");
    assert_eq!(
        detected(&f).await,
        vec![("repeat".to_owned(), "reuse".to_owned(), 2)]
    );
}

#[tokio::test]
async fn repeat_blocks_run_on_fifth_call() {
    let f = fx("repeat_block");
    for n in 1..=5 {
        f.model.push(read_turn(&format!("c{n}"), "a"));
    }
    let out = start(&f, &f.deps).await.unwrap();
    assert_eq!(
        out,
        LoopOutcome::Finished {
            status: RunStatus::Blocked,
            answer: None,
            finish: None,
        }
    );
    assert_eq!(f.count.load(Ordering::SeqCst), 1);
    let conv = history(&f).await;
    assert_eq!(result_of(&conv, "c3"), (REPEAT_REJECT.to_owned(), true));
    assert_eq!(result_of(&conv, "c4"), (REPEAT_REJECT.to_owned(), true));
    let nudges = conv
        .messages()
        .iter()
        .filter(|m| m.role == Role::User)
        .filter(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, Part::Text(t) if t.contains(REPEAT_NUDGE)))
        })
        .count();
    assert_eq!(nudges, 1, "nudge — один раз, после четвёртого вызова");
    let d = detected(&f).await;
    assert_eq!(
        d,
        [
            ("repeat", "reuse", 2),
            ("repeat", "reject", 3),
            ("repeat", "reject", 4),
            ("repeat", "nudge", 4),
            ("repeat", "block", 5),
        ]
        .map(|(a, b, t)| (a.to_owned(), b.to_owned(), t))
    );
}

#[tokio::test]
async fn reuse_without_blob_rejects() {
    // Хранилище блобы не удаляет; «блоба нет» (например, снимок после компакции в
    // хранилище без него) моделируется подменой хэша прошлого результата в снимке.
    let f = fx("reuse_no_blob");
    f.model.push(read_turn("c1", "a"));
    f.model.push(read_turn("c2", "b"));
    let mut crashing = f.deps.clone();
    crashing.crash = crash_at(CrashPoint::AfterBoundary { step: 3 });
    let err = start(&f, &crashing).await.unwrap_err();
    assert!(matches!(err, LoopError::Halted), "{err:?}");
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    let mut v = serde_json::to_value(&snap).unwrap();
    let results = v["detectors"]["repeat"]["results"]
        .as_object_mut()
        .expect("прошлые результаты в снимке");
    // Снимок `Model` шага 2: факты хода 2 (read b) ещё не учтены.
    assert_eq!(results.len(), 1, "{results:?}");
    let missing = "0".repeat(64);
    assert!(
        f.store
            .get_blob(&BlobHash(missing.clone()))
            .await
            .unwrap()
            .is_none()
    );
    for r in results.values_mut() {
        r["result"] = json!(missing);
    }
    let last = events(&f.store, f.run).await.last().unwrap().seq;
    f.store.save_snapshot(f.run, last, v).await.unwrap();

    f.model.push(read_turn("c3", "a"));
    f.model.push(answer("готово"));
    assert_eq!(resume(&f).await.unwrap(), done("готово"));
    let conv = history(&f).await;
    assert_eq!(result_of(&conv, "c3"), (REPEAT_REJECT.to_owned(), true));
    assert_eq!(f.count.load(Ordering::SeqCst), 2, "исполнены только a и b");
}

#[tokio::test]
async fn detector_state_survives_resume() {
    let f = fx("survives_resume");
    f.model.push(read_turn("c1", "a"));
    f.model.push(read_turn("c2", "a"));
    let mut crashing = f.deps.clone();
    crashing.crash = crash_at(CrashPoint::AfterBoundary { step: 3 });
    let err = start(&f, &crashing).await.unwrap_err();
    assert!(matches!(err, LoopError::Halted), "{err:?}");
    let (_, snap) = last_snapshot(&f.store, f.run).await;
    assert_eq!(snap.turn, 2, "ходы 1 и 2 сделаны");
    assert_ne!(snap.detectors, DetectorState::default());

    f.model.push(read_turn("c3", "a"));
    f.model.push(answer("готово"));
    assert_eq!(resume(&f).await.unwrap(), done("готово"));
    let conv = history(&f).await;
    let (first, _) = result_of(&conv, "c1");
    assert_eq!(result_of(&conv, "c2").0, format!("{REPEAT_MARK}\n{first}"));
    assert_eq!(result_of(&conv, "c3"), (REPEAT_REJECT.to_owned(), true));
    assert_eq!(
        f.count.load(Ordering::SeqCst),
        1,
        "инструмент исполнен 1 раз за оба процесса"
    );
}

// ---- Раунд исправлений 1 (ревью задачи 6) ----

/// Ответ модели с вызовами `(id, имя, аргументы)`.
fn calls_turn(cs: &[(&str, &str, Value)]) -> Vec<Result<StreamEvent, ModelError>> {
    vec![Ok(StreamEvent::Finished {
        message: Message {
            role: Role::Assistant,
            parts: cs
                .iter()
                .map(|(id, name, args)| {
                    Part::ToolCall(ToolCall {
                        id: (*id).into(),
                        name: (*name).into(),
                        arguments: args.clone(),
                        raw_arguments: None,
                    })
                })
                .collect(),
        },
        finish: FinishReason::ToolCalls,
        usage: Usage::default(),
    })]
}

/// Число исполнений инструмента `name` по событиям `tool.started` (D8).
async fn started(f: &Fx, name: &str) -> usize {
    events(&f.store, f.run)
        .await
        .iter()
        .filter(|e| e.kind == EventKind::ToolStarted && e.payload["name"] == name)
        .count()
}

/// Run с управлением: `on_started` — действие по первому `tool.started` (по событию
/// шины, не по таймеру).
async fn start_with(
    f: &Fx,
    on_started: impl FnOnce(za_agent::RunControl, Arc<Semaphore>) -> BoxFuture<'static, ()>
    + Send
    + 'static,
) -> LoopOutcome {
    let (ctl, rx) = control_channel(f.store.clone(), f.run);
    let mut bus = f.bus.subscribe();
    let release = Arc::clone(&f.release);
    let watcher = tokio::spawn(async move {
        within("tool.started", async {
            loop {
                let ev = bus.recv().await.expect("шина закрыта");
                if ev.event.kind == EventKind::ToolStarted {
                    return;
                }
            }
        })
        .await;
        on_started(ctl, release).await;
    });
    let out = within("start_run", start_run(&f.deps, f.run, spec(), rx))
        .await
        .unwrap();
    within("наблюдатель", watcher).await.unwrap();
    out
}

/// I-1: вызов, оборванный вводом, не появление — его повтор исполняется.
#[tokio::test]
async fn interrupted_call_repeat_executes() {
    let f = fx("interrupted_repeat");
    f.model.push(calls_turn(&[
        ("c1", "hold", json!({"id": 1})),
        ("c2", "read", json!({"p": "a"})),
    ]));
    f.model.push(read_turn("c3", "a"));
    f.model.push(answer("готово"));
    let out = start_with(&f, |ctl, release| {
        Box::pin(async move {
            ctl.input("стоп", za_agent::InputMode::Interrupt)
                .await
                .unwrap();
            release.add_permits(1);
        })
    })
    .await;
    assert_eq!(out, done("готово"));
    let conv = history(&f).await;
    assert_eq!(
        result_of(&conv, "c2"),
        ("interrupted: call not executed".to_owned(), true)
    );
    let (c3, is_error) = result_of(&conv, "c3");
    assert_eq!((c3.as_str(), is_error), (r#"content of "a""#, false));
    assert_eq!(f.count.load(Ordering::SeqCst), 1);
    assert!(detected(&f).await.is_empty(), "{:?}", detected(&f).await);
}

/// I-2: `[правка a, чтение a]` после прежнего чтения a — чтение исполняется заново.
#[tokio::test]
async fn edit_then_read_in_one_batch_reads_again() {
    let f = fx("edit_then_read");
    f.model.push(read_turn("c1", "a"));
    f.model.push(calls_turn(&[
        ("c2", "edit", json!({"p": "a"})),
        ("c3", "read", json!({"p": "a"})),
    ]));
    f.model.push(answer("готово"));
    assert_eq!(start(&f, &f.deps).await.unwrap(), done("готово"));
    assert_eq!(f.count.load(Ordering::SeqCst), 2, "второе чтение исполнено");
    let conv = history(&f).await;
    assert_eq!(
        result_of(&conv, "c3"),
        (r#"content of "a""#.to_owned(), false)
    );
    assert!(detected(&f).await.is_empty());
}

/// I-3: ввод пользователя на границе сбрасывает окно повтора.
#[tokio::test]
async fn user_input_resets_repeat() {
    let f = fx("input_resets");
    f.model
        .push(calls_turn(&[("c1", "hold", json!({"id": 1}))]));
    f.model
        .push(calls_turn(&[("c2", "hold", json!({"id": 1}))]));
    f.model.push(answer("готово"));
    let out = start_with(&f, |ctl, release| {
        Box::pin(async move {
            // Ввод записан до конца первого вызова: его прочитает граница хода 2.
            ctl.input("я поправил конфиг", za_agent::InputMode::Queue)
                .await
                .unwrap();
            release.add_permits(2);
        })
    })
    .await;
    assert_eq!(out, done("готово"));
    assert_eq!(started(&f, "hold").await, 2, "повтор после ввода исполнен");
    assert!(detected(&f).await.is_empty());
}

/// m-1: повтор прошлой ошибки — тоже ошибка.
#[tokio::test]
async fn reused_error_keeps_is_error() {
    let f = fx("reused_error");
    f.model
        .push(calls_turn(&[("c1", "fail", json!({"p": "a"}))]));
    f.model
        .push(calls_turn(&[("c2", "fail", json!({"p": "a"}))]));
    f.model.push(answer("готово"));
    assert_eq!(start(&f, &f.deps).await.unwrap(), done("готово"));
    assert_eq!(started(&f, "fail").await, 1);
    let conv = history(&f).await;
    let (first, e1) = result_of(&conv, "c1");
    assert!(e1);
    assert_eq!(
        result_of(&conv, "c2"),
        (format!("{REPEAT_MARK}\n{first}"), true)
    );
}

/// m-6: цикл через драйвер — nudge цикла и рецитация задачи после хода 4.
#[tokio::test]
async fn cycle_nudges_with_recitation() {
    let f = fx("cycle_driver");
    for (n, p) in [(1, "x"), (2, "y"), (3, "x"), (4, "y")] {
        f.model.push(read_turn(&format!("c{n}"), p));
    }
    f.model.push(answer("готово"));
    assert_eq!(start(&f, &f.deps).await.unwrap(), done("готово"));
    let conv = history(&f).await;
    let texts: Vec<String> = conv
        .messages()
        .iter()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| &m.parts)
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    let at = texts
        .iter()
        .position(|t| t.contains(CYCLE_NUDGE))
        .expect("нет nudge цикла");
    assert!(
        texts[at + 1].contains("Current task: Прочитай файл a"),
        "{:?}",
        texts[at + 1]
    );
    assert_eq!(
        detected(&f).await,
        [
            ("repeat", "reuse", 3),
            ("repeat", "reuse", 4),
            ("cycle", "nudge", 4),
        ]
        .map(|(a, b, t)| (a.to_owned(), b.to_owned(), t))
    );
}

/// m-6: застой через драйвер — 8 ходов без новых фактов, затем `blocked`.
#[tokio::test]
async fn stall_blocks_run() {
    let f = fx("stall_driver");
    for n in 1..=9 {
        f.model.push(calls_turn(&[(
            &format!("c{n}"),
            "peek",
            json!({"p": format!("f{n}")}),
        )]));
    }
    let out = start(&f, &f.deps).await.unwrap();
    assert_eq!(
        out,
        LoopOutcome::Finished {
            status: RunStatus::Blocked,
            answer: None,
            finish: None,
        }
    );
    assert_eq!(started(&f, "peek").await, 9);
    assert_eq!(
        detected(&f).await,
        vec![("stall".to_owned(), "block".to_owned(), 9)]
    );
}

// ---- Раунд исправлений 2 (повторное ревью задачи 6) ----

fn ask(id: &str) -> Vec<Result<StreamEvent, ModelError>> {
    calls_turn(&[(id, "ask_user", json!({"question": "Какой файл?"}))])
}

/// Run с `ask_user`: инструмент есть у top с тира standard.
async fn start_standard(f: &Fx) -> LoopOutcome {
    let mut s = spec();
    s.tier = Some(Tier::Standard);
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    within("start_run", start_run(&f.deps, f.run, s, rx))
        .await
        .unwrap()
}

/// Отвечает на открытое прерывание `ask_user` и продолжает run до следующего выхода.
async fn answer_and_resume(f: &Fx, out: LoopOutcome, text: &str) -> LoopOutcome {
    let LoopOutcome::Paused {
        interrupt: Some(id),
        ..
    } = out
    else {
        panic!("ожидалась пауза на ask_user: {out:?}");
    };
    assert!(
        f.store
            .resolve_interrupt(&id, json!({ "answer": text }))
            .await
            .unwrap()
    );
    resume(f).await.unwrap()
}

/// N-1: ответы на `ask_user` — прогресс (D56): девять вопросов подряд с одинаковым
/// ответом не дают `blocked (stall)`.
#[tokio::test]
async fn answered_ask_user_is_progress() {
    let f = fx("ask_progress");
    for n in 1..=9 {
        f.model.push(ask(&format!("q{n}")));
    }
    f.model.push(answer("готово"));
    let mut out = start_standard(&f).await;
    for _ in 1..=9 {
        out = answer_and_resume(&f, out, "да").await;
    }
    assert_eq!(out, done("готово"));
    assert!(detected(&f).await.is_empty(), "{:?}", detected(&f).await);
}

/// N-2: ответ на `ask_user` — ввод пользователя: сбрасывает окно повтора.
#[tokio::test]
async fn ask_user_answer_resets_repeat() {
    let f = fx("ask_resets");
    f.model.push(read_turn("c1", "a"));
    f.model.push(ask("q1"));
    f.model.push(read_turn("c2", "a"));
    f.model.push(answer("готово"));
    let out = start_standard(&f).await;
    let out = answer_and_resume(&f, out, "я поправил a").await;
    assert_eq!(out, done("готово"));
    assert_eq!(
        f.count.load(Ordering::SeqCst),
        2,
        "чтение после ответа исполнено"
    );
    assert!(detected(&f).await.is_empty());
}

/// I-1 на пути паузы по команде: не начатый вызов — не появление.
#[tokio::test]
async fn paused_call_repeat_executes() {
    let f = fx("paused_repeat");
    f.model.push(calls_turn(&[
        ("c1", "hold", json!({"id": 1})),
        ("c2", "read", json!({"p": "a"})),
    ]));
    let out = start_with(&f, |ctl, release| {
        Box::pin(async move {
            ctl.pause();
            release.add_permits(1);
        })
    })
    .await;
    assert_eq!(
        out,
        LoopOutcome::Paused {
            reason: "command".into(),
            interrupt: None,
        }
    );
    f.model.push(read_turn("c3", "a"));
    f.model.push(answer("готово"));
    assert_eq!(resume(&f).await.unwrap(), done("готово"));
    let conv = history(&f).await;
    assert_eq!(
        result_of(&conv, "c2"),
        ("interrupted: call not executed".to_owned(), true)
    );
    assert_eq!(
        result_of(&conv, "c3"),
        (r#"content of "a""#.to_owned(), false)
    );
    assert_eq!(f.count.load(Ordering::SeqCst), 1);
}

/// I-1: вызов, отклонённый гейтом режима, — не появление: повтор снова получает отказ
/// гейта, а не отказ повтора.
#[tokio::test]
async fn gate_denied_call_is_not_an_appearance() {
    let f = fx("gate_denied");
    for n in 1..=3 {
        f.model
            .push(calls_turn(&[(&format!("c{n}"), "edit", json!({"p": "a"}))]));
    }
    f.model.push(answer("готово"));
    let mut s = spec();
    s.run_mode = RunMode::ReadOnly;
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    let out = within("start_run", start_run(&f.deps, f.run, s, rx))
        .await
        .unwrap();
    assert_eq!(out, done("готово"));
    let conv = history(&f).await;
    for n in 1..=3 {
        let (text, is_error) = result_of(&conv, &format!("c{n}"));
        assert!(text.starts_with("denied:"), "c{n}: {text}");
        assert!(is_error);
    }
    assert_eq!(started(&f, "edit").await, 0);
    assert!(detected(&f).await.is_empty());
}
