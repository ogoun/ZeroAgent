//! Конфигурация моделей: раздел `[models]` (D23, D24, D25, D34, D37).
//!
//! Ключей в конфигурации нет: только ссылка `key_ref`, по которой ключ даёт
//! [`crate::SecretSource`] (D4). Поле `api_key` и любое другое неизвестное поле
//! отвергаются разбором (`deny_unknown_fields`), текст ошибки serde значения поля не
//! содержит.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use serde::{Deserialize, Serialize};
use za_protocol::RunRole;

use crate::caps::{BackendKind, PoolKind};
use crate::types::{EndpointId, ReasoningMode, Usage};

/// Эндпоинт модели.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    pub id: EndpointId,
    pub backend: BackendKind,
    /// Корень сервера (llama.cpp) или корень версии API (прочие), D37.
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// Имя ключа в [`crate::SecretSource`]; литерал ключа не принимается (D4).
    pub key_ref: Option<String>,
    /// Переопределение пула (D25).
    pub pool: Option<PoolKind>,
    pub max_parallel: Option<u32>,
    /// Размер контекста; первым в порядке MDL-004.
    pub context_tokens: Option<u32>,
    /// Дневной бюджет облачной группы (D14, D34).
    pub daily_budget_usd: Option<f64>,
    /// Цены для расчёта стоимости (D16); иначе — цены из профиля (OpenRouter).
    pub price: Option<Price>,
    /// Режим рассуждения по умолчанию для запросов к эндпоинту.
    pub reasoning: Option<ReasoningMode>,
}

/// Цены модели, USD за миллион токенов.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub input_per_mtok: f64,
    /// Чтение из кэша; `None` — по цене обычного входа.
    pub cached_per_mtok: Option<f64>,
    pub output_per_mtok: f64,
}

/// Раздел `[models]`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    /// Эндпоинт по умолчанию; только локальный (D24).
    pub default: Option<EndpointId>,
    #[serde(default)]
    pub endpoints: Vec<EndpointConfig>,
    /// Роль run → эндпоинт. Ключ — serde-имя [`RunRole`]: `top`, `orchestrator`, `worker`.
    #[serde(default)]
    pub roles: BTreeMap<String, EndpointId>,
}

/// Ошибка конфигурации. Текст называет поле; значения `key_ref` не печатаются.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ConfigError {
    #[error("models.endpoints: эндпоинт {0} описан дважды")]
    DuplicateId(EndpointId),
    #[error("models.default или models.roles: эндпоинт {0} не описан в models.endpoints")]
    UnknownRef(EndpointId),
    #[error(
        "models.default: эндпоинт {0} облачный, моделью по умолчанию может быть только локальная (D24)"
    )]
    CloudDefault(EndpointId),
    #[error("models.endpoints[{0}].model: для этого бэкенда модель обязательна")]
    MissingModel(EndpointId),
    #[error("models.endpoints[{0}].base_url: для vllm и openai_compatible адрес обязателен")]
    MissingBaseUrl(EndpointId),
    #[error("models.endpoints[{0}].base_url: не адрес http(s) с хостом")]
    BadBaseUrl(EndpointId),
    #[error("models.roles.{0}: неизвестная роль (допустимы top, orchestrator, worker)")]
    UnknownRole(String),
    /// Аргумент — id эндпоинта: значение `key_ref` могло быть самим ключом.
    #[error(
        "models.endpoints[{0}].key_ref: ожидается имя ключа (латиница, цифры, `_`, `-`, `.`, до 64 символов), а не сам ключ"
    )]
    BadKeyRef(String),
}

/// Предупреждение конфигурации.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigWarning {
    /// У эндпоинтов облачной группы разные `daily_budget_usd`; действует `used` —
    /// минимум (D34).
    BudgetMismatch { group: String, used: f64 },
}

impl ModelsConfig {
    /// Проверка ссылок и обязательных полей. Ошибка — первая найденная.
    pub fn validate(&self) -> Result<Vec<ConfigWarning>, ConfigError> {
        let mut ids = BTreeSet::new();
        for ep in &self.endpoints {
            if !ids.insert(&ep.id) {
                return Err(ConfigError::DuplicateId(ep.id.clone()));
            }
            ep.validate()?;
        }
        if let Some(d) = &self.default {
            let ep = self
                .endpoint(d)
                .ok_or_else(|| ConfigError::UnknownRef(d.clone()))?;
            if ep.pool() != PoolKind::Local {
                return Err(ConfigError::CloudDefault(d.clone()));
            }
        }
        for (role, id) in &self.roles {
            if parse_role(role).is_none() {
                return Err(ConfigError::UnknownRole(role.clone()));
            }
            if self.endpoint(id).is_none() {
                return Err(ConfigError::UnknownRef(id.clone()));
            }
        }
        Ok(self.budget_warnings())
    }

    /// Эндпоинт роли run (по serde-имени роли).
    pub fn role(&self, r: RunRole) -> Option<&EndpointId> {
        let name = serde_json::to_value(r).ok()?;
        self.roles.get(name.as_str()?)
    }

    /// Дневной бюджет группы: минимум заданных `daily_budget_usd` её эндпоинтов (D34).
    pub fn group_budget(&self, group: &str) -> Option<f64> {
        self.endpoints
            .iter()
            .filter(|e| e.group_key() == group)
            .filter_map(|e| e.daily_budget_usd)
            .reduce(f64::min)
    }

    /// Эндпоинт по id.
    pub fn endpoint(&self, id: &EndpointId) -> Option<&EndpointConfig> {
        self.endpoints.iter().find(|e| &e.id == id)
    }

    fn budget_warnings(&self) -> Vec<ConfigWarning> {
        let mut groups: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for e in &self.endpoints {
            if let Some(b) = e.daily_budget_usd {
                groups.entry(e.group_key()).or_default().push(b);
            }
        }
        groups
            .into_iter()
            .filter(|(_, v)| v.iter().any(|b| *b != v[0]))
            .map(|(group, v)| ConfigWarning::BudgetMismatch {
                group,
                used: v.into_iter().fold(f64::INFINITY, f64::min),
            })
            .collect()
    }
}

/// Роль по serde-имени.
fn parse_role(name: &str) -> Option<RunRole> {
    serde_json::from_value(serde_json::Value::String(name.to_owned())).ok()
}

impl EndpointConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        use BackendKind as B;
        if let Some(k) = &self.key_ref
            && !valid_key_ref(k)
        {
            return Err(ConfigError::BadKeyRef(self.id.0.clone()));
        }
        match (&self.base_url, self.backend) {
            (None, B::Vllm | B::OpenAiCompatible) => {
                return Err(ConfigError::MissingBaseUrl(self.id.clone()));
            }
            (Some(u), _) if parse_host(u).is_none() => {
                return Err(ConfigError::BadBaseUrl(self.id.clone()));
            }
            _ => {}
        }
        let model_required = matches!(
            self.backend,
            B::OpenRouter | B::Anthropic | B::OpenAiResponses | B::OpenAiChat
        );
        if model_required && self.model.as_deref().is_none_or(|m| m.trim().is_empty()) {
            return Err(ConfigError::MissingModel(self.id.clone()));
        }
        Ok(())
    }

    /// Адрес без завершающего `/`: `base_url` или умолчание бэкенда (D37). У `vllm` и
    /// `openai_compatible` умолчания нет — без `base_url` пустая строка (после
    /// [`ModelsConfig::validate`] так не бывает).
    pub fn effective_base_url(&self) -> String {
        if let Some(u) = &self.base_url {
            return u.trim_end_matches('/').to_owned();
        }
        match self.backend {
            BackendKind::Anthropic => "https://api.anthropic.com/v1",
            BackendKind::OpenAiResponses | BackendKind::OpenAiChat => "https://api.openai.com/v1",
            BackendKind::OpenRouter => "https://openrouter.ai/api/v1",
            BackendKind::LlamaCpp => "http://127.0.0.1:8080",
            BackendKind::Vllm | BackendKind::OpenAiCompatible => "",
        }
        .to_owned()
    }

    /// Пул (D25): облачные бэкенды всегда `Cloud`; llama.cpp, vLLM и совместимый —
    /// `Local` на loopback и частном адресе (RFC 1918, `fc00::/7`), иначе `Cloud`.
    /// Имя хоста, кроме `localhost`, не разрешается: оно `Cloud`, если `pool` не задан.
    pub fn pool(&self) -> PoolKind {
        if let Some(p) = self.pool {
            return p;
        }
        match self.backend {
            BackendKind::Anthropic
            | BackendKind::OpenAiResponses
            | BackendKind::OpenAiChat
            | BackendKind::OpenRouter => PoolKind::Cloud,
            BackendKind::LlamaCpp | BackendKind::Vllm | BackendKind::OpenAiCompatible => {
                if self.host_is_local() {
                    PoolKind::Local
                } else {
                    PoolKind::Cloud
                }
            }
        }
    }

    /// Сервер на этом хосте: loopback или `localhost`. Допуск по ресурсам узла
    /// применяется только к таким эндпоинтам (D25).
    pub fn on_host(&self) -> bool {
        match parse_host(&self.effective_base_url()) {
            Some(Host::Ip(ip)) => ip.is_loopback(),
            Some(Host::Name(n)) => n.eq_ignore_ascii_case("localhost"),
            None => false,
        }
    }

    /// Адрес в локальной сети или на этом хосте (по адресу, без учёта `pool`).
    pub fn host_is_local(&self) -> bool {
        match parse_host(&self.effective_base_url()) {
            Some(Host::Ip(ip)) => ip.is_loopback() || is_private(ip),
            Some(Host::Name(n)) => n.eq_ignore_ascii_case("localhost"),
            None => false,
        }
    }

    /// Ключ группы очереди: у локального — id эндпоинта, у облачного —
    /// `cloud:<backend>:<host>` (D34; задачи 13, 15).
    pub fn group_key(&self) -> String {
        match self.pool() {
            PoolKind::Local => self.id.0.clone(),
            PoolKind::Cloud => {
                let backend = serde_json::to_value(self.backend)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default();
                let host = reqwest::Url::parse(&self.effective_base_url())
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
                    .unwrap_or_default();
                format!("cloud:{backend}:{host}")
            }
        }
    }
}

/// Стоимость вызова по ценам (D16). `Usage.input` включает кэш: кэшированная часть
/// идёт по `cached_per_mtok` (нет — по цене входа), запись в кэш — по цене входа
/// (отдельной цены записи в [`Price`] нет).
pub fn compute_cost(price: &Price, u: &Usage) -> f64 {
    let cached = u.cached.min(u.input);
    let fresh = u.input - cached;
    let cached_price = price.cached_per_mtok.unwrap_or(price.input_per_mtok);
    (fresh as f64 * price.input_per_mtok
        + cached as f64 * cached_price
        + u.output as f64 * price.output_per_mtok)
        / 1e6
}

/// Имя ключа: 1–64 символа `[A-Za-z0-9_.-]`, начинается с буквы или цифры и не похоже
/// на сам ключ (`sk-…`, `sk_…`: так начинаются ключи OpenAI, Anthropic, OpenRouter).
fn valid_key_ref(k: &str) -> bool {
    let lower = k.to_ascii_lowercase();
    !k.is_empty()
        && k.len() <= 64
        && k.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && !lower.starts_with("sk-")
        && !lower.starts_with("sk_")
}

enum Host {
    Ip(IpAddr),
    Name(String),
}

/// Хост адреса `http(s)://…`; `None` — адрес не разобран или схема другая.
fn parse_host(url: &str) -> Option<Host> {
    let u = reqwest::Url::parse(url).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let h = u.host_str()?;
    let bare = h
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .unwrap_or(h);
    Some(match bare.parse::<IpAddr>() {
        Ok(ip) => Host::Ip(ip),
        Err(_) => Host::Name(h.to_owned()),
    })
}

/// Частный адрес: RFC 1918 и `fc00::/7` (D25).
fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => is_private_v4(a),
        IpAddr::V6(a) => is_ula(a) || a.to_ipv4_mapped().is_some_and(is_private_v4),
    }
}

fn is_private_v4(a: Ipv4Addr) -> bool {
    a.is_private()
}

fn is_ula(a: Ipv6Addr) -> bool {
    (a.segments()[0] & 0xfe00) == 0xfc00
}
