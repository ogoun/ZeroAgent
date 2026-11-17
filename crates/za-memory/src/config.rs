//! Подключение к серверу памяти (R1-M D134): `[memory]` глобальной конфигурации и токен.
//!
//! - **Адрес** — только `[memory] url` из `ZA_HOME/config.toml`: `http` или `https` с хостом,
//!   без учётных данных, строки запроса и фрагмента; путь допустим (`server.base_path` ZAM).
//!   Проектная конфигурация адрес не задаёт (её разбирает бинарник).
//! - **Машина** — `[memory] machine` или имя хоста (`COMPUTERNAME`, `HOSTNAME`, на Unix ещё
//!   `/etc/hostname`): до 128 печатных символов UTF-8, как принимает сервер
//!   (`crates/zam-server/src/attrib.rs` e391440).
//! - **Токен** — переменная окружения [`TOKEN_ENV`]; бинарник читает её один раз и передаёт
//!   [`Token`] параметром. Других источников нет: ни файла, ни `~/.zeroaimemory/config.toml`.
//!   `Debug` токена — `zam_…` без хвоста.

use std::fmt;

use serde::Deserialize;
use url::Url;

/// Переменная окружения с токеном памяти агента (D134).
pub const TOKEN_ENV: &str = "ZA_MEMORY_TOKEN";

/// Предел длины `X-Zam-Machine` и `X-Zam-Area`, символов (`ATTRIB_MAX_CHARS` ZAM).
pub const ATTRIB_MAX_CHARS: usize = 128;

/// Раздел `[memory]` глобальной конфигурации (D134, D152).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    /// Базовый адрес сервера (`http://host:7700`).
    pub url: String,
    /// Имя машины для `X-Zam-Machine`; `None` — имя хоста.
    pub machine: Option<String>,
    /// `false` — память выключена: L0 без запросов.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

impl MemoryConfig {
    /// Включённая память по адресу `url`, машина — имя хоста.
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_owned(),
            machine: None,
            enabled: true,
        }
    }
}

/// Токен памяти агента. Значение не печатается: `Debug` — `Token(zam_…)`.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Значение переменной [`TOKEN_ENV`]: пробелы по краям снимаются, пусто — `None`.
    pub fn from_env_value(value: Option<String>) -> Option<Self> {
        let v = value?;
        let v = v.trim();
        (!v.is_empty()).then(|| Self(v.to_owned()))
    }

    /// Значение — только транспорту (заголовок `Authorization`) и маске.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.starts_with("zam_") {
            f.write_str("Token(zam_…)")
        } else {
            f.write_str("Token(…)")
        }
    }
}

/// Ошибка подключения: неверный адрес, машина, токен или сборка клиента. Значения адреса,
/// машины и токена в текст не попадают.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("memory.url: {0}")]
    Url(&'static str),
    #[error("memory.machine: {0}")]
    Machine(&'static str),
    #[error("{env}: недопустимые символы в токене", env = TOKEN_ENV)]
    Token,
    #[error("HTTP-клиент памяти не собран: {0}")]
    Client(String),
}

/// Проверенный базовый адрес `[memory] url` (D134): `http` или `https`, хост, без учётных
/// данных, строки запроса и фрагмента; последний `/` пути снимается.
pub fn base_url(raw: &str) -> Result<Url, ConfigError> {
    let u = Url::parse(raw.trim())
        .map_err(|_| ConfigError::Url("адрес не разбирается; пример: http://127.0.0.1:7700"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(ConfigError::Url("только http или https"));
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err(ConfigError::Url("в адресе нет хоста"));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(ConfigError::Url(
            "учётные данные в адресе не допускаются: токен — переменная ZA_MEMORY_TOKEN",
        ));
    }
    if u.query().is_some() || u.fragment().is_some() {
        return Err(ConfigError::Url(
            "строка запроса и фрагмент в адресе не допускаются",
        ));
    }
    Ok(u)
}

/// Проверенное имя машины: 1–128 символов UTF-8 без управляющих, пробелы по краям сняты.
pub fn check_machine(raw: &str) -> Result<String, ConfigError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(ConfigError::Machine("пустое имя машины"));
    }
    if s.chars().count() > ATTRIB_MAX_CHARS {
        return Err(ConfigError::Machine("длиннее 128 символов"));
    }
    if s.chars().any(char::is_control) {
        return Err(ConfigError::Machine("управляющие символы недопустимы"));
    }
    Ok(s.to_owned())
}

/// Имя хоста для `X-Zam-Machine` (D134): `COMPUTERNAME`, затем `HOSTNAME`, на Unix ещё
/// `/etc/hostname`; негодное имя ([`check_machine`]) пропускается.
pub fn host_machine() -> Option<String> {
    let from_env = ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find_map(|v| check_machine(&v).ok());
    #[cfg(unix)]
    let from_env = from_env.or_else(|| {
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .and_then(|v| check_machine(&v).ok())
    });
    from_env
}
