//! Конфигурация агента: `ZA_HOME/config.toml`, разделы `[models]`, `[resources]` (D23),
//! `[budgets.run]` (бюджет run по умолчанию, R1-B1 D19), `[context]` (пороги контекста,
//! R1-B2 D65), `[policy]` с `[[policy.rules]]` и `[[policy.grants]]` (политика, R1-C1
//! D78, D80; правила по домену — R1-C2 D105), `[web]` с `[web.search]` (сетевой шлюз и
//! поставщик поиска, R1-C2 D107, D122, D125) и `[skills]` (источники скиллов, D114, D122).
//! Проектная конфигурация — `[policy]` и `[web] enabled = false`
//! ([`parse_project_config`]): адрес поставщика поиска проект не задаёт (D126).
//!
//! `[policy]`, `[web]` и `[skills]` разбираются вторым проходом по уже прочитанной
//! таблице: ошибка называет путь поля (`policy.rules[0]`, `web.search.url`), а не только
//! строку.
//!
//! Текст ошибки разбора наружу — только [`ConfigParseError`] (белый список: место,
//! категория, имя известного поля). `Display` ошибки `toml` печатает строку исходника,
//! где может стоять ключ (`api_key = "sk-…"`), поэтому он не используется нигде (SEC-044).
//!
//! Файла нет — пустая конфигурация. Раздела `[models]` нет, а `LLAMA_URL` задан — один
//! эндпоинт `local` (`llama_cpp`) и `default = "local"`: совместимость с `doctor` R0.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use za_agent::{
    ContextConfig, ContextConfigError, GrantConfig, PolicyConfig, PolicyError, PolicySnapshot, Rule,
};
use za_llm::config::{ConfigError, ConfigParseError, EndpointConfig, ModelsConfig};
use za_llm::{BackendKind, EndpointId};
use za_protocol::Budget;
use za_res::{ResourcesConfig, ResourcesConfigError};

use crate::doctor::LLAMA_URL_ENV;

/// Имя файла конфигурации в `ZA_HOME`.
pub const CONFIG_FILE: &str = "config.toml";
/// id эндпоинта, который создаётся из `LLAMA_URL` без раздела `[models]`.
pub const LLAMA_URL_ENDPOINT: &str = "local";

/// Конфигурация агента (R1-A, R1-B1, R1-C1).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentConfig {
    pub models: Option<ModelsConfig>,
    pub resources: ResourcesConfig,
    /// `[budgets.run]`: пределы run по умолчанию (R1-B1 D19).
    pub budgets: BudgetsConfig,
    /// `[context]`: тир, пороги компакции и удаления, доля выгрузки (R1-B2 D65).
    /// Незнакомый ключ — ошибка разбора (`deny_unknown_fields` у `ContextConfig`).
    pub context: ContextConfig,
    /// `[policy]`: `allow_bypass`, правила и гранты глобальной конфигурации (D78, D80).
    pub policy: PolicyConfig,
    /// `[web]` и `[web.search]` (D107, D122): шлюз, лимиты, поставщик поиска.
    pub web: WebSettings,
    /// `[skills]` (D114, D122): источники скиллов вне проекта.
    pub skills: SkillsSettings,
    /// `models` — запасной эндпоинт `local` из `LLAMA_URL`, а не раздел файла.
    pub llama_url_fallback: bool,
}

/// Раздел `[web]` глобальной конфигурации (D107, D122). Значения по умолчанию — D107:
/// веб включён, `robots.txt` соблюдается, 5 МиБ тела, 60 запросов в минуту, порты 80, 443,
/// 8080, 8443; поиска нет.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSettings {
    /// `false` — `web_fetch` и `web_search` не регистрируются ни в одном run.
    pub enabled: bool,
    pub robots: bool,
    /// Предел тела ответа, байт (у PDF — свой, 32 МиБ, D113).
    pub max_bytes: u64,
    /// Запросов в минуту на процесс, не меньше 1.
    pub requests_per_minute: u32,
    /// Разрешённые порты `web_fetch`, непустой список.
    pub ports: Vec<u16>,
    /// `[web.search]` с `url`; без `url` поиска нет (D111).
    pub search: Option<SearchSettings>,
}

impl Default for WebSettings {
    fn default() -> Self {
        let d = za_web::WebConfig::default();
        Self {
            enabled: true,
            robots: d.robots,
            max_bytes: d.max_bytes,
            requests_per_minute: d.requests_per_minute,
            ports: d.ports,
            search: None,
        }
    }
}

/// `[web.search]` (D122, D125): `provider` — только [`SEARXNG`] (по умолчанию), `url` —
/// базовый адрес своего экземпляра (`http`/`https` с хостом, без учётных данных).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSettings {
    pub provider: String,
    pub url: url::Url,
}

/// Единственный поставщик поиска R1-C2 (D125).
pub const SEARXNG: &str = "searxng";

/// Раздел `[skills]` (D114, D122): `enabled = false` — каталога нет (ни `activate_skill`,
/// ни `/имя`, ни роутера); `dirs` — каталоги источника `extra` по порядку (абсолютные
/// пути); `max_entries` — предел записей обхода на корень `user` и `extra` (перенос задачи
/// 12; по умолчанию [`za_skills::LOCAL_TREE_LIMITS`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillsSettings {
    pub enabled: bool,
    pub dirs: Vec<PathBuf>,
    pub max_entries: usize,
}

impl Default for SkillsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            dirs: Vec::new(),
            max_entries: za_skills::LOCAL_TREE_LIMITS.max_entries,
        }
    }
}

/// Наибольшее `[skills] max_entries`: обход стоит около 1,25 мс на файл (замер задачи 12),
/// так что 100 000 записей — около двух минут старта run на корень.
pub const MAX_SKILL_ENTRIES: usize = 100_000;

/// Файл как его читает `toml`: `[policy]`, `[web]` и `[skills]` — таблицами, их разбирают
/// [`parse_policy`], [`parse_web`] и [`parse_skills`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    models: Option<ModelsConfig>,
    #[serde(default)]
    resources: ResourcesConfig,
    #[serde(default)]
    budgets: BudgetsConfig,
    #[serde(default)]
    context: ContextConfig,
    #[serde(default)]
    policy: Option<toml::Value>,
    #[serde(default)]
    web: Option<toml::Value>,
    #[serde(default)]
    skills: Option<toml::Value>,
}

/// Раздел `[budgets]`. Незнакомый ключ — ошибка разбора: `deny_unknown_fields`
/// здесь и у `Budget` (атрибут внешней структуры на вложенную не действует).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetsConfig {
    /// `[budgets.run]`: `tokens`, `gpu_ms`, `usd`, `steps`, `duration_ms`; всё необязательно.
    #[serde(default)]
    pub run: Budget,
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
    /// Недопустимое значение предела `[budgets.run]`.
    #[error("{file}: budgets.run.{field}: {reason}", file = CONFIG_FILE)]
    Budget {
        field: &'static str,
        reason: &'static str,
    },
    /// Значение `[context]` вне диапазона (R1-B2 D65).
    #[error("{file}: {0}", file = CONFIG_FILE)]
    Context(ContextConfigError),
    /// Неверное поле `[policy]`: `at` — путь поля (`policy.rules[0]`).
    #[error("{file}: {at}: {reason}", file = CONFIG_FILE)]
    Policy { at: String, reason: String },
    /// Правило `[[policy.rules]]` без полей или с неверным шаблоном (D80).
    #[error("{file}: {0}", file = CONFIG_FILE)]
    PolicyRules(PolicyError),
    /// Неверное поле `[web]` или `[skills]` (D122): `at` — путь поля (`web.search.url`),
    /// значение в текст не попадает.
    #[error("{file}: {at}: {reason}", file = CONFIG_FILE)]
    Section { at: String, reason: String },
    /// Ошибка проектной конфигурации `.zero-agent/config.toml` (D80, D83): текст — как у
    /// ошибок глобальной (место, категория, путь поля), без фрагментов исходника.
    #[error("{file} проекта: {0}", file = PROJECT_CONFIG)]
    Project(String),
}

/// Проектная конфигурация в каталоге проекта (D80, D83): читается только у доверенного
/// проекта.
pub const PROJECT_CONFIG: &str = ".zero-agent/config.toml";

/// Проектная конфигурация (R1-C1 D80, ARC-041): действует раздел `[policy]`, и в нём —
/// правила `ask` и `deny` (проектные `allow`, `allow_bypass` и `[[policy.grants]]`
/// отбрасывает `PolicySnapshot::build` с `warn`), и `[web] enabled = false` (R1-C2 D105).
/// Прочие разделы и ключи не действуют — `warn`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectConfig {
    pub policy: PolicyConfig,
    /// `Some(false)` — проект выключил веб (`[web] enabled = false`): `web_search` и
    /// `web_fetch` в run не регистрируются. Проект только ужесточает: `true` и отсутствие
    /// ключа — `None`.
    pub web_enabled: Option<bool>,
}

/// Разделы глобальной конфигурации, которые в проектной не действуют: их имена `warn`
/// называет; прочие — без имени (ключ может оказаться секретом).
const GLOBAL_ONLY_SECTIONS: &[&str] = &["models", "resources", "budgets", "context"];

/// Ключи `[web]` и `[web.search]` глобальной конфигурации (D122): в проекте, кроме
/// `enabled`, не действуют. Только эти имена предупреждение называет (как у
/// [`GLOBAL_ONLY_SECTIONS`]).
const WEB_KEYS: &[&str] = &[
    "robots",
    "max_bytes",
    "requests_per_minute",
    "ports",
    "search",
];
const WEB_SEARCH_KEYS: &[&str] = &["provider", "url"];

/// Разбор текста `.zero-agent/config.toml` (D80, D105): `[policy]` — тем же разбором, что
/// глобальный, `[web] enabled` — выключение веба, прочие разделы и ключи — `warn` без
/// ошибки. Правила проверяются (шаблоны, `..`).
pub fn parse_project_config(src: &str) -> Result<ProjectConfig, AppConfigError> {
    let table = toml::from_str::<toml::Table>(src).map_err(|e| {
        AppConfigError::Project(
            ConfigParseError::from_message(e.message(), e.span().map(|s| s.start), src).to_string(),
        )
    })?;
    let mut out = ProjectConfig::default();
    for (key, value) in table {
        if key == "policy" {
            out.policy = parse_policy(value).map_err(project_error)?;
        } else if key == "web" {
            out.web_enabled = parse_project_web(value)?;
        } else if let Some(name) = GLOBAL_ONLY_SECTIONS.iter().find(|s| **s == key) {
            tracing::warn!(
                section = *name,
                "раздел проектной конфигурации не действует: только [policy] и [web] enabled (ARC-041)"
            );
            eprintln!(
                "zero-agent: {PROJECT_CONFIG}: раздел [{name}] в проекте не действует, только [policy] и [web] enabled"
            );
        } else {
            tracing::warn!("незнакомый раздел проектной конфигурации не действует");
            eprintln!(
                "zero-agent: {PROJECT_CONFIG}: незнакомый раздел не действует, только [policy] и [web] enabled"
            );
        }
    }
    // Только проверка правил: отбрасывание проектных `allow` (с `warn`) — при сборке
    // снимка run.
    PolicySnapshot::build(&out.policy, None).map_err(|e| AppConfigError::Project(e.to_string()))?;
    Ok(out)
}

/// Раздел `[web]` проектной конфигурации (D105, D122): `enabled = false` выключает веб,
/// `true` ничего не включает; `enabled` не булево — ошибка с путём `web.enabled`. Прочие
/// ключи `[web]` и `[web.search]` — `warn` с именем ключа (адрес поставщика поиска проект
/// не задаёт, D126); незнакомое имя не называется. Значения в предупреждения не попадают.
fn parse_project_web(v: toml::Value) -> Result<Option<bool>, AppConfigError> {
    let toml::Value::Table(t) = v else {
        return Err(AppConfigError::Project(
            "web: ожидается таблица [web]".to_owned(),
        ));
    };
    let mut enabled = None;
    for (key, value) in t {
        match (key.as_str(), value) {
            ("enabled", toml::Value::Boolean(b)) => enabled = (!b).then_some(false),
            ("enabled", _) => {
                return Err(AppConfigError::Project(
                    "web.enabled: ожидается true или false".to_owned(),
                ));
            }
            ("search", toml::Value::Table(search)) => {
                for k in search.keys() {
                    warn_project_web(
                        &format!("web.search.{k}"),
                        WEB_SEARCH_KEYS.contains(&k.as_str()),
                    );
                }
            }
            (k, _) => warn_project_web(&format!("web.{k}"), WEB_KEYS.contains(&k)),
        }
    }
    Ok(enabled)
}

/// Предупреждение о ключе `[web]` проекта, который не действует; `known` — имя ключа из
/// белого списка, иначе имя не называется (ключ может оказаться секретом).
fn warn_project_web(key: &str, known: bool) {
    if known {
        tracing::warn!(
            key,
            "ключ [web] проектной конфигурации не действует: только enabled = false"
        );
        eprintln!(
            "zero-agent: {PROJECT_CONFIG}: ключ {key} в проекте не действует, только [web] enabled = false"
        );
    } else {
        tracing::warn!("незнакомый ключ [web] проектной конфигурации не действует");
        eprintln!(
            "zero-agent: {PROJECT_CONFIG}: незнакомый ключ [web] в проекте не действует, только enabled = false"
        );
    }
}

/// Ошибка глобального разбора — как ошибка проектной конфигурации (без имени файла
/// глобальной).
fn project_error(e: AppConfigError) -> AppConfigError {
    match e {
        AppConfigError::Policy { at, reason } => AppConfigError::Project(format!("{at}: {reason}")),
        AppConfigError::PolicyRules(p) => AppConfigError::Project(p.to_string()),
        other => AppConfigError::Project(
            other
                .to_string()
                .trim_start_matches(&format!("{CONFIG_FILE}: "))
                .to_owned(),
        ),
    }
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
    let raw = toml::from_str::<RawConfig>(src).map_err(|e| {
        AppConfigError::Parse(ConfigParseError::from_message(
            e.message(),
            e.span().map(|s| s.start),
            src,
        ))
    })?;
    Ok(AgentConfig {
        models: raw.models,
        resources: raw.resources,
        budgets: raw.budgets,
        context: raw.context,
        policy: raw
            .policy
            .map(parse_policy)
            .transpose()?
            .unwrap_or_default(),
        web: raw.web.map(parse_web).transpose()?.unwrap_or_default(),
        skills: raw
            .skills
            .map(parse_skills)
            .transpose()?
            .unwrap_or_default(),
        llama_url_fallback: false,
    })
}

fn section_error(at: impl Into<String>, reason: impl Into<String>) -> AppConfigError {
    AppConfigError::Section {
        at: at.into(),
        reason: reason.into(),
    }
}

/// Имя незнакомого ключа, которое можно показать: строчные латинские буквы, цифры и `_`,
/// с буквы, не длиннее 24 символов. Ключи и токены известных форматов (`sk-…`, `ghp_…`
/// длиной 40, `AKIA…`, JWT) под это не подходят, и их имя не печатается (SEC-044).
fn shown_key(k: &str) -> Option<&str> {
    let ok = k.len() <= 24
        && k.starts_with(|c: char| c.is_ascii_lowercase())
        && k.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    ok.then_some(k)
}

/// Ошибка незнакомого ключа раздела `section`: путь — `section.<ключ>`, если имя можно
/// показать ([`shown_key`]), иначе только раздел.
fn unknown_key(section: &str, key: &str, expected: &str) -> AppConfigError {
    let at = match shown_key(key) {
        Some(k) => format!("{section}.{k}"),
        None => section.to_owned(),
    };
    section_error(at, format!("неизвестное поле; ожидается {expected}"))
}

fn table_at(at: &str, v: toml::Value) -> Result<toml::Table, AppConfigError> {
    match v {
        toml::Value::Table(t) => Ok(t),
        _ => Err(section_error(at, format!("ожидается таблица [{at}]"))),
    }
}

fn bool_at(at: &str, v: &toml::Value) -> Result<bool, AppConfigError> {
    v.as_bool()
        .ok_or_else(|| section_error(at, "ожидается true или false"))
}

/// Целое в `min..=max`.
fn int_at(at: &str, v: &toml::Value, min: i64, max: i64) -> Result<i64, AppConfigError> {
    match v.as_integer() {
        Some(n) if (min..=max).contains(&n) => Ok(n),
        Some(_) => Err(section_error(
            at,
            format!("ожидается целое от {min} до {max}"),
        )),
        None => Err(section_error(at, "ожидается целое число")),
    }
}

/// Раздел `[web]` (D107, D122): значения по умолчанию у отсутствующих ключей; незнакомый
/// ключ и неверный тип — ошибка с путём поля.
fn parse_web(v: toml::Value) -> Result<WebSettings, AppConfigError> {
    let mut out = WebSettings::default();
    for (key, value) in table_at("web", v)? {
        let at = format!("web.{key}");
        match key.as_str() {
            "enabled" => out.enabled = bool_at(&at, &value)?,
            "robots" => out.robots = bool_at(&at, &value)?,
            "max_bytes" => out.max_bytes = int_at(&at, &value, 1, i64::MAX)?.unsigned_abs(),
            "requests_per_minute" => {
                out.requests_per_minute =
                    u32::try_from(int_at(&at, &value, 1, i64::from(u32::MAX))?).unwrap_or(u32::MAX);
            }
            "ports" => out.ports = parse_ports(value)?,
            "search" => out.search = parse_search(value)?,
            other => {
                return Err(unknown_key(
                    "web",
                    other,
                    "enabled, robots, max_bytes, requests_per_minute, ports или [web.search]",
                ));
            }
        }
    }
    Ok(out)
}

/// `[web] ports`: непустой список портов 1–65535.
fn parse_ports(v: toml::Value) -> Result<Vec<u16>, AppConfigError> {
    let toml::Value::Array(items) = v else {
        return Err(section_error("web.ports", "ожидается список портов"));
    };
    if items.is_empty() {
        return Err(section_error(
            "web.ports",
            "пустой список: web_fetch не открыл бы ни одной страницы",
        ));
    }
    items
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let n = int_at(&format!("web.ports[{i}]"), p, 1, i64::from(u16::MAX))?;
            Ok(u16::try_from(n).unwrap_or(u16::MAX))
        })
        .collect()
}

/// `[web.search]` (D111, D122, D125): `provider` — только `searxng`; `url` — `http` или
/// `https` с хостом, без учётных данных. Без `url` — поиска нет и предупреждение.
fn parse_search(v: toml::Value) -> Result<Option<SearchSettings>, AppConfigError> {
    let mut provider = SEARXNG.to_owned();
    let mut url = None;
    for (key, value) in table_at("web.search", v)? {
        match key.as_str() {
            "provider" => {
                let Some(p) = value.as_str() else {
                    return Err(section_error("web.search.provider", "ожидается строка"));
                };
                if p != SEARXNG {
                    let shown = shown_key(p).unwrap_or("(имя не показано)");
                    return Err(section_error(
                        "web.search.provider",
                        format!("unknown search provider {shown}; поддерживается только {SEARXNG}"),
                    ));
                }
                provider = p.to_owned();
            }
            "url" => url = Some(parse_search_url(&value)?),
            other => return Err(unknown_key("web.search", other, "provider или url")),
        }
    }
    let Some(url) = url else {
        tracing::warn!(
            key = "web.search.url",
            "[web.search] url is required: поиск не настроен, web_search не регистрируется"
        );
        return Ok(None);
    };
    Ok(Some(SearchSettings { provider, url }))
}

/// Адрес экземпляра поставщика: значение в текст ошибки не попадает (в нём могут быть
/// учётные данные).
fn parse_search_url(v: &toml::Value) -> Result<url::Url, AppConfigError> {
    const AT: &str = "web.search.url";
    let Some(s) = v.as_str() else {
        return Err(section_error(AT, "ожидается строка с адресом"));
    };
    let u = url::Url::parse(s.trim())
        .map_err(|_| section_error(AT, "адрес не разбирается; пример: http://127.0.0.1:8888"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(section_error(AT, "только http или https"));
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err(section_error(AT, "в адресе нет хоста"));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(section_error(
            AT,
            "учётные данные в адресе не допускаются (у SearXNG ключа нет)",
        ));
    }
    Ok(u)
}

/// Раздел `[skills]` (D114, D122): `enabled`, `dirs` (абсолютные пути, `~` и переменные
/// не раскрываются), `max_entries` (1–[`MAX_SKILL_ENTRIES`]).
fn parse_skills(v: toml::Value) -> Result<SkillsSettings, AppConfigError> {
    let mut out = SkillsSettings::default();
    for (key, value) in table_at("skills", v)? {
        let at = format!("skills.{key}");
        match key.as_str() {
            "enabled" => out.enabled = bool_at(&at, &value)?,
            "dirs" => out.dirs = parse_skill_dirs(value)?,
            "max_entries" => {
                let n = int_at(&at, &value, 1, MAX_SKILL_ENTRIES as i64)?;
                out.max_entries = usize::try_from(n).unwrap_or(MAX_SKILL_ENTRIES);
            }
            other => {
                return Err(unknown_key(
                    "skills",
                    other,
                    "enabled, dirs или max_entries",
                ));
            }
        }
    }
    Ok(out)
}

/// `[skills] dirs`: абсолютные пути каталогов `extra` (как пути грантов, D78).
fn parse_skill_dirs(v: toml::Value) -> Result<Vec<PathBuf>, AppConfigError> {
    let toml::Value::Array(items) = v else {
        return Err(section_error("skills.dirs", "ожидается список путей"));
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let at = format!("skills.dirs[{i}]");
            let Some(s) = item.as_str() else {
                return Err(section_error(at, "ожидается строка с путём"));
            };
            if s.trim().is_empty() {
                return Err(section_error(at, "пустой путь"));
            }
            if s.starts_with(['~', '$', '%']) {
                return Err(section_error(
                    at,
                    "`~` и переменные окружения не раскрываются: укажите полный путь",
                ));
            }
            let p = PathBuf::from(s);
            if !p.is_absolute() {
                return Err(section_error(at, "нужен абсолютный путь"));
            }
            Ok(p)
        })
        .collect()
}

/// Поля `[policy]`, `[[policy.rules]]` и `[[policy.grants]]`: только эти имена ошибка
/// называет (белый список, как у [`ConfigParseError`]: ключ TOML может оказаться секретом).
const POLICY_FIELDS: &[&str] = &[
    "allow_bypass",
    "rules",
    "grants",
    "action",
    "tool",
    "path",
    "command",
    "domain",
    "mode",
    "reason",
];

fn policy_error(at: impl Into<String>, reason: impl Into<String>) -> AppConfigError {
    AppConfigError::Policy {
        at: at.into(),
        reason: reason.into(),
    }
}

/// Разбор таблицы `[policy]` с путём поля в ошибке (`policy.rules[1]`, `policy.grants[0]`).
fn parse_policy(v: toml::Value) -> Result<PolicyConfig, AppConfigError> {
    let toml::Value::Table(t) = v else {
        return Err(policy_error("policy", "ожидается таблица [policy]"));
    };
    let mut out = PolicyConfig::default();
    for (key, value) in t {
        match key.as_str() {
            "allow_bypass" => match value {
                toml::Value::Boolean(b) => out.allow_bypass = b,
                _ => {
                    return Err(policy_error(
                        "policy.allow_bypass",
                        "ожидается true или false",
                    ));
                }
            },
            "rules" => out.rules = parse_items::<Rule>("rules", value)?,
            "grants" => out.grants = parse_items::<GrantConfig>("grants", value)?,
            other => {
                return Err(policy_error(
                    "policy",
                    format!(
                        "{}; ожидается allow_bypass, [[policy.rules]] или [[policy.grants]]",
                        unknown_field(other)
                    ),
                ));
            }
        }
    }
    Ok(out)
}

/// Массив таблиц `[[policy.<name>]]`: каждая разбирается отдельно, ошибка — с индексом.
fn parse_items<T: serde::de::DeserializeOwned>(
    name: &str,
    value: toml::Value,
) -> Result<Vec<T>, AppConfigError> {
    let toml::Value::Array(items) = value else {
        return Err(policy_error(
            format!("policy.{name}"),
            format!("ожидается массив таблиц [[policy.{name}]]"),
        ));
    };
    items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            item.try_into::<T>()
                .map_err(|e| policy_error(format!("policy.{name}[{i}]"), de_reason(e.message())))
        })
        .collect()
}

/// «Неизвестное поле» с именем из [`POLICY_FIELDS`] или без имени.
fn unknown_field(name: &str) -> String {
    match POLICY_FIELDS.iter().find(|f| **f == name) {
        Some(f) => format!("неизвестное поле `{f}`"),
        None => "неизвестное поле".to_owned(),
    }
}

/// Причина ошибки разбора правила или гранта по началу сообщения serde. Значения
/// пользователя не выводятся, имена полей — только из [`POLICY_FIELDS`].
fn de_reason(message: &str) -> String {
    let m = message.trim_start();
    let name = |rest: &str| rest.split('`').next().unwrap_or("").to_owned();
    if let Some(rest) = m.strip_prefix("unknown field `") {
        return format!(
            "{}; поля правила: action, tool, path, command, domain; поля гранта: path, mode, reason",
            unknown_field(&name(rest))
        );
    }
    if let Some(rest) = m.strip_prefix("missing field `") {
        let n = name(rest);
        return match POLICY_FIELDS.iter().find(|f| **f == n) {
            Some(f) => format!("нет обязательного поля `{f}`"),
            None => "нет обязательного поля".to_owned(),
        };
    }
    if m.starts_with("unknown variant") {
        return "неизвестное значение: action — allow, ask или deny; mode — ro или rw".to_owned();
    }
    if m.starts_with("duplicate") {
        return "поле задано дважды".to_owned();
    }
    if m.starts_with("invalid type") {
        return "неверный тип значения".to_owned();
    }
    "ошибка разбора".to_owned()
}

/// Проверка значений: `[models]` (ссылки, D24, D25), `[resources]`, `[budgets.run]`,
/// `[context]` и `[policy]`.
pub fn validate(cfg: &AgentConfig) -> Result<(), AppConfigError> {
    cfg.resources
        .validate()
        .map_err(AppConfigError::Resources)?;
    validate_budget(&cfg.budgets.run)?;
    cfg.context.validate().map_err(AppConfigError::Context)?;
    validate_policy(&cfg.policy)?;
    if let Some(m) = &cfg.models {
        // Предупреждения (D34) печатает `Gateway::start` и показывают `models`, `doctor`.
        m.validate().map_err(AppConfigError::Models)?;
    }
    Ok(())
}

/// Пределы `[budgets.run]`: дробный `usd` — конечный и не отрицательный (NaN молча
/// снял бы предел: `x > NaN` всегда ложно). Нулевой предел допустим — «запрещено
/// сразу». Целые поля отрицательными быть не могут: их отвергает разбор.
fn validate_budget(b: &Budget) -> Result<(), AppConfigError> {
    let Some(usd) = b.usd else {
        return Ok(());
    };
    let reason = if !usd.is_finite() {
        "должно быть конечным числом"
    } else if usd < 0.0 {
        "не может быть отрицательным"
    } else {
        return Ok(());
    };
    Err(AppConfigError::Budget {
        field: "usd",
        reason,
    })
}

/// `[policy]`: правила собираются в снимок (поле, шаблон, `..`), путь гранта — абсолютный
/// (`~` и переменные не раскрываются, относительный путь неоднозначен).
fn validate_policy(p: &PolicyConfig) -> Result<(), AppConfigError> {
    PolicySnapshot::build(p, None).map_err(AppConfigError::PolicyRules)?;
    for (i, g) in p.grants.iter().enumerate() {
        let at = format!("policy.grants[{i}].path");
        let shown = g.path.to_string_lossy();
        if shown.trim().is_empty() {
            return Err(policy_error(at, "пустой путь"));
        }
        if shown.starts_with(['~', '$', '%']) {
            return Err(policy_error(
                at,
                "`~` и переменные окружения не раскрываются: укажите полный путь",
            ));
        }
        if !g.path.is_absolute() {
            return Err(policy_error(at, "нужен абсолютный путь"));
        }
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
        // Опечатка раздела молча сняла бы все пределы: `deny_unknown_fields` у `AgentConfig`.
        const SRC: &str = "[budget.run]\nsteps = 2\n";
        let e = parse_config(SRC).unwrap_err();
        assert!(matches!(e, AppConfigError::Parse(_)), "{e}");
        let shown = format!("{e} {e:?}");
        assert!(!shown.contains("steps = 2"), "{shown}");
        assert!(matches!(
            parse_config("[nosuch]\nx = 1\n"),
            Err(AppConfigError::Parse(_))
        ));
    }

    #[test]
    fn budget_values_are_validated() {
        for (src, ok) in [
            ("[budgets.run]\nusd = nan\n", false),
            ("[budgets.run]\nusd = inf\n", false),
            ("[budgets.run]\nusd = -0.5\n", false),
            ("[budgets.run]\nusd = 0.0\nsteps = 0\ntokens = 0\n", true),
            ("[budgets.run]\nusd = 1.5\n", true),
        ] {
            let r = parse_config(src).and_then(|c| validate(&c));
            assert_eq!(r.is_ok(), ok, "{src}: {r:?}");
            if let Err(e) = r {
                assert!(
                    matches!(e, AppConfigError::Budget { field: "usd", .. }),
                    "{e}"
                );
                assert!(e.to_string().contains("budgets.run.usd"), "{e}");
            }
        }
        // Отрицательный целый предел отвергает разбор.
        assert!(matches!(
            parse_config("[budgets.run]\nsteps = -1\n"),
            Err(AppConfigError::Parse(_))
        ));
    }

    #[test]
    fn budget_field_named_in_parse_error() {
        // Имя поля бюджета — в белом списке: ошибка разбора называет его (здесь предел
        // стоит в `[budgets]`, а не в `[budgets.run]`).
        let e = parse_config("[budgets]\nsteps = 2\n").unwrap_err();
        let AppConfigError::Parse(p) = &e else {
            panic!("{e}");
        };
        assert_eq!(p.field, Some("steps"), "{e}");
    }

    /// Разбор и проверка значений; ошибка — текстом.
    fn policy_err(src: &str) -> String {
        match parse_config(src).and_then(|c| validate(&c)) {
            Ok(()) => panic!("ожидалась ошибка: {src}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn policy_section_parses() {
        let grant_path = if cfg!(windows) { "D:/data" } else { "/data" };
        let src = format!(
            "[policy]\nallow_bypass = true\n\n[[policy.rules]]\naction = \"deny\"\ncommand = \"git push*\"\n\n\
             [[policy.rules]]\naction = \"allow\"\ntool = \"bash\"\ncommand = \"cargo test\"\n\n\
             [[policy.grants]]\npath = \"{grant_path}\"\nmode = \"rw\"\nreason = \"датасет\"\n\n\
             [[policy.grants]]\npath = \"{grant_path}/ro\"\n"
        );
        let cfg = parse_config(&src).unwrap();
        validate(&cfg).unwrap();
        let p = &cfg.policy;
        assert!(p.allow_bypass);
        assert_eq!(p.rules.len(), 2);
        assert_eq!(p.rules[0].action, za_agent::RuleAction::Deny);
        assert_eq!(p.rules[1].tool.as_deref(), Some("bash"));
        assert_eq!(p.grants[0].mode, za_agent::GrantMode::Rw);
        assert_eq!(p.grants[0].reason, "датасет");
        assert_eq!(p.grants[1].mode, za_agent::GrantMode::Ro);
        // Без раздела — пустая политика, bypass выключен.
        assert_eq!(parse_config("").unwrap().policy, PolicyConfig::default());
    }

    #[test]
    fn policy_unknown_rule_field_names_path() {
        let e = policy_err("[[policy.rules]]\naction = \"deny\"\ntol = \"bash\"\n");
        assert!(e.contains("policy.rules[0]"), "{e}");
        assert!(e.contains("неизвестное поле"), "{e}");
        // Второе правило — свой индекс; известное имя поля называется.
        let e = policy_err(
            "[[policy.rules]]\naction = \"deny\"\ntool = \"x\"\n[[policy.rules]]\naction = \"ask\"\nmode = \"rw\"\n",
        );
        assert!(
            e.contains("policy.rules[1]: неизвестное поле `mode`"),
            "{e}"
        );
        // Незнакомое имя (возможно, секрет) не выводится.
        let key = ["sk", "policytest", "0123456789"].join("-");
        let e = policy_err(&format!(
            "[[policy.rules]]\naction = \"deny\"\n\"{key}\" = 1\n"
        ));
        assert!(!e.contains(&key), "{e}");
        let e = policy_err(&format!("[policy]\n\"{key}\" = 1\n"));
        assert!(e.contains("policy: неизвестное поле;"), "{e}");
        assert!(!e.contains(&key), "{e}");
    }

    #[test]
    fn policy_values_are_checked() {
        for (src, want) in [
            (
                "[[policy.rules]]\naction = \"permit\"\ntool = \"x\"\n",
                "policy.rules[0]: неизвестное значение",
            ),
            (
                "[[policy.rules]]\ntool = \"x\"\n",
                "policy.rules[0]: нет обязательного поля `action`",
            ),
            (
                "[[policy.rules]]\naction = \"deny\"\n",
                "policy.rules[0]: нужно хотя бы одно поле tool, path, command или domain",
            ),
            (
                "[[policy.rules]]\naction = \"deny\"\ncommand = \"[\"\n",
                "policy.rules[0].command: неверный шаблон",
            ),
            (
                "[[policy.rules]]\naction = \"allow\"\npath = \"/a/../**\"\n",
                "policy.rules[0].path: неверный шаблон",
            ),
            (
                "[[policy.rules]]\naction = \"deny\"\ntool = 1\n",
                "policy.rules[0]: неверный тип значения",
            ),
            (
                "[policy]\nallow_bypass = \"yes\"\n",
                "policy.allow_bypass: ожидается true или false",
            ),
            ("[policy]\nrules = 1\n", "policy.rules: ожидается массив"),
            (
                "[[policy.grants]]\npath = \"data\"\n",
                "policy.grants[0].path: нужен абсолютный путь",
            ),
            (
                "[[policy.grants]]\npath = \"~/data\"\n",
                "policy.grants[0].path: `~`",
            ),
            (
                "[[policy.grants]]\npath = \"\"\n",
                "policy.grants[0].path: пустой путь",
            ),
            (
                "[[policy.grants]]\nmode = \"rw\"\n",
                "policy.grants[0]: нет обязательного поля `path`",
            ),
            (
                "[[policy.grants]]\npath = \"/x\"\nmode = \"wr\"\n",
                "policy.grants[0]: неизвестное значение",
            ),
        ] {
            let e = policy_err(src);
            assert!(e.contains(want), "{src}: {e}");
            assert!(e.starts_with(CONFIG_FILE), "{e}");
        }
    }

    #[test]
    fn unknown_budgets_field_is_error() {
        // `[budgets]` известен с задачи 13: незнакомый ключ в нём — тоже ошибка разбора.
        assert!(matches!(
            parse_config("[budgets]\nx = 1\n"),
            Err(AppConfigError::Parse(_))
        ));
    }
}
