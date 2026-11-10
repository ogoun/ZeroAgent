//! Трасса дрейфа префикса (CTX-002, R1-B2 задача 9): золотой список запросов модели
//! длинного run с компакцией и тот же список после сбоя и resume.
//!
//! Сценарий: окно 8192, тир small (`Effort(Low)`: резерв 3072 — D40, компакция при
//! оценке ≥ 0,75 × 8192 − 3072 = 3072, удаление старых результатов — при оценке ≥ 2253).
//! Модель 9 раз вызывает `echo` с номером хода, инструмент отвечает 1600 байт ASCII
//! (своими для каждого хода: детекторы повтора и застоя молчат), десятый ход — ответ.
//! Модель не сообщает расход (`usage.input == 0`): оценка — весь запрос байтами / 4
//! (D41). RunId фиксирован: путь выгрузки при удалении результата содержит id run.
//!
//! Золотой файл `tests/golden/trace_prefixes.json` пишется первым прогоном (файла нет
//! или `ZA_UPDATE_GOLDEN=1`) и коммитится; меняется только вместе с намеренной правкой
//! промптов, запроса или правил контекста.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use za_agent::{
    ContextConfig, CrashPoint, EnvInfo, LoopDeps, LoopError, LoopOutcome, NoMemory, ResumeChange,
    RunSpec, builtin_tools, control_channel, resume_run, start_run,
};
use za_core::{
    Approval, EventBus, Journal, Tool, ToolCx, ToolKind, ToolOutput, ToolRegistry, ToolSpec,
};
use za_llm::{
    BackendKind, BoxFuture, ChatRequest, EndpointId, FinishReason, Message, ModelCall, ModelError,
    ModelPort, ModelStream, Part, ProviderCapabilities, Role, ScriptedModel, StreamEvent, ToolCall,
    ToolSchema, Usage, default_caps,
};
use za_protocol::{
    Budget, EffectClass, Event, EventKind, Priority, RunId, RunMode, RunRole, RunStatus, Seq, Tier,
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
    tokio::time::timeout(Duration::from_secs(30) * time_scale(), f)
        .await
        .unwrap_or_else(|_| panic!("не дождались в срок: {what}"))
}

/// Фиксированный id run (UUIDv7 с нулевым временем): трасса не зависит от прогона.
const RUN: &str = "00000000-0000-7000-8000-000000000009";

/// Окно сценария и тир.
const N_CTX: u32 = 8192;

/// Ходов с вызовом `echo`; ход `CALLS + 1` — ответ.
const CALLS: u32 = 9;

/// Длина вывода `echo`, байт: ниже лимита выгрузки 0,15 × 8192 × 4 = 4915 и выше
/// 1024 (результат можно удалить, D45).
const ECHO_BYTES: usize = 1600;

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/trace_prefixes.json")
}

/// Каталог теста: журнал и проект (выгрузка удалённых результатов).
fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("trace")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("project")).unwrap();
    d
}

/// `echo {"text": t}` возвращает `t`. Модель передаёт [`echo_text`] хода: 1600 байт
/// ASCII, свои для каждого хода.
struct Echo {
    spec: ToolSpec,
}

impl Tool for Echo {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let text = args["text"].as_str().unwrap_or_default().to_owned();
        Box::pin(async move {
            ToolOutput {
                content: text,
                is_error: false,
            }
        })
    }
}

fn echo_text(i: u32) -> String {
    let line = format!("echo line {i:04} abcdefghijklmnopqrstuvwxyz0123456789\n");
    line.repeat(ECHO_BYTES.div_ceil(line.len()))[..ECHO_BYTES].to_owned()
}

fn registry() -> ToolRegistry {
    let mut reg = ToolRegistry::default();
    for t in builtin_tools() {
        reg.register(t).unwrap();
    }
    reg.register(Arc::new(Echo {
        spec: ToolSpec {
            schema: ToolSchema {
                name: "echo".into(),
                description: "Echo test output".into(),
                parameters: json!({"type": "object"}),
            },
            class: EffectClass::ReadOnly,
            kind: ToolKind::Read,
            approval: Approval::Mode,
            roles: vec![RunRole::Top],
            min_tier: Tier::Small,
            timeout: Duration::from_secs(600),
            output_limit: None,
            tainted: false,
        },
    }))
    .unwrap();
    reg
}

struct Fx {
    store: ProjectStore,
    model: Arc<Responder>,
    deps: LoopDeps,
    run: RunId,
    project: PathBuf,
}

fn fx(name: &str) -> Fx {
    let d = dir(name);
    let store = ProjectStore::open(&d).unwrap();
    let model = Arc::new(Responder {
        inner: ScriptedModel::new(default_caps(BackendKind::LlamaCpp)).with_context(N_CTX),
    });
    let deps = LoopDeps {
        journal: Journal::new(store.clone(), EventBus::new(EventBus::DEFAULT_CAPACITY)),
        model: model.clone(),
        tools: Arc::new(registry()),
        capabilities: Vec::new(),
        memory: Arc::new(NoMemory),
        crash: None,
    };
    Fx {
        store,
        model,
        deps,
        run: RunId(uuid::Uuid::parse_str(RUN).unwrap()),
        project: d.join("project"),
    }
}

fn spec(project: &std::path::Path) -> RunSpec {
    RunSpec {
        project_id: "p".into(),
        project_dir: project.to_path_buf(),
        role: RunRole::Top,
        parent: None,
        endpoint: EndpointId("a".into()),
        run_mode: RunMode::Ask,
        work_mode: None,
        tier: Some(Tier::Small),
        budget: Budget::default(),
        priority: Priority::Top,
        max_wait: None,
        task: "Collect the echo outputs and report".into(),
        agents_md: None,
        env: EnvInfo {
            os: "windows".into(),
            shell: "powershell".into(),
            project_dir: "H:/p".into(),
            date: "2026-11-10".into(),
            tier: Tier::Small,
            n_ctx: N_CTX,
            budget: "x".into(),
            branch: "main".into(),
        },
        context: ContextConfig::default(),
    }
}

type Script = Vec<Result<StreamEvent, ModelError>>;

fn finished(parts: Vec<Part>, finish: FinishReason) -> Script {
    vec![Ok(StreamEvent::Finished {
        message: Message {
            role: Role::Assistant,
            parts,
        },
        finish,
        // Провайдер не сообщил расход (D41: оценка — весь запрос).
        usage: Usage::default(),
    })]
}

/// Ответ на запрос: сводка на запрос компакции, иначе вызов `echo` с номером хода
/// (`turn_calls` — запросов ходов до этого) или после `CALLS` вызовов финальный ответ.
fn reply_for(req: &ChatRequest, turn_calls: u32) -> Script {
    if is_compaction(req) {
        return finished(
            vec![Part::Text(
                "## Decisions\nkeep echoing\n## Files changed\n-\n## Errors\n-\n## Next step\ncall echo with the next number".into(),
            )],
            FinishReason::Stop,
        );
    }
    if turn_calls < CALLS {
        let i = turn_calls + 1;
        finished(
            vec![Part::ToolCall(ToolCall {
                id: format!("call_{i}"),
                name: "echo".into(),
                arguments: json!({ "text": echo_text(i) }),
                raw_arguments: None,
            })],
            FinishReason::ToolCalls,
        )
    } else {
        finished(vec![Part::Text("готово".into())], FinishReason::Stop)
    }
}

/// Модель сценария: число компакций заранее не задаётся, поэтому ответ выбирается по
/// запросу ([`reply_for`]) и кладётся в очередь [`ScriptedModel`] перед вызовом. Реплей
/// после сбоя модель не вызывает (ответ — из квитанции), счёт ходов не сбивается.
struct Responder {
    inner: ScriptedModel,
}

impl Responder {
    /// Запросы ходов (без компакций), отправленные модели.
    fn turn_calls(&self) -> u32 {
        let n = self
            .inner
            .calls()
            .iter()
            .filter(|c| !is_compaction(&c.request))
            .count();
        u32::try_from(n).unwrap()
    }

    fn compactions(&self) -> usize {
        self.inner
            .calls()
            .iter()
            .filter(|c| is_compaction(&c.request))
            .count()
    }
}

impl ModelPort for Responder {
    fn stream(&self, call: ModelCall) -> BoxFuture<'_, Result<ModelStream, ModelError>> {
        self.inner.push(reply_for(&call.request, self.turn_calls()));
        self.inner.stream(call)
    }

    fn capabilities(&self, endpoint: &EndpointId) -> Option<ProviderCapabilities> {
        self.inner.capabilities(endpoint)
    }

    fn context_tokens(&self, endpoint: &EndpointId) -> Option<u32> {
        self.inner.context_tokens(endpoint)
    }

    fn record_parse_error(&self, run: RunId, call: &ModelCall) {
        self.inner.record_parse_error(run, call);
    }

    fn finish_run(&self, run: RunId) {
        self.inner.finish_run(run);
    }
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

fn is_compaction(req: &ChatRequest) -> bool {
    req.messages
        .last()
        .is_some_and(|m| text_of(m).starts_with("<control from=\"runtime\" action=\"compact\""))
}

async fn events(s: &ProjectStore, run: RunId) -> Vec<Event> {
    s.read_run(run, Seq(0), 100_000).await.unwrap()
}

fn of_kind(evs: &[Event], k: EventKind) -> Vec<&Event> {
    evs.iter().filter(|e| e.kind == k).collect()
}

/// Трасса: `(turn, turn_attempt, kind, prefix, request)` событий `model.request`.
fn trace(evs: &[Event]) -> Value {
    Value::Array(
        of_kind(evs, EventKind::ModelRequest)
            .iter()
            .map(|e| {
                let p = &e.payload;
                json!({
                    "turn": p["turn"],
                    "turn_attempt": p["turn_attempt"],
                    "kind": p["kind"],
                    "prefix": p["prefix"],
                    "request": p["request"],
                })
            })
            .collect(),
    )
}

fn done() -> LoopOutcome {
    LoopOutcome::Finished {
        status: RunStatus::Done,
        answer: Some("готово".into()),
        finish: None,
    }
}

async fn start(f: &Fx) -> Result<LoopOutcome, LoopError> {
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    within("start_run", start_run(&f.deps, f.run, spec(&f.project), rx)).await
}

async fn resume(f: &Fx) -> LoopOutcome {
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    within(
        "resume_run",
        resume_run(&f.deps, f.run, ResumeChange::default(), rx),
    )
    .await
    .unwrap()
}

/// Эталонный прогон без сбоя: события run и число запросов компакции у модели.
async fn reference(name: &str) -> (Vec<Event>, usize) {
    let f = fx(name);
    assert_eq!(start(&f).await.unwrap(), done());
    (events(&f.store, f.run).await, f.model.compactions())
}

/// Золотая трасса из файла; `None` — файла нет или `ZA_UPDATE_GOLDEN=1`.
fn read_golden() -> Option<Value> {
    if std::env::var("ZA_UPDATE_GOLDEN").is_ok_and(|v| v == "1") {
        return None;
    }
    let text = std::fs::read_to_string(golden_path()).ok()?;
    Some(serde_json::from_str(&text).expect("golden/trace_prefixes.json — не JSON"))
}

/// Золотая трасса: из файла, а если его нет — из эталонного прогона (файл пишется).
async fn golden() -> Value {
    static WRITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _g = WRITE.lock().await;
    if let Some(g) = read_golden() {
        return g;
    }
    let (evs, _) = reference("golden_write").await;
    let t = trace(&evs);
    let mut text = serde_json::to_string_pretty(&t).unwrap();
    text.push('\n');
    std::fs::write(golden_path(), text).unwrap();
    eprintln!("записан {}", golden_path().display());
    t
}

/// `(turn, request)` ходов без запросов компакции, по порядку.
fn turn_requests(t: &Value) -> Vec<(u64, Value)> {
    t.as_array()
        .unwrap()
        .iter()
        .filter(|r| r["kind"] != "compact")
        .map(|r| (r["turn"].as_u64().unwrap(), r["request"].clone()))
        .collect()
}

// ---- Тесты брифа ----

#[tokio::test]
async fn trace_prefix_hashes_match_golden() {
    let g = golden().await;
    let (evs, compactions) = reference("reference").await;
    let t = trace(&evs);
    assert_eq!(
        t, g,
        "трасса запросов разошлась с golden/trace_prefixes.json"
    );

    let rows = t.as_array().unwrap();
    // Ходы 1..10 — по одному запросу, повторов хода в сценарии нет.
    let turns: Vec<u64> = turn_requests(&t).iter().map(|(n, _)| *n).collect();
    assert_eq!(turns, (1..=u64::from(CALLS) + 1).collect::<Vec<_>>());
    assert!(rows.iter().all(|r| r["turn_attempt"] == 0), "{t:#}");
    // Префикс заморожен на run: одинаков у всех запросов, в том числе у компакции.
    let prefix = &rows[0]["prefix"];
    assert!(prefix.is_string(), "{t:#}");
    assert!(rows.iter().all(|r| &r["prefix"] == prefix), "{t:#}");
    // Каждый запрос уникален.
    let mut reqs: Vec<&str> = rows
        .iter()
        .map(|r| r["request"].as_str().unwrap())
        .collect();
    reqs.sort_unstable();
    reqs.dedup();
    assert_eq!(reqs.len(), rows.len(), "{t:#}");
    // Компакция была; запросов компакции у модели столько же, сколько строк `compact`.
    let compacted = of_kind(&evs, EventKind::ContextCompacted);
    assert!(!compacted.is_empty(), "компакции нет: {t:#}");
    let compact_rows = rows.iter().filter(|r| r["kind"] == "compact").count();
    assert_eq!(compact_rows, compactions);
    assert_eq!(compacted.len(), compactions);
    // Детекторы не вмешивались.
    assert!(of_kind(&evs, EventKind::LoopDetected).is_empty());
}

/// Сбой в точке `at` один раз, resume: запросы ходов совпадают с golden, сводка не
/// запрашивается второй раз.
async fn crash_and_resume(name: &str, at: CrashPoint, golden: &Value, compactions: usize) {
    let mut f = fx(name);
    let fired = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&fired);
    f.deps.crash = Some(Arc::new(move |p: CrashPoint| {
        p == at && !flag.swap(true, Ordering::SeqCst)
    }));
    assert!(
        matches!(start(&f).await, Err(LoopError::Halted)),
        "сбой {at:?} не сработал"
    );
    assert!(fired.load(Ordering::SeqCst));
    assert_eq!(resume(&f).await, done());
    let evs = events(&f.store, f.run).await;
    assert_eq!(
        turn_requests(&trace(&evs)),
        turn_requests(golden),
        "запросы ходов после {at:?} и resume разошлись с golden"
    );
    assert_eq!(f.model.compactions(), compactions, "после {at:?}");
}

#[tokio::test]
async fn trace_replay_after_crash_matches() {
    let g = golden().await;
    let (ref_evs, compactions) = reference("replay_ref").await;
    assert_eq!(trace(&ref_evs), g);
    // (а) Сбой после эффекта model хода 7: шаг — из `turn.started {turn: 7}` эталона.
    let step7 = of_kind(&ref_evs, EventKind::TurnStarted)
        .iter()
        .find(|e| e.payload["turn"] == 7)
        .and_then(|e| e.payload["step"].as_u64())
        .expect("нет turn.started хода 7");
    crash_and_resume(
        "crash_model",
        CrashPoint::AfterModelEffect { step: step7 },
        &g,
        compactions,
    )
    .await;
    // (б) Сбой после квитанции первой компакции эталона.
    let step = of_kind(&ref_evs, EventKind::ContextCompacted)
        .first()
        .and_then(|e| e.payload["step"].as_u64())
        .expect("нет context.compacted");
    crash_and_resume(
        "crash_compact",
        CrashPoint::AfterCompact { step },
        &g,
        compactions,
    )
    .await;
}
