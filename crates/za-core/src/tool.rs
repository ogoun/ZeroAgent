//! Инструменты run: описание, гейт режима (D14) и реестр по роли и тиру.
//!
//! Инструмент описывает себя [`ToolSpec`] и выполняется через [`Tool::call`].
//! Набор `tools[]` от режима не зависит (CTX-003): режим решает только, можно ли
//! выполнить вызов ([`gate`]); запрещённый вызов получает результат `denied: <причина>`.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use za_llm::{BoxFuture, ToolSchema};
use za_protocol::{EffectClass, EffectKey, RunId, RunMode, RunRole, Tier};

/// Вид действия инструмента: по нему режим решает, нужен ли гейт.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// Чтение: разрешено в любом режиме.
    Read,
    /// Правка файлов проекта.
    Edit,
    /// Запуск команд.
    Exec,
    /// Управление run (вопрос пользователю, завершение): разрешено всегда.
    Control,
}

/// Поправка инструмента к решению режима.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Решает режим.
    Mode,
    /// Всегда спрашивать: `Allow` режима становится `Ask`.
    Always,
    /// Не спрашивать: `Ask` режима становится `Allow`.
    Never,
}

/// Описание инструмента.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// Схема для модели; `schema.name` — имя инструмента в реестре.
    pub schema: ToolSchema,
    /// Класс эффекта вызова по идемпотентности.
    pub class: EffectClass,
    pub kind: ToolKind,
    pub approval: Approval,
    /// Роли run, которым инструмент доступен.
    pub roles: Vec<RunRole>,
    /// Наименьший тир модели, которому инструмент выдаётся.
    pub min_tier: Tier,
    /// Предел времени вызова.
    pub timeout: Duration,
    /// Предел размера результата в байтах; `None` — без предела инструмента.
    pub output_limit: Option<usize>,
    /// Результат несёт недоверенные данные извне (помечается `tainted`).
    pub tainted: bool,
}

/// Результат вызова инструмента; сериализуется в квитанцию эффекта.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

/// Контекст одного вызова инструмента.
#[derive(Debug, Clone)]
pub struct ToolCx {
    pub run: RunId,
    /// Корень проекта run.
    pub project_dir: PathBuf,
    /// Id вызова от модели.
    pub call_id: String,
    /// Ключ эффекта вызова: для идемпотентности на стороне инструмента.
    pub effect_key: EffectKey,
}

/// Инструмент run.
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    /// Выполняет вызов. Ошибка инструмента — `ToolOutput { is_error: true, .. }`.
    fn call(&self, args: Value, cx: ToolCx) -> BoxFuture<'static, ToolOutput>;
}

/// Решение гейта о вызове.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Allow,
    /// Нужно одобрение пользователя.
    Ask,
    /// Запрещено режимом; строка — причина для `denied: <причина>`.
    Deny(&'static str),
}

/// Причина запрета в режиме `read-only`.
const DENY_READ_ONLY: &str = "read-only mode";
/// Причина запрета в режиме `plan`.
const DENY_PLAN: &str = "plan mode";

/// Таблица D14. Тексты Deny: "read-only mode", "plan mode".
///
/// `Read` и `Control` разрешены всегда; `Edit` — `Deny` в `read-only` и `plan`,
/// `Ask` в `ask`, `Allow` в `accept-edits` и `auto`; `Exec` — `Deny` в `read-only`
/// и `plan`, `Ask` в `ask` и `accept-edits`, `Allow` в `auto`. Затем
/// [`Approval::Always`] превращает `Allow` в `Ask`, [`Approval::Never`] — `Ask`
/// в `Allow`; `Deny` не меняется.
pub fn gate(mode: RunMode, spec: &ToolSpec) -> Gate {
    let by_mode = match (spec.kind, mode) {
        (ToolKind::Read | ToolKind::Control, _) => Gate::Allow,
        (ToolKind::Edit | ToolKind::Exec, RunMode::ReadOnly) => Gate::Deny(DENY_READ_ONLY),
        (ToolKind::Edit | ToolKind::Exec, RunMode::Plan) => Gate::Deny(DENY_PLAN),
        (ToolKind::Edit | ToolKind::Exec, RunMode::Ask) => Gate::Ask,
        (ToolKind::Edit, RunMode::AcceptEdits) => Gate::Allow,
        (ToolKind::Exec, RunMode::AcceptEdits) => Gate::Ask,
        (ToolKind::Edit | ToolKind::Exec, RunMode::Auto) => Gate::Allow,
    };
    match (by_mode, spec.approval) {
        (Gate::Allow, Approval::Always) => Gate::Ask,
        (Gate::Ask, Approval::Never) => Gate::Allow,
        (g, _) => g,
    }
}

/// Повторная регистрация имени в [`ToolRegistry`].
#[derive(Debug, thiserror::Error)]
#[error("инструмент {0} уже зарегистрирован")]
pub struct DuplicateTool(pub String);

/// Реестр инструментов по имени.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Регистрирует инструмент под `spec().schema.name`; занятое имя —
    /// [`DuplicateTool`], первая регистрация остаётся.
    pub fn register(&mut self, t: Arc<dyn Tool>) -> Result<(), DuplicateTool> {
        match self.tools.entry(t.spec().schema.name.clone()) {
            Entry::Occupied(e) => Err(DuplicateTool(e.key().clone())),
            Entry::Vacant(e) => {
                e.insert(t);
                Ok(())
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Инструменты роли с `min_tier <= tier`, по имени.
    pub fn for_run(&self, role: RunRole, tier: Tier) -> Vec<Arc<dyn Tool>> {
        self.tools
            .values()
            .filter(|t| {
                let s = t.spec();
                s.min_tier <= tier && s.roles.contains(&role)
            })
            .cloned()
            .collect()
    }
}
