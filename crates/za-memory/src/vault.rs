//! Записи Vault памяти по имени `key_ref` (R1-M D143, D144; решение владельца 1; MEM-110…112).
//!
//! - **Поиск** ([`VaultClient::find`]): имя записи — `key_ref` из `config.toml`. Сначала Vault
//!   решения проекта (`GET /api/v1/spaces/{guid}/vault`, только у доверенного проекта с
//!   решением, D135), затем личный (`GET /api/v1/spaces/knowledge/vault`). В каждом —
//!   точное сравнение `title` с учётом регистра, затем единственное совпадение без учёта
//!   регистра; несколько совпадений — [`VaultError::Ambiguous`] без догадки. Совпадение в
//!   Vault решения закрывает личный: запись решения важнее.
//! - **Окружение** — записи `host` и `environment` (адреса, назначение, ограничения, без
//!   паролей) — ключом не считаются: совпадение с ними — [`VaultError::NotAKey`] без
//!   раскрытия.
//! - **Список** — только метаданные: значений в нём нет, аудита у сервера нет. `?name=` не
//!   шлётся (сервер молча отдаёт все записи, CR-24), `?q=` тоже (FTS режет имена по словам,
//!   а список мал). Открытый список (`locked: false`) кэшируется на процесс; список
//!   заблокированного Vault — нет: следующая попытка ожидания должна увидеть разблокировку.
//! - **Раскрытие** ([`VaultClient::reveal`]) — `GET /api/v1/vault/{id}?reveal=1`; у `run` и
//!   resume — с `X-Zam-Session` этого run (аудит ZAM `vault.reveal {item, session, machine}`),
//!   у `ask` и `models probe` — без сессии. Заблокированный Vault — 503 `/problems/vault-locked`
//!   ([`VaultError::Locked`]).
//!
//! Значения не попадают в `Debug`, логи и тексты ошибок: [`VaultClient::reveal`] отдаёт строку
//! вызывающему ([`crate::keys::VaultKeys`]), ошибки — без тела ответа со значением.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;

use crate::error::{ErrorKind, ZamError};
use crate::transport::{Ctx, VAULT_LIMIT, ZamClient};

/// Пространство личного Vault в пути (`/api/v1/spaces/knowledge/vault`).
pub const KNOWLEDGE: &str = "knowledge";
/// Типы записей окружения (`data.type`, `VAULT_ENV_TYPES` ZAM): ключом не считаются.
pub const ENV_TYPES: [&str; 2] = ["host", "environment"];
/// Причина пропуска Vault решения: проект не доверен (D135).
pub const SKIP_UNTRUSTED: &str = "project not trusted";
/// Причина пропуска Vault решения: доверенный проект не привязан к решению.
pub const SKIP_NO_SOLUTION: &str = "project has no memory solution";

/// Область записи Vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultScope {
    /// Vault решения: GUID решения.
    Solution(String),
    /// Личный Vault пользователя токена.
    Knowledge,
}

impl VaultScope {
    /// Имя области в журнале и аудите: `solution` или `knowledge`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Solution(_) => "solution",
            Self::Knowledge => KNOWLEDGE,
        }
    }

    /// Сегмент пути списка: GUID решения или `knowledge`.
    fn space(&self) -> &str {
        match self {
            Self::Solution(g) => g,
            Self::Knowledge => KNOWLEDGE,
        }
    }
}

/// Vault решения проекта для поиска (D135): привязка доверенного проекта или почему её нет.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolutionVault {
    /// Доверенный проект с решением: GUID.
    Bound(String),
    /// Доверенный проект без решения (`zero-agent memory init` не делался).
    Unbound,
    /// Проект не доверен: Vault решения не читается.
    Untrusted,
}

impl SolutionVault {
    /// По привязке run: решение есть — `Bound`; нет у доверенного (`writable`) — `Unbound`;
    /// у недоверенного — `Untrusted`.
    pub fn of(binding: &crate::session::RunBinding) -> Self {
        match (&binding.solution, binding.writable) {
            (Some(g), _) => Self::Bound(g.clone()),
            (None, true) => Self::Unbound,
            (None, false) => Self::Untrusted,
        }
    }

    /// Почему Vault решения не смотрелся; `None` — смотрелся.
    fn skipped(&self) -> Option<&'static str> {
        match self {
            Self::Bound(_) => None,
            Self::Unbound => Some(SKIP_NO_SOLUTION),
            Self::Untrusted => Some(SKIP_UNTRUSTED),
        }
    }
}

/// Запись Vault без значения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultItem {
    pub id: String,
    pub title: String,
    pub scope: VaultScope,
    /// `data.type` (`token`, `password`, `host`, …).
    pub item_type: String,
    pub rev: i64,
}

impl VaultItem {
    /// Запись окружения (`host`, `environment`): не ключ.
    pub fn is_environment(&self) -> bool {
        ENV_TYPES.contains(&self.item_type.as_str())
    }
}

/// Ошибка поиска или раскрытия. Тексты — для человека, без значений.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultError {
    /// Vault заблокирован (`locked: true` списка или 503 `vault-locked`): причина сервера.
    Locked(String),
    /// 403: нет доступа к Vault решения (`solution-access`) или к записи.
    Forbidden,
    /// Записи с таким именем нет; `solution_skipped` — почему Vault решения не смотрелся
    /// ([`SKIP_UNTRUSTED`], [`SKIP_NO_SOLUTION`]).
    NotFound {
        solution_skipped: Option<&'static str>,
    },
    /// Несколько записей без учёта регистра: их имена.
    Ambiguous(Vec<String>),
    /// Запись окружения (`host`, `environment`) или запись без значения.
    NotAKey,
    /// Сервер не ответил или ответил ошибкой.
    Unavailable(ZamError),
}

impl VaultError {
    /// Ожидание имеет смысл (решение владельца 1): Vault заблокирован, сервер недоступен
    /// (не соединились, срок, обрыв, «не ZAM»), временная ошибка сервера. `Forbidden`,
    /// `NotFound`, `Ambiguous`, `NotAKey`, отказ токена — ошибка без ожидания.
    pub fn is_waitable(&self) -> bool {
        match self {
            Self::Locked(_) => true,
            Self::Unavailable(e) => {
                e.is_unreachable()
                    || matches!(
                        e.kind,
                        ErrorKind::Server
                            | ErrorKind::Busy
                            | ErrorKind::RateLimited
                            | ErrorKind::RequestTimeout
                            | ErrorKind::VaultLocked
                    )
            }
            Self::Forbidden | Self::NotFound { .. } | Self::Ambiguous(_) | Self::NotAKey => false,
        }
    }
}

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Locked(reason) if reason.trim().is_empty() => f.write_str("Vault заблокирован"),
            Self::Locked(reason) => write!(f, "Vault заблокирован ({reason})"),
            Self::Forbidden => f.write_str("нет доступа к Vault решения"),
            Self::NotFound { solution_skipped } => {
                f.write_str("не найден в Vault памяти (Vault решения, затем личный)")?;
                match *solution_skipped {
                    Some(SKIP_UNTRUSTED) => f.write_str(
                        "; Vault решения не читается: проект не доверен (zero-agent trust)",
                    ),
                    Some(SKIP_NO_SOLUTION) => f.write_str(
                        "; Vault решения не читается: проект не привязан к решению (zero-agent memory init)",
                    ),
                    Some(other) => write!(f, "; Vault решения не читается: {other}"),
                    None => Ok(()),
                }
            }
            Self::Ambiguous(titles) => write!(
                f,
                "в Vault несколько записей без учёта регистра ({}): назовите key_ref точно",
                titles.join(", ")
            ),
            Self::NotAKey => f.write_str(
                "запись Vault — окружение (host, environment) или без значения, не ключ",
            ),
            Self::Unavailable(e) if e.is_unreachable() => {
                write!(f, "сервер памяти недоступен: {e}")
            }
            Self::Unavailable(e) => write!(f, "Vault памяти: {e}"),
        }
    }
}

impl std::error::Error for VaultError {}

/// Список области: записи и признак блокировки.
#[derive(Debug, Clone)]
struct Listing {
    items: Vec<VaultItem>,
    /// `Some(reason)` — Vault заблокирован.
    locked: Option<String>,
}

/// Клиент Vault: поиск по имени, раскрытие, записи окружения. Открытые списки кэшируются на
/// процесс (D143).
pub struct VaultClient {
    client: Arc<ZamClient>,
    lists: Mutex<BTreeMap<String, Listing>>,
}

impl fmt::Debug for VaultClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cached: Vec<String> = self.lists().keys().cloned().collect();
        f.debug_struct("VaultClient")
            .field("client", &self.client)
            .field("cached", &cached)
            .finish()
    }
}

/// Сегмент пути (GUID решения, id записи): буквы, цифры, `-`, `_`, до 64 байт; иначе запрос не
/// идёт.
fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn protocol(title: &str) -> VaultError {
    VaultError::Unavailable(ZamError::new(ErrorKind::Protocol, title))
}

/// Ошибка сервера как ошибка Vault: 503 `vault-locked` — `Locked` (причина — `detail`), 403 —
/// `Forbidden`, 404 — `NotFound`, прочее — `Unavailable`.
fn vault_error(e: ZamError) -> VaultError {
    match e.kind {
        ErrorKind::VaultLocked => VaultError::Locked(if e.detail.is_empty() {
            e.title
        } else {
            e.detail
        }),
        ErrorKind::Forbidden => VaultError::Forbidden,
        ErrorKind::NotFound => VaultError::NotFound {
            solution_skipped: None,
        },
        _ => VaultError::Unavailable(e),
    }
}

impl VaultClient {
    pub fn new(client: Arc<ZamClient>) -> Self {
        Self {
            client,
            lists: Mutex::new(BTreeMap::new()),
        }
    }

    fn lists(&self) -> MutexGuard<'_, BTreeMap<String, Listing>> {
        self.lists.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Список области `scope`: из кэша или с сервера (открытый — в кэш).
    async fn listing(&self, scope: &VaultScope) -> Result<Listing, VaultError> {
        let space = scope.space();
        if let Some(l) = self.lists().get(space) {
            return Ok(l.clone());
        }
        if !segment_ok(space) {
            return Err(protocol("solution id is not a valid path segment"));
        }
        let path = format!("/api/v1/spaces/{space}/vault");
        let v = self
            .client
            .get_json(&path, &Ctx::default(), VAULT_LIMIT)
            .await
            .map_err(vault_error)?;
        let Some(raw) = v.get("items").and_then(Value::as_array) else {
            return Err(protocol("Vault list without items"));
        };
        let items = raw
            .iter()
            .filter_map(|it| item_of(it, scope))
            .collect::<Vec<_>>();
        let locked = match v.get("locked").and_then(Value::as_bool) {
            Some(true) => Some(
                v.get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ),
            _ => None,
        };
        let l = Listing { items, locked };
        if l.locked.is_none() {
            self.lists().insert(space.to_owned(), l.clone());
        }
        Ok(l)
    }

    /// Запись по имени `name` (D143): Vault решения (если `solution` — `Bound`), затем личный.
    /// Список заблокированного Vault ищется так же (метаданные открыты), блокировка мешает
    /// только раскрытию.
    pub async fn find(
        &self,
        name: &str,
        solution: &SolutionVault,
    ) -> Result<VaultItem, VaultError> {
        self.locate(name, solution).await.map(|(item, _)| item)
    }

    /// Как [`VaultClient::find`], и причина блокировки Vault записи (`locked: true` её
    /// списка): ключ заблокированного Vault ждёт без запроса раскрытия.
    pub async fn locate(
        &self,
        name: &str,
        solution: &SolutionVault,
    ) -> Result<(VaultItem, Option<String>), VaultError> {
        let mut scopes = Vec::with_capacity(2);
        if let SolutionVault::Bound(g) = solution {
            scopes.push(VaultScope::Solution(g.clone()));
        }
        scopes.push(VaultScope::Knowledge);
        for scope in scopes {
            let l = self.listing(&scope).await?;
            if let Some(item) = pick(&l.items, name)? {
                if item.is_environment() {
                    return Err(VaultError::NotAKey);
                }
                return Ok((item, l.locked));
            }
        }
        Err(VaultError::NotFound {
            solution_skipped: solution.skipped(),
        })
    }

    /// Значение записи (`?reveal=1`); `session` — `X-Zam-Session` run (`None` у `ask` и
    /// `models probe`). Запись окружения — [`VaultError::NotAKey`] без запроса; ответ без
    /// строки `secret` — тоже `NotAKey`.
    pub async fn reveal(
        &self,
        item: &VaultItem,
        session: Option<&str>,
    ) -> Result<String, VaultError> {
        if item.is_environment() {
            return Err(VaultError::NotAKey);
        }
        if !segment_ok(&item.id) {
            return Err(protocol("Vault item id is not a valid path segment"));
        }
        let path = format!("/api/v1/vault/{}?reveal=1", item.id);
        let cx = Ctx {
            session,
            ..Ctx::default()
        };
        let v = self
            .client
            .get_json(&path, &cx, VAULT_LIMIT)
            .await
            .map_err(vault_error)?;
        match v.get("secret") {
            Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
            _ => Err(VaultError::NotAKey),
        }
    }

    /// Записи окружения (`host`, `environment`) Vault решения (если `Bound`) и личного — для
    /// `secret list` (D144); без раскрытия.
    pub async fn environment(
        &self,
        solution: &SolutionVault,
    ) -> Result<Vec<VaultItem>, VaultError> {
        let mut out = Vec::new();
        if let SolutionVault::Bound(g) = solution {
            let l = self.listing(&VaultScope::Solution(g.clone())).await?;
            out.extend(l.items.into_iter().filter(VaultItem::is_environment));
        }
        let l = self.listing(&VaultScope::Knowledge).await?;
        out.extend(l.items.into_iter().filter(VaultItem::is_environment));
        Ok(out)
    }
}

/// Запись списка: `id`, `title`, `data.type`, `rev`; без `id` или `title` — пропуск.
fn item_of(v: &Value, scope: &VaultScope) -> Option<VaultItem> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    Some(VaultItem {
        id: s("id")?,
        title: s("title")?,
        scope: scope.clone(),
        item_type: v
            .get("data")
            .and_then(|d| d.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        rev: v.get("rev").and_then(Value::as_i64).unwrap_or(0),
    })
}

/// Совпадение имени в списке: точное (с учётом регистра), затем единственное без учёта
/// регистра; несколько — `Ambiguous` с именами.
fn pick(items: &[VaultItem], name: &str) -> Result<Option<VaultItem>, VaultError> {
    let exact: Vec<&VaultItem> = items.iter().filter(|i| i.title == name).collect();
    match exact.as_slice() {
        [one] => return Ok(Some((*one).clone())),
        [] => {}
        many => {
            return Err(VaultError::Ambiguous(
                many.iter().map(|i| i.title.clone()).collect(),
            ));
        }
    }
    let lower = name.to_lowercase();
    let folded: Vec<&VaultItem> = items
        .iter()
        .filter(|i| i.title.to_lowercase() == lower)
        .collect();
    match folded.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some((*one).clone())),
        many => Err(VaultError::Ambiguous(
            many.iter().map(|i| i.title.clone()).collect(),
        )),
    }
}
