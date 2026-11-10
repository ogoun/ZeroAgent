//! Ввод пользователя и управление (задача 14): ввод `queue` и `interrupt` (D25),
//! пауза и отмена со статусами `pausing` и `cancelling` (D26), поправка `correct`
//! с идентификатором команды (D36), вычерпывание сигналов на границе (D9), NFR-03.
//!
//! Сигналы посылаются по событию шины (первая дельта, `tool.started`), а не по
//! таймеру от начала: под нагрузкой таймер приходил до открытия стрима. Инструмент
//! `hold` ждёт разрешения теста, поэтому «пока инструмент идёт» — факт, а не тайминг.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{Semaphore, broadcast};
use tokio::time::Instant;
use za_agent::{
    Conversation, EnvInfo, InputMode, LoopDeps, LoopOutcome, NoMemory, RunControl, RunSpec,
    Snapshot, builtin_tools, control_channel, start_run, submit_input,
};
use za_core::{
    AgentEvent, Approval, EventBus, Journal, Tool, ToolCx, ToolKind, ToolOutput, ToolRegistry,
    ToolSpec,
};
use za_llm::{
    BackendKind, BoxFuture, EndpointId, FinishReason, Message, ModelError, Part, Role,
    ScriptedModel, StreamEvent, ToolCall, ToolSchema, Usage, default_caps,
};
use za_protocol::{
    Budget, EffectClass, EffectKey, Event, EventKind, Priority, RunId, RunMode, RunRole, RunStatus,
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

/// Предел ожиданий, которые при регрессии повисли бы.
fn limit() -> Duration {
    Duration::from_secs(10) * time_scale()
}

/// NFR-03: эффект сигнала не позже 200 мс (× `ZA_TEST_TIME_SCALE`).
fn nfr03() -> Duration {
    Duration::from_millis(200) * time_scale()
}

async fn within<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(limit(), f)
        .await
        .unwrap_or_else(|_| panic!("не дождались в срок: {what}"))
}

fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("control")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Инструмент чтения `hold`: ждёт разрешения теста (`release`), результат `held {args}`.
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
            let permit = tokio::time::timeout(limit(), release.acquire())
                .await
                .expect("инструмент hold не получил разрешения в срок")
                .expect("семафор закрыт");
            permit.forget();
            ToolOutput {
                content: format!("held {args}"),
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
    ctl: RunControl,
    rx: Option<za_agent::ControlRx>,
    bus: EventBus,
    release: Arc<Semaphore>,
}

/// Модель без параллельных вызовов (LlamaCpp): чтения пакета идут по одному.
fn fx(name: &str) -> Fx {
    let store = ProjectStore::open(&dir(name)).unwrap();
    let model = Arc::new(ScriptedModel::new(default_caps(BackendKind::LlamaCpp)));
    let bus = EventBus::new(EventBus::DEFAULT_CAPACITY);
    let release = Arc::new(Semaphore::new(0));
    let mut reg = ToolRegistry::default();
    for t in builtin_tools() {
        reg.register(t).unwrap();
    }
    reg.register(Arc::new(Hold {
        spec: tool_spec("hold"),
        release: Arc::clone(&release),
    }))
    .unwrap();
    let deps = LoopDeps {
        journal: Journal::new(store.clone(), bus.clone()),
        model: model.clone(),
        tools: Arc::new(reg),
        capabilities: Vec::new(),
        memory: Arc::new(NoMemory),
        crash: None,
    };
    let run = RunId::new();
    let (ctl, rx) = control_channel(store.clone(), run);
    Fx {
        store,
        model,
        deps,
        run,
        ctl,
        rx: Some(rx),
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

async fn start(f: &mut Fx) -> LoopOutcome {
    let rx = f.rx.take().expect("ControlRx уже взят");
    within("start_run", start_run(&f.deps, f.run, spec(), rx))
        .await
        .expect("цикл завершился ошибкой")
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

fn hold_calls(ids: &[&str]) -> Vec<Result<StreamEvent, ModelError>> {
    vec![Ok(StreamEvent::Finished {
        message: Message {
            role: Role::Assistant,
            parts: ids
                .iter()
                .map(|id| {
                    Part::ToolCall(ToolCall {
                        id: (*id).into(),
                        name: "hold".into(),
                        arguments: json!({"id": id}),
                        raw_arguments: None,
                    })
                })
                .collect(),
        },
        finish: FinishReason::ToolCalls,
        usage: Usage::default(),
    })]
}

fn done(t: &str) -> LoopOutcome {
    LoopOutcome::Finished {
        status: RunStatus::Done,
        answer: Some(t.into()),
        finish: None,
    }
}

/// Ждёт на шине первое событие вида `kind` (до предела).
async fn next_of(bus: &mut broadcast::Receiver<AgentEvent>, kind: EventKind) -> AgentEvent {
    within(&format!("событие {kind:?}"), async {
        loop {
            let ev = bus.recv().await.expect("шина закрыта");
            if ev.event.kind == kind {
                return ev;
            }
        }
    })
    .await
}

/// От сигнала до drop стрима (NFR-03): опрос `open_streams()` каждую миллисекунду.
async fn until_dropped(model: &ScriptedModel, at: Instant) -> Duration {
    within("drop стрима", async {
        while model.open_streams() > 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    at.elapsed()
}

/// Ждёт на шине `run.status` с `to == want`.
async fn next_status(bus: &mut broadcast::Receiver<AgentEvent>, want: RunStatus) {
    within(&format!("статус {want:?}"), async {
        loop {
            let ev = bus.recv().await.expect("шина закрыта");
            if ev.event.kind == EventKind::RunStatus
                && serde_json::from_value::<RunStatus>(ev.event.payload["to"].clone()).ok()
                    == Some(want)
            {
                return;
            }
        }
    })
    .await;
}

async fn events(s: &ProjectStore, run: RunId) -> Vec<Event> {
    s.read_run(run, Seq(0), 100_000).await.unwrap()
}

fn of_kind(evs: &[Event], k: EventKind) -> Vec<&Event> {
    evs.iter().filter(|e| e.kind == k).collect()
}

/// Цепочка статусов run по событиям `run.status`.
fn statuses(evs: &[Event]) -> Vec<RunStatus> {
    of_kind(evs, EventKind::RunStatus)
        .into_iter()
        .map(|e| serde_json::from_value(e.payload["to"].clone()).unwrap())
        .collect()
}

/// События после первой дельты с отметками времени журнала от неё — для сообщения
/// о NFR-03: видно, какая запись журнала задержала статус.
fn timeline(evs: &[Event]) -> String {
    let Some(t0) = evs
        .iter()
        .find(|e| e.kind == EventKind::ModelDelta)
        .map(|e| e.ts)
    else {
        return String::new();
    };
    evs.iter()
        .filter(|e| e.ts >= t0)
        .map(|e| {
            let ms = (e.ts - t0).total(jiff::Unit::Millisecond).unwrap_or(-1.0);
            format!("{ms:.1} мс {}", e.kind.as_str())
        })
        .collect::<Vec<_>>()
        .join("; ")
}

async fn last_history(s: &ProjectStore, run: RunId) -> Conversation {
    let (_, v) = s.latest_snapshot(run).await.unwrap().expect("нет снимка");
    let snap: Snapshot = serde_json::from_value(v).unwrap();
    Conversation::load(s, &snap.messages).await.unwrap()
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

/// Позиция user-сообщения с текстом `t` в сообщениях запроса.
fn user_at(msgs: &[Message], t: &str) -> Option<usize> {
    msgs.iter()
        .position(|m| m.role == Role::User && text_of(m) == t)
}

/// Результат вызова `call_id` в сообщениях.
fn result_of(msgs: &[Message], id: &str) -> Option<(String, bool)> {
    msgs.iter().flat_map(|m| &m.parts).find_map(|p| match p {
        Part::ToolResult {
            call_id,
            content,
            is_error,
        } if call_id == id => Some((content.clone(), *is_error)),
        _ => None,
    })
}

// ---- Тесты брифа ----

/// D25 `queue`: ввод во время инструмента берёт ближайшая граница, после результата.
#[tokio::test]
async fn queued_input_taken_at_boundary() {
    let mut f = fx("queued_input");
    f.model.push(hold_calls(&["c1"]));
    f.model.push(answer("ок"));
    let mut bus = f.bus.subscribe();
    let (ctl, release) = (f.ctl.clone(), Arc::clone(&f.release));
    let watcher = tokio::spawn(async move {
        next_of(&mut bus, EventKind::ToolStarted).await;
        ctl.input("ещё", InputMode::Queue).await.unwrap();
        release.add_permits(1);
    });
    let out = start(&mut f).await;
    within("наблюдатель", watcher).await.unwrap();
    assert_eq!(out, done("ок"));
    let calls = f.model.calls();
    assert_eq!(calls.len(), 2);
    let msgs = &calls[1].request.messages;
    let user = user_at(msgs, "ещё").expect("ввода нет в запросе хода 2");
    let res = msgs
        .iter()
        .position(|m| m.role == Role::Tool)
        .expect("нет результата");
    assert!(user > res, "ввод должен идти после результата");
    assert_eq!(
        result_of(msgs, "c1"),
        Some((r#"held {"id":"c1"}"#.into(), false))
    );
    let evs = events(&f.store, f.run).await;
    let inputs = of_kind(&evs, EventKind::TurnInput);
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].payload, json!({"text": "ещё", "mode": "queue"}));
    assert_eq!(inputs[0].actor, za_protocol::Actor::User);
}

/// D25 `interrupt`: ввод обрывает стрим; обрывка ответа в истории нет.
#[tokio::test]
async fn interrupt_input_aborts_stream() {
    let mut f = fx("interrupt_stream");
    f.model
        .push_open(vec![Ok(StreamEvent::TextDelta("a".into()))]);
    f.model.push(answer("ок"));
    let mut bus = f.bus.subscribe();
    let ctl = f.ctl.clone();
    let watcher = tokio::spawn(async move {
        next_of(&mut bus, EventKind::ModelDelta).await;
        ctl.input("стоп", InputMode::Interrupt).await.unwrap();
    });
    let out = start(&mut f).await;
    within("наблюдатель", watcher).await.unwrap();
    assert_eq!(out, done("ок"));
    assert_eq!(f.model.open_streams(), 0);
    let calls = f.model.calls();
    assert_eq!(calls.len(), 2);
    let msgs = &calls[1].request.messages;
    assert!(user_at(msgs, "стоп").is_some(), "{msgs:?}");
    assert!(
        !msgs
            .iter()
            .any(|m| m.role == Role::Assistant && text_of(m).contains('a')),
        "обрывок ответа в истории: {msgs:?}"
    );
}

/// D25, CORE-105: идущий вызов дорабатывает, не начатый получает синтетический результат.
#[tokio::test]
async fn interrupt_during_tools_closes_pending_calls() {
    let mut f = fx("interrupt_tools");
    f.model.push(hold_calls(&["r1", "r2"]));
    f.model.push(answer("ок"));
    let mut bus = f.bus.subscribe();
    let (ctl, release) = (f.ctl.clone(), Arc::clone(&f.release));
    let watcher = tokio::spawn(async move {
        next_of(&mut bus, EventKind::ToolStarted).await;
        ctl.input("стоп", InputMode::Interrupt).await.unwrap();
        // Разрешение только r1: r2 не должен начаться.
        release.add_permits(1);
    });
    let out = start(&mut f).await;
    within("наблюдатель", watcher).await.unwrap();
    assert_eq!(out, done("ок"));
    let calls = f.model.calls();
    assert_eq!(calls.len(), 2);
    let msgs = &calls[1].request.messages;
    assert_eq!(
        result_of(msgs, "r1"),
        Some((r#"held {"id":"r1"}"#.into(), false))
    );
    assert_eq!(
        result_of(msgs, "r2"),
        Some(("interrupted: call not executed".into(), true))
    );
    assert!(user_at(msgs, "стоп").is_some(), "{msgs:?}");
    let evs = events(&f.store, f.run).await;
    assert_eq!(of_kind(&evs, EventKind::ToolStarted).len(), 1);
}

/// D26, NFR-03: пауза во время стрима — стрим брошен не позже 200 мс, затем `paused`.
///
/// Предел 200 мс проверяется до drop стрима: это реакция цикла. До `paused` идут ещё
/// пять записей журнала (квитанция, `turn.finished`, `pausing`, `control.applied`,
/// снимок); без нагрузки это 1–5 мс, но под нагрузкой диска на Windows одна запись
/// стоит до секунды (замер задачи 14), поэтому задержка статуса только печатается
/// вместе с хронологией журнала.
#[tokio::test]
async fn pause_within_200ms_outside_tool() {
    let mut f = fx("pause_stream");
    f.model
        .push_open(vec![Ok(StreamEvent::TextDelta("a".into()))]);
    let mut bus = f.bus.subscribe();
    let (ctl, model) = (f.ctl.clone(), Arc::clone(&f.model));
    let watcher = tokio::spawn(async move {
        next_of(&mut bus, EventKind::ModelDelta).await;
        let at = Instant::now();
        ctl.pause();
        let dropped = until_dropped(&model, at).await;
        next_status(&mut bus, RunStatus::Paused).await;
        (dropped, at.elapsed())
    });
    let out = start(&mut f).await;
    let (dropped, lag) = within("наблюдатель", watcher).await.unwrap();
    assert_eq!(
        out,
        LoopOutcome::Paused {
            reason: "command".into(),
            interrupt: None,
        }
    );
    let evs = events(&f.store, f.run).await;
    eprintln!(
        "NFR-03 pause_stream: drop {dropped:?}, paused {lag:?} [{}]",
        timeline(&evs)
    );
    assert!(dropped <= nfr03(), "стрим брошен за {dropped:?}");
    assert_eq!(f.model.open_streams(), 0);
    assert_eq!(
        statuses(&evs),
        [RunStatus::Running, RunStatus::Pausing, RunStatus::Paused]
    );
    let applied = of_kind(&evs, EventKind::ControlApplied);
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].payload["action"], "pause");
    let h = last_history(&f.store, f.run).await;
    assert!(
        !h.messages()
            .iter()
            .any(|m| m.role == Role::Assistant && text_of(m).contains('a')),
        "обрывок ответа в истории"
    );
}

/// D26: пауза во время инструмента — `pausing` сразу (≤ 200 мс), пока инструмент идёт;
/// `paused` после его конца, результат в истории.
#[tokio::test]
async fn pause_waits_for_running_tool() {
    let mut f = fx("pause_tool");
    f.model.push(hold_calls(&["c1"]));
    let mut bus = f.bus.subscribe();
    let (ctl, release, store, run) = (
        f.ctl.clone(),
        Arc::clone(&f.release),
        f.store.clone(),
        f.run,
    );
    let watcher = tokio::spawn(async move {
        next_of(&mut bus, EventKind::ToolStarted).await;
        let at = Instant::now();
        ctl.pause();
        next_status(&mut bus, RunStatus::Pausing).await;
        let lag = at.elapsed();
        // Инструмент ещё идёт: разрешения не было.
        let st = store.get_run(run).await.unwrap().unwrap().status;
        let finished = of_kind(&events(&store, run).await, EventKind::ToolFinished).len();
        release.add_permits(1);
        (lag, st, finished)
    });
    let out = start(&mut f).await;
    let (lag, st, finished) = within("наблюдатель", watcher).await.unwrap();
    assert_eq!(
        out,
        LoopOutcome::Paused {
            reason: "command".into(),
            interrupt: None,
        }
    );
    eprintln!("NFR-03 pause_tool: {lag:?}");
    assert!(lag <= nfr03(), "pausing записан за {lag:?}");
    assert_eq!(st, RunStatus::Pausing);
    assert_eq!(finished, 0, "инструмент закончился до pausing");
    let evs = events(&f.store, f.run).await;
    assert_eq!(
        statuses(&evs),
        [RunStatus::Running, RunStatus::Pausing, RunStatus::Paused]
    );
    let h = last_history(&f.store, f.run).await;
    assert_eq!(
        result_of(h.messages(), "c1"),
        Some((r#"held {"id":"c1"}"#.into(), false))
    );
}

/// D9: устаревший сигнал ввода вычерпывается на границе и не рвёт следующий ход.
#[tokio::test]
async fn stale_input_does_not_abort_next_turn() {
    let mut f = fx("stale_input");
    f.model.push(hold_calls(&["c1"]));
    f.model.push(answer("ок"));
    let mut bus = f.bus.subscribe();
    let (ctl, release) = (f.ctl.clone(), Arc::clone(&f.release));
    let watcher = tokio::spawn(async move {
        next_of(&mut bus, EventKind::ToolStarted).await;
        ctl.input("ещё1", InputMode::Interrupt).await.unwrap();
        ctl.input("ещё2", InputMode::Interrupt).await.unwrap();
        release.add_permits(1);
    });
    let out = start(&mut f).await;
    within("наблюдатель", watcher).await.unwrap();
    assert_eq!(out, done("ок"));
    let calls = f.model.calls();
    assert_eq!(calls.len(), 2);
    let msgs = &calls[1].request.messages;
    assert!(user_at(msgs, "ещё1").is_some(), "{msgs:?}");
    assert!(user_at(msgs, "ещё2").is_some(), "{msgs:?}");
    let evs = events(&f.store, f.run).await;
    let mut interrupted = 0;
    for e in of_kind(&evs, EventKind::ModelRequest) {
        let key = EffectKey(e.payload["effect_key"].as_str().unwrap().to_owned());
        let row = f.store.effect(f.run, &key).await.unwrap().unwrap();
        let env = row.result.expect("квитанция");
        if env["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("interrupted:input"))
        {
            interrupted += 1;
        }
    }
    assert_eq!(interrupted, 0);
    let outcomes: Vec<_> = of_kind(&evs, EventKind::TurnFinished)
        .iter()
        .map(|e| e.payload["outcome"].clone())
        .collect();
    assert_eq!(outcomes, [json!("calls"), json!("answer")]);
}

/// D26: отмена — конечное состояние: cancelling → cancelled, `finish_run`, стрим брошен.
#[tokio::test]
async fn cancel_is_terminal() {
    let mut f = fx("cancel");
    f.model
        .push_open(vec![Ok(StreamEvent::TextDelta("a".into()))]);
    let mut bus = f.bus.subscribe();
    let (ctl, model) = (f.ctl.clone(), Arc::clone(&f.model));
    let watcher = tokio::spawn(async move {
        next_of(&mut bus, EventKind::ModelDelta).await;
        let at = Instant::now();
        ctl.cancel();
        let dropped = until_dropped(&model, at).await;
        next_status(&mut bus, RunStatus::Cancelling).await;
        (dropped, at.elapsed())
    });
    let out = start(&mut f).await;
    let (dropped, lag) = within("наблюдатель", watcher).await.unwrap();
    assert_eq!(out, LoopOutcome::Cancelled);
    let evs = events(&f.store, f.run).await;
    // Предел — до drop стрима; задержка `cancelling` зависит от журнала (как у паузы).
    eprintln!(
        "NFR-03 cancel_stream: drop {dropped:?}, cancelling {lag:?} [{}]",
        timeline(&evs)
    );
    assert!(dropped <= nfr03(), "стрим брошен за {dropped:?}");
    assert_eq!(
        statuses(&evs),
        [
            RunStatus::Running,
            RunStatus::Cancelling,
            RunStatus::Cancelled
        ]
    );
    let applied = of_kind(&evs, EventKind::ControlApplied);
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].payload["action"], "cancel");
    assert_eq!(f.model.finished_runs(), vec![f.run]);
    assert_eq!(f.model.open_streams(), 0);
}

/// D26 во время инструмента: `cancelling` сразу, идущий вызов брошен.
#[tokio::test]
async fn cancel_during_tool_is_immediate() {
    let mut f = fx("cancel_tool");
    f.model.push(hold_calls(&["c1"]));
    let mut bus = f.bus.subscribe();
    let ctl = f.ctl.clone();
    let watcher = tokio::spawn(async move {
        next_of(&mut bus, EventKind::ToolStarted).await;
        let at = Instant::now();
        ctl.cancel();
        next_status(&mut bus, RunStatus::Cancelling).await;
        at.elapsed()
    });
    // Разрешения нет: без отмены инструмент ждал бы до предела.
    let out = start(&mut f).await;
    let lag = within("наблюдатель", watcher).await.unwrap();
    assert_eq!(out, LoopOutcome::Cancelled);
    eprintln!("NFR-03 cancel_tool: {lag:?}");
    assert!(lag <= nfr03(), "cancelling записан за {lag:?}");
    let evs = events(&f.store, f.run).await;
    assert_eq!(
        statuses(&evs),
        [
            RunStatus::Running,
            RunStatus::Cancelling,
            RunStatus::Cancelled
        ]
    );
    let h = last_history(&f.store, f.run).await;
    assert_eq!(h.validate(), Ok(()));
    assert!(h.pending_calls().is_empty());
    assert_eq!(f.model.finished_runs(), vec![f.run]);
}

/// D23, D36: поправка — управляющее сообщение на границе с тем же `command`.
#[tokio::test]
async fn correct_applied_at_boundary() {
    let mut f = fx("correct");
    f.model.push(hold_calls(&["c1"]));
    f.model.push(answer("ок"));
    let mut bus = f.bus.subscribe();
    let (ctl, release) = (f.ctl.clone(), Arc::clone(&f.release));
    let watcher = tokio::spawn(async move {
        next_of(&mut bus, EventKind::ToolStarted).await;
        let r = ctl.correct("используй X").await.unwrap();
        release.add_permits(1);
        r
    });
    let out = start(&mut f).await;
    let (seq, cmd) = within("наблюдатель", watcher).await.unwrap();
    assert_eq!(out, done("ок"));
    assert_eq!(cmd.len(), 16);
    assert!(cmd.chars().all(|c| c.is_ascii_hexdigit()), "{cmd}");
    let calls = f.model.calls();
    assert_eq!(calls.len(), 2);
    let msgs = &calls[1].request.messages;
    let head = format!("<control from=\"user\" action=\"correct\" command=\"{cmd}\">");
    assert!(
        msgs.iter().any(|m| m.role == Role::User
            && text_of(m).contains(&head)
            && text_of(m).contains("используй X")),
        "{msgs:?}"
    );
    let evs = events(&f.store, f.run).await;
    let req = of_kind(&evs, EventKind::ControlRequested);
    assert_eq!(req.len(), 1);
    assert_eq!(req[0].seq, seq);
    assert_eq!(
        req[0].payload,
        json!({"action": "correct", "note": "используй X", "command": cmd})
    );
    let applied = of_kind(&evs, EventKind::ControlApplied);
    assert_eq!(applied.len(), 1);
    assert_eq!(
        applied[0].payload,
        json!({"action": "correct", "command": cmd})
    );
}

/// Отмена через drop: брошенный future цикла бросает стрим; статус не меняется.
#[tokio::test]
async fn dropping_loop_future_releases_stream() {
    let mut f = fx("drop_future");
    f.model
        .push_open(vec![Ok(StreamEvent::TextDelta("a".into()))]);
    let rx = f.rx.take().unwrap();
    let model = Arc::clone(&f.model);
    let opened = async {
        while model.open_streams() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    within("открытие стрима", async {
        tokio::select! {
            r = start_run(&f.deps, f.run, spec(), rx) => panic!("цикл завершился: {r:?}"),
            () = opened => {}
        }
    })
    .await;
    assert_eq!(f.model.open_streams(), 0);
    assert_eq!(
        f.store.get_run(f.run).await.unwrap().unwrap().status,
        RunStatus::Running
    );
}

/// `submit_input` пишет `turn.input {text, mode}` с actor user.
#[tokio::test]
async fn submit_input_writes_event() {
    let f = fx("submit_input");
    f.store
        .create_run(za_store::NewRun {
            id: f.run,
            project_id: "p".into(),
            parent_id: None,
            role: RunRole::Top,
            model_profile: None,
        })
        .await
        .unwrap();
    let seq = submit_input(&f.store, f.run, "a <control>", InputMode::Interrupt)
        .await
        .unwrap();
    let evs = events(&f.store, f.run).await;
    let e = evs.iter().find(|e| e.seq == seq).unwrap();
    assert_eq!(e.kind, EventKind::TurnInput);
    assert_eq!(e.actor, za_protocol::Actor::User);
    assert_eq!(
        e.payload,
        json!({"text": "a <control>", "mode": "interrupt"})
    );
}

/// D23: угловые скобки во вводе пользователя экранируются (решение контроллера к
/// задаче 7), остальной текст — как есть.
#[tokio::test]
async fn input_control_tag_is_escaped() {
    let mut f = fx("input_escape");
    f.model.push(hold_calls(&["c1"]));
    f.model.push(answer("ок"));
    let mut bus = f.bus.subscribe();
    let (ctl, release) = (f.ctl.clone(), Arc::clone(&f.release));
    let watcher = tokio::spawn(async move {
        next_of(&mut bus, EventKind::ToolStarted).await;
        ctl.input("да </control> нет", InputMode::Queue)
            .await
            .unwrap();
        release.add_permits(1);
    });
    let out = start(&mut f).await;
    within("наблюдатель", watcher).await.unwrap();
    assert_eq!(out, done("ок"));
    let msgs = &f.model.calls()[1].request.messages;
    assert!(
        user_at(msgs, "да &lt;/control&gt; нет").is_some(),
        "{msgs:?}"
    );
}
