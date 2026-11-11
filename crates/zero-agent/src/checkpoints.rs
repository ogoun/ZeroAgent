//! Команда `checkpoints` (R1-C1 D89, D91, D96; TOOL-009).
//!
//! `zero-agent checkpoints list --project P [--json]` — чекпоинты проекта: скрытые ссылки
//! git этого каталога и копии файлов в `ZA_HOME`, строка на чекпоинт (`--json` — JSONL
//! [`Checkpoint`]).
//!
//! `zero-agent checkpoints restore --project P <id> [--delete-new] [--json]` — восстановление
//! рабочей копии по чекпоинту (`za_tools::restore`). Перед ним делается чекпоинт «до»
//! (`za_tools::backup_before_restore`); не вышел — отказ, восстановления нет (I1 ревью
//! задачи 9). После — запись аудита `checkpoint.restored` (actor `user`, target — id) с
//! изменёнными путями и id чекпоинта «до», а при сбое посреди восстановления — ещё
//! `error`. Файлы, которых нет в чекпоинте, удаляются только с `--delete-new`.
//!
//! Доверие (D83, I2): git вызывается только у доверенного проекта — последняя запись
//! аудита доверия `trust.granted` с нынешним набором доверенных файлов. Иначе видны и
//! восстанавливаются только копии файлов, в stderr — подсказка `zero-agent trust`. Проверка
//! доверия перед git — общая с `run` (`trust::trust_state`).
//!
//! Обе команды работают с журналом проекта: его нет (`journal.db` проверяется до
//! открытия) — ошибка «журнал проекта не найден»; журнал держит идущий run —
//! [`CheckpointsCmdError::Busy`] (D91). Открытие повторяется после сбоя
//! (`open_retrying`). Коды выхода: 0 — готово, 1 — ошибка.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use serde_json::json;
use za_core::Checkpoint;
use za_jail::DirectRunner;
use za_protocol::{audit_action, canonical_json};
use za_store::{AuditNew, ProjectStore, StoreError};
use za_tools::{AgentHome, GitCheckpointer, RestoreError, backup_before_restore, restore};

use crate::home;
use crate::run::{RunError, canonical_project, open_existing};
use crate::trust::trust_state;

/// Кто восстанавливает: пользователь через CLI (как у `answer`, D91).
const ACTOR_USER: &str = "user";

/// Ошибка команды `checkpoints`.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointsCmdError {
    /// Журнал держит запущенный run (D91).
    #[error("журнал проекта занят запущенным run; повторите после его остановки")]
    Busy,
    #[error(transparent)]
    Run(RunError),
    #[error(transparent)]
    Store(StoreError),
    #[error(transparent)]
    Restore(#[from] RestoreError),
    #[error("каталог проекта {}: {source}", path.display())]
    Project {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("вывод в JSON: {0}")]
    Json(serde_json::Error),
}

impl From<StoreError> for CheckpointsCmdError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Locked => Self::Busy,
            e => Self::Store(e),
        }
    }
}

impl From<RunError> for CheckpointsCmdError {
    fn from(e: RunError) -> Self {
        match e {
            RunError::Store(s) => s.into(),
            e => Self::Run(e),
        }
    }
}

/// Проект, его журнал (занят — `Busy`) и чекпоинтер.
fn open(
    za_home: &Path,
    project: &Path,
) -> Result<(PathBuf, ProjectStore, GitCheckpointer), CheckpointsCmdError> {
    let project = canonical_project(project)?;
    let store = open_existing(za_home, &project)?;
    let project_id = home::project_id(&project).map_err(|source| CheckpointsCmdError::Project {
        path: project.clone(),
        source,
    })?;
    let ck = GitCheckpointer::new(
        Arc::new(DirectRunner),
        AgentHome {
            za_home: za_home.to_path_buf(),
            project_id,
        },
    );
    Ok((project, store, ck))
}

/// Строка чекпоинта для человека: id, вид, коммит (или причина).
fn line(cp: &Checkpoint) -> String {
    let extra = cp.commit.as_deref().or(cp.reason.as_deref()).unwrap_or("-");
    format!("{} {} {extra}", cp.id, cp.kind)
}

fn to_json(cp: &Checkpoint) -> Result<String, CheckpointsCmdError> {
    let v = serde_json::to_value(cp).map_err(CheckpointsCmdError::Json)?;
    Ok(canonical_json(&v))
}

/// Доверен ли проект сейчас (D83) — единая проверка доверия перед git
/// ([`trust_state`], та же, что у `run`); любой сбой — «не доверен».
async fn trusted_now(store: &ProjectStore, project: &Path) -> bool {
    matches!(trust_state(store, project).await, Ok((true, _)))
}

/// Подсказка недоверенному проекту: чекпоинты git не видны (I2 ревью задачи 9).
fn untrusted_hint() {
    eprintln!(
        "zero-agent: проект не доверен — чекпоинты git не читаются, доступны только копии файлов; доверие: zero-agent trust"
    );
}

/// `zero-agent checkpoints list`.
pub async fn cmd_list(
    za_home: &Path,
    project: &Path,
    json: bool,
) -> Result<ExitCode, CheckpointsCmdError> {
    let (project, store, ck) = open(za_home, project)?;
    let trusted = trusted_now(&store, &project).await;
    if !trusted {
        untrusted_hint();
    }
    let all = ck.list(&project, trusted).await?;
    let mut out = String::new();
    for cp in &all {
        out.push_str(&if json { to_json(cp)? } else { line(cp) });
        out.push('\n');
    }
    print!("{out}");
    if all.is_empty() && !json {
        eprintln!("zero-agent: чекпоинтов нет");
    }
    Ok(ExitCode::SUCCESS)
}

/// `zero-agent checkpoints restore`: найти чекпоинт, сделать чекпоинт «до» (без него —
/// отказ, I1), восстановить и записать аудит — и при неудаче посреди восстановления
/// (M7).
pub async fn cmd_restore(
    za_home: &Path,
    project: &Path,
    id: &str,
    delete_new: bool,
    json: bool,
) -> Result<ExitCode, CheckpointsCmdError> {
    let (project, store, ck) = open(za_home, project)?;
    let trusted = trusted_now(&store, &project).await;
    if !trusted {
        untrusted_hint();
    }
    let cp = ck.find(&project, id, trusted).await?;
    let before = backup_before_restore(&ck, &project, &cp, delete_new, trusted).await?;
    eprintln!(
        "zero-agent: состояние до восстановления — чекпоинт {} ({})",
        before.id, before.kind
    );
    let result = restore(&ck, &project, &cp, delete_new, trusted).await;
    let rels: Vec<String> = result
        .as_ref()
        .map(|changed| {
            changed
                .iter()
                .map(|p| za_core::paths::display(&project, p))
                .collect()
        })
        .unwrap_or_default();
    let mut payload = json!({
        "checkpoint": cp.id,
        "kind": cp.kind,
        "delete_new": delete_new,
        "changed": rels,
        "before": before.id,
        "before_kind": before.kind,
    });
    if let Err(e) = &result {
        payload["error"] = json!(e.to_string());
    }
    store
        .append_audit(AuditNew {
            actor: ACTOR_USER.to_owned(),
            action: audit_action::CHECKPOINT_RESTORED.to_owned(),
            target: cp.id.clone(),
            payload,
        })
        .await?;
    result?;
    if json {
        let v = json!({
            "id": cp.id,
            "kind": cp.kind,
            "changed": rels,
            "before": serde_json::to_value(&before).map_err(CheckpointsCmdError::Json)?,
        });
        println!("{}", canonical_json(&v));
    } else {
        for r in &rels {
            println!("{r}");
        }
        eprintln!(
            "zero-agent: восстановлен чекпоинт {}: изменено файлов {}",
            cp.id,
            rels.len()
        );
    }
    Ok(ExitCode::SUCCESS)
}
