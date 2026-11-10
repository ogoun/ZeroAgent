//! Пакет вызовов инструментов (D14–D17, D25, D30, CORE-104, CORE-105).
//!
//! [`run_tools`] исполняет вызовы одного ответа модели по порядку. Ключ эффекта
//! резервируется на каждый вызов, даже на отклонённый (D15), поэтому ключи при
//! реплее не зависят от исходов. Подряд идущие разрешённые `Read`-вызовы при
//! `parallel` выполняются одновременно в `JoinSet`, любой другой вызов — барьер.
//!
//! Вызов, отклонённый разбором ответа ([`CallVerdict::Reject`]) или гейтом режима
//! ([`gate`]), слот не запускает: результат-ошибка без эффекта. Повтор
//! ([`CallVerdict::Reuse`], R1-B2 D54) — тоже без эффекта: результат — готовый текст. Вызов, требующий
//! одобрения, и `ask_user` перед слотом читают состояние прерывания (исключение
//! D30: `open → resolved` необратимо): решения нет — прерывание открывается и пакет
//! возвращает `suspend`.
//!
//! Квитанция эффекта — [`ToolOutput`] как вернул инструмент (ошибка инструмента —
//! данные, исход `ok`); в результат для модели текст идёт через
//! [`escape_untrusted`] (D23, TOOL-005). События `tool.started` и `tool.finished`
//! пишутся только вживую, внутри замыкания слота: реплей их не повторяет.
//!
//! Вывод длиннее лимита (R1-B2 D44, `ToolEnv::spill`) выгружается в файл тоже внутри
//! замыкания: квитанция хранит голову, строку пропуска и хвост экранированного текста.

use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinSet;
use tokio::time::Instant;
use za_core::interrupt::{self, InterruptState};
use za_core::{
    Approval, EffectError, EffectSlot, EffectSpec, Gate, Interrupt, InterruptKind, Journal,
    RiskClass, RunCtx, Tool, ToolCx, ToolKind, ToolOutput, ToolRegistry, ToolSpec, gate,
};
use za_llm::{BoxFuture, Part, ToolCall, ToolSchema};
use za_protocol::{
    Actor, EffectClass, EventKind, NewEvent, RunId, RunMode, RunRole, Tier, canonical_json,
};
use za_store::StoreError;

use crate::context::{SPILL_NOT_SAVED, SpillCfg, spill, spill_cut, spill_limit_bytes, spill_name};
use crate::driver::{FaultHook, FaultPoint, INJECTED_FAULT};
use crate::prompt::escape_untrusted;
use crate::turn::{CallVerdict, PlannedCall, Signal, Signals};

/// Имя встроенного инструмента завершения (D17).
pub const FINISH_TOOL: &str = "finish";
/// Имя встроенного инструмента вопроса пользователю (D17).
pub const ASK_USER_TOOL: &str = "ask_user";
/// Синтетический результат `finish` (D17).
pub(crate) const FINISH_OK: &str = "ok";
/// Текст квитанции-отказа при ошибке журнала внутри слота: при реплее он уходит
/// модели, поэтому по-английски и без подробностей (они — в `tracing`).
pub(crate) const JOURNAL_FAILED: &str = "internal error: journal write failed";
/// Квитанция вызова, вывод которого потерян при остановке рантайма во время выгрузки.
pub(crate) const OUTPUT_LOST: &str = "internal error: tool output lost";
/// Результат вызовов пакета после принятого `finish`: они не исполняются.
pub(crate) const AFTER_FINISH: &str = "not executed: run finished by finish";

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
    /// Прерывание (одобрение или `ask_user`), на котором пакет остановился.
    pub suspend: Option<Interrupt>,
    /// Аргументы принятого вызова `finish`.
    pub finish: Option<FinishArgs>,
    /// Input или Pause: не начатые вызовы пропущены (D25).
    pub aborted: bool,
    /// Cancel: JoinSet::abort_all, идущий вызов брошен (D15).
    pub cancelled: bool,
    /// `call_id` вызовов, чей слот исполнен (результат — квитанция эффекта), по порядку
    /// результатов. Отказ разбора или гейта, отказ пользователя, повтор (D54), вызов
    /// после `finish` и не начатый вызов сюда не входят.
    pub executed: Vec<String>,
}

/// Окружение пакета.
#[derive(Debug, Clone)]
pub struct ToolEnv {
    pub run: RunId,
    /// Корень проекта run (`ToolCx::project_dir`).
    pub project_dir: PathBuf,
    pub mode: RunMode,
    /// Чтения одновременно: `capabilities(endpoint).parallel_tools` (исключение D30).
    pub parallel: bool,
    /// Выгрузка большого вывода (R1-B2 D44); `None` — без выгрузки, как в R1-B1.
    pub spill: Option<SpillCfg>,
}

/// Выгрузка вывода одного вызова (D44): файл `<project>/.zero-agent/work/spill/{name}.txt`.
struct SpillJob {
    project_dir: PathBuf,
    name: String,
    limit: usize,
}

/// Что делает слот вызова вживую.
enum Action {
    Tool {
        tool: Arc<dyn Tool>,
        args: Value,
        cx: ToolCx,
        timeout: Duration,
        spill: Option<SpillJob>,
    },
    /// Ответ пользователя на `ask_user`.
    Answer(String),
    /// `finish`: синтетический `ok`.
    Finish,
}

/// Подготовленный к запуску слот.
struct Job {
    slot: EffectSlot,
    spec: EffectSpec,
    call_id: String,
    action: Action,
    journal: Journal,
    run: RunId,
    /// Шаг пакета и хук разового сбоя `tool.finished` (тест, R1-C1 D72).
    fault: Option<(u64, FaultHook)>,
}

/// Итог запуска слота.
struct Executed {
    result: Result<Value, EffectError>,
    /// Ошибка журнала внутри замыкания (события `tool.*`): контекст отравляется.
    store: Option<StoreError>,
}

/// Решение по вызову до запуска слота.
enum Plan {
    /// Результат без эффекта: отказ разбора, гейта, неизвестный инструмент.
    Immediate(String),
    /// Результат без эффекта и без ошибки: прошлый результат повторного вызова (D54).
    Reused(String),
    /// Разрешённый вызов; `parallel` — можно в группу одновременных чтений.
    Run {
        tool: Arc<dyn Tool>,
        parallel: bool,
    },
    /// Нужно одобрение пользователя.
    Approve {
        tool: Arc<dyn Tool>,
    },
    AskUser,
    Finish(FinishArgs),
}

/// D14, D15, D16, D17. Прерывание: результаты до прерывания в outcome, suspend = Some.
/// Signal::Input/Pause во время пакета: идущий вызов дорабатывает, остальные не начинаются (aborted = true).
/// Signal::Cancel: идущие вызовы брошены, cancelled = true. finish получает результат "ok" (D17).
/// Вживую, внутри замыкания слота: tool.started {call_id, name, effect_key} и tool.finished {call_id, is_error, duration_ms}.
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
    run_tools_with(ctx, j, reg, env, calls, sig, None).await
}

/// [`run_tools`] с хуком разового сбоя журнала `LoopDeps.fault` (D72): точка
/// [`FaultPoint::BeforeToolFinished`] даёт ошибку записи `tool.finished`, как настоящая.
pub(crate) async fn run_tools_with(
    ctx: &mut RunCtx,
    j: &Journal,
    reg: &ToolRegistry,
    env: &ToolEnv,
    calls: &[PlannedCall],
    sig: &mut Signals,
    fault: Option<FaultHook>,
) -> Result<ToolsOutcome, EffectError> {
    let fault = fault.map(|h| (ctx.step(), h));
    let fault = fault.as_ref();
    // D15: ключи на все вызовы по порядку, до любых исходов.
    let mut slots = Vec::with_capacity(calls.len());
    for _ in calls {
        slots.push(Some(ctx.reserve()?));
    }
    let plans: Vec<Plan> = calls.iter().map(|c| plan(reg, env.mode, c)).collect();
    let mut out = ToolsOutcome::default();
    let mut i = 0;
    while i < calls.len() {
        // Сигнал, пришедший между вызовами, останавливает пакет до следующего.
        if let Some(s) = pending(sig).await {
            stop(&mut out, s);
            return Ok(out);
        }
        // Группа подряд идущих разрешённых чтений (D15).
        let mut end = i;
        while env.parallel && end < calls.len() && is_parallel(&plans[end]) {
            end += 1;
        }
        if end - i >= 2 {
            let mut jobs = Vec::with_capacity(end - i);
            for k in i..end {
                let Plan::Run { tool, .. } = &plans[k] else {
                    continue;
                };
                let slot = take(&mut slots, k)?;
                let action = tool_action(tool, &calls[k].call, env, slot.key());
                jobs.push((k, job(slot, &calls[k].call, action, j, env, fault)));
            }
            let done = run_group(jobs, sig, &mut out).await;
            for (k, ex) in done {
                let part = finish_part(ctx, &calls[k].call, ex)?;
                out.executed.push(calls[k].call.id.clone());
                out.results.push(part);
            }
            if out.cancelled || out.aborted {
                return Ok(out);
            }
            i = end;
            continue;
        }

        let call = &calls[i].call;
        let action = match &plans[i] {
            Plan::Immediate(text) => {
                out.results.push(result(call, text.clone(), true));
                i += 1;
                continue;
            }
            Plan::Reused(text) => {
                out.results.push(result(call, text.clone(), false));
                i += 1;
                continue;
            }
            Plan::Run { tool, .. } => {
                let key = slot_ref(&slots, i)?.key().clone();
                tool_action(tool, call, env, &key)
            }
            Plan::Approve { tool } => {
                let slot = slot_ref(&slots, i)?;
                let int = Interrupt::for_effect(
                    InterruptKind::Approval,
                    slot.key(),
                    json!({ "call_id": call.id, "name": call.name, "arguments": call.arguments }),
                );
                match decision(ctx, &int).await? {
                    None => {
                        out.suspend = Some(int);
                        return Ok(out);
                    }
                    Some(v) if v.get("approved").and_then(Value::as_bool) == Some(true) => {
                        let key = slot.key().clone();
                        tool_action(tool, call, env, &key)
                    }
                    Some(v) => {
                        out.results.push(result(call, denied_by_user(&v), true));
                        i += 1;
                        continue;
                    }
                }
            }
            Plan::AskUser => {
                let slot = slot_ref(&slots, i)?;
                let int = Interrupt::for_effect(
                    InterruptKind::Input,
                    slot.key(),
                    json!({
                        "call_id": call.id,
                        "question": call.arguments.get("question").cloned().unwrap_or(Value::Null),
                        "options": call.arguments.get("options").cloned().unwrap_or(json!([])),
                    }),
                );
                match decision(ctx, &int).await? {
                    None => {
                        out.suspend = Some(int);
                        return Ok(out);
                    }
                    Some(v) => Action::Answer(answer(&v)),
                }
            }
            Plan::Finish(_) => Action::Finish,
        };
        let jb = job(take(&mut slots, i)?, call, action, j, env, fault);
        let Some(ex) = run_one(jb, sig, &mut out).await else {
            return Ok(out);
        };
        let part = finish_part(ctx, call, ex)?;
        out.executed.push(call.id.clone());
        if let (
            Plan::Finish(args),
            Part::ToolResult {
                is_error: false, ..
            },
        ) = (&plans[i], &part)
        {
            out.finish = Some(args.clone());
        }
        out.results.push(part);
        if out.finish.is_some() {
            // `finish` — последнее действие run: остальные вызовы пакета получают
            // результат без исполнения, история остаётся валидной.
            for rest in &calls[i + 1..] {
                out.results
                    .push(result(&rest.call, AFTER_FINISH.to_owned(), true));
            }
            return Ok(out);
        }
        if out.aborted {
            return Ok(out);
        }
        i += 1;
    }
    Ok(out)
}

/// Решение по вызову без обращения к журналу.
fn plan(reg: &ToolRegistry, mode: RunMode, pc: &PlannedCall) -> Plan {
    match &pc.verdict {
        CallVerdict::Reject(text) => return Plan::Immediate(text.clone()),
        // Повтор не исполняется и не проходит гейт: ничего не делается (D54).
        CallVerdict::Reuse(text) => return Plan::Reused(text.clone()),
        CallVerdict::Run => {}
    }
    let call = &pc.call;
    let Some(tool) = reg.get(&call.name) else {
        return Plan::Immediate(unknown_tool_text(&call.name));
    };
    let spec = tool.spec();
    match gate(mode, spec) {
        Gate::Deny(reason) => Plan::Immediate(denied_text(reason)),
        Gate::Ask => Plan::Approve { tool },
        Gate::Allow => match call.name.as_str() {
            ASK_USER_TOOL => Plan::AskUser,
            FINISH_TOOL => match finish_args(&call.arguments) {
                Ok(a) => Plan::Finish(a),
                Err(e) => Plan::Immediate(finish_invalid_text(&e)),
            },
            _ => Plan::Run {
                parallel: spec.kind == ToolKind::Read,
                tool,
            },
        },
    }
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

/// Результат вызова, запрещённого режимом run (D14).
fn denied_text(reason: &str) -> String {
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
        ("runtime/tools.runtime_only", runtime_only_text("t")),
        (
            "runtime/tools.timeout",
            timeout_text(Duration::from_millis(1500)),
        ),
        ("runtime/tools.unknown_tool", unknown_tool_text("t")),
    ]
}

fn is_parallel(p: &Plan) -> bool {
    matches!(p, Plan::Run { parallel: true, .. })
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

fn tool_action(
    tool: &Arc<dyn Tool>,
    call: &ToolCall,
    env: &ToolEnv,
    key: &za_protocol::EffectKey,
) -> Action {
    let spill = env.spill.map(|c| SpillJob {
        project_dir: env.project_dir.clone(),
        name: spill_name(key),
        limit: spill_limit_bytes(c.n_ctx, c.share, tool.spec().output_limit),
    });
    Action::Tool {
        tool: Arc::clone(tool),
        args: call.arguments.clone(),
        cx: ToolCx {
            run: env.run,
            project_dir: env.project_dir.clone(),
            call_id: call.id.clone(),
            effect_key: key.clone(),
        },
        timeout: tool.spec().timeout,
        spill,
    }
}

fn job(
    slot: EffectSlot,
    call: &ToolCall,
    action: Action,
    j: &Journal,
    env: &ToolEnv,
    fault: Option<&(u64, FaultHook)>,
) -> Job {
    let class = match &action {
        Action::Tool { tool, .. } => tool.spec().class,
        Action::Answer(_) | Action::Finish => EffectClass::Pure,
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
        fault: fault.cloned(),
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

/// Итог слота в результат для модели; ошибки эффекта, кроме `Failed`, — `Err`.
fn finish_part(ctx: &mut RunCtx, call: &ToolCall, ex: Executed) -> Result<Part, EffectError> {
    ctx.observe(&ex.result);
    if let Some(e) = ex.store {
        let err = EffectError::Store(e);
        ctx.observe::<()>(&Err(err.clone()));
        return Err(err);
    }
    match ex.result {
        Ok(v) => Ok(match serde_json::from_value::<ToolOutput>(v.clone()) {
            Ok(o) => result(call, escape_untrusted(&o.content), o.is_error),
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
/// в итог идут только завершённые.
async fn run_group(
    jobs: Vec<(usize, Job)>,
    sig: &mut Signals,
    out: &mut ToolsOutcome,
) -> Vec<(usize, Executed)> {
    let mut set = JoinSet::new();
    for (k, jb) in jobs {
        set.spawn(async move { (k, execute(jb).await) });
    }
    let mut done = Vec::new();
    loop {
        tokio::select! {
            r = set.join_next() => match r {
                None => break,
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

/// Протокол эффекта слота; вживую — события `tool.*` и сам вызов.
/// Future — `Send + 'static` (годится для `JoinSet`): `Job` владеет всем нужным.
async fn execute(jb: Job) -> Executed {
    let Job {
        slot,
        spec,
        call_id,
        action,
        journal,
        run,
        fault,
    } = jb;
    let key = slot.key().0.clone();
    let name = spec.name.clone();
    let mut failure: Option<StoreError> = None;
    let fref = &mut failure;
    let result = slot
        .run(spec, move || async move {
            let started = Instant::now();
            let ev = |kind, actor, payload| NewEvent {
                run_id: run,
                parent_seq: None,
                actor,
                kind,
                payload,
                tainted: false,
            };
            if let Err(e) = journal
                .append(ev(
                    EventKind::ToolStarted,
                    Actor::Runtime,
                    json!({ "call_id": call_id, "name": name, "effect_key": key }),
                ))
                .await
            {
                tracing::error!(key = %key, error = %e, "tool.started не записано, инструмент не вызван");
                *fref = Some(e);
                return Err(JOURNAL_FAILED.to_owned());
            }
            let out = call_action(action).await;
            let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            // Инструмент уже отработал: квитанция фиксирует его итог, а ошибка
            // журнала отравляет контекст.
            let finished = match &fault {
                // Тест (D72): ошибка записи `tool.finished`, как настоящая.
                Some((step, h)) if h(FaultPoint::BeforeToolFinished { step: *step }) => {
                    Err(StoreError::Io(INJECTED_FAULT.into()))
                }
                _ => {
                    journal
                        .append(ev(
                            EventKind::ToolFinished,
                            Actor::Tool,
                            json!({ "call_id": call_id, "is_error": out.is_error, "duration_ms": duration_ms }),
                        ))
                        .await
                }
            };
            if let Err(e) = finished {
                tracing::error!(key = %key, error = %e, "tool.finished не записано");
                *fref = Some(e);
            }
            serde_json::to_value(&out).map_err(|e| {
                tracing::error!(key = %key, error = %e, "итог инструмента не сериализуется");
                JOURNAL_FAILED.to_owned()
            })
        })
        .await;
    Executed {
        result,
        store: failure,
    }
}

async fn call_action(action: Action) -> ToolOutput {
    match action {
        Action::Tool {
            tool,
            args,
            cx,
            timeout,
            spill,
        } => {
            let out = match tokio::time::timeout(timeout, tool.call(args, cx)).await {
                Ok(o) => o,
                Err(_) => ToolOutput::error(timeout_text(timeout)),
            };
            match spill {
                Some(job) => spill_output(out, job).await,
                None => out,
            }
        }
        Action::Answer(a) => ToolOutput::ok(a),
        Action::Finish => ToolOutput::ok(FINISH_OK),
    }
}

/// Выгрузка вывода (D44) внутри замыкания слота: квитанция хранит уже урезанный
/// экранированный текст, поэтому реплей возвращает его без нового вызова и без записи
/// файла. Вывод в лимите (после экранирования) остаётся как вернул инструмент. Не
/// записался файл — `error!` и тот же разрез с пометкой `full output not saved`.
/// Экранирование и запись (до десятков МиБ) — в `spawn_blocking`.
async fn spill_output(out: ToolOutput, job: SpillJob) -> ToolOutput {
    let ToolOutput {
        content,
        is_error,
        taint,
        digest,
        post,
    } = out;
    let work = tokio::task::spawn_blocking(move || {
        let escaped = escape_untrusted(&content);
        if escaped.len() <= job.limit {
            return content;
        }
        match spill(&job.project_dir, &job.name, &escaped, &content, job.limit) {
            Ok(r) => r.content,
            Err(e) => {
                tracing::error!(name = %job.name, error = %e, "вывод инструмента не выгружен в файл");
                spill_cut(&escaped, job.limit, SPILL_NOT_SAVED).content
            }
        }
    });
    match work.await {
        Ok(content) => ToolOutput {
            content,
            is_error,
            taint,
            digest,
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

// ---- Встроенные инструменты (D17) ----

/// Предел времени встроенных инструментов: они не ждут ничего внешнего.
const BUILTIN_TIMEOUT: Duration = Duration::from_secs(10);

struct Builtin {
    spec: ToolSpec,
}

impl Tool for Builtin {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    /// `finish` и `ask_user` исполняет сам [`run_tools`]; прямой вызов — ошибка.
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
}
