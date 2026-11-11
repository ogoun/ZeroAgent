//! Хранилище секретов (D90): трейт [`SecretStore`] и [`EnvStore`] поверх `ZA_SECRET_*`.
//!
//! Целевая реализация — `VaultStore` поверх раздела Vault ZeroAIMemory (CR-21
//! `docs/memory_upgrade.md`); она появится с клиентом памяти. До него работает только
//! [`EnvStore`]: значения переменных `ZA_SECRET_<KEY_REF>` по правилу `secret_env_name`
//! R1-A D4. Запись секретов агентом не предусмотрена: хранилища только читают.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;

/// Префикс переменных окружения с секретами (R1-A D4).
pub const ENV_PREFIX: &str = "ZA_SECRET_";
/// Значения короче не маскируются: короткая строка совпадает с обычным текстом (D90).
pub const MIN_SECRET_LEN: usize = 8;

/// Источник секретов по имени `key_ref`.
///
/// Реализация не печатает значения ни в `Debug`, ни в логах, ни в ошибках.
pub trait SecretStore: Send + Sync {
    /// Значение секрета; `None` — не задан.
    fn get(&self, name: &str) -> Option<String>;
    /// Имена заданных секретов по возрастанию.
    fn names(&self) -> Vec<String>;
    /// Вид хранилища: `"env"`, позже `"vault"`.
    fn kind(&self) -> &'static str;
}

/// Имя переменной окружения секрета `name`: [`ENV_PREFIX`] и `name` в верхнем регистре,
/// любой символ вне `[A-Za-z0-9_]` заменён на `_` (то же правило, что у
/// `za_llm::secret_env_name`; крейт от `za-llm` не зависит, D73).
/// `"open-router"` → `"ZA_SECRET_OPEN_ROUTER"`.
pub fn env_var_name(name: &str) -> String {
    let tail: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{ENV_PREFIX}{tail}")
}

/// Секреты из переменных окружения `ZA_SECRET_<KEY_REF>` (D90). Только чтение.
///
/// Имя секрета — хвост имени переменной в нижнем регистре, `_` → `-`
/// (`ZA_SECRET_OPEN_ROUTER` → `open-router`). [`SecretStore::get`] принимает любое
/// написание, дающее ту же переменную (`open_router`, `OPEN.ROUTER`).
#[derive(Clone, Default)]
pub struct EnvStore {
    /// Имя переменной (хвост в верхнем регистре) → значение.
    vars: BTreeMap<String, String>,
}

impl EnvStore {
    /// Секреты из `vars` (продукт передаёт `std::env::vars_os()`, тесты — свой список).
    /// Имена переменных без учёта регистра — только на Windows, как у `std::env::var`.
    pub fn from_vars(vars: impl Iterator<Item = (OsString, OsString)>) -> Self {
        Self::from_vars_with(vars, cfg!(windows))
    }

    /// [`EnvStore::from_vars`] с явной платформой: `windows` — имена переменных без учёта
    /// регистра ASCII.
    ///
    /// Берутся только переменные, которые `secret_env_name` может дать: [`ENV_PREFIX`] и
    /// непустой хвост из `[A-Z0-9_]` (на Windows — в любом регистре). Пустые значения
    /// и имена или значения не в UTF-8 пропускаются: `std::env::var` их тоже не отдавал.
    pub fn from_vars_with(vars: impl Iterator<Item = (OsString, OsString)>, windows: bool) -> Self {
        let vars = vars
            .filter_map(|(k, v)| {
                let k = k.into_string().ok()?;
                let tail = env_tail(&k, windows)?;
                let v = v.into_string().ok().filter(|v| !v.is_empty())?;
                Some((tail, v))
            })
            .collect();
        Self { vars }
    }
}

/// Хвост имени переменной секрета в верхнем регистре; `None` — не переменная секрета.
fn env_tail(name: &str, windows: bool) -> Option<String> {
    let head = name.get(..ENV_PREFIX.len())?;
    let prefix_ok = if windows {
        head.eq_ignore_ascii_case(ENV_PREFIX)
    } else {
        head == ENV_PREFIX
    };
    let tail = &name[ENV_PREFIX.len()..];
    let valid = |c: char| {
        c.is_ascii_digit()
            || c == '_'
            || c.is_ascii_uppercase()
            || (windows && c.is_ascii_lowercase())
    };
    (prefix_ok && !tail.is_empty() && tail.chars().all(valid)).then(|| tail.to_ascii_uppercase())
}

/// Имя секрета по хвосту переменной: нижний регистр, `_` → `-`.
fn name_of(tail: &str) -> String {
    tail.chars()
        .map(|c| {
            if c == '_' {
                '-'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}

impl SecretStore for EnvStore {
    fn get(&self, name: &str) -> Option<String> {
        let var = env_var_name(name);
        self.vars.get(&var[ENV_PREFIX.len()..]).cloned()
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.vars.keys().map(|t| name_of(t)).collect();
        names.sort();
        names
    }

    fn kind(&self) -> &'static str {
        "env"
    }
}

impl fmt::Debug for EnvStore {
    /// Только имена: значения не печатаются.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvStore")
            .field("names", &self.names())
            .finish()
    }
}

/// Значения для маскировки (D90): пары (имя, значение) с значением от
/// [`MIN_SECRET_LEN`] символов, в порядке [`SecretStore::names`].
pub fn known_values(s: &dyn SecretStore) -> Vec<(String, String)> {
    s.names()
        .into_iter()
        .filter_map(|n| {
            let v = s.get(&n)?;
            (v.chars().count() >= MIN_SECRET_LEN).then_some((n, v))
        })
        .collect()
}
