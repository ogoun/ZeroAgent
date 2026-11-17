//! Память в бинарнике — минимальная форма задачи 5 R1-M: клиент, обнаружение, привязка
//! проекта к решению (D135) и ключи моделей из Vault памяти с ожиданием (D143, D160).
//! Порт памяти run, инструменты, outbox и команды `memory` — задача 12.
//!
//! - **Привязка** ([`read_binding`], [`project_binding`]) — только у доверенного проекта:
//!   `agent_solution_id` из `.zeroaimemory/solution.toml` и `area` из `.zeroaimemory/area.toml`,
//!   оба файла — в наборе доверия ([`crate::trust::TRUSTED_FILES`]) и читаются через
//!   [`read_trusted`] со сверкой хэша. Журнал проекта открывается только на проверку доверия
//!   и закрывается до ожидания Vault: блокировка журнала закрыла бы проект для других команд.
//! - **Ключи, нужные команде** ([`KeyPlan`]) — `key_ref` эндпоинта, который выберет
//!   `Gateway::select` по порядку D24 (флаг модели, роль, `models.default`). У поставщика
//!   поиска `[web.search]` своего `key_ref` нет (SearXNG без ключа, D122): ему раскрывать нечего.
//! - **Ожидание** ([`wait_for_keys`], решение владельца 1): Vault заблокирован или сервер
//!   недоступен — ключ ждёт, строка stderr при каждой смене причины, повтор через 2, 4, 8, 16,
//!   30, 30… с на часах [`Clock`] (D165: это и есть `Sleeper` плана — второй абстракции
//!   времени нет); отмена — будущее `cancel` (в продукте Ctrl+C), код 130, run не создаётся.
//!   `Forbidden`, «не найден», «несколько записей», запись окружения — ошибка без ожидания:
//!   причина уходит в `Inactive` эндпоинта через [`crate::secret::StoreSecrets`].
//! - **Без `[memory]`** ждать нечего: ключей нет, причина «память не настроена ([memory]
//!   url)». Запасного чтения `ZA_SECRET_*` нет. Исключение — сборка с признаком
//!   `test-hooks` без раздела `[memory]`: ключи из `ZA_SECRET_*` ([`EnvStore`]) — для
//!   тестов R1-A…R1-C2 (D143, D128).
//! - **Маска** (D160): значение токена памяти — известное значение маски процесса с
//!   начала команды ([`process_mask`]); каждое раскрытие пополняет её до возврата значения.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use za_llm::EndpointId;
use za_llm::config::ModelsConfig;
use za_memory::config::check_attrib;
use za_memory::{
    Clock, Discovery, KeyState, Revealed, RunBinding, SolutionVault, Token, VaultClient, VaultKeys,
    ZamClient, discover, process_mask,
};
use za_protocol::{RunRole, audit_action};
use za_secrets::{MaskCell, SecretStore};
use za_store::AuditNew;

use crate::config::{AgentConfig, memory_token_from_env};
use crate::journal::open_retrying;
use crate::run::{JOURNAL_FILE, RunError, canonical_project};
use crate::trust::{TrustSet, read_trusted, trust_check};

/// Файл привязки проекта к решению (формат `zam init`: `agent_solution_id = "<GUID>"`, `name`).
pub const SOLUTION_FILE: &str = ".zeroaimemory/solution.toml";
/// Файл области проекта (`area = "…"`, до 128 печатных символов; ZAM его не читает, MEM-012).
pub const AREA_FILE: &str = ".zeroaimemory/area.toml";
/// Паузы ожидания Vault, с: дальше — последняя, без предела (D143).
pub const WAIT_DELAYS: [u64; 5] = [2, 4, 8, 16, 30];
/// Актор записей аудита раскрытия.
const AUDIT_ACTOR: &str = "runtime";

/// Почему памяти у команды нет.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoMemory {
    /// Нет раздела `[memory]` в `ZA_HOME/config.toml`.
    NotConfigured,
    /// `[memory] enabled = false`.
    Disabled,
    /// Клиент не собран: причина без значений.
    Client(String),
}

impl NoMemory {
    /// Причина для человека (`ключ <k> недоступен: <причина>`).
    pub fn reason(&self) -> String {
        match self {
            Self::NotConfigured => "память не настроена ([memory] url)".to_owned(),
            Self::Disabled => "память выключена ([memory] enabled = false)".to_owned(),
            Self::Client(e) => format!("клиент памяти не собран: {e}"),
        }
    }
}

/// Память команды в минимальной форме (задача 5): клиент, обнаружение, привязка и ключи
/// Vault. Задача 12 дополняет порт run, инструменты и outbox.
pub struct MemoryRuntime {
    pub client: Arc<ZamClient>,
    pub discovery: Discovery,
    pub binding: RunBinding,
    /// Ключи из Vault: кэш процесса и маска процесса.
    pub keys: Arc<VaultKeys>,
}

impl fmt::Debug for MemoryRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryRuntime")
            .field("client", &self.client)
            .field("level", &self.discovery.level)
            .field("binding", &self.binding)
            .field("keys", &self.keys)
            .finish()
    }
}

/// Память команды: рантайм или почему его нет.
#[derive(Debug)]
pub enum Memory {
    On(Box<MemoryRuntime>),
    Off(NoMemory),
}

impl Memory {
    pub fn runtime(&self) -> Option<&MemoryRuntime> {
        match self {
            Self::On(rt) => Some(rt),
            Self::Off(_) => None,
        }
    }
}

/// Привязка run к памяти (D135): `solution.toml` и `area.toml` — только у доверенного
/// проекта (`trusted` — его доверенный набор), через [`read_trusted`] (хэш прочитанного
/// сверяется с набором). Недоверенный — без решения и области, `writable: false`. Файл не
/// читается — `warn` в лог; не разбирается или значение негодное — строка stderr; поля нет.
/// Блокирующая.
pub fn read_binding(project: &Path, trusted: Option<&TrustSet>, project_id: &str) -> RunBinding {
    let Some(set) = trusted else {
        return RunBinding {
            project_id: project_id.to_owned(),
            solution: None,
            area: None,
            writable: false,
        };
    };
    let field = |rel: &str, key: &str, check: fn(&str) -> Option<String>| -> Option<String> {
        let text = match read_trusted(project, set, rel) {
            Ok(t) => t?,
            Err(e) => {
                tracing::warn!(file = rel, error = %e, "файл привязки к памяти не прочитан");
                return None;
            }
        };
        let table: toml::Table = match toml::from_str(&text) {
            Ok(t) => t,
            Err(_) => {
                eprintln!("zero-agent: {rel}: файл не разобран (TOML): привязка не действует");
                return None;
            }
        };
        let v = table.get(key).and_then(toml::Value::as_str).and_then(check);
        if v.is_none() {
            eprintln!(
                "zero-agent: {rel}: поле {key} отсутствует или недопустимо: привязка не действует"
            );
        }
        v
    };
    RunBinding {
        project_id: project_id.to_owned(),
        solution: field(SOLUTION_FILE, "agent_solution_id", guid),
        area: field(AREA_FILE, "area", |s| check_attrib(s).ok()),
        writable: true,
    }
}

/// GUID решения: значение заголовка атрибуции из букв, цифр и `-`.
fn guid(s: &str) -> Option<String> {
    check_attrib(s)
        .ok()
        .filter(|g| g.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// Привязка и доверие проекта по журналу.
#[derive(Debug, Clone)]
pub struct ProjectBinding {
    pub binding: RunBinding,
    pub trusted: bool,
}

/// Привязка проекта `project` (канонический путь): доверие (D83) по журналу проекта, затем
/// [`read_binding`]. Журнала нет — проект не доверен, журнал не создаётся. Журнал
/// закрывается до возврата (D160): ожидание Vault его не держит.
pub async fn project_binding(home: &Path, project: &Path) -> Result<ProjectBinding, RunError> {
    let project_id = crate::home::project_id(project).map_err(|source| RunError::Io {
        what: "каталог проекта",
        path: project.to_path_buf(),
        source,
    })?;
    let dir = crate::home::journal_dir(home, project).map_err(|source| RunError::Io {
        what: "каталог проекта",
        path: project.to_path_buf(),
        source,
    })?;
    let set = if dir.join(JOURNAL_FILE).is_file() {
        let store = open_retrying(&dir)?;
        let check = trust_check(&store, project).await?;
        drop(store);
        check.trusted.then_some(check.set)
    } else {
        None
    };
    let trusted = set.is_some();
    let p = project.to_path_buf();
    let binding = tokio::task::spawn_blocking(move || read_binding(&p, set.as_ref(), &project_id))
        .await
        .map_err(|e| RunError::Io {
            what: "привязка проекта к памяти",
            path: project.to_path_buf(),
            source: std::io::Error::other(e.to_string()),
        })?;
    Ok(ProjectBinding { binding, trusted })
}

/// Привязка без проекта или когда журнал не читается: не доверен, без решения.
fn untrusted(project_id: &str) -> RunBinding {
    read_binding(Path::new(""), None, project_id)
}

/// Память команды по `[memory]` (D134): клиент с токеном, обнаружение (`/api/v1/health`),
/// привязка и ключи Vault с маской процесса `mask`. Нет раздела, память выключена, клиент
/// не собран — [`Memory::Off`] с причиной.
pub async fn memory_runtime(
    cfg: &AgentConfig,
    binding: RunBinding,
    token: Option<Token>,
    mask: &Arc<MaskCell>,
) -> Memory {
    let Some(mc) = cfg.memory.as_ref() else {
        return Memory::Off(NoMemory::NotConfigured);
    };
    if !mc.enabled {
        return Memory::Off(NoMemory::Disabled);
    }
    let client = match ZamClient::new(mc, token) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("zero-agent: {e}: клиент памяти не собран, работа без памяти (L0)");
            return Memory::Off(NoMemory::Client(e.reason()));
        }
    };
    let discovery = discover(&client).await;
    let keys = Arc::new(VaultKeys::new(
        VaultClient::new(Arc::clone(&client)),
        SolutionVault::of(&binding),
        Arc::clone(mask),
    ));
    Memory::On(Box::new(MemoryRuntime {
        client,
        discovery,
        binding,
        keys,
    }))
}

/// Память команды без своего проекта (`ask`, `models probe`, `secret list`): проект —
/// `project` или текущий каталог, привязка — если он доверен; журнал не читается — без
/// решения (`warn`). Маска процесса — с токеном памяти.
pub async fn command_memory(
    home: &Path,
    cfg: &AgentConfig,
    project: Option<&Path>,
) -> (Memory, Arc<MaskCell>) {
    let token = memory_token_from_env();
    let cell = Arc::new(process_mask(token.as_ref()));
    let binding = if cfg.memory.as_ref().is_some_and(|m| m.enabled) {
        let dir = match project {
            Some(p) => Some(p.to_path_buf()),
            None => std::env::current_dir().ok(),
        };
        match dir.as_deref().map(canonical_project) {
            Some(Ok(p)) => match project_binding(home, &p).await {
                Ok(b) => b.binding,
                Err(e) => {
                    tracing::warn!(error = %e, "привязка проекта к памяти не прочитана");
                    untrusted("")
                }
            },
            _ => untrusted(""),
        }
    } else {
        untrusted("")
    };
    let mem = memory_runtime(cfg, binding, token, &cell).await;
    (mem, cell)
}

/// Ключи, нужные команде (D143): `key_ref` по порядку D24, без повторов.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyPlan {
    pub names: Vec<String>,
}

impl KeyPlan {
    /// Ключ эндпоинта `id`; без `key_ref` или неизвестный эндпоинт — пусто.
    pub fn for_endpoint(models: Option<&ModelsConfig>, id: &EndpointId) -> Self {
        let names = models
            .and_then(|m| m.endpoint(id))
            .and_then(|e| e.key_ref.clone())
            .into_iter()
            .collect();
        Self { names }
    }

    /// Ключ эндпоинта, который выберет `Gateway::select` (D24): `choice`, затем эндпоинт
    /// роли `role`, затем `models.default`.
    pub fn for_selection(
        models: Option<&ModelsConfig>,
        choice: Option<&EndpointId>,
        role: Option<RunRole>,
    ) -> Self {
        let Some(m) = models else {
            return Self::default();
        };
        let id = choice
            .cloned()
            .or_else(|| role.and_then(|r| m.role(r).cloned()))
            .or_else(|| m.default.clone());
        id.map(|id| Self::for_endpoint(Some(m), &id))
            .unwrap_or_default()
    }
}

/// Откуда ключи кольца.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// Vault памяти (раскрыты ключи плана).
    Vault,
    /// `ZA_SECRET_*` — только сборка с `test-hooks` без `[memory]`.
    Env,
    /// Памяти нет: причина.
    Off(NoMemory),
}

/// Ключи команды после фазы раскрытия: значения, причины «нет ключа» и раскрытия для журнала
/// и аудита. `Debug` — без значений.
#[derive(Clone)]
pub struct KeyRing {
    pub source: KeySource,
    values: BTreeMap<String, String>,
    /// `key_ref` → причина «нет ключа» целиком (`ключ <k> …`).
    missing: BTreeMap<String, String>,
    /// Раскрытия этой команды по порядку, у каждого имени — одно (D143).
    pub revealed: Vec<Revealed>,
}

impl fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyRing")
            .field("source", &self.source)
            .field("names", &self.values.keys().collect::<Vec<_>>())
            .field("missing", &self.missing)
            .field("revealed", &self.revealed)
            .finish()
    }
}

impl KeyRing {
    fn empty(source: KeySource) -> Self {
        Self {
            source,
            values: BTreeMap::new(),
            missing: BTreeMap::new(),
            revealed: Vec::new(),
        }
    }

    /// Кольцо команды, которая ключи не раскрывает (`models`, `doctor`, `inspect`,
    /// `resources`, D143): пустое; в сборке с `test-hooks` без `[memory]` — `ZA_SECRET_*`.
    pub fn unrevealed(cfg: &AgentConfig) -> Self {
        match &cfg.memory {
            None => Self::without_memory(&NoMemory::NotConfigured),
            Some(m) if !m.enabled => Self::without_memory(&NoMemory::Disabled),
            Some(_) => Self::empty(KeySource::Vault),
        }
    }

    /// Без памяти: ключей нет (решение владельца 1, без запасного источника); в сборке с
    /// `test-hooks` без `[memory]` — `ZA_SECRET_*` (D143).
    fn without_memory(why: &NoMemory) -> Self {
        #[cfg(feature = "test-hooks")]
        if *why == NoMemory::NotConfigured {
            return Self::from_env();
        }
        Self::empty(KeySource::Off(why.clone()))
    }

    /// `ZA_SECRET_*` процесса (`EnvStore`) — только тестовая сборка (D143, D128).
    #[cfg(feature = "test-hooks")]
    fn from_env() -> Self {
        let store = za_secrets::EnvStore::from_vars(std::env::vars_os());
        let values = store
            .names()
            .into_iter()
            .filter_map(|n| store.get(&n).map(|v| (n, v)))
            .collect();
        Self {
            values,
            ..Self::empty(KeySource::Env)
        }
    }

    /// Значения ключей (имя → значение).
    pub fn values(&self) -> &BTreeMap<String, String> {
        &self.values
    }

    /// Причины «нет ключа» по `key_ref`, которые назвала фаза раскрытия.
    pub fn missing(&self) -> &BTreeMap<String, String> {
        &self.missing
    }

    /// Причина «нет ключа» для `key_ref` (`SecretSource::missing_reason`, D143).
    pub fn missing_reason(&self, key_ref: &str) -> String {
        match self.missing.get(key_ref) {
            Some(r) => r.clone(),
            None => fallback_reason(&self.source, key_ref),
        }
    }
}

/// Причина «нет ключа» для `key_ref`, о котором фаза раскрытия ничего не сказала: ключ не из
/// плана команды (другой эндпоинт), памяти нет или источник — `ZA_SECRET_*` тестовой сборки.
pub fn fallback_reason(source: &KeySource, key_ref: &str) -> String {
    match source {
        KeySource::Env => za_llm::http::missing_key_reason(key_ref),
        KeySource::Off(why) => format!("ключ {key_ref} недоступен: {}", why.reason()),
        KeySource::Vault => format!(
            "ключ {key_ref} не раскрыт: команда раскрывает из Vault памяти только нужные ей ключи (run, ask и models probe — ключ выбранной модели)"
        ),
    }
}

/// Отказ ожидания ключей.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WaitError {
    /// Отмена во время ожидания (Ctrl+C): код 130, run не создаётся.
    #[error("ожидание Vault памяти отменено")]
    Cancelled,
}

/// Итог одной попытки: кольцо и ключи, которые ждут (имя → причина).
async fn attempt(
    mem: &Memory,
    plan: &KeyPlan,
    session: Option<&str>,
) -> (KeyRing, BTreeMap<String, String>) {
    let rt = match mem {
        Memory::Off(why) => {
            let mut ring = KeyRing::without_memory(why);
            if ring.source != KeySource::Env {
                for k in &plan.names {
                    ring.missing
                        .insert(k.clone(), format!("ключ {k} недоступен: {}", why.reason()));
                }
            }
            return (ring, BTreeMap::new());
        }
        Memory::On(rt) => rt,
    };
    let mut ring = KeyRing::empty(KeySource::Vault);
    if plan.names.is_empty() {
        return (ring, BTreeMap::new());
    }
    if !rt.client.has_token() {
        for k in &plan.names {
            ring.missing.insert(
                k.clone(),
                format!("ключ {k} недоступен: токен памяти не задан (ZA_MEMORY_TOKEN)"),
            );
        }
        return (ring, BTreeMap::new());
    }
    let (revealed, states) = rt.keys.reveal(&plan.names, session).await;
    ring.revealed = revealed;
    let mut waiting = BTreeMap::new();
    for (k, st) in states {
        match st {
            KeyState::Ready => {
                if let Some(v) = rt.keys.get(&k) {
                    ring.values.insert(k, v);
                }
            }
            KeyState::Waiting(r) => {
                ring.missing
                    .insert(k.clone(), format!("ключ {k} ждёт Vault памяти: {r}"));
                waiting.insert(k, r);
            }
            KeyState::Missing(r) => {
                ring.missing.insert(k.clone(), format!("ключ {k}: {r}"));
            }
        }
    }
    (ring, waiting)
}

/// Одна попытка раскрытия без ожидания (`models probe`, D143): ключ, который ждал бы Vault, —
/// причина «ждёт Vault памяти» в `Inactive`.
pub async fn reveal_once(mem: &Memory, plan: &KeyPlan, session: Option<&str>) -> KeyRing {
    attempt(mem, plan, session).await.0
}

/// Ключи плана с ожиданием Vault (D143, решение владельца 1): пока хоть один ключ ждёт —
/// строка stderr при смене причины и повтор через [`WAIT_DELAYS`] на часах `clock`; `cancel`
/// прерывает и запрос, и паузу ([`WaitError::Cancelled`]). Ошибки без ожидания — причины в
/// кольце. Значения раскрытых ключей — уже в маске процесса.
pub async fn wait_for_keys(
    mem: &Memory,
    plan: &KeyPlan,
    session: Option<&str>,
    clock: &dyn Clock,
    cancel: impl Future<Output = ()>,
) -> Result<KeyRing, WaitError> {
    tokio::pin!(cancel);
    let mut delays = WAIT_DELAYS.iter().copied();
    let longest = WAIT_DELAYS[WAIT_DELAYS.len() - 1];
    let mut shown: BTreeMap<String, String> = BTreeMap::new();
    let mut revealed: Vec<Revealed> = Vec::new();
    loop {
        let (mut ring, waiting) = tokio::select! {
            biased;
            () = &mut cancel => return Err(WaitError::Cancelled),
            r = attempt(mem, plan, session) => r,
        };
        for r in std::mem::take(&mut ring.revealed) {
            if !revealed.iter().any(|x| x.name == r.name) {
                revealed.push(r);
            }
        }
        if waiting.is_empty() {
            ring.revealed = revealed;
            return Ok(ring);
        }
        let secs = delays.next().unwrap_or(longest);
        for (k, reason) in &waiting {
            if shown.get(k) != Some(reason) {
                eprintln!(
                    "zero-agent: ключ {k} ждёт Vault памяти: {reason}; повтор через {secs} с (Ctrl+C — отмена)"
                );
                shown.insert(k.clone(), reason.clone());
            }
        }
        tokio::select! {
            biased;
            () = &mut cancel => return Err(WaitError::Cancelled),
            () = clock.sleep(Duration::from_secs(secs)) => {}
        }
    }
}

/// Payload события `secret.revealed {name, scope, item, cached}` (D143, D153): без значения.
pub fn revealed_event(r: &Revealed) -> Value {
    json!({"name": r.name, "scope": r.scope, "item": r.item, "cached": r.cached})
}

/// Запись аудита проекта `secret.revealed {name, scope, item, session}` (D143): без значения.
pub fn revealed_audit(r: &Revealed, session: Option<&str>) -> AuditNew {
    AuditNew {
        actor: AUDIT_ACTOR.to_owned(),
        action: audit_action::SECRET_REVEALED.to_owned(),
        target: r.name.clone(),
        payload: json!({"name": r.name, "scope": r.scope, "item": r.item, "session": session}),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use za_llm::config::ModelsConfig;

    use super::*;
    use crate::trust::trust_set;

    /// Свой каталог на процесс и вызов `.tmp/zero-agent-unit/<name>-<pid>-<n>`.
    fn dir(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.tmp/zero-agent-unit");
        crate::test_unique_dir::unique_dir_in(&root, name)
    }

    const GUID: &str = "0192f1c4-7a1e-7c3b-9a55-2b1f6f0d9e77";

    fn write(p: &Path, rel: &str, text: &str) {
        let f = p.join(rel);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, text).unwrap();
    }

    /// D135: решение и область — только у доверенного проекта, по доверенному набору; файл,
    /// изменённый после проверки доверия, и негодный GUID — без решения.
    #[test]
    fn binding_only_for_trusted_with_valid_files() {
        let p = dir("binding");
        write(
            &p,
            SOLUTION_FILE,
            &format!("agent_solution_id = \"{GUID}\"\nname = \"x\"\n"),
        );
        write(&p, AREA_FILE, "area = \"ядро\"\n");
        let set = trust_set(&p).unwrap();
        let b = read_binding(&p, Some(&set), "pid");
        assert_eq!(
            b,
            RunBinding {
                project_id: "pid".into(),
                solution: Some(GUID.into()),
                area: Some("ядро".into()),
                writable: true,
            }
        );
        assert_eq!(SolutionVault::of(&b), SolutionVault::Bound(GUID.into()));
        let u = read_binding(&p, None, "pid");
        assert_eq!((u.solution, u.area, u.writable), (None, None, false));
        // Правка после проверки доверия: хэш не совпал — файл не читается.
        write(&p, SOLUTION_FILE, "agent_solution_id = \"other\"\n");
        assert_eq!(read_binding(&p, Some(&set), "pid").solution, None);
        // Негодный GUID в доверенном файле — без решения; доверенный без решения — `Unbound`.
        write(&p, SOLUTION_FILE, "agent_solution_id = \"../evil/x\"\n");
        let set = trust_set(&p).unwrap();
        let b = read_binding(&p, Some(&set), "pid");
        assert_eq!(b.solution, None);
        assert!(b.writable);
        assert_eq!(SolutionVault::of(&b), SolutionVault::Unbound);
    }

    fn models(toml_text: &str) -> ModelsConfig {
        toml::from_str(toml_text).unwrap()
    }

    /// D24: ключ эндпоинта, который выберет `Gateway::select` — флаг, роль, `models.default`;
    /// без `key_ref` — пусто.
    #[test]
    fn key_plan_follows_selection_order() {
        let m = models(
            "default = \"local\"\n[roles]\ntop = \"cloud\"\n\n[[endpoints]]\nid = \"local\"\nbackend = \"llama_cpp\"\nbase_url = \"http://127.0.0.1:1\"\n\n[[endpoints]]\nid = \"cloud\"\nbackend = \"anthropic\"\nmodel = \"m\"\nkey_ref = \"an\"\n\n[[endpoints]]\nid = \"router\"\nbackend = \"openrouter\"\nmodel = \"m\"\nkey_ref = \"or\"\n",
        );
        let names = |p: KeyPlan| p.names;
        let id = |s: &str| EndpointId(s.to_owned());
        assert_eq!(
            names(KeyPlan::for_selection(
                Some(&m),
                Some(&id("router")),
                Some(RunRole::Top)
            )),
            ["or"]
        );
        assert_eq!(
            names(KeyPlan::for_selection(Some(&m), None, Some(RunRole::Top))),
            ["an"]
        );
        assert!(names(KeyPlan::for_selection(Some(&m), None, None)).is_empty());
        assert!(names(KeyPlan::for_endpoint(Some(&m), &id("nope"))).is_empty());
        assert!(names(KeyPlan::for_selection(None, None, None)).is_empty());
    }

    /// Причины «нет ключа» без раскрытия: памяти нет, ключ не из плана команды.
    #[test]
    fn fallback_reasons_name_the_cause() {
        assert_eq!(
            fallback_reason(&KeySource::Off(NoMemory::NotConfigured), "x"),
            "ключ x недоступен: память не настроена ([memory] url)"
        );
        assert!(fallback_reason(&KeySource::Vault, "x").starts_with("ключ x не раскрыт"));
        assert!(fallback_reason(&KeySource::Env, "x").contains("ZA_SECRET_X"));
    }
}
