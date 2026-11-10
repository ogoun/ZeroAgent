//! Трасса дрейфа префикса (CTX-002, R1-B2 задача 9): золотой список запросов модели
//! длинного run с удалением старых результатов и компакциями, проверка «история внутри
//! эпохи только дописывается» и тот же список после сбоя и resume.
//!
//! Сценарий: окно 8192, тир small (`Effort(Low)`: резерв 3072 — D40, компакция при
//! оценке ≥ 0,75 × 8192 − 3072 = 3072, удаление старых результатов — при оценке ≥ 2253).
//! Модель 9 раз вызывает `echo {"i": n}`, десятый ход — ответ. Вывод `echo` — ASCII,
//! свой для каждого хода (детекторы повтора и застоя молчат): ходы 1–6 — 1600 байт
//! (≈ 415 токенов на ход), с хода 6 оценка выше порога удаления и результат старше 4
//! ходов заменяется ссылкой; ходы 7–9 — 4000 байт (≈ 1015 токенов), и оценка после
//! удаления доходит до порога компакции. Модель не сообщает расход
//! (`usage.input == 0`): оценка — весь запрос байтами / 4 (D41). RunId фиксирован: путь
//! выгрузки при удалении результата содержит id run.
//!
//! Золотой файл `tests/golden/trace_prefixes.json` перезаписывается только при
//! `ZA_UPDATE_GOLDEN=1` и коммитится; без файла тест падает. Меняется только вместе с
//! намеренной правкой промптов, запроса или правил контекста.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use support::*;

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

/// Предел ожиданий длинного run трассы (× `ZA_TEST_TIME_SCALE`).
const TRACE_LIMIT: Duration = Duration::from_secs(30);

/// Фиксированный id run (UUIDv7 с нулевым временем): трасса не зависит от прогона.
const RUN: &str = "00000000-0000-7000-8000-000000000009";

/// Окно сценария и тир.
const N_CTX: u32 = 8192;

/// Ходов с вызовом `echo`; ход `CALLS + 1` — ответ.
const CALLS: u32 = 9;

/// Длина вывода `echo` ходов 1..=`SHORT_TURNS`, байт: выше 1024 (результат можно
/// удалить, D45).
const ECHO_BYTES: usize = 1600;

/// Ходов с коротким выводом; дальше вывод [`ECHO_LONG_BYTES`].
const SHORT_TURNS: u32 = 6;

/// Длина вывода `echo` поздних ходов, байт: ниже лимита выгрузки 0,15 × 8192 × 4 = 4915.
const ECHO_LONG_BYTES: usize = 4000;

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/trace_prefixes.json")
}

/// Свой каталог теста `CARGO_TARGET_TMPDIR/trace/<name>-<pid>-<n>`: журнал и проект
/// (выгрузка удалённых результатов).
fn dir(name: &str) -> PathBuf {
    let d = unique_dir("trace", name);
    std::fs::create_dir_all(d.join("project")).unwrap();
    d
}

/// `echo {"i": n}` возвращает [`echo_text`] хода `n`: ASCII, свой для каждого хода.
struct Echo {
    spec: ToolSpec,
}

impl Tool for Echo {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let i = args["i"]
            .as_u64()
            .and_then(|i| u32::try_from(i).ok())
            .unwrap_or(0);
        Box::pin(async move {
            ToolOutput {
                content: echo_text(i),
                is_error: false,
            }
        })
    }
}

fn echo_text(i: u32) -> String {
    let len = if i <= SHORT_TURNS {
        ECHO_BYTES
    } else {
        ECHO_LONG_BYTES
    };
    let line = format!("echo line {i:04} abcdefghijklmnopqrstuvwxyz0123456789\n");
    line.repeat(len.div_ceil(line.len()))[..len].to_owned()
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
        fault: None,
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
                arguments: json!({ "i": i }),
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
    within_for(
        TRACE_LIMIT,
        "start_run",
        start_run(&f.deps, f.run, spec(&f.project), rx),
    )
    .await
}

async fn resume(f: &Fx) -> LoopOutcome {
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    within_for(
        TRACE_LIMIT,
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

/// Золотая трасса. Файл перезаписывается только при `ZA_UPDATE_GOLDEN=1` (один раз
/// на прогон); без файла тест падает: CI сравнивает, а не пишет эталон сам себе.
async fn golden() -> Value {
    static WRITE: tokio::sync::Mutex<bool> = tokio::sync::Mutex::const_new(false);
    let mut written = WRITE.lock().await;
    let update = std::env::var("ZA_UPDATE_GOLDEN").is_ok_and(|v| v == "1");
    if update && !*written {
        let (evs, _) = reference("golden_write").await;
        let mut text = serde_json::to_string_pretty(&trace(&evs)).unwrap();
        text.push('\n');
        std::fs::write(golden_path(), text).unwrap();
        eprintln!("записан {}", golden_path().display());
        *written = true;
    }
    let text = std::fs::read_to_string(golden_path()).unwrap_or_else(|e| {
        panic!(
            "нет {}: {e}; эталон пишется командой ZA_UPDATE_GOLDEN=1 cargo test -p za-agent --test trace",
            golden_path().display()
        )
    });
    serde_json::from_str(&text).expect("golden/trace_prefixes.json — не JSON")
}

/// Итог проверки [`check_append_only`].
#[derive(Debug, Default, PartialEq, Eq)]
struct AppendOnly {
    /// Пар соседних запросов одной эпохи: история только дописана.
    pairs: usize,
    /// Удалений старых результатов (`context.pruned`).
    pruned: usize,
    /// Компакций (`context.compacted`).
    compacted: usize,
}

/// Дрейф истории (CTX-002, NFR-06): внутри эпохи `messages` (блобы) каждого
/// `model.request` — строгий префикс `messages` следующего запроса, в том числе запроса
/// компакции. Эпоху меняют только `context.pruned` и `context.compacted`, их `epoch`
/// растёт на 1. После удаления первое сообщение (`<env>` и задача) прежнее.
fn check_append_only(evs: &[Event]) -> Result<AppendOnly, String> {
    let mut out = AppendOnly::default();
    let mut epoch = 0u64;
    let mut prev: Option<&Vec<Value>> = None;
    let mut rewrite: Option<EventKind> = None;
    for e in evs {
        match e.kind {
            EventKind::ContextPruned | EventKind::ContextCompacted => {
                let got = e.payload["epoch"].as_u64();
                if got != Some(epoch + 1) {
                    return Err(format!(
                        "seq {}: эпоха {got:?}, ждали {}",
                        e.seq.0,
                        epoch + 1
                    ));
                }
                epoch += 1;
                if e.kind == EventKind::ContextPruned {
                    out.pruned += 1;
                } else {
                    out.compacted += 1;
                }
                rewrite = Some(e.kind.clone());
            }
            EventKind::ModelRequest => {
                let cur = e.payload["messages"]
                    .as_array()
                    .ok_or_else(|| format!("seq {}: нет messages", e.seq.0))?;
                if let Some(p) = prev {
                    match rewrite {
                        None => {
                            if cur.len() <= p.len() || cur[..p.len()] != p[..] {
                                return Err(format!(
                                    "seq {}: история эпохи {epoch} переписана, а не дописана",
                                    e.seq.0
                                ));
                            }
                            out.pairs += 1;
                        }
                        Some(EventKind::ContextPruned) => {
                            if cur.first() != p.first() {
                                return Err(format!(
                                    "seq {}: удаление сменило первое сообщение",
                                    e.seq.0
                                ));
                            }
                        }
                        // Компакция: новая история; первое сообщение может совпасть
                        // с прошлым (та же сводка у двух компакций подряд).
                        Some(_) => {}
                    }
                }
                prev = Some(cur);
                rewrite = None;
            }
            _ => {}
        }
    }
    Ok(out)
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
    // История внутри эпохи только дописывается; в сценарии есть и удаление, и компакция.
    let a = check_append_only(&evs).unwrap();
    assert!(a.pruned >= 1 && a.compacted >= 1, "{a:?}");
    assert!(a.pairs >= 5, "{a:?}");
}

/// RED-проверка самой проверки: копия журнала эталона с подменённым блобом сообщения
/// внутри эпохи (как если бы рантайм переписал историю) — `check_append_only` отвергает.
#[tokio::test]
async fn append_only_check_catches_rewrite() {
    let (evs, _) = reference("append_only").await;
    assert!(check_append_only(&evs).is_ok());
    // Второй запрос run: та же эпоха, что у первого.
    let i = evs
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == EventKind::ModelRequest)
        .nth(1)
        .map(|(i, _)| i)
        .unwrap();
    for at in [0, 1] {
        let mut forged = evs.clone();
        forged[i].payload["messages"][at] = json!("0".repeat(64));
        let err = check_append_only(&forged).unwrap_err();
        assert!(err.contains("переписана"), "{err}");
    }
    // Удаление без события: следующий запрос короче — тоже переписывание.
    let mut cut = evs.clone();
    let msgs = cut[i].payload["messages"].as_array().unwrap().clone();
    cut[i].payload["messages"] = Value::Array(msgs[..1].to_vec());
    assert!(check_append_only(&cut).is_err());
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
