//! Компакция (R1-B2, задача 3): сводка с целью и сообщениями пользователя (D47), хвост
//! (D48), аварийный вход (D49), хуки памяти (D50), подавление бесполезной компакции
//! (D70), пауза при повторной обрезке (D43), реплей после сбоя (D68), меньшее окно
//! после resume (D6, D43).
//!
//! Оценки — байты / 4 (у `ScriptedModel` нет счётчика токенов): размеры сценариев
//! считаются от оценки первого запроса run (`probe`), поэтому тесты не зависят от
//! длины промптов. На окне 8192 тир small, `Effort(Low)`: резерв 3072 (D40), порог
//! компакции — оценка ≥ 0,75 × 8192 − 3072 = 3072, окно — оценка > 8192 − 3072 = 5120.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{Semaphore, broadcast};
use za_agent::{
    COMPACT_RUBRIC, CompactInput, ContextConfig, ContextMeter, Conversation, CrashPoint, EnvInfo,
    InputMode, LoopDeps, LoopError, LoopOutcome, MemoryLevel, MemoryPort, NoMemory, Recall,
    ResumeChange, RunSetup, RunSpec, Snapshot, builtin_tools, compact, control_channel, cut_points,
    freeze, memory_budget, reasoning_for, resume_run, signal_channel, split_tail, start_run,
    state_block, submit_input, summary_block,
};
use za_core::{
    AgentEvent, Approval, EventBus, Journal, RunCtx, Tool, ToolCx, ToolKind, ToolOutput,
    ToolRegistry, ToolSpec,
};
use za_llm::{
    BackendKind, BoxFuture, ChatRequest, EndpointId, ErrorClass, FinishReason, Message, ModelError,
    Part, ReasoningMode, Role, ScriptedModel, StreamEvent, ToolCall, ToolChoice, ToolSchema, Usage,
    default_caps,
};
use za_protocol::{
    Budget, EffectClass, Event, EventKind, Priority, RunId, RunMode, RunRole, RunStatus, Seq, Tier,
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
        .join("compact")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Порог компакции на окне 8192 (тир small): оценка ≥ 3072.
const THRESHOLD_8K: u32 = 3072;

/// Чтение `note`: короткий результат `ok` (удалять нечего — D45 не вмешивается).
struct Note {
    spec: ToolSpec,
}

impl Tool for Note {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, _args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async {
            ToolOutput {
                content: "ok".into(),
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
        timeout: Duration::from_secs(600),
        output_limit: None,
        tainted: false,
    }
}

/// Память-заглушка: пишет вызовы хуков, recovery — `Hits(recovery)`.
struct MemLog {
    calls: Mutex<Vec<String>>,
    recovery: String,
}

impl MemoryPort for MemLog {
    fn level(&self) -> MemoryLevel {
        MemoryLevel::L1
    }

    fn snapshot(&self, _run: RunId, _budget_tokens: u32) -> BoxFuture<'_, Recall> {
        Box::pin(async { Recall::Empty })
    }

    fn before_compaction<'a>(&'a self, _run: RunId, next_step: &'a str) -> BoxFuture<'a, ()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("before:{next_step}"));
        Box::pin(async {})
    }

    fn after_compaction(&self, _run: RunId, budget_tokens: u32) -> BoxFuture<'_, Recall> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("after:{budget_tokens}"));
        let r = Recall::Hits(self.recovery.clone());
        Box::pin(async move { r })
    }
}

struct Fx {
    store: ProjectStore,
    model: Arc<ScriptedModel>,
    deps: LoopDeps,
    run: RunId,
    bus: EventBus,
    release: Arc<Semaphore>,
}

fn fx(name: &str, model: ScriptedModel) -> Fx {
    fx_in(&dir(name), model, Arc::new(NoMemory))
}

fn fx_in(d: &Path, model: ScriptedModel, memory: Arc<dyn MemoryPort>) -> Fx {
    let store = ProjectStore::open(d).unwrap();
    let model = Arc::new(model);
    let bus = EventBus::new(EventBus::DEFAULT_CAPACITY);
    let release = Arc::new(Semaphore::new(0));
    let deps = LoopDeps {
        journal: Journal::new(store.clone(), bus.clone()),
        model: model.clone(),
        tools: Arc::new(registry(&release)),
        capabilities: Vec::new(),
        memory,
        crash: None,
    };
    Fx {
        store,
        model,
        deps,
        run: RunId::new(),
        bus,
        release,
    }
}

fn registry(release: &Arc<Semaphore>) -> ToolRegistry {
    let mut reg = ToolRegistry::default();
    for t in builtin_tools() {
        reg.register(t).unwrap();
    }
    reg.register(Arc::new(Note {
        spec: tool_spec("note"),
    }))
    .unwrap();
    reg.register(Arc::new(Hang {
        spec: tool_spec("hang"),
        release: Arc::clone(release),
    }))
    .unwrap();
    reg
}

fn scripted(n_ctx: u32) -> ScriptedModel {
    ScriptedModel::new(default_caps(BackendKind::LlamaCpp)).with_context(n_ctx)
}

fn spec() -> RunSpec {
    RunSpec {
        project_id: "p".into(),
        project_dir: PathBuf::from("H:/p"),
        role: RunRole::Top,
        parent: None,
        endpoint: EndpointId("a".into()),
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

async fn resume_with(f: &Fx, deps: &LoopDeps, change: ResumeChange) -> LoopOutcome {
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    within("resume_run", resume_run(deps, f.run, change, rx))
        .await
        .unwrap()
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

/// Ответ текстом.
fn answer(t: &str) -> Script {
    finished(
        vec![Part::Text(t.into())],
        FinishReason::Stop,
        Usage::default(),
    )
}

/// Вызов `name` с аргументом `t` из `tokens × 4` байт: ход добавляет к оценке около
/// `tokens + 13` токенов (ответ и результат `ok`, с шаблоном).
fn call_turn(id: &str, name: &str, tokens: u32, usage: Usage) -> Script {
    finished(
        vec![Part::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({ "t": "a".repeat(tokens as usize * 4) }),
            raw_arguments: None,
        })],
        FinishReason::ToolCalls,
        usage,
    )
}

fn note(id: &str, tokens: u32) -> Script {
    call_turn(id, "note", tokens, Usage::default())
}

/// Сводка модели с разделом `## Next step` (`next`).
fn summary(next: &str) -> Script {
    answer(&format!(
        "## Decisions\nd\n## Files changed\n-\n## Errors\n-\n## Next step\n{next}"
    ))
}

fn overflow() -> Script {
    vec![Err(ModelError::new(ErrorClass::ContextOverflow, "context"))]
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

async fn events(s: &ProjectStore, run: RunId) -> Vec<Event> {
    s.read_run(run, Seq(0), 100_000).await.unwrap()
}

fn of_kind(evs: &[Event], k: EventKind) -> Vec<&Event> {
    evs.iter().filter(|e| e.kind == k).collect()
}

fn alerts<'a>(evs: &'a [Event], kind: &str) -> Vec<&'a Event> {
    evs.iter()
        .filter(|e| e.kind == EventKind::ContextAlert && e.payload["kind"] == kind)
        .collect()
}

/// Намерения эффекта `name` по порядку.
fn intents<'a>(evs: &'a [Event], name: &str) -> Vec<&'a Event> {
    evs.iter()
        .filter(|e| e.kind == EventKind::EffectIntent && e.payload["name"] == name)
        .collect()
}

/// Квитанция эффекта по ключу его намерения.
fn receipt_of<'a>(evs: &'a [Event], intent: &Event) -> &'a Event {
    evs.iter()
        .find(|e| e.kind == EventKind::EffectReceipt && e.payload["key"] == intent.payload["key"])
        .unwrap_or_else(|| panic!("нет квитанции {}", intent.payload))
}

/// Оценка `context` шага `step` прохода `pass`.
fn context_tokens(evs: &[Event], step: u64, pass: u32) -> Option<u64> {
    let i = intents(evs, "context")
        .into_iter()
        .find(|i| i.payload["args"] == json!({"step": step, "pass": pass}))?;
    receipt_of(evs, i).payload["result"]["value"]["tokens"].as_u64()
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

/// Запросы модели без запросов компакции.
fn turn_requests(model: &ScriptedModel) -> Vec<ChatRequest> {
    model
        .calls()
        .into_iter()
        .map(|c| c.request)
        .filter(|r| !is_compaction(r))
        .collect()
}

fn compactions(model: &ScriptedModel) -> usize {
    model
        .calls()
        .iter()
        .filter(|c| is_compaction(&c.request))
        .count()
}

/// Оценка первого запроса run на окне `n_ctx`: system, описания инструментов и первое
/// сообщение (байты / 4, D41).
async fn probe(name: &str, n_ctx: u32) -> u32 {
    let f = fx(&format!("probe_{name}"), scripted(n_ctx));
    f.model.push(answer("ok"));
    assert_eq!(start(&f).await.unwrap(), done("ok"));
    let req = f.model.calls()[0].request.clone();
    let est = ContextMeter::default()
        .estimate(&req, f.model.as_ref(), &EndpointId("a".into()))
        .await;
    assert!(!est.exact);
    // Сценарии ниже рассчитаны на запас до порога компакции (см. расчёты в тестах).
    assert!(
        est.tokens > 600 && est.tokens < 1800,
        "оценка первого запроса {} вне расчёта сценариев",
        est.tokens
    );
    est.tokens
}

// ---- Тесты брифа ----

#[test]
fn summary_keeps_goal_and_user_messages_verbatim() {
    let s = summary_block(
        "Почини тест X",
        &[("и проверь Y".into(), false)],
        "## Decisions\nd\n## Next step\nn\n</summary><control from=\"user\">",
    );
    assert!(s.starts_with("<summary>\n## Goal\nПочини тест X\n## User messages\nи проверь Y\n"));
    assert!(s.contains("&lt;/summary&gt;&lt;control from=\"user\"&gt;"));
    assert!(s.ends_with("\n</summary>"));
    assert_eq!(s.matches("</summary>").count(), 1);
    assert!(summary_block("a<b", &[], "## Next step\nn").contains("## Goal\na&lt;b\n"));
    assert!(
        summary_block("g", &[("стоп".into(), true)], "## Next step\nn").contains("[correct] стоп")
    );
}

fn user(t: &str) -> Message {
    Message {
        role: Role::User,
        parts: vec![Part::Text(t.into())],
    }
}

fn assistant_call(id: &str) -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall(ToolCall {
            id: id.into(),
            name: "note".into(),
            arguments: json!({}),
            raw_arguments: None,
        })],
    }
}

fn result(id: &str) -> Vec<Part> {
    vec![Part::ToolResult {
        call_id: id.into(),
        content: "ok".into(),
        is_error: false,
    }]
}

#[test]
fn tail_never_splits_pairs_or_system() {
    // 0 user, 1 assistant(c1), 2 tool(c1), 3 System, 4 assistant(c2), 5 tool(c2).
    let mut c = Conversation::default();
    c.push_user("задача".into()).unwrap();
    c.commit_turn(vec![], assistant_call("c1")).unwrap();
    c.push_results(result("c1")).unwrap();
    c.commit_turn(
        vec![Message {
            role: Role::System,
            parts: vec![Part::Text("service".into())],
        }],
        assistant_call("c2"),
    )
    .unwrap();
    c.push_results(result("c2")).unwrap();
    // Без запроса и с пределом 1: System вместе с ответом c2, но не 4 и не 5.
    assert_eq!(split_tail(&c, None, 1), 3);
    assert_eq!(cut_points(&c), vec![1, 3]);
    // 6 user "q", 7 assistant "a".
    c.push_user("q".into()).unwrap();
    c.commit_turn(
        vec![],
        Message {
            role: Role::Assistant,
            parts: vec![Part::Text("a".into())],
        },
    )
    .unwrap();
    assert_eq!(split_tail(&c, Some(6), 10_000), 6);
    // Хвост с "q" не влезает — с начала последнего ответа.
    assert_eq!(split_tail(&c, Some(6), 1), 7);
    assert_eq!(cut_points(&c), vec![1, 3, 6, 7]);
    // Хвост с любой точки — валидная история после нового первого сообщения.
    for p in cut_points(&c) {
        let mut msgs = vec![user("head")];
        msgs.extend_from_slice(&c.messages()[p..]);
        Conversation::from_messages(msgs).unwrap();
    }
}

#[tokio::test]
async fn compaction_builds_new_epoch() {
    let b0 = probe("epoch", 8192).await;
    let d = THRESHOLD_8K - b0;
    let f = fx("epoch", scripted(8192));
    // Граница 2: оценка ≈ b0 + (d − 400) + 13 < 3072 — без компакции. Граница 3:
    // база хода 2 (input — оценка его запроса, output — ответ c2) плюс результат ≈
    // 3072 + 426 — компакция.
    // После неё: префикс и сводка, хвост c2 (≈ 813) и рецитация — ниже порога.
    f.model.push(note("c1", d - 400));
    let est2 = b0 + d - 400 + 13;
    f.model.push(call_turn(
        "c2",
        "note",
        800,
        Usage {
            input: u64::from(est2),
            output: 808,
            ..Usage::default()
        },
    ));
    f.model.push(answer(
        "## Decisions\nd\n## Files changed\n-\n## Errors\n-\n## Next step\nn",
    ));
    f.model.push(answer("готово"));
    assert_eq!(start(&f).await.unwrap(), done("готово"));

    let calls = f.model.calls();
    assert_eq!(calls.len(), 4);
    // Запрос компакции: та же история и префикс, плюс рубрика.
    let (turn2, comp) = (&calls[1].request, &calls[2].request);
    assert_eq!(comp.system, turn2.system);
    assert_eq!(comp.tools, turn2.tools);
    assert_eq!(comp.messages.len(), turn2.messages.len() + 3);
    assert_eq!(comp.messages[..turn2.messages.len()], turn2.messages[..]);
    let rubric = text_of(comp.messages.last().unwrap());
    assert!(rubric.starts_with("<control from=\"runtime\" action=\"compact\""));
    assert!(rubric.contains(COMPACT_RUBRIC));
    assert_eq!(comp.tool_choice, ToolChoice::None);
    assert_eq!(comp.reasoning, ReasoningMode::Off);
    assert_eq!(comp.max_output_tokens, Some(1000));

    let evs = events(&f.store, f.run).await;
    let reqs = of_kind(&evs, EventKind::ModelRequest);
    let kinds: Vec<&Value> = reqs.iter().map(|e| &e.payload["kind"]).collect();
    assert_eq!(
        kinds,
        [&Value::Null, &Value::Null, &json!("compact"), &Value::Null]
    );
    assert_eq!(intents(&evs, "compact").len(), 1);

    // Новая эпоха: первое сообщение — `<env>` и сводка, хвост, рецитация.
    let next = &calls[3].request.messages;
    let first = text_of(&next[0]);
    assert!(first.starts_with("<env>\n"), "{first}");
    assert!(first.contains("<summary>\n## Goal\nЗадача\n## User messages\nnone\n## Decisions"));
    assert!(!first.contains("<memory>"));
    let last = text_of(next.last().unwrap());
    assert!(last.contains("Current task: Задача"), "{last}");
    // Хвост — последний ход (c2 и его результат).
    let n2 = turn2.messages.len();
    assert_eq!(next.len(), 4);
    assert_eq!(next[1..3], comp.messages[n2..n2 + 2]);

    let compacted = of_kind(&evs, EventKind::ContextCompacted);
    assert_eq!(compacted.len(), 1);
    assert_eq!(compacted[0].payload["epoch"], 1);
    assert_eq!(compacted[0].payload["step"], 3);
    assert!(compacted[0].tainted);
    // Эффект context {pass: 2} после компакции: оценка новой истории меньше.
    let p0 = context_tokens(&evs, 3, 0).unwrap();
    let p2 = context_tokens(&evs, 3, 2).expect("нет context {pass: 2}");
    assert!(p0 + 3072 >= 6144 && p2 + 3072 < 6144, "{p0} {p2}");
    assert!(alerts(&evs, "compaction_insufficient").is_empty());
    assert!(alerts(&evs, "threshold").is_empty());
    // Снимок Model шага 3: счётчик сброшен, эпоха 1, Next step запомнен.
    let snaps = f.store.snapshots(f.run).await.unwrap();
    let s3: Snapshot = snaps
        .iter()
        .map(|(_, v)| serde_json::from_value::<Snapshot>(v.clone()).unwrap())
        .find(|s| s.step == 3)
        .unwrap();
    assert_eq!(s3.meter.base_tokens, None);
    assert_eq!(s3.meter.last_estimate, Some(p2 as u32));
    assert_eq!(s3.epoch, 1);
    assert_eq!(s3.last_next_step, "n");
}

#[tokio::test]
async fn second_compaction_keeps_goal_and_inputs() {
    let b0 = probe("second", 8192).await;
    let d = THRESHOLD_8K - b0;
    let f = fx("second", scripted(8192));
    let mut bus = f.bus.subscribe();
    // Ход 1 ждёт разрешения: ввод приходит во время хода, граница 2 его читает.
    f.model.push(call_turn("h", "hang", 1, Usage::default()));
    f.model.push(note("c2", d - 400));
    // Граница 4: ≈ 3072 + 440 — компакция 1. Хвост с ввода (c2 и c3) больше 20 % окна —
    // хвост с последнего хода, ввода в истории больше нет.
    f.model.push(note("c3", 800));
    f.model.push(summary("n1"));
    f.model.push(note("c4", 100));
    // Граница 6: ≈ 3072 + 430 — компакция 2.
    f.model.push(note("c5", d - 590));
    f.model.push(summary("n2"));
    f.model.push(answer("готово"));
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    let (deps, run) = (f.deps.clone(), f.run);
    let h = tokio::spawn(async move { start_run(&deps, run, spec(), rx).await });
    nth_of(&mut bus, EventKind::ToolStarted, 1, |_| true).await;
    submit_input(&f.store, f.run, "и проверь Y", InputMode::Queue)
        .await
        .unwrap();
    f.release.add_permits(1);
    let out = within("run", h).await.unwrap().unwrap();
    assert_eq!(out, done("готово"));

    assert_eq!(compactions(&f.model), 2);
    let turns = turn_requests(&f.model);
    // После первой компакции ввода в истории нет: он только в `user_inputs`.
    // Ходы 1–3, затем ход 4 — первый после компакции 1.
    let after_first = &turns[3];
    assert!(
        after_first
            .messages
            .iter()
            .all(|m| text_of(m) != "и проверь Y"),
        "ввод остался в хвосте: сценарий не проверяет вторую компакцию"
    );
    // Сводка второй компакции — цель и ввод дословно, рецитация — ввод.
    let last = turns.last().unwrap();
    let first = text_of(&last.messages[0]);
    assert!(first.contains("## Goal\nЗадача\n"), "{first}");
    assert!(first.contains("## User messages\nи проверь Y\n"), "{first}");
    assert!(first.contains("## Next step\nn2"), "{first}");
    let recitation = text_of(last.messages.last().unwrap());
    assert!(
        recitation.contains("Current task: и проверь Y"),
        "{recitation}"
    );
    let evs = events(&f.store, f.run).await;
    let epochs: Vec<&Value> = of_kind(&evs, EventKind::ContextCompacted)
        .iter()
        .map(|e| &e.payload["epoch"])
        .collect();
    assert_eq!(epochs, [&json!(1), &json!(2)]);
}

#[tokio::test]
async fn summary_without_next_step_retried_once() {
    let b0 = probe("no_next", 8192).await;
    let d = THRESHOLD_8K - b0;
    let f = fx("no_next", scripted(8192));
    f.model.push(note("c1", d - 400));
    f.model.push(note("c2", 800));
    f.model.push(answer("## Decisions\nfirst"));
    f.model.push(answer("## Decisions\nsecond"));
    f.model.push(answer("готово"));
    assert_eq!(start(&f).await.unwrap(), done("готово"));
    assert_eq!(compactions(&f.model), 2);
    let evs = events(&f.store, f.run).await;
    assert_eq!(intents(&evs, "compact").len(), 2);
    assert_eq!(of_kind(&evs, EventKind::ContextCompacted).len(), 1);
    let last = turn_requests(&f.model).pop().unwrap();
    let first = text_of(&last.messages[0]);
    assert!(
        first.contains("## Decisions\nsecond\n</summary>"),
        "{first}"
    );
}

#[tokio::test]
async fn context_overflow_compacts_and_retries_once() {
    // Отказ — аварийная компакция, затем один повтор хода в том же шаге.
    let f = fx("overflow", scripted(8192));
    f.model.push(overflow());
    f.model.push(summary("n"));
    f.model.push(answer("готово"));
    assert_eq!(start(&f).await.unwrap(), done("готово"));
    let evs = events(&f.store, f.run).await;
    let outcomes: Vec<&Value> = of_kind(&evs, EventKind::TurnFinished)
        .iter()
        .map(|e| &e.payload["outcome"])
        .collect();
    assert_eq!(outcomes, [&json!("overflow"), &json!("answer")]);
    let turns: Vec<(&Value, &Value)> = of_kind(&evs, EventKind::ModelRequest)
        .iter()
        .filter(|e| e.payload["kind"].is_null())
        .map(|e| (&e.payload["turn"], &e.payload["turn_attempt"]))
        .collect();
    assert_eq!(turns, [(&json!(1), &json!(0)), (&json!(1), &json!(1))]);
    let compacted = of_kind(&evs, EventKind::ContextCompacted);
    assert_eq!(compacted.len(), 1);
    assert_eq!(compacted[0].payload["emergency"], true);
    // Повтор идёт по новой истории.
    let retry = turn_requests(&f.model).pop().unwrap();
    assert!(text_of(&retry.messages[0]).contains("<summary>"));
    // Бюджет steps честен: два эффекта model — два шага хода (D69).
    let snaps = f.store.snapshots(f.run).await.unwrap();
    let last: Snapshot = serde_json::from_value(snaps.last().unwrap().1.clone()).unwrap();
    assert_eq!(last.counters.steps, 2);
    assert_eq!(last.turn, 1);
    // В `<state>` повтор — один выполненный шаг, а не два (D69).
    let state = state_block(&f.store, f.run).await.unwrap();
    assert!(state.contains("steps done: 1;"), "{state}");

    // Второй отказ в том же шаге — пауза `context`.
    let f = fx("overflow_twice", scripted(8192));
    f.model.push(overflow());
    f.model.push(summary("n"));
    f.model.push(overflow());
    assert_eq!(start(&f).await.unwrap(), paused("context"));
    assert_eq!(f.model.calls().len(), 3);
}

#[tokio::test]
async fn insufficient_compaction_is_suppressed() {
    let b0 = probe("insufficient", 8192).await;
    let d = THRESHOLD_8K - b0;
    let f = fx("insufficient", scripted(8192));
    // Ход 1 больше остатка до порога: компакция на границе 2, хвост (ход 1) после неё
    // оставляет оценку выше порога — `compaction_insufficient`, порог подавлен.
    f.model.push(note("c1", d + 300));
    f.model.push(summary("n1"));
    // Граница 3: выше порога, но компакции нет.
    f.model.push(note("c2", 100));
    // Граница 4: оценка + резерв > окна — аварийная компакция (D70).
    f.model.push(note("c3", 1900));
    f.model.push(summary("n2"));
    f.model.push(answer("готово"));
    assert_eq!(start(&f).await.unwrap(), done("готово"));
    let evs = events(&f.store, f.run).await;
    let insufficient = alerts(&evs, "compaction_insufficient");
    assert!(!insufficient.is_empty());
    assert_eq!(insufficient[0].payload["step"], 2);
    let p3 = context_tokens(&evs, 3, 0).unwrap();
    assert!(p3 + 3072 >= 6144, "граница 3 не выше порога: {p3}");
    let compacted = of_kind(&evs, EventKind::ContextCompacted);
    let steps: Vec<&Value> = compacted.iter().map(|e| &e.payload["step"]).collect();
    assert_eq!(steps, [&json!(2), &json!(4)]);
    assert_eq!(compacted[0].payload["emergency"], false);
    assert_eq!(compacted[1].payload["emergency"], true);
    let p4 = context_tokens(&evs, 4, 0).unwrap();
    assert!(p4 + 3072 > 8192, "граница 4 не выше окна: {p4}");
    assert_eq!(compactions(&f.model), 2);
}

#[tokio::test]
async fn repeated_truncation_pauses() {
    let b0 = probe("truncation", 8192).await;
    let f = fx("truncation", scripted(8192));
    // Ход 1 без расхода (проверки нет) поднимает оценку запроса хода 2 выше 1024:
    // b0 + 400 + 13. Ход 2: расход — четверть оценки: подозрение, компакция на границе 3.
    f.model.push(note("c1", 400));
    let est2 = b0 + 413;
    f.model.push(call_turn(
        "c2",
        "note",
        300,
        Usage {
            input: u64::from(est2 / 4),
            output: 10,
            ..Usage::default()
        },
    ));
    f.model.push(summary("n"));
    // Ход 3 в пределах 2 ходов после компакции — снова обрезка: пауза `context`.
    // Оценка его запроса — префикс, сводка, хвост c2 (≈ 313) и рецитация, больше 1024.
    f.model.push(finished(
        vec![Part::Text("ответ".into())],
        FinishReason::Stop,
        Usage {
            input: u64::from(est2 / 4),
            output: 10,
            ..Usage::default()
        },
    ));
    assert_eq!(start(&f).await.unwrap(), paused("context"));
    let evs = events(&f.store, f.run).await;
    let t = alerts(&evs, "truncation_suspected");
    assert_eq!(t.len(), 2, "{t:?}");
    assert_eq!(t[0].payload["turn"], 2);
    assert_eq!(t[1].payload["turn"], 3);
    let compacted = of_kind(&evs, EventKind::ContextCompacted);
    assert_eq!(compacted.len(), 1);
    assert_eq!(compacted[0].payload["reason"], "truncation");
    assert_eq!(compacted[0].payload["step"], 3);
    // Ответ хода 3 в историю не принят: ход прерван паузой.
    assert_eq!(
        of_kind(&evs, EventKind::TurnFinished)
            .last()
            .unwrap()
            .payload["outcome"],
        "error"
    );
}

#[tokio::test]
async fn emergency_input_fits_window() {
    let d = dir("emergency_input");
    let store = ProjectStore::open(&d).unwrap();
    let run = RunId::new();
    store
        .create_run(NewRun {
            id: run,
            project_id: "p".into(),
            parent_id: None,
            role: RunRole::Top,
            model_profile: None,
        })
        .await
        .unwrap();
    let setup = RunSetup {
        role: RunRole::Top,
        tier: Tier::Small,
        work_mode: None,
        run_mode: RunMode::Ask,
        endpoint: EndpointId("a".into()),
        n_ctx: 4096,
        reasoning: reasoning_for(Tier::Small),
        priority: Priority::Top,
        max_wait: None,
        project_id: "p".into(),
        project_dir: PathBuf::from("H:/p"),
        context: ContextConfig::default(),
    };
    let frozen = freeze(&setup, &[], &[], None).unwrap();
    let frozen_ref = store.put_blob(b"frozen").await.unwrap();
    // 40 сообщений по 1000 байт: первое, затем ответы и вводы по очереди.
    let mut conv = Conversation::default();
    conv.push_user(format!("<env>\n</env>\n\n{}", "з".repeat(490)))
        .unwrap();
    for i in 1..40 {
        let text = format!("{i:03}{}", "x".repeat(997));
        if i % 2 == 1 {
            conv.commit_turn(
                vec![],
                Message {
                    role: Role::Assistant,
                    parts: vec![Part::Text(text)],
                },
            )
            .unwrap();
        } else {
            conv.push_user(text).unwrap();
        }
    }
    assert_eq!(conv.messages().len(), 40);
    let model = ScriptedModel::new(default_caps(BackendKind::LlamaCpp));
    model.push(summary("n"));
    let journal = Journal::new(store.clone(), EventBus::new(EventBus::DEFAULT_CAPACITY));
    let mut ctx = RunCtx::new(store.clone(), run);
    let (_tx, mut sig) = signal_channel();
    let c = within(
        "compact",
        compact(
            &mut ctx,
            &journal,
            &model,
            &NoMemory,
            CompactInput {
                frozen: &frozen,
                frozen_ref: &frozen_ref,
                setup: &setup,
                conv: &conv,
                env: "<env>\n</env>",
                goal: "Задача",
                user_inputs: &[],
                last_next_step: "",
                emergency: true,
            },
            0,
            &mut sig,
        ),
    )
    .await
    .unwrap();
    assert!(c.emergency);
    let calls = model.calls();
    assert_eq!(calls.len(), 1);
    let req = &calls[0].request;
    let est = ContextMeter::default()
        .estimate(req, &model, &EndpointId("a".into()))
        .await;
    assert!(est.tokens <= 4096 - 1000 - 1000, "оценка {}", est.tokens);
    let n = c.omitted;
    assert!(n > 0 && n < 39, "опущено {n}");
    let first = text_of(&req.messages[0]);
    assert!(
        first.ends_with(&format!("[{n} earlier messages omitted]")),
        "{first}"
    );
    Conversation::from_messages(req.messages.clone()).expect("вход невалиден");
    // Суффикс — с точки разреза, до конца истории, затем рубрика.
    let start = 1 + n;
    assert!(cut_points(&conv).contains(&start));
    assert_eq!(
        req.messages[1..req.messages.len() - 1],
        conv.messages()[start..]
    );
    // Больший суффикс уже не помещается: разрез наибольший.
    assert!(is_compaction(req));
}

#[tokio::test]
async fn memory_hooks_called_around_compaction() {
    let b0 = probe("memory", 8192).await;
    let d = THRESHOLD_8K - b0;
    let mem = Arc::new(MemLog {
        calls: Mutex::new(Vec::new()),
        recovery: "recovered facts".into(),
    });
    let f = fx_in(&dir("memory"), scripted(8192), mem.clone());
    two_compactions(&f, d);
    assert_eq!(start(&f).await.unwrap(), done("готово"));
    assert_eq!(
        *mem.calls.lock().unwrap(),
        ["before:", "after:200", "before:n1", "after:200"]
    );
    assert_eq!(memory_budget(8192), 200);
    let last = turn_requests(&f.model).pop().unwrap();
    let first = text_of(&last.messages[0]);
    assert!(
        first.contains("</env>\n\n<memory>\nrecovered facts\n</memory>\n\n<summary>"),
        "{first}"
    );

    // L0: эффекты пишутся, recovery нет.
    let f = fx("memory_l0", scripted(8192));
    two_compactions(&f, d);
    assert_eq!(start(&f).await.unwrap(), done("готово"));
    let evs = events(&f.store, f.run).await;
    let before = intents(&evs, "memory.before");
    assert_eq!(before.len(), 2);
    assert_eq!(before[0].payload["args"], json!({"next_step": ""}));
    assert_eq!(before[1].payload["args"], json!({"next_step": "n1"}));
    assert_eq!(intents(&evs, "memory.after").len(), 2);
    let last = turn_requests(&f.model).pop().unwrap();
    assert!(!text_of(&last.messages[0]).contains("<memory>"));
}

/// Две компакции по порогу: на границах 3 и 5, затем ответ.
fn two_compactions(f: &Fx, d: u32) {
    f.model.push(note("c1", d - 400));
    f.model.push(note("c2", 800));
    f.model.push(summary("n1"));
    f.model.push(note("c3", 100));
    f.model.push(note("c4", d - 590));
    f.model.push(summary("n2"));
    f.model.push(answer("готово"));
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

#[tokio::test]
async fn compaction_pause_is_interrupted() {
    let b0 = probe("pause", 8192).await;
    let d = THRESHOLD_8K - b0;
    let f = fx("pause", scripted(8192));
    let mut bus = f.bus.subscribe();
    f.model.push(note("c1", d - 400));
    f.model.push(note("c2", 800));
    // Сводка не приходит: стрим компакции молчит, пока его не бросят.
    f.model.push_open(vec![]);
    let (ctl, rx) = control_channel(f.store.clone(), f.run);
    let (deps, run) = (f.deps.clone(), f.run);
    let h = tokio::spawn(async move { start_run(&deps, run, spec(), rx).await });
    nth_of(&mut bus, EventKind::ModelRequest, 1, |p| {
        p["kind"] == "compact"
    })
    .await;
    within("стрим компакции открыт", async {
        while f.model.open_streams() == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    let t0 = Instant::now();
    ctl.pause();
    // Реакция на паузу — обрыв стрима (без записи журнала в мерке).
    within("обрыв стрима компакции", async {
        while f.model.open_streams() > 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    let took = t0.elapsed();
    assert!(
        took <= Duration::from_millis(200) * time_scale(),
        "стрим компакции оборван за {took:?}"
    );
    let out = within("run", h).await.unwrap().unwrap();
    assert_eq!(out, paused("command"));
    let evs = events(&f.store, f.run).await;
    let comp = intents(&evs, "compact");
    assert_eq!(comp.len(), 1);
    assert!(
        receipt_of(&evs, comp[0])
            .payload
            .to_string()
            .contains("interrupted:pause"),
        "квитанция компакции не interrupted:pause"
    );
    // Хук памяти и сводка — не изменения проекта: в `<state>` их нет.
    let state = state_block(&f.store, f.run).await.unwrap();
    assert!(state.contains("changes made: none"), "{state}");
    assert!(!intents(&evs, "memory.before").is_empty());

    // Resume: компакция выполняется заново, сводка — из нового ответа.
    f.model.push(summary("after resume"));
    f.model.push(answer("готово"));
    let out = resume_with(&f, &f.deps, ResumeChange::default()).await;
    assert_eq!(out, done("готово"));
    let evs = events(&f.store, f.run).await;
    assert_eq!(intents(&evs, "compact").len(), 2);
    assert_eq!(of_kind(&evs, EventKind::ContextCompacted).len(), 1);
    let last = turn_requests(&f.model).pop().unwrap();
    assert!(text_of(&last.messages[0]).contains("## Next step\nafter resume"));
}

/// Сценарий реплея: два хода, компакция на границе 3, ответ.
fn replay_script(model: &ScriptedModel, d: u32) {
    model.push(note("c1", d - 400));
    model.push(note("c2", 800));
    model.push(summary("n"));
    model.push(answer("готово"));
}

/// `messages` всех `model.request` run по порядку.
async fn request_messages(s: &ProjectStore, run: RunId) -> Vec<Value> {
    of_kind(&events(s, run).await, EventKind::ModelRequest)
        .iter()
        .map(|e| e.payload["messages"].clone())
        .collect()
}

#[tokio::test]
async fn compaction_replays_without_second_summary() {
    let b0 = probe("replay", 8192).await;
    let d = THRESHOLD_8K - b0;
    // Эталон без сбоя.
    let reference = fx("replay_ref", scripted(8192));
    replay_script(&reference.model, d);
    assert_eq!(start(&reference).await.unwrap(), done("готово"));
    let ref_evs = events(&reference.store, reference.run).await;
    let step = of_kind(&ref_evs, EventKind::ContextCompacted)[0].payload["step"]
        .as_u64()
        .unwrap();
    let expected = request_messages(&reference.store, reference.run).await;

    // Сбой после квитанции `compact` (один раз), resume с теми же deps.
    let mut f = fx("replay", scripted(8192));
    replay_script(&f.model, d);
    let fired = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&fired);
    let at = CrashPoint::AfterCompact { step };
    f.deps.crash = Some(Arc::new(move |p: CrashPoint| {
        p == at && !flag.swap(true, Ordering::SeqCst)
    }));
    assert!(matches!(start(&f).await, Err(LoopError::Halted)));
    assert!(fired.load(Ordering::SeqCst));
    let evs = events(&f.store, f.run).await;
    assert!(of_kind(&evs, EventKind::ContextCompacted).is_empty());
    let out = resume_with(&f, &f.deps, ResumeChange::default()).await;
    assert_eq!(out, done("готово"));
    // Сводка не запрашивается второй раз; запросы — как у эталона.
    assert_eq!(compactions(&f.model), 1);
    assert_eq!(request_messages(&f.store, f.run).await, expected);
    let evs = events(&f.store, f.run).await;
    assert_eq!(of_kind(&evs, EventKind::ContextCompacted).len(), 1);
}

#[tokio::test]
async fn smaller_window_after_resume_compacts() {
    let b0 = probe("window_32k", 32768).await;
    let d = THRESHOLD_8K - b0;
    // Эндпоинт a — 32768 (тир standard): оценка ≈ 3900 далеко от порога 15360.
    let f = fx("window", scripted(32768));
    f.model.push(note("c1", d + 800));
    f.model
        .push(vec![Err(ModelError::new(ErrorClass::Server, "500"))]);
    assert_eq!(start(&f).await.unwrap(), paused("provider"));
    assert_eq!(compactions(&f.model), 0);

    // Resume на эндпоинт b с окном 8192 (тир заморожен, резерв 3072 — D40): та же
    // история выше порога компакции и ниже окна.
    let small = Arc::new(scripted(8192));
    small.push(summary("n"));
    small.push(answer("готово"));
    let mut deps = f.deps.clone();
    deps.model = small.clone();
    let out = resume_with(
        &f,
        &deps,
        ResumeChange {
            endpoint: Some(EndpointId("b".into())),
            ..ResumeChange::default()
        },
    )
    .await;
    assert_eq!(out, done("готово"));
    let evs = events(&f.store, f.run).await;
    let changed = alerts(&evs, "n_ctx_changed");
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert_eq!(changed[0].payload["from"], 32768);
    assert_eq!(changed[0].payload["to"], 8192);
    let compacted = of_kind(&evs, EventKind::ContextCompacted);
    assert_eq!(compacted.len(), 1);
    // Первая граница после resume — шаг 3 (шаги 1 и 2 — ходы до паузы).
    assert_eq!(compacted[0].payload["step"], 3);
    assert!(compacted[0].seq > changed[0].seq);
    assert_eq!(compactions(&small), 1);
}
