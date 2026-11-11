//! Конфигурация агента: `ZA_HOME/config.toml`, разделы `[models]`, `[resources]` (D23),
//! `[budgets.run]` (бюджет run по умолчанию, R1-B1 D19), `[context]` (пороги контекста,
//! R1-B2 D65) и `[policy]` с `[[policy.rules]]` и `[[policy.grants]]` (политика, R1-C1
//! D78, D80).
//!
//! `[policy]` разбирается вторым проходом по уже прочитанной таблице: ошибка называет
//! путь поля (`policy.rules[0]`), а не только строку.
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
    /// `models` — запасной эндпоинт `local` из `LLAMA_URL`, а не раздел файла.
    pub llama_url_fallback: bool,
}

/// Файл как его читает `toml`: `[policy]` — таблицей, её разбирает [`parse_policy`].
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
        llama_url_fallback: false,
    })
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
            "{}; поля правила: action, tool, path, command; поля гранта: path, mode, reason",
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
                "policy.rules[0]: нужно хотя бы одно поле tool, path или command",
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
