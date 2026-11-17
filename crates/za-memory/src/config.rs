//! Подключение к серверу памяти (R1-M D134): `[memory]` глобальной конфигурации и токен.
//!
//! - **Адрес** — только `[memory] url` из `ZA_HOME/config.toml`: `http` или `https` с хостом,
//!   без учётных данных, строки запроса и фрагмента; путь допустим (`server.base_path` ZAM).
//!   Проектная конфигурация адрес не задаёт (её разбирает бинарник).
//! - **Машина** — `[memory] machine` или имя хоста (`COMPUTERNAME`, `HOSTNAME`, на Unix ещё
//!   `/etc/hostname`): до 128 печатных символов UTF-8, как принимает сервер
//!   (`crates/zam-server/src/attrib.rs` e391440) — та же проверка [`check_attrib`], что у
//!   всех заголовков атрибуции.
//! - **Токен** — переменная окружения [`TOKEN_ENV`]; бинарник читает её один раз и передаёт
//!   [`Token`] параметром. Других источников нет: ни файла, ни `~/.zeroaimemory/config.toml`.
//!   `Debug` токена — `zam_…` без хвоста.

use std::fmt;

use serde::Deserialize;
use url::Url;

/// Переменная окружения с токеном памяти агента (D134).
pub const TOKEN_ENV: &str = "ZA_MEMORY_TOKEN";

/// Предел длины значений заголовков атрибуции `X-Zam-*`, символов (`ATTRIB_MAX_CHARS` ZAM).
pub const ATTRIB_MAX_CHARS: usize = 128;

/// Путь поля адреса в конфигурации (текст [`ConfigError::Url`], [`ConfigError::field`]).
pub const URL_FIELD: &str = "memory.url";
/// Путь поля имени машины в конфигурации.
pub const MACHINE_FIELD: &str = "memory.machine";

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
/// машины и токена в текст не попадают. Текст ошибки поля — `<путь>: <причина>`; путь и
/// причину отдельно дают [`ConfigError::field`] и [`ConfigError::reason`] (M7 ревью задачи 2:
/// бинарник не разбирает `Display`).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{at}: {0}", at = URL_FIELD)]
    Url(&'static str),
    #[error("{at}: {0}", at = MACHINE_FIELD)]
    Machine(&'static str),
    #[error("{env}: {TOKEN_REASON}", env = TOKEN_ENV)]
    Token,
    #[error("{CLIENT_REASON}: {0}")]
    Client(String),
}

/// Причина [`ConfigError::Token`].
const TOKEN_REASON: &str = "недопустимые символы в токене";
/// Начало причины [`ConfigError::Client`].
const CLIENT_REASON: &str = "HTTP-клиент памяти не собран";

impl ConfigError {
    /// Путь поля `[memory]` (`memory.url`, `memory.machine`); у токена (переменная окружения,
    /// не поле) и у сборки клиента — `None`.
    pub fn field(&self) -> Option<&'static str> {
        match self {
            Self::Url(_) => Some(URL_FIELD),
            Self::Machine(_) => Some(MACHINE_FIELD),
            Self::Token | Self::Client(_) => None,
        }
    }

    /// Причина без пути поля и без значений.
    pub fn reason(&self) -> String {
        match self {
            Self::Url(r) | Self::Machine(r) => (*r).to_owned(),
            Self::Token => TOKEN_REASON.to_owned(),
            Self::Client(m) => format!("{CLIENT_REASON}: {m}"),
        }
    }
}

/// Проверенный базовый адрес `[memory] url` (D134): `http` или `https`, хост, без учётных
/// данных, строки запроса и фрагмента; путь — как есть (у адреса без пути `Url` даёт `/`).
/// Последний `/` снимает не эта функция, а [`crate::ZamClient::new`].
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

/// Проверенное значение заголовка атрибуции — `X-Zam-Machine`, `X-Zam-Session`,
/// `X-Zam-Solution`, `X-Zam-Area` (как принимает сервер, `attrib.rs` e391440): 1–128 символов
/// UTF-8 без управляющих, пробелы по краям сняты. Ошибка — причина без значения.
pub fn check_attrib(raw: &str) -> Result<String, &'static str> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("пустое значение");
    }
    if s.chars().count() > ATTRIB_MAX_CHARS {
        return Err("длиннее 128 символов");
    }
    if s.chars().any(char::is_control) {
        return Err("управляющие символы недопустимы");
    }
    Ok(s.to_owned())
}

/// Имя хоста для `X-Zam-Machine` (D134): `COMPUTERNAME`, затем `HOSTNAME`, на Unix ещё
/// `/etc/hostname`; негодное имя ([`check_attrib`]) пропускается.
pub fn host_machine() -> Option<String> {
    let from_env = ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find_map(|v| check_attrib(&v).ok());
    #[cfg(unix)]
    let from_env = from_env.or_else(|| {
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .and_then(|v| check_attrib(&v).ok())
    });
    from_env
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M7 ревью задачи 2: путь поля и причина — отдельно; текст ошибки — `<путь>: <причина>`.
    #[test]
    fn config_error_field_and_reason() {
        for (e, field) in [
            (base_url("ftp://h:1").unwrap_err(), URL_FIELD),
            (base_url("http://u:p@h:7700").unwrap_err(), URL_FIELD),
            (ConfigError::Machine("пустое значение"), MACHINE_FIELD),
        ] {
            assert_eq!(e.field(), Some(field));
            assert_eq!(e.to_string(), format!("{field}: {}", e.reason()));
            assert!(!e.reason().contains(field), "{e}");
            assert!(!e.reason().contains("u:p"), "{e}");
        }
        assert_eq!(ConfigError::Token.field(), None);
        assert_eq!(
            ConfigError::Token.to_string(),
            format!("{TOKEN_ENV}: {}", ConfigError::Token.reason())
        );
        let c = ConfigError::Client("tls".into());
        assert_eq!(c.field(), None);
        assert_eq!(c.to_string(), c.reason());
    }

    /// Значения атрибуции (M12 ревью задачи 2: одна проверка для всех `X-Zam-*`).
    #[test]
    fn attrib_values_are_checked() {
        assert_eq!(check_attrib("  ПК-1 ").as_deref(), Ok("ПК-1"));
        assert_eq!(
            check_attrib(&"я".repeat(128)).map(|s| s.chars().count()),
            Ok(128)
        );
        for bad in [
            String::new(),
            "   ".to_owned(),
            "a\nb".to_owned(),
            "a\u{7f}b".to_owned(),
            "x".repeat(129),
        ] {
            assert!(check_attrib(&bad).is_err(), "{bad:?}");
        }
    }
}
