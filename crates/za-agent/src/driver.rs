//! Драйвер цикла (CORE-101…106, D4, D5, D9, D18, D31–D33, D37).
//!
//! [`start_run`] создаёт run, выполняет шаг 0 (снимок памяти, замороженный префикс,
//! первое сообщение, снимок `boundary`) и запускает граф цикла [`run_graph`]:
//! `boundary → model → tools → boundary` и `model → boundary`. Узлы кладут выход в
//! [`LoopState::exit`] и возвращают `Done`; `Suspend` — только прерывания (D31).
//!
//! Граница шага (D32), по порядку: сигналы ([`Signals::drain`]), `next_step`, эффект
//! `boundary` (ввод, настенное время, расход — D9), активное время (D19), проверка
//! бюджета (D19, [`check_budget`]), ввод → история (и `user_inputs`, R1-B2 D66),
//! напоминания хуков, эффект `context` — оценка запроса хода (R1-B2 D42), `persist`,
//! `turn.started`, снимок `Model` на его `seq`. Ход получает лимит вывода из
//! [`crate::output_plan`] (R1-B2 D40); после хода — проверка тихой обрезки (R1-B2 D43).
//!
//! Чистая пауза (D5): событие шага, снимок `{next: Boundary, exit: Some}` на `seq`
//! этого события, затем статус. Конечные состояния после записи статуса вызывают
//! `ModelPort::finish_run`. Ошибка цикла после старта — попытка записать `failed`;
//! не вышло (например, журнал полон) — `error!`, статус остаётся для resume.
//!
//! Управление (D25, D26, D36): `running → pausing` и `running → cancelling` пишутся в
//! момент наблюдения флага — по обрыву стрима в узле `model`, через `select!` по флагам
//! рядом с пакетом в узле `tools` (идущий инструмент не ждётся), сигналом на границе.
//! Пауза дожидается идущего инструмента и становится чистой паузой `paused`
//! (`command`) после `control.applied {action: "pause"}`; отмена бросает стрим и
//! инструменты, пишет `control.applied {action: "cancel"}` и `cancelled`. Поправка
//! `correct` применяется на границе: управляющее сообщение в историю и
//! `control.applied {action: "correct", command}` с тем же `command`.

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
    RunStatus, Seq, Tier, canonical_json, fail_reason, pause_reason,
};
use za_store::{NewRun, ProjectStore, StoreError, UsageTotals};

use crate::budget::{Spent, budget_number, check_budget};
use crate::context::{
    ContextBudget, ContextConfig, Estimate, UserInput, output_plan, truncation_suspected,
};
use crate::control::ControlRx;
use crate::conversation::{Conversation, HistoryError};
use crate::exit::{LoopExit, exit_for_effect_error, exit_for_model_error};
use crate::memory::{MemoryLevel, MemoryPort, memory_budget};
use crate::prompt::{
    ControlAction, ControlFrom, EnvInfo, WorkMode, escape_untrusted, reasoning_for, render_control,
    render_control_trusted, render_env,
};
use crate::request::{
    FrozenPrefix, PrefixError, RunSetup, TurnOptions, build_request, first_message, freeze,
};
use crate::resume::state_block;
use crate::state::{AtBoundary, Counters, LoopState, Next, PendingExit};
use crate::tools::{FINISH_TOOL, FinishArgs, ToolEnv, ToolsOutcome, run_tools};
use crate::turn::{
    CallVerdict, ControlFlags, LENGTH_CUT_NUDGE, RequestRefs, Reviewed, Signal, TurnError,
    TurnOutput, model_turn, report_parse_errors, review,
};

/// Точки «сбоя» для тестов и --crash-at (D33).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// эффект model шага вернулся (квитанция записана), до turn.finished и снимка
    AfterModelEffect { step: u64 },
    /// эффект boundary шага записан, снимок Model ещё нет (задача 15)
    AfterBoundary { step: u64 },
    /// снимок чистой паузы записан, статус ещё нет
    AfterExitSnapshot { step: u64 },
    /// конечный статус (done, partial, blocked, cancelled) записан, конечного снимка ещё нет
    AfterFinalStatus { step: u64 },
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
    /// Раздел `[context]` (R1-B2 D65): замораживается на run в `RunSetup.context`.
    /// Тир: `tier` (флаг `--tier`) важнее `context.tier`, тот — размера окна.
    pub context: ContextConfig,
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
    /// `None` — у `Halted`: точка сбоя ничего не пишет (D33).
    fn code(&self) -> Option<&'static str> {
        Some(match self {
            LoopError::Store(_) | LoopError::Effect(EffectError::Store(_)) => fail_reason::STORE,
            LoopError::Effect(EffectError::Poisoned) => fail_reason::POISONED,
            LoopError::Effect(_) => fail_reason::EFFECT,
            LoopError::History(_) => fail_reason::HISTORY,
            LoopError::Snapshot(_) => fail_reason::SNAPSHOT,
            LoopError::Graph(_) => fail_reason::GRAPH,
            LoopError::NotResumable(_) => fail_reason::NOT_RESUMABLE,
            LoopError::UnknownEndpoint(_) => fail_reason::UNKNOWN_ENDPOINT,
            LoopError::ChangeMidStep => fail_reason::CHANGE_MID_STEP,
            LoopError::Prefix(_) => fail_reason::PREFIX,
            LoopError::Request(_) => fail_reason::REQUEST,
            LoopError::Halted => return None,
        })
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

pub(crate) const BOUNDARY: NodeId = "boundary";
pub(crate) const MODEL: NodeId = "model";
const TOOLS: NodeId = "tools";

/// Имя эффекта границы шага (D9).
const BOUNDARY_EFFECT: &str = "boundary";
/// Имя эффекта блока `<state>` сообщения `resume` (задача 15).
const STATE_EFFECT: &str = "state";
/// Имя эффекта снимка памяти шага 0 (D24).
const MEMORY_EFFECT: &str = "memory.snapshot";
/// Имя эффекта оценки контекста на границе шага (R1-B2 D42).
const CONTEXT_EFFECT: &str = "context";
/// Префикс квитанции-отказа эффекта, у которого внутри не прочитался журнал.
const STORE_ERROR: &str = "store_error:";
/// `n_ctx`, если эндпоинт не сообщает размер контекста (D37).
pub(crate) const DEFAULT_N_CTX: u32 = 8192;
/// Пустых ходов подряд до `blocked` (D18).
const EMPTY_TURNS_LIMIT: u32 = 3;
/// Страница чтения ввода на границе.
const INBOX_PAGE: usize = 512;

/// Nudge пустого хода top (D18, D23).
const EMPTY_NUDGE_TOP: &str = "Your last response had no text and no tool calls. Continue the task: answer the user or call a tool.";
/// Nudge обрезанного ответа: текст без вызовов с `finish = Length` (решение контроллера, I2).
const TRUNCATED_NUDGE: &str = "Your previous reply was cut off by the output limit. Continue more briefly or split the answer into parts.";
/// Nudge пустого хода worker и orchestrator (D18, D23).
const EMPTY_NUDGE_SUBAGENT: &str = "Your last response made no tool calls. Continue the task with tool calls; when the work is done, partly done or blocked, call finish.";

// ---- Шаг 0 ----

/// Создаёт run (role, parent, model_profile = endpoint), queued → running. Шаг 0 (без next_step):
/// n_ctx = model.context_tokens(&endpoint), нет значения — 8192 и `context.alert {kind:
/// "n_ctx_unknown"}` (D37, R1-B2 D43); tier = spec.tier, иначе spec.context.tier, иначе
/// Tier::for_context(n_ctx) (R1-B2 D65); эффект memory.snapshot (ReadOnly, ключ {run}:0:0); при L0 —
/// memory.unavailable; freeze, блоб FrozenPrefix (put_blob), первое сообщение, persist; снимок шага 0
/// {step: 0, next: Boundary, exit: None} на seq последнего события (D4); затем run_graph с узла boundary.
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
    let known_ctx = deps.model.context_tokens(&spec.endpoint);
    let n_ctx = known_ctx.unwrap_or(DEFAULT_N_CTX);
    let tier = spec
        .tier
        .or(spec.context.tier)
        .unwrap_or_else(|| Tier::for_context(n_ctx));
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
        context: spec.context.clone(),
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
    // D43: окно неизвестно — 8192 и событие (после снимка памяти: снимок шага 0 стоит
    // на этом событии).
    if known_ctx.is_none() {
        last_seq = Some(n_ctx_unknown(&deps.journal, run, &spec.endpoint, n_ctx).await?);
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
    let env_text = render_env(&env);
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
        stopping: None,
        at_boundary: AtBoundary::default(),
        meter: Default::default(),
        task: spec.task,
        env_text,
        user_inputs: Vec::new(),
    };
    save_snapshot(&store, &st, Next::Boundary, None).await?;
    Ok(st)
}

/// Реестр run: `deps.tools` и инструменты capability. Повтор имени — ошибка префикса.
pub(crate) fn run_registry(deps: &LoopDeps) -> Result<Arc<ToolRegistry>, LoopError> {
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
pub(crate) fn run_tools_of(
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
    let ab = &st.at_boundary;
    if ab.budget.is_some() || ab.run_mode.is_some() {
        // Resume со снимка `Model` отложил их до границы, а run остановился раньше:
        // изменения не применены и в снимок не попали (M1 ревью задачи 15).
        tracing::warn!(run = %run, budget = ?ab.budget, run_mode = ?ab.run_mode,
            "изменения resume не применены: run остановился до границы шага, их нужно передать снова");
    }
    let exit = match (exit, st.stopping) {
        // `cancelling` уже записан: из него есть только `cancelled` (часть 01, 4.3).
        // Итог `finish`, исполненного до наблюдения отмены, остаётся в истории.
        (LoopExit::Finished(status), Some(RunStatus::Cancelling)) => {
            tracing::info!(run = %run, ?status, "отмена записана раньше итога finish: run cancelled");
            LoopExit::Cancelled
        }
        (exit, _) => exit,
    };
    let outcome = match exit {
        LoopExit::Finished(status) => {
            if st.stopping == Some(RunStatus::Pausing) {
                // Пауза, наблюдённая во время `finish`, снимается: `finish` важнее
                // (решение контроллера к задаче 11), `pausing → running → status`.
                st.generation = j
                    .set_status(run, RunStatus::Running, st.generation, None)
                    .await?;
                st.stopping = None;
            }
            st.generation = j.set_status(run, status, st.generation, None).await?;
            final_snapshot(deps, st).await?;
            deps.model.finish_run(run);
            LoopOutcome::Finished {
                status,
                answer: st.answer.clone(),
                finish: st.finish.clone(),
            }
        }
        LoopExit::Paused(reason) => {
            if st.stopping == Some(RunStatus::Pausing) {
                // `pausing` записан, а run встаёт в `paused` по другой причине: пауза
                // по команде поглощена этим выходом, флаг снимается (ревью задачи 14, M2).
                st.sig.clear_pause();
            }
            st.generation = j
                .set_status(run, RunStatus::Paused, st.generation, Some(reason))
                .await?;
            st.stopping = None;
            LoopOutcome::Paused {
                reason: reason.into(),
                interrupt: None,
            }
        }
        LoopExit::Suspended { reason, interrupt } => {
            if st.stopping == Some(RunStatus::Pausing) {
                // Пауза по команде исполнена прерыванием: run и так встаёт в `paused`.
                st.sig.clear_pause();
            }
            st.generation = j
                .set_status(run, RunStatus::Paused, st.generation, Some(reason))
                .await?;
            st.stopping = None;
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
            mark_stop(
                j,
                run,
                &mut st.generation,
                &mut st.stopping,
                RunStatus::Cancelling,
            )
            .await?;
            st.last_seq = append(
                j,
                run,
                Actor::Runtime,
                EventKind::ControlApplied,
                json!({ "action": "cancel", "command": "" }),
            )
            .await?;
            st.generation = j
                .set_status(run, RunStatus::Cancelled, st.generation, None)
                .await?;
            final_snapshot(deps, st).await?;
            deps.model.finish_run(run);
            LoopOutcome::Cancelled
        }
    };
    tracing::info!(run = %run, outcome = ?outcome, "цикл run остановлен");
    Ok(outcome)
}

/// Ошибка цикла после старта (решение R0): попытка `failed` с причиной и `finish_run`;
/// не вышло — `error!`, статус остаётся для resume. `Halted` ничего не пишет (D33).
pub(crate) async fn fail(deps: &LoopDeps, run: RunId, generation: u64, e: LoopError) -> LoopError {
    let Some(reason) = e.code() else {
        return e;
    };
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
    // Изменения resume со снимка `Model` вступают с первой границы после шага (D6).
    if let Some(b) = st.at_boundary.budget.take() {
        st.budget = b;
    }
    if let Some(m) = st.at_boundary.run_mode.take() {
        st.setup.run_mode = m;
    }
    // (1) Сигналы до шага (D9): Input отброшен — ввод прочитает сама граница.
    // `pausing`/`cancelling`, записанные во время пакета, доводятся до конца, даже
    // если сам пакет флаг не прочитал (кончился раньше).
    match st.sig.drain() {
        Some(Signal::Cancel) => return command_cancel(deps, st).await,
        Some(Signal::Pause) => return command_pause(sh, st).await,
        _ if st.stopping == Some(RunStatus::Cancelling) => {
            return command_cancel(deps, st).await;
        }
        _ if st.stopping == Some(RunStatus::Pausing) => return command_pause(sh, st).await,
        _ => {}
    }
    // (2), (3) Новый шаг и его эффект `boundary`.
    st.ctx.next_step();
    let step = st.ctx.step();
    let store = deps.journal.store().clone();
    let (run, after) = (st.run, st.inbox_after);
    let mut failure: Option<StoreError> = None;
    let fref = &mut failure;
    // Источник нужен активному времени (4): реплей квитанции несёт `now_ms` прошлого процесса.
    let slot = st.ctx.reserve().map_err(effect_err)?;
    let (r, source) = slot
        .run_traced(
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
    st.ctx.observe(&r);
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
    if crashed(deps, CrashPoint::AfterBoundary { step }) {
        return Err(LoopError::Halted);
    }
    // (4) Активное время (D19). Реплей границы из журнала (после resume) время не
    // считает: `now_ms` квитанции записан до простоя между процессами, и `tick` с ним
    // засчитал бы простой следующей живой границей. Теряется один интервал (решение
    // контроллера, I1 ревью задачи 15).
    match source {
        za_core::EffectSource::Live => st.counters.tick(b.now_ms),
        za_core::EffectSource::Journal => st.counters.last_boundary_ms = None,
    }
    // (5) Проверка бюджета до приёма ввода (D32): ввод остаётся непрочитанным,
    // `inbox_after` не сдвигается, следующая граница прочитает его ровно один раз.
    let spent = Spent::from_totals(&b.usage, st.counters.steps, st.counters.active_ms);
    if let Some((kind, used, limit)) = check_budget(&st.budget, &spent) {
        return budget_exceeded(deps, st, kind, used, limit).await;
    }
    // (6) Ввод в историю.
    for i in &b.inputs {
        match i {
            Inbox::Input { seq, text } => {
                if text.is_empty() {
                    tracing::warn!(seq, "пустой ввод пользователя пропущен");
                    continue;
                }
                // Текст пользователя — как есть, только управляющий тег экранируется:
                // ввод не выдаёт себя за голос рантайма (D23).
                let text = escape_untrusted(text);
                st.conv.push_user(text.clone())?;
                push_input(&store_of(deps), st, &text, false).await?;
            }
            Inbox::Correct { note, command, .. } => {
                st.conv.push_user(render_control(
                    ControlFrom::User,
                    ControlAction::Correct,
                    command,
                    note,
                ))?;
                push_input(&store_of(deps), st, &escape_untrusted(note), true).await?;
                // D36: тот же `command`, что в `control.requested`.
                st.last_seq = append(
                    &deps.journal,
                    st.run,
                    Actor::Runtime,
                    EventKind::ControlApplied,
                    json!({ "action": "correct", "command": command }),
                )
                .await?;
            }
        }
    }
    st.inbox_after = b.inbox_after;
    // (6') Первая граница resume со снимка `Boundary`: после ввода — `resume` с блоком
    // `<state>` (A.6). Блок — эффект: реплей границы получает тот же текст.
    if st.at_boundary.resume_note {
        let text = state_effect(deps, st).await?;
        st.conv.push_user(render_control_trusted(
            ControlFrom::Runtime,
            ControlAction::Resume,
            "",
            &text,
        ))?;
        st.at_boundary.resume_note = false;
    }
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
    // (7') Оценка запроса хода (R1-B2 D42): ввод и напоминания уже в истории.
    let est = context_effect(deps, st, step, 0).await?;
    context_thresholds(deps, st, est).await?;
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

/// Эффект `state` (ReadOnly): текст блока `<state>` для сообщения `resume`.
async fn state_effect(deps: &LoopDeps, st: &mut LoopState) -> Result<String, LoopError> {
    let store = store_of(deps);
    let run = st.run;
    let mut failure: Option<StoreError> = None;
    let fref = &mut failure;
    let r = st
        .ctx
        .effect(
            EffectSpec {
                class: EffectClass::ReadOnly,
                name: STATE_EFFECT.into(),
                args: json!({}),
            },
            move || async move {
                match state_block(&store, run).await {
                    Ok(text) => Ok(json!({ "text": text })),
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
    v.get("text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            let e = StoreError::Corrupt("квитанция state без text".into());
            st.ctx.observe::<()>(&Err(EffectError::Store(e.clone())));
            LoopError::Store(e)
        })
}

// ---- Контекст (R1-B2) ----

/// Параметры хода: `Required` после второго пустого хода (D18), лимит вывода и
/// мышление — [`output_plan`] (D40). Одни и те же для оценки на границе и для хода.
fn turn_options(st: &LoopState) -> TurnOptions {
    let plan = output_plan(
        &ContextBudget::for_context(st.setup.n_ctx),
        &st.setup.reasoning,
    );
    TurnOptions {
        tool_choice: if st.counters.forced_required {
            ToolChoice::Required
        } else {
            ToolChoice::Auto
        },
        max_output_tokens: Some(plan.max_output_tokens),
        reasoning: plan.reasoning,
        ..TurnOptions::default()
    }
}

/// D66: ввод пользователя на шаге (6) границы — блоб текста и индекс его сообщения.
async fn push_input(
    store: &ProjectStore,
    st: &mut LoopState,
    text: &str,
    correct: bool,
) -> Result<(), StoreError> {
    // Блоб адресуется содержимым: повтор при реплее границы безвреден, как `persist`.
    let hash = store.put_blob(text.as_bytes()).await?;
    st.user_inputs.push(UserInput {
        text: hash,
        correct,
        at: st.conv.messages().len().checked_sub(1),
    });
    Ok(())
}

/// Эффект `context` (D42, `ReadOnly`, аргументы `{step, pass}`): оценка запроса хода
/// по текущей истории ([`crate::ContextMeter::estimate`]). Внешний счётчик токенов
/// входит в цикл только так: реплей получает ту же оценку и то же решение.
async fn context_effect(
    deps: &LoopDeps,
    st: &mut LoopState,
    step: u64,
    pass: u32,
) -> Result<Estimate, LoopError> {
    let req = build_request(&st.frozen, &st.setup, &st.conv, &turn_options(st));
    let model = Arc::clone(&deps.model);
    let ep = st.setup.endpoint.clone();
    let meter = st.meter.clone();
    let v = st
        .ctx
        .effect(
            EffectSpec {
                class: EffectClass::ReadOnly,
                name: CONTEXT_EFFECT.into(),
                args: json!({ "step": step, "pass": pass }),
            },
            move || async move {
                let e = meter.estimate(&req, model.as_ref(), &ep).await;
                serde_json::to_value(e).map_err(|e| format!("оценка не сериализуется: {e}"))
            },
        )
        .await
        .map_err(effect_err)?;
    let est: Estimate = serde_json::from_value(v).map_err(|e| {
        let e = StoreError::Corrupt(format!("квитанция context шага {step}: {e}"));
        st.ctx.observe::<()>(&Err(EffectError::Store(e.clone())));
        LoopError::Store(e)
    })?;
    st.meter.last_estimate = Some(est.tokens);
    Ok(est)
}

/// Пороги по одной мере `оценка + резерв` (D43, D45, D46), резерв — лимит вывода хода.
/// Удаление старых результатов (задача 2) и компакция (задача 3) ещё не подключены:
/// превышение порога или ждущая компакция пишут `context.alert {kind: "threshold"}`.
async fn context_thresholds(
    deps: &LoopDeps,
    st: &mut LoopState,
    est: Estimate,
) -> Result<(), StoreError> {
    let reserve = turn_options(st).max_output_tokens.unwrap_or(0);
    let measure = u64::from(est.tokens) + u64::from(reserve);
    let n_ctx = st.setup.n_ctx;
    let cfg = &st.setup.context;
    let at = |share: f32| measure as f64 >= f64::from(share) * f64::from(n_ctx);
    let prune = at(cfg.prune_at);
    let compact = at(cfg.compact_at) || st.meter.compact_pending;
    if prune || compact {
        tracing::debug!(run = %st.run, measure, n_ctx, prune, compact,
            "порог контекста достигнут, разгрузка ещё не подключена");
        st.last_seq = context_alert(
            &deps.journal,
            st.run,
            json!({
                "kind": "threshold",
                "estimate": est.tokens,
                "exact": est.exact,
                "reserve": reserve,
                "n_ctx": n_ctx,
                "prune": prune,
                "compact": compact,
            }),
        )
        .await?;
    }
    Ok(())
}

/// D43: расход хода меньше половины оценки его запроса — сервер, вероятно, срезал
/// начало промпта: `context.alert {kind: "truncation_suspected"}` и компакция на
/// ближайшей границе. Расход неизвестен (`input == 0`) — проверки нет.
async fn check_truncation(
    deps: &LoopDeps,
    st: &mut LoopState,
    out: &TurnOutput,
) -> Result<(), StoreError> {
    let Some(estimate) = st.meter.last_estimate else {
        return Ok(());
    };
    let input = out.usage.input;
    if !truncation_suspected(estimate, input) {
        return Ok(());
    }
    tracing::warn!(run = %st.run, turn = st.turn, estimate, input,
        "расход хода меньше половины оценки: подозрение на обрезку контекста сервером");
    st.last_seq = context_alert(
        &deps.journal,
        st.run,
        json!({
            "kind": "truncation_suspected",
            "turn": st.turn,
            "estimate": estimate,
            "input": input,
        }),
    )
    .await?;
    st.meter.compact_pending = true;
    Ok(())
}

/// `context.alert {kind: "n_ctx_unknown", endpoint, n_ctx}` (D43): старт run и resume
/// со сменой эндпоинта.
pub(crate) async fn n_ctx_unknown(
    j: &Journal,
    run: RunId,
    endpoint: &EndpointId,
    n_ctx: u32,
) -> Result<Seq, StoreError> {
    tracing::debug!(run = %run, endpoint = %endpoint.0, n_ctx,
        "размер контекста эндпоинта неизвестен, принят по умолчанию");
    context_alert(
        j,
        run,
        json!({ "kind": "n_ctx_unknown", "endpoint": endpoint.0, "n_ctx": n_ctx }),
    )
    .await
}

/// Событие `context.alert` (D43, D63).
async fn context_alert(j: &Journal, run: RunId, payload: Value) -> Result<Seq, StoreError> {
    append(j, run, Actor::Runtime, EventKind::ContextAlert, payload).await
}

// ---- Узел model ----

async fn model_node(sh: &Shared, st: &mut LoopState) -> Result<Transition, LoopError> {
    let deps = &sh.deps;
    let opts = turn_options(st);
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
                // Флаг наблюдён обрывом стрима: `pausing`/`cancelling` пишутся сейчас (D26).
                (Signal::Pause, za_core::EffectSource::Live) => command_pause(sh, st).await,
                (Signal::Cancel, za_core::EffectSource::Live) => command_cancel(deps, st).await,
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
    check_truncation(deps, st, &out).await?;
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
    // База счётчика (D41): ход принят, история включает его ответ.
    st.meter.after_turn(st.conv.messages().len(), &out.usage);

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
        return finish_with(st, RunStatus::Done);
    }
    // Обрезанный ответ (решение контроллера, I2): текст есть, вызовов нет, `Length`.
    // Ответом не считается и идёт в счёт пустых ходов D18, но без `Required`: модели
    // нужно продолжить текст, а не вызвать инструмент.
    let truncated = out.finish == FinishReason::Length && !rev.text.trim().is_empty();
    // Пустой ход (D18): нет вызовов и, у top, нет ответа.
    let outcome = if truncated { "truncated" } else { "empty" };
    turn_finished(deps, st, outcome, Some(&rev)).await?;
    push_nudges(st, &rev.nudges)?;
    st.counters.empty_turns += 1;
    if st.counters.empty_turns >= EMPTY_TURNS_LIMIT {
        tracing::warn!(run = %st.run, "пустые ходы подряд: run blocked");
        return finish_with(st, RunStatus::Blocked);
    }
    // Обрезанный вызов в тексте уже получил `LENGTH_CUT_NUDGE` от разбора: он точнее
    // (вызов не выполнен из-за обрезки), второй nudge про обрезку не нужен.
    let cut_call = rev.nudges.iter().any(|n| n == LENGTH_CUT_NUDGE);
    let nudge = if truncated {
        (!cut_call).then_some(TRUNCATED_NUDGE)
    } else if st.setup.role == RunRole::Top {
        Some(EMPTY_NUDGE_TOP)
    } else {
        Some(EMPTY_NUDGE_SUBAGENT)
    };
    if let Some(nudge) = nudge {
        push_nudges(st, &[nudge.to_owned()])?;
    }
    if !truncated && st.counters.empty_turns == EMPTY_TURNS_LIMIT - 1 {
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
    // Флаги наблюдаются рядом с пакетом (D26): `pausing`/`cancelling` пишутся сразу,
    // не дожидаясь конца идущего инструмента. На сигналы пакет реагирует сам.
    let mut flags = st.sig.flags();
    let r = {
        let batch = run_tools(
            &mut st.ctx,
            &deps.journal,
            &sh.registry,
            &env,
            &rev.calls,
            &mut st.sig,
        );
        tokio::pin!(batch);
        let mut watching = true;
        loop {
            // Флаг, поднятый до пакета, — тоже наблюдение.
            let f = *flags.borrow_and_update();
            let to = if f.cancel {
                Some(RunStatus::Cancelling)
            } else if f.pause {
                Some(RunStatus::Pausing)
            } else {
                None
            };
            if let Some(to) = to {
                mark_stop(
                    &deps.journal,
                    st.run,
                    &mut st.generation,
                    &mut st.stopping,
                    to,
                )
                .await?;
            }
            tokio::select! {
                biased;
                r = &mut batch => break r,
                ch = flags.changed(), if watching => {
                    // Отправитель живёт в `Signals`; закрытый канал — больше не следим.
                    if ch.is_err() {
                        watching = false;
                    }
                }
            }
        }
    };
    let out = match r {
        Ok(o) => o,
        Err(e) => {
            st.exit = Some(effect_exit(e)?);
            return Ok(Transition::Done);
        }
    };
    // `ToolsOutcome` не различает Pause и Input: решают флаги (Carry к задаче 12/14).
    let flags = *st.sig.flags().borrow();
    let verdict = tools_verdict(&out, flags)?;
    let ToolsOutcome {
        results,
        suspend,
        finish,
        ..
    } = out;
    st.conv.push_results(results)?;
    match verdict {
        ToolsVerdict::Finish(status) => {
            // Синтетические результаты — только вызовам без результата (CORE-105).
            st.conv.close_interrupted();
            if let Some(f) = finish {
                st.answer = Some(f.result.clone());
                st.finish = Some(f);
            }
            finish_with(st, status)
        }
        ToolsVerdict::Cancel => {
            st.conv.close_interrupted();
            command_cancel(deps, st).await
        }
        // Снимка нет (D5): resume реплеит шаг до прерывания и читает решение.
        ToolsVerdict::Suspend => match suspend {
            Some(int) => Ok(Transition::Suspend(int)),
            None => Err(LoopError::Graph("прерывание пакета без прерывания".into())),
        },
        ToolsVerdict::Next { aborted } => {
            if aborted {
                st.conv.close_interrupted();
            }
            // Nudge — отдельными сообщениями после результатов (D23).
            push_nudges(st, &rev.nudges)?;
            Ok(Transition::Goto(BOUNDARY))
        }
        ToolsVerdict::Pause => {
            st.conv.close_interrupted();
            push_nudges(st, &rev.nudges)?;
            command_pause(sh, st).await
        }
    }
}

/// Что делать после пакета вызовов.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolsVerdict {
    /// Принят `finish`: run завершается с этим статусом.
    Finish(RunStatus),
    Cancel,
    /// Пакет остановился на прерывании.
    Suspend,
    /// Пакет оборван паузой по команде.
    Pause,
    /// К следующей границе; `aborted` — пакет оборван вводом.
    Next {
        aborted: bool,
    },
}

/// Решение по итогу пакета. Приоритет: `finish` важнее отмены и обрыва (решения
/// контроллера к задачам 11 и 12: вызов `finish` исполнен и записан), затем отмена,
/// прерывание, обрыв; Pause и Input различаются по флагам.
fn tools_verdict(out: &ToolsOutcome, flags: ControlFlags) -> Result<ToolsVerdict, LoopError> {
    if let Some(f) = &out.finish {
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
        return Ok(ToolsVerdict::Finish(status));
    }
    if out.cancelled {
        return Ok(ToolsVerdict::Cancel);
    }
    if out.suspend.is_some() {
        return Ok(ToolsVerdict::Suspend);
    }
    if out.aborted {
        if flags.cancel {
            return Ok(ToolsVerdict::Cancel);
        }
        if flags.pause {
            return Ok(ToolsVerdict::Pause);
        }
    }
    Ok(ToolsVerdict::Next {
        aborted: out.aborted,
    })
}

// ---- Выходы ----

/// Ошибка эффекта в выход (D11): `ReplayMismatch`, `Unknown` — пауза без снимка
/// (resume реплеит шаг), прочие — ошибка цикла.
fn effect_exit(e: EffectError) -> Result<LoopExit, LoopError> {
    exit_for_effect_error(&e).ok_or_else(|| effect_err(e))
}

/// `running → pausing` или `→ cancelling` в момент наблюдения флага (D26); повтор
/// безвреден. Отмена важнее паузы: после `cancelling` пауза не пишется, из `pausing`
/// отмена допустима (часть 01, 4.3).
async fn mark_stop(
    j: &Journal,
    run: RunId,
    generation: &mut u64,
    stopping: &mut Option<RunStatus>,
    to: RunStatus,
) -> Result<(), StoreError> {
    if *stopping == Some(to) || *stopping == Some(RunStatus::Cancelling) {
        return Ok(());
    }
    let reason = (to == RunStatus::Pausing).then_some(pause_reason::COMMAND);
    *generation = j.set_status(run, to, *generation, reason).await?;
    *stopping = Some(to);
    tracing::info!(run = %run, status = ?to, "команда управления наблюдена");
    Ok(())
}

/// Отмена по команде (D26): `cancelling` сразу; выход `Cancelled` пишет
/// `control.applied {action: "cancel"}`, `cancelled` и `finish_run`.
async fn command_cancel(deps: &LoopDeps, st: &mut LoopState) -> Result<Transition, LoopError> {
    mark_stop(
        &deps.journal,
        st.run,
        &mut st.generation,
        &mut st.stopping,
        RunStatus::Cancelling,
    )
    .await?;
    st.exit = Some(LoopExit::Cancelled);
    Ok(Transition::Done)
}

/// Чистая пауза по команде (D5, D26): `pausing` (если ещё не записан),
/// `control.applied {action: "pause"}`, снимок, статус `paused` (`command`). Флаг
/// паузы снимается: следующая пауза — новый сигнал.
async fn command_pause(sh: &Shared, st: &mut LoopState) -> Result<Transition, LoopError> {
    mark_stop(
        &sh.deps.journal,
        st.run,
        &mut st.generation,
        &mut st.stopping,
        RunStatus::Pausing,
    )
    .await?;
    if st.stopping == Some(RunStatus::Cancelling) {
        // Отмена пришла раньше: пауза поглощена.
        st.exit = Some(LoopExit::Cancelled);
        return Ok(Transition::Done);
    }
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

/// Выход по бюджету (D19, D5): `budget.exceeded {kind, used, limit}`, снимок
/// `{next: Boundary, exit: Some(BlockedOnBudget)}` на его seq, `blocked_on_budget`.
async fn budget_exceeded(
    deps: &LoopDeps,
    st: &mut LoopState,
    kind: BudgetKind,
    used: f64,
    limit: f64,
) -> Result<Transition, LoopError> {
    tracing::warn!(run = %st.run, ?kind, used, limit, "бюджет run исчерпан");
    st.last_seq = append(
        &deps.journal,
        st.run,
        Actor::Runtime,
        EventKind::BudgetExceeded,
        json!({
            "kind": kind,
            "used": budget_number(kind, used),
            "limit": budget_number(kind, limit),
        }),
    )
    .await?;
    clean_pause(deps, st, LoopExit::BlockedOnBudget(kind)).await
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

/// Конечное состояние: только выход. Снимок до статуса не пишется (решение
/// контроллера, I1): сбой до записи статуса оставляет последним снимок `Model` шага,
/// и resume реплеит шаг по квитанциям к тому же итогу.
fn finish_with(st: &mut LoopState, status: RunStatus) -> Result<Transition, LoopError> {
    st.exit = Some(LoopExit::Finished(status));
    Ok(Transition::Done)
}

/// Конечный снимок `{next: Boundary, exit: None}` на `last_seq` — после записи
/// конечного статуса и до `finish_run` (решение контроллера, I1; D17: история снимка
/// валидна). Run в конечном статусе не продолжается (`NotResumable`): снимок — для
/// чтения итоговой истории. Ошибка его записи итог не меняет (статус в журнале —
/// источник истины), пишется `error!`.
async fn final_snapshot(deps: &LoopDeps, st: &mut LoopState) -> Result<(), LoopError> {
    if crashed(
        deps,
        CrashPoint::AfterFinalStatus {
            step: st.ctx.step(),
        },
    ) {
        return Err(LoopError::Halted);
    }
    let store = store_of(deps);
    let r = match persist(&store, st).await {
        Ok(()) => save_snapshot(&store, st, Next::Boundary, None).await,
        Err(e) => Err(e.into()),
    };
    if let Err(e) = r {
        tracing::error!(run = %st.run, error = %e, "конечный снимок не записан, статус уже конечный");
    }
    Ok(())
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

    fn finish_args(status: &str) -> FinishArgs {
        FinishArgs {
            status: status.into(),
            result: "r".into(),
            evidence: Vec::new(),
            open_issues: Vec::new(),
        }
    }

    #[test]
    fn finish_wins_over_cancel_and_abort() {
        let pause = ControlFlags {
            pause: true,
            cancel: false,
        };
        let cancel = ControlFlags {
            pause: false,
            cancel: true,
        };
        // M5: `finish` и `cancelled` в одном пакете — итог по `finish`.
        let out = ToolsOutcome {
            finish: Some(finish_args("done")),
            cancelled: true,
            ..ToolsOutcome::default()
        };
        assert_eq!(
            tools_verdict(&out, cancel).unwrap(),
            ToolsVerdict::Finish(RunStatus::Done)
        );
        // Решение к задаче 11: `finish` и `aborted` — тоже по `finish`.
        let out = ToolsOutcome {
            finish: Some(finish_args("partial")),
            aborted: true,
            ..ToolsOutcome::default()
        };
        assert_eq!(
            tools_verdict(&out, pause).unwrap(),
            ToolsVerdict::Finish(RunStatus::Partial)
        );
        // Без `finish`: отмена, затем обрыв по флагам.
        let out = ToolsOutcome {
            cancelled: true,
            ..ToolsOutcome::default()
        };
        assert_eq!(tools_verdict(&out, pause).unwrap(), ToolsVerdict::Cancel);
        let out = ToolsOutcome {
            aborted: true,
            ..ToolsOutcome::default()
        };
        assert_eq!(tools_verdict(&out, pause).unwrap(), ToolsVerdict::Pause);
        assert_eq!(tools_verdict(&out, cancel).unwrap(), ToolsVerdict::Cancel);
        assert_eq!(
            tools_verdict(&out, ControlFlags::default()).unwrap(),
            ToolsVerdict::Next { aborted: true }
        );
        assert_eq!(
            tools_verdict(&ToolsOutcome::default(), pause).unwrap(),
            ToolsVerdict::Next { aborted: false }
        );
    }

    #[test]
    fn halted_has_no_fail_reason() {
        assert_eq!(LoopError::Halted.code(), None);
        assert_eq!(
            LoopError::Store(StoreError::Full).code(),
            Some(fail_reason::STORE)
        );
    }

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

    /// Модульная обвязка `apply_exit`: run после шага 0 на `ScriptedModel`, канал
    /// управления и состояние цикла. Каталог — `.tmp/za-agent-unit/<name>`.
    async fn unit_state(
        name: &str,
    ) -> (
        LoopDeps,
        ProjectStore,
        Arc<za_llm::ScriptedModel>,
        RunId,
        crate::control::RunControl,
        LoopState,
    ) {
        use crate::control::control_channel;
        use crate::memory::NoMemory;
        use za_core::EventBus;
        use za_llm::{BackendKind, ScriptedModel, default_caps};

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.tmp/za-agent-unit")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = ProjectStore::open(&dir).unwrap();
        let model = Arc::new(ScriptedModel::new(default_caps(BackendKind::LlamaCpp)));
        let deps = LoopDeps {
            journal: Journal::new(store.clone(), EventBus::new(EventBus::DEFAULT_CAPACITY)),
            model: model.clone(),
            tools: Arc::new(ToolRegistry::default()),
            capabilities: Vec::new(),
            memory: Arc::new(NoMemory),
            crash: None,
        };
        let run = RunId::new();
        let spec = RunSpec {
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
            task: "t".into(),
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
        };
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
        let g = deps
            .journal
            .set_status(run, RunStatus::Running, 0, None)
            .await
            .unwrap();
        let (ctl, rx) = control_channel(store.clone(), run);
        let st = step_zero(&deps, run, spec, rx, g).await.unwrap();
        (deps, store, model, run, ctl, st)
    }

    /// Статусы run по порядку из событий `run.status`.
    async fn statuses(store: &ProjectStore, run: RunId) -> Vec<String> {
        store
            .read_run(run, Seq(0), usize::MAX)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.kind == EventKind::RunStatus)
            .filter_map(|e| e.payload["to"].as_str().map(str::to_owned))
            .collect()
    }

    /// M3 ревью задачи 14, решение progress:145: `finish` после записанного
    /// `cancelling` даёт `cancelled` (из `cancelling` переход только в `cancelled`).
    #[tokio::test]
    async fn finished_after_cancelling_is_cancelled() {
        let (deps, store, _model, run, _ctl, mut st) =
            unit_state("finished_after_cancelling").await;
        mark_stop(
            &deps.journal,
            run,
            &mut st.generation,
            &mut st.stopping,
            RunStatus::Cancelling,
        )
        .await
        .unwrap();
        let out = apply_exit(&deps, &mut st, LoopExit::Finished(RunStatus::Done))
            .await
            .unwrap();
        assert_eq!(out, LoopOutcome::Cancelled);
        let r = store.get_run(run).await.unwrap().unwrap();
        assert_eq!(r.status, RunStatus::Cancelled);
        assert_eq!(
            statuses(&store, run).await,
            ["running", "cancelling", "cancelled"]
        );
    }

    /// M3 ревью задачи 14, решение к задаче 11: пауза, наблюдённая во время `finish`,
    /// снимается — `pausing → running → done`.
    #[tokio::test]
    async fn finished_after_pausing_is_done() {
        let (deps, store, _model, run, ctl, mut st) = unit_state("finished_after_pausing").await;
        ctl.pause();
        mark_stop(
            &deps.journal,
            run,
            &mut st.generation,
            &mut st.stopping,
            RunStatus::Pausing,
        )
        .await
        .unwrap();
        st.answer = Some("ok".into());
        let out = apply_exit(&deps, &mut st, LoopExit::Finished(RunStatus::Done))
            .await
            .unwrap();
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
        let r = store.get_run(run).await.unwrap().unwrap();
        assert_eq!(r.status, RunStatus::Done);
        assert_eq!(
            statuses(&store, run).await,
            ["running", "pausing", "running", "done"]
        );
    }

    /// M2 ревью задачи 14: `pausing` записан, а run встал в `paused` по другой
    /// причине — флаг паузы поглощён, resume на том же канале не встаёт на паузу.
    #[tokio::test]
    async fn paused_after_pausing_clears_pause_flag() {
        use crate::resume::{ResumeChange, resume_run};
        use za_llm::{Message, Part, Role, StreamEvent, Usage};

        let (deps, _store, model, run, ctl, mut st) = unit_state("paused_after_pausing").await;
        // Пауза наблюдена (`pausing`), но выход — пауза провайдера.
        ctl.pause();
        mark_stop(
            &deps.journal,
            run,
            &mut st.generation,
            &mut st.stopping,
            RunStatus::Pausing,
        )
        .await
        .unwrap();
        let out = apply_exit(&deps, &mut st, LoopExit::Paused(pause_reason::PROVIDER))
            .await
            .unwrap();
        assert_eq!(
            out,
            LoopOutcome::Paused {
                reason: pause_reason::PROVIDER.into(),
                interrupt: None,
            }
        );
        assert!(!st.sig.flags().borrow().pause, "флаг паузы не снят");
        // Тот же канал (тот же `RunControl`) — resume в том же процессе.
        model.push(vec![Ok(StreamEvent::Finished {
            message: Message {
                role: Role::Assistant,
                parts: vec![Part::Text("ок".into())],
            },
            finish: FinishReason::Stop,
            usage: Usage::default(),
        })]);
        let LoopState { sig, .. } = st;
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            resume_run(&deps, run, ResumeChange::default(), ControlRx { sig }),
        )
        .await
        .expect("resume не завершился в срок")
        .unwrap();
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
        drop(ctl);
    }
}
