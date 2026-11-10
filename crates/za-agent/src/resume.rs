//! Продолжение run (CORE-006, CORE-110, D4–D6, D26, D30).
//!
//! [`resume_run`] читает вход resume до запуска графа — статус run, незавершённые
//! эффекты, последний снимок, начат ли следующий шаг (исключение D30, пункт 4), —
//! восстанавливает [`LoopState`] из снимка и запускает общий граф цикла
//! (`run_graph`): с узла `model` (снимок посреди шага — реплей шага по квитанциям) или с
//! узла `boundary` (новая граница шагом `step + 1`). [`state_block`] — текст `<state>`
//! управляющего сообщения `resume` (A.6) для модели, по-английски (D23).

use serde_json::Value;
use za_core::{Recovery, RunCtx, recover};
use za_llm::EndpointId;
use za_protocol::{
    Budget, EffectClass, EffectKey, EventKind, RunId, RunMode, RunStatus, Seq, canonical_json,
    pause_reason,
};
use za_store::{INTERRUPT_RESOLVED, ProjectStore, StoreError};

use crate::control::{ControlRx, InputMode, submit_input};
use crate::conversation::{Conversation, LoadError};
use crate::driver::{
    BOUNDARY, DEFAULT_N_CTX, LoopDeps, LoopError, LoopOutcome, MODEL, fail, run_graph,
    run_registry, run_tools_of,
};
use crate::prompt::escape_untrusted;
use crate::state::{AtBoundary, LoopState, Next, PendingExit, SNAPSHOT_V, Snapshot};
use crate::turn::MODEL_EFFECT;

/// Изменения run при resume (CORE-110).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResumeChange {
    /// Новый эндпоинт: только со снимка `Boundary` (D6), иначе `ChangeMidStep`.
    pub endpoint: Option<EndpointId>,
    /// Новый бюджет (выход из `blocked_on_budget`, D19).
    pub budget: Option<Budget>,
    /// Ввод пользователя: `turn.input` (`queue`) до первой границы.
    pub input: Option<String>,
    pub run_mode: Option<RunMode>,
}

/// Длина аргументов вызова в блоке `<state>`, символы.
const STATE_ARGS_CHARS: usize = 120;
/// Страница чтения журнала для блока `<state>`.
const STATE_PAGE: usize = 512;

/// CORE-006, CORE-110. Статусы: Running (сбой процесса) — реплей; Pausing — сначала Paused(command), затем как Paused;
/// Cancelling — Cancelled и finish_run без шагов; Paused, BlockedOnBudget — → Running; прочие — NotResumable.
pub async fn resume_run(
    deps: &LoopDeps,
    run: RunId,
    change: ResumeChange,
    ctl: ControlRx,
) -> Result<LoopOutcome, LoopError> {
    let j = &deps.journal;
    let store = j.store().clone();
    // Флаг паузы канала не снимается: `ControlRx` одноразовый (его забирает цикл), и
    // пауза на канале, переданном сюда, — новая команда пользователя (решение
    // контроллера, I2 ревью задачи 15). Её применит первая граница.

    // (1) Статус run.
    let r = store
        .get_run(run)
        .await?
        .ok_or(StoreError::UnknownRun(run))?;
    let mut generation = r.generation;
    let mut status = r.status;
    match status {
        RunStatus::Running | RunStatus::Paused | RunStatus::BlockedOnBudget => {}
        RunStatus::Pausing => {
            // D26: процесс упал в `pausing` — пауза доводится до `paused`.
            generation = j
                .set_status(
                    run,
                    RunStatus::Paused,
                    generation,
                    Some(pause_reason::COMMAND),
                )
                .await?;
            status = RunStatus::Paused;
        }
        RunStatus::Cancelling => {
            // D26: процесс упал в `cancelling` — отмена доводится, шагов нет.
            j.set_status(run, RunStatus::Cancelled, generation, None)
                .await?;
            deps.model.finish_run(run);
            tracing::info!(run = %run, "resume: отмена доведена до cancelled");
            return Ok(LoopOutcome::Cancelled);
        }
        other => return Err(LoopError::NotResumable(other)),
    }

    // (2) Незавершённые NonIdempotent-эффекты: нужно решение человека.
    if let Recovery::NeedsDecision { interrupt_ids } = recover(&store, run).await? {
        if status == RunStatus::Running {
            j.set_status(
                run,
                RunStatus::Paused,
                generation,
                Some(pause_reason::NEEDS_DECISION),
            )
            .await?;
        }
        return Ok(LoopOutcome::Paused {
            reason: pause_reason::NEEDS_DECISION.into(),
            interrupt: interrupt_ids.into_iter().next(),
        });
    }

    // (3) Последний снимок.
    let Some((seq, v)) = store.latest_snapshot(run).await? else {
        let e = LoopError::Snapshot("нет снимка".into());
        return Err(fail_broken(deps, run, status, generation, e).await);
    };
    let ver = v.get("v").and_then(Value::as_u64);
    if ver != Some(u64::from(SNAPSHOT_V)) {
        let shown = ver.map_or_else(|| "?".to_owned(), |n| n.to_string());
        return Err(LoopError::Snapshot(format!("версия снимка {shown}")));
    }
    // Снимок своей версии, который не разбирается, — нарушенный инвариант (решение
    // контроллера, M2 ревью задачи 15): `failed` с причиной `snapshot`.
    let snap: Snapshot = match serde_json::from_value(v) {
        Ok(s) => s,
        Err(e) => {
            let e = LoopError::Snapshot(format!("снимок не разбирается: {e}"));
            return Err(fail_broken(deps, run, status, generation, e).await);
        }
    };

    // (4) D5: процесс упал между снимком чистой паузы и её статусом.
    if status == RunStatus::Running
        && let Some(e) = &snap.exit
        && !store.step_has_effects(run, snap.step + 1).await?
    {
        return Ok(match e {
            PendingExit::Paused { reason } => {
                j.set_status(run, RunStatus::Paused, generation, Some(reason))
                    .await?;
                LoopOutcome::Paused {
                    reason: reason.clone(),
                    interrupt: None,
                }
            }
            PendingExit::BlockedOnBudget { budget } => {
                j.set_status(run, RunStatus::BlockedOnBudget, generation, None)
                    .await?;
                LoopOutcome::BlockedOnBudget { kind: *budget }
            }
        });
    }

    // (5), (6) Состояние из снимка и изменения resume.
    let next = snap.next;
    if next == Next::Model && change.endpoint.is_some() {
        return Err(LoopError::ChangeMidStep);
    }
    let mut st = match restore(deps, run, seq, snap, ctl).await {
        Ok(st) => st,
        Err(e @ LoopError::Snapshot(_)) => {
            return Err(fail_broken(deps, run, status, generation, e).await);
        }
        Err(e) => return Err(e),
    };
    // D19: простой между процессами и время в паузе — не активное время.
    st.counters.last_boundary_ms = None;
    let at = match next {
        Next::Model => {
            // Шаг дорабатывается с прежними бюджетом и режимом (D6).
            st.at_boundary = AtBoundary {
                budget: change.budget,
                run_mode: change.run_mode,
                resume_note: false,
            };
            MODEL
        }
        Next::Boundary => {
            if let Some(ep) = change.endpoint {
                if deps.model.capabilities(&ep).is_none() {
                    return Err(LoopError::UnknownEndpoint(ep));
                }
                // D6: префикс и тир заморожены, окно — по новому эндпоинту.
                st.setup.n_ctx = deps.model.context_tokens(&ep).unwrap_or_else(|| {
                    tracing::warn!(endpoint = %ep.0, n_ctx = DEFAULT_N_CTX,
                        "размер контекста эндпоинта неизвестен, принят по умолчанию");
                    DEFAULT_N_CTX
                });
                st.setup.endpoint = ep;
            }
            if let Some(b) = change.budget {
                st.budget = b;
            }
            if let Some(m) = change.run_mode {
                st.setup.run_mode = m;
            }
            st.at_boundary.resume_note = true;
            BOUNDARY
        }
    };
    if let Some(text) = &change.input {
        submit_input(&store, run, text, InputMode::Queue).await?;
    }
    st.generation = if status == RunStatus::Running {
        generation
    } else {
        j.set_status(run, RunStatus::Running, generation, None)
            .await?
    };
    tracing::info!(run = %run, step = st.ctx.step(), ?next, "resume run");
    run_graph(deps, &mut st, at).await
}

/// Снимка нет или он испорчен: `failed` с причиной `snapshot` (D4, D11). Из `paused` и
/// `blocked_on_budget` перехода в `failed` нет (часть 01, 4.3): run сначала переводится
/// в `running` — resume начат и не удался.
async fn fail_broken(
    deps: &LoopDeps,
    run: RunId,
    status: RunStatus,
    mut generation: u64,
    e: LoopError,
) -> LoopError {
    if !status.can_transition(RunStatus::Failed) && status.can_transition(RunStatus::Running) {
        match deps
            .journal
            .set_status(run, RunStatus::Running, generation, None)
            .await
        {
            Ok(g) => generation = g,
            Err(se) => {
                tracing::error!(run = %run, error = %e, status_error = %se,
                    "снимок run испорчен, статус failed не записан");
                return e;
            }
        }
    }
    fail(deps, run, generation, e).await
}

/// [`LoopState`] из снимка на `seq`.
async fn restore(
    deps: &LoopDeps,
    run: RunId,
    seq: Seq,
    snap: Snapshot,
    ctl: ControlRx,
) -> Result<LoopState, LoopError> {
    let store = deps.journal.store().clone();
    // Испорченная история снимка (нет блоба, блоб не разбирается, нарушен инвариант) —
    // `LoopError::Snapshot`: вызывающий переводит run в `failed`. Прочие ошибки журнала
    // (ввод-вывод) статус не меняют: resume можно повторить.
    let conv = Conversation::load(&store, &snap.messages)
        .await
        .map_err(|e| match e {
            LoadError::Store(e @ StoreError::Corrupt(_)) => {
                LoopError::Snapshot(format!("история снимка: {e}"))
            }
            LoadError::Store(e) => LoopError::Store(e),
            LoadError::History(e) => LoopError::Snapshot(format!("история снимка: {e}")),
        })?;
    let registry = run_registry(deps)?;
    let (_, classes) = run_tools_of(&registry, snap.setup.role, snap.setup.tier);
    Ok(LoopState {
        run,
        ctx: RunCtx::resume_at(store, run, snap.step, snap.effect_seq),
        conv,
        refs: snap.messages,
        tools: snap.frozen.tools.clone(),
        frozen: snap.frozen,
        frozen_ref: snap.frozen_ref,
        setup: snap.setup,
        budget: snap.budget,
        counters: snap.counters,
        turn: snap.turn,
        inbox_after: snap.inbox_after,
        last_seq: seq,
        generation: 0,
        classes,
        reviewed: None,
        sig: ctl.sig,
        answer: None,
        finish: None,
        exit: None,
        stopping: None,
        at_boundary: AtBoundary::default(),
    })
}

/// Текст <state>: шаги, последняя причина паузы, решённые неизвестные исходы, мутирующие вызовы
/// (имя и аргументы до 120 символов) — эффект ReadOnly "state".
///
/// Шаги — число `turn.finished`; пауза — причина последнего перехода в `paused`
/// (`blocked_on_budget` — так и пишется); мутирующие вызовы — намерения классов
/// `Idempotent` и `NonIdempotent`, кроме эффекта модели, с квитанцией-успехом без
/// `is_error`. Имена и аргументы экранируются [`escape_untrusted`].
pub async fn state_block(store: &ProjectStore, run: RunId) -> Result<String, StoreError> {
    let mut steps = 0u64;
    let mut last_pause = String::from("none");
    let mut changes = Vec::new();
    let mut cursor = Seq(0);
    loop {
        let page = store.read_run(run, cursor, STATE_PAGE).await?;
        let n = page.len();
        for e in page {
            cursor = e.seq;
            match e.kind {
                EventKind::TurnFinished => steps += 1,
                EventKind::RunStatus => {
                    let to = serde_json::from_value::<RunStatus>(e.payload["to"].clone()).ok();
                    match to {
                        Some(RunStatus::Paused) => {
                            last_pause = e.payload["reason"].as_str().unwrap_or("-").to_owned();
                        }
                        Some(RunStatus::BlockedOnBudget) => {
                            last_pause = "blocked_on_budget".into();
                        }
                        _ => {}
                    }
                }
                EventKind::EffectIntent => {
                    if let Some(c) = change_of(store, run, &e.payload).await? {
                        changes.push(c);
                    }
                }
                _ => {}
            }
        }
        if n < STATE_PAGE {
            break;
        }
    }
    let unknown: Vec<String> = store
        .interrupts(run)
        .await?
        .into_iter()
        .filter(|i| i.kind == za_core::UNKNOWN_OUTCOME && i.status == INTERRUPT_RESOLVED)
        .filter_map(|i| {
            let key = i.payload.as_ref()?.get("effect_key")?.as_str()?.to_owned();
            let d = i.response.as_ref()?.get("decision")?.as_str()?.to_owned();
            Some(escape_untrusted(&format!("{key} → {d}")))
        })
        .collect();
    Ok(format!(
        "<state>\nsteps done: {steps}; last pause: {}\nunknown outcomes: {}\nchanges made: {}\n</state>",
        escape_untrusted(&last_pause),
        list(&unknown),
        list(&changes),
    ))
}

/// Пункты через "; " или "none".
fn list(items: &[String]) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items.join("; ")
    }
}

/// Мутирующий вызов из payload `effect.intent {key, class, name, args}`: `name(args…)`.
async fn change_of(
    store: &ProjectStore,
    run: RunId,
    p: &Value,
) -> Result<Option<String>, StoreError> {
    let class = serde_json::from_value::<EffectClass>(p["class"].clone()).ok();
    let name = p["name"].as_str().unwrap_or_default();
    if !matches!(
        class,
        Some(EffectClass::Idempotent | EffectClass::NonIdempotent)
    ) || name == MODEL_EFFECT
    {
        return Ok(None);
    }
    let Some(key) = p["key"].as_str() else {
        return Ok(None);
    };
    let Some(row) = store.effect(run, &EffectKey(key.to_owned())).await? else {
        return Ok(None);
    };
    // Квитанция-успех: конверт `{"outcome": "ok", "value": …}`; результат-ошибка
    // инструмента (`is_error`) изменением не считается.
    let ok = row.result.as_ref().is_some_and(|r| {
        r["outcome"] == "ok" && r["value"].get("is_error").and_then(Value::as_bool) != Some(true)
    });
    if !ok {
        return Ok(None);
    }
    let args = canonical_json(&p["args"]);
    let mut short: String = args.chars().take(STATE_ARGS_CHARS).collect();
    if args.chars().count() > STATE_ARGS_CHARS {
        short.push('…');
    }
    Ok(Some(escape_untrusted(&format!("{name}({short})"))))
}
