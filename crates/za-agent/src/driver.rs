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
//! напоминания хуков, эффект `context` — оценка запроса хода (R1-B2 D42), по порогам
//! удаление старых результатов (новая эпоха, R1-B2 D45) и компакция (R1-B2 D46–D50,
//! D70), `persist`, `turn.started`, снимок `Model` на его `seq`. Ход получает лимит
//! вывода из [`crate::output_plan`] (R1-B2 D40); после хода — проверка тихой обрезки
//! (R1-B2 D43); `ContextOverflow` хода — аварийная компакция и один повтор (R1-B2 D49).
//! Детекторы (R1-B2 D54–D56, [`crate::DetectorState`]): до пакета вызовов — повтор,
//! отказ или `blocked`; после результатов хода — nudge цикла с рецитацией или `blocked`.
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use za_core::{
    Capability, EffectError, EffectSpec, Exit, Graph, GraphError, Journal, Node, NodeId, RunCtx,
    ToolKind, ToolRegistry, Transition, pause_reason_for,
};
use za_llm::{
    BoxFuture, EndpointId, ErrorClass, FinishReason, ModelCall, ModelPort, Part, ReasoningMode,
    RequestMeta, Sampling, ToolChoice,
};
use za_protocol::{
    Actor, BlobHash, Budget, BudgetKind, EffectClass, EventKind, NewEvent, Priority, RunId,
    RunMode, RunRole, RunStatus, Seq, Tier, canonical_json, detector, fail_reason, pause_reason,
};
use za_store::{NewRun, ProjectStore, StoreError, UsageTotals};

use crate::budget::{Spent, budget_number, check_budget};
use crate::context::{
    CompactInput, ContextBudget, ContextConfig, Estimate, PRUNE_KEEP_TURNS, PRUNE_MIN_BYTES,
    SpillCfg, UserInput, compact, output_plan, prune, recite_task, rubric_tokens,
    truncation_suspected,
};
use crate::control::ControlRx;
use crate::conversation::{Conversation, HistoryError};
use crate::detect::{
    CYCLE_NUDGE, DetectorState, REPEAT_MARK, REPEAT_REJECT, Reaction, StreamWatch, retry_params,
};
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
    CallVerdict, ControlFlags, LENGTH_CUT_NUDGE, PlannedCall, RequestRefs, Reviewed, Signal,
    TurnError, TurnOutput, model_turn_with, report_parse_errors, review,
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
    /// квитанция `compact` шага записана (сводка или обрыв `interrupted:*`); новая
    /// история, `context.compacted` и снимок `Model` — нет (R1-B2 D68): компакция по
    /// порогу на границе и аварийная в узле `model`
    AfterCompact { step: u64 },
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
/// Имя эффекта повторного чтения ввода после компакции на границе (R1-B2, Ruling I-2).
const INBOX_EFFECT: &str = "inbox";
/// Префикс квитанции-отказа эффекта, у которого внутри не прочитался журнал.
const STORE_ERROR: &str = "store_error:";
/// `n_ctx`, если эндпоинт не сообщает размер контекста (D37).
pub(crate) const DEFAULT_N_CTX: u32 = 8192;
/// Пустых ходов подряд до `blocked` (D18).
const EMPTY_TURNS_LIMIT: u32 = 3;
/// Страница чтения ввода на границе.
const INBOX_PAGE: usize = 512;

/// Nudge пустого хода top (D18, D23).
pub(crate) const EMPTY_NUDGE_TOP: &str = "Your last response had no text and no tool calls. Continue the task: answer the user or call a tool.";
/// Nudge обрезанного ответа: текст без вызовов с `finish = Length` (решение контроллера, I2).
pub(crate) const TRUNCATED_NUDGE: &str = "Your previous reply was cut off by the output limit. Continue more briefly or split the answer into parts.";
/// Nudge пустого хода worker и orchestrator (D18, D23).
pub(crate) const EMPTY_NUDGE_SUBAGENT: &str = "Your last response made no tool calls. Continue the task with tool calls; when the work is done, partly done or blocked, call finish.";

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
        legacy_step: false,
        task: spec.task,
        env_text,
        user_inputs: Vec::new(),
        epoch: 0,
        last_next_step: String::new(),
        detectors: DetectorState::default(),
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
pub(crate) fn budget_text(b: &Budget) -> String {
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
        // Реплей обрыва компакции на границе (D7): новая граница без хода.
        .edge(BOUNDARY, BOUNDARY)
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
    // Шаг R1-B1 доработан: следующие ходы — с параметрами R1-B2 (D66).
    st.legacy_step = false;
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
    push_inbox(deps, st, &b.inputs).await?;
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
    boundary_tail(sh, st, step).await
}

/// Шаг (6) границы: ввод и поправки — в историю и `user_inputs` (D66).
async fn push_inbox(
    deps: &LoopDeps,
    st: &mut LoopState,
    inputs: &[Inbox],
) -> Result<(), LoopError> {
    for i in inputs {
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
                // Ввод пользователя сбрасывает окно повтора (Ruling I-3 ревью задачи 6).
                st.detectors.on_user_input();
            }
            Inbox::Correct { note, command, .. } => {
                st.conv.push_user(render_control(
                    ControlFrom::User,
                    ControlAction::Correct,
                    command,
                    note,
                ))?;
                push_input(&store_of(deps), st, &escape_untrusted(note), true).await?;
                st.detectors.on_user_input();
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
    Ok(())
}

/// Повторное чтение ввода после компакции на границе (Ruling I-2): эффект `inbox`
/// (`ReadOnly`, `{step}`) — ввод, пришедший во время компакции, попадает в историю до
/// хода модели. Эффект есть после каждой компакции (на границе и аварийной в узле
/// `model`), поэтому реплей повторяет те же ключи. Сигнал ввода снимается до чтения
/// (m-A): ввод, записанный после чтения, оставит новый сигнал, а не потеряет его.
async fn reread_inbox(deps: &LoopDeps, st: &mut LoopState, step: u64) -> Result<(), LoopError> {
    st.sig.discard_input();
    let store = store_of(deps);
    let (run, after) = (st.run, st.inbox_after);
    let mut failure: Option<StoreError> = None;
    let fref = &mut failure;
    let r = st
        .ctx
        .effect(
            EffectSpec {
                class: EffectClass::ReadOnly,
                name: INBOX_EFFECT.into(),
                args: json!({ "step": step }),
            },
            move || async move {
                match read_boundary(&store, run, after).await {
                    Ok(v) => serde_json::to_value(&v)
                        .map_err(|e| format!("{STORE_ERROR} ввод не сериализуется: {e}")),
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
        let e = StoreError::Corrupt(format!("квитанция inbox шага {step}: {e}"));
        st.ctx.observe::<()>(&Err(EffectError::Store(e.clone())));
        LoopError::Store(e)
    })?;
    push_inbox(deps, st, &b.inputs).await?;
    st.inbox_after = b.inbox_after;
    Ok(())
}

/// Граница после ввода: напоминания (7), `context` и пороги (7'), `persist`,
/// `turn.started`, снимок `Model` (8)–(10).
async fn boundary_tail(
    sh: &Shared,
    st: &mut LoopState,
    step: u64,
) -> Result<Transition, LoopError> {
    let deps = &sh.deps;
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
    if let Some(t) = context_thresholds(sh, st, step, est).await? {
        return Ok(t);
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
///
/// Шаг R1-B1, начатый до обновления (`legacy_step`, снимок версии 1 посреди шага), —
/// с параметрами R1-B1: без лимита вывода и с мышлением run. Тогда `request_hash`
/// реплея совпадает с записанным в журнале (D66).
fn turn_options(st: &LoopState) -> TurnOptions {
    if st.legacy_step {
        return TurnOptions {
            tool_choice: if st.counters.forced_required {
                ToolChoice::Required
            } else {
                ToolChoice::Auto
            },
            ..TurnOptions::default()
        };
    }
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
    st.meter.last_floor = Some(est.floor());
    Ok(est)
}

/// Причина компакции в `context.compacted.reason`: порог `compact_at` (D46).
const COMPACT_THRESHOLD: &str = "threshold";
/// Причина компакции: подозрение на тихую обрезку (D43, `compact_pending`).
const COMPACT_TRUNCATION: &str = "truncation";
/// Причина компакции: `оценка + резерв > n_ctx` на границе (D49, D70).
const COMPACT_WINDOW: &str = "window";
/// Причина компакции: `ContextOverflow` хода (D49).
const COMPACT_OVERFLOW: &str = "overflow";
/// Подозрение на обрезку в пределах стольких ходов после компакции по этой причине —
/// пауза `context` (D43).
const TRUNCATION_REPEAT_TURNS: u32 = 2;

/// `оценка + резерв ≥ share × n_ctx` — одна мера порогов (D43, D45, D46).
fn over_share(est: Estimate, reserve: u32, share: f32, n_ctx: u32) -> bool {
    (u64::from(est.tokens) + u64::from(reserve)) as f64 >= f64::from(share) * f64::from(n_ctx)
}

/// `оценка + резерв > n_ctx`: запрос хода не помещается в окно (D49, D70).
fn beyond_window(est: Estimate, reserve: u32, n_ctx: u32) -> bool {
    u64::from(est.tokens) + u64::from(reserve) > u64::from(n_ctx)
}

/// Пороги по одной мере `оценка + резерв` (D43, D45, D46), резерв — лимит вывода хода.
/// Порядок (D46): удаление старых результатов при `≥ prune_at × n_ctx` (D45), затем
/// `reset` и эффект `context {pass: 1}`; компакция по новой оценке — при
/// `≥ compact_at × n_ctx` (не больше одной по порогу на границе), при ждущей компакции
/// (D43) или при `оценка + резерв > n_ctx` (аварийная, D49); затем `reset` и
/// `context {pass: 2}`. Пока порог подавлен (D70), по порогу не сжимается, предел —
/// окно. Остался выше окна после обычной компакции — одна аварийная и
/// `context {pass: 3}`. `Some` — выход из границы (пауза, отмена, ошибка модели).
async fn context_thresholds(
    sh: &Shared,
    st: &mut LoopState,
    step: u64,
    mut est: Estimate,
) -> Result<Option<Transition>, LoopError> {
    let deps = &sh.deps;
    let reserve = turn_options(st).max_output_tokens.unwrap_or(0);
    let n_ctx = st.setup.n_ctx;
    let (prune_at, compact_at) = (st.setup.context.prune_at, st.setup.context.compact_at);
    if over_share(est, reserve, prune_at, n_ctx) {
        est = prune_results(deps, st, step, est).await?;
    }
    let pending = st.meter.compact_pending;
    let threshold = over_share(est, reserve, compact_at, n_ctx);
    let window = beyond_window(est, reserve, n_ctx);
    if !(pending || window || (threshold && !st.meter.suppressed)) {
        if threshold {
            tracing::debug!(run = %st.run, step, estimate = est.tokens, reserve,
                "компакция по порогу подавлена: прошлая не опустила оценку ниже порога (D70)");
        }
        return Ok(None);
    }
    let reason = if pending {
        COMPACT_TRUNCATION
    } else if window {
        COMPACT_WINDOW
    } else {
        COMPACT_THRESHOLD
    };
    // Запрос компакции не помещается в окно — сразу аварийный вход (D49).
    let budget = ContextBudget::for_context(n_ctx);
    let emergency = window
        || u64::from(est.tokens)
            + u64::from(rubric_tokens())
            + u64::from(budget.compaction_reserve)
            > u64::from(n_ctx);
    let done_emergency = match run_compaction(sh, st, step, emergency, reason).await? {
        CompactStep::Done { emergency } => emergency,
        CompactStep::Skipped => return Ok(Some(Transition::Goto(BOUNDARY))),
        CompactStep::Exit(t) => return Ok(Some(t)),
    };
    reread_inbox(deps, st, step).await?;
    if pending {
        st.meter.truncation_turn = Some(st.turn);
    }
    let est = context_effect(deps, st, step, 2).await?;
    check_sufficient(deps, st, step, est, reserve).await?;
    if !beyond_window(est, reserve, n_ctx) {
        return Ok(None);
    }
    if done_emergency {
        return window_pause(deps, st, step, est, reserve).await.map(Some);
    }
    // Новая эпоха сама не помещается в окно: одна аварийная компакция с минимальным
    // хвостом (D49, Ruling 1.17, I-1).
    match run_compaction(sh, st, step, true, COMPACT_WINDOW).await? {
        CompactStep::Done { .. } => {}
        CompactStep::Skipped => return Ok(Some(Transition::Goto(BOUNDARY))),
        CompactStep::Exit(t) => return Ok(Some(t)),
    }
    reread_inbox(deps, st, step).await?;
    let est = context_effect(deps, st, step, 3).await?;
    check_sufficient(deps, st, step, est, reserve).await?;
    if beyond_window(est, reserve, n_ctx) {
        return window_pause(deps, st, step, est, reserve).await.map(Some);
    }
    Ok(None)
}

/// После аварийной компакции запрос хода всё ещё выше окна (Ruling 1.17, I-1): пауза
/// `context` без хода — повторять компакцию бесполезно, ход ушёл бы за окно. Событие
/// `compaction_insufficient` уже записано.
async fn window_pause(
    deps: &LoopDeps,
    st: &mut LoopState,
    step: u64,
    est: Estimate,
    reserve: u32,
) -> Result<Transition, LoopError> {
    tracing::warn!(run = %st.run, step, estimate = est.tokens, reserve, n_ctx = st.setup.n_ctx,
        "после аварийной компакции запрос выше окна: run на паузе context");
    clean_pause(deps, st, LoopExit::Paused(pause_reason::CONTEXT)).await
}

/// D70: после компакции `оценка + резерв ≥ compact_at × n_ctx` — компакция бесполезна:
/// `context.alert {kind: "compaction_insufficient"}` и подавление порога; ниже порога —
/// подавление снято.
async fn check_sufficient(
    deps: &LoopDeps,
    st: &mut LoopState,
    step: u64,
    est: Estimate,
    reserve: u32,
) -> Result<(), StoreError> {
    let n_ctx = st.setup.n_ctx;
    let insufficient = over_share(est, reserve, st.setup.context.compact_at, n_ctx);
    st.meter.suppressed = insufficient;
    if insufficient {
        tracing::warn!(run = %st.run, step, estimate = est.tokens, reserve, n_ctx,
            "после компакции оценка выше порога: компакции по порогу подавлены");
        st.last_seq = context_alert(
            &deps.journal,
            st.run,
            json!({
                "kind": "compaction_insufficient",
                "step": step,
                "estimate": est.tokens,
                "reserve": reserve,
                "n_ctx": n_ctx,
            }),
        )
        .await?;
    }
    Ok(())
}

/// Итог [`run_compaction`].
enum CompactStep {
    /// История новой эпохи принята; `emergency` — сводку дал аварийный вход.
    Done { emergency: bool },
    /// Квитанция обрыва компакции прочитана при реплее: команда не применяется второй
    /// раз (D7), история прежняя.
    Skipped,
    /// Выход из узла: пауза, отмена, ошибка модели или эффекта.
    Exit(Transition),
}

/// Компакция (D46–D50, D68): [`compact`], точка сбоя `AfterCompact`, новая история
/// (`epoch += 1`, `meter.reset()`, вводы с новым `at`, `last_next_step`) и событие
/// `context.compacted` с `tainted = true` (CTX-011). Ошибка модели — пауза по D11
/// (`ContextOverflow` после аварийного входа — `context`) с событием
/// `context.alert {kind: "compaction_failed"}`; пауза и отмена во время компакции —
/// как у хода (D26).
async fn run_compaction(
    sh: &Shared,
    st: &mut LoopState,
    step: u64,
    emergency: bool,
    reason: &str,
) -> Result<CompactStep, LoopError> {
    let deps = &sh.deps;
    let r = compact(
        &mut st.ctx,
        &deps.journal,
        deps.model.as_ref(),
        deps.memory.as_ref(),
        CompactInput {
            frozen: &st.frozen,
            frozen_ref: &st.frozen_ref,
            setup: &st.setup,
            conv: &st.conv,
            env: &st.env_text,
            goal: &st.task,
            user_inputs: &st.user_inputs,
            last_next_step: &st.last_next_step,
            emergency,
        },
        st.turn,
        &mut st.sig,
    )
    .await;
    if matches!(r, Ok(_) | Err(TurnError::Aborted { .. }))
        && crashed(deps, CrashPoint::AfterCompact { step })
    {
        return Err(LoopError::Halted);
    }
    match r {
        Ok(c) => {
            st.conv = c.conv;
            st.user_inputs = c.user_inputs;
            st.last_next_step = c.next_step;
            st.epoch += 1;
            st.meter.reset();
            st.meter.compact_pending = false;
            tracing::info!(run = %st.run, step, epoch = st.epoch, reason, emergency = c.emergency,
                before = c.before, after = c.after, tail = c.tail, omitted = c.omitted,
                "история сжата: новая эпоха");
            st.last_seq = append_tainted(
                &deps.journal,
                st.run,
                EventKind::ContextCompacted,
                json!({
                    "epoch": st.epoch,
                    "step": step,
                    "turn": st.turn,
                    "reason": reason,
                    "emergency": c.emergency,
                    "before": c.before,
                    "after": c.after,
                    "tail": c.tail,
                    "omitted": c.omitted,
                }),
            )
            .await?;
            Ok(CompactStep::Done {
                emergency: c.emergency,
            })
        }
        Err(TurnError::Model { error, last_wait }) => {
            tracing::warn!(run = %st.run, step, error = %error, "компакция завершилась ошибкой модели");
            st.last_seq = context_alert(
                &deps.journal,
                st.run,
                json!({
                    "kind": "compaction_failed",
                    "step": step,
                    "class": error.class,
                    "error": error.message,
                }),
            )
            .await?;
            let exit = exit_for_model_error(&error, last_wait.as_ref());
            Ok(CompactStep::Exit(clean_pause(deps, st, exit).await?))
        }
        Err(TurnError::Effect(e)) => {
            st.exit = Some(effect_exit(e)?);
            Ok(CompactStep::Exit(Transition::Done))
        }
        // У компакции детекторов стрима нет: такая квитанция — чужая запись.
        Err(TurnError::Degenerate(d)) => {
            st.exit = Some(effect_exit(EffectError::Failed(format!(
                "обрыв детектором {} у компакции",
                d.as_str()
            )))?);
            Ok(CompactStep::Exit(Transition::Done))
        }
        Err(TurnError::Aborted { signal, source }) => match (signal, source) {
            (Signal::Pause, za_core::EffectSource::Live) => {
                Ok(CompactStep::Exit(command_pause(sh, st).await?))
            }
            (Signal::Cancel, za_core::EffectSource::Live) => {
                Ok(CompactStep::Exit(command_cancel(deps, st).await?))
            }
            // Реплей обрыва (D7) или ввод (компакция его не ждёт): история прежняя.
            _ => {
                tracing::info!(run = %st.run, step, ?signal, ?source,
                    "компакция прервана раньше: при реплее не применяется");
                Ok(CompactStep::Skipped)
            }
        },
        Err(TurnError::Request(e)) => Err(LoopError::Request(e)),
    }
}

/// Удаление старых результатов (D45): новая эпоха истории, `meter.reset()`, эффект
/// `context {pass: 1}` и событие `context.pruned {epoch, count, before, after}`
/// (`before`, `after` — оценки до и после, токены); возвращает новую оценку. Нечего
/// удалять — история, эпоха и оценка прежние, события нет. Файлы выгрузки пишутся вне
/// эффекта (исключение D30: имя и содержимое детерминированы историей); ошибка записи
/// — `warn`, событие `context.alert {kind: "prune_failed", step, error}` и удаление
/// на этой границе пропускается: run продолжается с полной историей.
async fn prune_results(
    deps: &LoopDeps,
    st: &mut LoopState,
    step: u64,
    before: Estimate,
) -> Result<Estimate, LoopError> {
    let pruned = prune(
        &st.conv,
        st.run,
        &st.setup.project_dir,
        PRUNE_KEEP_TURNS,
        PRUNE_MIN_BYTES,
    );
    let (conv, count) = match pruned {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(run = %st.run, step, error = %e,
                "старые результаты не удалены: файл выгрузки не записан");
            // Видно в журнале, а не только в логе: удаление на этой границе не сделано.
            st.last_seq = context_alert(
                &deps.journal,
                st.run,
                json!({ "kind": "prune_failed", "step": step, "error": e.to_string() }),
            )
            .await?;
            return Ok(before);
        }
    };
    if count == 0 {
        return Ok(before);
    }
    st.conv = conv;
    st.epoch += 1;
    st.meter.reset();
    let after = context_effect(deps, st, step, 1).await?;
    tracing::info!(run = %st.run, step, epoch = st.epoch, count,
        before = before.tokens, after = after.tokens, "старые результаты инструментов удалены");
    st.last_seq = append(
        &deps.journal,
        st.run,
        Actor::Runtime,
        EventKind::ContextPruned,
        json!({
            "epoch": st.epoch,
            "count": count,
            "before": before.tokens,
            "after": after.tokens,
        }),
    )
    .await?;
    Ok(after)
}

/// D43: расход хода меньше половины оценки его запроса — сервер, вероятно, срезал
/// начало промпта: `context.alert {kind: "truncation_suspected"}` и компакция на
/// ближайшей границе. Расход неизвестен (`input == 0`) — проверки нет. Повторное
/// подозрение в пределах 2 ходов после компакции по этой причине — `true`: пауза
/// `context` (компакция не помогла, повторять её бессмысленно).
async fn check_truncation(
    deps: &LoopDeps,
    st: &mut LoopState,
    out: &TurnOutput,
) -> Result<bool, StoreError> {
    // Нижняя оценка — без рассуждений прошлых ходов: шаблоны llama.cpp их выбрасывают,
    // и полная оценка дала бы ложное подозрение после длинного рассуждения.
    let Some(estimate) = st.meter.last_floor.or(st.meter.last_estimate) else {
        return Ok(false);
    };
    let input = out.usage.input;
    if !truncation_suspected(estimate, input) {
        return Ok(false);
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
    let repeated = st
        .meter
        .truncation_turn
        .is_some_and(|t| st.turn.saturating_sub(t) <= TRUNCATION_REPEAT_TURNS);
    if repeated {
        tracing::warn!(run = %st.run, turn = st.turn,
            "обрезка повторилась после компакции: run на паузе context");
        // Следующее подозрение после resume — снова сначала компакция.
        st.meter.truncation_turn = None;
        st.meter.compact_pending = false;
        return Ok(true);
    }
    st.meter.compact_pending = true;
    Ok(false)
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

/// `context.alert {kind: "n_ctx_changed", endpoint, from, to}` (D6, D43): resume на
/// эндпоинт с другим окном.
pub(crate) async fn n_ctx_changed(
    j: &Journal,
    run: RunId,
    endpoint: &EndpointId,
    from: u32,
    to: u32,
) -> Result<Seq, StoreError> {
    tracing::info!(run = %run, endpoint = %endpoint.0, from, to, "окно контекста сменилось на resume");
    context_alert(
        j,
        run,
        json!({ "kind": "n_ctx_changed", "endpoint": endpoint.0, "from": from, "to": to }),
    )
    .await
}

/// Событие `context.alert` (D43, D63).
async fn context_alert(j: &Journal, run: RunId, payload: Value) -> Result<Seq, StoreError> {
    append(j, run, Actor::Runtime, EventKind::ContextAlert, payload).await
}

// ---- Узел model ----

/// Узел `model`: ход модели. `ContextOverflow` первой попытки (D49) — исход
/// `overflow`, аварийная компакция и один повтор хода в том же шаге (тот же `turn`,
/// `turn_attempt = 1`, `steps += 1` — D67, D69). Снимка после компакции нет: сбой до
/// конца шага реплеит его со снимка `Model` по квитанциям к той же истории.
///
/// Детекторы стрима (D58, D59): обрыв (`TurnError::Degenerate`) — исход `degenerate` с
/// полем `detector`, `loop.detected {action: "retry"}` и повтор хода в том же шаге с
/// параметрами [`retry_params`] (история та же, `turn` тот же, `turn_attempt` и
/// `steps` растут, D69); второе срабатывание в шаге — `loop.detected {action: "pause"}`
/// и пауза `model_loop`. Реплей получает обрыв из квитанции и повторяет то же решение.
async fn model_node(sh: &Shared, st: &mut LoopState) -> Result<Transition, LoopError> {
    let deps = &sh.deps;
    let mut attempt = 0;
    // Параметры повтора после обрыва детектором (D58, D59): выборка и мышление.
    let mut retry: Option<(Sampling, Option<ReasoningMode>)> = None;
    loop {
        let mut opts = turn_options(st);
        if let Some((sampling, reasoning)) = &retry {
            opts.sampling = sampling.clone();
            if let Some(r) = reasoning {
                opts.reasoning = Some(r.clone());
            }
        }
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
            prompt_version: st.frozen.prompt_version.clone(),
        };
        let mut watch = StreamWatch::new(&call.request.reasoning);
        let r = model_turn_with(
            &mut st.ctx,
            &deps.journal,
            deps.model.as_ref(),
            call.clone(),
            &refs,
            st.turn,
            &mut st.sig,
            Some(&mut watch),
            attempt,
        )
        .await;
        // Квитанция эффекта записана (успех, ошибка модели или обрыв) — точка сбоя D33.
        if matches!(
            r,
            Ok(_)
                | Err(TurnError::Model { .. }
                    | TurnError::Aborted { .. }
                    | TurnError::Degenerate(_))
        ) && crashed(
            deps,
            CrashPoint::AfterModelEffect {
                step: st.ctx.step(),
            },
        ) {
            return Err(LoopError::Halted);
        }
        match r {
            Err(TurnError::Model { error, .. })
                if error.class == ErrorClass::ContextOverflow && attempt == 0 =>
            {
                tracing::warn!(run = %st.run, turn = st.turn, error = %error,
                    "запрос хода не поместился в окно: аварийная компакция и повтор");
                turn_finished(deps, st, "overflow", None).await?;
                let step = st.ctx.step();
                match run_compaction(sh, st, step, true, COMPACT_OVERFLOW).await? {
                    CompactStep::Done { .. } => {}
                    CompactStep::Skipped => return Ok(Transition::Goto(BOUNDARY)),
                    CompactStep::Exit(t) => return Ok(t),
                }
                // Ввод, пришедший во время компакции, — в историю до повтора (Ruling I-2).
                reread_inbox(deps, st, step).await?;
                persist(&store_of(deps), st).await?;
                // Оценка прежнего запроса к новому не относится: проверки обрезки у
                // повтора нет (D43).
                st.meter.last_estimate = None;
                st.meter.last_floor = None;
                st.counters.steps += 1;
                attempt += 1;
            }
            Err(TurnError::Degenerate(d)) => {
                let name = d.as_str();
                turn_finished_with(deps, st, "degenerate", None, Some(name)).await?;
                if retry.is_some() {
                    tracing::warn!(run = %st.run, turn = st.turn, detector = name,
                        "повтор хода снова оборван детектором: пауза model_loop");
                    loop_detected(deps, st, name, "pause").await?;
                    return clean_pause(deps, st, LoopExit::Paused(pause_reason::MODEL_LOOP)).await;
                }
                tracing::warn!(run = %st.run, turn = st.turn, detector = name,
                    "стрим хода оборван детектором: повтор хода");
                loop_detected(deps, st, name, "retry").await?;
                retry = Some(retry_params(d, &opts.sampling));
                // Повтор — тот же ход: `empty_turns` не меняется, шаг бюджета — новый (D69).
                st.counters.steps += 1;
                attempt += 1;
            }
            r => return model_result(sh, st, r, &call).await,
        }
    }
}

/// Итог хода модели: ответ, ошибка модели (D11), ошибка эффекта, обрыв сигналом.
async fn model_result(
    sh: &Shared,
    st: &mut LoopState,
    r: Result<TurnOutput, TurnError>,
    call: &ModelCall,
) -> Result<Transition, LoopError> {
    let deps = &sh.deps;
    match r {
        Ok(out) => model_output(sh, st, out, call).await,
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
        // Обрыв детектором разбирает `model_node` (повтор или пауза) и сюда не передаёт.
        Err(TurnError::Degenerate(d)) => {
            let exit = effect_exit(EffectError::Failed(format!(
                "обрыв детектором {} вне узла model",
                d.as_str()
            )))?;
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
    if check_truncation(deps, st, &out).await? {
        // Ответ по срезанному контексту в историю не принимается (D43).
        turn_finished(deps, st, "error", None).await?;
        return clean_pause(deps, st, LoopExit::Paused(pause_reason::CONTEXT)).await;
    }
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
    st.meter
        .after_reply(st.conv.messages().len(), &out.usage, Some(&rev.assistant));

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
    let mut rev = st
        .reviewed
        .take()
        .ok_or_else(|| LoopError::Graph("узел tools без ответа модели".into()))?;
    // Детектор повтора до пакета (R1-B2 D54): повтор, отказ или блокировка run.
    let screened = screen_calls(sh, st, &mut rev.calls).await?;
    if let Some(d) = screened.block {
        tracing::warn!(run = %st.run, turn = st.turn, detector = d, "детектор повтора: run blocked");
        loop_detected(deps, st, d, "block").await?;
        st.conv.close_interrupted();
        return finish_with(st, RunStatus::Blocked);
    }
    let env = ToolEnv {
        run: st.run,
        project_dir: st.setup.project_dir.clone(),
        mode: st.setup.run_mode,
        parallel: sh.parallel,
        spill: Some(SpillCfg {
            n_ctx: st.setup.n_ctx,
            share: st.setup.context.spill_share,
        }),
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
        executed,
        ..
    } = out;
    // Детекторы после результатов хода (D55, D56) — когда ход доходит до границы.
    let facts = match verdict {
        ToolsVerdict::Next { .. } | ToolsVerdict::Pause => {
            Some(results_facts(sh, &results, &executed, &screened).await?)
        }
        _ => None,
    };
    st.conv.push_results(results)?;
    let reaction = match facts {
        Some(f) => st.detectors.on_results(st.turn, &f, false),
        None => Reaction::None,
    };
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
            if let Some(t) = apply_detected(deps, st, &screened.nudges, reaction).await? {
                return Ok(t);
            }
            Ok(Transition::Goto(BOUNDARY))
        }
        ToolsVerdict::Pause => {
            st.conv.close_interrupted();
            push_nudges(st, &rev.nudges)?;
            if let Some(t) = apply_detected(deps, st, &screened.nudges, reaction).await? {
                return Ok(t);
            }
            command_pause(sh, st).await
        }
    }
}

// ---- Детекторы повтора, цикла и застоя (R1-B2 D54–D56) ----

/// Итог детектора повтора до пакета.
#[derive(Debug, Default)]
struct Screened {
    /// Вызовы под детектором (класс по имени). Вызовы `Control` (`finish`, `ask_user`)
    /// под детектор не идут: повтор вопроса — новый вопрос пользователю.
    subjects: BTreeMap<String, ToolKind>,
    /// Вызовы, перехваченные детектором (повтор, отказ): результат без исполнения.
    intercepted: BTreeSet<String>,
    /// Nudge детектора повтора: после результатов хода.
    nudges: Vec<String>,
    /// Детектор, заблокировавший run.
    block: Option<&'static str>,
}

/// D54: реакции детектора повтора на вызовы, допущенные разбором ответа.
///
/// `ReusePrevious` — прошлый результат из блоба с пометкой `REPEAT_MARK`: успех —
/// `CallVerdict::Reuse`, прошлая ошибка — `CallVerdict::Reject` того же текста
/// (`is_error = true`, Ruling m-1). Блоб адресован содержимым и не меняется, поэтому
/// его чтение при реплее даёт тот же текст (как чтение истории при resume), а не новое
/// внешнее значение (D30, Ruling ревью задачи 6). Блоба нет — отказ `REPEAT_REJECT`.
async fn screen_calls(
    sh: &Shared,
    st: &mut LoopState,
    calls: &mut [PlannedCall],
) -> Result<Screened, LoopError> {
    let mut out = Screened::default();
    let mut idx = Vec::new();
    let mut input = Vec::new();
    for (i, c) in calls.iter().enumerate() {
        if c.verdict != CallVerdict::Run {
            continue;
        }
        let spec = sh.registry.get(&c.call.name).map(|t| t.spec().clone());
        // Неизвестный инструмент не исполняется: он не появление и не мутация.
        let (kind, class) = spec.map_or((ToolKind::Read, EffectClass::ReadOnly), |s| {
            (s.kind, s.class)
        });
        if kind == ToolKind::Control {
            continue;
        }
        idx.push(i);
        out.subjects.insert(c.call.id.clone(), kind);
        input.push((
            c.call.id.clone(),
            c.call.name.clone(),
            c.call.arguments.clone(),
            class,
        ));
    }
    if idx.is_empty() {
        return Ok(out);
    }
    let reactions = st.detectors.on_calls(st.turn, &input);
    let store = store_of(&sh.deps);
    for (k, r) in reactions.into_iter().enumerate() {
        match (r, idx.get(k).copied()) {
            (Reaction::None, _) => {}
            (
                Reaction::ReusePrevious {
                    result, is_error, ..
                },
                Some(i),
            ) => {
                let (verdict, action) = match store.get_blob(&result).await? {
                    Some(raw) => {
                        let text = format!("{REPEAT_MARK}\n{}", String::from_utf8_lossy(&raw));
                        let verdict = if is_error {
                            CallVerdict::Reject(text)
                        } else {
                            CallVerdict::Reuse(text)
                        };
                        (verdict, "reuse")
                    }
                    None => {
                        tracing::warn!(run = %st.run, blob = %result.0,
                            "прошлого результата повтора нет: отказ вместо повтора");
                        (CallVerdict::Reject(REPEAT_REJECT.to_owned()), "reject")
                    }
                };
                calls[i].verdict = verdict;
                out.intercepted.insert(calls[i].call.id.clone());
                loop_detected(&sh.deps, st, detector::REPEAT, action).await?;
            }
            (Reaction::Reject(text), Some(i)) => {
                calls[i].verdict = CallVerdict::Reject(text);
                out.intercepted.insert(calls[i].call.id.clone());
                loop_detected(&sh.deps, st, detector::REPEAT, "reject").await?;
            }
            (Reaction::Block(d), _) => out.block = Some(d),
            (Reaction::Nudge(text), _) => {
                loop_detected(&sh.deps, st, detector::REPEAT, "nudge").await?;
                out.nudges.push(text);
            }
            (other, _) => {
                tracing::error!(run = %st.run, reaction = ?other,
                    "реакция детектора не применяется до пакета");
            }
        }
    }
    Ok(out)
}

/// Факты хода для `on_results` в порядке результатов (Ruling I-1): вызовы под
/// детектором с настоящим результатом — исполненные (`ToolsOutcome::executed`) и
/// перехваченные детектором. Оборванные, отклонённые гейтом или пользователем не
/// входят. Блоб исполненного результата пишется здесь (`put_blob` идемпотентен, адрес
/// — содержимое): он нужен повтору D54 и записан раньше снимка с его хэшем. У
/// перехваченного — только хэш, детектор его фактом не считает.
async fn results_facts(
    sh: &Shared,
    results: &[Part],
    executed: &[String],
    screened: &Screened,
) -> Result<Vec<(String, ToolKind, bool, BlobHash)>, LoopError> {
    let store = store_of(&sh.deps);
    let mut facts = Vec::new();
    for p in results {
        let Part::ToolResult {
            call_id,
            content,
            is_error,
        } = p
        else {
            continue;
        };
        let Some(kind) = screened.subjects.get(call_id) else {
            continue;
        };
        let hash = if screened.intercepted.contains(call_id) {
            BlobHash(blake3::hash(content.as_bytes()).to_hex().to_string())
        } else if executed.contains(call_id) {
            store.put_blob(content.as_bytes()).await?
        } else {
            continue;
        };
        facts.push((call_id.clone(), *kind, *is_error, hash));
    }
    Ok(facts)
}

/// Реакции хода после результатов: nudge повтора (D54), цикл (D55), застой (D56).
/// `Some` — выход вместо границы. Nudge после результатов — только цикла (nudge
/// повтора приходит из `screen_calls`); за ним — рецитация задачи (D55).
async fn apply_detected(
    deps: &LoopDeps,
    st: &mut LoopState,
    repeat_nudges: &[String],
    reaction: Reaction,
) -> Result<Option<Transition>, LoopError> {
    push_nudges(st, repeat_nudges)?;
    match reaction {
        Reaction::None => Ok(None),
        Reaction::Nudge(text) => {
            tracing::info!(run = %st.run, turn = st.turn, "детектор цикла: nudge");
            loop_detected(deps, st, detector::CYCLE, "nudge").await?;
            let recite = text == CYCLE_NUDGE;
            push_nudges(st, &[text])?;
            if recite {
                st.conv.push_user(recite_task(&st.task, st.setup.n_ctx))?;
            }
            Ok(None)
        }
        Reaction::Block(d) => {
            tracing::warn!(run = %st.run, turn = st.turn, detector = d, "детектор: run blocked");
            loop_detected(deps, st, d, "block").await?;
            finish_with(st, RunStatus::Blocked).map(Some)
        }
        other => {
            tracing::error!(run = %st.run, reaction = ?other,
                "реакция детектора не применяется после хода");
            Ok(None)
        }
    }
}

/// `loop.detected {detector, action, turn}` (D63).
async fn loop_detected(
    deps: &LoopDeps,
    st: &mut LoopState,
    detector: &str,
    action: &str,
) -> Result<(), StoreError> {
    st.last_seq = append(
        &deps.journal,
        st.run,
        Actor::Runtime,
        EventKind::LoopDetected,
        json!({ "detector": detector, "action": action, "turn": st.turn }),
    )
    .await?;
    Ok(())
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
    turn_finished_with(deps, st, outcome, rev, None).await
}

/// [`turn_finished`] с полем `detector` у исхода `degenerate` (R1-B2 D69).
async fn turn_finished_with(
    deps: &LoopDeps,
    st: &mut LoopState,
    outcome: &str,
    rev: Option<&Reviewed>,
    detector: Option<&str>,
) -> Result<(), StoreError> {
    let (calls, repairs, rejected) =
        rev.map_or((0, 0, 0), |r| (r.calls.len(), r.repairs, r.rejected));
    let mut payload = json!({
        "turn": st.turn,
        "outcome": outcome,
        "calls": calls,
        "repairs": repairs,
        "rejected": rejected,
    });
    if let Some(d) = detector {
        payload["detector"] = Value::String(d.to_owned());
    }
    st.last_seq = append(
        &deps.journal,
        st.run,
        Actor::Runtime,
        EventKind::TurnFinished,
        payload,
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

/// Событие с `tainted = true` (D63): производный недоверенный ввод — сводка компакции
/// (CTX-011).
async fn append_tainted(
    j: &Journal,
    run: RunId,
    kind: EventKind,
    payload: Value,
) -> Result<Seq, StoreError> {
    j.append(NewEvent {
        run_id: run,
        parent_seq: None,
        actor: Actor::Runtime,
        kind,
        payload,
        tainted: true,
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
