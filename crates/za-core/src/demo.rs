//! Демонстрационный run: сквозная проверка журнала и протокола эффектов.
//!
//! Сценарий детерминирован и не ходит в сеть. Шаги нумеруются с 1: перед шагом
//! `n` вызывается [`RunCtx::next_step`], поэтому шаг `n` в журнале — это ключи
//! эффектов `{run}:n:0` и `{run}:n:1`, а в `log.txt` — строка `step n`.
//! На каждом шаге:
//! - эффект `model` класса `Idempotent`: ответ, вычисленный по номеру шага;
//!   в аргументах хэш стабильного префикса демонстрационного промпта;
//! - эффект `tool` класса `NonIdempotent`: дописывает строку `step n` в
//!   `workdir/log.txt`; другие файлы не трогаются.
//!
//! Повтор run берёт завершённые эффекты из журнала, поэтому строка каждого шага
//! пишется ровно один раз. Если исход `tool` неизвестен (намерение без
//! квитанции после сбоя), run не продолжается: [`recover`] открывает
//! прерывание, и [`run_demo`] возвращает [`DemoOutcome::NeedsDecision`].

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use za_llm::{PromptBlocks, prefix_hash};
use za_protocol::{EffectClass, Event, EventKind, RunId, RunRole, RunStatus, Seq, canonical_json};
use za_store::{NewRun, ProjectStore, StoreError};

use crate::recovery::{Recovery, recover};
use crate::runtime::{EffectError, EffectSpec, RunCtx};

/// Файл, в который пишет эффект `tool`, внутри [`DemoConfig::workdir`].
const LOG_FILE: &str = "log.txt";
/// `project_id` run, создаваемого демонстрацией.
const DEMO_PROJECT: &str = "demo";
/// Подстановка вместо id run в [`replay_hash`].
const RUN_PLACEHOLDER: &str = "{run}";
/// Версия формата [`replay_hash`]; входит в хэш.
const REPLAY_TAG: &[u8] = b"za-replay-v1\0";
/// Размер страницы чтения событий в [`replay_hash`].
const PAGE: usize = 1000;

/// Параметры демонстрационного run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemoConfig {
    /// Число шагов; шаги нумеруются с 1.
    pub steps: u32,
    /// Каталог, внутри которого пишется `log.txt`; создаётся при необходимости.
    pub workdir: PathBuf,
    /// Точка аварийного завершения процесса для проверки восстановления.
    pub crash_at: Option<CrashPoint>,
}

/// Точка, в которой процесс завершается `std::process::abort()`; число — номер шага.
///
/// Только для проверки восстановления из отдельного процесса: в тестах внутри
/// процесса не применять, abort завершит процесс тестов.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// После записи намерения `tool` шага и до записи в файл.
    AfterIntent(u32),
    /// После записи в файл и до квитанции `tool`.
    AfterExecute(u32),
}

/// Итог демонстрационного run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemoOutcome {
    /// Все шаги выполнены, run в статусе `done`.
    Done,
    /// Исход `NonIdempotent`-эффектов неизвестен; id открытых прерываний.
    /// Run остаётся в статусе `running`, шаги не выполнялись.
    NeedsDecision(Vec<String>),
}

/// Выполняет или продолжает демонстрационный run.
///
/// Нет run — создаётся (`RunRole::Top`, статус `queued`) и переводится в
/// `running`. Run в `running` (прерван сбоем) сначала проходит [`recover`]:
/// при неизвестном исходе возвращается [`DemoOutcome::NeedsDecision`] без
/// выполнения шагов. Run в `done` возвращает [`DemoOutcome::Done`], ничего не
/// выполняя и не записывая. Иной статус — [`StoreError::InvalidTransition`].
/// После всех шагов run переводится в `done`.
pub async fn run_demo(
    store: &ProjectStore,
    run: RunId,
    cfg: &DemoConfig,
) -> Result<DemoOutcome, EffectError> {
    let current = match store.get_run(run).await? {
        Some(r) => r,
        None => {
            store
                .create_run(NewRun {
                    id: run,
                    project_id: DEMO_PROJECT.to_owned(),
                    parent_id: None,
                    role: RunRole::Top,
                    model_profile: None,
                })
                .await?
        }
    };
    let generation = match current.status {
        RunStatus::Done => {
            tracing::info!(%run, "демонстрационный run уже завершён");
            return Ok(DemoOutcome::Done);
        }
        RunStatus::Queued => {
            store
                .set_status(run, RunStatus::Running, current.generation)
                .await?
        }
        RunStatus::Running => current.generation,
        from => {
            return Err(StoreError::InvalidTransition {
                from,
                to: RunStatus::Running,
            }
            .into());
        }
    };
    if let Recovery::NeedsDecision { interrupt_ids } = recover(store, run).await? {
        return Ok(DemoOutcome::NeedsDecision(interrupt_ids));
    }

    let prefix = demo_prefix_hash();
    let mut ctx = RunCtx::new(store.clone(), run);
    for n in 1..=cfg.steps {
        ctx.next_step();
        match step(&mut ctx, n, &prefix, cfg).await {
            Ok(()) => {}
            Err(EffectError::Unknown { interrupt_id }) => {
                return Ok(DemoOutcome::NeedsDecision(vec![interrupt_id]));
            }
            Err(e) => return Err(e),
        }
    }
    store.set_status(run, RunStatus::Done, generation).await?;
    tracing::info!(%run, steps = cfg.steps, "демонстрационный run завершён");
    Ok(DemoOutcome::Done)
}

/// Шаг `n`: эффект `model`, затем эффект `tool`.
async fn step(ctx: &mut RunCtx, n: u32, prefix: &str, cfg: &DemoConfig) -> Result<(), EffectError> {
    let model = EffectSpec {
        class: EffectClass::Idempotent,
        name: "model".to_owned(),
        args: json!({ "step": n, "prefix_hash": prefix }),
    };
    ctx.effect(model, || async move {
        Ok(json!({ "text": format!("ответ модели на шаг {n}") }))
    })
    .await?;

    let line = format!("step {n}");
    let tool = EffectSpec {
        class: EffectClass::NonIdempotent,
        name: "tool".to_owned(),
        args: json!({ "file": LOG_FILE, "line": line }),
    };
    let crash_at = cfg.crash_at;
    let workdir = cfg.workdir.as_path();
    ctx.effect(tool, || async move {
        crash_if(crash_at == Some(CrashPoint::AfterIntent(n)));
        append_line(workdir, &line).map_err(|e| format!("запись {LOG_FILE}: {e}"))?;
        crash_if(crash_at == Some(CrashPoint::AfterExecute(n)));
        Ok(json!({ "appended": line }))
    })
    .await?;
    Ok(())
}

/// Аварийно завершает процесс в точке [`CrashPoint`].
fn crash_if(hit: bool) {
    if hit {
        tracing::warn!("демонстрационный run: аварийное завершение по crash_at");
        std::process::abort();
    }
}

/// Дописывает строку в `workdir/log.txt` и сбрасывает данные на диск.
fn append_line(workdir: &Path, line: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(workdir)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(workdir.join(LOG_FILE))?;
    f.write_all(format!("{line}\n").as_bytes())?;
    f.sync_data()
}

/// Хэш стабильного префикса демонстрационного промпта.
fn demo_prefix_hash() -> String {
    prefix_hash(&PromptBlocks {
        core: "zero-agent: демонстрационный run".to_owned(),
        ..PromptBlocks::default()
    })
}

/// BLAKE3 (hex) от значимых событий run в порядке `seq`.
///
/// Для каждого события берётся канонический JSON `{actor, kind, payload}`, с
/// префиксом длины (u64 LE); в начале — тег формата. Не входят: `seq`, `id`,
/// `ts`, `run_id`, `parent_seq`, `schema_v`, `blob_ref`, `tainted` события.
/// В payload id run (в `root_id`, ключах эффектов, id прерываний) заменяется на
/// `{run}`, у `interrupt.opened` удаляется `payload.intent_seq` — это `seq`,
/// зависящий от других run в журнале. Иных времён и uuid хранилище в payload не
/// пишет. Поэтому одинаковый сценарий даёт одинаковый хэш при любом времени,
/// id run и соседях по журналу. Неизвестный run даёт [`StoreError::UnknownRun`].
pub async fn replay_hash(store: &ProjectStore, run: RunId) -> Result<String, StoreError> {
    if store.get_run(run).await?.is_none() {
        return Err(StoreError::UnknownRun(run));
    }
    let run_text = run.to_string();
    let mut h = blake3::Hasher::new();
    h.update(REPLAY_TAG);
    let mut after = Seq(0);
    loop {
        let page = store.read_run(run, after, PAGE).await?;
        for e in &page {
            let item = canonical_json(&significant(e, &run_text)?);
            h.update(&(item.len() as u64).to_le_bytes());
            h.update(item.as_bytes());
        }
        match page.last() {
            Some(last) if page.len() == PAGE => after = last.seq,
            _ => break,
        }
    }
    Ok(h.finalize().to_hex().to_string())
}

/// Значимые поля события для [`replay_hash`].
fn significant(e: &Event, run_text: &str) -> Result<Value, StoreError> {
    let actor = serde_json::to_value(e.actor)
        .map_err(|err| StoreError::Sqlite(format!("actor не сериализуется: {err}")))?;
    let mut payload = replace_run(&e.payload, run_text);
    if e.kind == EventKind::InterruptOpened
        && let Some(Value::Object(inner)) = payload.get_mut("payload")
    {
        inner.remove("intent_seq");
    }
    Ok(json!({ "kind": e.kind.as_str(), "actor": actor, "payload": payload }))
}

/// Копия `v`, в строках которой id run заменён на [`RUN_PLACEHOLDER`].
fn replace_run(v: &Value, run_text: &str) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace(run_text, RUN_PLACEHOLDER)),
        Value::Array(a) => Value::Array(a.iter().map(|x| replace_run(x, run_text)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), replace_run(x, run_text)))
                .collect(),
        ),
        other => other.clone(),
    }
}
