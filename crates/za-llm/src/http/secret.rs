//! Секреты: ключи провайдеров (SEC-040, SEC-044, D3, D4).
//!
//! `Secret` не реализует `Display` и `Serialize`, а `Debug` печатает `***`: ключ не
//! попадает в логи, журнал и сообщения ошибок случайно. Текст, который мог вернуть
//! ключ эхом (тело ошибки провайдера), проходит через [`redact`].

use reqwest::header::HeaderValue;

use crate::error::{ErrorClass, ModelError};

/// Предел текста после [`redact`]: тело ошибки обрезается до 2 КиБ.
const REDACT_LIMIT: usize = 2048;

/// Ключ или токен. Значение доступно только через [`Secret::expose`].
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: String) -> Self {
        Self(s)
    }

    /// Значение ключа. Вызывать только там, где ключ уходит в запрос.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Значение заголовка `prefix + ключ` (например, `"Bearer "`), помеченное
    /// `sensitive`: hyper и reqwest не печатают его в `Debug`.
    pub fn header_value(&self, prefix: &str) -> Result<HeaderValue, ModelError> {
        let mut v = HeaderValue::from_str(&format!("{prefix}{}", self.0)).map_err(|_| {
            ModelError::new(
                ErrorClass::Auth,
                "ключ содержит символы, недопустимые в заголовке HTTP",
            )
        })?;
        v.set_sensitive(true);
        Ok(v)
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// Источник ключей по ссылке `key_ref` из конфигурации (D3, D4).
pub trait SecretSource: Send + Sync {
    /// Ключ по ссылке; `None` — ключ не настроен.
    fn get(&self, key_ref: &str) -> Option<Secret>;
}

/// Ключи из переменных окружения `ZA_SECRET_<KEY_REF>` (D4).
/// Хранилище ОС и шифрованный файл (SEC-040) — R1-C, без смены конфигурации.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvSecrets;

impl SecretSource for EnvSecrets {
    fn get(&self, key_ref: &str) -> Option<Secret> {
        std::env::var(secret_env_name(key_ref))
            .ok()
            .filter(|v| !v.is_empty())
            .map(Secret::new)
    }
}

/// Имя переменной окружения ключа: `ZA_SECRET_` и `key_ref` в верхнем регистре,
/// где `-` и любой другой символ вне `[A-Za-z0-9_]` заменён на `_`.
/// `"open-router"` → `"ZA_SECRET_OPEN_ROUTER"`.
pub fn secret_env_name(key_ref: &str) -> String {
    let tail: String = key_ref
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("ZA_SECRET_{tail}")
}

/// Заменяет вхождения ключей на `***` и обрезает текст до 2 КиБ (по границе
/// символа, с `…` в конце). Сначала замена, потом обрезка: обрезка не может
/// оставить видимым начало ключа.
pub fn redact(text: &str, secrets: &[&Secret]) -> String {
    let mut out = text.to_owned();
    for s in secrets {
        let k = s.expose();
        if !k.is_empty() && out.contains(k) {
            out = out.replace(k, "***");
        }
    }
    truncate(out, REDACT_LIMIT)
}

/// Обрезает строку до `limit` байт по границе символа, добавляя `…`.
pub(crate) fn truncate(mut s: String, limit: usize) -> String {
    if s.len() <= limit {
        return s;
    }
    let mut cut = limit;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_name_maps_non_alnum() {
        assert_eq!(secret_env_name("anthropic"), "ZA_SECRET_ANTHROPIC");
        assert_eq!(secret_env_name("my.key-2"), "ZA_SECRET_MY_KEY_2");
    }

    #[test]
    fn header_value_is_sensitive() {
        let v = Secret::new("k".into()).header_value("Bearer ").unwrap();
        assert!(v.is_sensitive());
        assert_eq!(v.to_str().unwrap(), "Bearer k");
        assert!(!format!("{v:?}").contains('k'));
    }

    #[test]
    fn redact_handles_several_and_empty_secrets() {
        let a = Secret::new("aaa".into());
        let e = Secret::new(String::new());
        assert_eq!(redact("aaa-bbb-aaa", &[&a, &e]), "***-bbb-***");
    }
}
