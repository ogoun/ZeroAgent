//! Конфигурация моделей: раздел `[models]` (D23, D24, D25, D34, D37).
//!
//! Ключей в конфигурации нет: только ссылка `key_ref`, по которой ключ даёт
//! [`crate::SecretSource`] (D4). Поле `api_key` и любое другое неизвестное поле
//! отвергаются разбором (`deny_unknown_fields`).
//!
//! Текст ошибки разбора наружу — только через [`ConfigParseError`]: белый список из
//! места (строка, столбец), категории из фиксированного набора и имени поля, если оно
//! из списка известных. Текста парсера в ней нет: `Display` ошибки `toml` печатает
//! строку источника (`api_key = "sk-…"` целиком), а сообщения serde — значения
//! (`invalid type: string "…"`, ``unknown variant `…` ``, ``unknown field `…` ``).
//! Ошибки [`ModelsConfig::validate`] выводят id и имена ролей, только если они в
//! формате id; id эндпоинта вне формата — ошибка `BadId(№)` по номеру.

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

/// Ошибка конфигурации. Текст называет поле; значения `key_ref` не печатаются, id и
/// имена ролей — только в формате id (иначе `<недопустимый id>` или номер эндпоинта).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ConfigError {
    /// Номер эндпоинта с 1: id вне формата `[A-Za-z0-9][A-Za-z0-9_.-]{0,63}` (или
    /// похож на ключ) не печатается.
    #[error(
        "models.endpoints, эндпоинт №{0}: id должен быть из латиницы, цифр, `_`, `-`, `.` (до 64 символов)"
    )]
    BadId(usize),
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
    /// У группы задан `daily_budget_usd`, а у эндпоинта группы нет источника стоимости:
    /// нет `price`, и бэкенд не сообщает стоимость (её сообщает только OpenRouter).
    /// Расход такого эндпоинта не учитывался бы, и бюджет не наступил бы никогда.
    #[error(
        "models.endpoints[{0}]: у группы задан daily_budget_usd, но стоимость запросов неизвестна —          этот бэкенд её не сообщает; задайте price (USD за миллион токенов: input_per_mtok, output_per_mtok)          или уберите бюджет"
    )]
    BudgetWithoutPrice(EndpointId),
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
        for (i, ep) in self.endpoints.iter().enumerate() {
            if !valid_key_ref(&ep.id.0) {
                return Err(ConfigError::BadId(i + 1));
            }
        }
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
                .ok_or_else(|| ConfigError::UnknownRef(shown_id(d)))?;
            if ep.pool() != PoolKind::Local {
                return Err(ConfigError::CloudDefault(d.clone()));
            }
        }
        for (role, id) in &self.roles {
            let Some(_) = parse_role(role) else {
                return Err(ConfigError::UnknownRole(
                    shown_id(&EndpointId(role.clone())).0,
                ));
            };
            if self.endpoint(id).is_none() {
                return Err(ConfigError::UnknownRoleTarget {
                    role: role.clone(),
                    id: shown_id(id),
                });
            }
        }
        self.validate_budget_cost()?;
        Ok(self.budget_warnings())
    }

    /// Бюджет группы действует, только если стоимость известна для каждого эндпоинта
    /// группы (D16): `price` в конфигурации или стоимость от провайдера (OpenRouter).
    fn validate_budget_cost(&self) -> Result<(), ConfigError> {
        for ep in &self.endpoints {
            let reports_cost = ep.backend == BackendKind::OpenRouter;
            if ep.price.is_none() && !reports_cost && self.group_budget(&ep.group_key()).is_some() {
                return Err(ConfigError::BudgetWithoutPrice(ep.id.clone()));
            }
        }
        Ok(())
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

/// Категория ошибки разбора — фиксированный набор: текст парсера наружу не выходит.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// Синтаксис TOML/JSON (незакрытая строка, лишний символ и т. п.).
    Syntax,
    /// Неверный тип значения (строка вместо числа и т. п.).
    InvalidType,
    /// Значение нужного типа, но недопустимое (длина, диапазон).
    InvalidValue,
    UnknownField,
    /// Неизвестное значение перечисления (`backend`, `pool`, `reasoning`).
    UnknownVariant,
    MissingField,
    DuplicateField,
    Other,
}

impl ParseErrorKind {
    fn text(self) -> &'static str {
        match self {
            Self::Syntax => "синтаксическая ошибка",
            Self::InvalidType => "неверный тип значения",
            Self::InvalidValue => "недопустимое значение",
            Self::UnknownField => "неизвестное поле",
            Self::UnknownVariant => "неизвестное значение перечисления",
            Self::MissingField => "нет обязательного поля",
            Self::DuplicateField => "поле задано дважды",
            Self::Other => "ошибка разбора",
        }
    }
}

/// Имена полей, которые ошибка разбора может назвать: разделы `config.toml`, поля схемы
/// `[models]` и `[resources]` (`za-res::ResourcesConfig`) и частые ошибочные имена поля
/// ключа. Любое другое имя (в том числе ключ, записанный именем поля TOML) не выводится.
const SHOWN_FIELDS: &[&str] = &[
    "models",
    "resources",
    "ram_reserve_mib",
    "ram_floor_mib",
    "vram_reserve_mib",
    "vram_floor_mib",
    "disk_margin_mib",
    "journal_low_mib",
    "cpu_busy_percent",
    "cpu_busy_for_s",
    "hysteresis",
    "nvidia_smi",
    "default",
    "endpoints",
    "roles",
    "id",
    "backend",
    "base_url",
    "model",
    "key_ref",
    "pool",
    "max_parallel",
    "context_tokens",
    "daily_budget_usd",
    "price",
    "reasoning",
    "input_per_mtok",
    "cached_per_mtok",
    "cache_write_per_mtok",
    "output_per_mtok",
    "effort",
    "budget",
    "api_key",
    "apikey",
    "key",
    "token",
    "secret",
    "password",
];

/// Ошибка разбора конфигурации (D4, SEC-044) по белому списку: место, категория из
/// фиксированного набора и имя поля, только если оно из [`SHOWN_FIELDS`]. Текст
/// парсера не хранится и не выводится.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.render())]
pub struct ConfigParseError {
    /// Строка и столбец (с 1), если известны.
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub kind: ParseErrorKind,
    /// Имя поля из [`SHOWN_FIELDS`], если ошибка его называет.
    pub field: Option<&'static str>,
}

impl ConfigParseError {
    /// Из сообщения парсера и смещения в исходнике. Для `toml`:
    /// `ConfigParseError::from_message(e.message(), e.span().map(|s| s.start), src)`.
    /// Сообщение только классифицируется; `Display` самой ошибки `toml` печатает
    /// строку исходника и наружу не выводится.
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
        let (kind, field) = classify(message, ParseErrorKind::Syntax);
        Self {
            line,
            column,
            kind,
            field,
        }
    }

    /// Из ошибки `serde_json`: место — `line()`/`column()`, категория — `classify()`
    /// и начало сообщения.
    pub fn from_json(e: &serde_json::Error) -> Self {
        let (kind, field) = match e.classify() {
            serde_json::error::Category::Data => classify(&e.to_string(), ParseErrorKind::Other),
            _ => (ParseErrorKind::Syntax, None),
        };
        Self {
            line: Some(e.line()).filter(|l| *l > 0),
            column: Some(e.column()).filter(|c| *c > 0),
            kind,
            field,
        }
    }

    fn render(&self) -> String {
        let mut out = String::from("конфигурация");
        match (self.line, self.column) {
            (Some(l), Some(c)) => out.push_str(&format!(", строка {l}, столбец {c}")),
            (Some(l), None) => out.push_str(&format!(", строка {l}")),
            _ => {}
        }
        out.push_str(": ");
        out.push_str(self.kind.text());
        if let Some(f) = self.field {
            out.push_str(&format!(" `{f}`"));
            if matches!(
                f,
                "api_key" | "apikey" | "key" | "token" | "secret" | "password"
            ) {
                out.push_str(" (ключ в конфигурации не задаётся: key_ref и ZA_SECRET_<ИМЯ>)");
            }
        }
        out
    }
}

/// Категория и имя поля по началу сообщения serde/toml. Сообщения, не начинающиеся с
/// известных форм serde, — `fallback` (у `toml` — синтаксис).
fn classify(message: &str, fallback: ParseErrorKind) -> (ParseErrorKind, Option<&'static str>) {
    let m = message.trim_start();
    let forms = [
        ("unknown field `", ParseErrorKind::UnknownField),
        ("missing field `", ParseErrorKind::MissingField),
        ("duplicate field `", ParseErrorKind::DuplicateField),
        ("duplicate key `", ParseErrorKind::DuplicateField),
    ];
    for (prefix, kind) in forms {
        if let Some(rest) = m.strip_prefix(prefix) {
            let name = rest.split('`').next().unwrap_or("");
            let field = SHOWN_FIELDS.iter().copied().find(|f| *f == name);
            return (kind, field);
        }
    }
    let kind = if m.starts_with("unknown variant") {
        ParseErrorKind::UnknownVariant
    } else if m.starts_with("invalid type") {
        ParseErrorKind::InvalidType
    } else if m.starts_with("invalid value") || m.starts_with("invalid length") {
        ParseErrorKind::InvalidValue
    } else if m.starts_with("duplicate") {
        ParseErrorKind::DuplicateField
    } else {
        fallback
    };
    (kind, None)
}

impl ModelsConfig {
    /// Разбор JSON с ошибкой без фрагментов исходника.
    pub fn from_json_str(s: &str) -> Result<Self, ConfigParseError> {
        serde_json::from_str(s).map_err(|e| ConfigParseError::from_json(&e))
    }
}

/// Значение, которое можно показать в ошибке `validate` как id: формат id (как у
/// `key_ref`: `[A-Za-z0-9][A-Za-z0-9_.-]{0,63}`, не `sk-…`). Иначе — заглушка.
fn shown_id(id: &EndpointId) -> EndpointId {
    if valid_key_ref(&id.0) {
        id.clone()
    } else {
        EndpointId("<недопустимый id>".to_owned())
    }
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
