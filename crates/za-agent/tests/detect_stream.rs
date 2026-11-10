//! Детекторы стрима (R1-B2 D58, D59, D69): повтор текста и бесконечное рассуждение
//! ([`StreamWatch`]), параметры повтора хода ([`retry_params`]) и их применение
//! драйвером — обрыв стрима, повтор хода в том же шаге, пауза `model_loop`, реплей
//! обрыва по квитанции после сбоя (`CrashPoint`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;
use za_agent::{
    ContextBudget, ContextConfig, CrashPoint, Degenerate, EnvInfo, LoopDeps, LoopError,
    LoopOutcome, NoMemory, ResumeChange, RunSpec, Snapshot, StreamWatch, THINKING_TIMEOUT,
    builtin_tools, control_channel, resume_run, retry_params, start_run, state_block,
};
use za_core::{EventBus, Journal, ToolRegistry};
use za_llm::{
    BackendKind, Effort, EndpointId, FinishReason, Message, ModelError, Part, ReasoningMode, Role,
    Sampling, ScriptedModel, StreamEvent, Usage, default_caps,
};
use za_protocol::{
    Budget, BudgetKind, Event, EventKind, Priority, RunId, RunMode, RunRole, RunStatus, Seq, Tier,
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

/// Строка повтора брифа: 52 байта, 34 символа.
const LINE: &str = "Проверяю файл config.toml ещё раз\n";

/// Дельты по `n` байт (по границам символов).
fn chunks(text: &str, n: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut cut = n.min(rest.len());
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (a, b) = rest.split_at(cut);
        out.push(a);
        rest = b;
    }
    out
}

// ---- StreamWatch ----

/// Ruling I-1: петля — не меньше 8 копий годной единицы подряд и 2 КиБ повтора. Строка
/// брифа (52 байта) срабатывает на 40-й копии (2080 байт), 39 копий — ещё нет.
#[test]
fn repeated_line_detected() {
    assert_eq!(LINE.len(), 52);
    let mut w = StreamWatch::new(&ReasoningMode::Default);
    for i in 1..=39 {
        assert_eq!(w.on_text(LINE), None, "строка {i}");
    }
    assert_eq!(w.on_text(LINE), Some(Degenerate::Repetition));
}

/// Правило n-грамм (Ruling I-1): 12-словная фраза 8 раз без перекрытия в последних 600
/// словах, от первого вхождения до конца не меньше 2 КиБ; 7 вставок — не повтор.
#[test]
fn ngram_detected() {
    let phrase = "the build failed again so I will clean the cache and retry now";
    assert_eq!(phrase.split_whitespace().count(), 13);
    let phrase: Vec<&str> = phrase.split_whitespace().take(12).collect();
    let text_with = |inserts: usize| {
        // 600 слов: разные слова-заполнители и вставки фразы.
        let mut words: Vec<String> = Vec::new();
        let mut filler = 0;
        for slot in 0..inserts {
            let gap = if slot == 0 { 20 } else { 55 };
            for _ in 0..gap {
                words.push(format!("w{filler}"));
                filler += 1;
            }
            words.extend(phrase.iter().map(|s| (*s).to_owned()));
        }
        while words.len() < 600 {
            words.push(format!("w{filler}"));
            filler += 1;
        }
        assert_eq!(words.len(), 600);
        words.join(" ")
    };
    let feed = |text: &str| {
        let mut w = StreamWatch::new(&ReasoningMode::Default);
        chunks(text, 64).into_iter().find_map(|d| w.on_text(d))
    };
    assert_eq!(feed(&text_with(8)), Some(Degenerate::Repetition));
    assert_eq!(feed(&text_with(7)), None);
}

/// Настоящие петли (фраза × N, абзац × N) ловятся не позже 4 КиБ повторов (Ruling I-1),
/// и после нормального текста, и дельтами по 64 байта.
#[test]
fn real_loops_detected_within_4k() {
    let normal = include_str!("fixtures/stream_normal.txt");
    let mut cut = 3000;
    while !normal.is_char_boundary(cut) {
        cut -= 1;
    }
    let prefix = &normal[..cut];
    let phrase = "I will check the config file once more to be sure.\n";
    let paragraph = "Сейчас я ещё раз проверю конфигурацию сервера: открою файл \
        config.toml, сравню параметры с документацией и запущу тесты, чтобы убедиться, что \
        изменения применились правильно и ничего не сломалось.\n\n";
    assert!(
        paragraph.len() > 300 && paragraph.len() <= 512,
        "{}",
        paragraph.len()
    );
    for unit in [phrase, paragraph, LINE] {
        for size in [64, usize::MAX] {
            let mut w = StreamWatch::new(&ReasoningMode::Default);
            for d in chunks(prefix, 64) {
                assert_eq!(w.on_text(d), None);
            }
            let looped = unit.repeat(4096 / unit.len() + 1);
            let mut fed = 0;
            let hit = chunks(&looped, size.min(unit.len()))
                .into_iter()
                .find_map(|d| {
                    fed += d.len();
                    w.on_text(d)
                });
            assert_eq!(hit, Some(Degenerate::Repetition), "{unit:?}");
            assert!(fed <= 4096, "{unit:?}: сработало после {fed} байт повтора");
        }
    }
}

/// Корпус ложных срабатываний ревьюера (матрица нулей, рамки, разделители, строки и
/// пустой шаблон таблицы, повторённый `println!`) и те же входы в большем масштабе — не
/// петля: в единице меньше 3 слов или повтор короче 2 КиБ (Ruling I-1).
#[test]
fn false_positives_not_detected() {
    let corpus = include_str!("fixtures/stream_false_positives.txt");
    let cases: Vec<(&str, &str)> = corpus
        .split("%%% case: ")
        .filter(|c| !c.is_empty())
        .map(|c| c.split_once('\n').expect("имя случая"))
        .collect();
    assert_eq!(cases.len(), 15);
    for (name, text) in cases {
        for by_lines in [true, false] {
            let mut w = StreamWatch::new(&ReasoningMode::Default);
            let deltas: Vec<&str> = if by_lines {
                text.split_inclusive('\n').collect()
            } else {
                chunks(text, 64)
            };
            for d in deltas {
                assert_eq!(w.on_text(d), None, "{name}: {d:?}");
            }
        }
    }
}

#[test]
fn normal_text_not_detected() {
    let text = include_str!("fixtures/stream_normal.txt");
    assert!(text.len() >= 20 * 1024, "{}", text.len());
    let mut w = StreamWatch::new(&ReasoningMode::Default);
    for (i, d) in chunks(text, 64).into_iter().enumerate() {
        assert_eq!(w.on_text(d), None, "дельта {i}: {d:?}");
    }
}

#[test]
fn thinking_over_budget() {
    let low = ReasoningMode::Effort(Effort::Low);
    let limit = 2048 * 3 / 2 * 4;
    let t0 = Instant::now();
    let mut w = StreamWatch::new(&low);
    assert_eq!(w.on_reasoning(&"r".repeat(limit), t0), None);
    let mut w = StreamWatch::new(&low);
    assert_eq!(
        w.on_reasoning(&"r".repeat(limit + 4), t0),
        Some(Degenerate::Thinking)
    );
    // Дельтами: срабатывает на дельте, которая переходит предел.
    let mut w = StreamWatch::new(&low);
    for _ in 0..limit / 64 {
        assert_eq!(w.on_reasoning(&"r".repeat(64), t0), None);
    }
    assert_eq!(w.on_reasoning("rrrr", t0), Some(Degenerate::Thinking));
}

#[test]
fn thinking_timeout() {
    let t0 = Instant::now();
    let mut w = StreamWatch::new(&ReasoningMode::Default);
    assert_eq!(w.deadline(), None, "рассуждения ещё нет");
    assert_eq!(w.on_reasoning("x", t0), None);
    assert_eq!(THINKING_TIMEOUT, Duration::from_secs(300));
    assert_eq!(w.deadline(), Some(t0 + Duration::from_secs(300)));
    assert_eq!(
        w.on_reasoning("y", t0 + Duration::from_secs(301)),
        Some(Degenerate::Thinking)
    );
    assert_eq!(w.on_text("a"), None);
    assert_eq!(w.deadline(), None);

    // Вызов тоже снимает дедлайн; после него долгое рассуждение не обрывается.
    let mut w = StreamWatch::new(&ReasoningMode::Default);
    assert_eq!(w.on_reasoning("x", t0), None);
    w.on_calls();
    assert_eq!(w.deadline(), None);
    assert_eq!(w.on_reasoning("y", t0 + Duration::from_secs(301)), None);
}

#[test]
fn retry_params_values() {
    let (s, r) = retry_params(
        Degenerate::Repetition,
        &Sampling {
            temperature: Some(0.7),
            ..Default::default()
        },
    );
    assert!((s.temperature.unwrap() - 0.4).abs() < 1e-6);
    assert_eq!(s.presence_penalty, Some(0.5));
    assert_eq!(r, None);
    let (s, _) = retry_params(
        Degenerate::Repetition,
        &Sampling {
            temperature: Some(0.2),
            ..Default::default()
        },
    );
    assert!((s.temperature.unwrap() - 0.1).abs() < 1e-6);
    let (s, _) = retry_params(Degenerate::Repetition, &Sampling::default());
    assert!((s.temperature.unwrap() - 0.3).abs() < 1e-6);
    let (s, r) = retry_params(
        Degenerate::Thinking,
        &Sampling {
            seed: Some(7),
            ..Default::default()
        },
    );
    assert_eq!(r, Some(ReasoningMode::Off));
    assert_eq!(s.seed, Some(7), "прочие параметры выборки сохраняются");
    assert_eq!(s.presence_penalty, None);
}

// ---- Драйвер ----

struct Fx {
    store: ProjectStore,
    model: Arc<ScriptedModel>,
    deps: LoopDeps,
    run: RunId,
}

fn fx(name: &str) -> Fx {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("detect_stream")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let store = ProjectStore::open(&d).unwrap();
    let model = Arc::new(ScriptedModel::new(default_caps(BackendKind::LlamaCpp)));
    let mut reg = ToolRegistry::default();
    for t in builtin_tools() {
        reg.register(t).unwrap();
    }
    let bus = EventBus::new(EventBus::DEFAULT_CAPACITY);
    let deps = LoopDeps {
        journal: Journal::new(store.clone(), bus),
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
        task: "Проверь конфигурацию".into(),
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

/// Стрим, повторяющий строку: 60 дельт (срабатывание на 40-й, Ruling I-1), затем стрим молчит и не закрывается сам.
fn looping() -> Vec<Result<StreamEvent, ModelError>> {
    (0..60)
        .map(|_| Ok(StreamEvent::TextDelta(LINE.into())))
        .collect()
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

async fn events(s: &ProjectStore, run: RunId) -> Vec<Event> {
    s.read_run(run, Seq(0), 100_000).await.unwrap()
}

async fn payloads(f: &Fx, kind: EventKind) -> Vec<Value> {
    events(&f.store, f.run)
        .await
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload)
        .collect()
}

/// Сообщения квитанций-отказов эффектов `model`.
async fn model_failures(f: &Fx) -> Vec<String> {
    let evs = events(&f.store, f.run).await;
    let model_keys: Vec<String> = evs
        .iter()
        .filter(|e| e.kind == EventKind::EffectIntent && e.payload["name"] == "model")
        .filter_map(|e| e.payload["key"].as_str().map(str::to_owned))
        .collect();
    let mut out = Vec::new();
    for key in model_keys {
        let row = f
            .store
            .effect(f.run, &za_protocol::EffectKey(key))
            .await
            .unwrap()
            .expect("эффект model");
        if let Some(env) = row.result
            && env["outcome"] == "error"
        {
            out.push(env["message"].as_str().unwrap().to_owned());
        }
    }
    out
}

async fn last_snapshot(f: &Fx) -> Snapshot {
    let (_, v) = f
        .store
        .latest_snapshot(f.run)
        .await
        .unwrap()
        .expect("снимок");
    serde_json::from_value(v).unwrap()
}

/// (detector, action, turn) событий `loop.detected`.
async fn detected(f: &Fx) -> Vec<(String, String, u64)> {
    payloads(f, EventKind::LoopDetected)
        .await
        .into_iter()
        .map(|p| {
            (
                p["detector"].as_str().unwrap().to_owned(),
                p["action"].as_str().unwrap().to_owned(),
                p["turn"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// Стримы модели закрыты (drop): ожидание по событию, с пределом.
async fn streams_closed(m: &ScriptedModel) {
    within("drop стрима после обрыва", async {
        while m.open_streams() > 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
}

#[tokio::test]
async fn repetition_aborts_and_retries_then_pauses() {
    // Сценарий 1: повтор строки, затем нормальный ответ.
    let f = fx("repetition_retry");
    f.model.push_open(looping());
    f.model.push(answer("Конфигурация в порядке."));
    assert_eq!(
        start(&f, &f.deps).await.unwrap(),
        done("Конфигурация в порядке.")
    );
    streams_closed(&f.model).await;
    let calls = f.model.calls();
    assert_eq!(calls.len(), 2);
    let first = &calls[0].request.sampling;
    let second = &calls[1].request.sampling;
    assert_eq!(first.presence_penalty, None);
    assert_eq!(second.presence_penalty, Some(0.5));
    assert!((second.temperature.unwrap() - 0.3).abs() < 1e-6);
    // Повтор — тот же ход и та же история, другая попытка.
    assert_eq!(calls[0].request.messages, calls[1].request.messages);
    let requests = payloads(&f, EventKind::ModelRequest).await;
    let turns: Vec<(u64, u64)> = requests
        .iter()
        .map(|p| {
            (
                p["turn"].as_u64().unwrap(),
                p["turn_attempt"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(turns, [(1, 0), (1, 1)]);
    assert_eq!(
        model_failures(&f).await,
        ["interrupted:repetition".to_owned()]
    );
    let finished = payloads(&f, EventKind::TurnFinished).await;
    assert_eq!(finished.len(), 2, "{finished:?}");
    assert_eq!(finished[0]["outcome"], "degenerate");
    assert_eq!(finished[0]["detector"], "repetition");
    assert_eq!(finished[0]["turn"], 1);
    assert_eq!(finished[1]["outcome"], "answer");
    assert_eq!(finished[1]["turn"], 1);
    let snap = last_snapshot(&f).await;
    assert_eq!(snap.counters.steps, 2, "каждый эффект model — шаг бюджета");
    assert_eq!(snap.counters.empty_turns, 0);
    // <state> считает конечные исходы: оборванная попытка — не выполненный шаг (D69).
    let state = state_block(&f.store, f.run).await.unwrap();
    assert!(state.contains("steps done: 1;"), "{state}");
    assert_eq!(
        detected(&f).await,
        [("repetition".to_owned(), "retry".to_owned(), 1)]
    );

    // Сценарий 2: повтор и в повторе хода — пауза model_loop.
    let f = fx("repetition_pause");
    f.model.push_open(looping());
    f.model.push_open(looping());
    let out = start(&f, &f.deps).await.unwrap();
    assert_eq!(
        out,
        LoopOutcome::Paused {
            reason: "model_loop".into(),
            interrupt: None,
        }
    );
    streams_closed(&f.model).await;
    assert_eq!(f.model.calls().len(), 2, "не больше одного повтора в шаге");
    assert_eq!(
        detected(&f).await,
        [
            ("repetition".to_owned(), "retry".to_owned(), 1),
            ("repetition".to_owned(), "pause".to_owned(), 1),
        ]
    );
    let outcomes: Vec<Value> = payloads(&f, EventKind::TurnFinished)
        .await
        .into_iter()
        .map(|p| json!([p["outcome"], p["detector"]]))
        .collect();
    assert_eq!(
        outcomes,
        [
            json!(["degenerate", "repetition"]),
            json!(["degenerate", "repetition"])
        ]
    );
}

/// Дедлайн рассуждения в `select!` стрима (D59): стрим с одной дельтой рассуждения
/// молчит, ход обрывается по виртуальным 300 с. Часы остановлены (`start_paused`), и
/// рантайм продвигает их сам; внешнего предела по времени нет: пока тест ждёт писателя
/// журнала (поток ОС), автопродвижение сработало бы на нём. Утверждается исход.
#[tokio::test(start_paused = true)]
async fn silent_reasoning_stream_is_cut() {
    let f = fx("silent_reasoning");
    f.model.push_open(vec![Ok(StreamEvent::ReasoningDelta(
        "Думаю о структуре".into(),
    ))]);
    f.model.push(answer("Готово."));
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    let out = start_run(&f.deps, f.run, spec(), rx).await.unwrap();
    assert_eq!(out, done("Готово."));
    assert_eq!(f.model.open_streams(), 0);
    assert_eq!(
        model_failures(&f).await,
        ["interrupted:thinking".to_owned()]
    );
    let calls = f.model.calls();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0].request.reasoning, ReasoningMode::Off);
    assert_eq!(calls[1].request.reasoning, ReasoningMode::Off);
    // Повтор без мышления — лимит вывода `Off` по D40: `output_reserve`.
    let n_ctx = last_snapshot(&f).await.setup.n_ctx;
    let reserve = ContextBudget::for_context(n_ctx).output_reserve;
    assert_eq!(calls[1].request.max_output_tokens, Some(reserve));
    assert_ne!(calls[0].request.max_output_tokens, Some(reserve));
    assert_eq!(calls[0].request.sampling, calls[1].request.sampling);
    let finished = payloads(&f, EventKind::TurnFinished).await;
    assert_eq!(finished[0]["outcome"], "degenerate");
    assert_eq!(finished[0]["detector"], "thinking");
    // Дельта рассуждения до обрыва записана.
    let deltas = payloads(&f, EventKind::ModelDelta).await;
    assert!(
        deltas.iter().any(|p| p["reasoning"] == "Думаю о структуре"),
        "{deltas:?}"
    );
}

#[tokio::test]
async fn degenerate_replays_from_receipt() {
    let f = fx("degenerate_replay");
    f.model.push_open(looping());
    let mut crashing = f.deps.clone();
    crashing.crash = crash_at(CrashPoint::AfterModelEffect { step: 1 });
    let err = start(&f, &crashing).await.unwrap_err();
    assert!(matches!(err, LoopError::Halted), "{err:?}");
    streams_closed(&f.model).await;
    assert_eq!(f.model.calls().len(), 1);
    assert_eq!(
        model_failures(&f).await,
        ["interrupted:repetition".to_owned()]
    );

    f.model.push(answer("Готово."));
    assert_eq!(resume(&f).await.unwrap(), done("Готово."));
    // Оборванная попытка воспроизведена по квитанции: модель вызвана только для повтора.
    let calls = f.model.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].request.sampling.presence_penalty, Some(0.5));
    let finished = payloads(&f, EventKind::TurnFinished).await;
    let outcomes: Vec<&str> = finished
        .iter()
        .map(|p| p["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes, ["degenerate", "answer"]);
    assert_eq!(
        detected(&f).await,
        [("repetition".to_owned(), "retry".to_owned(), 1)]
    );
    let statuses: Vec<Value> = payloads(&f, EventKind::RunStatus)
        .await
        .into_iter()
        .map(|p| p["to"].clone())
        .collect();
    assert!(!statuses.contains(&json!("failed")), "{statuses:?}");
    assert_eq!(last_snapshot(&f).await.counters.steps, 2);
}

/// Путь паузы при реплее (m-5): сбой после второй квитанции обрыва в шаге; resume
/// воспроизводит обе по журналу без модели и встаёт на паузу `model_loop`. Resume с
/// паузы начинает новый шаг с обычными параметрами (без повтора).
#[tokio::test]
async fn model_loop_pause_replays_and_resumes_fresh() {
    let f = fx("model_loop_replay");
    f.model.push_open(looping());
    f.model.push_open(looping());
    let seen = Arc::new(AtomicUsize::new(0));
    let mut crashing = f.deps.clone();
    let counter = Arc::clone(&seen);
    crashing.crash = Some(Arc::new(move |p: CrashPoint| {
        p == (CrashPoint::AfterModelEffect { step: 1 })
            && counter.fetch_add(1, Ordering::SeqCst) == 1
    }));
    let err = start(&f, &crashing).await.unwrap_err();
    assert!(matches!(err, LoopError::Halted), "{err:?}");
    streams_closed(&f.model).await;
    assert_eq!(f.model.calls().len(), 2);

    let out = resume(&f).await.unwrap();
    assert_eq!(
        out,
        LoopOutcome::Paused {
            reason: "model_loop".into(),
            interrupt: None,
        }
    );
    assert_eq!(f.model.calls().len(), 2, "обе попытки — из квитанций");
    assert_eq!(
        model_failures(&f).await,
        [
            "interrupted:repetition".to_owned(),
            "interrupted:repetition".to_owned()
        ]
    );

    f.model.push(answer("Готово."));
    assert_eq!(resume(&f).await.unwrap(), done("Готово."));
    let calls = f.model.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[2].request.sampling.presence_penalty, None,
        "новый шаг без повтора"
    );
    let last = payloads(&f, EventKind::ModelRequest).await;
    let last = last.last().unwrap();
    assert_eq!(last["turn_attempt"], 0);
}

/// Повтор хода — ещё один шаг бюджета (D69): при исчерпанном бюджете шагов вместо
/// повтора — `blocked_on_budget`.
#[tokio::test]
async fn retry_respects_step_budget() {
    let f = fx("retry_budget");
    f.model.push_open(looping());
    let mut s = spec();
    s.budget = Budget {
        steps: Some(1),
        ..Budget::default()
    };
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    let out = within("start_run", start_run(&f.deps, f.run, s, rx))
        .await
        .unwrap();
    assert_eq!(
        out,
        LoopOutcome::BlockedOnBudget {
            kind: BudgetKind::Steps
        }
    );
    streams_closed(&f.model).await;
    assert_eq!(f.model.calls().len(), 1, "повтора нет");
    assert!(
        detected(&f).await.is_empty(),
        "loop.detected retry не пишется"
    );
    let finished = payloads(&f, EventKind::TurnFinished).await;
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "degenerate");
}
