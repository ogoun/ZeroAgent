//! Конфигурация моделей: раздел `[models]` (D23, D24, D25, D34, D37).
//!
//! Ключей в конфигурации нет: только ссылка `key_ref`, по которой ключ даёт
//! [`crate::SecretSource`] (D4). Поле `api_key` и любое другое неизвестное поле
//! отвергаются разбором (`deny_unknown_fields`).
//!
//! Текст ошибки разбора наружу — только через [`ConfigParseError`]: место (строка,
//! столбец) и сообщение без фрагментов исходного текста. `Display` ошибки `toml`
//! печатает строку источника с указателем (`api_key = "sk-…"` целиком), а сообщения
//! serde вида `invalid type: string "…"` — значение поля; оба пути значения теряют.

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
    /// Запись в кэш (у Anthropic ×1,25 или ×2 от входа); `None` — по цене входа.
    #[serde(default)]
    pub cache_write_per_mtok: Option<f64>,
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
    #[error("models.default: эндпоинт {0} не описан в models.endpoints")]
    UnknownRef(EndpointId),
    #[error("models.roles.{role}: эндпоинт {id} не описан в models.endpoints")]
    UnknownRoleTarget { role: String, id: EndpointId },
    #[error(
        "models.default: эндпоинт {0} облачный, моделью по умолчанию может быть только локальная (D24)"
    )]
    CloudDefault(EndpointId),
    #[error("models.endpoints[{0}].model: для этого бэкенда модель обязательна")]
    MissingModel(EndpointId),
    #[error("models.endpoints[{0}].base_url: для vllm и openai_compatible адрес обязателен")]
    MissingBaseUrl(EndpointId),
    #[error(
        "models.endpoints[{0}].base_url: нужен адрес http(s) с хостом, без учётных данных и параметров запроса"
    )]
    BadBaseUrl(EndpointId),
    #[error(
        "models.endpoints[{0}].pool: anthropic, openai_responses, openai_chat и openrouter всегда облачные (D25)"
    )]
    CloudPool(EndpointId),
    /// Аргумент — путь поля (`models.endpoints[x].max_parallel`).
    #[error("{0}: значение вне допустимого диапазона (больше нуля, конечное число)")]
    BadValue(String),
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
                return Err(ConfigError::UnknownRoleTarget {
                    role: role.clone(),
                    id: id.clone(),
                });
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
            // Значения конечны и положительны после `validate`.
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
            // Учётные данные и параметры запроса в адресе (`user:sk-…@`, `?key=…`) попали
            // бы в текст ошибок reqwest и в логи.
            (Some(u), _) if parse_host(u).is_none() || has_credentials(u) => {
                return Err(ConfigError::BadBaseUrl(self.id.clone()));
            }
            _ => {}
        }
        if self.pool == Some(PoolKind::Local) && is_cloud_backend(self.backend) {
            return Err(ConfigError::CloudPool(self.id.clone()));
        }
        self.validate_ranges()?;
        let model_required = matches!(
            self.backend,
            B::OpenRouter | B::Anthropic | B::OpenAiResponses | B::OpenAiChat
        );
        if model_required && self.model.as_deref().is_none_or(|m| m.trim().is_empty()) {
            return Err(ConfigError::MissingModel(self.id.clone()));
        }
        Ok(())
    }

    /// Числа: `max_parallel`, `context_tokens` больше нуля; бюджет конечный и больше
    /// нуля, цены конечные и не меньше нуля.
    fn validate_ranges(&self) -> Result<(), ConfigError> {
        let field =
            |name: &str| ConfigError::BadValue(format!("models.endpoints[{}].{name}", self.id));
        if self.max_parallel == Some(0) {
            return Err(field("max_parallel"));
        }
        if self.context_tokens == Some(0) {
            return Err(field("context_tokens"));
        }
        if self
            .daily_budget_usd
            .is_some_and(|b| !(b.is_finite() && b > 0.0))
        {
            return Err(field("daily_budget_usd"));
        }
        if let Some(p) = &self.price {
            let bad = |x: f64| !(x.is_finite() && x >= 0.0);
            if bad(p.input_per_mtok)
                || bad(p.output_per_mtok)
                || p.cached_per_mtok.is_some_and(bad)
                || p.cache_write_per_mtok.is_some_and(bad)
            {
                return Err(field("price"));
            }
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

    /// Пул (D25): облачные бэкенды всегда `Cloud`, `pool` у них не действует
    /// (`pool = "local"` отвергает [`ModelsConfig::validate`]). llama.cpp, vLLM и
    /// совместимый — `pool`, если задан, иначе `Local` на loopback, частном адресе
    /// (RFC 1918, `fc00::/7`), CGNAT `100.64.0.0/10` (Tailscale) и link-local, иначе
    /// `Cloud`. Имя хоста, кроме `localhost`, не разрешается: оно `Cloud`.
    pub fn pool(&self) -> PoolKind {
        if is_cloud_backend(self.backend) {
            return PoolKind::Cloud;
        }
        if let Some(p) = self.pool {
            return p;
        }
        if self.host_is_local() {
            PoolKind::Local
        } else {
            PoolKind::Cloud
        }
    }

    /// Сервер на этом хосте: loopback или `localhost`. Допуск по ресурсам узла
    /// применяется только к таким эндпоинтам (D25).
    pub fn on_host(&self) -> bool {
        match parse_host(&self.effective_base_url()) {
            Some(Host::Ip(ip)) => is_loopback(ip),
            Some(Host::Name(n)) => n.eq_ignore_ascii_case("localhost"),
            None => false,
        }
    }

    /// Адрес в локальной сети или на этом хосте (по адресу, без учёта `pool`).
    pub fn host_is_local(&self) -> bool {
        match parse_host(&self.effective_base_url()) {
            Some(Host::Ip(ip)) => is_loopback(ip) || is_private(ip),
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

/// Стоимость вызова по ценам (D16). `Usage.input` включает кэш: прочитанное из кэша
/// идёт по `cached_per_mtok`, записанное — по `cache_write_per_mtok` (нет цены — по
/// цене входа), остальной вход — по `input_per_mtok`.
pub fn compute_cost(price: &Price, u: &Usage) -> f64 {
    let cached = u.cached.min(u.input);
    let written = u.cache_write.min(u.input - cached);
    let fresh = u.input - cached - written;
    let input = price.input_per_mtok;
    (fresh as f64 * input
        + cached as f64 * price.cached_per_mtok.unwrap_or(input)
        + written as f64 * price.cache_write_per_mtok.unwrap_or(input)
        + u.output as f64 * price.output_per_mtok)
        / 1e6
}

/// Ошибка разбора конфигурации без фрагментов исходного текста (D4, SEC-044): место
/// и сообщение, из которого вырезаны значения в кавычках.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.render())]
pub struct ConfigParseError {
    /// Строка и столбец (с 1), если известны.
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// Тип ошибки и путь ключа (``unknown field `api_key` ``), без значений.
    pub message: String,
}

impl ConfigParseError {
    /// Из сообщения парсера и смещения в исходнике. Для `toml`:
    /// `ConfigParseError::from_message(e.message(), e.span().map(|s| s.start), src)` —
    /// не `e.to_string()`: его `Display` печатает строку исходника.
    pub fn from_message(message: &str, offset: Option<usize>, source: &str) -> Self {
        let (line, column) = match offset {
            Some(off) => {
                let mut off = off.min(source.len());
                while !source.is_char_boundary(off) {
                    off -= 1;
                }
                let before = &source[..off];
                let line = before.matches('\n').count() + 1;
                let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
                (Some(line), Some(column))
            }
            None => (None, None),
        };
        Self {
            line,
            column,
            message: sanitize_parse_message(message),
        }
    }

    /// Из ошибки `serde_json`: её `Display` добавляет « at line L column C», место
    /// берётся из `line()`/`column()`.
    pub fn from_json(e: &serde_json::Error) -> Self {
        let text = e.to_string();
        let text = text
            .rfind(" at line ")
            .map_or(text.as_str(), |i| &text[..i]);
        Self {
            line: Some(e.line()).filter(|l| *l > 0),
            column: Some(e.column()).filter(|c| *c > 0),
            message: sanitize_parse_message(text),
        }
    }

    fn render(&self) -> String {
        match (self.line, self.column) {
            (Some(l), Some(c)) => format!(
                "конфигурация моделей, строка {l}, столбец {c}: {}",
                self.message
            ),
            (Some(l), None) => format!("конфигурация моделей, строка {l}: {}", self.message),
            _ => format!("конфигурация моделей: {}", self.message),
        }
    }
}

impl ModelsConfig {
    /// Разбор JSON с ошибкой без фрагментов исходника.
    pub fn from_json_str(s: &str) -> Result<Self, ConfigParseError> {
        serde_json::from_str(s).map_err(|e| ConfigParseError::from_json(&e))
    }
}

/// Сообщение парсера без значений: строки в двойных и одинарных кавычках заменены на
/// `"…"`; имена полей в обратных кавычках остаются. Только первая непустая строка
/// сообщения, не длиннее 300 символов.
fn sanitize_parse_message(msg: &str) -> String {
    let first = msg
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut out = String::new();
    let mut quote: Option<char> = None;
    for c in first.chars() {
        match quote {
            Some(q) if c == q => {
                out.push('…');
                out.push(c);
                quote = None;
            }
            Some(_) => {}
            None if c == '"' || c == '\'' => {
                out.push(c);
                quote = Some(c);
            }
            None => out.push(c),
        }
    }
    if quote.is_some() {
        out.push('…');
    }
    // serde пишет значение неизвестного варианта в обратных кавычках
    // (``unknown variant `sk-…`, expected one of …``): оно вырезается тоже. Имя
    // неизвестного поля (``unknown field `api_key` ``) остаётся — это путь ключа.
    const VARIANT: &str = "unknown variant `";
    if let Some(i) = out.find(VARIANT) {
        let start = i + VARIANT.len();
        if let Some(len) = out[start..].find('`') {
            out.replace_range(start..start + len, "…");
        }
    }
    out.chars().take(300).collect()
}

/// Облачные бэкенды — всегда облачные, по адресу не уточняются (D25).
fn is_cloud_backend(b: BackendKind) -> bool {
    matches!(
        b,
        BackendKind::Anthropic
            | BackendKind::OpenAiResponses
            | BackendKind::OpenAiChat
            | BackendKind::OpenRouter
    )
}

/// В адресе есть учётные данные или параметры запроса.
fn has_credentials(url: &str) -> bool {
    reqwest::Url::parse(url)
        .is_ok_and(|u| !u.username().is_empty() || u.password().is_some() || u.query().is_some())
}

/// Имя ключа: 1–64 символа `[A-Za-z0-9_.-]`, начинается с буквы или цифры и не похоже
/// на сам ключ (`sk-…`, `sk_…`: так начинаются ключи OpenAI, Anthropic, OpenRouter).
pub(crate) fn valid_key_ref(k: &str) -> bool {
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

/// Loopback, в том числе IPv4 в IPv6 (`::ffff:127.0.0.1`).
fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_loopback(),
        IpAddr::V6(a) => a.is_loopback() || a.to_ipv4_mapped().is_some_and(|v| v.is_loopback()),
    }
}

/// Адрес локальной сети (D25): RFC 1918, CGNAT `100.64.0.0/10` (Tailscale),
/// link-local `169.254.0.0/16`, `fc00::/7`, `fe80::/10`, IPv4 в IPv6.
fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => is_private_v4(a),
        IpAddr::V6(a) => {
            is_ula(a) || is_link_local_v6(a) || a.to_ipv4_mapped().is_some_and(is_private_v4)
        }
    }
}

fn is_private_v4(a: Ipv4Addr) -> bool {
    let [o1, o2, ..] = a.octets();
    a.is_private() || a.is_link_local() || (o1 == 100 && (o2 & 0xc0) == 64)
}

fn is_link_local_v6(a: Ipv6Addr) -> bool {
    (a.segments()[0] & 0xffc0) == 0xfe80
}

fn is_ula(a: Ipv6Addr) -> bool {
    (a.segments()[0] & 0xfe00) == 0xfc00
}
