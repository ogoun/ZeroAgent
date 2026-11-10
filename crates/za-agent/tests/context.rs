//! Контекст модели (R1-B2, задача 1): бюджеты окна (D40), лимит вывода хода
//! (`OutputPlan`), раздел `[context]` (D65), счётчик токенов (D41), эффект `context`
//! на границе (D42), защита от тихой обрезки (D43), payload `model.request` (D67).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::Value;
use za_agent::{
    ContextBudget, ContextConfig, ContextMeter, EnvInfo, Estimate, LoopDeps, LoopError,
    LoopOutcome, NoMemory, OutputPlan, RunSpec, Snapshot, builtin_tools, control_channel,
    memory_budget, output_plan, start_run, truncation_suspected,
};
use za_core::{BoundaryInfo, Capability, EventBus, Hook, Journal, ToolRegistry};
use za_llm::{
    BackendKind, ChatRequest, Effort, EndpointId, FinishReason, Message, ModelError, Part,
    ReasoningMode, Role, ScriptedModel, StreamEvent, Usage, default_caps,
};
use za_protocol::{Budget, Event, EventKind, Priority, RunId, RunMode, RunRole, Seq, Tier};
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

/// Предел ожиданий, которые при регрессии повисли бы.
fn limit() -> Duration {
    Duration::from_secs(10) * time_scale()
}

fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("context")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Fx {
    store: ProjectStore,
    model: Arc<ScriptedModel>,
    deps: LoopDeps,
    run: RunId,
}

fn fx(name: &str, model: ScriptedModel) -> Fx {
    let store = ProjectStore::open(&dir(name)).unwrap();
    let model = Arc::new(model);
    let mut reg = ToolRegistry::default();
    for t in builtin_tools() {
        reg.register(t).unwrap();
    }
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
    }
}

fn scripted() -> ScriptedModel {
    ScriptedModel::new(default_caps(BackendKind::LlamaCpp))
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
        context: ContextConfig::default(),
    }
}

async fn start(f: &Fx, s: RunSpec) -> Result<LoopOutcome, LoopError> {
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
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
    vec![Ok(StreamEvent::Finished {
        message: text_msg(Role::Assistant, t),
        finish: FinishReason::Stop,
        usage: Usage::default(),
    })]
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

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input,
        output,
        ..Usage::default()
    }
}

fn request(system: &str, messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        system: system.into(),
        messages,
        ..ChatRequest::default()
    }
}

// ---- Тесты брифа ----

#[test]
fn budget_rows() {
    assert_eq!(ContextBudget::for_context(8192).output_reserve, 1000);
    assert_eq!(ContextBudget::for_context(16384).prompt, 900);
    assert_eq!(ContextBudget::for_context(32768).compaction_reserve, 3000);
    let b = ContextBudget::for_context(131072);
    assert_eq!(b.tools, None);
    assert_eq!(b.memory, 1500);
    assert_eq!(b.output_reserve, 8000);
    assert_eq!(ContextBudget::for_context(8192).memory, memory_budget(8192));
}

#[test]
fn output_plan_values() {
    let p = |n, r| output_plan(&ContextBudget::for_context(n), &r);
    assert_eq!(
        p(32768, ReasoningMode::Effort(Effort::Medium)),
        OutputPlan {
            max_output_tokens: 9216,
            reasoning: None
        }
    );
    assert_eq!(
        p(8192, ReasoningMode::Effort(Effort::Low)),
        OutputPlan {
            max_output_tokens: 3072,
            reasoning: None
        }
    );
    assert_eq!(
        p(8192, ReasoningMode::Off),
        OutputPlan {
            max_output_tokens: 1000,
            reasoning: None
        }
    );
    assert_eq!(
        p(4096, ReasoningMode::Effort(Effort::Medium)),
        OutputPlan {
            max_output_tokens: 2048,
            reasoning: Some(ReasoningMode::Budget(1024))
        }
    );
    assert_eq!(
        p(131072, ReasoningMode::Default),
        OutputPlan {
            max_output_tokens: 17408,
            reasoning: None
        }
    );
}

#[test]
fn config_ranges() {
    assert!(
        ContextConfig {
            compact_at: 0.85,
            ..Default::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        ContextConfig {
            spill_share: 0.10,
            ..Default::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        ContextConfig {
            prune_at: 0.80,
            compact_at: 0.75,
            ..Default::default()
        }
        .validate()
        .is_err()
    );
    assert!(ContextConfig::default().validate().is_ok());
    let e = ContextConfig {
        compact_at: 0.85,
        ..Default::default()
    }
    .validate()
    .unwrap_err();
    assert_eq!(e.field, "compact_at");
    assert!(e.to_string().starts_with("context.compact_at: 0.85"), "{e}");
    // Границы диапазонов допустимы.
    for (c, s) in [(0.70, 0.15), (0.80, 0.20)] {
        let cfg = ContextConfig {
            compact_at: c,
            spill_share: s,
            ..Default::default()
        };
        assert!(cfg.validate().is_ok(), "{cfg:?}");
    }
    let d = ContextConfig::default();
    assert_eq!(
        (d.tier, d.compact_at, d.prune_at, d.spill_share),
        (None, 0.75, 0.65, 0.15)
    );
}

#[tokio::test]
async fn estimate_uses_base_plus_delta() {
    let mut m = ContextMeter::default();
    m.after_turn(3, &usage(1000, 50));
    let big = "a".repeat(400);
    let msgs = vec![
        text_msg(Role::User, "x"),
        text_msg(Role::Assistant, "y"),
        text_msg(Role::User, "z"),
        text_msg(Role::User, &big),
        text_msg(Role::User, &big),
    ];
    let req = request("s", msgs);
    let ep = EndpointId("local".into());
    let model = scripted();
    assert_eq!(
        m.estimate(&req, &model, &ep).await,
        Estimate {
            tokens: 1258,
            exact: false
        }
    );
    let counting = scripted().with_token_counter(|s| s.len() as u32 / 2);
    assert_eq!(
        m.estimate(&req, &counting, &ep).await,
        Estimate {
            tokens: 1458,
            exact: true
        }
    );
}

#[tokio::test]
async fn estimate_without_base_counts_whole_request() {
    let req = request(
        &"s".repeat(4000),
        vec![text_msg(Role::User, &"a".repeat(400))],
    );
    let ep = EndpointId("local".into());
    let model = scripted();
    let want = Estimate {
        tokens: 1105,
        exact: false,
    };
    let mut m = ContextMeter::default();
    assert_eq!(m.estimate(&req, &model, &ep).await, want);
    // Провайдер не сообщил расход: базы нет.
    m.after_turn(1, &usage(0, 50));
    assert_eq!(m.base_tokens, None);
    assert_eq!(m.estimate(&req, &model, &ep).await, want);
    // После переписывания истории базы тоже нет.
    m.after_turn(1, &usage(500, 50));
    assert_eq!(m.base_tokens, Some(550));
    m.reset();
    assert_eq!(m.estimate(&req, &model, &ep).await, want);
}

#[test]
fn truncation_rule() {
    assert!(truncation_suspected(4000, 1500));
    assert!(!truncation_suspected(1000, 100));
    assert!(!truncation_suspected(4000, 2500));
    assert!(!truncation_suspected(4000, 0));
}

#[tokio::test]
async fn unknown_n_ctx_alerts_and_uses_8192() {
    let f = fx("unknown_n_ctx", scripted());
    f.model.push(answer("Париж"));
    start(&f, spec()).await.unwrap();
    let evs = events(&f.store, f.run).await;
    let alerts = of_kind(&evs, EventKind::ContextAlert);
    let unknown: Vec<_> = alerts
        .iter()
        .filter(|e| e.payload["kind"] == "n_ctx_unknown")
        .collect();
    assert_eq!(unknown.len(), 1, "{alerts:?}");
    assert_eq!(unknown[0].payload["n_ctx"], 8192);
    let snaps = snapshots(&f.store, f.run).await;
    let (seq0, s0) = &snaps[0];
    assert_eq!(s0.step, 0);
    assert_eq!(s0.setup.n_ctx, 8192);
    assert_eq!(s0.setup.tier, Tier::Small);
    assert_eq!(*seq0, unknown[0].seq, "снимок шага 0 не на событии alert");

    // Размер известен — события нет.
    let f = fx("known_n_ctx", scripted().with_context(32768));
    f.model.push(answer("Париж"));
    start(&f, spec()).await.unwrap();
    let evs = events(&f.store, f.run).await;
    assert!(
        of_kind(&evs, EventKind::ContextAlert)
            .iter()
            .all(|e| e.payload["kind"] != "n_ctx_unknown")
    );
}

#[tokio::test]
async fn turn_sets_output_plan() {
    let f = fx("output_plan", scripted().with_context(32768));
    f.model.push(answer("Париж"));
    start(&f, spec()).await.unwrap();
    let calls = f.model.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].request.max_output_tokens, Some(9216));
    assert_eq!(
        calls[0].request.reasoning,
        ReasoningMode::Effort(Effort::Medium)
    );
    let evs = events(&f.store, f.run).await;
    let req = of_kind(&evs, EventKind::ModelRequest);
    assert_eq!(req.len(), 1);
    let p = &req[0].payload;
    assert_eq!(p["options"]["max_output_tokens"], 9216);
    assert_eq!(p["turn_attempt"], 0);
    assert!(p["options"].get("tool_choice").is_some(), "{p}");
    assert!(p["options"].get("sampling").is_some(), "{p}");
    assert!(p["options"].get("reasoning").is_some(), "{p}");

    // `[context].tier` важнее размера окна; `--tier` (RunSpec.tier) важнее `[context].tier`.
    let f = fx("context_tier", scripted().with_context(131072));
    f.model.push(answer("Париж"));
    let mut s = spec();
    s.context.tier = Some(Tier::Standard);
    start(&f, s).await.unwrap();
    let (_, s0) = &snapshots(&f.store, f.run).await[0];
    assert_eq!(s0.setup.tier, Tier::Standard);
    let f = fx("flag_tier", scripted().with_context(131072));
    f.model.push(answer("Париж"));
    let mut s = spec();
    s.context.tier = Some(Tier::Standard);
    s.tier = Some(Tier::Small);
    start(&f, s).await.unwrap();
    let (_, s0) = &snapshots(&f.store, f.run).await[0];
    assert_eq!(s0.setup.tier, Tier::Small);
}

/// Хук границы: напоминание на каждой границе.
struct Remind;

impl Hook for Remind {
    fn at_boundary(&self, b: &BoundaryInfo) -> Option<String> {
        Some(format!("reminder {}", b.step))
    }
}

#[tokio::test]
async fn context_effect_after_reminders() {
    let mut f = fx("context_effect", scripted().with_context(32768));
    f.deps.capabilities = vec![Capability {
        name: "remind".into(),
        hooks: vec![Arc::new(Remind)],
        ..Capability::default()
    }];
    f.model.push(answer("Париж"));
    start(&f, spec()).await.unwrap();
    let evs = events(&f.store, f.run).await;
    let intents = of_kind(&evs, EventKind::EffectIntent);
    let seq_of = |name: &str| -> (Seq, Value) {
        let e = intents
            .iter()
            .find(|e| e.payload["name"] == name)
            .unwrap_or_else(|| panic!("нет эффекта {name}"));
        (e.seq, e.payload["args"].clone())
    };
    let (boundary, _) = seq_of("boundary");
    let (context, args) = seq_of("context");
    assert_eq!(args, serde_json::json!({"step": 1, "pass": 0}));
    assert!(boundary < context);
    let started = of_kind(&evs, EventKind::TurnStarted)[0].seq;
    assert!(context < started, "эффект context после turn.started");
    // Снимок Model шага 1 — на turn.started, после эффекта context.
    let snaps = snapshots(&f.store, f.run).await;
    let (seq1, s1) = snaps
        .iter()
        .find(|(_, s)| s.step == 1 && s.next == za_agent::Next::Model)
        .expect("нет снимка Model шага 1");
    assert_eq!(*seq1, started);
    // Оценка учитывает напоминание: оно уже в истории запроса.
    let est = s1.meter.last_estimate.expect("нет оценки");
    let calls = f.model.calls();
    let msgs = &calls[0].request.messages;
    assert!(msgs.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, Part::Text(t) if t.contains("reminder 1")))
    }));
    let rec = of_kind(&evs, EventKind::EffectReceipt)
        .into_iter()
        .find(|e| {
            e.payload["key"]
                == intents
                    .iter()
                    .find(|i| i.payload["name"] == "context")
                    .unwrap()
                    .payload["key"]
        })
        .expect("нет квитанции context");
    assert_eq!(rec.payload["result"]["value"]["tokens"], est);
    assert_eq!(rec.payload["result"]["value"]["exact"], false);
}

/// D43: расход заметно меньше оценки — `context.alert {truncation_suspected}` и
/// компакция на ближайшей границе.
#[tokio::test]
async fn truncation_alert_sets_compact_pending() {
    let f = fx("truncation", scripted().with_context(32768));
    f.model.push(vec![Ok(StreamEvent::Finished {
        message: text_msg(Role::Assistant, "Париж"),
        finish: FinishReason::Stop,
        usage: usage(10, 5),
    })]);
    // Задача длиннее 4096 байт: оценка запроса заведомо больше 1024.
    let mut s = spec();
    s.task = "x".repeat(8000);
    start(&f, s).await.unwrap();
    let evs = events(&f.store, f.run).await;
    let alerts = of_kind(&evs, EventKind::ContextAlert);
    let t: Vec<_> = alerts
        .iter()
        .filter(|e| e.payload["kind"] == "truncation_suspected")
        .collect();
    assert_eq!(t.len(), 1, "{alerts:?}");
    assert_eq!(t[0].payload["input"], 10);
    assert!(t[0].payload["estimate"].as_u64().unwrap() > 1024);
    let snaps = snapshots(&f.store, f.run).await;
    let (_, last) = snaps.last().unwrap();
    assert!(last.meter.compact_pending);
    // Без usage (input 0) проверка не делается.
    let f = fx("no_usage", scripted().with_context(32768));
    f.model.push(answer("Париж"));
    start(&f, spec()).await.unwrap();
    let evs = events(&f.store, f.run).await;
    assert!(
        of_kind(&evs, EventKind::ContextAlert)
            .iter()
            .all(|e| e.payload["kind"] != "truncation_suspected")
    );
}
