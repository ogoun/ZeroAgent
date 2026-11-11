//! Детекторы хода (R1-B2 D57, D60–D62): обещание без действия, цикл ошибок
//! инструмента, порча JSON подряд, падение кэша — реакции [`DetectorState`] и их
//! применение драйвером. Для каждого детектора — проверка, что нормальный ход его не
//! включает: ложное срабатывание ставит run на паузу или тратит ход.

mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::time::Instant;
use za_agent::{
    ContextConfig, Conversation, CrashPoint, DetectorState, EnvInfo, LoopDeps, LoopError,
    LoopOutcome, NoMemory, PROMISE_NUDGE, Reaction, ResumeChange, RunSpec, Snapshot, builtin_tools,
    control_channel, promise_without_action, resume_run, start_run,
};
use za_core::{
    AgentEvent, Approval, EventBus, Journal, RiskClass, Tool, ToolCx, ToolKind, ToolOutput,
    ToolRegistry, ToolSpec,
};
use za_llm::{
    BackendKind, BoxFuture, EndpointId, FinishReason, Message, ModelCall, ModelError, ModelPort,
    ModelStream, Part, ProviderCapabilities, Role, ScriptedModel, StreamEvent, ToolCall,
    ToolSchema, Usage, default_caps,
};
use za_protocol::{
    BlobHash, Budget, EffectClass, Event, EventKind, Priority, RunId, RunMode, RunRole, RunStatus,
    Seq, Tier, pause_reason,
};
use za_store::ProjectStore;

// ---- Помощники ----

/// NFR-03: эффект сигнала не позже 200 мс (× `ZA_TEST_TIME_SCALE`).
fn nfr03() -> Duration {
    Duration::from_millis(200) * scale()
}

fn h(s: &str) -> BlobHash {
    BlobHash(blake3::hash(s.as_bytes()).to_hex().to_string())
}

fn sleep_s(n: u64) -> Reaction {
    Reaction::Sleep(Duration::from_secs(n))
}

fn usage(input: u64, cached: u64) -> Usage {
    Usage {
        input,
        cached,
        ..Usage::default()
    }
}

// ---- D60: обещание без действия ----

#[test]
fn promise_patterns() {
    assert!(promise_without_action("Понял. Сейчас я проверю файл."));
    assert!(promise_without_action("OK. Let me run the tests."));
    assert!(!promise_without_action("Готово: тесты зелёные."));
    for t in [
        "I will check the logs.",
        "I'll fix it.",
        "I\u{2019}ll fix it.",
        "Next, I open the config.",
        "Now I'll run cargo test.",
        "Я сейчас посмотрю.",
        "Давайте посмотрим журнал.",
        "Приступаю к правке.",
        // Корпус обещаний Ruling I-2 ревью задачи 8.
        "Сейчас проверю.",
        "Сейчас посмотрю конфиг.",
        "Запущу тесты.",
        "Проверю логи сборки.",
        "Исправлю импорт.",
        "Теперь я исправлю тест.",
        "Далее я обновлю README.",
        "Let's check the logs.",
        "Let us look at the config.",
        "I'm going to rebuild the index.",
        "I am going to rebuild the index.",
        "Now, I'll open the file.",
        "Let me run the tests:\n```\ncargo test\n```",
        // Условие в другом предложении обещание не снимает (m2 финального ревью R1-B2).
        "If you want more, ask. Let me run the tests now.",
        "Если нужно, скажите. Сейчас проверю логи.",
        "Let me check if you have the config.",
    ] {
        assert!(promise_without_action(t), "{t}");
    }
    // Оборот — в последних 200 символах: обещание в начале длинного ответа не в счёт.
    let long = format!(
        "Let me explain. {}",
        "Тесты прошли, файл исправлен. ".repeat(10)
    );
    assert!(!promise_without_action(&long));
    assert!(promise_without_action(&format!(
        "{long} Let me run them again."
    )));
    // Обещание в последней строке после ответа — обещание.
    assert!(promise_without_action("Файл найден.\nСейчас я его открою."));
}

/// Нормальные ответы не обещания: вежливые концовки, «API will», «know I», вопрос
/// пользователю, пустой текст.
#[test]
fn promise_normal_answers_not_detected() {
    for t in [
        "",
        "   ",
        "The fix is in src/lib.rs. Let me know if you need anything else.",
        "Тесты зелёные. I'll be happy to help with the next step.",
        "Done. I will be glad to explain the change.",
        "The API will return 404 for missing keys.",
        "You know I prefer small commits; the change is one commit.",
        "Шаги готовы. Should I run the full suite? I'll wait for your answer?",
        "Готово. Хотите, я сейчас запущу тесты?",
        "Ответ: функция возвращает 42.",
        "Summary: the module is split into three files, tests pass.",
        "Let me summarize the changes:\n- edited src/a.rs\n- tests pass",
        "Давайте подведём итог:\n1. файл исправлен\n2. тесты зелёные",
        // Корпус ложных срабатываний Ruling I-2 ревью задачи 8: границы слова Unicode.
        "Сейчас ясно, в чём ошибка: индекс сдвинут.",
        "Сейчас явно видно, что кэш пуст.",
        "Сейчас я вижу, что тест зелёный.",
        "Теперь я понимаю причину: кэш не сброшен.",
        "Now I see the issue: the index is off by one.",
        "Now I understand: the config was stale.",
        "Рабочие приступают к сборке в 9:00.",
        "Проверено: всё работает.",
        "Проверка прошла, сборка зелёная.",
        "I'll let you know if anything changes.",
        "The tests will pass after the fix.",
        "Утилита проверяет файл и запускает тесты.",
        // Условное предложение — предложение, а не обещание (m2 финального ревью R1-B2).
        "If you want, I'll add tests for this.",
        "If you'd like, I can add tests for this.",
        "If you\u{2019}d like, I'll run the full suite.",
        "If needed, I'll rerun the build.",
        "Готово. If necessary, I will update the docs.",
        "Если хотите, я добавлю тесты.",
        "Если нужно, проверю ещё раз.",
        "Тесты зелёные. Если потребуется, запущу полный набор.",
        "При желании давайте обновим README.",
    ] {
        assert!(!promise_without_action(t), "{t:?}");
    }
}

#[test]
fn promise_top_second_time_accepted() {
    let mut d = DetectorState::default();
    let nudge = Reaction::Nudge(PROMISE_NUDGE.to_owned());
    assert_eq!(d.on_promise(RunRole::Top), nudge);
    assert_eq!(d.on_promise(RunRole::Top), Reaction::None);
    // Ход с вызовом между ними сбрасывает серию.
    d.on_calls(
        3,
        &[(
            "c1".into(),
            "read".into(),
            json!({"p": "a"}),
            EffectClass::ReadOnly,
        )],
    );
    assert_eq!(d.on_promise(RunRole::Top), nudge);
    // Ввод пользователя — тоже (любой другой ход сбрасывает серию в драйвере).
    d.on_user_input();
    assert_eq!(d.on_promise(RunRole::Worker), nudge);
    assert_eq!(d.on_promise(RunRole::Worker), Reaction::None);
    assert_eq!(d.on_promise(RunRole::Worker), Reaction::None);
}

// ---- D57: ошибки инструмента ----

#[test]
fn tool_error_backoff_then_pause() {
    let mut d = DetectorState::default();
    let got: Vec<Reaction> = (0..6)
        .map(|_| d.on_tool_errors(&[(ToolKind::Exec, true)]))
        .collect();
    assert_eq!(
        got,
        [
            Reaction::None,
            Reaction::None,
            sleep_s(1),
            sleep_s(2),
            sleep_s(4),
            Reaction::Pause(pause_reason::TOOL_ERRORS),
        ]
    );
    // Успех класса Exec между ошибками сбрасывает счёт.
    let mut d = DetectorState::default();
    for _ in 0..2 {
        assert_eq!(d.on_tool_errors(&[(ToolKind::Exec, true)]), Reaction::None);
    }
    assert_eq!(d.on_tool_errors(&[(ToolKind::Exec, false)]), Reaction::None);
    for _ in 0..2 {
        assert_eq!(d.on_tool_errors(&[(ToolKind::Exec, true)]), Reaction::None);
    }
    assert_eq!(d.on_tool_errors(&[(ToolKind::Exec, true)]), sleep_s(1));
    // Ошибка класса Read счёт Exec не сбрасывает и не двигает.
    let mut d = DetectorState::default();
    for _ in 0..2 {
        d.on_tool_errors(&[(ToolKind::Exec, true)]);
    }
    assert_eq!(d.on_tool_errors(&[(ToolKind::Read, true)]), Reaction::None);
    assert_eq!(d.on_tool_errors(&[(ToolKind::Read, false)]), Reaction::None);
    assert_eq!(d.on_tool_errors(&[(ToolKind::Exec, true)]), sleep_s(1));
    // Пауза — конец серии: дальше снова с начала.
    let mut d = DetectorState::default();
    for _ in 0..6 {
        d.on_tool_errors(&[(ToolKind::Exec, true)]);
    }
    assert_eq!(d.on_tool_errors(&[(ToolKind::Exec, true)]), Reaction::None);
}

/// Ruling I-1 ревью задачи 8: счёт — по ходам. Шесть параллельных ошибок одного
/// класса в одном ходе — +1, без паузы; успех класса в том же ходе — обнуление.
#[test]
fn tool_errors_counted_per_turn() {
    let mut d = DetectorState::default();
    let six = [(ToolKind::Read, true); 6];
    assert_eq!(d.on_tool_errors(&six), Reaction::None);
    assert_eq!(d.on_tool_errors(&six), Reaction::None);
    assert_eq!(
        d.on_tool_errors(&six),
        sleep_s(1),
        "третий ход, не шестая ошибка"
    );
    // Успех класса в ходе с ошибкой — счёт класса обнулён.
    assert_eq!(
        d.on_tool_errors(&[(ToolKind::Read, true), (ToolKind::Read, false)]),
        Reaction::None
    );
    assert_eq!(d.on_tool_errors(&six), Reaction::None);
    assert_eq!(d.on_tool_errors(&six), Reaction::None);
    assert_eq!(d.on_tool_errors(&six), sleep_s(1));
    // Сильнейшая реакция по классам: Read на третьем ходе, Exec на четвёртом.
    let mut d = DetectorState::default();
    d.on_tool_errors(&[(ToolKind::Exec, true)]);
    for _ in 0..2 {
        d.on_tool_errors(&[(ToolKind::Exec, true), (ToolKind::Read, true)]);
    }
    assert_eq!(
        d.on_tool_errors(&[(ToolKind::Exec, true), (ToolKind::Read, true)]),
        sleep_s(2)
    );
}

/// Ruling I-1: цикл «правка → ошибка Exec» (TDD) паузы не даёт — успешная правка
/// обнуляет счёт всех классов; и в одном ходе, и в разных.
#[test]
fn edit_then_exec_error_cycle_no_pause() {
    let mut d = DetectorState::default();
    for i in 0..10 {
        assert_eq!(d.on_tool_errors(&[(ToolKind::Edit, false)]), Reaction::None);
        assert_eq!(
            d.on_tool_errors(&[(ToolKind::Exec, true)]),
            Reaction::None,
            "ход {i}"
        );
    }
    let mut d = DetectorState::default();
    for i in 0..10 {
        let r = d.on_tool_errors(&[(ToolKind::Edit, false), (ToolKind::Exec, true)]);
        assert_eq!(r, Reaction::None, "ход {i}");
    }
    // Успешный Exec обнуляет и Read.
    let mut d = DetectorState::default();
    for _ in 0..2 {
        d.on_tool_errors(&[(ToolKind::Read, true)]);
    }
    d.on_tool_errors(&[(ToolKind::Exec, false)]);
    assert_eq!(d.on_tool_errors(&[(ToolKind::Read, true)]), Reaction::None);
}

/// Ruling I-1: ввод пользователя обнуляет счёт ошибок.
#[test]
fn user_input_resets_tool_errors() {
    let mut d = DetectorState::default();
    for _ in 0..2 {
        d.on_tool_errors(&[(ToolKind::Exec, true)]);
    }
    d.on_user_input();
    assert_eq!(d.on_tool_errors(&[(ToolKind::Exec, true)]), Reaction::None);
    assert_eq!(d.on_tool_errors(&[(ToolKind::Exec, true)]), Reaction::None);
    assert_eq!(d.on_tool_errors(&[(ToolKind::Exec, true)]), sleep_s(1));
}

/// Нормальная работа: ошибки вперемешку с успехами того же класса, редкие ошибки
/// разных классов — ни одной паузы.
#[test]
fn tool_errors_normal_work_no_reaction() {
    let mut d = DetectorState::default();
    for i in 0..50 {
        let r = d.on_tool_errors(&[
            (ToolKind::Read, i % 3 == 0),
            (ToolKind::Control, i % 2 == 0),
        ]);
        assert_eq!(r, Reaction::None, "ход {i}");
        let r = d.on_tool_errors(&[(ToolKind::Control, false)]);
        assert_eq!(r, Reaction::None, "ход {i}");
    }
    assert_eq!(d.on_tool_errors(&[]), Reaction::None);
}

// ---- D61: порча JSON ----

#[test]
fn parse_error_chain() {
    let mut d = DetectorState::default();
    assert_eq!(d.on_parse_errors(1), Reaction::None);
    assert_eq!(d.on_parse_errors(1), Reaction::None);
    assert_eq!(d.on_parse_errors(1), Reaction::Compact);
    assert!(d.compaction_due());
    assert_eq!(d.on_parse_errors(1), Reaction::None);
    assert_eq!(
        d.on_parse_errors(2),
        Reaction::Pause(pause_reason::MODEL_LOOP)
    );
    // `on_parse_errors(0)` сбрасывает серию — и до компакции, и после неё.
    let mut d = DetectorState::default();
    d.on_parse_errors(1);
    d.on_parse_errors(1);
    assert_eq!(d.on_parse_errors(0), Reaction::None);
    assert_eq!(d.on_parse_errors(1), Reaction::None);
    assert_eq!(d.on_parse_errors(1), Reaction::None);
    assert_eq!(d.on_parse_errors(1), Reaction::Compact);
    assert_eq!(d.on_parse_errors(1), Reaction::None);
    assert_eq!(d.on_parse_errors(0), Reaction::None);
    assert_eq!(d.on_parse_errors(1), Reaction::None);
    assert_eq!(d.on_parse_errors(1), Reaction::None);
    assert_eq!(d.on_parse_errors(1), Reaction::Compact, "серия с начала");
}

/// Редкие неразобранные вызовы (не подряд) — ни компакции, ни паузы.
#[test]
fn parse_errors_sporadic_no_reaction() {
    let mut d = DetectorState::default();
    for i in 0..60 {
        let n = u32::from(i % 3 != 2);
        assert_eq!(d.on_parse_errors(n), Reaction::None, "ход {i}");
    }
    assert!(!d.compaction_due());
}

// ---- D62: падение кэша ----

#[test]
fn cache_drop_alert() {
    let refs = [h("m0"), h("m1"), h("m2"), h("m3"), h("m4")];
    let mut d = DetectorState::default();
    assert_eq!(d.on_usage(&usage(1000, 900), true, 0, &[], &refs), None);
    let v = d
        .on_usage(&usage(1100, 100), true, 0, &refs, &refs)
        .expect("падение кэша");
    assert_eq!(v["kind"], "cache_drop");
    assert!(
        (v["prev_share"].as_f64().unwrap() - 0.9).abs() < 1e-9,
        "{v}"
    );
    assert!(
        (v["share"].as_f64().unwrap() - 100.0 / 1100.0).abs() < 1e-9,
        "{v}"
    );
    assert_eq!(v["first_diff"], Value::Null);

    // Ссылки отличаются с индекса 3.
    let changed = [h("m0"), h("m1"), h("m2"), h("x3"), h("m4"), h("m5")];
    let mut d = DetectorState::default();
    d.on_usage(&usage(1000, 900), true, 0, &[], &refs);
    let v = d
        .on_usage(&usage(1100, 100), true, 0, &refs, &changed)
        .expect("падение кэша");
    assert_eq!(v["first_diff"], 3);

    // Эпоха изменилась (удаление или компакция) — не падение.
    let mut d = DetectorState::default();
    d.on_usage(&usage(1000, 900), true, 0, &[], &refs);
    assert_eq!(d.on_usage(&usage(1100, 100), true, 1, &refs, &refs), None);

    // Эндпоинт без поля кэша — никогда.
    let mut d = DetectorState::default();
    d.on_usage(&usage(1000, 900), false, 0, &[], &refs);
    assert_eq!(d.on_usage(&usage(1100, 100), false, 0, &refs, &refs), None);
}

/// Нормальные ходы: доля стабильна или растёт, большой новый результат снижает долю,
/// но не число токенов из кэша, прошлая доля ниже 0,5, расход неизвестен.
#[test]
fn cache_normal_turns_no_alert() {
    let refs = [h("m0")];
    let mut d = DetectorState::default();
    let mut input = 1000;
    for i in 0..10 {
        let cached = input * 9 / 10;
        assert_eq!(
            d.on_usage(&usage(input, cached), true, 0, &refs, &refs),
            None,
            "ход {i}"
        );
        input += 200;
    }
    // Результат инструмента 20 000 токенов: доля 0,1, токены из кэша прежние.
    let mut d = DetectorState::default();
    d.on_usage(&usage(1000, 900), true, 0, &refs, &refs);
    assert_eq!(
        d.on_usage(&usage(21_000, 1000), true, 0, &refs, &refs),
        None
    );
    // Прошлая доля ниже 0,5.
    let mut d = DetectorState::default();
    d.on_usage(&usage(1000, 400), true, 0, &refs, &refs);
    assert_eq!(d.on_usage(&usage(1100, 10), true, 0, &refs, &refs), None);
    // Первый ход и расход неизвестен (`input == 0`): сравнивать не с чем.
    let mut d = DetectorState::default();
    assert_eq!(d.on_usage(&usage(1000, 900), true, 0, &refs, &refs), None);
    assert_eq!(d.on_usage(&usage(0, 0), true, 0, &refs, &refs), None);
    assert_eq!(d.on_usage(&usage(1100, 100), true, 0, &refs, &refs), None);
}

#[test]
fn turn_state_roundtrips_and_defaults() {
    let mut d = DetectorState::default();
    d.on_promise(RunRole::Top);
    d.on_tool_errors(&[(ToolKind::Exec, true)]);
    d.on_parse_errors(1);
    d.on_usage(&usage(1000, 900), true, 2, &[], &[]);
    let v = serde_json::to_value(&d).unwrap();
    assert_eq!(
        serde_json::from_value::<DetectorState>(v.clone()).unwrap(),
        d
    );
    // Снимок без детекторов хода (задачи 6–7) — пустое состояние.
    let mut old = v;
    old.as_object_mut().unwrap().remove("turn");
    let back: DetectorState = serde_json::from_value(old).unwrap();
    assert_eq!(back, DetectorState::default());
}

// ---- Драйвер ----

/// Обёртка `ModelPort`: время (`tokio::time::Instant`) каждого вызова модели.
struct Timed {
    inner: Arc<ScriptedModel>,
    at: Mutex<Vec<Instant>>,
}

impl Timed {
    fn times(&self) -> Vec<Instant> {
        self.at.lock().unwrap().clone()
    }
}

impl ModelPort for Timed {
    fn stream(&self, call: ModelCall) -> BoxFuture<'_, Result<ModelStream, ModelError>> {
        self.at.lock().unwrap().push(Instant::now());
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
}

/// `bash` (класс Exec) — всегда ошибка; `read` (Read) — содержимое пути.
struct Probe {
    spec: ToolSpec,
}

impl Tool for Probe {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let out = match self.spec.schema.name.as_str() {
            "bash" => ToolOutput::error(format!("exit 1: {}", args["cmd"])),
            "fail" => ToolOutput::error(format!("no such file {}", args["p"])),
            _ => ToolOutput::ok(format!("content of {}", args["p"])),
        };
        Box::pin(async move { out })
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
        // Запуск без одобрения в режиме Ask.
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
    timed: Arc<Timed>,
    deps: LoopDeps,
    run: RunId,
    bus: EventBus,
}

fn fx(name: &str) -> Fx {
    fx_with(
        name,
        ScriptedModel::new(default_caps(BackendKind::LlamaCpp)),
    )
}

fn fx_with(name: &str, model: ScriptedModel) -> Fx {
    let d = unique_dir("detect_turn", name);
    let store = ProjectStore::open(&d).unwrap();
    let model = Arc::new(model);
    let timed = Arc::new(Timed {
        inner: Arc::clone(&model),
        at: Mutex::new(Vec::new()),
    });
    let mut reg = ToolRegistry::default();
    for t in builtin_tools() {
        reg.register(t).unwrap();
    }
    for (name, kind, class) in [
        ("bash", ToolKind::Exec, EffectClass::NonIdempotent),
        ("read", ToolKind::Read, EffectClass::ReadOnly),
        ("fail", ToolKind::Read, EffectClass::ReadOnly),
    ] {
        reg.register(Arc::new(Probe {
            spec: spec_of(name, kind, class),
        }))
        .unwrap();
    }
    let bus = EventBus::new(EventBus::DEFAULT_CAPACITY);
    let deps = LoopDeps {
        capabilities: Vec::new(),
        memory: Arc::new(NoMemory),
        crash: None,
        fault: None,
        ..LoopDeps::new(
            Journal::new(store.clone(), bus.clone()),
            timed.clone(),
            Arc::new(reg),
        )
    };
    Fx {
        store,
        model,
        timed,
        deps,
        run: RunId::new(),
        bus,
    }
}

fn spec(role: RunRole) -> RunSpec {
    spec_mode(role, RunMode::Ask)
}

fn spec_mode(role: RunRole, run_mode: RunMode) -> RunSpec {
    RunSpec {
        project_id: "p".into(),
        project_dir: PathBuf::from("H:/p"),
        role,
        parent: None,
        endpoint: EndpointId("local".into()),
        run_mode,
        work_mode: None,
        tier: None,
        budget: Budget::default(),
        priority: Priority::Top,
        max_wait: None,
        task: "Запусти тесты".into(),
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
        trusted: true,
        grants: Vec::new(),
    }
}

async fn start(f: &Fx, role: RunRole) -> LoopOutcome {
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    within("start_run", start_run(&f.deps, f.run, spec(role), rx))
        .await
        .unwrap()
}

fn crash_at(at: CrashPoint) -> Option<za_agent::CrashHook> {
    Some(Arc::new(move |p: CrashPoint| p == at))
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

fn text(t: &str) -> Script {
    let mut s = vec![Ok(StreamEvent::TextDelta(t.into()))];
    s.extend(finished(
        vec![Part::Text(t.into())],
        FinishReason::Stop,
        Usage::default(),
    ));
    s
}

fn call_part(id: &str, name: &str, args: Value) -> Part {
    Part::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        raw_arguments: None,
    })
}

fn bash(id: &str) -> Script {
    finished(
        vec![call_part(id, "bash", json!({"cmd": "cargo test"}))],
        FinishReason::ToolCalls,
        Usage::default(),
    )
}

/// Нативный вызов `read` с испорченным JSON аргументов: `parse_errors = 1`.
fn broken(id: &str) -> Script {
    finished(
        vec![Part::ToolCall(ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: Value::Null,
            raw_arguments: Some("{\"p\": \"a".into()),
        })],
        FinishReason::ToolCalls,
        Usage::default(),
    )
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

async fn last_snapshot(s: &ProjectStore, run: RunId) -> Snapshot {
    let (_, v) = s.latest_snapshot(run).await.unwrap().expect("нет снимка");
    serde_json::from_value(v).unwrap()
}

async fn history(f: &Fx) -> Conversation {
    let snap = last_snapshot(&f.store, f.run).await;
    Conversation::load(&f.store, &snap.messages).await.unwrap()
}

/// Сообщения пользователя (и рантайма), содержащие `needle`.
fn user_with(c: &Conversation, needle: &str) -> usize {
    c.messages()
        .iter()
        .filter(|m| m.role == Role::User)
        .filter(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, Part::Text(t) if t.contains(needle)))
        })
        .count()
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

async fn outcomes(f: &Fx) -> Vec<String> {
    payloads(f, EventKind::TurnFinished)
        .await
        .iter()
        .map(|p| p["outcome"].as_str().unwrap().to_owned())
        .collect()
}

/// (detector, action, turn) событий `loop.detected`.
async fn detected(f: &Fx) -> Vec<(String, String, u64)> {
    payloads(f, EventKind::LoopDetected)
        .await
        .iter()
        .map(|p| {
            (
                p["detector"].as_str().unwrap().to_owned(),
                p["action"].as_str().unwrap().to_owned(),
                p["turn"].as_u64().unwrap(),
            )
        })
        .collect()
}

fn triples(v: &[(&str, &str, u64)]) -> Vec<(String, String, u64)> {
    v.iter()
        .map(|(a, b, t)| ((*a).to_owned(), (*b).to_owned(), *t))
        .collect()
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

fn status_is(want: RunStatus) -> impl Fn(&Value) -> bool {
    move |p| serde_json::from_value::<RunStatus>(p["to"].clone()).ok() == Some(want)
}

// ---- D60 в драйвере ----

#[tokio::test]
async fn promise_checked_before_top_answer() {
    let f = fx("promise_top");
    let t = "Let me run the tests.";
    f.model.push(text(t));
    f.model.push(text(t));
    assert_eq!(start(&f, RunRole::Top).await, done(t));
    assert_eq!(outcomes(&f).await, ["promise", "answer"]);
    assert_eq!(f.model.calls().len(), 2, "первое обещание — не ответ");
    let conv = history(&f).await;
    assert_eq!(user_with(&conv, PROMISE_NUDGE), 1, "nudge в истории");
    let snap = last_snapshot(&f.store, f.run).await;
    assert_eq!(snap.counters.empty_turns, 0, "обещание — не пустой ход");
    assert!(!snap.counters.forced_required);
    // Второй запрос модели уже видит nudge.
    let second = &f.model.calls()[1].request.messages;
    assert!(
        second.iter().any(|m| m
            .parts
            .iter()
            .any(|p| matches!(p, Part::Text(x) if x.contains(PROMISE_NUDGE)))),
        "nudge не дошёл до модели"
    );
    assert_eq!(detected(&f).await, triples(&[("promise", "nudge", 1)]));
}

/// Worker: второе обещание подряд — пустой ход D18 (nudge субагента, счёт растёт).
#[tokio::test]
async fn promise_worker_second_is_empty_turn() {
    let f = fx("promise_worker");
    let t = "I'll open the file now.";
    f.model.push(text(t));
    f.model.push(text(t));
    f.model.push(finished(
        vec![call_part(
            "f1",
            "finish",
            json!({"status": "done", "result": "ok"}),
        )],
        FinishReason::ToolCalls,
        Usage::default(),
    ));
    let out = start(&f, RunRole::Worker).await;
    assert!(
        matches!(
            out,
            LoopOutcome::Finished {
                status: RunStatus::Done,
                ..
            }
        ),
        "{out:?}"
    );
    assert_eq!(outcomes(&f).await, ["promise", "empty", "finish"]);
    let conv = history(&f).await;
    assert_eq!(user_with(&conv, PROMISE_NUDGE), 1);
    assert_eq!(
        user_with(&conv, "Your last response made no tool calls."),
        1,
        "второе обещание — пустой ход"
    );
}

/// Нормальный ход: «I will now read the file» вместе с вызовом — не обещание; ответ
/// top без оборота обещания — сразу ответ.
#[tokio::test]
async fn promise_with_call_is_not_promise() {
    let f = fx("promise_with_call");
    f.model.push(finished(
        vec![
            Part::Text("I will now read the file.".into()),
            call_part("c1", "read", json!({"p": "a"})),
        ],
        FinishReason::ToolCalls,
        Usage::default(),
    ));
    f.model
        .push(text("Файл прочитан. Let me know if you need more."));
    assert_eq!(
        start(&f, RunRole::Top).await,
        done("Файл прочитан. Let me know if you need more.")
    );
    assert_eq!(outcomes(&f).await, ["calls", "answer"]);
    assert!(detected(&f).await.is_empty());
    assert_eq!(user_with(&history(&f).await, PROMISE_NUDGE), 0);
}

// ---- D57 в драйвере ----

/// Часы остановлены (`start_paused`), рантайм продвигает их сам; внешнего предела по
/// времени нет: пока тест ждёт писателя журнала (поток ОС), автопродвижение сработало
/// бы на нём. Проверяются только нижние границы пауз.
#[tokio::test(start_paused = true)]
async fn tool_error_sleep_applied() {
    let f = fx("sleep_applied");
    for n in 1..=6 {
        f.model.push(bash(&format!("c{n}")));
    }
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    let out = start_run(&f.deps, f.run, spec(RunRole::Top), rx)
        .await
        .unwrap();
    assert_eq!(out, paused(pause_reason::TOOL_ERRORS));
    let at = f.timed.times();
    assert_eq!(at.len(), 6, "шестой ошибки достаточно для паузы");
    let gap = |i: usize| at[i] - at[i - 1];
    assert!(
        gap(3) >= Duration::from_secs(1),
        "после третьей: {:?}",
        gap(3)
    );
    assert!(
        gap(4) >= Duration::from_secs(2),
        "после четвёртой: {:?}",
        gap(4)
    );
    assert!(
        gap(5) >= Duration::from_secs(4),
        "после пятой: {:?}",
        gap(5)
    );
    assert_eq!(
        detected(&f).await,
        triples(&[
            ("tool_errors", "sleep", 3),
            ("tool_errors", "sleep", 4),
            ("tool_errors", "sleep", 5),
            ("tool_errors", "pause", 6),
        ])
    );
    let r = f.store.get_run(f.run).await.unwrap().unwrap();
    assert_eq!(r.status, RunStatus::Paused);
}

/// Пауза во время ожидания после ошибок (D57, NFR-03): сигнал подаётся по событию
/// `loop.detected {detector: "tool_errors", action: "sleep"}`; `pausing` пишется в
/// момент наблюдения флага, не через 1 с.
#[tokio::test]
async fn pause_during_backoff() {
    let f = fx("pause_backoff");
    for n in 1..=3 {
        f.model.push(bash(&format!("c{n}")));
    }
    f.model.push(text("не дойдёт"));
    let (ctl, rx) = control_channel(f.store.clone(), f.run);
    let mut bus = f.bus.subscribe();
    let watcher = tokio::spawn(async move {
        nth_of(&mut bus, EventKind::LoopDetected, 1, |p| {
            p["detector"] == "tool_errors" && p["action"] == "sleep"
        })
        .await;
        let at = std::time::Instant::now();
        ctl.pause();
        nth_of(
            &mut bus,
            EventKind::RunStatus,
            1,
            status_is(RunStatus::Pausing),
        )
        .await;
        let pausing = at.elapsed();
        nth_of(
            &mut bus,
            EventKind::RunStatus,
            1,
            status_is(RunStatus::Paused),
        )
        .await;
        (pausing, at.elapsed())
    });
    let out = within(
        "start_run",
        start_run(&f.deps, f.run, spec(RunRole::Top), rx),
    )
    .await
    .unwrap();
    let (pausing, lag) = within("наблюдатель", watcher).await.unwrap();
    eprintln!("NFR-03 pause_during_backoff: pausing {pausing:?}, paused {lag:?}");
    assert_eq!(out, paused(pause_reason::COMMAND));
    assert!(pausing <= nfr03(), "pausing записан за {pausing:?}");
    assert_eq!(f.model.calls().len(), 3, "ход после паузы не начат");
    let statuses: Vec<RunStatus> = payloads(&f, EventKind::RunStatus)
        .await
        .iter()
        .map(|p| serde_json::from_value(p["to"].clone()).unwrap())
        .collect();
    assert_eq!(
        statuses,
        [RunStatus::Running, RunStatus::Pausing, RunStatus::Paused]
    );
}

/// Счёт ошибок — в снимке (D66): после сбоя и resume третья ошибка даёт паузу 1 с.
#[tokio::test]
async fn tool_errors_survive_resume() {
    let f = fx("errors_resume");
    f.model.push(bash("c1"));
    f.model.push(bash("c2"));
    let mut crashing = f.deps.clone();
    crashing.crash = crash_at(CrashPoint::AfterBoundary { step: 3 });
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    let err = within(
        "start_run",
        start_run(&crashing, f.run, spec(RunRole::Top), rx),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, LoopError::Halted), "{err:?}");
    assert!(detected(&f).await.is_empty());

    f.model.push(bash("c3"));
    f.model.push(text("Готово."));
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    let out = within(
        "resume_run",
        resume_run(&f.deps, f.run, ResumeChange::default(), rx),
    )
    .await
    .unwrap();
    assert_eq!(out, done("Готово."));
    assert_eq!(detected(&f).await, triples(&[("tool_errors", "sleep", 3)]));
}

/// Нормальная работа: ошибки Exec вперемешку с успешными чтениями и успешным Exec —
/// без пауз. Повтор прошлого результата (D54) счёт не трогает.
#[tokio::test]
async fn tool_errors_with_successes_no_sleep() {
    let f = fx("errors_normal");
    f.model.push(bash("c1"));
    f.model.push(bash("c2"));
    // Чтение другого класса не сбрасывает Exec, но и не двигает его.
    f.model.push(finished(
        vec![call_part("c3", "read", json!({"p": "a"}))],
        FinishReason::ToolCalls,
        Usage::default(),
    ));
    f.model
        .push(text("Тесты падают на сборке, нужен ваш ответ по версии."));
    assert_eq!(
        start(&f, RunRole::Top).await,
        done("Тесты падают на сборке, нужен ваш ответ по версии.")
    );
    assert!(detected(&f).await.is_empty(), "{:?}", detected(&f).await);
}

// ---- D61 в драйвере ----

/// Три хода подряд с порчей JSON — аварийная компакция на ближайшей границе; ещё два
/// — пауза `model_loop`.
#[tokio::test]
async fn parse_errors_compact_then_pause() {
    let f = fx_with(
        "parse_chain",
        ScriptedModel::new(default_caps(BackendKind::LlamaCpp)).with_context(32768),
    );
    for n in 1..=3 {
        f.model.push(broken(&format!("c{n}")));
    }
    f.model.push(text(
        "## Decisions\nd\n## Files changed\n-\n## Errors\n-\n## Next step\nn",
    ));
    f.model.push(broken("c4"));
    f.model.push(broken("c5"));
    assert_eq!(
        start(&f, RunRole::Top).await,
        paused(pause_reason::MODEL_LOOP)
    );
    assert_eq!(
        detected(&f).await,
        triples(&[("parse_errors", "compact", 3), ("parse_errors", "pause", 5)])
    );
    let compacted = payloads(&f, EventKind::ContextCompacted).await;
    assert_eq!(compacted.len(), 1, "{compacted:?}");
    assert_eq!(compacted[0]["reason"], "parse_errors");
    assert_eq!(compacted[0]["emergency"], true);
    assert_eq!(f.model.calls().len(), 6);
    let snap = last_snapshot(&f.store, f.run).await;
    assert!(!snap.detectors.compaction_due());
}

// ---- D62 в драйвере ----

#[tokio::test]
async fn cache_drop_alert_in_journal() {
    let f = fx_with(
        "cache_drop",
        ScriptedModel::new(default_caps(BackendKind::LlamaCpp)).with_context(32768),
    );
    f.model.push(finished(
        vec![call_part("c1", "read", json!({"p": "a"}))],
        FinishReason::ToolCalls,
        usage(4000, 3600),
    ));
    let mut answer = text("Готово.");
    if let Some(Ok(StreamEvent::Finished { usage: u, .. })) = answer.last_mut() {
        *u = usage(4400, 400);
    }
    f.model.push(answer);
    assert_eq!(start(&f, RunRole::Top).await, done("Готово."));
    let alerts: Vec<Value> = payloads(&f, EventKind::ContextAlert)
        .await
        .into_iter()
        .filter(|p| p["kind"] == "cache_drop")
        .collect();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    let a = &alerts[0];
    assert!(
        (a["prev_share"].as_f64().unwrap() - 0.9).abs() < 1e-9,
        "{a}"
    );
    assert!(
        (a["share"].as_f64().unwrap() - 400.0 / 4400.0).abs() < 1e-9,
        "{a}"
    );
    assert_eq!(
        a["first_diff"],
        Value::Null,
        "история та же: кэш вытеснил сервер"
    );
    assert_eq!(a["turn"], 2);
}

/// Нормальные ходы с устойчивым кэшем — без алерта.
#[tokio::test]
async fn cache_steady_no_alert() {
    let f = fx_with(
        "cache_steady",
        ScriptedModel::new(default_caps(BackendKind::LlamaCpp)).with_context(32768),
    );
    f.model.push(finished(
        vec![call_part("c1", "read", json!({"p": "a"}))],
        FinishReason::ToolCalls,
        usage(4000, 3600),
    ));
    let mut answer = text("Готово.");
    if let Some(Ok(StreamEvent::Finished { usage: u, .. })) = answer.last_mut() {
        *u = usage(4400, 4000);
    }
    f.model.push(answer);
    assert_eq!(start(&f, RunRole::Top).await, done("Готово."));
    let alerts = payloads(&f, EventKind::ContextAlert)
        .await
        .into_iter()
        .filter(|p| p["kind"] == "cache_drop")
        .count();
    assert_eq!(alerts, 0);
}

// ---- Исправления после ревью (раунд 1) ----

fn fail_calls(ids: &[&str]) -> Script {
    finished(
        ids.iter()
            .map(|id| call_part(id, "fail", json!({ "p": id })))
            .collect(),
        FinishReason::ToolCalls,
        Usage::default(),
    )
}

async fn tool_error_events(f: &Fx) -> Vec<(String, String, u64)> {
    detected(f)
        .await
        .into_iter()
        .filter(|(d, _, _)| d == "tool_errors")
        .collect()
}

/// I-1: шесть параллельных ошибок чтения в одном ходе — одна ошибка хода, без паузы.
#[tokio::test]
async fn parallel_errors_one_turn_no_pause() {
    let f = fx("parallel_errors");
    f.model
        .push(fail_calls(&["c1", "c2", "c3", "c4", "c5", "c6"]));
    f.model.push(text("Файлов нет."));
    assert_eq!(start(&f, RunRole::Top).await, done("Файлов нет."));
    assert!(tool_error_events(&f).await.is_empty());
}

/// M-4: повтор упавшего чтения — отказ D54 без исполнения, в счёт D57 не идёт.
#[tokio::test]
async fn repeated_failed_read_not_counted() {
    let f = fx("reuse_not_counted");
    for n in 1..=4 {
        f.model.push(finished(
            vec![call_part(&format!("c{n}"), "fail", json!({"p": "a"}))],
            FinishReason::ToolCalls,
            Usage::default(),
        ));
    }
    f.model.push(text("Файла нет."));
    assert_eq!(start(&f, RunRole::Top).await, done("Файла нет."));
    let d = detected(&f).await;
    assert!(d.iter().any(|(det, _, _)| det == "repeat"), "{d:?}");
    assert!(tool_error_events(&f).await.is_empty(), "{d:?}");
}

/// M-4: отказ гейта (режим Plan) — не исполнение, в счёт D57 не идёт.
#[tokio::test]
async fn gate_denials_not_counted() {
    let f = fx("gate_not_counted");
    for n in 1..=6 {
        f.model.push(bash(&format!("c{n}")));
    }
    f.model.push(text("В режиме плана запуск запрещён."));
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    let out = within(
        "start_run",
        start_run(&f.deps, f.run, spec_mode(RunRole::Top, RunMode::Plan), rx),
    )
    .await
    .unwrap();
    assert_eq!(out, done("В режиме плана запуск запрещён."));
    let started = events(&f.store, f.run)
        .await
        .iter()
        .filter(|e| e.kind == EventKind::ToolStarted)
        .count();
    assert_eq!(started, 0, "bash не исполнялся");
    assert!(tool_error_events(&f).await.is_empty());
}

/// M-5: сбой после третьего испорченного хода — после resume компакция `parse_errors`
/// ровно одна, третий ход не спрашивается у модели заново.
#[tokio::test]
async fn parse_errors_compaction_replays_once() {
    let f = fx_with(
        "parse_replay",
        ScriptedModel::new(default_caps(BackendKind::LlamaCpp)).with_context(32768),
    );
    for n in 1..=3 {
        f.model.push(broken(&format!("c{n}")));
    }
    let mut crashing = f.deps.clone();
    crashing.crash = crash_at(CrashPoint::AfterModelEffect { step: 3 });
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    let err = within(
        "start_run",
        start_run(&crashing, f.run, spec(RunRole::Top), rx),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, LoopError::Halted), "{err:?}");
    assert_eq!(f.model.calls().len(), 3);

    f.model.push(text(
        "## Decisions\nd\n## Files changed\n-\n## Errors\n-\n## Next step\nn",
    ));
    f.model.push(text("Готово."));
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    let out = within(
        "resume_run",
        resume_run(&f.deps, f.run, ResumeChange::default(), rx),
    )
    .await
    .unwrap();
    assert_eq!(out, done("Готово."));
    assert_eq!(f.model.calls().len(), 5, "третий ход — из квитанции");
    let compacted = payloads(&f, EventKind::ContextCompacted).await;
    assert_eq!(compacted.len(), 1, "{compacted:?}");
    assert_eq!(compacted[0]["reason"], "parse_errors");
    let compact = detected(&f)
        .await
        .into_iter()
        .filter(|(d, a, _)| d == "parse_errors" && a == "compact")
        .count();
    assert_eq!(compact, 1);
}

/// Minor (D62): метка кэша сбрасывается при resume — первый ход после него не
/// сравнивается с ходом до паузы.
#[tokio::test]
async fn cache_mark_reset_on_resume() {
    let f = fx_with(
        "cache_resume",
        ScriptedModel::new(default_caps(BackendKind::LlamaCpp)).with_context(32768),
    );
    f.model.push(finished(
        vec![call_part("c1", "read", json!({"p": "a"}))],
        FinishReason::ToolCalls,
        usage(4000, 3600),
    ));
    // Чистая пауза по команде после первого хода (по `tool.started`): resume начнёт
    // новую границу, а не реплей хода с меткой.
    let (ctl, rx) = control_channel(f.store.clone(), f.run);
    let mut bus = f.bus.subscribe();
    let watcher = tokio::spawn(async move {
        nth_of(&mut bus, EventKind::ToolStarted, 1, |_| true).await;
        ctl.pause();
    });
    let out = within(
        "start_run",
        start_run(&f.deps, f.run, spec(RunRole::Top), rx),
    )
    .await
    .unwrap();
    within("наблюдатель", watcher).await.unwrap();
    assert_eq!(out, paused(pause_reason::COMMAND));
    let snap = last_snapshot(&f.store, f.run).await;
    assert_ne!(
        snap.detectors,
        DetectorState::default(),
        "метка кэша в снимке"
    );

    let mut answer = text("Готово.");
    if let Some(Ok(StreamEvent::Finished { usage: u, .. })) = answer.last_mut() {
        *u = usage(4400, 400);
    }
    f.model.push(answer);
    let (_ctl, rx) = control_channel(f.store.clone(), f.run);
    let out = within(
        "resume_run",
        resume_run(&f.deps, f.run, ResumeChange::default(), rx),
    )
    .await
    .unwrap();
    assert_eq!(out, done("Готово."));
    let alerts = payloads(&f, EventKind::ContextAlert)
        .await
        .into_iter()
        .filter(|p| p["kind"] == "cache_drop")
        .count();
    assert_eq!(alerts, 0);
}
