//! Контракт чекпоинта рабочей копии (R1-C1 D89, TOOL-009): перед каждым исполняемым
//! вызовом вида `Edit` или `Exec` пакет вызовов просит [`Checkpointer`] сохранить
//! состояние. Реализация (git plumbing или копия файлов) — в `za-tools`; здесь только
//! типы, общие для `za-agent` и `za-tools` (D99).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use za_protocol::{EffectKey, RunId};

use crate::BoxFuture;
use crate::tool::ToolKind;

/// Запрос чекпоинта перед вызовом.
#[derive(Debug, Clone)]
pub struct CheckpointRequest {
    /// Канонический корень проекта.
    pub project_dir: PathBuf,
    pub run: RunId,
    /// Ключ эффекта вызова: от него имя чекпоинта (`EffectKey::file_stem`).
    pub effect_key: EffectKey,
    /// Вид вызова: `Edit` или `Exec`.
    pub kind: ToolKind,
    /// Затронутые пути (канонические); у `Exec` может быть пусто.
    pub paths: Vec<PathBuf>,
    /// Проект доверен (D83): в недоверенном git не вызывается.
    pub trusted: bool,
}

/// Сделанный (или пропущенный) чекпоинт: payload события `checkpoint.created`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// `"<run>/<step>-<seq>"`; повтор с тем же ключом эффекта возвращает тот же чекпоинт.
    pub id: String,
    /// Вид (`za_protocol::checkpoint_kind`): `git`, `files`, `skipped`.
    pub kind: String,
    /// Ссылка git (`refs/zero-agent/checkpoints/…`) у вида `git`.
    pub git_ref: Option<String>,
    /// Коммит у вида `git`.
    pub commit: Option<String>,
    /// Причина у вида `skipped`.
    pub reason: Option<String>,
    /// Затронутые вызовом файлы (от корня проекта через `/`), не вошедшие в чекпоинт как
    /// секреты (R1-C2 D103): по имени или по содержимому. Пусто — поле не пишется.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_skipped: Vec<String>,
}

/// Делает чекпоинт. Сбой — не ошибка, а `Checkpoint` вида `skipped` с причиной: вызов
/// всё равно исполняется (D89).
pub trait Checkpointer: Send + Sync {
    fn create(&self, req: CheckpointRequest) -> BoxFuture<'static, Checkpoint>;
}
