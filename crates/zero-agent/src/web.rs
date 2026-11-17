//! Веб в бинарнике (R1-C2 D105, D107, D111, D112, D122, D125, D126): настройки шлюза из
//! глобальной конфигурации, решение «веб выключен» (глобально или проектом), набор
//! веб-инструментов run, проверка поставщика поиска в `doctor` и строки `web.request` для
//! stderr `run`.
//!
//! **Адрес поставщика поиска** берётся только из `[web.search] url` глобальной
//! конфигурации ([`web_config`]): у проектной конфигурации такого поля нет вовсе
//! (`ProjectConfig` — только `[policy]` и `[web] enabled = false`), поэтому чужой
//! репозиторий не может открыть исключение фильтра адресов D126 на свой адрес.
//!
//! **Шлюз** — один объект на процесс (`Arc<WebGateway>`, D107): его строит `run` и держат
//! оба инструмента. `web_search` регистрируется, только если у шлюза есть адрес поставщика
//! ([`WebGateway::search_endpoint`], D111). Тексты инструментов (`Tool::texts`, в том числе
//! `runtime/web.fetch.simplified`) входят в хэш промптов run при регистрации: их собирает
//! `freeze_for` по инструментам набора (D94, D131).

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use za_core::Tool;
use za_core::extract::Extractor;
use za_core::net::normalize_host;
use za_protocol::net_outcome;
use za_web::{
    FetchError, PROVIDER_TIMEOUT, SearchProvider, Searxng, WebConfig, WebGateway, web_fetch_tool,
    web_search_tool,
};

use crate::config::{AgentConfig, ProjectConfig, WebSettings};
use crate::doctor::{Check, display_url};

/// Веб выключен глобальной конфигурацией (`[web] enabled = false`): значение
/// `policy.loaded.web_disabled`.
pub const DISABLED_BY_CONFIG: &str = "config";
/// Веб выключен проектом (`.zero-agent/config.toml` `[web] enabled = false`, D105).
pub const DISABLED_BY_PROJECT: &str = "project";

/// Кто выключил веб: глобальная конфигурация ([`DISABLED_BY_CONFIG`]) или доверенный проект
/// ([`DISABLED_BY_PROJECT`]); `None` — веб включён. Проект только ужесточает: его
/// `enabled = true` ничего не включает (`ProjectConfig::web_enabled` — `None`).
pub fn web_disabled(cfg: &AgentConfig, project: Option<&ProjectConfig>) -> Option<&'static str> {
    if !cfg.web.enabled {
        Some(DISABLED_BY_CONFIG)
    } else if project.is_some_and(|p| p.web_enabled == Some(false)) {
        Some(DISABLED_BY_PROJECT)
    } else {
        None
    }
}

/// Настройки шлюза из `[web]` глобальной конфигурации; `search_endpoint` — только
/// `[web.search] url` (D122, D126).
pub fn web_config(s: &WebSettings) -> WebConfig {
    WebConfig {
        robots: s.robots,
        max_bytes: s.max_bytes,
        requests_per_minute: s.requests_per_minute,
        ports: s.ports.clone(),
        search_endpoint: s.search.as_ref().map(|s| s.url.clone()),
        ..WebConfig::default()
    }
}

/// Нормализованный хост поставщика поиска (`policy.loaded.search_host`, D105): правила по
/// домену к поиску не применяются, поэтому хост виден в аудите.
pub fn search_host(s: &WebSettings) -> Option<String> {
    s.search
        .as_ref()
        .and_then(|s| s.url.host_str().and_then(normalize_host))
}

/// Веб-инструменты run (D110, D111): `web_fetch` всегда, `web_search` — только при адресе
/// поставщика у шлюза (`Searxng` с этим адресом, D125).
pub fn web_tools(gw: &Arc<WebGateway>, extractor: Arc<dyn Extractor>) -> Vec<Arc<dyn Tool>> {
    let mut tools = vec![web_fetch_tool(Arc::clone(gw), extractor)];
    if let Some(url) = gw.search_endpoint() {
        tools.push(web_search_tool(
            Arc::clone(gw),
            Arc::new(Searxng::new(url.clone())),
        ));
    }
    tools
}

/// Запрос пробы поставщика в `doctor`.
const PROBE_QUERY: &str = "zero-agent doctor";

/// Подсказка на 403 SearXNG (перенос задачи 9): формат `json` не включён.
pub const HINT_403: &str =
    "у экземпляра не включён формат json: добавьте json в search.formats файла settings.yml";
/// Подсказка на 429 SearXNG (fix-9s, m1): с `server.limiter` API `format=json` пропускает
/// не больше 4 запросов в час с адреса, заголовки этого не меняют. Путей два.
pub const HINT_429: &str = "у экземпляра включён server.limiter: с ним API format=json пропускает не больше 4 запросов в час с адреса, и заголовки этого не меняют; выключите server.limiter в settings.yml или внесите адрес агента в botdetection.ip_lists.pass_ip файла limiter.toml";

/// Проверка поиска для `doctor` (D111, D122): адреса нет — «поиск не настроен»; веб
/// выключен — так и сказано; иначе пробный запрос к экземпляру через
/// [`WebGateway::get_provider`] (то же исключение D126, что у `web_search`).
pub async fn check_search(cfg: &WebSettings) -> Check {
    let Some(search) = &cfg.search else {
        return Check::Warn(
            "поиск не настроен: [web.search] url (нужен свой экземпляр SearXNG); web_search в run не регистрируется".to_owned(),
        );
    };
    let shown = display_url(search.url.as_str());
    let head = format!("поиск: {} {shown}", search.provider);
    if !cfg.enabled {
        return Check::Ok(format!(
            "{head}, не проверялся: веб выключен ([web] enabled = false)"
        ));
    }
    let gw = match WebGateway::new(web_config(cfg)) {
        Ok(g) => g,
        Err(e) => return Check::Warn(format!("{head}, шлюз не построен: {e}")),
    };
    probe(&gw, &head).await
}

/// Пробный запрос к поставщику шлюза `gw`; `head` — начало строки отчёта.
async fn probe(gw: &WebGateway, head: &str) -> Check {
    let Some(url) = gw.search_endpoint() else {
        return Check::Warn(format!("{head}, не настроен"));
    };
    let provider = Searxng::new(url.clone());
    let req = provider.request(PROBE_QUERY, 1);
    let deadline = tokio::time::Instant::now() + PROVIDER_TIMEOUT + Duration::from_secs(1);
    let fetched = gw.get_provider(&req, deadline).await;
    match fetched.result {
        Ok(body) if (200..300).contains(&body.status) => match provider.parse(&body.bytes, 5) {
            Ok(hits) => Check::Ok(format!("{head}, отвечает (результатов: {})", hits.len())),
            Err(e) => Check::Warn(format!("{head}, отвечает, но ответ не разобран: {e}")),
        },
        Ok(body) => {
            let hint = match body.status {
                403 => format!(": {HINT_403}"),
                429 => format!(": {HINT_429}"),
                _ => String::new(),
            };
            Check::Warn(format!("{head}, ошибка: HTTP {}{hint}", body.status))
        }
        Err(FetchError::Timeout) => Check::Warn(format!(
            "{head}, не отвечает: нет ответа за {} с",
            PROVIDER_TIMEOUT.as_secs()
        )),
        Err(FetchError::Network(r)) => Check::Warn(format!("{head}, не отвечает: {r}")),
        Err(e) => Check::Warn(format!("{head}, не отвечает: {}", e.text())),
    }
}

/// Проверка `[web]` для `doctor` (D122): включён ли веб и его пределы.
pub fn check_web(cfg: &WebSettings) -> Check {
    if !cfg.enabled {
        return Check::Ok(
            "веб выключен ([web] enabled = false): web_fetch и web_search не регистрируются"
                .to_owned(),
        );
    }
    let ports: Vec<String> = cfg.ports.iter().map(u16::to_string).collect();
    Check::Ok(format!(
        "веб включён: robots.txt {}, ответ до {}, {} запросов в минуту, порты {}",
        if cfg.robots {
            "соблюдается"
        } else {
            "не проверяется"
        },
        size_text(cfg.max_bytes),
        cfg.requests_per_minute,
        ports.join(", ")
    ))
}

/// Размер для человека: байты, КиБ, МиБ.
pub fn size_text(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if n >= MIB {
        format!("{:.1} МиБ", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.1} КиБ", n as f64 / KIB as f64)
    } else {
        format!("{n} байт")
    }
}

/// Строка stderr `run` по событию `web.request` (D112): `zero-agent: веб: GET <url> →
/// <код>, <размер>`, отказ — `zero-agent: веб: отказ <url>: <причина>`, сбой —
/// `zero-agent: веб: ошибка <url>: <причина>`. Адрес в событии уже замаскирован слотом.
pub fn request_line(p: &Value) -> String {
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default();
    let url = s("url");
    let reason = s("reason");
    match s("outcome") {
        o if o == net_outcome::BLOCKED => format!("zero-agent: веб: отказ {url}: {reason}"),
        o if o == net_outcome::OK => {
            let status = p
                .get("status")
                .and_then(Value::as_u64)
                .map_or_else(|| "?".to_owned(), |c| c.to_string());
            let bytes = p.get("bytes").and_then(Value::as_u64).unwrap_or(0);
            format!(
                "zero-agent: веб: {} {url} → {status}, {}",
                if s("method").is_empty() {
                    "GET"
                } else {
                    s("method")
                },
                size_text(bytes)
            )
        }
        _ => format!("zero-agent: веб: ошибка {url}: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_lines() {
        assert_eq!(
            request_line(
                &json!({"method": "GET", "url": "https://a.test/", "status": 200,
                "bytes": 2048, "outcome": "ok"})
            ),
            "zero-agent: веб: GET https://a.test/ → 200, 2.0 КиБ"
        );
        assert_eq!(
            request_line(
                &json!({"method": "GET", "url": "http://127.0.0.1/", "bytes": 0,
                "outcome": "blocked", "reason": "local or private address"})
            ),
            "zero-agent: веб: отказ http://127.0.0.1/: local or private address"
        );
        assert_eq!(
            request_line(&json!({"url": "https://b.test/", "outcome": "error",
                "reason": "timeout"})),
            "zero-agent: веб: ошибка https://b.test/: timeout"
        );
    }

    #[test]
    fn sizes() {
        assert_eq!(size_text(10), "10 байт");
        assert_eq!(size_text(5 * 1024 * 1024), "5.0 МиБ");
    }

    #[test]
    fn hints_name_both_paths() {
        assert!(HINT_429.contains("server.limiter") && HINT_429.contains("pass_ip"));
        assert!(HINT_429.contains("4 запросов в час"));
        assert!(HINT_403.contains("search.formats"));
    }
}
