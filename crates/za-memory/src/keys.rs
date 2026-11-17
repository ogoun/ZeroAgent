//! Ключи моделей и поиска из Vault памяти (R1-M D143, D160; решение владельца 1).
//!
//! - **Только нужные команде.** [`VaultKeys::reveal`] раскрывает имена, которые назвал
//!   вызывающий (`key_ref` выбранного эндпоинта и поставщика поиска), — не весь Vault: аудит
//!   Vault не забивается, лишнее не раскрывается.
//! - **Кэш на процесс.** Раскрытое значение живёт в процессе: второй запрос того же имени —
//!   из кэша (`cached: true`), без запроса к серверу. Внутри идущего run ключ уже в кэше,
//!   блокировка Vault на него не влияет.
//! - **Маска до возврата** (D160). Значение попадает в маску процесса ([`MaskCell`]) раньше,
//!   чем его увидит вызывающий: ключ не используется раньше маски.
//! - **Состояние имени** ([`KeyState`]): `Ready`, `Waiting` (Vault заблокирован, сервер
//!   недоступен, временная ошибка — ждать имеет смысл) или `Missing` (не найден, несколько
//!   записей, запись окружения, нет доступа, токен не принят — ошибка без ожидания). Ключ
//!   заблокированного Vault (`locked: true` списка) ждёт без запроса раскрытия.
//! - Значений нет ни в `Debug`, ни в логах, ни в причинах состояний.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use za_secrets::{MaskCell, SecretStore};

use crate::config::Token;
use crate::vault::{SolutionVault, VaultClient, VaultError};

/// Метка значения токена памяти в маске — как у шаблона `zam_` (`[secret masked: zam_token]`).
pub const TOKEN_MASK_LABEL: &str = "zam_token";

/// Маска процесса на старте команды (D134, D160): значение токена памяти — известное
/// значение маски. Ключи Vault добавит [`VaultKeys::reveal`].
pub fn process_mask(token: Option<&Token>) -> MaskCell {
    MaskCell::new(
        token
            .map(|t| vec![(TOKEN_MASK_LABEL.to_owned(), t.expose().to_owned())])
            .unwrap_or_default(),
    )
}

/// Состояние ключа после попытки раскрытия.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    /// Значение в кэше процесса.
    Ready,
    /// Ждать имеет смысл: причина (Vault заблокирован, сервер недоступен).
    Waiting(String),
    /// Ошибка без ожидания: причина.
    Missing(String),
}

/// Раскрытие ключа для журнала и аудита (D143): без значения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revealed {
    /// Имя — `key_ref`.
    pub name: String,
    /// Область записи: `solution` или `knowledge`.
    pub scope: String,
    /// Id записи Vault.
    pub item: String,
    /// Значение взято из кэша процесса, запроса к серверу не было.
    pub cached: bool,
}

/// Раскрытое значение и откуда оно.
struct Entry {
    value: String,
    scope: &'static str,
    item: String,
}

/// Ключи из Vault: раскрытие по имени, кэш на процесс, маска процесса (D143, D160).
pub struct VaultKeys {
    vault: VaultClient,
    solution: SolutionVault,
    cache: RwLock<BTreeMap<String, Entry>>,
    states: Mutex<BTreeMap<String, KeyState>>,
    mask: Arc<MaskCell>,
}

impl fmt::Debug for VaultKeys {
    /// Имена, состояния и Vault решения; значений нет.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VaultKeys")
            .field("solution", &self.solution)
            .field("revealed", &self.names())
            .field("states", &*self.states())
            .finish()
    }
}

impl VaultKeys {
    /// Ключи по Vault `vault`; `solution` — Vault решения проекта (D135); `mask` — маска
    /// процесса, которую пополняет каждое раскрытие.
    pub fn new(vault: VaultClient, solution: SolutionVault, mask: Arc<MaskCell>) -> Self {
        Self {
            vault,
            solution,
            cache: RwLock::new(BTreeMap::new()),
            states: Mutex::new(BTreeMap::new()),
            mask,
        }
    }

    /// Клиент Vault (поиск и окружение без раскрытия — `secret list`, `doctor`).
    pub fn vault(&self) -> &VaultClient {
        &self.vault
    }

    /// Vault решения проекта.
    pub fn solution(&self) -> &SolutionVault {
        &self.solution
    }

    /// Маска процесса.
    pub fn mask(&self) -> &Arc<MaskCell> {
        &self.mask
    }

    fn states(&self) -> MutexGuard<'_, BTreeMap<String, KeyState>> {
        self.states.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Раскрывает ключи `names` (повторы — один раз): из кэша или с сервера, `session` —
    /// `X-Zam-Session` run (`None` у `ask` и `models probe`). Итог — раскрытия по порядку и
    /// состояние каждого имени. Каждое новое значение — в маске процесса до возврата.
    pub async fn reveal(
        &self,
        names: &[String],
        session: Option<&str>,
    ) -> (Vec<Revealed>, BTreeMap<String, KeyState>) {
        let mut revealed = Vec::new();
        let mut states = BTreeMap::new();
        for name in names {
            if states.contains_key(name) {
                continue;
            }
            let state = match self.one(name, session).await {
                Ok(r) => {
                    revealed.push(r);
                    KeyState::Ready
                }
                Err(e) if e.is_waitable() => KeyState::Waiting(e.to_string()),
                Err(e) => KeyState::Missing(e.to_string()),
            };
            self.states().insert(name.clone(), state.clone());
            states.insert(name.clone(), state);
        }
        (revealed, states)
    }

    /// Одно имя: кэш, затем поиск и раскрытие.
    async fn one(&self, name: &str, session: Option<&str>) -> Result<Revealed, VaultError> {
        if let Some(e) = self.cached(name) {
            return Ok(Revealed {
                name: name.to_owned(),
                scope: e.0.to_owned(),
                item: e.1,
                cached: true,
            });
        }
        let (item, locked) = self.vault.locate(name, &self.solution).await?;
        if let Some(reason) = locked {
            return Err(VaultError::Locked(reason));
        }
        let value = self.vault.reveal(&item, session).await?;
        // D160: маска знает значение раньше вызывающего.
        self.mask.extend(vec![(name.to_owned(), value.clone())]);
        let scope = item.scope.as_str();
        self.cache
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                name.to_owned(),
                Entry {
                    value,
                    scope,
                    item: item.id.clone(),
                },
            );
        tracing::info!(key = name, scope, "ключ раскрыт из Vault памяти");
        Ok(Revealed {
            name: name.to_owned(),
            scope: scope.to_owned(),
            item: item.id,
            cached: false,
        })
    }

    /// Область и id записи раскрытого `name`.
    fn cached(&self, name: &str) -> Option<(&'static str, String)> {
        self.cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .map(|e| (e.scope, e.item.clone()))
    }
}

impl SecretStore for VaultKeys {
    /// Значение из кэша раскрытых: сервер здесь не спрашивается.
    fn get(&self, name: &str) -> Option<String> {
        self.cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .map(|e| e.value.clone())
    }

    /// Раскрытые имена по возрастанию.
    fn names(&self) -> Vec<String> {
        self.cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    fn kind(&self) -> &'static str {
        "vault"
    }
}
