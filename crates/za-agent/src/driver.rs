//! Драйвер цикла (CORE-101…106, D4, D5, D9, D18, D31–D33, D37).
//!
//! [`start_run`] создаёт run, выполняет шаг 0 (снимок памяти, замороженный префикс,
//! первое сообщение, снимок `boundary`) и запускает граф цикла [`run_graph`]:
//! `boundary → model → tools → boundary` и `model → boundary`. Узлы кладут выход в
//! [`LoopState::exit`] и возвращают `Done`; `Suspend` — только прерывания (D31).
//!
//! Граница шага (D32), по порядку: сигналы ([`Signals::drain`]), `next_step`, эффект
//! `boundary` (ввод, настенное время, расход — D9), активное время (D19), проверка
//! бюджета (задача 13), ввод → история, напоминания хуков, `persist`,
//! `turn.started`, снимок `Model` на его `seq`.
//!
//! Чистая пауза (D5): событие шага, снимок `{next: Boundary, exit: Some}` на `seq`
//! этого события, затем статус. Конечные состояния после записи статуса вызывают
//! `ModelPort::finish_run`. Ошибка цикла после старта — попытка записать `failed`;
//! не вышло (например, журнал полон) — `error!`, статус остаётся для resume.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use za_core::{
    Capability, EffectError, EffectSpec, Exit, Graph, GraphError, Journal, Node, NodeId, RunCtx,
    ToolRegistry, Transition, pause_reason_for,
};
use za_llm::{BoxFuture, EndpointId, FinishReason, ModelCall, ModelPort, RequestMeta, ToolChoice};
use za_protocol::{
    Actor, Budget, BudgetKind, EffectClass, EventKind, NewEvent, Priority, RunId, RunMode, RunRole,
    RunStatus, Seq, Tier, canonical_json, pause_reason,
};
use za_store::{NewRun, ProjectStore, StoreError, UsageTotals};

use crate::control::ControlRx;
use crate::conversation::{Conversation, HistoryError};
use crate::exit::{LoopExit, exit_for_effect_error, exit_for_model_error};
use crate::memory::{MemoryLevel, MemoryPort, memory_budget};
use crate::prompt::{ControlAction, ControlFrom, EnvInfo, WorkMode, reasoning_for, render_control};
use crate::request::{
    FrozenPrefix, PrefixError, RunSetup, TurnOptions, build_request, first_message, freeze,
};
use crate::state::{Counters, LoopState, Next, PendingExit};
use crate::tools::{FINISH_TOOL, FinishArgs, ToolEnv, run_tools};
use crate::turn::{
    CallVerdict, RequestRefs, Reviewed, Signal, TurnError, TurnOutput, model_turn,
    report_parse_errors, review,
};

/// Точки «сбоя» для тестов и --crash-at (D33).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// эффект model шага вернулся (квитанция записана), до turn.finished и снимка
    AfterModelEffect { step: u64 },
    /// снимок чистой паузы записан, статус ещё нет
    AfterExitSnapshot { step: u64 },
}

/// true — драйвер немедленно возвращает Err(LoopError::Halted) без единой записи.
pub type CrashHook = Arc<dyn Fn(CrashPoint) -> bool + Send + Sync>;

/// Зависимости цикла.
#[derive(Clone)]
pub struct LoopDeps {
    pub journal: Journal,
    pub model: Arc<dyn ModelPort>,
    /// Инструменты run, включая встроенные (`builtin_tools`), если они нужны роли.
    pub tools: Arc<ToolRegistry>,
    /// Capability run: инструменты (дополняют `tools`), инструкции, хуки границы.
    pub capabilities: Vec<Capability>,
    pub memory: Arc<dyn MemoryPort>,
    pub crash: Option<CrashHook>,
}

/// Параметры нового run.
#[derive(Clone, Debug)]
pub struct RunSpec {
    pub project_id: String,
    pub project_dir: PathBuf,
    pub role: RunRole,
    pub parent: Option<RunId>,
    pub endpoint: EndpointId,
    pub run_mode: RunMode,
    pub work_mode: Option<WorkMode>,
    pub tier: Option<Tier>,
    pub budget: Budget,
    pub priority: Priority,
    pub max_wait: Option<Duration>,
    pub task: String,
    pub agents_md: Option<String>,
    /// os, shell, project_dir, date, branch задаёт вызывающий; tier, n_ctx, budget перезаписывает start_run (D37).
    pub env: EnvInfo,
}

/// Итог цикла.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopOutcome {
    Finished {
        status: RunStatus,
        answer: Option<String>,
        finish: Option<FinishArgs>,
    },
    Paused {
        reason: String,
        interrupt: Option<String>,
    },
    BlockedOnBudget {
        kind: BudgetKind,
    },
    Cancelled,
}

/// Ошибка цикла: журнал, нарушенный инвариант, конфигурация run (D11).
#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Effect(#[from] EffectError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error("снимок run: {0}")]
    Snapshot(String),
    #[error("граф цикла: {0}")]
    Graph(String),
    #[error("run в статусе {0:?} продолжить нельзя")]
    NotResumable(RunStatus),
    #[error("эндпоинт {0} неизвестен")]
    UnknownEndpoint(EndpointId),
    #[error("run прервался посреди шага: смена модели — после его завершения")]
    ChangeMidStep,
    /// Префикс run не собирается (повтор имени инструмента): ошибка конфигурации run.
    #[error("префикс run: {0}")]
    Prefix(#[from] PrefixError),
    /// Запрос хода не сериализуется для хэша (D7).
    #[error("запрос хода не сериализуется: {0}")]
    Request(#[source] serde_json::Error),
    #[error("цикл остановлен точкой сбоя (тест)")]
    Halted,
}

impl LoopError {
    /// Причина в `run.status` при переходе в `failed`.
    fn code(&self) -> &'static str {
        match self {
            LoopError::Store(_) => "store",
            LoopError::Effect(EffectError::Store(_)) => "store",
            LoopError::Effect(EffectError::Poisoned) => "poisoned",
            LoopError::Effect(_) => "effect",
            LoopError::History(_) => "history",
            LoopError::Snapshot(_) => "snapshot",
            LoopError::Graph(_) => "graph",
            LoopError::NotResumable(_) => "not_resumable",
            LoopError::UnknownEndpoint(_) => "unknown_endpoint",
            LoopError::ChangeMidStep => "change_mid_step",
            LoopError::Prefix(_) => "prefix",
            LoopError::Request(_) => "request",
            LoopError::Halted => "halted",
        }
    }
}

/// Ошибка эффекта в ошибку цикла: ошибка журнала — `LoopError::Store` (D11).
fn effect_err(e: EffectError) -> LoopError {
    match e {
        EffectError::Store(s) => LoopError::Store(s),
        e => LoopError::Effect(e),
    }
}

// ---- Имена и тексты ----

const BOUNDARY: NodeId = "boundary";
const MODEL: NodeId = "model";
const TOOLS: NodeId = "tools";

/// Имя эффекта границы шага (D9).
const BOUNDARY_EFFECT: &str = "boundary";
/// Имя эффекта снимка памяти шага 0 (D24).
const MEMORY_EFFECT: &str = "memory.snapshot";
/// Префикс квитанции-отказа эффекта, у которого внутри не прочитался журнал.
const STORE_ERROR: &str = "store_error:";
/// `n_ctx`, если эндпоинт не сообщает размер контекста (D37).
const DEFAULT_N_CTX: u32 = 8192;
/// Пустых ходов подряд до `blocked` (D18).
const EMPTY_TURNS_LIMIT: u32 = 3;
/// Страница чтения ввода на границе.
const INBOX_PAGE: usize = 512;

/// Nudge пустого хода top (D18, D23).
const EMPTY_NUDGE_TOP: &str = "Your last response had no text and no tool calls. Continue the task: answer the user or call a tool.";
/// Nudge пустого хода worker и orchestrator (D18, D23).
const EMPTY_NUDGE_SUBAGENT: &str = "Your last response made no tool calls. Continue the task with tool calls; when the work is done, partly done or blocked, call finish.";

// ---- Шаг 0 ----

/// Создаёт run (role, parent, model_profile = endpoint), queued → running. Шаг 0 (без next_step):
/// n_ctx = model.context_tokens(&endpoint).unwrap_or(8192) с warn (D37), tier = spec.tier или
/// Tier::for_context(n_ctx); эффект memory.snapshot (ReadOnly, ключ {run}:0:0); при L0 — memory.unavailable;
/// freeze, блоб FrozenPrefix (put_blob), первое сообщение, persist; снимок шага 0 {step: 0, next: Boundary,
/// exit: None} на seq последнего события (D4); затем run_graph с узла boundary.
///
/// Неизвестный эндпоинт проверяется до создания run: run не создаётся.
pub async fn start_run(
    deps: &LoopDeps,
    run: RunId,
    spec: RunSpec,
    ctl: ControlRx,
) -> Result<LoopOutcome, LoopError> {
    if deps.model.capabilities(&spec.endpoint).is_none() {
        return Err(LoopError::UnknownEndpoint(spec.endpoint));
    }
    let store = deps.journal.store();
    store
        .create_run(NewRun {
            id: run,
            project_id: spec.project_id.clone(),
            parent_id: spec.parent,
            role: spec.role,
            model_profile: Some(spec.endpoint.0.clone()),
        })
        .await?;
    let generation = deps
        .journal
        .set_status(run, RunStatus::Running, 0, None)
        .await?;
    let mut st = match step_zero(deps, run, spec, ctl, generation).await {
        Ok(st) => st,
        Err(e) => return Err(fail(deps, run, generation, e).await),
    };
    run_graph(deps, &mut st, BOUNDARY).await
}

async fn step_zero(
    deps: &LoopDeps,
    run: RunId,
    spec: RunSpec,
    ctl: ControlRx,
    generation: u64,
) -> Result<LoopState, LoopError> {
    let store = deps.journal.store().clone();
    let n_ctx = match deps.model.context_tokens(&spec.endpoint) {
        Some(n) => n,
        None => {
            tracing::warn!(endpoint = %spec.endpoint.0, n_ctx = DEFAULT_N_CTX,
                "размер контекста эндпоинта неизвестен, принят по умолчанию");
            DEFAULT_N_CTX
        }
    };
    let tier = spec.tier.unwrap_or_else(|| Tier::for_context(n_ctx));
    let setup = RunSetup {
        role: spec.role,
        tier,
        work_mode: spec.work_mode,
        run_mode: spec.run_mode,
        endpoint: spec.endpoint.clone(),
        n_ctx,
        reasoning: reasoning_for(tier),
        priority: spec.priority,
        max_wait: spec.max_wait,
        project_id: spec.project_id.clone(),
        project_dir: spec.project_dir.clone(),
    };
    let mut ctx = RunCtx::new(store.clone(), run);
    let mut last_seq = None;

    // Снимок памяти — эффект: внешнее значение входит в цикл только так (D9, D24).
    let budget_tokens = memory_budget(n_ctx);
    let memory = Arc::clone(&deps.memory);
    let v = ctx
        .effect(
            EffectSpec {
                class: EffectClass::ReadOnly,
                name: MEMORY_EFFECT.into(),
                args: json!({ "budget_tokens": budget_tokens }),
            },
            move || async move {
                let recall = memory.snapshot(run, budget_tokens).await;
                Ok(json!({ "text": recall.text() }))
            },
        )
        .await
        .map_err(effect_err)?;
    let memory_text = v.get("text").and_then(Value::as_str).map(str::to_owned);
    if deps.memory.level() == MemoryLevel::L0 {
        last_seq = Some(
            append(
                &deps.journal,
                run,
                Actor::Runtime,
                EventKind::MemoryUnavailable,
                json!({}),
            )
            .await?,
        );
    }

    let registry = run_registry(deps)?;
    let (tools, classes) = run_tools_of(&registry, spec.role, tier);
    let frozen = freeze(
        &setup,
        &tools,
        &deps.capabilities,
        spec.agents_md.as_deref(),
    )?;
    let frozen_ref = put_json(&store, &frozen).await?;

    let mut env = spec.env.clone();
    env.tier = tier;
    env.n_ctx = n_ctx;
    env.budget = budget_text(&spec.budget);
    let mut conv = Conversation::default();
    conv.push_user(first_message(&env, memory_text.as_deref(), &spec.task))?;
    let refs = conv.persist(&store).await?.refs;

    let last_seq = match last_seq {
        Some(s) => s,
        // Событий драйвера на шаге 0 нет (память не L0): снимок — на последнем
        // событии run. Шаг 0 не реплеится, поэтому это чтение — вход цикла, а не его ход.
        None => last_event_seq(&store, run).await?,
    };
    let st = LoopState {
        run,
        ctx,
        conv,
        refs,
        tools: frozen.tools.clone(),
        frozen,
        frozen_ref,
        setup,
        budget: spec.budget,
        counters: Counters::default(),
        turn: 0,
        inbox_after: last_seq.0,
        last_seq,
        generation,
        classes,
        reviewed: None,
        sig: ctl.sig,
        answer: None,
        finish: None,
        exit: None,
    };
    save_snapshot(&store, &st, Next::Boundary, None).await?;
    Ok(st)
}

/// Реестр run: `deps.tools` и инструменты capability. Повтор имени — ошибка префикса.
fn run_registry(deps: &LoopDeps) -> Result<Arc<ToolRegistry>, LoopError> {
    if deps.capabilities.iter().all(|c| c.tools.is_empty()) {
        return Ok(Arc::clone(&deps.tools));
    }
    let mut reg = (*deps.tools).clone();
    for c in &deps.capabilities {
        for t in &c.tools {
            reg.register(Arc::clone(t))
                .map_err(|e| PrefixError::DuplicateTool(e.0))?;
        }
    }
    Ok(Arc::new(reg))
}

/// Описания и классы инструментов роли и тира (CTX-003: от режима не зависят).
fn run_tools_of(
    reg: &ToolRegistry,
    role: RunRole,
    tier: Tier,
) -> (Vec<za_llm::ToolSchema>, BTreeMap<String, EffectClass>) {
    let tools = reg.for_run(role, tier);
    let schemas = tools.iter().map(|t| t.spec().schema.clone()).collect();
    let classes = tools
        .iter()
        .map(|t| (t.spec().schema.name.clone(), t.spec().class))
        .collect();
    (schemas, classes)
}

/// Бюджет для `<env>`: `steps 50, tokens 1000`; без пределов — `-`.
fn budget_text(b: &Budget) -> String {
    let mut parts = Vec::new();
    if let Some(n) = b.steps {
        parts.push(format!("steps {n}"));
    }
    if let Some(n) = b.tokens {
        parts.push(format!("tokens {n}"));
    }
    if let Some(n) = b.gpu_ms {
        parts.push(format!("gpu_ms {n}"));
    }
    if let Some(n) = b.usd {
        parts.push(format!("usd {n}"));
    }
    if let Some(n) = b.duration_ms {
        parts.push(format!("duration_ms {n}"));
    }
    if parts.is_empty() {
        "-".into()
    } else {
        parts.join(", ")
    }
}

// ---- Граф ----

/// Общее для узлов графа.
struct Shared {
    deps: LoopDeps,
    registry: Arc<ToolRegistry>,
    parallel: bool,
}

/// Узел цикла: один тип на три id.
struct LoopNode {
    id: NodeId,
    sh: Arc<Shared>,
}

impl Node<LoopState, LoopError> for LoopNode {
    fn id(&self) -> NodeId {
        self.id
    }

    fn run<'a>(&'a self, st: &'a mut LoopState) -> BoxFuture<'a, Result<Transition, LoopError>> {
        Box::pin(async move {
            match self.id {
                BOUNDARY => boundary(&self.sh, st).await,
                MODEL => model_node(&self.sh, st).await,
                _ => tools_node(&self.sh, st).await,
            }
        })
    }
}

/// Граф boundary → model → tools → boundary (+ model → boundary); выход — LoopState.exit (D31); применяет
/// выход: статус, finish_run на конечных. Общий для start_run и resume_run (задача 15).
pub(crate) async fn run_graph(
    deps: &LoopDeps,
    st: &mut LoopState,
    at: NodeId,
) -> Result<LoopOutcome, LoopError> {
    match drive(deps, st, at).await {
        Ok(o) => Ok(o),
        Err(e) => Err(fail(deps, st.run, st.generation, e).await),
    }
}

async fn drive(deps: &LoopDeps, st: &mut LoopState, at: NodeId) -> Result<LoopOutcome, LoopError> {
    // Исключение D30: `parallel_tools` влияет только на одновременность чтений.
    let parallel = deps
        .model
        .capabilities(&st.setup.endpoint)
        .ok_or_else(|| LoopError::UnknownEndpoint(st.setup.endpoint.clone()))?
        .parallel_tools;
    let sh = Arc::new(Shared {
        deps: deps.clone(),
        registry: run_registry(deps)?,
        parallel,
    });
    let node = |id| -> Box<dyn Node<LoopState, LoopError>> {
        Box::new(LoopNode {
            id,
            sh: Arc::clone(&sh),
        })
    };
    let graph = Graph::new(BOUNDARY)
        .node(node(BOUNDARY))
        .node(node(MODEL))
        .node(node(TOOLS))
        .edge(BOUNDARY, MODEL)
        .edge(MODEL, TOOLS)
        .edge(MODEL, BOUNDARY)
        .edge(TOOLS, BOUNDARY);
    st.exit = None;
    let exit = match graph.run_from(at, st).await {
        Ok(Exit::Done) => st
            .exit
            .take()
            .ok_or_else(|| LoopError::Graph("узел завершил граф без выхода цикла".into()))?,
        Ok(Exit::Suspended(i)) => LoopExit::Suspended {
            reason: pause_reason_for(i.kind),
            interrupt: i.id,
        },
        Err(GraphError::Node(e)) => return Err(e),
        Err(e) => return Err(LoopError::Graph(e.to_string())),
    };
    apply_exit(deps, st, exit).await
}

/// Записывает статус выхода; на конечных — `finish_run` (Carry R1-A progress:183).
async fn apply_exit(
    deps: &LoopDeps,
    st: &mut LoopState,
    exit: LoopExit,
) -> Result<LoopOutcome, LoopError> {
    let j = &deps.journal;
    let run = st.run;
    let outcome = match exit {
        LoopExit::Finished(status) => {
            st.generation = j.set_status(run, status, st.generation, None).await?;
            deps.model.finish_run(run);
            LoopOutcome::Finished {
                status,
                answer: st.answer.clone(),
                finish: st.finish.clone(),
            }
        }
        LoopExit::Paused(reason) => {
            st.generation = j
                .set_status(run, RunStatus::Paused, st.generation, Some(reason))
                .await?;
            LoopOutcome::Paused {
                reason: reason.into(),
                interrupt: None,
            }
        }
        LoopExit::Suspended { reason, interrupt } => {
            st.generation = j
                .set_status(run, RunStatus::Paused, st.generation, Some(reason))
                .await?;
            LoopOutcome::Paused {
                reason: reason.into(),
                interrupt: Some(interrupt),
            }
        }
        LoopExit::BlockedOnBudget(kind) => {
            st.generation = j
                .set_status(run, RunStatus::BlockedOnBudget, st.generation, None)
                .await?;
            LoopOutcome::BlockedOnBudget { kind }
        }
        LoopExit::Cancelled => {
            st.generation = j
                .set_status(run, RunStatus::Cancelling, st.generation, None)
                .await?;
            st.generation = j
                .set_status(run, RunStatus::Cancelled, st.generation, None)
                .await?;
            deps.model.finish_run(run);
            LoopOutcome::Cancelled
        }
    };
    tracing::info!(run = %run, outcome = ?outcome, "цикл run остановлен");
    Ok(outcome)
}

/// Ошибка цикла после старта (решение R0): попытка `failed` с причиной и `finish_run`;
/// не вышло — `error!`, статус остаётся для resume. `Halted` ничего не пишет (D33).
async fn fail(deps: &LoopDeps, run: RunId, generation: u64, e: LoopError) -> LoopError {
    if matches!(e, LoopError::Halted) {
        return e;
    }
    let reason = e.code();
    match deps
        .journal
        .set_status(run, RunStatus::Failed, generation, Some(reason))
        .await
    {
        Ok(_) => {
            tracing::error!(run = %run, error = %e, reason, "run завершён с ошибкой цикла");
            deps.model.finish_run(run);
        }
        Err(se) => tracing::error!(run = %run, error = %e, status_error = %se,
            "ошибка цикла, статус failed не записан: run остаётся для resume"),
    }
    e
}

// ---- Граница шага ----

/// Ввод, прочитанный эффектом `boundary`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Inbox {
    /// `turn.input {text, mode}`.
    Input { seq: u64, text: String },
    /// `control.requested {action: "correct", note, command}`.
    Correct {
        seq: u64,
        note: String,
        command: String,
    },
}

/// Значение квитанции эффекта `boundary` (D9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct BoundaryValue {
    inputs: Vec<Inbox>,
    /// Последний просмотренный `seq` журнала run.
    inbox_after: u64,
    now_ms: u64,
    usage: UsageTotals,
}

/// Чтение входа границы внутри эффекта: ввод и поправки после `after`, время, расход.
async fn read_boundary(
    store: &ProjectStore,
    run: RunId,
    after: u64,
) -> Result<BoundaryValue, StoreError> {
    let mut cursor = after;
    let mut inputs = Vec::new();
    loop {
        let page = store.read_run(run, Seq(cursor), INBOX_PAGE).await?;
        let n = page.len();
        for e in page {
            cursor = e.seq.0;
            let text = |k: &str| {
                e.payload
                    .get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            match e.kind {
                EventKind::TurnInput => inputs.push(Inbox::Input {
                    seq: e.seq.0,
                    text: text("text"),
                }),
                EventKind::ControlRequested
                    if e.payload.get("action").and_then(Value::as_str) == Some("correct") =>
                {
                    inputs.push(Inbox::Correct {
                        seq: e.seq.0,
                        note: text("note"),
                        command: text("command"),
                    });
                }
                _ => {}
            }
        }
        if n < INBOX_PAGE {
            break;
        }
    }
    let now_ms = u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap_or(0);
    let usage = store.usage_totals(run).await?;
    Ok(BoundaryValue {
        inputs,
        inbox_after: cursor,
        now_ms,
        usage,
    })
}

/// Узел `boundary` (D32).
async fn boundary(sh: &Shared, st: &mut LoopState) -> Result<Transition, LoopError> {
    let deps = &sh.deps;
    // (1) Сигналы до шага (D9): Input отброшен — ввод прочитает сама граница.
    match st.sig.drain() {
        Some(Signal::Pause) => return command_pause(sh, st).await,
        Some(Signal::Cancel) => {
            st.exit = Some(LoopExit::Cancelled);
            return Ok(Transition::Done);
        }
        _ => {}
    }
    // (2), (3) Новый шаг и его эффект `boundary`.
    st.ctx.next_step();
    let step = st.ctx.step();
    let store = deps.journal.store().clone();
    let (run, after) = (st.run, st.inbox_after);
    let mut failure: Option<StoreError> = None;
    let fref = &mut failure;
    let r = st
        .ctx
        .effect(
            EffectSpec {
                class: EffectClass::ReadOnly,
                name: BOUNDARY_EFFECT.into(),
                args: json!({ "step": step }),
            },
            move || async move {
                match read_boundary(&store, run, after).await {
                    Ok(v) => serde_json::to_value(&v)
                        .map_err(|e| format!("{STORE_ERROR} граница не сериализуется: {e}")),
                    Err(e) => {
                        let msg = format!("{STORE_ERROR} {e}");
                        *fref = Some(e);
                        Err(msg)
                    }
                }
            },
        )
        .await;
    if let Some(e) = failure {
        st.ctx.observe::<()>(&Err(EffectError::Store(e.clone())));
        return Err(LoopError::Store(e));
    }
    let v = r.map_err(effect_err)?;
    let b: BoundaryValue = serde_json::from_value(v).map_err(|e| {
        let e = StoreError::Corrupt(format!("квитанция границы шага {step}: {e}"));
        st.ctx.observe::<()>(&Err(EffectError::Store(e.clone())));
        LoopError::Store(e)
    })?;
    // (4) Активное время (D19).
    if let Some(last) = st.counters.last_boundary_ms {
        st.counters.active_ms += b.now_ms.saturating_sub(last);
    }
    st.counters.last_boundary_ms = Some(b.now_ms);
    // (5) Проверка бюджета — задача 13 (до приёма ввода, D32).
    // (6) Ввод в историю.
    for i in &b.inputs {
        match i {
            Inbox::Input { seq, text } => {
                if text.is_empty() {
                    tracing::warn!(seq, "пустой ввод пользователя пропущен");
                    continue;
                }
                st.conv.push_user(text.clone())?;
            }
            Inbox::Correct { note, command, .. } => {
                st.conv.push_user(render_control(
                    ControlFrom::User,
                    ControlAction::Correct,
                    command,
                    note,
                ))?;
            }
        }
    }
    st.inbox_after = b.inbox_after;
    // (7) Напоминания хуков: чистые функции позиции, реплей зовёт их снова.
    let info = za_core::BoundaryInfo {
        run: st.run,
        step,
        turn: st.turn + 1,
    };
    for cap in &deps.capabilities {
        for h in &cap.hooks {
            if let Some(text) = h.at_boundary(&info) {
                st.conv.push_user(render_control(
                    ControlFrom::Runtime,
                    ControlAction::Reminder,
                    "",
                    &text,
                ))?;
            }
        }
    }
    // (8)–(10) persist, turn.started, снимок `Model` на его seq.
    persist(&store_of(deps), st).await?;
    st.turn += 1;
    st.counters.steps += 1;
    st.last_seq = append(
        &deps.journal,
        st.run,
        Actor::Runtime,
        EventKind::TurnStarted,
        json!({ "turn": st.turn, "step": step }),
    )
    .await?;
    save_snapshot(&store_of(deps), st, Next::Model, None).await?;
    Ok(Transition::Goto(MODEL))
}

// ---- Узел model ----

async fn model_node(sh: &Shared, st: &mut LoopState) -> Result<Transition, LoopError> {
    let deps = &sh.deps;
    let opts = TurnOptions {
        tool_choice: if st.counters.forced_required {
            ToolChoice::Required
        } else {
            ToolChoice::Auto
        },
        ..TurnOptions::default()
    };
    let call = ModelCall {
        endpoint: st.setup.endpoint.clone(),
        meta: RequestMeta {
            run_id: st.run,
            priority: st.setup.priority,
            session: Some(st.run.to_string()),
            max_wait: st.setup.max_wait,
        },
        request: build_request(&st.frozen, &st.setup, &st.conv, &opts),
    };
    let refs = RequestRefs {
        frozen: st.frozen_ref.clone(),
        messages: st.refs.clone(),
        prefix: st.frozen.prefix_hash.clone(),
    };
    let r = model_turn(
        &mut st.ctx,
        &deps.journal,
        deps.model.as_ref(),
        call.clone(),
        &refs,
        st.turn,
        &mut st.sig,
    )
    .await;
    // Квитанция эффекта записана (успех, ошибка модели или обрыв) — точка сбоя D33.
    if matches!(
        r,
        Ok(_) | Err(TurnError::Model { .. } | TurnError::Aborted { .. })
    ) && crashed(
        deps,
        CrashPoint::AfterModelEffect {
            step: st.ctx.step(),
        },
    ) {
        return Err(LoopError::Halted);
    }
    match r {
        Ok(out) => model_output(sh, st, out, &call).await,
        Err(TurnError::Model { error, last_wait }) => {
            tracing::warn!(run = %st.run, turn = st.turn, error = %error, "ход модели завершился ошибкой");
            turn_finished(deps, st, "error", None).await?;
            clean_pause(deps, st, exit_for_model_error(&error, last_wait.as_ref())).await
        }
        Err(TurnError::Effect(e)) => {
            let exit = effect_exit(e)?;
            turn_finished(deps, st, "error", None).await?;
            st.exit = Some(exit);
            Ok(Transition::Done)
        }
        Err(TurnError::Aborted { signal, source }) => {
            turn_finished(deps, st, "interrupted", None).await?;
            match (signal, source) {
                (Signal::Input, _) => Ok(Transition::Goto(BOUNDARY)),
                // Реплей обрыва по команде (D7): команда не применяется второй раз.
                (_, za_core::EffectSource::Journal) => {
                    st.conv.close_interrupted();
                    Ok(Transition::Goto(BOUNDARY))
                }
                (Signal::Pause, za_core::EffectSource::Live) => command_pause(sh, st).await,
                (Signal::Cancel, za_core::EffectSource::Live) => {
                    st.exit = Some(LoopExit::Cancelled);
                    Ok(Transition::Done)
                }
            }
        }
        Err(TurnError::Request(e)) => Err(LoopError::Request(e)),
    }
}

/// Ответ модели: отказ (D13), фиксация хода (D10), завершение и пустые ходы (D18).
async fn model_output(
    sh: &Shared,
    st: &mut LoopState,
    out: TurnOutput,
    call: &ModelCall,
) -> Result<Transition, LoopError> {
    let deps = &sh.deps;
    if matches!(
        out.finish,
        FinishReason::Refusal | FinishReason::ContentFilter
    ) {
        // Частичный текст — только в `model.delta`, история без него (D13).
        turn_finished(deps, st, "refusal", None).await?;
        return clean_pause(deps, st, LoopExit::Paused(pause_reason::REFUSAL)).await;
    }
    let rev = review(&out, &st.tools, &st.classes, st.turn);
    report_parse_errors(deps.model.as_ref(), st.run, call, rev.parse_errors);
    st.conv.commit_turn(out.service, rev.assistant.clone())?;

    if !rev.calls.is_empty() {
        st.counters.empty_turns = 0;
        st.counters.forced_required = false;
        let finishing = rev
            .calls
            .iter()
            .any(|c| c.call.name == FINISH_TOOL && c.verdict == CallVerdict::Run);
        let outcome = if finishing { "finish" } else { "calls" };
        turn_finished(deps, st, outcome, Some(&rev)).await?;
        st.reviewed = Some(rev);
        return Ok(Transition::Goto(TOOLS));
    }
    if st.setup.role == RunRole::Top
        && out.finish == FinishReason::Stop
        && !rev.text.trim().is_empty()
    {
        turn_finished(deps, st, "answer", Some(&rev)).await?;
        st.answer = Some(rev.text);
        return finish_with(deps, st, RunStatus::Done).await;
    }
    // Пустой ход (D18): нет вызовов и, у top, нет ответа.
    turn_finished(deps, st, "empty", Some(&rev)).await?;
    push_nudges(st, &rev.nudges)?;
    st.counters.empty_turns += 1;
    if st.counters.empty_turns >= EMPTY_TURNS_LIMIT {
        tracing::warn!(run = %st.run, "пустые ходы подряд: run blocked");
        return finish_with(deps, st, RunStatus::Blocked).await;
    }
    let nudge = if st.setup.role == RunRole::Top {
        EMPTY_NUDGE_TOP
    } else {
        EMPTY_NUDGE_SUBAGENT
    };
    push_nudges(st, &[nudge.to_owned()])?;
    if st.counters.empty_turns == EMPTY_TURNS_LIMIT - 1 {
        st.counters.forced_required = true;
    }
    Ok(Transition::Goto(BOUNDARY))
}

// ---- Узел tools ----

async fn tools_node(sh: &Shared, st: &mut LoopState) -> Result<Transition, LoopError> {
    let deps = &sh.deps;
    let rev = st
        .reviewed
        .take()
        .ok_or_else(|| LoopError::Graph("узел tools без ответа модели".into()))?;
    let env = ToolEnv {
        run: st.run,
        project_dir: st.setup.project_dir.clone(),
        mode: st.setup.run_mode,
        parallel: sh.parallel,
    };
    let out = match run_tools(
        &mut st.ctx,
        &deps.journal,
        &sh.registry,
        &env,
        &rev.calls,
        &mut st.sig,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            st.exit = Some(effect_exit(e)?);
            return Ok(Transition::Done);
        }
    };
    st.conv.push_results(out.results)?;
    // `finish` важнее обрыва пакета: run завершён (решение контроллера к задаче 11).
    if let Some(f) = out.finish {
        // Синтетические результаты — только вызовам без результата (CORE-105).
        st.conv.close_interrupted();
        let status = match f.status.as_str() {
            "done" => RunStatus::Done,
            "partial" => RunStatus::Partial,
            "blocked" => RunStatus::Blocked,
            other => {
                return Err(LoopError::Graph(format!(
                    "finish с неизвестным статусом {other}"
                )));
            }
        };
        st.answer = Some(f.result.clone());
        st.finish = Some(f);
        return finish_with(deps, st, status).await;
    }
    if out.cancelled {
        st.conv.close_interrupted();
        st.exit = Some(LoopExit::Cancelled);
        return Ok(Transition::Done);
    }
    if let Some(int) = out.suspend {
        // Снимка нет (D5): resume реплеит шаг до прерывания и читает решение.
        return Ok(Transition::Suspend(int));
    }
    if out.aborted {
        st.conv.close_interrupted();
    }
    // Nudge — отдельными сообщениями после результатов (D23).
    push_nudges(st, &rev.nudges)?;
    if out.aborted {
        // `ToolsOutcome` не различает Pause и Input: решают флаги (Carry к задаче 12/14).
        let flags = *st.sig.flags().borrow();
        if flags.cancel {
            st.exit = Some(LoopExit::Cancelled);
            return Ok(Transition::Done);
        }
        if flags.pause {
            return command_pause(sh, st).await;
        }
    }
    Ok(Transition::Goto(BOUNDARY))
}

// ---- Выходы ----

/// Ошибка эффекта в выход (D11): `ReplayMismatch`, `Unknown` — пауза без снимка
/// (resume реплеит шаг), прочие — ошибка цикла.
fn effect_exit(e: EffectError) -> Result<LoopExit, LoopError> {
    exit_for_effect_error(&e).ok_or_else(|| effect_err(e))
}

/// Чистая пауза по команде (D5, D26): `control.applied {action: "pause"}`, снимок,
/// статус `paused` (`command`). Флаг паузы снимается: следующая пауза — новый сигнал.
async fn command_pause(sh: &Shared, st: &mut LoopState) -> Result<Transition, LoopError> {
    st.last_seq = append(
        &sh.deps.journal,
        st.run,
        Actor::Runtime,
        EventKind::ControlApplied,
        json!({ "action": "pause", "command": "" }),
    )
    .await?;
    st.sig.clear_pause();
    clean_pause(&sh.deps, st, LoopExit::Paused(pause_reason::COMMAND)).await
}

/// Чистая пауза (D5): событие шага уже записано (`last_seq`); снимок
/// `{next: Boundary, exit: Some}` на его seq, точка сбоя, выход.
async fn clean_pause(
    deps: &LoopDeps,
    st: &mut LoopState,
    exit: LoopExit,
) -> Result<Transition, LoopError> {
    let pending = match &exit {
        LoopExit::Paused(reason) => PendingExit::Paused {
            reason: (*reason).to_owned(),
        },
        LoopExit::BlockedOnBudget(kind) => PendingExit::BlockedOnBudget { budget: *kind },
        other => {
            return Err(LoopError::Graph(format!("выход {other:?} не чистая пауза")));
        }
    };
    let store = store_of(deps);
    persist(&store, st).await?;
    save_snapshot(&store, st, Next::Boundary, Some(pending)).await?;
    if crashed(
        deps,
        CrashPoint::AfterExitSnapshot {
            step: st.ctx.step(),
        },
    ) {
        return Err(LoopError::Halted);
    }
    st.exit = Some(exit);
    Ok(Transition::Done)
}

/// Конечное состояние: история сохранена, снимок `{next: Boundary, exit: None}` на
/// `last_seq` (D17: история конечного снимка валидна), выход.
async fn finish_with(
    deps: &LoopDeps,
    st: &mut LoopState,
    status: RunStatus,
) -> Result<Transition, LoopError> {
    let store = store_of(deps);
    persist(&store, st).await?;
    save_snapshot(&store, st, Next::Boundary, None).await?;
    st.exit = Some(LoopExit::Finished(status));
    Ok(Transition::Done)
}

// ---- Помощники ----

fn store_of(deps: &LoopDeps) -> ProjectStore {
    deps.journal.store().clone()
}

fn crashed(deps: &LoopDeps, p: CrashPoint) -> bool {
    deps.crash.as_ref().is_some_and(|h| h(p))
}

/// Nudge рантайма — отдельными user-сообщениями (D23).
fn push_nudges(st: &mut LoopState, nudges: &[String]) -> Result<(), HistoryError> {
    for n in nudges {
        st.conv.push_user(render_control(
            ControlFrom::Runtime,
            ControlAction::Nudge,
            "",
            n,
        ))?;
    }
    Ok(())
}

/// `turn.finished {turn, outcome, calls, repairs, rejected}` (D8).
async fn turn_finished(
    deps: &LoopDeps,
    st: &mut LoopState,
    outcome: &str,
    rev: Option<&Reviewed>,
) -> Result<(), StoreError> {
    let (calls, repairs, rejected) =
        rev.map_or((0, 0, 0), |r| (r.calls.len(), r.repairs, r.rejected));
    st.last_seq = append(
        &deps.journal,
        st.run,
        Actor::Runtime,
        EventKind::TurnFinished,
        json!({
            "turn": st.turn,
            "outcome": outcome,
            "calls": calls,
            "repairs": repairs,
            "rejected": rejected,
        }),
    )
    .await?;
    Ok(())
}

async fn append(
    j: &Journal,
    run: RunId,
    actor: Actor,
    kind: EventKind,
    payload: Value,
) -> Result<Seq, StoreError> {
    j.append(NewEvent {
        run_id: run,
        parent_seq: None,
        actor,
        kind,
        payload,
        tainted: false,
    })
    .await
}

async fn persist(store: &ProjectStore, st: &mut LoopState) -> Result<(), StoreError> {
    st.refs = st.conv.persist(store).await?.refs;
    Ok(())
}

async fn save_snapshot(
    store: &ProjectStore,
    st: &LoopState,
    next: Next,
    exit: Option<PendingExit>,
) -> Result<(), LoopError> {
    let snap = st.snapshot(next, exit);
    let v = serde_json::to_value(&snap).map_err(|e| LoopError::Snapshot(e.to_string()))?;
    store.save_snapshot(st.run, st.last_seq, v).await?;
    Ok(())
}

/// Блоб с каноническим JSON значения.
async fn put_json(
    store: &ProjectStore,
    v: &FrozenPrefix,
) -> Result<za_protocol::BlobHash, LoopError> {
    let v = serde_json::to_value(v).map_err(|e| LoopError::Snapshot(e.to_string()))?;
    Ok(store.put_blob(canonical_json(&v).as_bytes()).await?)
}

/// `seq` последнего события run.
async fn last_event_seq(store: &ProjectStore, run: RunId) -> Result<Seq, StoreError> {
    let mut last = Seq(0);
    loop {
        let page = store.read_run(run, last, INBOX_PAGE).await?;
        let n = page.len();
        if let Some(e) = page.last() {
            last = e.seq;
        }
        if n < INBOX_PAGE {
            return Ok(last);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_text_lists_limits() {
        assert_eq!(budget_text(&Budget::default()), "-");
        let b = Budget {
            steps: Some(50),
            tokens: Some(1000),
            ..Budget::default()
        };
        assert_eq!(budget_text(&b), "steps 50, tokens 1000");
    }

    #[test]
    fn boundary_value_round_trips() {
        let v = BoundaryValue {
            inputs: vec![
                Inbox::Input {
                    seq: 3,
                    text: "a".into(),
                },
                Inbox::Correct {
                    seq: 4,
                    note: "n".into(),
                    command: "c".into(),
                },
            ],
            inbox_after: 9,
            now_ms: 1,
            usage: UsageTotals::default(),
        };
        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(j["inputs"][0]["kind"], "input");
        assert_eq!(serde_json::from_value::<BoundaryValue>(j).unwrap(), v);
    }
}
