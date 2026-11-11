//! Инструменты run: описание, гейт режима (D14) и реестр по роли и тиру.
//!
//! Инструмент описывает себя [`ToolSpec`] и выполняется через [`Tool::call`].
//! Набор `tools[]` от режима не зависит (CTX-003): режим решает только, можно ли
//! выполнить вызов ([`gate`]); запрещённый вызов получает результат `denied: <причина>`.
//!
//! Для политики R1-C1 инструмент сообщает класс риска ([`RiskClass`], D81) и предметы
//! вызова ([`Tool::subjects`], D77) — сырые пути, рабочий каталог и команду, без
//! ввода-вывода. Результат ([`ToolOutput`], D75) несёт происхождение недоверенного
//! содержимого, сжатое представление и постусловия.

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

/// Класс риска инструмента (R1-C1 D81): по нему политика решает, действует ли метка
/// недоверенного содержимого (D82). Порядок вариантов — от безопасного к опасному.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    /// Только чтение.
    Read,
    /// Правка файлов проекта.
    LocalWrite,
    /// Запуск команд.
    Exec,
    /// Сетевой доступ.
    Network,
    /// Необратимое действие.
    Irreversible,
    /// Публикация наружу.
    Publish,
}

/// Вид доступа к пути вызова (D77).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read,
    Write,
    Exec,
}

/// Путь из аргументов вызова как его дала модель, до разбора [`crate::paths::classify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPath {
    pub arg: String,
    pub access: Access,
}

/// Предметы вызова (D77): что вызов затронет. Строит [`Tool::subjects`] без ввода-вывода;
/// пути разрешает пакет вызовов непосредственно перед решением (D98).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Subjects {
    pub paths: Vec<RawPath>,
    /// Рабочий каталог команды, как в аргументах.
    pub cwd: Option<String>,
    /// Текст команды.
    pub command: Option<String>,
    /// Разобранная команда (D81, D99): сегменты [`crate::shell::segments`]; политика
    /// решает по ним без второго лексера.
    pub segments: Vec<crate::shell::Segment>,
    /// Метки необратимости (`za_protocol::irreversible`).
    pub irreversible: Vec<&'static str>,
    /// Вызов обращается к сети.
    pub network: bool,
}

/// Постусловие результата (TOOL-004, D75): что пакет проверяет после вызова.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Postcondition {
    /// Файл существует и его BLAKE3 (hex) равен `blake3`.
    FileHash { path: PathBuf, blake3: String },
    /// Файла нет.
    FileAbsent { path: PathBuf },
}

/// Описание инструмента.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// Схема для модели; `schema.name` — имя инструмента в реестре.
    pub schema: ToolSchema,
    /// Класс эффекта вызова по идемпотентности: решает повтор при восстановлении
    /// (CORE-003). Гейт режима на него не смотрит, только на `kind`; ТЗ
    /// соответствия `kind` и `class` не задаёт, согласованность (например, `Read`
    /// не бывает `NonIdempotent`) — ответственность автора инструмента.
    pub class: EffectClass,
    pub kind: ToolKind,
    pub approval: Approval,
    /// Класс риска (D81); у `bash` по вызову может повыситься до `Irreversible`.
    pub risk: RiskClass,
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
///
/// Поля R1-C1 (D75) необязательны и не пишутся пустыми: квитанции R1-B
/// `{"content", "is_error"}` читаются, а новые без признаков выглядят как прежде.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// Происхождение недоверенного содержимого (`za_protocol::taint_origin`), если
    /// инструмент сам знает, что результат недоверенный.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taint: Option<String>,
    /// Сжатое представление вывода для контекста при выгрузке (D86).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Постусловия (TOOL-004).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post: Vec<Postcondition>,
}

impl ToolOutput {
    /// Успешный результат без признаков.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Self::default()
        }
    }

    /// Результат-ошибка без признаков.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            ..Self::default()
        }
    }
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
    /// Пути вызова, разрешённые пакетом непосредственно перед вызовом (D98), в порядке
    /// предметов политики (`Subjects::paths`, затем `cwd`); пусто — пакет путей не
    /// разрешал (вызов вне пакета, тест инструмента).
    pub paths: Vec<crate::paths::ResolvedPath>,
}

/// Инструмент run.
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    /// Выполняет вызов. Ошибка инструмента — `ToolOutput { is_error: true, .. }`.
    fn call(&self, args: Value, cx: ToolCx) -> BoxFuture<'static, ToolOutput>;

    /// Предметы вызова (D77): без ввода-вывода. По умолчанию — пусто.
    fn subjects(&self, _args: &Value) -> Subjects {
        Subjects::default()
    }

    /// Предметы вызова относительно канонического корня проекта `root` (D77, D99): пакет
    /// вызывает этот вариант, чтобы лексическая граница команды (`overwrite` и пр.)
    /// считалась от настоящего корня. По умолчанию — [`Tool::subjects`].
    fn subjects_in(&self, _root: &std::path::Path, args: &Value) -> Subjects {
        self.subjects(args)
    }

    /// Тексты для модели, входящие в хэш промптов (D94): `(имя, текст)`.
    fn texts(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }
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
/// и `plan`, `Ask` в `ask` и `accept-edits`, `Allow` в `auto`; в `bypass` (R1-C1 D80)
/// `Edit` и `Exec` — `Allow`. Затем
/// [`Approval::Always`] превращает `Allow` в `Ask`, [`Approval::Never`] — `Ask`
/// в `Allow`; `Deny` не меняется. Класс эффекта (`spec.class`) не учитывается.
pub fn gate(mode: RunMode, spec: &ToolSpec) -> Gate {
    let by_mode = match (spec.kind, mode) {
        (ToolKind::Read | ToolKind::Control, _) => Gate::Allow,
        (ToolKind::Edit | ToolKind::Exec, RunMode::ReadOnly) => Gate::Deny(DENY_READ_ONLY),
        (ToolKind::Edit | ToolKind::Exec, RunMode::Plan) => Gate::Deny(DENY_PLAN),
        (ToolKind::Edit | ToolKind::Exec, RunMode::Ask) => Gate::Ask,
        (ToolKind::Edit, RunMode::AcceptEdits) => Gate::Allow,
        (ToolKind::Exec, RunMode::AcceptEdits) => Gate::Ask,
        (ToolKind::Edit | ToolKind::Exec, RunMode::Auto | RunMode::Bypass) => Gate::Allow,
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
