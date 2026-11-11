//! Пакет вызовов инструментов (D14–D17, D25, D30, CORE-104, CORE-105; R1-C1 D77–D100).
//!
//! [`run_tools`] исполняет вызовы одного ответа модели по порядку. Ключ эффекта
//! резервируется на каждый вызов, даже на отклонённый (D15), поэтому ключи при
//! реплее не зависят от исходов. Подряд идущие разрешённые `Read`-вызовы при
//! `parallel` выполняются одновременно в `JoinSet`, любой другой вызов — барьер.
//!
//! **Решение перед каждым вызовом (D98).** Предметы вызова ([`Tool::subjects_in`])
//! разрешаются (`paths::classify`, `paths::resolve` в `spawn_blocking`) непосредственно
//! перед ним, после завершения предыдущих вызовов пакета: ссылка, созданная `bash` в том
//! же пакете, учитывается. Группа одновременных чтений решается целиком перед стартом.
//! Решение принимает движок политики ([`decide`], D80) по состоянию пакета: доверию,
//! грантам и меткам, которые предыдущие вызовы пакета могли изменить.
//!
//! **Записанное решение (D92, ревью задачи 11).** Решение, оставившее след в журнале,
//! при реплее берётся из журнала, а не вычисляется заново:
//! - есть строка эффекта вызова (намерение, с квитанцией или без) — вызов был разрешён
//!   и исполняется (реплей квитанции или повтор по классу) без политики (исключение D30
//!   №6);
//! - есть прерывание доверия `trust-{key}` или одобрения `appr-{key}` — решение берётся
//!   из него (открыто — снова пауза, решено — ответ пользователя), политика не
//!   пересчитывается: конфигурация, гранты или доверие могли измениться до resume.
//!
//! Решение без следа (отказ политики, разрешённый вызов, не дошедший до намерения) ничего
//! не записало и результата в журнал не дало: реплей принимает его заново. Решение пишется
//! и в payload: `tool.started.decision`, причины в payload прерывания.
//!
//! **Слот вызова** (внутри замыкания эффекта, только вживую): `tool.started` с
//! маскированными аргументами; повторное разрешение путей — смена класса даёт ошибку
//! `path changed since planning; call again` (D98); `boundary.outside`; чекпоинт `Edit` и
//! `Exec` и `checkpoint.created` (D89); точка сбоя [`CrashPoint::InTool`] до и после
//! вызова; вызов; постусловия (TOOL-004); `file.changed`; метки происхождения и очистка
//! невидимых символов (D82, SEC-051); маскировка секретов (D90); выгрузка или обрезка по
//! `output_limit` с `digest` (D44, D75, D86); `tool.finished` с `tainted`.
//!
//! Квитанция эффекта — [`ToolOutput`] с признаками пакета ([`Receipt`]); в результат для
//! модели текст идёт через [`escape_untrusted`] (D23, D94).

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio::time::Instant;
use za_core::interrupt::{self, InterruptState};
use za_core::paths::{self, PathError, ResolvedPath, Roots};
use za_core::{
    Access, Approval, CheckpointRequest, Checkpointer, EffectError, EffectSlot, EffectSpec, Gate,
    Interrupt, InterruptKind, Journal, Postcondition, RawPath, RiskClass, RunCtx, Subjects, Tool,
    ToolCx, ToolKind, ToolOutput, ToolRegistry, ToolSpec, gate,
};
use za_llm::{BoxFuture, Part, ToolCall, ToolSchema};
use za_protocol::{
    Actor, EffectClass, EffectKey, EventKind, NewEvent, RunId, RunMode, RunRole, Tier,
    approval_reason, audit_action, canonical_json, mask_json_with, taint_origin,
};
use za_secrets::SecretMasker;
use za_store::{AuditNew, StoreError};

use crate::context::{
    SPILL_NOT_SAVED, SpillCfg, spill, spill_cut, spill_digest, spill_limit_bytes, spill_name,
};
use crate::driver::{CrashHook, CrashPoint, FaultHook, FaultPoint, INJECTED_FAULT};
use crate::policy::{
    Decision, Grant, GrantMode, GrantSource, PolicyCx, PolicySnapshot, covers, decide,
    subject_paths,
};
use crate::prompt::escape_untrusted;
use crate::turn::{CallVerdict, PlannedCall, Signal, Signals};

/// Имя встроенного инструмента завершения (D17).
pub const FINISH_TOOL: &str = "finish";
/// Имя встроенного инструмента вопроса пользователю (D17).
pub const ASK_USER_TOOL: &str = "ask_user";
/// Имя встроенного инструмента запроса гранта (D78); исполняет [`run_tools`].
pub const REQUEST_GRANT_TOOL: &str = "request_grant";
/// Начало пометки недоверенного результата (D82): `[untrusted content: <origin>]`.
pub const UNTRUSTED_MARK: &str = "[untrusted content: ";
/// Синтетический результат `finish` (D17).
pub(crate) const FINISH_OK: &str = "ok";
/// Текст квитанции-отказа при ошибке журнала внутри слота: при реплее он уходит
/// модели, поэтому по-английски и без подробностей (они — в `tracing`).
pub(crate) const JOURNAL_FAILED: &str = "internal error: journal write failed";
/// Квитанция вызова, вывод которого потерян при остановке рантайма во время выгрузки.
pub(crate) const OUTPUT_LOST: &str = "internal error: tool output lost";
/// Результат вызовов пакета после принятого `finish`: они не исполняются.
pub(crate) const AFTER_FINISH: &str = "not executed: run finished by finish";
/// Путь вызова сменил класс между решением и слотом (D98).
pub(crate) const PATH_CHANGED: &str = "path changed since planning; call again";
/// Начало строки о нарушенном постусловии (TOOL-004).
const POSTCONDITION_FAILED: &str = "[postcondition failed: ";
/// Действующее лицо записей аудита пакета.
const AUDIT_ACTOR: &str = "agent";

/// Аргументы `finish` (D17, часть 02, 5.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinishArgs {
    /// `done` | `partial` | `blocked`.
    pub status: String,
    pub result: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub open_issues: Vec<String>,
}

/// Допустимые значения `FinishArgs::status`.
const FINISH_STATUSES: [&str; 3] = ["done", "partial", "blocked"];

/// Итог пакета вызовов.
#[derive(Debug, Default)]
pub struct ToolsOutcome {
    /// `Part::ToolResult` обработанных вызовов по порядку вызовов.
    pub results: Vec<Part>,
    /// Прерывание (одобрение, доверие или `ask_user`), на котором пакет остановился.
    pub suspend: Option<Interrupt>,
    /// Аргументы принятого вызова `finish`.
    pub finish: Option<FinishArgs>,
    /// Input или Pause: не начатые вызовы пропущены (D25).
    pub aborted: bool,
    /// Cancel: JoinSet::abort_all, идущий вызов брошен (D15).
    pub cancelled: bool,
    /// `call_id` вызовов, чей слот исполнен (результат — квитанция эффекта), по порядку
    /// результатов. Отказ разбора, гейта или политики, отказ пользователя, повтор (D54),
    /// вызов после `finish` и не начатый вызов сюда не входят.
    pub executed: Vec<String>,
    /// Новые происхождения недоверенного содержимого (D82): драйвер добавляет их в
    /// `LoopState.taint`.
    pub taint: BTreeSet<String>,
    /// Гранты, выданные в пакете (`request_grant`, D78).
    pub grants: Vec<Grant>,
    /// Решение о доверии к проекту в пакете (D83): `Some(true)` — доверие дано,
    /// `Some(false)` — отказано.
    pub trusted: Option<bool>,
    /// Точка сбоя [`CrashPoint::InTool`] сработала: future слота брошен (D92), драйвер
    /// возвращает `LoopError::Halted`.
    pub halted: bool,
}

/// Набор доверенных файлов проекта для payload прерывания `Trust` (D83): по корню
/// проекта — `{"files": …, "set_hash": …}`. Блокирующая (читает файлы); подаёт бинарник.
pub type TrustSetFn = Arc<dyn Fn(&Path) -> Value + Send + Sync>;

/// Окружение пакета.
#[derive(Clone)]
pub struct ToolEnv {
    pub run: RunId,
    /// Корень проекта run (`ToolCx::project_dir`).
    pub project_dir: PathBuf,
    pub mode: RunMode,
    /// Чтения одновременно: `capabilities(endpoint).parallel_tools` (исключение D30).
    pub parallel: bool,
    /// Выгрузка большого вывода (R1-B2 D44); `None` — без выгрузки, вывод режется по
    /// `ToolSpec.output_limit` (D75).
    pub spill: Option<SpillCfg>,
    /// Снимок политики run (D80, D92).
    pub policy: Arc<PolicySnapshot>,
    /// Канонические корни run (D76).
    pub roots: Arc<Roots>,
    /// Проект доверен (D83).
    pub trusted: bool,
    /// Пользователь отказал проекту в доверии (D83).
    pub trust_refused: bool,
    /// Происхождения недоверенного содержимого run (D82).
    pub taint: BTreeSet<String>,
    /// Гранты run (D78).
    pub grants: Arc<Vec<Grant>>,
    /// Чекпоинт перед `Edit` и `Exec` (D89); `None` — без чекпоинтов.
    pub checkpointer: Option<Arc<dyn Checkpointer>>,
    /// Маскировка секретов (D90); `None` — без маски.
    pub masker: Option<Arc<SecretMasker>>,
    /// Точки сбоя тестов (D33, D92).
    pub crash: Option<CrashHook>,
    /// Разовые сбои журнала тестов (D72).
    pub fault: Option<FaultHook>,
    /// Шаг пакета (точки сбоя).
    pub step: u64,
    /// Набор доверенных файлов для прерывания `Trust`; `None` — пустой набор.
    pub trust_set: Option<TrustSetFn>,
}

impl fmt::Debug for ToolEnv {
    /// Без маскировщика и крюков: значения секретов не печатаются.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolEnv")
            .field("run", &self.run)
            .field("project_dir", &self.project_dir)
            .field("mode", &self.mode)
            .field("parallel", &self.parallel)
            .field("spill", &self.spill)
            .field("trusted", &self.trusted)
            .field("trust_refused", &self.trust_refused)
            .field("taint", &self.taint)
            .field("grants", &self.grants)
            .field("checkpointer", &self.checkpointer.is_some())
            .field("masker", &self.masker.is_some())
            .field("step", &self.step)
            .finish_non_exhaustive()
    }
}

impl ToolEnv {
    /// Окружение с умолчаниями R1-B: проект доверен, политика пустая, без грантов, меток,
    /// чекпоинтов и маски. Корни — [`fallback_roots`] от `project_dir`.
    pub fn new(
        run: RunId,
        project_dir: PathBuf,
        mode: RunMode,
        parallel: bool,
        spill: Option<SpillCfg>,
    ) -> Self {
        let roots = Arc::new(fallback_roots(&project_dir));
        Self {
            run,
            project_dir,
            mode,
            parallel,
            spill,
            policy: Arc::new(PolicySnapshot::default()),
            roots,
            trusted: true,
            trust_refused: false,
            taint: BTreeSet::new(),
            grants: Arc::new(Vec::new()),
            checkpointer: None,
            masker: None,
            crash: None,
            fault: None,
            step: 0,
            trust_set: None,
        }
    }
}

/// Корни без канонизации, когда вызывающий их не задал (`LoopDeps::new`, `ToolEnv::new`):
/// проект — как есть, `ZA_HOME` — `<project>/.zero-agent` (защищённое место, D79): запись
/// туда и так запрещена, ошибка — в безопасную сторону.
pub(crate) fn fallback_roots(project: &Path) -> Roots {
    Roots {
        project: project.to_path_buf(),
        za_home: project.join(".zero-agent"),
    }
}

/// Состояние пакета: доверие, гранты и метки, которые меняют вызовы пакета.
struct BatchState {
    trusted: bool,
    trust_refused: bool,
    taint: BTreeSet<String>,
    grants: Vec<Grant>,
}

/// Квитанция вызова: результат инструмента и признаки пакета. Поля пакета
/// необязательны: квитанции R1-B (`{content, is_error}`) читаются.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Receipt {
    #[serde(flatten)]
    out: ToolOutput,
    /// Происхождения недоверенного содержимого результата (D82).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    origins: Vec<String>,
    /// Выданный грант (`request_grant`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grant: Option<Grant>,
}

/// Выгрузка вывода одного вызова (D44): файл `<project>/.zero-agent/work/spill/{name}.txt`.
struct SpillJob {
    project_dir: PathBuf,
    name: String,
    limit: usize,
}

/// Как укоротить длинный вывод.
enum Cut {
    /// Выгрузка в файл (D44).
    Spill(SpillJob),
    /// Без выгрузки: обрезка по `output_limit` (D75).
    Limit(usize),
    /// Без предела.
    None,
}

/// Вызов инструмента в слоте: всё, что нужно замыканию.
struct ToolJob {
    tool: Arc<dyn Tool>,
    args: Value,
    cx: ToolCx,
    timeout: Duration,
    cut: Cut,
    /// Пути предметов (как в аргументах) и их разрешение при решении (D98).
    raw: Vec<RawPath>,
    planned: Vec<ResolvedPath>,
    subjects: Subjects,
    roots: Arc<Roots>,
    grants: Arc<Vec<Grant>>,
    trusted: bool,
    checkpointer: Option<Arc<dyn Checkpointer>>,
    masker: Option<Arc<SecretMasker>>,
    /// Решение для `tool.started` (D92).
    decision: Value,
}

/// Что делает слот вызова вживую.
enum Action {
    Tool(Box<ToolJob>),
    /// Ответ пользователя на `ask_user`.
    Answer(String),
    /// `finish`: синтетический `ok`.
    Finish,
    /// Одобренный `request_grant` (D78).
    Grant(Grant),
}

/// Подготовленный к запуску слот.
struct Job {
    slot: EffectSlot,
    spec: EffectSpec,
    call_id: String,
    action: Action,
    journal: Journal,
    run: RunId,
    step: u64,
    /// Хук разового сбоя `tool.finished` (тест, R1-C1 D72).
    fault: Option<FaultHook>,
    /// Точка сбоя `InTool` (тест, D92).
    crash: Option<CrashHook>,
    /// Аргументы `finish` принятого вызова.
    finish: Option<FinishArgs>,
}

/// Итог запуска слота.
struct Executed {
    result: Result<Value, EffectError>,
    /// Ошибка журнала внутри замыкания (события `tool.*`): контекст отравляется.
    store: Option<StoreError>,
    /// Сработала точка сбоя `InTool`: future слота брошен.
    halted: bool,
}

/// Итог подготовки вызова.
enum Prep {
    /// Результат без эффекта: отказ разбора, гейта или политики, повтор (D54).
    Done(Part),
    /// Слот запускается.
    Run(Box<Job>),
    /// Нужно решение пользователя.
    Suspend(Interrupt),
}

/// D14–D17, D77–D100. Прерывание: результаты до прерывания в outcome, suspend = Some.
/// Signal::Input/Pause во время пакета: идущий вызов дорабатывает, остальные не
/// начинаются (aborted = true). Signal::Cancel: идущие вызовы брошены, cancelled = true.
/// finish получает результат "ok" (D17).
///
/// Ошибки эффекта, отравляющие контекст (`Store`, `Unknown`, `ReplayMismatch`), и
/// `Poisoned` возвращаются как `Err` после [`RunCtx::observe`]; квитанция-отказ
/// (`Failed`) — результат-ошибка с её текстом.
pub async fn run_tools(
    ctx: &mut RunCtx,
    j: &Journal,
    reg: &ToolRegistry,
    env: &ToolEnv,
    calls: &[PlannedCall],
    sig: &mut Signals,
) -> Result<ToolsOutcome, EffectError> {
    // D15: ключи на все вызовы по порядку, до любых исходов.
    let mut slots = Vec::with_capacity(calls.len());
    for _ in calls {
        slots.push(Some(ctx.reserve()?));
    }
    let mut st = BatchState {
        trusted: env.trusted,
        trust_refused: env.trust_refused,
        taint: env.taint.clone(),
        grants: env.grants.as_ref().clone(),
    };
    let mut out = ToolsOutcome::default();
    batch(ctx, j, reg, env, calls, sig, &mut slots, &mut st, &mut out).await?;
    // Итог состояния пакета для драйвера.
    out.taint = st.taint.difference(&env.taint).cloned().collect();
    out.trusted = if st.trusted && !env.trusted {
        Some(true)
    } else if st.trust_refused && !env.trust_refused {
        Some(false)
    } else {
        None
    };
    Ok(out)
}

/// Вызовы пакета по порядку (тело [`run_tools`]).
#[allow(clippy::too_many_arguments)]
async fn batch(
    ctx: &mut RunCtx,
    j: &Journal,
    reg: &ToolRegistry,
    env: &ToolEnv,
    calls: &[PlannedCall],
    sig: &mut Signals,
    slots: &mut [Option<EffectSlot>],
    st: &mut BatchState,
    out: &mut ToolsOutcome,
) -> Result<(), EffectError> {
    let mut i = 0;
    while i < calls.len() {
        // Сигнал, пришедший между вызовами, останавливает пакет до следующего.
        if let Some(s) = pending(sig).await {
            stop(out, s);
            return Ok(());
        }
        // Группа подряд идущих чтений (D15): решается целиком перед стартом (D98).
        if env.parallel
            && is_read(reg, &calls[i])
            && calls.get(i + 1).is_some_and(|c| is_read(reg, c))
        {
            let mut group = Vec::new();
            let mut rest = None;
            let mut k = i;
            while k < calls.len() && is_read(reg, &calls[k]) {
                match prepare(ctx, j, reg, env, st, slots, k, &calls[k]).await? {
                    Prep::Run(job) => group.push((k, job)),
                    other => {
                        rest = Some(other);
                        break;
                    }
                }
                k += 1;
            }
            if group.len() >= 2 {
                let done = run_group(group, sig, out).await;
                for (k, ex) in done {
                    let part = finish_part(ctx, &calls[k].call, ex, st, out)?;
                    out.executed.push(calls[k].call.id.clone());
                    out.results.push(part);
                }
            } else if let Some((k, job)) = group.pop()
                && !run_single(ctx, &calls[k].call, *job, sig, st, out).await?
            {
                return Ok(());
            }
            if out.cancelled || out.aborted || out.halted {
                return Ok(());
            }
            i = k;
            let Some(p) = rest else { continue };
            if !apply(ctx, &calls[i].call, p, sig, st, out, &calls[i + 1..]).await? {
                return Ok(());
            }
            i += 1;
            continue;
        }
        let p = prepare(ctx, j, reg, env, st, slots, i, &calls[i]).await?;
        if !apply(ctx, &calls[i].call, p, sig, st, out, &calls[i + 1..]).await? {
            return Ok(());
        }
        i += 1;
    }
    Ok(())
}

/// Исход подготовки вызова в outcome; `false` — пакет закончен (прерывание, `finish`,
/// сигнал, точка сбоя).
async fn apply(
    ctx: &mut RunCtx,
    call: &ToolCall,
    p: Prep,
    sig: &mut Signals,
    st: &mut BatchState,
    out: &mut ToolsOutcome,
    rest: &[PlannedCall],
) -> Result<bool, EffectError> {
    match p {
        Prep::Done(part) => {
            out.results.push(part);
            Ok(true)
        }
        Prep::Suspend(int) => {
            out.suspend = Some(int);
            Ok(false)
        }
        Prep::Run(job) => {
            if !run_single(ctx, call, *job, sig, st, out).await? {
                return Ok(false);
            }
            if out.finish.is_some() {
                // `finish` — последнее действие run: остальные вызовы пакета получают
                // результат без исполнения, история остаётся валидной.
                for r in rest {
                    out.results
                        .push(result(&r.call, AFTER_FINISH.to_owned(), true));
                }
                return Ok(false);
            }
            Ok(!out.aborted)
        }
    }
}

/// Один слот под сигналами; `false` — пакет закончен (отмена, точка сбоя).
async fn run_single(
    ctx: &mut RunCtx,
    call: &ToolCall,
    job: Job,
    sig: &mut Signals,
    st: &mut BatchState,
    out: &mut ToolsOutcome,
) -> Result<bool, EffectError> {
    let finish = job.finish.clone();
    let Some(ex) = run_one(job, sig, out).await else {
        return Ok(false);
    };
    if ex.halted {
        out.halted = true;
        return Ok(false);
    }
    let part = finish_part(ctx, call, ex, st, out)?;
    out.executed.push(call.id.clone());
    if let (
        Some(args),
        Part::ToolResult {
            is_error: false, ..
        },
    ) = (finish, &part)
    {
        out.finish = Some(args);
    }
    out.results.push(part);
    Ok(true)
}

/// Вызов — кандидат группы одновременных чтений: исполняемый инструмент вида `Read`.
fn is_read(reg: &ToolRegistry, pc: &PlannedCall) -> bool {
    pc.verdict == CallVerdict::Run
        && !is_runtime_tool(&pc.call.name)
        && reg
            .get(&pc.call.name)
            .is_some_and(|t| t.spec().kind == ToolKind::Read)
}

/// Встроенные инструменты, которые исполняет сам пакет.
fn is_runtime_tool(name: &str) -> bool {
    matches!(name, FINISH_TOOL | ASK_USER_TOOL | REQUEST_GRANT_TOOL)
}

/// Подготовка вызова `i`: записанное решение, иначе политика (D92, D98).
#[allow(clippy::too_many_arguments)]
async fn prepare(
    ctx: &mut RunCtx,
    j: &Journal,
    reg: &ToolRegistry,
    env: &ToolEnv,
    st: &mut BatchState,
    slots: &mut [Option<EffectSlot>],
    i: usize,
    pc: &PlannedCall,
) -> Result<Prep, EffectError> {
    let call = &pc.call;
    match &pc.verdict {
        CallVerdict::Reject(text) => return Ok(Prep::Done(result(call, text.clone(), true))),
        // Повтор не исполняется и не проходит гейт: ничего не делается (D54).
        CallVerdict::Reuse(text) => return Ok(Prep::Done(result(call, text.clone(), false))),
        CallVerdict::Run => {}
    }
    let Some(tool) = reg.get(&call.name) else {
        return Ok(Prep::Done(result(
            call,
            unknown_tool_text(&call.name),
            true,
        )));
    };
    let key = slot_ref(slots, i)?.key().clone();
    let run = |slots: &mut [Option<EffectSlot>], action, finish| -> Result<Prep, EffectError> {
        let slot = take(slots, i)?;
        Ok(Prep::Run(Box::new(job(slot, call, action, j, env, finish))))
    };
    match call.name.as_str() {
        FINISH_TOOL | ASK_USER_TOOL => {
            if let Gate::Deny(reason) = gate(env.mode, tool.spec()) {
                return Ok(Prep::Done(result(call, denied_text(reason), true)));
            }
            if call.name == FINISH_TOOL {
                return match finish_args(&call.arguments) {
                    Ok(a) => run(slots, Action::Finish, Some(a)),
                    Err(e) => Ok(Prep::Done(result(call, finish_invalid_text(&e), true))),
                };
            }
            let int = Interrupt::for_effect(
                InterruptKind::Input,
                &key,
                json!({
                    "call_id": call.id,
                    "question": call.arguments.get("question").cloned().unwrap_or(Value::Null),
                    "options": call.arguments.get("options").cloned().unwrap_or(json!([])),
                }),
            );
            return match decision(ctx, &int).await? {
                None => Ok(Prep::Suspend(int)),
                Some(v) => run(slots, Action::Answer(answer(&v)), None),
            };
        }
        REQUEST_GRANT_TOOL => return prepare_grant(ctx, env, call, &key, slots, i, j).await,
        _ => {}
    }

    // Записанное решение о доверии (D83, D92): ответ на прерывание этого вызова.
    if let Some(p) = recorded_trust(ctx, &key, st).await? {
        return Ok(p);
    }
    // Есть строка эффекта: вызов разрешён раньше — без политики (D92, исключение 6).
    let row = ctx.store().effect(ctx.run_id(), &key).await;
    let row = observe_store(ctx, row)?;
    let subjects = tool.subjects_in(&env.roots.project, &call.arguments);
    let raw = subject_paths(&subjects);
    if let Some(row) = row {
        let planned = if row.result.is_some() {
            Vec::new()
        } else {
            resolve_all(&env.roots, &raw).await.unwrap_or_default()
        };
        let decision = json!({"action": "recorded"});
        let action = tool_action(&tool, call, env, st, &key, raw, planned, subjects, decision);
        return run(slots, action, None);
    }
    let planned = match resolve_all(&env.roots, &raw).await {
        Ok(p) => p,
        Err(e) => return Ok(Prep::Done(result(call, e.to_string(), true))),
    };
    // Записанное одобрение (D92): решение пользователя, политика не пересчитывается.
    let appr = Interrupt::for_effect(InterruptKind::Approval, &key, Value::Null).id;
    let row = ctx.store().interrupt(&appr).await;
    let decided = match observe_store(ctx, row)? {
        Some(row) => match row.response {
            Some(v) if approved(&v) => {
                // Одобрен вызов с путями тех классов, что видел пользователь (D77, D98):
                // путь, сменивший класс до ответа (ссылка наружу), — повторить вызов.
                let seen = row.payload.as_ref().and_then(|p| p.get("paths"));
                if seen.is_some_and(|s| class_of(s) != class_of(&path_classes(&raw, &planned))) {
                    tracing::warn!(id = %appr, "путь одобренного вызова сменил класс");
                    return Ok(Prep::Done(result(call, PATH_CHANGED.to_owned(), true)));
                }
                json!({"action": "approved", "interrupt": appr})
            }
            Some(v) => return Ok(Prep::Done(result(call, denied_by_user(&v), true))),
            None => {
                return Ok(Prep::Suspend(Interrupt {
                    id: row.id,
                    kind: InterruptKind::Approval,
                    payload: row.payload.unwrap_or(Value::Null),
                }));
            }
        },
        None => {
            let cx = PolicyCx {
                policy: &env.policy,
                mode: env.mode,
                roots: &env.roots,
                trusted: st.trusted,
                trust_refused: st.trust_refused,
                taint: &st.taint,
                grants: &st.grants,
            };
            match decide(&cx, tool.spec(), &subjects, &planned) {
                Decision::Allow => json!({"action": "allow"}),
                Decision::Deny(reason) => {
                    return Ok(Prep::Done(result(call, denied_text(reason), true)));
                }
                Decision::Trust => {
                    let int = trust_interrupt(env, &key).await?;
                    interrupt_open(ctx, &int).await?;
                    return Ok(Prep::Suspend(int));
                }
                Decision::Ask { reasons } => {
                    let classes = path_classes(&raw, &planned);
                    let int = approval_interrupt(env, call, &key, &subjects, reasons, classes);
                    interrupt_open(ctx, &int).await?;
                    return Ok(Prep::Suspend(int));
                }
            }
        }
    };
    let action = tool_action(&tool, call, env, st, &key, raw, planned, subjects, decided);
    run(slots, action, None)
}

/// Записанный ответ на прерывание доверия вызова (D83, D92): `Some` — исход вызова
/// (пауза или отказ), `None` — решать дальше (доверие дано или прерывания нет). Ответ
/// `{"superseded_by": id}` — ждать прерывание с этим id.
async fn recorded_trust(
    ctx: &mut RunCtx,
    key: &EffectKey,
    st: &mut BatchState,
) -> Result<Option<Prep>, EffectError> {
    let mut id = Interrupt::for_effect(InterruptKind::Trust, key, Value::Null).id;
    // Цепочка замен конечна: каждая ссылка — новое прерывание; предел — от зацикливания.
    for _ in 0..16 {
        let row = ctx.store().interrupt(&id).await;
        let Some(row) = observe_store(ctx, row)? else {
            return Ok(None);
        };
        let Some(v) = row.response else {
            return Ok(Some(Prep::Suspend(Interrupt {
                id: row.id,
                kind: InterruptKind::Trust,
                payload: row.payload.unwrap_or(Value::Null),
            })));
        };
        if let Some(next) = v.get("superseded_by").and_then(Value::as_str) {
            id = next.to_owned();
            continue;
        }
        if v.get("trusted").and_then(Value::as_bool) == Some(true) {
            if !st.trusted {
                st.trusted = true;
                st.trust_refused = false;
                st.taint.remove(taint_origin::UNTRUSTED_PROJECT);
            }
            return Ok(None);
        }
        st.trust_refused = true;
        return Ok(None);
    }
    tracing::error!(id = %id, "цепочка замен прерывания доверия слишком длинная");
    Err(EffectError::Store(StoreError::Corrupt(format!(
        "прерывание {id}: цепочка superseded_by"
    ))))
}

/// `request_grant` (D78): путь разрешается, прерывание `appr-{key}` с причиной `grant`.
async fn prepare_grant(
    ctx: &mut RunCtx,
    env: &ToolEnv,
    call: &ToolCall,
    key: &EffectKey,
    slots: &mut [Option<EffectSlot>],
    i: usize,
    j: &Journal,
) -> Result<Prep, EffectError> {
    let arg = call.arguments.get("path").and_then(Value::as_str);
    let mode = match call.arguments.get("mode").and_then(Value::as_str) {
        None | Some("ro") => GrantMode::Ro,
        Some("rw") => GrantMode::Rw,
        Some(m) => {
            return Ok(Prep::Done(result(call, grant_mode_text(m), true)));
        }
    };
    let reason = call
        .arguments
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let Some(arg) = arg else {
        return Ok(Prep::Done(result(call, GRANT_NO_PATH.to_owned(), true)));
    };
    let raw = vec![RawPath {
        arg: arg.to_owned(),
        access: Access::Read,
    }];
    let path = match resolve_all(&env.roots, &raw).await {
        Ok(mut p) => p.remove(0).path,
        Err(e) => return Ok(Prep::Done(result(call, e.to_string(), true))),
    };
    let grant = Grant {
        path,
        mode,
        source: GrantSource::Request,
        reason,
    };
    let int = Interrupt::for_effect(
        InterruptKind::Approval,
        key,
        json!({
            "call_id": call.id,
            "name": REQUEST_GRANT_TOOL,
            "grant": {
                "path": grant.path.to_string_lossy(),
                "mode": grant.mode,
                "reason": mask_text(env.masker.as_deref(), &grant.reason),
            },
            "reasons": [approval_reason::GRANT],
        }),
    );
    match decision(ctx, &int).await? {
        None => Ok(Prep::Suspend(int)),
        Some(v) if approved(&v) => {
            let slot = take(slots, i)?;
            Ok(Prep::Run(Box::new(job(
                slot,
                call,
                Action::Grant(grant),
                j,
                env,
                None,
            ))))
        }
        Some(v) => Ok(Prep::Done(result(call, denied_by_user(&v), true))),
    }
}

/// Прерывание одобрения (D90, 3.4): `{call_id, name, arguments (маска), reasons, paths,
/// irreversible?}`; `paths` — пути вызова (D77); порядок полей — канонический.
fn approval_interrupt(
    env: &ToolEnv,
    call: &ToolCall,
    key: &EffectKey,
    s: &Subjects,
    reasons: Vec<&'static str>,
    paths: Value,
) -> Interrupt {
    let mut payload = json!({
        "call_id": call.id,
        "name": call.name,
        "arguments": mask_value(env.masker.as_deref(), &call.arguments),
        "reasons": reasons,
        "paths": paths,
    });
    if !s.irreversible.is_empty()
        && let Some(o) = payload.as_object_mut()
    {
        o.insert("irreversible".into(), json!(s.irreversible));
    }
    Interrupt::for_effect(InterruptKind::Approval, key, payload)
}

/// Пути вызова для payload прерывания (D77): `[{path, access, inside, protected}]` —
/// по ним одобрение сверяется с путями в момент исполнения (D98).
fn path_classes(raw: &[RawPath], planned: &[ResolvedPath]) -> Value {
    Value::Array(
        raw.iter()
            .zip(planned)
            .map(|(r, p)| {
                json!({
                    "path": p.path.to_string_lossy(),
                    "access": r.access,
                    "inside": p.inside,
                    "protected": p.protected,
                })
            })
            .collect(),
    )
}

/// Классы путей payload (`inside`, `protected`) по порядку: сравнение одобрения с
/// текущим разрешением (D98) — без самих строк путей (регистр с диска может меняться).
fn class_of(paths: &Value) -> Vec<(Value, Value)> {
    paths
        .as_array()
        .map(|a| {
            a.iter()
                .map(|p| (p["inside"].clone(), p["protected"].clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Прерывание доверия (D83): `{project, files, set_hash}`.
async fn trust_interrupt(env: &ToolEnv, key: &EffectKey) -> Result<Interrupt, EffectError> {
    let project = env.roots.project.clone();
    let set = match env.trust_set.clone() {
        Some(f) => {
            let p = project.clone();
            match tokio::task::spawn_blocking(move || f(&p)).await {
                Ok(v) => v,
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                Err(e) => {
                    tracing::error!(error = %e, "набор доверия не посчитан");
                    json!({})
                }
            }
        }
        None => json!({}),
    };
    let payload = json!({
        "project": project.to_string_lossy(),
        "files": set.get("files").cloned().unwrap_or_else(|| json!({})),
        "set_hash": set.get("set_hash").cloned().unwrap_or_else(|| json!("")),
    });
    Ok(Interrupt::for_effect(InterruptKind::Trust, key, payload))
}

/// Открыть прерывание; ошибка журнала отравляет контекст.
async fn interrupt_open(ctx: &mut RunCtx, int: &Interrupt) -> Result<(), EffectError> {
    let r = interrupt::open(ctx.store(), ctx.run_id(), int)
        .await
        .map_err(EffectError::Store);
    ctx.observe(&r);
    if r.is_ok() {
        tracing::info!(id = %int.id, kind = int.kind.as_str(), "пакет ждёт решения пользователя");
    }
    r
}

/// Ошибка журнала вне слота отравляет контекст, как ошибка слота.
fn observe_store<T>(ctx: &mut RunCtx, r: Result<T, StoreError>) -> Result<T, EffectError> {
    let r = r.map_err(EffectError::Store);
    if let Err(e) = &r {
        ctx.observe::<()>(&Err(e.clone()));
    }
    r
}

/// Разрешение путей предметов (D76, D98) в `spawn_blocking`; первый отказ — ошибка.
async fn resolve_all(roots: &Arc<Roots>, raw: &[RawPath]) -> Result<Vec<ResolvedPath>, PathError> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let roots = Arc::clone(roots);
    let raw = raw.to_vec();
    let work = tokio::task::spawn_blocking(move || {
        raw.iter()
            .map(|r| {
                let lex = paths::classify(&roots.project, &r.arg)?;
                paths::resolve(&roots, &lex, r.access)
            })
            .collect::<Result<Vec<_>, _>>()
    });
    match work.await {
        Ok(r) => r,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(PathError::Invalid(e.to_string())),
    }
}

/// Ответ одобрения: `{"approved": true}`.
fn approved(v: &Value) -> bool {
    v.get("approved").and_then(Value::as_bool) == Some(true)
}

fn finish_args(v: &Value) -> Result<FinishArgs, String> {
    let a: FinishArgs = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
    if !FINISH_STATUSES.contains(&a.status.as_str()) {
        return Err(finish_status_text(&a.status));
    }
    Ok(a)
}

/// Результат вызова неизвестного инструмента; имя экранируется.
fn unknown_tool_text(name: &str) -> String {
    format!("unknown tool \"{}\"", escape_untrusted(name))
}

/// Результат вызова, запрещённого режимом run (D14) или политикой (D80).
pub(crate) fn denied_text(reason: &str) -> String {
    format!("denied: {reason}")
}

/// Результат `finish` с неверными аргументами; текст ошибки экранируется.
fn finish_invalid_text(e: &str) -> String {
    format!(
        "invalid arguments for \"{FINISH_TOOL}\": {}",
        escape_untrusted(e)
    )
}

/// Ошибка статуса `finish` (экранируется в [`finish_invalid_text`]).
fn finish_status_text(status: &str) -> String {
    format!("status must be one of done, partial, blocked, got \"{status}\"")
}

/// Результат прямого вызова встроенного инструмента.
fn runtime_only_text(name: &str) -> String {
    format!("{name} is handled by the runtime and cannot be called directly")
}

/// Результат одобренного `request_grant` (D78).
fn granted_text(g: &Grant) -> String {
    let mode = match g.mode {
        GrantMode::Ro => "ro",
        GrantMode::Rw => "rw",
    };
    format!("granted: {mode} access to {}", g.path.display())
}

/// `request_grant` без пути.
const GRANT_NO_PATH: &str = "invalid arguments for \"request_grant\": path is required";

/// `request_grant` с неизвестным режимом; значение экранирует `finish_part`.
fn grant_mode_text(m: &str) -> String {
    format!("invalid arguments for \"{REQUEST_GRANT_TOOL}\": mode must be ro or rw, got \"{m}\"")
}

/// Пометка недоверенного результата (D82).
fn untrusted_line(origins: &[String]) -> String {
    format!("{UNTRUSTED_MARK}{}]", origins.join(", "))
}

/// Строка о нарушенном постусловии (TOOL-004).
fn postcondition_text(what: &str) -> String {
    format!("{POSTCONDITION_FAILED}{what}]")
}

/// Образцы текстов для модели, собираемых `format!`, с фиксированными аргументами
/// (PRM-010, M-1 ревью задачи 5): правка шаблона меняет образец и хэш промптов.
pub(crate) fn text_samples() -> Vec<(&'static str, String)> {
    // Причины запрета режимом — из таблицы гейта za-core для инструмента правки.
    let edit = builtin_tools().into_iter().next().map(|t| ToolSpec {
        kind: ToolKind::Edit,
        approval: Approval::Mode,
        ..t.spec().clone()
    });
    let deny = |mode: RunMode| match edit.as_ref().map(|spec| gate(mode, spec)) {
        Some(Gate::Deny(reason)) => denied_text(reason),
        _ => String::new(),
    };
    let (read_only, plan) = (deny(RunMode::ReadOnly), deny(RunMode::Plan));
    let grant = Grant {
        path: PathBuf::from("/data"),
        mode: GrantMode::Rw,
        source: GrantSource::Request,
        reason: String::new(),
    };
    vec![
        (
            "runtime/tools.denied_by_user",
            denied_by_user(&json!({"note": "n"})),
        ),
        (
            "runtime/tools.denied_by_user_bare",
            denied_by_user(&json!({})),
        ),
        ("runtime/tools.denied_plan", plan),
        ("runtime/tools.denied_read_only", read_only),
        (
            "runtime/tools.finish_invalid",
            finish_invalid_text(&finish_status_text("s")),
        ),
        ("runtime/tools.grant_mode", grant_mode_text("x")),
        ("runtime/tools.grant_no_path", GRANT_NO_PATH.to_owned()),
        ("runtime/tools.granted", granted_text(&grant)),
        ("runtime/tools.path_changed", PATH_CHANGED.to_owned()),
        ("runtime/tools.postcondition", postcondition_text("p")),
        ("runtime/tools.runtime_only", runtime_only_text("t")),
        (
            "runtime/tools.timeout",
            timeout_text(Duration::from_millis(1500)),
        ),
        ("runtime/tools.unknown_tool", unknown_tool_text("t")),
        (
            "runtime/tools.untrusted",
            untrusted_line(&[taint_origin::OUTSIDE_PROJECT.to_owned()]),
        ),
    ]
}

fn slot_ref(slots: &[Option<EffectSlot>], i: usize) -> Result<&EffectSlot, EffectError> {
    slots
        .get(i)
        .and_then(Option::as_ref)
        .ok_or(EffectError::Poisoned)
}

/// Слот вызова; каждый берётся один раз (иначе — нарушенный инвариант пакета).
fn take(slots: &mut [Option<EffectSlot>], i: usize) -> Result<EffectSlot, EffectError> {
    slots.get_mut(i).and_then(Option::take).ok_or_else(|| {
        tracing::error!(index = i, "слот вызова уже использован");
        EffectError::Poisoned
    })
}

#[allow(clippy::too_many_arguments)]
fn tool_action(
    tool: &Arc<dyn Tool>,
    call: &ToolCall,
    env: &ToolEnv,
    st: &BatchState,
    key: &EffectKey,
    raw: Vec<RawPath>,
    planned: Vec<ResolvedPath>,
    subjects: Subjects,
    decision: Value,
) -> Action {
    let spec = tool.spec();
    let cut = match (env.spill, spec.output_limit) {
        (Some(c), _) => Cut::Spill(SpillJob {
            project_dir: env.project_dir.clone(),
            name: spill_name(key),
            limit: spill_limit_bytes(c.n_ctx, c.share, spec.output_limit),
        }),
        (None, Some(n)) => Cut::Limit(n),
        (None, None) => Cut::None,
    };
    Action::Tool(Box::new(ToolJob {
        tool: Arc::clone(tool),
        args: call.arguments.clone(),
        cx: ToolCx {
            run: env.run,
            project_dir: env.project_dir.clone(),
            call_id: call.id.clone(),
            effect_key: key.clone(),
            paths: planned.clone(),
        },
        timeout: spec.timeout,
        cut,
        raw,
        planned,
        subjects,
        roots: Arc::clone(&env.roots),
        grants: Arc::new(st.grants.clone()),
        trusted: st.trusted,
        checkpointer: env.checkpointer.clone(),
        masker: env.masker.clone(),
        decision,
    }))
}

fn job(
    slot: EffectSlot,
    call: &ToolCall,
    action: Action,
    j: &Journal,
    env: &ToolEnv,
    finish: Option<FinishArgs>,
) -> Job {
    let class = match &action {
        Action::Tool(t) => t.tool.spec().class,
        Action::Answer(_) | Action::Finish | Action::Grant(_) => EffectClass::Pure,
    };
    Job {
        slot,
        spec: EffectSpec {
            class,
            name: call.name.clone(),
            args: call.arguments.clone(),
        },
        call_id: call.id.clone(),
        action,
        journal: j.clone(),
        run: env.run,
        step: env.step,
        fault: env.fault.clone(),
        crash: env.crash.clone(),
        finish,
    }
}

/// Записанное решение по прерыванию; нет решения — открыть и вернуть `None`.
/// Чтение вне эффекта — исключение D30.
async fn decision(ctx: &mut RunCtx, int: &Interrupt) -> Result<Option<Value>, EffectError> {
    let r = read_or_open(ctx, int).await.map_err(EffectError::Store);
    // Ошибка журнала отравляет контекст, как ошибка слота.
    ctx.observe(&r);
    r
}

async fn read_or_open(ctx: &RunCtx, int: &Interrupt) -> Result<Option<Value>, StoreError> {
    let store = ctx.store();
    match interrupt::state(store, &int.id).await? {
        Some(InterruptState::Resolved(v)) => Ok(Some(v)),
        Some(InterruptState::Open) | None => {
            interrupt::open(store, ctx.run_id(), int).await?;
            tracing::info!(id = %int.id, kind = int.kind.as_str(), "пакет ждёт решения пользователя");
            Ok(None)
        }
    }
}

/// Результат отказа пользователя (D16): `denied by user: {note}`, заметка экранируется.
fn denied_by_user(v: &Value) -> String {
    match v.get("note").and_then(Value::as_str) {
        Some(note) if !note.is_empty() => format!("denied by user: {}", escape_untrusted(note)),
        _ => "denied by user".to_owned(),
    }
}

/// Ответ пользователя на `ask_user` (D16: `{"answer": str}`); иная форма — канонический JSON.
fn answer(v: &Value) -> String {
    match v.get("answer").and_then(Value::as_str) {
        Some(a) => a.to_owned(),
        None => {
            tracing::warn!("ответ ask_user без строкового answer");
            canonical_json(v)
        }
    }
}

fn result(call: &ToolCall, content: String, is_error: bool) -> Part {
    Part::ToolResult {
        call_id: call.id.clone(),
        content,
        is_error,
    }
}

/// Итог слота в результат для модели; ошибки эффекта, кроме `Failed`, — `Err`. Признаки
/// квитанции (происхождения, грант) — в состояние пакета и outcome, вживую и при реплее.
fn finish_part(
    ctx: &mut RunCtx,
    call: &ToolCall,
    ex: Executed,
    st: &mut BatchState,
    out: &mut ToolsOutcome,
) -> Result<Part, EffectError> {
    ctx.observe(&ex.result);
    if let Some(e) = ex.store {
        let err = EffectError::Store(e);
        ctx.observe::<()>(&Err(err.clone()));
        return Err(err);
    }
    match ex.result {
        Ok(v) => Ok(match serde_json::from_value::<Receipt>(v.clone()) {
            Ok(r) => {
                st.taint.extend(r.origins);
                if let Some(g) = r.grant {
                    st.grants.push(g.clone());
                    out.grants.push(g);
                }
                result(call, escape_untrusted(&r.out.content), r.out.is_error)
            }
            // Значение другой формы — решение `unknown_outcome` (D16).
            Err(_) => result(call, escape_untrusted(&canonical_json(&v)), false),
        }),
        Err(EffectError::Failed(m)) => Ok(result(call, escape_untrusted(&m), true)),
        Err(e) => Err(e),
    }
}

/// Сигнал, уже ждущий в канале, без ожидания.
async fn pending(sig: &mut Signals) -> Option<Signal> {
    tokio::select! {
        biased;
        s = sig.recv() => Some(s),
        () = std::future::ready(()) => None,
    }
}

fn stop(out: &mut ToolsOutcome, s: Signal) {
    match s {
        Signal::Cancel => out.cancelled = true,
        Signal::Input | Signal::Pause => out.aborted = true,
    }
}

/// Один вызов под сигналами: Input/Pause — дождаться, Cancel — бросить (`None`).
async fn run_one(jb: Job, sig: &mut Signals, out: &mut ToolsOutcome) -> Option<Executed> {
    let mut fut = pin!(execute(jb));
    loop {
        tokio::select! {
            ex = &mut fut => return Some(ex),
            s = sig.recv() => {
                stop(out, s);
                if out.cancelled {
                    tracing::info!("отмена: идущий вызов инструмента брошен");
                    return None;
                }
            }
        }
    }
}

/// Группа одновременных чтений; итоги по индексу вызова. Cancel — `abort_all`,
/// в итог идут только завершённые. Точка сбоя в любом чтении — группа брошена.
async fn run_group(
    jobs: Vec<(usize, Box<Job>)>,
    sig: &mut Signals,
    out: &mut ToolsOutcome,
) -> Vec<(usize, Executed)> {
    let mut set = JoinSet::new();
    for (k, jb) in jobs {
        set.spawn(async move { (k, execute(*jb).await) });
    }
    let mut done = Vec::new();
    loop {
        tokio::select! {
            r = set.join_next() => match r {
                None => break,
                Some(Ok((_, ex))) if ex.halted => {
                    out.halted = true;
                    set.abort_all();
                    return Vec::new();
                }
                Some(Ok(item)) => done.push(item),
                Some(Err(e)) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                Some(Err(e)) => tracing::warn!(error = %e, "задача чтения прервана"),
            },
            s = sig.recv() => {
                stop(out, s);
                if out.cancelled {
                    set.abort_all();
                    tracing::info!("отмена: идущие чтения брошены");
                    break;
                }
            }
        }
    }
    done.sort_by_key(|(k, _)| *k);
    done
}

/// Запись события вызова.
fn event(run: RunId, kind: EventKind, actor: Actor, payload: Value, tainted: bool) -> NewEvent {
    NewEvent {
        run_id: run,
        parent_seq: None,
        actor,
        kind,
        payload,
        tainted,
    }
}

/// Протокол эффекта слота; вживую — события `tool.*` и сам вызов. Точка сбоя `InTool`
/// бросает future слота (D92): `execute` ждёт в `select!` замыкание и сигнал остановки.
/// Future — `Send + 'static` (годится для `JoinSet`): `Job` владеет всем нужным.
async fn execute(jb: Job) -> Executed {
    let Job {
        slot,
        spec,
        call_id,
        action,
        journal,
        run,
        step,
        fault,
        crash,
        finish: _,
    } = jb;
    let key = slot.key().0.clone();
    let name = spec.name.clone();
    let (halt_tx, mut halt_rx) = oneshot::channel::<()>();
    let mut failure: Option<StoreError> = None;
    let fref = &mut failure;
    let live = Live {
        journal,
        run,
        step,
        call_id,
        name,
        key,
        fault,
        crash,
    };
    let result = {
        let fut = slot.run(spec, move || async move {
            live.run(action, fref, halt_tx).await
        });
        let mut fut = pin!(fut);
        tokio::select! {
            r = &mut fut => r,
            Ok(()) = &mut halt_rx => {
                tracing::warn!("точка сбоя InTool: слот вызова брошен");
                return Executed { result: Err(EffectError::Poisoned), store: None, halted: true };
            }
        }
    };
    Executed {
        result,
        store: failure,
        halted: false,
    }
}

/// Живое исполнение слота.
struct Live {
    journal: Journal,
    run: RunId,
    step: u64,
    call_id: String,
    name: String,
    key: String,
    fault: Option<FaultHook>,
    crash: Option<CrashHook>,
}

impl Live {
    async fn append(
        &self,
        kind: EventKind,
        actor: Actor,
        payload: Value,
        tainted: bool,
        fref: &mut Option<StoreError>,
    ) -> bool {
        let what = kind.as_str().to_owned();
        match self
            .journal
            .append(event(self.run, kind, actor, payload, tainted))
            .await
        {
            Ok(_) => true,
            Err(e) => {
                tracing::error!(key = %self.key, kind = %what, error = %e, "событие вызова не записано");
                if fref.is_none() {
                    *fref = Some(e);
                }
                false
            }
        }
    }

    /// Точка сбоя `InTool`: сообщить `execute` и не продолжать.
    async fn crash_at(&self, after_call: bool, halt: &mut Option<oneshot::Sender<()>>) {
        let p = CrashPoint::InTool {
            step: self.step,
            after_call,
        };
        if self.crash.as_ref().is_some_and(|h| h(p))
            && let Some(tx) = halt.take()
        {
            let _ = tx.send(());
            std::future::pending::<()>().await;
        }
    }

    async fn run(
        self,
        action: Action,
        fref: &mut Option<StoreError>,
        halt: oneshot::Sender<()>,
    ) -> Result<Value, String> {
        let started = Instant::now();
        let mut halt = Some(halt);
        let (args, decision) = match &action {
            Action::Tool(t) => (mask_value(t.masker.as_deref(), &t.args), t.decision.clone()),
            _ => (Value::Null, Value::Null),
        };
        let mut payload =
            json!({ "call_id": self.call_id, "name": self.name, "effect_key": self.key });
        if let Some(o) = payload.as_object_mut()
            && !args.is_null()
        {
            o.insert("arguments".into(), args);
            o.insert("decision".into(), decision);
        }
        if !self
            .append(EventKind::ToolStarted, Actor::Runtime, payload, false, fref)
            .await
        {
            tracing::error!(key = %self.key, "tool.started не записано, инструмент не вызван");
            return Err(JOURNAL_FAILED.to_owned());
        }
        let receipt = match action {
            Action::Tool(t) => self.tool(*t, fref, &mut halt).await,
            Action::Answer(a) => Receipt {
                out: ToolOutput::ok(a),
                ..Receipt::default()
            },
            Action::Finish => Receipt {
                out: ToolOutput::ok(FINISH_OK),
                ..Receipt::default()
            },
            Action::Grant(g) => self.grant(g, fref).await,
        };
        let tainted = !receipt.origins.is_empty();
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        // Инструмент уже отработал: квитанция фиксирует его итог, а ошибка
        // журнала отравляет контекст.
        let injected = self
            .fault
            .as_ref()
            .is_some_and(|h| h(FaultPoint::BeforeToolFinished { step: self.step }));
        if injected {
            // Тест (D72): ошибка записи `tool.finished`, как настоящая.
            tracing::error!(key = %self.key, "tool.finished не записано");
            *fref = Some(StoreError::Io(INJECTED_FAULT.into()));
        } else {
            self.append(
                EventKind::ToolFinished,
                Actor::Tool,
                json!({ "call_id": self.call_id, "is_error": receipt.out.is_error,
                    "duration_ms": duration_ms, "tainted": tainted }),
                tainted,
                fref,
            )
            .await;
        }
        serde_json::to_value(&receipt).map_err(|e| {
            tracing::error!(key = %self.key, error = %e, "итог инструмента не сериализуется");
            JOURNAL_FAILED.to_owned()
        })
    }

    /// Одобренный грант (D78): `grant.added` и запись аудита.
    async fn grant(&self, g: Grant, fref: &mut Option<StoreError>) -> Receipt {
        let payload = json!({
            "path": g.path.to_string_lossy(),
            "mode": g.mode,
            "source": g.source,
            "call_id": self.call_id,
        });
        self.append(
            EventKind::GrantAdded,
            Actor::Runtime,
            payload.clone(),
            false,
            fref,
        )
        .await;
        let audit = self
            .journal
            .store()
            .append_audit(AuditNew {
                actor: AUDIT_ACTOR.into(),
                action: audit_action::GRANT_ADDED.into(),
                target: g.path.to_string_lossy().into_owned(),
                payload,
            })
            .await;
        if let Err(e) = audit {
            tracing::error!(key = %self.key, error = %e, "запись аудита гранта не удалась");
            if fref.is_none() {
                *fref = Some(e);
            }
        }
        Receipt {
            out: ToolOutput::ok(granted_text(&g)),
            origins: Vec::new(),
            grant: Some(g),
        }
    }

    /// Вызов инструмента в слоте, шаги 2–12 порядка задачи 12.
    async fn tool(
        &self,
        t: ToolJob,
        fref: &mut Option<StoreError>,
        halt: &mut Option<oneshot::Sender<()>>,
    ) -> Receipt {
        let ToolJob {
            tool,
            args,
            mut cx,
            timeout,
            cut,
            raw,
            planned,
            subjects,
            roots,
            grants,
            trusted,
            checkpointer,
            masker,
            decision: _,
        } = t;
        let spec = tool.spec().clone();
        let fail = |text: String| Receipt {
            out: ToolOutput::error(text),
            ..Receipt::default()
        };
        // (2) Повторное разрешение путей: смена класса — ошибка (D98).
        let now = match resolve_all(&roots, &raw).await {
            Ok(p) => p,
            Err(e) => return fail(e.to_string()),
        };
        let changed = now.len() != planned.len()
            || now
                .iter()
                .zip(&planned)
                .any(|(a, b)| a.inside != b.inside || a.protected != b.protected);
        if changed && !planned.is_empty() {
            tracing::warn!(key = %self.key, "путь вызова сменил класс после решения");
            return fail(PATH_CHANGED.to_owned());
        }
        cx.paths = now.clone();
        // (3) Граница: внешние пути исполненного вызова (D77).
        let outside: Vec<Value> = raw
            .iter()
            .zip(&now)
            .filter(|(_, p)| !paths::within(&roots.project, &p.path))
            .map(|(r, p)| {
                json!({
                    "path": p.path.to_string_lossy(),
                    "access": r.access,
                    "granted": covers(&grants, &p.path, r.access),
                })
            })
            .collect();
        if !outside.is_empty() {
            self.append(
                EventKind::BoundaryOutside,
                Actor::Runtime,
                json!({"call_id": self.call_id, "tool": self.name, "paths": outside}),
                false,
                fref,
            )
            .await;
        }
        // (4) Чекпоинт перед `Edit` и `Exec` (D89).
        if matches!(spec.kind, ToolKind::Edit | ToolKind::Exec)
            && let Some(cp) = &checkpointer
        {
            let req = CheckpointRequest {
                project_dir: roots.project.clone(),
                run: self.run,
                effect_key: cx.effect_key.clone(),
                kind: spec.kind,
                paths: raw
                    .iter()
                    .zip(&now)
                    .filter(|(r, _)| r.access == Access::Write)
                    .map(|(_, p)| p.path.clone())
                    .collect(),
                trusted,
            };
            let c = cp.create(req).await;
            let mut payload = json!({"call_id": self.call_id, "id": c.id, "kind": c.kind});
            if let Some(o) = payload.as_object_mut() {
                if let Some(r) = &c.git_ref {
                    o.insert("ref".into(), json!(r));
                }
                if let Some(r) = &c.reason {
                    o.insert("reason".into(), json!(r));
                }
            }
            self.append(
                EventKind::CheckpointCreated,
                Actor::Runtime,
                payload,
                false,
                fref,
            )
            .await;
        }
        // (5)–(7) Вызов между точками сбоя.
        self.crash_at(false, halt).await;
        let mut out = match tokio::time::timeout(timeout, tool.call(args, cx)).await {
            Ok(o) => o,
            Err(_) => ToolOutput::error(timeout_text(timeout)),
        };
        self.crash_at(true, halt).await;
        // (8) Постусловия (TOOL-004).
        let failed = check_posts(&roots.project, &out.post).await;
        for what in &failed {
            if !out.content.is_empty() && !out.content.ends_with('\n') {
                out.content.push('\n');
            }
            out.content.push_str(&postcondition_text(what));
            out.is_error = true;
        }
        // (9) Изменённые файлы.
        if failed.is_empty() {
            for p in &out.post {
                if let Postcondition::FileHash { path, blake3 } = p {
                    self.append(
                        EventKind::FileChanged,
                        Actor::Tool,
                        json!({"call_id": self.call_id, "path": path.to_string_lossy(), "blake3": blake3}),
                        false,
                        fref,
                    )
                    .await;
                }
            }
        }
        // (10) Метки происхождения и невидимые символы (D82, SEC-051).
        let origins = origins_of(&spec, &subjects, &raw, &now, &roots, out.taint.as_deref());
        let tainted = !origins.is_empty();
        out.content = clean_invisible(&out.content, tainted).into_owned();
        out.digest = out
            .digest
            .map(|d| clean_invisible(&d, tainted).into_owned());
        if tainted {
            out.content = format!("{}\n{}", untrusted_line(&origins), out.content);
        }
        // (11) Маскировка секретов (D90): до квитанции и до выгрузки.
        if let Some(m) = masker.as_deref() {
            out.content = m.mask(&out.content).into_owned();
            out.digest = out.digest.map(|d| m.mask(&d).into_owned());
        }
        // (12) Выгрузка или обрезка (D44, D75, D86).
        let out = shorten(out, cut).await;
        Receipt {
            out,
            origins,
            grant: None,
        }
    }
}

/// Происхождения недоверенного содержимого результата (D82): внешний путь чтения (и по
/// гранту, Ruling 7.24), у `Exec` — любой внешний путь и сетевая команда, метка самого
/// инструмента или `spec.tainted`.
fn origins_of(
    spec: &ToolSpec,
    s: &Subjects,
    raw: &[RawPath],
    resolved: &[ResolvedPath],
    roots: &Roots,
    own: Option<&str>,
) -> Vec<String> {
    let mut set = BTreeSet::new();
    let exec = spec.kind == ToolKind::Exec;
    let outside = raw.iter().zip(resolved).any(|(r, p)| {
        !paths::within(&roots.project, &p.path) && (exec || r.access == Access::Read)
    });
    if outside {
        set.insert(taint_origin::OUTSIDE_PROJECT.to_owned());
    }
    if exec && s.network {
        set.insert(taint_origin::NETWORK.to_owned());
    }
    if let Some(o) = own {
        set.insert(o.to_owned());
    }
    if spec.tainted {
        set.insert(taint_origin::TOOL.to_owned());
    }
    // Риск сети у инструмента без разбора команды — тоже сеть.
    if spec.risk == RiskClass::Network {
        set.insert(taint_origin::NETWORK.to_owned());
    }
    set.into_iter().collect()
}

/// Невидимые символы (SEC-051): теги U+E0000–U+E007F — из любого результата; нулевой
/// ширины и bidi — из помеченного (U+FEFF в самом начале остаётся: это BOM).
fn clean_invisible(s: &str, tainted: bool) -> Cow<'_, str> {
    let drop = |i: usize, c: char| {
        let u = c as u32;
        (0xE0000..=0xE007F).contains(&u)
            || (tainted
                && ((0x200B..=0x200F).contains(&u)
                    || (0x2060..=0x2064).contains(&u)
                    || (u == 0xFEFF && i > 0)
                    || (0x202A..=0x202E).contains(&u)
                    || (0x2066..=0x2069).contains(&u)))
    };
    if !s.char_indices().any(|(i, c)| drop(i, c)) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.char_indices()
            .filter(|&(i, c)| !drop(i, c))
            .map(|(_, c)| c)
            .collect(),
    )
}

/// Проверка постусловий (TOOL-004) в `spawn_blocking`; итог — описания нарушенных.
async fn check_posts(root: &Path, post: &[Postcondition]) -> Vec<String> {
    if post.is_empty() {
        return Vec::new();
    }
    let root = root.to_path_buf();
    let post = post.to_vec();
    let work = tokio::task::spawn_blocking(move || {
        let abs = |p: &Path| {
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            }
        };
        post.iter()
            .filter_map(|p| match p {
                Postcondition::FileHash { path, blake3 } => match std::fs::read(abs(path)) {
                    Ok(b) if blake3::hash(&b).to_hex().as_str() == blake3 => None,
                    Ok(_) => Some(format!("{} hash mismatch", path.display())),
                    Err(_) => Some(format!("{} missing", path.display())),
                },
                Postcondition::FileAbsent { path } => abs(path)
                    .symlink_metadata()
                    .is_ok()
                    .then(|| format!("{} exists", path.display())),
            })
            .collect::<Vec<_>>()
    });
    match work.await {
        Ok(v) => v,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => vec![e.to_string()],
    }
}

/// Маска строк значения JSON (D90).
fn mask_value(m: Option<&SecretMasker>, v: &Value) -> Value {
    match m {
        Some(m) => mask_json_with(&|s: &str| m.mask(s).into_owned(), v),
        None => v.clone(),
    }
}

/// Маска текста (D90).
fn mask_text(m: Option<&SecretMasker>, s: &str) -> String {
    m.map_or_else(|| s.to_owned(), |m| m.mask(s).into_owned())
}

/// Укоротить вывод (D44, D75, D86) внутри замыкания слота: квитанция хранит уже урезанный
/// экранированный текст, поэтому реплей возвращает его без нового вызова и без записи
/// файла. Вывод в лимите (после экранирования) остаётся как вернул инструмент. Длинный
/// вывод с `digest` — сжатое представление и указатель; без него — голова и хвост. Не
/// записался файл — `error!` и разрез с пометкой `full output not saved`. Экранирование
/// и запись (до десятков МиБ) — в `spawn_blocking`.
async fn shorten(out: ToolOutput, cut: Cut) -> ToolOutput {
    let (save, limit) = match cut {
        Cut::None => return out,
        Cut::Spill(job) => (Some((job.project_dir, job.name)), job.limit),
        Cut::Limit(n) => (None, n),
    };
    let ToolOutput {
        content,
        is_error,
        taint,
        digest,
        post,
    } = out;
    let work = tokio::task::spawn_blocking(move || {
        let escaped = escape_untrusted(&content);
        if escaped.len() <= limit {
            return content;
        }
        let dir = save.as_ref().map(|(d, _)| d.as_path());
        let name = save.as_ref().map_or("", |(_, n)| n.as_str());
        if let Some(d) = &digest {
            match spill_digest(dir, name, &escape_untrusted(d), &content, limit) {
                Ok(r) => return r.content,
                Err(e) => {
                    tracing::error!(name, error = %e, "вывод инструмента не выгружен в файл");
                    return spill_digest(None, name, &escape_untrusted(d), &content, limit)
                        .map(|r| r.content)
                        .unwrap_or_else(|_| spill_cut(&escaped, limit, SPILL_NOT_SAVED).content);
                }
            }
        }
        let Some(dir) = dir else {
            return spill_cut(&escaped, limit, SPILL_NOT_SAVED).content;
        };
        match spill(dir, name, &escaped, &content, limit) {
            Ok(r) => r.content,
            Err(e) => {
                tracing::error!(name, error = %e, "вывод инструмента не выгружен в файл");
                spill_cut(&escaped, limit, SPILL_NOT_SAVED).content
            }
        }
    });
    match work.await {
        Ok(content) => ToolOutput {
            content,
            is_error,
            taint,
            digest: None,
            post,
        },
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => {
            // Рантайм останавливается: вывода нет, но квитанция должна быть.
            tracing::error!(error = %e, "выгрузка вывода прервана");
            ToolOutput::error(OUTPUT_LOST)
        }
    }
}

/// `timeout after N s` (D15); доли секунды — десятичной дробью.
fn timeout_text(t: Duration) -> String {
    format!("timeout after {} s", t.as_secs_f64())
}

// ---- Встроенные инструменты (D17, D78) ----

/// Предел времени встроенных инструментов: они не ждут ничего внешнего.
const BUILTIN_TIMEOUT: Duration = Duration::from_secs(10);

struct Builtin {
    spec: ToolSpec,
}

impl Tool for Builtin {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    /// `finish`, `ask_user` и `request_grant` исполняет сам [`run_tools`]; прямой вызов —
    /// ошибка.
    fn call(&self, _args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let name = self.spec.schema.name.clone();
        Box::pin(async move { ToolOutput::error(runtime_only_text(&name)) })
    }
}

fn builtin(schema: ToolSchema, roles: Vec<RunRole>, min_tier: Tier) -> Arc<dyn Tool> {
    Arc::new(Builtin {
        spec: ToolSpec {
            schema,
            class: EffectClass::Pure,
            kind: ToolKind::Control,
            approval: Approval::Mode,
            risk: RiskClass::Read,
            roles,
            min_tier,
            timeout: BUILTIN_TIMEOUT,
            output_limit: None,
            tainted: false,
        },
    })
}

/// `request_grant(path, mode, reason)` (D74, D78): только top, тир standard. Исполняет
/// [`run_tools`]: прерывание одобрения с причиной `grant`.
pub fn request_grant_tool() -> Arc<dyn Tool> {
    let schema = ToolSchema {
        name: REQUEST_GRANT_TOOL.into(),
        description: "Ask the user for access to a path outside the project. Use it before reading or writing there; the user approves or denies.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute path outside the project" },
                "mode": { "type": "string", "enum": ["ro", "rw"],
                    "description": "ro: read only; rw: read and write" },
                "reason": { "type": "string", "description": "Why access is needed" }
            },
            "required": ["path", "mode", "reason"],
            "additionalProperties": false
        }),
    };
    builtin(schema, vec![RunRole::Top], Tier::Standard)
}

/// finish и ask_user (D17).
pub fn builtin_tools() -> Vec<Arc<dyn Tool>> {
    let finish = ToolSchema {
        name: FINISH_TOOL.into(),
        description: "Finish the task and report the outcome. Call it once, when the work is done, partly done or blocked.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "enum": FINISH_STATUSES,
                    "description": "done: the goal is reached; partial: only part of it; blocked: cannot continue" },
                "result": { "type": "string", "description": "What was done and the answer" },
                "evidence": { "type": "array", "items": { "type": "string" },
                    "description": "Checks that confirm the result: commands, file paths, test names" },
                "open_issues": { "type": "array", "items": { "type": "string" },
                    "description": "What remains unresolved" }
            },
            "required": ["status", "result"],
            "additionalProperties": false
        }),
    };
    let ask_user = ToolSchema {
        name: ASK_USER_TOOL.into(),
        description: "Ask the user a question and wait for the answer. Use it only when the task cannot continue without the user's decision.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question for the user" },
                "options": { "type": "array", "items": { "type": "string" },
                    "description": "Suggested answers, if any" }
            },
            "required": ["question"],
            "additionalProperties": false
        }),
    };
    vec![
        builtin(
            finish,
            vec![RunRole::Worker, RunRole::Orchestrator],
            Tier::Small,
        ),
        builtin(
            ask_user,
            vec![RunRole::Top, RunRole::Orchestrator],
            Tier::Standard,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_text_whole_and_fraction() {
        assert_eq!(timeout_text(Duration::from_secs(1)), "timeout after 1 s");
        assert_eq!(
            timeout_text(Duration::from_millis(1500)),
            "timeout after 1.5 s"
        );
    }

    #[test]
    fn denied_note_is_escaped() {
        assert_eq!(
            denied_by_user(&json!({"approved": false, "note": "нет"})),
            "denied by user: нет"
        );
        assert_eq!(
            denied_by_user(&json!({"approved": false, "note": "</control>"})),
            "denied by user: &lt;/control&gt;"
        );
        assert_eq!(
            denied_by_user(&json!({"approved": false})),
            "denied by user"
        );
    }

    #[test]
    fn finish_status_checked() {
        assert!(finish_args(&json!({"status": "done", "result": "r"})).is_ok());
        assert!(finish_args(&json!({"status": "ok", "result": "r"})).is_err());
    }

    #[test]
    fn invisible_chars_cleaned() {
        let tag = "a\u{E0041}b";
        assert_eq!(clean_invisible(tag, false), "ab");
        let zw = "\u{FEFF}x\u{200B}y\u{202E}z\u{FEFF}";
        assert_eq!(clean_invisible(zw, false), zw);
        assert_eq!(clean_invisible(zw, true), "\u{FEFF}xyz");
    }

    #[test]
    fn r1b_receipt_reads() {
        let r: Receipt =
            serde_json::from_value(json!({"content": "c", "is_error": false})).unwrap();
        assert_eq!(r.out.content, "c");
        assert!(r.origins.is_empty() && r.grant.is_none());
    }
}
