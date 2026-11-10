//! Бюджеты run (задача 13): проверка пределов (D19), активное время, выход по
//! бюджету на границе до приёма ввода (D32) чистой паузой (D5).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use za_agent::{
    Conversation, Counters, EnvInfo, LoopDeps, LoopError, LoopOutcome, Next, NoMemory, PendingExit,
    RunSpec, Snapshot, Spent, builtin_tools, check_budget, control_channel, start_run,
};
use za_core::{
    Approval, EventBus, Journal, Tool, ToolCx, ToolKind, ToolOutput, ToolRegistry, ToolSpec,
};
use za_llm::{
    BackendKind, BoxFuture, EndpointId, FinishReason, Message, ModelError, Part, Role,
    ScriptedModel, StreamEvent, ToolCall, ToolSchema, Usage, default_caps,
};
use za_protocol::{
    Actor, Budget, BudgetKind, EffectClass, Event, EventKind, NewEvent, Priority, RunId, RunMode,
    RunRole, RunStatus, Seq, Tier,
};
use za_store::{ProjectStore, StoreOptions};

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
        .join("budget")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        schema: ToolSchema {
            name: name.into(),
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
    }
}

/// Инструмент чтения `echo`: результат — аргументы.
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

/// Инструмент `nap`: спит [`NAP`]. Задаёт нижнюю границу активного времени шага —
/// не порог: тест только требует, чтобы прошло не меньше.
struct Nap {
    spec: ToolSpec,
}

const NAP: Duration = Duration::from_millis(20);

impl Tool for Nap {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, _args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            tokio::time::sleep(NAP).await;
            ToolOutput {
                content: "slept".into(),
                is_error: false,
            }
        })
    }
}

/// Инструмент `inject`: во время хода пишет `turn.input {text: "ещё", mode: "queue"}`
/// прямо через `store.append` (без сигнала `Input`: `submit_input` — задача 14).
struct Inject {
    spec: ToolSpec,
    store: ProjectStore,
}

impl Tool for Inject {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, _args: Value, cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let store = self.store.clone();
        Box::pin(async move {
            store
                .append(NewEvent {
                    run_id: cx.run,
                    parent_seq: None,
                    actor: Actor::User,
                    kind: EventKind::TurnInput,
                    payload: json!({"text": "ещё", "mode": "queue"}),
                    tainted: false,
                })
                .await
                .expect("turn.input не записан");
            ToolOutput {
                content: "injected".into(),
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
}

fn fx(name: &str) -> Fx {
    let store = ProjectStore::open_with(&dir(name), StoreOptions::default()).unwrap();
    let mut reg = ToolRegistry::default();
    for t in builtin_tools() {
        reg.register(t).unwrap();
    }
    reg.register(Arc::new(Echo {
        spec: tool_spec("echo"),
    }))
    .unwrap();
    reg.register(Arc::new(Inject {
        spec: tool_spec("inject"),
        store: store.clone(),
    }))
    .unwrap();
    reg.register(Arc::new(Nap {
        spec: tool_spec("nap"),
    }))
    .unwrap();
    let model = Arc::new(ScriptedModel::new(default_caps(BackendKind::LlamaCpp)));
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

fn spec(budget: Budget) -> RunSpec {
    RunSpec {
        project_id: "p".into(),
        project_dir: PathBuf::from("H:/p"),
        role: RunRole::Worker,
        parent: None,
        endpoint: EndpointId("local".into()),
        run_mode: RunMode::Ask,
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
        context: za_agent::ContextConfig::default(),
    }
}

async fn start(f: &Fx, s: RunSpec) -> Result<LoopOutcome, LoopError> {
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    tokio::time::timeout(limit(), start_run(&f.deps, f.run, s, rx))
        .await
        .expect("start_run не завершился в срок")
}

/// Ход с одним вызовом `name` и расходом `usage`.
fn call_turn(id: &str, name: &str, usage: Usage) -> Vec<Result<StreamEvent, ModelError>> {
    vec![Ok(StreamEvent::Finished {
        message: Message {
            role: Role::Assistant,
            parts: vec![Part::ToolCall(ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: json!({}),
                raw_arguments: None,
            })],
        },
        finish: FinishReason::ToolCalls,
        usage,
    })]
}

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input,
        output,
        ..Usage::default()
    }
}

async fn events(s: &ProjectStore, run: RunId) -> Vec<Event> {
    tokio::time::timeout(limit(), s.read_run(run, Seq(0), 100_000))
        .await
        .expect("чтение журнала не завершилось в срок")
        .unwrap()
}

fn of_kind(evs: &[Event], k: EventKind) -> Vec<&Event> {
    evs.iter().filter(|e| e.kind == k).collect()
}

async fn snapshots(s: &ProjectStore, run: RunId) -> Vec<(Seq, Snapshot)> {
    tokio::time::timeout(limit(), s.snapshots(run))
        .await
        .expect("чтение снимков не завершилось в срок")
        .unwrap()
        .into_iter()
        .map(|(seq, v)| (seq, serde_json::from_value(v).unwrap()))
        .collect()
}

async fn status(s: &ProjectStore, run: RunId) -> RunStatus {
    tokio::time::timeout(limit(), s.get_run(run))
        .await
        .expect("чтение run не завершилось в срок")
        .unwrap()
        .unwrap()
        .status
}

async fn history(s: &ProjectStore, snap: &Snapshot) -> Conversation {
    tokio::time::timeout(limit(), Conversation::load(s, &snap.messages))
        .await
        .expect("загрузка истории не завершилась в срок")
        .unwrap()
}

fn texts(c: &Conversation) -> Vec<String> {
    c.messages()
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

// ---- Тесты брифа ----

#[test]
fn check_order_and_equality() {
    let b = Budget {
        tokens: Some(100),
        steps: Some(3),
        duration_ms: Some(1000),
        ..Default::default()
    };
    assert_eq!(
        check_budget(
            &b,
            &Spent {
                tokens: 100,
                steps: 2,
                duration_ms: 1000,
                ..Default::default()
            }
        ),
        None
    );
    assert_eq!(
        check_budget(
            &b,
            &Spent {
                tokens: 101,
                steps: 3,
                ..Default::default()
            }
        ),
        Some((BudgetKind::Tokens, 101.0, 100.0))
    );
    assert_eq!(
        check_budget(
            &b,
            &Spent {
                tokens: 50,
                steps: 3,
                ..Default::default()
            }
        ),
        Some((BudgetKind::Steps, 3.0, 3.0))
    );
    assert_eq!(
        check_budget(
            &b,
            &Spent {
                steps: 1,
                duration_ms: 1001,
                ..Default::default()
            }
        ),
        Some((BudgetKind::Duration, 1001.0, 1000.0))
    );
}

#[tokio::test]
async fn run_blocks_on_budget() {
    let f = fx("blocks_on_budget");
    for i in 0..5 {
        f.model
            .push(call_turn(&format!("c{i}"), "echo", Usage::default()));
    }
    let out = start(
        &f,
        spec(Budget {
            steps: Some(2),
            ..Budget::default()
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        LoopOutcome::BlockedOnBudget {
            kind: BudgetKind::Steps
        }
    );
    assert_eq!(f.model.calls().len(), 2, "модель вызвана не ровно 2 раза");
    assert_eq!(status(&f.store, f.run).await, RunStatus::BlockedOnBudget);
    let evs = events(&f.store, f.run).await;
    let ex = of_kind(&evs, EventKind::BudgetExceeded);
    assert_eq!(ex.len(), 1, "budget.exceeded: {ex:?}");
    assert_eq!(
        ex[0].payload,
        json!({"kind": "steps", "used": 2, "limit": 2})
    );
    let snaps = snapshots(&f.store, f.run).await;
    let (seq, snap) = snaps.last().expect("нет снимков");
    assert_eq!(snap.next, Next::Boundary);
    assert_eq!(
        snap.exit,
        Some(PendingExit::BlockedOnBudget {
            budget: BudgetKind::Steps
        })
    );
    assert_eq!(*seq, ex[0].seq);
    // Пауза, не конечное состояние: finish_run не вызывается.
    assert!(f.model.finished_runs().is_empty());
}

#[tokio::test]
async fn budget_exit_leaves_input_unread() {
    let f = fx("input_unread");
    f.model.push(call_turn("c1", "inject", usage(50, 20)));
    f.model.push(call_turn("c2", "echo", Usage::default()));
    let out = start(
        &f,
        spec(Budget {
            tokens: Some(60),
            ..Budget::default()
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        LoopOutcome::BlockedOnBudget {
            kind: BudgetKind::Tokens
        }
    );
    assert_eq!(f.model.calls().len(), 1);
    let evs = events(&f.store, f.run).await;
    assert_eq!(
        of_kind(&evs, EventKind::TurnInput).len(),
        1,
        "turn.input не записан инструментом"
    );
    let ex = of_kind(&evs, EventKind::BudgetExceeded);
    assert_eq!(ex.len(), 1);
    assert_eq!(
        ex[0].payload,
        json!({"kind": "tokens", "used": 70, "limit": 60})
    );
    let snaps = snapshots(&f.store, f.run).await;
    let (seq, last) = snaps.last().expect("нет снимков");
    assert_eq!(*seq, ex[0].seq);
    assert_eq!(
        last.exit,
        Some(PendingExit::BlockedOnBudget {
            budget: BudgetKind::Tokens
        })
    );
    let h = history(&f.store, last).await;
    assert!(
        !texts(&h).iter().any(|t| t.contains("ещё")),
        "ввод попал в историю: {:?}",
        texts(&h)
    );
    let step1 = snaps
        .iter()
        .find(|(_, s)| s.step == 1 && s.next == Next::Model)
        .map(|(_, s)| s)
        .expect("нет снимка Model шага 1");
    assert_eq!(
        last.inbox_after, step1.inbox_after,
        "граница сдвинула inbox_after (D32)"
    );
}

#[test]
fn active_time_skips_pauses() {
    let mut c = Counters {
        active_ms: 0,
        last_boundary_ms: None,
        ..Counters::default()
    };
    c.tick(1000);
    assert_eq!(c.active_ms, 0);
    assert_eq!(c.last_boundary_ms, Some(1000));
    c.tick(1500);
    assert_eq!(c.active_ms, 500);
    assert_eq!(c.last_boundary_ms, Some(1500));
    // resume обнуляет last_boundary_ms (D19, задача 15).
    c.last_boundary_ms = None;
    c.tick(90_000);
    assert_eq!(c.active_ms, 500, "время в paused засчитано");
    assert_eq!(c.last_boundary_ms, Some(90_000));
}

#[tokio::test]
async fn tokens_counted_from_usage() {
    let f = fx("tokens_from_usage");
    f.model.push(call_turn("c1", "echo", usage(50, 20)));
    f.model.push(call_turn("c2", "echo", Usage::default()));
    let out = start(
        &f,
        spec(Budget {
            tokens: Some(60),
            ..Budget::default()
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        LoopOutcome::BlockedOnBudget {
            kind: BudgetKind::Tokens
        }
    );
    assert_eq!(
        f.model.calls().len(),
        1,
        "второй ход начался после превышения"
    );
    assert_eq!(status(&f.store, f.run).await, RunStatus::BlockedOnBudget);
}

// ---- Дополнительно ----

#[test]
fn spent_from_totals_sums_input_and_output() {
    let t = za_store::UsageTotals {
        calls: 2,
        input: 50,
        cached: 30,
        output: 20,
        reasoning: 5,
        gpu_ms: 7,
        cost_usd: 0.25,
    };
    assert_eq!(
        Spent::from_totals(&t, 3, 900),
        Spent {
            tokens: 70,
            gpu_ms: 7,
            usd: 0.25,
            steps: 3,
            duration_ms: 900,
        }
    );
}

#[test]
fn check_gpu_and_usd() {
    let b = Budget {
        gpu_ms: Some(10),
        usd: Some(0.5),
        ..Budget::default()
    };
    let at = |gpu_ms, usd| Spent {
        gpu_ms,
        usd,
        ..Spent::default()
    };
    assert_eq!(check_budget(&b, &at(10, 0.5)), None);
    assert_eq!(
        check_budget(&b, &at(11, 0.6)),
        Some((BudgetKind::GpuMs, 11.0, 10.0))
    );
    assert_eq!(
        check_budget(&b, &at(10, 0.75)),
        Some((BudgetKind::Usd, 0.75, 0.5))
    );
    assert_eq!(check_budget(&Budget::default(), &at(u64::MAX, 1e9)), None);
}

// ---- Бюджеты через драйвер: gpu_ms, usd, duration_ms (ревью задачи 13, M6) ----

/// Один ход с вызовом `tool` и расходом `usage`, затем ещё ходы про запас: регрессия
/// упадёт на утверждении, а не повиснет.
async fn blocked_after_one_turn(
    name: &str,
    tool: &str,
    usage: Usage,
    budget: Budget,
) -> (Fx, Event) {
    let f = fx(name);
    f.model.push(call_turn("c0", tool, usage));
    for i in 1..4 {
        f.model
            .push(call_turn(&format!("c{i}"), "echo", Usage::default()));
    }
    let out = start(&f, spec(budget)).await.unwrap();
    assert!(
        matches!(out, LoopOutcome::BlockedOnBudget { .. }),
        "{out:?}"
    );
    assert_eq!(f.model.calls().len(), 1, "второй ход не должен начаться");
    assert_eq!(status(&f.store, f.run).await, RunStatus::BlockedOnBudget);
    let evs = events(&f.store, f.run).await;
    let ex = of_kind(&evs, EventKind::BudgetExceeded);
    assert_eq!(ex.len(), 1, "budget.exceeded: {ex:?}");
    let ev = ex[0].clone();
    (f, ev)
}

#[tokio::test]
async fn loop_blocks_on_gpu_ms() {
    let usage = Usage {
        gpu_ms: Some(150),
        ..Usage::default()
    };
    let budget = Budget {
        gpu_ms: Some(100),
        ..Budget::default()
    };
    let (_, ev) = blocked_after_one_turn("loop_gpu", "echo", usage, budget).await;
    assert_eq!(
        ev.payload,
        json!({"kind": "gpu_ms", "used": 150, "limit": 100})
    );
}

#[tokio::test]
async fn loop_blocks_on_usd() {
    let usage = Usage {
        cost_usd: Some(0.75),
        ..Usage::default()
    };
    let budget = Budget {
        usd: Some(0.5),
        ..Budget::default()
    };
    let (_, ev) = blocked_after_one_turn("loop_usd", "echo", usage, budget).await;
    assert_eq!(
        ev.payload,
        json!({"kind": "usd", "used": 0.75, "limit": 0.5})
    );
}

/// Активное время: первая граница ничего не считает (`last_boundary_ms` нет), вторая
/// прибавляет время шага, а шаг держит инструмент `nap` не меньше [`NAP`]. Порога
/// настенного времени нет: предел 0, нужно лишь `used > 0`.
#[tokio::test]
async fn loop_blocks_on_duration() {
    let budget = Budget {
        duration_ms: Some(0),
        ..Budget::default()
    };
    let (f, ev) = blocked_after_one_turn("loop_duration", "nap", Usage::default(), budget).await;
    assert_eq!(ev.payload["kind"], "duration");
    assert_eq!(ev.payload["limit"], 0);
    let used = ev.payload["used"].as_u64().expect("used — целое");
    assert!(used >= 1, "used = {used}");
    let snaps = snapshots(&f.store, f.run).await;
    let (_, snap) = snaps.last().expect("нет снимков");
    assert_eq!(snap.counters.active_ms, used);
    assert!(snap.counters.last_boundary_ms.is_some());
}
