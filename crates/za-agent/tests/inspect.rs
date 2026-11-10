//! Инспектор контекста и метрики run (R1-B2, задача 4): точное восстановление запроса
//! хода по журналу (D52, D67), разбивка по блокам, предсказание следующего хода (D41),
//! метрики run по журналу (D53).
//!
//! Run идут на `ScriptedModel`; журнал метрик собран вручную, чтобы цифры были точными.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};
use za_agent::{
    BlockSizes, ContextConfig, EnvInfo, FrozenPrefix, InspectError, LoopDeps, LoopError,
    LoopOutcome, NoMemory, RunSpec, builtin_tools, control_channel, inspect, request_hash,
    run_metrics, start_run,
};
use za_core::{
    Approval, EventBus, Journal, Tool, ToolCx, ToolKind, ToolOutput, ToolRegistry, ToolSpec,
};
use za_llm::{
    BackendKind, BoxFuture, ErrorClass, FinishReason, Message, ModelError, Part, Role,
    ScriptedModel, StreamEvent, ToolCall, ToolChoice, ToolSchema, Usage, default_caps,
};
use za_protocol::{
    Actor, BlobHash, Budget, EffectClass, EffectKey, Event, EventKind, NewEvent, Priority, RunId,
    RunMode, RunRole, RunStatus, Seq, Tier, canonical_json,
};
use za_store::{NewRun, ProjectStore, UsageRow};

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

fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("inspect")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Чтение с постоянным результатом `out`.
struct Fixed {
    spec: ToolSpec,
    out: String,
}

impl Tool for Fixed {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, _args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let content = self.out.clone();
        Box::pin(async move {
            ToolOutput {
                content,
                is_error: false,
            }
        })
    }
}

fn tool(name: &str, out: &str) -> Arc<dyn Tool> {
    Arc::new(Fixed {
        spec: ToolSpec {
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
            timeout: Duration::from_secs(600),
            output_limit: None,
            tainted: false,
        },
        out: out.into(),
    })
}

/// Размер результата инструмента `big` (D41: 400 байт — 100 токенов и 4 шаблона).
const BIG: usize = 400;

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
    reg.register(tool("note", "ok")).unwrap();
    reg.register(tool("big", &"x".repeat(BIG))).unwrap();
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
    ScriptedModel::new(default_caps(BackendKind::LlamaCpp)).with_context(32_768)
}

fn spec() -> RunSpec {
    RunSpec {
        project_id: "p".into(),
        project_dir: PathBuf::from("H:/p"),
        role: RunRole::Top,
        parent: None,
        endpoint: za_llm::EndpointId("a".into()),
        run_mode: RunMode::Ask,
        work_mode: None,
        tier: None,
        budget: Budget::default(),
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
    }
}

async fn start(f: &Fx) -> Result<LoopOutcome, LoopError> {
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    within("start_run", start_run(&f.deps, f.run, spec(), rx)).await
}

type Script = Vec<Result<StreamEvent, ModelError>>;

fn finished(parts: Vec<Part>, finish: FinishReason, usage: Usage) -> Script {
    vec![Ok(StreamEvent::Finished {
        message: Message {
            role: Role::Assistant,
            parts,
        },
        finish,
        usage,
    })]
}

fn answer(t: &str) -> Script {
    finished(
        vec![Part::Text(t.into())],
        FinishReason::Stop,
        Usage::default(),
    )
}

fn call(id: &str, name: &str, usage: Usage) -> Script {
    finished(
        vec![Part::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
            raw_arguments: None,
        })],
        FinishReason::ToolCalls,
        usage,
    )
}

fn done(t: &str) -> LoopOutcome {
    LoopOutcome::Finished {
        status: RunStatus::Done,
        answer: Some(t.into()),
        finish: None,
    }
}

async fn events(s: &ProjectStore, run: RunId) -> Vec<Event> {
    s.read_run(run, Seq(0), 100_000).await.unwrap()
}

/// `model.request` ходов (без компакции) по порядку.
fn turn_requests(evs: &[Event]) -> Vec<&Event> {
    evs.iter()
        .filter(|e| e.kind == EventKind::ModelRequest && e.payload["kind"].is_null())
        .collect()
}

/// Дописывает копию `model.request` с изменённым payload: инспектор берёт последний
/// запрос хода.
async fn append_request(s: &ProjectStore, run: RunId, payload: Value) {
    s.append(NewEvent {
        run_id: run,
        parent_seq: None,
        actor: Actor::Runtime,
        kind: EventKind::ModelRequest,
        payload,
        tainted: false,
    })
    .await
    .unwrap();
}

fn names(i: &za_agent::Inspection) -> Vec<&'static str> {
    i.blocks.iter().map(|b| b.name).collect()
}

// ---- Инспектор ----

#[tokio::test]
async fn inspection_reconstructs_request_exactly() {
    // Два пустых хода (D18): третий запрос — с `tool_choice = Required`, затем ответ.
    // Брифовое «второй ход — Required» по D18 недостижимо: `Required` получает ход после
    // второго пустого (EMPTY_TURNS_LIMIT 3).
    let f = fx("exact", scripted());
    f.model.push(answer(""));
    f.model.push(answer(""));
    f.model.push(call("c1", "note", Usage::default()));
    f.model.push(answer("готово"));
    assert_eq!(start(&f).await.unwrap(), done("готово"));
    let calls = f.model.calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[2].request.tool_choice, ToolChoice::Required);
    let evs = events(&f.store, f.run).await;
    let reqs = turn_requests(&evs);
    for (n, c) in calls.iter().enumerate() {
        let turn = u32::try_from(n + 1).unwrap();
        let i = inspect(&f.store, f.run, Some(turn))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("нет хода {turn}"));
        assert_eq!(i.turn, turn);
        assert_eq!(i.request, c.request, "ход {turn}");
        assert_eq!(
            reqs[n].payload["request"],
            json!(request_hash(&i.request).unwrap())
        );
        assert_eq!(i.endpoint, c.endpoint);
        assert_eq!(i.n_ctx, 32_768);
        assert_eq!(
            names(&i),
            [
                "tools",
                "core",
                "adapter",
                "overlay",
                "agents_md",
                "first_message",
                "dialog",
                "control"
            ]
        );
        // Ход у ScriptedModel без usage: итога провайдера нет, оценка — по D41.
        assert_eq!(i.input, Some(0));
        let msgs: u32 = c.request.messages.len().try_into().unwrap();
        let sum: u32 = i.blocks.iter().map(|b| b.tokens).sum();
        // Оценка — system целиком (без округления по блокам) и сообщения с шаблоном.
        assert!(
            i.estimate >= msgs * 4 && i.estimate.abs_diff(sum) <= 4,
            "{i:?}"
        );
    }
    // Без номера — последний ход.
    let last = inspect(&f.store, f.run, None).await.unwrap().unwrap();
    assert_eq!(last.turn, 4);
    assert_eq!(last.request, calls[3].request);
    // Размеры блоков system сходятся с самим system (разделители `\n\n`).
    let i = inspect(&f.store, f.run, Some(1)).await.unwrap().unwrap();
    let block = |n: &str| i.blocks.iter().find(|b| b.name == n).unwrap().clone();
    assert_eq!(block("core").bytes, i.request.system.len());
    assert_eq!(block("adapter").bytes, 0);
    assert_eq!(block("first_message").bytes, {
        let Part::Text(t) = &i.request.messages[0].parts[0] else {
            panic!("первое сообщение не текст");
        };
        t.len()
    });
    assert!(block("tools").hash.is_some() && block("core").hash.is_some());
    // Nudge пустого хода — блок 8 (напоминания), ответ ассистента — диалог.
    let i3 = inspect(&f.store, f.run, Some(3)).await.unwrap().unwrap();
    assert!(block_of(&i3, "control").bytes > 0, "{i3:?}");
    assert_eq!(block_of(&i3, "control").hash, None);
    // Хода 5 нет.
    assert!(inspect(&f.store, f.run, Some(5)).await.unwrap().is_none());
    // Run без событий — тоже нет хода.
    assert!(
        inspect(&f.store, RunId::new(), None)
            .await
            .unwrap()
            .is_none()
    );
}

fn block_of(i: &za_agent::Inspection, name: &str) -> za_agent::BlockCost {
    i.blocks.iter().find(|b| b.name == name).unwrap().clone()
}

#[tokio::test]
async fn request_hash_mismatch_is_bad() {
    let f = fx("mismatch", scripted());
    f.model.push(answer("готово"));
    assert_eq!(start(&f).await.unwrap(), done("готово"));
    let evs = events(&f.store, f.run).await;
    let mut p = turn_requests(&evs)[0].payload.clone();
    assert!(inspect(&f.store, f.run, Some(1)).await.unwrap().is_some());
    p["options"]["max_output_tokens"] = json!(1);
    append_request(&f.store, f.run, p).await;
    let err = inspect(&f.store, f.run, Some(1)).await.unwrap_err();
    assert!(matches!(err, InspectError::Bad(_)), "{err:?}");
    // Без `options` (журнал R1-B1) — тоже `Bad`, а не чужой запрос.
    let mut p = turn_requests(&evs)[0].payload.clone();
    p.as_object_mut().unwrap().remove("options");
    append_request(&f.store, f.run, p).await;
    let err = inspect(&f.store, f.run, Some(1)).await.unwrap_err();
    assert!(matches!(err, InspectError::Bad(_)), "{err:?}");
}

#[tokio::test]
async fn legacy_prefix_without_sizes() {
    let f = fx("legacy", scripted());
    f.model.push(answer("готово"));
    assert_eq!(start(&f).await.unwrap(), done("готово"));
    let evs = events(&f.store, f.run).await;
    let mut p = turn_requests(&evs)[0].payload.clone();
    let frozen_ref = BlobHash(p["frozen"].as_str().unwrap().to_owned());
    let raw = f.store.get_blob(&frozen_ref).await.unwrap().unwrap();
    let mut frozen: Value = serde_json::from_slice(&raw).unwrap();
    let sizes: BlockSizes = serde_json::from_value(frozen["sizes"].clone()).unwrap();
    assert!(sizes.core > 0);
    frozen.as_object_mut().unwrap().remove("sizes");
    let legacy: FrozenPrefix = serde_json::from_value(frozen.clone()).unwrap();
    assert_eq!(legacy.sizes, None);
    let h = f
        .store
        .put_blob(canonical_json(&frozen).as_bytes())
        .await
        .unwrap();
    p["frozen"] = json!(h.0);
    append_request(&f.store, f.run, p).await;
    let i = inspect(&f.store, f.run, Some(1)).await.unwrap().unwrap();
    assert_eq!(
        names(&i),
        ["tools", "system", "first_message", "dialog", "control"]
    );
    assert_eq!(block_of(&i, "system").bytes, i.request.system.len());
    assert_eq!(i.request, f.model.calls()[0].request);
}

#[tokio::test]
async fn retried_turn_uses_last_request() {
    // `ContextOverflow` первой попытки — аварийная компакция и повтор хода 1 (D49).
    let f = fx("retried", scripted().with_context(8192));
    f.model.push(vec![Err(ModelError::new(
        ErrorClass::ContextOverflow,
        "ctx",
    ))]);
    f.model.push(answer(
        "## Decisions\nd\n## Files changed\n-\n## Errors\n-\n## Next step\nn",
    ));
    f.model.push(answer("готово"));
    assert_eq!(start(&f).await.unwrap(), done("готово"));
    let evs = events(&f.store, f.run).await;
    let attempts: Vec<(&Value, &Value)> = turn_requests(&evs)
        .iter()
        .map(|e| (&e.payload["turn"], &e.payload["turn_attempt"]))
        .collect();
    assert_eq!(attempts, [(&json!(1), &json!(0)), (&json!(1), &json!(1))]);
    let calls = f.model.calls();
    assert_eq!(calls.len(), 3);
    let i = inspect(&f.store, f.run, Some(1)).await.unwrap().unwrap();
    assert_eq!(i.request, calls[2].request);
    assert_ne!(i.request, calls[0].request);
    assert_eq!(i.n_ctx, 8192);
}

#[tokio::test]
async fn next_turn_prediction() {
    // Счётчик модели 1 токен на текст: оценки цикла малы, ни обрезки (D43), ни
    // разгрузки — история между ходами только дополняется.
    let f = fx("next", scripted().with_token_counter(|_| 1));
    f.model.push(call("c1", "note", Usage::default()));
    f.model.push(call("c2", "note", Usage::default()));
    let usage = Usage {
        input: 900,
        output: 100,
        ..Usage::default()
    };
    f.model.push(call("c3", "big", usage));
    f.model.push(answer("готово"));
    assert_eq!(start(&f).await.unwrap(), done("готово"));
    let calls = f.model.calls();
    // После хода 3 в истории — его ответ и одно сообщение результата в 400 байт.
    let (r3, r4) = (&calls[2].request.messages, &calls[3].request.messages);
    assert_eq!(r4.len(), r3.len() + 2);
    let Part::ToolResult { content, .. } = &r4[r4.len() - 1].parts[0] else {
        panic!("последнее сообщение не результат: {:?}", r4.last());
    };
    assert_eq!(content.len(), BIG);
    let i = inspect(&f.store, f.run, Some(3)).await.unwrap().unwrap();
    assert_eq!(i.input, Some(900));
    assert_eq!(i.next_turn, 1000 + 104);
    // У последнего хода базы нет (`usage.input == 0`): предсказание — запрос целиком.
    let last = inspect(&f.store, f.run, Some(4)).await.unwrap().unwrap();
    assert!(last.next_turn > last.estimate, "{last:?}");
}

// ---- Метрики ----

async fn event(s: &ProjectStore, run: RunId, kind: EventKind, payload: Value) {
    s.append(NewEvent {
        run_id: run,
        parent_seq: None,
        actor: Actor::Runtime,
        kind,
        payload,
        tainted: false,
    })
    .await
    .unwrap();
}

async fn usage_event(s: &ProjectStore, run: RunId, turn: u32, u: Usage, ttft: u64, tps: f64) {
    let row = UsageRow {
        model: "a".into(),
        input: u.input,
        cached: u.cached,
        output: u.output,
        reasoning: u.reasoning,
        gpu_ms: u.gpu_ms,
        cost_usd: u.cost_usd,
    };
    let payload = json!({
        "turn": turn,
        "endpoint": "a",
        "effect_key": format!("k{turn}"),
        "usage": u,
        "finish": "stop",
        "ttft_ms": ttft,
        "duration_ms": ttft + 100,
        "output_tps": tps,
    });
    s.record_usage(run, row, payload).await.unwrap();
}

#[tokio::test]
async fn metrics_from_journal() {
    let store = ProjectStore::open(&dir("metrics")).unwrap();
    let run = RunId::new();
    store
        .create_run(NewRun {
            id: run,
            project_id: "p".into(),
            parent_id: None,
            role: RunRole::Top,
            model_profile: Some("a".into()),
        })
        .await
        .unwrap();
    for turn in 1..=2u32 {
        event(
            &store,
            run,
            EventKind::ModelRequest,
            json!({"turn": turn, "turn_attempt": 0, "effect_key": format!("k{turn}")}),
        )
        .await;
        let (input, cached, output, ttft, tps) = if turn == 1 {
            (100, 80, 10, 200, 50.0)
        } else {
            (120, 100, 20, 400, 100.0)
        };
        let u = Usage {
            input,
            cached,
            output,
            cost_usd: Some(0.25),
            ..Usage::default()
        };
        usage_event(&store, run, turn, u, ttft, tps).await;
        event(
            &store,
            run,
            EventKind::TurnFinished,
            json!({"turn": turn, "outcome": "calls", "calls": 1,
                   "repairs": turn - 1, "rejected": 0}),
        )
        .await;
    }
    // Компакция: запрос с `kind: "compact"` — не ход и не шаг.
    event(
        &store,
        run,
        EventKind::ModelRequest,
        json!({"turn": 2, "turn_attempt": 0, "effect_key": "kc", "kind": "compact"}),
    )
    .await;
    // Квитанция-отказ с ошибкой модели (D7).
    let key = EffectKey("k3".into());
    store
        .record_intent(run, &key, EffectClass::Idempotent, "model", json!({}))
        .await
        .unwrap();
    let body = json!({"error": ModelError::new(ErrorClass::Server, "500"), "last_wait": null});
    store
        .record_receipt(
            run,
            &key,
            json!({"outcome": "error", "message": format!("model_error:{body}")}),
        )
        .await
        .unwrap();

    let m = run_metrics(&store, run).await.unwrap();
    assert_eq!(m.turns, 2);
    assert_eq!(m.steps, 2);
    assert_eq!(m.input, 220);
    assert_eq!(m.cached, 180);
    assert!((m.cache_share - 180.0 / 220.0).abs() < 1e-9, "{m:?}");
    assert_eq!(m.output, 30);
    assert_eq!(m.ttft_p50_ms, Some(200));
    assert_eq!(m.ttft_p95_ms, Some(400));
    assert!((m.output_tps.unwrap() - 75.0).abs() < 1e-9, "{m:?}");
    assert_eq!(m.errors, [("server".to_owned(), 1)].into_iter().collect());
    assert_eq!(m.repairs, 1);
    assert_eq!(m.compactions, 1);
    assert!((m.cost_usd - 0.5).abs() < 1e-9, "{m:?}");

    // Run без событий — нули.
    let empty = run_metrics(&store, RunId::new()).await.unwrap();
    assert_eq!(empty, za_agent::RunMetrics::default());
}
