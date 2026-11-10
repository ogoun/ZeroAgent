//! Детекторы повтора A,A,A (D54), цикла ABAB (D55) и застоя (D56): реакции
//! [`DetectorState`] по ходам и их применение драйвером — повтор без исполнения,
//! отказ без блоба, состояние детекторов в снимке после сбоя (D66, `CrashPoint`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use za_agent::CYCLE_NUDGE;
use za_agent::{
    ContextConfig, Conversation, CrashPoint, DetectorState, EnvInfo, LoopDeps, LoopError,
    LoopOutcome, NoMemory, REPEAT_MARK, REPEAT_NUDGE, REPEAT_REJECT, Reaction, ResumeChange,
    RunSpec, Snapshot, builtin_tools, call_fingerprint, control_channel, resume_run, start_run,
};
use za_core::{
    Approval, EventBus, Journal, Tool, ToolCx, ToolKind, ToolOutput, ToolRegistry, ToolSpec,
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

async fn within<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10) * time_scale(), f)
        .await
        .unwrap_or_else(|_| panic!("не дождались в срок: {what}"))
}

fn h(s: &str) -> BlobHash {
    BlobHash(blake3::hash(s.as_bytes()).to_hex().to_string())
}

fn call(id: &str, name: &str, args: Value) -> (String, String, Value) {
    (id.to_owned(), name.to_owned(), args)
}

fn read(id: &str, p: &str) -> (String, String, Value) {
    call(id, "read", json!({ "p": p }))
}

fn fact(id: &str, kind: ToolKind, content: &str) -> (String, ToolKind, bool, BlobHash) {
    (id.to_owned(), kind, false, h(content))
}

fn reject() -> Reaction {
    Reaction::Reject(REPEAT_REJECT.to_owned())
}

/// Ход: реакции на вызовы и реакция после результатов.
fn turn(
    d: &mut DetectorState,
    n: u32,
    calls: &[(String, String, Value)],
    facts: &[(String, ToolKind, bool, BlobHash)],
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
        vec![Reaction::ReusePrevious {
            call_id: "c1".into(),
            result: h("A"),
        }],
        vec![reject()],
        vec![reject(), Reaction::Nudge(REPEAT_NUDGE.to_owned())],
        vec![Reaction::Block("repeat")],
    ];
    for (i, want) in expected.iter().enumerate() {
        let n = i as u32 + 1;
        let id = format!("c{n}");
        let r = d.on_calls(n, &[read(&id, "a")]);
        assert_eq!(&r, want, "ход {n}");
        // Драйвер передаёт факты только исполненных вызовов: здесь — первого.
        let facts = if n == 1 {
            vec![fact(&id, ToolKind::Read, "A")]
        } else {
            Vec::new()
        };
        assert_eq!(d.on_results(n, &facts, false), Reaction::None, "ход {n}");
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
    let (r, _) = turn(&mut d, 2, &[read("c2", "a")], &[]);
    assert!(matches!(r[..], [Reaction::ReusePrevious { .. }]), "{r:?}");
    let exec = call("c3", "bash", json!({"cmd": "cargo test"}));
    let (r, _) = turn(&mut d, 3, &[exec], &[fact("c3", ToolKind::Exec, "ok")]);
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
    // Неуспешный Exec — не мутация: счётчик идёт дальше.
    let mut failed = fact("c5", ToolKind::Exec, "err");
    failed.2 = true;
    turn(
        &mut d,
        5,
        &[call("c5", "bash", json!({"cmd": "x"}))],
        &[failed],
    );
    let (r, _) = turn(&mut d, 6, &[read("c6", "a")], &[]);
    assert!(matches!(r[..], [Reaction::ReusePrevious { .. }]), "{r:?}");
}

#[test]
fn parallel_set_is_order_independent() {
    // Отпечаток хода {a, b} не зависит от порядка: [a, b], [c], [b, a], [c] — цикл ABAB.
    let mut d = DetectorState::default();
    let ab = [read("c1", "a"), read("c2", "b")];
    turn(
        &mut d,
        1,
        &ab,
        &[
            fact("c1", ToolKind::Read, "A"),
            fact("c2", ToolKind::Read, "B"),
        ],
    );
    turn(
        &mut d,
        2,
        &[read("c3", "c")],
        &[fact("c3", ToolKind::Read, "C")],
    );
    let (r, after) = turn(&mut d, 3, &[read("c4", "b"), read("c5", "a")], &[]);
    assert_eq!(
        r,
        vec![
            Reaction::ReusePrevious {
                call_id: "c2".into(),
                result: h("B"),
            },
            Reaction::ReusePrevious {
                call_id: "c1".into(),
                result: h("A"),
            },
        ],
        "реакции — по порядку вызовов"
    );
    assert_eq!(after, Reaction::None);
    let (_, after) = turn(&mut d, 4, &[read("c6", "c")], &[]);
    assert_eq!(after, Reaction::Nudge(CYCLE_NUDGE.to_owned()));

    // Контроль: другой набор в третьем ходу — не цикл.
    let mut d = DetectorState::default();
    turn(
        &mut d,
        1,
        &ab,
        &[
            fact("c1", ToolKind::Read, "A"),
            fact("c2", ToolKind::Read, "B"),
        ],
    );
    turn(
        &mut d,
        2,
        &[read("c3", "c")],
        &[fact("c3", ToolKind::Read, "C")],
    );
    turn(&mut d, 3, &[read("c4", "b"), read("c5", "d")], &[]);
    let (_, after) = turn(&mut d, 4, &[read("c6", "c")], &[]);
    assert_eq!(after, Reaction::None);
}

#[test]
fn abab_reflects_then_blocks() {
    let mut d = DetectorState::default();
    let x = |id: &str| read(id, "x");
    let y = |id: &str| read(id, "y");
    // Содержимое результатов одно и то же: новых фактов после ходов 1 и 2 нет.
    let fx = |id: &str| fact(id, ToolKind::Read, "X");
    let fy = |id: &str| fact(id, ToolKind::Read, "Y");
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
        let (c, f) = if is_x {
            (x(&id), fx(&id))
        } else {
            (y(&id), fy(&id))
        };
        let (_, after) = turn(&mut d, n, &[c], &[f]);
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
    let mut d = DetectorState::default();
    for n in 1..=8u32 {
        let id = format!("c{n}");
        let p = if n % 2 == 1 { "x" } else { "y" };
        let (_, after) = turn(
            &mut d,
            n,
            &[read(&id, p)],
            &[fact(&id, ToolKind::Read, &format!("content {n}"))],
        );
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

/// Инструмент чтения `read`: считает исполнения, результат — `content of <p>`.
struct Read {
    spec: ToolSpec,
    count: Arc<AtomicUsize>,
}

impl Tool for Read {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            ToolOutput {
                content: format!("content of {}", args["p"]),
                is_error: false,
            }
        })
    }
}

struct Fx {
    store: ProjectStore,
    model: Arc<ScriptedModel>,
    deps: LoopDeps,
    run: RunId,
    count: Arc<AtomicUsize>,
}

fn fx(name: &str) -> Fx {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("detect_repeat")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let store = ProjectStore::open(&d).unwrap();
    let model = Arc::new(ScriptedModel::new(default_caps(BackendKind::LlamaCpp)));
    let count = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::default();
    for t in builtin_tools() {
        reg.register(t).unwrap();
    }
    reg.register(Arc::new(Read {
        spec: ToolSpec {
            schema: ToolSchema {
                name: "read".into(),
                description: "t".into(),
                parameters: json!({"type": "object"}),
            },
            class: EffectClass::ReadOnly,
            kind: ToolKind::Read,
            approval: Approval::Mode,
            roles: vec![RunRole::Top, RunRole::Worker, RunRole::Orchestrator],
            min_tier: Tier::Small,
            timeout: Duration::from_secs(60),
            output_limit: None,
            tainted: false,
        },
        count: Arc::clone(&count),
    }))
    .unwrap();
    let deps = LoopDeps {
        journal: Journal::new(store.clone(), EventBus::new(EventBus::DEFAULT_CAPACITY)),
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
        count,
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
