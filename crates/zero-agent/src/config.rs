//! Конфигурация агента: `ZA_HOME/config.toml`, разделы `[models]` и `[resources]` (D23).
//!
//! Текст ошибки разбора наружу — только [`ConfigParseError`] (белый список: место,
//! категория, имя известного поля). `Display` ошибки `toml` печатает строку исходника,
//! где может стоять ключ (`api_key = "sk-…"`), поэтому он не используется нигде (SEC-044).
//!
//! Файла нет — пустая конфигурация. Раздела `[models]` нет, а `LLAMA_URL` задан — один
//! эндпоинт `local` (`llama_cpp`) и `default = "local"`: совместимость с `doctor` R0.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use za_llm::config::{ConfigError, ConfigParseError, EndpointConfig, ModelsConfig};
use za_llm::{BackendKind, EndpointId};
use za_res::{ResourcesConfig, ResourcesConfigError};

use crate::doctor::LLAMA_URL_ENV;

/// Имя файла конфигурации в `ZA_HOME`.
pub const CONFIG_FILE: &str = "config.toml";
/// id эндпоинта, который создаётся из `LLAMA_URL` без раздела `[models]`.
pub const LLAMA_URL_ENDPOINT: &str = "local";

/// Конфигурация агента R1-A.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub models: Option<ModelsConfig>,
    #[serde(default)]
    pub resources: ResourcesConfig,
    /// `models` — запасной эндпоинт `local` из `LLAMA_URL`, а не раздел файла.
    #[serde(skip)]
    pub llama_url_fallback: bool,
}

/// Ошибка чтения конфигурации. Текст не содержит фрагментов исходника.
#[derive(Debug, thiserror::Error)]
pub enum AppConfigError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{file}: {0}", file = CONFIG_FILE)]
    Parse(ConfigParseError),
    #[error("{file}: {0}", file = CONFIG_FILE)]
    Models(ConfigError),
    #[error("{file}: {0}", file = CONFIG_FILE)]
    Resources(ResourcesConfigError),
}

/// Читает `home/config.toml`; `LLAMA_URL` — из окружения.
pub fn load_config(home: &Path) -> Result<AgentConfig, AppConfigError> {
    let url = std::env::var(LLAMA_URL_ENV).ok();
    load_config_with(home, url.as_deref())
}

/// [`load_config`] с явным `LLAMA_URL`.
pub fn load_config_with(
    home: &Path,
    llama_url: Option<&str>,
) -> Result<AgentConfig, AppConfigError> {
    let path = home.join(CONFIG_FILE);
    let mut cfg = match std::fs::read_to_string(&path) {
        Ok(src) => parse_config(&src)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => AgentConfig::default(),
        Err(source) => return Err(AppConfigError::Io { path, source }),
    };
    if cfg.models.is_none()
        && let Some(url) = llama_url.map(str::trim).filter(|u| !u.is_empty())
    {
        cfg.models = Some(llama_url_models(url));
        cfg.llama_url_fallback = true;
    }
    validate(&cfg)?;
    Ok(cfg)
}

/// Разбор текста `config.toml` без проверки значений.
pub fn parse_config(src: &str) -> Result<AgentConfig, AppConfigError> {
    toml::from_str::<AgentConfig>(src).map_err(|e| {
        AppConfigError::Parse(ConfigParseError::from_message(
            e.message(),
            e.span().map(|s| s.start),
            src,
        ))
    })
}

/// Проверка значений: `[models]` (ссылки, D24, D25) и `[resources]`.
pub fn validate(cfg: &AgentConfig) -> Result<(), AppConfigError> {
    cfg.resources
        .validate()
        .map_err(AppConfigError::Resources)?;
    if let Some(m) = &cfg.models {
        // Предупреждения (D34) печатает `Gateway::start` и показывают `models`, `doctor`.
        m.validate().map_err(AppConfigError::Models)?;
    }
    Ok(())
}

/// Один локальный эндпоинт llama.cpp по `LLAMA_URL`.
fn llama_url_models(url: &str) -> ModelsConfig {
    let id = EndpointId(LLAMA_URL_ENDPOINT.to_owned());
    ModelsConfig {
        default: Some(id.clone()),
        endpoints: vec![EndpointConfig {
            id,
            backend: BackendKind::LlamaCpp,
            base_url: Some(url.to_owned()),
            model: None,
            key_ref: None,
            pool: None,
            max_parallel: None,
            context_tokens: None,
            daily_budget_usd: None,
            price: None,
            reasoning: None,
        }],
        roles: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Чтение файла и `LLAMA_URL` — в `tests/config.rs` (нужен `CARGO_TARGET_TMPDIR`).

    #[test]
    fn llama_url_models_are_local_default() {
        let m = llama_url_models("http://127.0.0.1:8080");
        assert_eq!(m.default, Some(EndpointId("local".into())));
        assert_eq!(m.endpoints.len(), 1);
        assert_eq!(m.endpoints[0].backend, BackendKind::LlamaCpp);
        assert!(m.validate().is_ok());
    }

    #[test]
    fn resources_are_validated() {
        let e = parse_config("[resources]\nhysteresis = 2.0\n")
            .and_then(|c| validate(&c).map(|_| c))
            .unwrap_err();
        assert!(matches!(e, AppConfigError::Resources(_)), "{e}");
        assert!(e.to_string().contains("resources.hysteresis"), "{e}");
    }

    #[test]
    fn parse_errors_never_show_source() {
        const KEY: &str = "sk-unit-secret-1";
        for src in [
            format!("[models]\n[[models.endpoints]]\nid = \"c\"\napi_key = \"{KEY}\"\n"),
            format!("[models]\n[[models.endpoints]]\nid = \"c\"\napi_key = \"{KEY}\n"),
            format!("[resources]\nnvidia_smi = \"{KEY}\" ]]\n"),
            format!("{KEY} = 1\n"),
        ] {
            let e = parse_config(&src).unwrap_err();
            let shown = format!("{e} {e:?}");
            assert!(!shown.contains(KEY), "{shown}");
            assert!(shown.contains("строка"), "{shown}");
        }
    }

    #[test]
    fn unknown_top_level_section_is_error() {
        assert!(matches!(
            parse_config("[budgets]\nx = 1\n"),
            Err(AppConfigError::Parse(_))
        ));
    }
}
