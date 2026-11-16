//! Инструмент `web_search` (R1-C2 D111, D123, D125, D126) против поддельного SearXNG на
//! `127.0.0.1:0`: адрес экземпляра задан как `WebConfig::search_endpoint` (`[web.search] url`
//! глобальной конфигурации). Настоящий интернет не используется.
//!
//! Исключение фильтра адресов (D126) действует только для происхождения этого адреса и только
//! через `WebGateway::get_provider`: `get` (`web_fetch`) на тот же адрес — отказ
//! `local or private address`, канал сервера пуст.

mod support;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use support::extractor::StubExtractor;
use support::{
    Server, drip, redirect, scaled, serve, serve_with, text, typed, with_header, within,
};
use tokio::time::Instant;
use url::Url;
use za_core::text::escaped_len;
use za_core::{
    Approval, DIGEST_RESERVE, RiskClass, Tool, ToolCx, ToolKind, ToolOutput, call_deadline,
};
use za_protocol::{EffectClass, EffectKey, RunId, RunRole, Tier, net_outcome};
use za_web::search::{SearchHit, SearchProvider, Searxng};
use za_web::{FetchError, Fetched, WebConfig, WebGateway, web_fetch_tool, web_search_tool};

const LONG: Duration = Duration::from_secs(20);
/// Отказ `get_provider` и резолвера поставщика (D126).
const NOT_PROVIDER: &str = "not the configured search provider";

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

fn deadline() -> Instant {
    Instant::now() + scaled(LONG)
}

/// Настройки шлюза с экземпляром поставщика `endpoint`; интервал хоста снят: тесты делают
/// несколько запросов к одному экземпляру подряд.
fn config(endpoint: &str) -> WebConfig {
    WebConfig {
        search_endpoint: Some(url(endpoint)),
        host_interval: Duration::ZERO,
        ..WebConfig::default()
    }
}

/// Адрес экземпляра на поддельном сервере — IP-литерал, без `with_test_resolve`.
fn endpoint(s: &Server) -> String {
    format!("http://127.0.0.1:{}/", s.port())
}

/// Инструмент поверх шлюза с экземпляром `endpoint`.
fn tool(gw: WebGateway, endpoint: &str) -> Arc<dyn Tool> {
    web_search_tool(Arc::new(gw), Arc::new(Searxng::new(url(endpoint))))
}

fn searx(endpoint: &str) -> Arc<dyn Tool> {
    tool(WebGateway::new(config(endpoint)).unwrap(), endpoint)
}

/// Инструмент без экземпляра: вызовы отвергаются до соединения.
fn offline() -> Arc<dyn Tool> {
    tool(
        WebGateway::new(WebConfig::default()).unwrap(),
        "http://127.0.0.1:8888/",
    )
}

fn cx() -> ToolCx {
    cx_with(call_deadline(std::time::Instant::now(), scaled(LONG)), None)
}

/// Контекст вызова с дедлайном `deadline` и пределом результата слота `budget`.
fn cx_with(deadline: std::time::Instant, budget: Option<usize>) -> ToolCx {
    let run = RunId::new();
    ToolCx {
        run,
        project_dir: std::path::PathBuf::from("."),
        call_id: "c1".into(),
        effect_key: EffectKey::derive(run, 0, 0),
        paths: Vec::new(),
        deadline: Some(deadline),
        output_budget: budget,
    }
}

async fn search(tool: &Arc<dyn Tool>, args: Value) -> ToolOutput {
    within(LONG * 2, "web_search", tool.call(args, cx())).await
}

/// Заголовки, без которых `limiter` поддельного экземпляра отвечает 429 (M2 ревью задачи 9).
const PROVIDER_HEADERS: [(&str, &str); 3] = [
    ("accept", "application/json"),
    ("accept-encoding", "identity"),
    ("accept-language", "en"),
];

async fn provider_get(gw: &WebGateway, u: &str) -> Fetched {
    within(LONG * 2, u, gw.get_provider(&url(u), deadline())).await
}

/// Ответ SearXNG `format=json`: `results[].{title, url, content}` и посторонние поля.
fn results(hits: &[(&str, &str, &str)]) -> Vec<u8> {
    let results: Vec<Value> = hits
        .iter()
        .map(|(t, u, c)| json!({"title": t, "url": u, "content": c, "engine": "x", "score": 1.0}))
        .collect();
    serde_json::to_vec(&json!({
        "query": "q",
        "number_of_results": 0,
        "results": results,
        "answers": [],
        "suggestions": []
    }))
    .unwrap()
}

/// Экземпляр, отвечающий `body` на `/search`, 404 — на прочие пути.
async fn instance(body: Vec<u8>) -> Server {
    serve(move |p| {
        if p.starts_with("/search?") {
            typed(200, "application/json", body.clone())
        } else {
            text(404, "")
        }
    })
    .await
}

fn many(n: usize) -> Vec<u8> {
    let hits: Vec<(String, String, String)> = (1..=n)
        .map(|i| {
            (
                format!("T{i}"),
                format!("https://e{i}.example/"),
                format!("s{i}"),
            )
        })
        .collect();
    let refs: Vec<(&str, &str, &str)> = hits
        .iter()
        .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str()))
        .collect();
    results(&refs)
}

#[tokio::test]
async fn searxng_results_rendered() {
    let mut s = instance(results(&[
        ("Tokio", "https://tokio.rs/", "An async runtime"),
        (
            "Rust",
            "https://www.rust-lang.org/",
            "A language empowering everyone",
        ),
    ]))
    .await;
    let ep = endpoint(&s);
    let out = search(&searx(&ep), json!({"query": "rust tokio"})).await;

    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        "1. Tokio\n   https://tokio.rs/\n   An async runtime\n\
         2. Rust\n   https://www.rust-lang.org/\n   A language empowering everyone"
    );
    assert_eq!(out.taint.as_deref(), Some("web"));
    // Один запрос, без `robots.txt` (API своего экземпляра, D126).
    assert_eq!(out.requests.len(), 1, "{:?}", out.requests);
    let r = &out.requests[0];
    assert_eq!(r.method, "GET");
    assert_eq!(r.url, format!("{ep}search?q=rust+tokio&format=json"));
    assert_eq!(r.status, Some(200));
    assert_eq!(r.outcome, net_outcome::OK);
    let seen = s.seen();
    let paths: Vec<&str> = seen.iter().map(|x| x.path.as_str()).collect();
    assert_eq!(paths, ["/search?q=rust+tokio&format=json"]);
    // Клиент поставщика — без cookie и сжатия (`Accept-Encoding: identity`), со своими
    // заголовками `Accept` и `Accept-Language` (M2 ревью задачи 9).
    for (name, value) in PROVIDER_HEADERS {
        assert_eq!(seen[0].headers[name], value, "{name}");
    }
    assert!(seen[0].headers.get("cookie").is_none());
}

/// M2 ревью задачи 9: экземпляр с `limiter` отвечает 429 клиенту без `Accept`,
/// `Accept-Encoding` и `Accept-Language`. Клиент поставщика шлёт их всегда; клиент страниц
/// (`web_fetch`) — нет: заголовки — свойство только клиента поставщика.
#[tokio::test]
async fn provider_sends_accept_headers() {
    let body = results(&[("Tokio", "https://tokio.rs/", "An async runtime")]);
    let mut s = serve_with(move |p, h| {
        let ok = PROVIDER_HEADERS
            .iter()
            .all(|(n, v)| h.get(*n).and_then(|x| x.to_str().ok()) == Some(*v));
        match (p.starts_with("/search?"), ok) {
            (true, true) => typed(200, "application/json", body.clone()),
            (true, false) => text(429, "Too Many Requests"),
            _ => text(404, ""),
        }
    })
    .await;
    let out = search(&searx(&endpoint(&s)), json!({"query": "tokio"})).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        "1. Tokio\n   https://tokio.rs/\n   An async runtime"
    );
    assert_eq!(s.paths().len(), 1);

    // Клиент страниц на публичный хост (подмена имени) — без этих заголовков.
    let gw = WebGateway::new(WebConfig {
        robots: false,
        ports: vec![s.port()],
        ..config(&endpoint(&s))
    })
    .unwrap()
    .with_test_resolve(vec![("site.test".into(), s.addr)]);
    let page = format!("http://site.test:{}/search?q=x&format=json", s.port());
    let f = within(LONG * 2, "get", gw.get(&url(&page), deadline())).await;
    assert_eq!(f.result.unwrap().status, 429);
    let seen = s.seen();
    assert_eq!(seen.len(), 1);
    for (name, _) in PROVIDER_HEADERS {
        if name == "accept" {
            assert_eq!(seen[0].headers[name], "*/*");
        } else {
            assert!(seen[0].headers.get(name).is_none(), "{name}");
        }
    }
}

#[tokio::test]
async fn provider_exception_only_for_configured_origin() {
    let mut s = instance(results(&[("A", "https://a.example/", "a")])).await;
    let mut other = serve(|_| text(200, "x")).await;
    let port = s.port();
    let ep = endpoint(&s);
    let gw = Arc::new(WebGateway::new(config(&ep)).unwrap());

    // Настроенное происхождение: исключение фильтра действует (частный адрес).
    let ok = provider_get(&gw, &format!("{ep}search?q=a&format=json")).await;
    assert_eq!(ok.result.unwrap().status, 200);
    assert_eq!(s.paths(), ["/search?q=a&format=json"]);

    // Тот же хост с другим портом или схемой, другое имя того же адреса — отказ до соединения.
    for u in [
        format!("http://127.0.0.1:{}/search?q=a&format=json", other.port()),
        format!("https://127.0.0.1:{port}/search?q=a&format=json"),
        format!("http://localhost:{port}/search?q=a&format=json"),
        format!("http://[::1]:{port}/search?q=a&format=json"),
        format!("http://127.0.0.2:{port}/search?q=a&format=json"),
    ] {
        let f = provider_get(&gw, &u).await;
        assert_eq!(
            f.result.err(),
            Some(FetchError::Blocked(NOT_PROVIDER.into())),
            "{u}"
        );
        assert_eq!(f.requests.len(), 1, "{u}");
        assert_eq!(f.requests[0].outcome, net_outcome::BLOCKED, "{u}");
        assert_eq!(f.requests[0].reason.as_deref(), Some(NOT_PROVIDER), "{u}");
    }

    // `get` (путь `web_fetch`) на адрес поставщика — полный фильтр: исключение привязано к
    // методу, а не к адресу.
    let f = within(LONG * 2, "get", gw.get(&url(&ep), deadline())).await;
    assert_eq!(
        f.result.err(),
        Some(FetchError::Blocked("local or private address".into()))
    );
    assert_eq!(f.requests[0].outcome, net_outcome::BLOCKED);
    let fetch = web_fetch_tool(gw.clone(), StubExtractor::new());
    let out = within(
        LONG * 2,
        "web_fetch",
        fetch.call(json!({"url": format!("{ep}search?q=a&format=json")}), cx()),
    )
    .await;
    assert!(out.is_error);
    assert_eq!(out.content, "blocked: local or private address");
    assert!(s.paths().is_empty(), "к экземпляру не было подключений");
    assert!(
        other.paths().is_empty(),
        "к другому порту не было подключений"
    );
}

#[tokio::test]
async fn provider_resolver_refuses_other_names() {
    let mut s = instance(results(&[("A", "https://a.example/", "a")])).await;
    let port = s.port();
    // Имя в конфигурации — с заглавными и последней точкой: сравнение после `normalize_host`.
    let ep = format!("http://SearX.Test.:{port}/");
    // Оба имени подменены на адрес сервера: отказ даёт сам резолвер поставщика, а не DNS.
    let gw = WebGateway::new(config(&ep))
        .unwrap()
        .with_test_resolve(vec![
            ("searx.test".into(), s.addr),
            ("other.test".into(), s.addr),
        ]);

    // Настроенное имя разрешается в частный адрес (исключение D126).
    let f = provider_get(
        &gw,
        &format!("http://searx.test:{port}/search?q=a&format=json"),
    )
    .await;
    assert_eq!(f.result.unwrap().status, 200);
    assert_eq!(s.paths(), ["/search?q=a&format=json"]);

    // Другое имя клиентом поставщика (крюк без сверки происхождения) — отказ резолвера.
    let u = url(&format!("http://other.test:{port}/search?q=a&format=json"));
    let f = within(
        LONG * 2,
        "provider client",
        gw.provider_client_get_for_tests(&u, deadline()),
    )
    .await;
    assert_eq!(
        f.result.err(),
        Some(FetchError::Blocked(NOT_PROVIDER.into()))
    );
    assert_eq!(f.requests.len(), 1);
    assert_eq!(f.requests[0].outcome, net_outcome::BLOCKED);
    assert!(s.paths().is_empty(), "к other.test не было подключений");

    // `get_provider` на other.test отвергается раньше — сверкой происхождения.
    let f = provider_get(&gw, u.as_str()).await;
    assert_eq!(
        f.result.err(),
        Some(FetchError::Blocked(NOT_PROVIDER.into()))
    );
    assert!(s.paths().is_empty());
}

#[tokio::test]
async fn count_and_query_limits() {
    let mut s = instance(many(30)).await;
    let t = searx(&endpoint(&s));

    // count 50 → 20: обрезка в инструменте, схема его не ограничивает (D111).
    let out = search(&t, json!({"query": "x", "count": 50})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("\n20. T20\n"), "{}", out.content);
    assert!(!out.content.contains("21. "), "{}", out.content);
    // По умолчанию — 8.
    let out = search(&t, json!({"query": "x"})).await;
    assert!(out.content.contains("8. T8\n") && !out.content.contains("9. "));
    let out = search(&t, json!({"query": "x", "count": 3})).await;
    assert!(
        out.content
            .ends_with("3. T3\n   https://e3.example/\n   s3")
    );
    assert_eq!(s.paths().len(), 3);

    // Запрос длиннее 400 символов — ошибка до обращения к экземпляру; длина — в символах.
    let out = search(&t, json!({"query": "я".repeat(401)})).await;
    assert!(out.is_error);
    assert_eq!(out.content, "query is longer than 400 characters");
    assert_eq!(out.taint.as_deref(), Some("web"));
    assert!(out.requests.is_empty());
    assert!(s.paths().is_empty());
    let out = search(&t, json!({"query": "я".repeat(400)})).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(s.paths().len(), 1);

    // Пустой запрос и нет `query` — ошибки аргументов без обращения.
    let out = search(&t, json!({"query": "  "})).await;
    assert_eq!(
        (out.is_error, out.content.as_str()),
        (true, "query is empty")
    );
    let out = search(&t, json!({"count": 2})).await;
    assert!(out.is_error && out.content.starts_with("invalid arguments: "));
    assert!(s.paths().is_empty());
}

/// M4 ревью задачи 9: `count` — любое число JSON. Берётся целая часть, затем обрезка до
/// 1..=20; целое больше `i64::MAX` и `5.0` (целое по JSON Schema) не дают ошибки аргументов.
#[tokio::test]
async fn count_accepts_any_number() {
    let mut s = instance(many(30)).await;
    let t = searx(&endpoint(&s));
    let shown = |content: &str| content.lines().filter(|l| !l.starts_with(' ')).count();
    for (count, want) in [
        (json!(0), 1),
        (json!(-3), 1),
        (json!(0.5), 1),
        (json!(5.0), 5),
        (json!(5.7), 5),
        (json!(20.9), 20),
        (json!(10_000_000_000_000_000_000_u64), 20),
        (json!(1e300), 20),
        (json!(-1e300), 1),
        (Value::Null, 8),
    ] {
        let out = search(&t, json!({"query": "x", "count": count})).await;
        assert!(!out.is_error, "{count}: {}", out.content);
        assert_eq!(shown(&out.content), want, "{count}: {}", out.content);
    }
    assert_eq!(s.paths().len(), 10);
    // Не число — ошибка аргументов.
    let out = search(&t, json!({"query": "x", "count": "5"})).await;
    assert!(out.is_error && out.content.starts_with("invalid arguments: "));
    assert!(s.paths().is_empty());
}

#[test]
fn schema_count_has_no_maximum() {
    let t = offline();
    let p = &t.spec().schema.parameters;
    let count = &p["properties"]["count"];
    assert_eq!(count["type"], "integer");
    assert_eq!(count["minimum"], 1);
    assert!(count.get("maximum").is_none(), "{count}");
    assert_eq!(p["required"], json!(["query"]));
    assert_eq!(p["additionalProperties"], false);
}

#[tokio::test]
async fn provider_redirect_not_followed() {
    // Даже редирект на тот же экземпляр не исполняется (D126).
    let mut s = serve(|p| {
        if p.starts_with("/search?") {
            redirect("/next")
        } else {
            typed(200, "application/json", many(1))
        }
    })
    .await;
    let out = search(&searx(&endpoint(&s)), json!({"query": "x"})).await;
    assert!(out.is_error);
    assert_eq!(out.content, "search provider redirected; not followed");
    assert_eq!(out.taint.as_deref(), Some("web"));
    assert_eq!(out.requests.len(), 1);
    assert_eq!(out.requests[0].status, Some(302));
    assert_eq!(out.requests[0].outcome, net_outcome::ERROR);
    assert_eq!(
        s.paths(),
        ["/search?q=x&format=json"],
        "второй запрос не пришёл"
    );
}

/// M3 ревью задачи 9: любой 3xx — `redirected`, в том числе с `Location`, который не
/// разбирается, и без `Location`.
#[tokio::test]
async fn provider_any_3xx_is_redirect() {
    for (status, location) in [
        (302, Some("http://[")),
        (301, Some("http://exa mple.test/")),
        (300, None),
        (304, None),
        (308, Some("/next")),
    ] {
        let mut s = serve(move |_| match location {
            Some(l) => with_header(text(status, ""), "location", l),
            None => text(status, ""),
        })
        .await;
        let out = search(&searx(&endpoint(&s)), json!({"query": "x"})).await;
        assert_eq!(
            (out.is_error, out.content.as_str()),
            (true, "search provider redirected; not followed"),
            "{status} {location:?}"
        );
        assert_eq!(out.requests.len(), 1, "{status}");
        assert_eq!(out.requests[0].status, Some(status));
        assert_eq!(out.requests[0].outcome, net_outcome::ERROR);
        assert_eq!(s.paths().len(), 1, "{status}: второй запрос не пришёл");
    }
}

#[tokio::test]
async fn provider_unavailable() {
    // Порт, на котором никто не слушает.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let out = search(
        &searx(&format!("http://127.0.0.1:{port}/")),
        json!({"query": "x"}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.content.starts_with("search provider unavailable: "),
        "{}",
        out.content
    );
    assert!(out.content.is_ascii());
    assert_eq!(out.requests.len(), 1);
    assert_eq!(out.requests[0].outcome, net_outcome::ERROR);
}

#[tokio::test]
async fn empty_results() {
    let mut s = instance(results(&[])).await;
    let out = search(&searx(&endpoint(&s)), json!({"query": "nothing"})).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "no results");
    assert_eq!(out.taint.as_deref(), Some("web"));
    assert_eq!(s.paths().len(), 1);
}

/// Ответ SearXNG с пустым `results` и списком `unresponsive_engines` — пары
/// `[движок, причина]`, как их отдаёт `webutils.get_json_response`.
fn unresponsive(hits: &[(&str, &str, &str)], engines: &[(&str, &str)]) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(&results(hits)).unwrap();
    v["unresponsive_engines"] = engines.iter().map(|(e, r)| json!([e, r])).collect();
    serde_json::to_vec(&v).unwrap()
}

/// M1 ревью задачи 9: все движки не ответили (CAPTCHA, тайм-аут, лимит) — SearXNG отвечает
/// 200 с пустым `results`, причины — в `unresponsive_engines`. Модель должна видеть, что
/// это сбой движков, а не «ничего не найдено».
#[tokio::test]
async fn no_results_names_unresponsive_engines() {
    let s = instance(unresponsive(
        &[],
        &[("duckduckgo", "Suspended: CAPTCHA"), ("google", "timeout")],
    ))
    .await;
    let out = search(&searx(&endpoint(&s)), json!({"query": "x"})).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        "no results (search engines unresponsive: duckduckgo (Suspended: CAPTCHA), google (timeout))"
    );
    assert_eq!(out.taint.as_deref(), Some("web"));

    // Есть результаты — частичный сбой движков в выдачу не попадает.
    let s = instance(unresponsive(
        &[("Tokio", "https://tokio.rs/", "rt")],
        &[("google", "timeout")],
    ))
    .await;
    let out = search(&searx(&endpoint(&s)), json!({"query": "x"})).await;
    assert_eq!(out.content, "1. Tokio\n   https://tokio.rs/\n   rt");

    // Список — не больше 10 движков; поля — одной строкой; движок без имени пропускается.
    let names: Vec<String> = (1..=12).map(|i| format!("e{i:02}")).collect();
    let mut engines: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "timeout")).collect();
    engines.insert(0, ("", "x"));
    engines.insert(1, ("multi\nline", "too many\nrequests"));
    let s = instance(unresponsive(&[], &engines)).await;
    let out = search(&searx(&endpoint(&s)), json!({"query": "x"})).await;
    let listed: Vec<String> = (1..=9).map(|i| format!("e{i:02} (timeout)")).collect();
    assert_eq!(
        out.content,
        format!(
            "no results (search engines unresponsive: multi line (too many requests), {}, and 3 more)",
            listed.join(", ")
        )
    );

    // Не список или элементы другого вида — обычное `no results`.
    let mut v: Value = serde_json::from_slice(&results(&[])).unwrap();
    v["unresponsive_engines"] = json!("google");
    let s = instance(serde_json::to_vec(&v).unwrap()).await;
    let out = search(&searx(&endpoint(&s)), json!({"query": "x"})).await;
    assert_eq!(out.content, "no results");
}

/// I1 ревью задачи 9: угловые скобки выдачи доходят до слота как есть (экранирует слот).
#[tokio::test]
async fn angle_brackets_reach_output() {
    let s = instance(results(&[(
        "<details>: The Details disclosure element",
        "https://developer.mozilla.org/",
        "HashMap<K, V> and Vec<T>",
    )]))
    .await;
    let out = search(&searx(&endpoint(&s)), json!({"query": "details"})).await;
    assert_eq!(
        out.content,
        "1. <details>: The Details disclosure element\n   https://developer.mozilla.org/\n   HashMap<K, V> and Vec<T>"
    );
}

/// M5 ревью задачи 9: имя из конфигурации разрешает системный резолвер (без подмены
/// `with_test_resolve`): `http://localhost:<порт>/` — частый случай своего экземпляра.
#[tokio::test]
async fn provider_system_resolver_for_configured_name() {
    let mut s = instance(results(&[("A", "https://a.example/", "a")])).await;
    let ep = format!("http://localhost:{}/", s.port());
    let out = search(&searx(&ep), json!({"query": "a"})).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "1. A\n   https://a.example/\n   a");
    assert_eq!(out.requests.len(), 1);
    assert_eq!(out.requests[0].url, format!("{ep}search?q=a&format=json"));
    assert_eq!(s.paths(), ["/search?q=a&format=json"]);

    // Имя, которое не разрешается (RFC 6761 `.invalid`), — честная ошибка с записью.
    let ep = format!("http://searx.invalid:{}/", s.port());
    let out = search(&searx(&ep), json!({"query": "a"})).await;
    assert!(out.is_error);
    assert!(
        out.content.starts_with("search provider unavailable: "),
        "{}",
        out.content
    );
    assert!(out.content.is_ascii(), "{}", out.content);
    assert_eq!(out.requests.len(), 1);
    assert_eq!(out.requests[0].outcome, net_outcome::ERROR);
    assert!(s.paths().is_empty());
}

/// M5 ревью задачи 9: дедлайн вызова — `search provider unavailable: request timed out`, и
/// запись прерванного запроса (`reason = "timeout"`) не теряется (SEC-045).
#[tokio::test]
async fn provider_timeout_keeps_request() {
    let s = serve(|_| drip(Duration::from_millis(50))).await;
    let t = searx(&endpoint(&s));
    let budget = scaled(Duration::from_secs(1));
    let dl = std::time::Instant::now() + budget;
    // Внешний предел — как тайм-аут слота: вызов возвращается сам, по дедлайну.
    let out = tokio::time::timeout(
        budget + scaled(Duration::from_secs(10)),
        t.call(json!({"query": "x"}), cx_with(dl, None)),
    )
    .await
    .expect("внешний тайм-аут не должен срабатывать: дедлайн соблюдает шлюз");
    assert!(std::time::Instant::now() >= dl);
    assert_eq!(
        (out.is_error, out.content.as_str()),
        (true, "search provider unavailable: request timed out")
    );
    assert_eq!(out.taint.as_deref(), Some("web"));
    assert_eq!(out.requests.len(), 1, "{:?}", out.requests);
    let r = &out.requests[0];
    assert_eq!(r.outcome, net_outcome::ERROR);
    assert_eq!(r.reason.as_deref(), Some("timeout"));
    assert_eq!(r.status, Some(200));
}

/// 20 результатов в худшем случае: адрес почти 2048 байт, заголовок 200 символов,
/// описание 300 символов, угловые скобки (экранирование слота их удлиняет).
fn huge(n: usize) -> Vec<u8> {
    let hits: Vec<(String, String, String)> = (1..=n)
        .map(|i| {
            let path = "p".repeat(2000);
            (
                format!("T{i} {}", "<".repeat(200)),
                format!("https://e{i}.example/{path}"),
                format!("s{i} {}", ">".repeat(300)),
            )
        })
        .collect();
    let refs: Vec<(&str, &str, &str)> = hits
        .iter()
        .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str()))
        .collect();
    results(&refs)
}

/// M6 ревью задачи 9: выдача после экранирования вместе со строками слота укладывается в
/// предел (`output_limit` 32 КиБ или меньший `output_budget` окна): лишние результаты
/// отбрасываются целиком с конца, с пометкой; слот не режет выдачу посередине.
#[tokio::test]
async fn output_fits_limit() {
    let s = instance(huge(20)).await;
    let t = searx(&endpoint(&s));
    let call = |budget: Option<usize>| {
        let t = t.clone();
        async move {
            let dl = call_deadline(std::time::Instant::now(), scaled(LONG));
            within(
                LONG * 2,
                "web_search",
                t.call(json!({"query": "x", "count": 20}), cx_with(dl, budget)),
            )
            .await
        }
    };
    for (budget, limit) in [
        (None, 32 * 1024),
        (Some(64 * 1024), 32 * 1024),
        (Some(9000), 9000),
        // Ни один результат не помещается — только пометка.
        (Some(600), 600),
    ] {
        let out = call(budget).await;
        assert!(!out.is_error, "{}", out.content);
        let len = escaped_len(&out.content);
        assert!(len <= limit - DIGEST_RESERVE, "{budget:?}: {len}");
        let last = out.content.lines().last().unwrap();
        let shown = out
            .content
            .lines()
            .filter(|l| {
                l.split_once(". ")
                    .is_some_and(|(n, _)| n.parse::<u32>().is_ok())
            })
            .count();
        assert_eq!(
            last,
            format!(
                "[{} of 20 results omitted to fit the output limit]",
                20 - shown
            ),
            "{budget:?}"
        );
        // Показанные результаты — целиком: по три строки, номера сплошные.
        assert_eq!(out.content.lines().count(), shown * 3 + 1, "{budget:?}");
        for i in 1..=shown {
            assert!(
                out.content.contains(&format!("\n   https://e{i}.example/")),
                "{budget:?}: {i}"
            );
        }
    }
    // На обычной выдаче пометки нет.
    let s = instance(many(20)).await;
    let out = search(&searx(&endpoint(&s)), json!({"query": "x", "count": 20})).await;
    assert!(!out.content.contains("omitted"), "{}", out.content);
}

#[tokio::test]
async fn provider_http_error_and_bad_body() {
    // Код не 2xx (SearXNG отвечает 403, если формат json не включён в его настройках).
    let mut s = serve(|_| text(403, "Forbidden")).await;
    let out = search(&searx(&endpoint(&s)), json!({"query": "x"})).await;
    assert_eq!(
        (out.is_error, out.content.as_str()),
        (true, "search provider returned HTTP 403 Forbidden")
    );
    assert_eq!(s.paths().len(), 1);

    // Не JSON.
    let s = serve(|_| typed(200, "text/html", b"<html>search</html>".to_vec())).await;
    let out = search(&searx(&endpoint(&s)), json!({"query": "x"})).await;
    assert!(out.is_error);
    assert!(
        out.content
            .starts_with("search provider returned an unreadable response: "),
        "{}",
        out.content
    );

    // Ответ больше 1 МиБ — ошибка без разбора.
    let big = vec![b' '; 1024 * 1024 + 1];
    let s = serve(move |_| typed(200, "application/json", big.clone())).await;
    let out = search(&searx(&endpoint(&s)), json!({"query": "x"})).await;
    assert_eq!(
        (out.is_error, out.content.as_str()),
        (
            true,
            "search provider response is larger than 1048576 bytes"
        )
    );
}

#[tokio::test]
async fn provider_not_configured_refuses() {
    let out = search(&offline(), json!({"query": "x"})).await;
    assert!(out.is_error);
    assert_eq!(out.content, "blocked: not the configured search provider");
    assert_eq!(out.requests.len(), 1);
    assert_eq!(out.requests[0].outcome, net_outcome::BLOCKED);
}

#[test]
fn bad_endpoint_rejected() {
    for ep in [
        "ftp://127.0.0.1/",
        "http://user:pw@127.0.0.1:8888/",
        "file:///c:/searx",
    ] {
        let cfg = WebConfig {
            search_endpoint: Some(url(ep)),
            ..WebConfig::default()
        };
        assert!(WebGateway::new(cfg).is_err(), "{ep}");
    }
    let gw = WebGateway::new(config("http://Localhost.:8888/")).unwrap();
    assert_eq!(
        gw.search_endpoint().map(Url::as_str),
        Some("http://localhost.:8888/")
    );
    assert!(
        WebGateway::new(WebConfig::default())
            .unwrap()
            .search_endpoint()
            .is_none()
    );
}

/// I1 ревью задачи 9 (вместо `snippet_html_stripped` брифа): SearXNG `format=json` отдаёт
/// заголовок и описание простым текстом (экранирование и подсветка у него — только для
/// `format=html`). Теги не снимаются и сущности не раскрываются: поле проходит только
/// `one_line`, дальше — общая маска и экранирование слота. Иначе `<details>`, `Vec<T>` и
/// `HashMap<K, V>` теряли бы главное слово выдачи.
#[test]
fn snippet_plain_text_kept() {
    let p = Searxng::new(url("http://127.0.0.1:8888/"));
    let body = json!({"results": [
        {"title": "<details>: The Details disclosure element - HTML", "url": "https://developer.mozilla.org/", "content": "The <details> HTML element creates a disclosure widget"},
        {"title": "Vec<T> in std::vec", "url": "https://doc.rust-lang.org/", "content": "A contiguous growable array type, written as Vec<T>; see also HashMap<K, V>"},
        {"title": "<b>Tokio</b> &amp; Rust", "url": "https://tokio.rs/", "content": "<strong>Tokio</strong> runtime, a<b>c, 1 &lt; 2"},
        {"title": "no url"},
        {"title": "script", "url": "javascript:alert(1)", "content": "x"},
        {"title": "Multi\nline\r\n2. fake", "url": "https://y.example/", "content": null},
    ]});
    let hits = p
        .parse(serde_json::to_vec(&body).unwrap().as_slice(), 8)
        .unwrap();
    assert_eq!(
        hits,
        [
            SearchHit {
                title: "<details>: The Details disclosure element - HTML".into(),
                url: "https://developer.mozilla.org/".into(),
                snippet: "The <details> HTML element creates a disclosure widget".into(),
            },
            SearchHit {
                title: "Vec<T> in std::vec".into(),
                url: "https://doc.rust-lang.org/".into(),
                snippet:
                    "A contiguous growable array type, written as Vec<T>; see also HashMap<K, V>"
                        .into(),
            },
            SearchHit {
                title: "<b>Tokio</b> &amp; Rust".into(),
                url: "https://tokio.rs/".into(),
                snippet: "<strong>Tokio</strong> runtime, a<b>c, 1 &lt; 2".into(),
            },
            SearchHit {
                title: "Multi line 2. fake".into(),
                url: "https://y.example/".into(),
                snippet: String::new(),
            },
        ]
    );
    // `count` ограничивает число результатов.
    let hits = p.parse(&many(30), 5).unwrap();
    assert_eq!(hits.len(), 5);
    // Описание — до 300 символов вместе с `...`.
    let long = "я".repeat(400);
    let body = serde_json::to_vec(
        &json!({"results": [{"title": "t", "url": "https://z.example/", "content": long}]}),
    )
    .unwrap();
    let hit = &p.parse(&body, 8).unwrap()[0];
    assert_eq!(hit.snippet.chars().count(), 300);
    assert!(hit.snippet.ends_with("..."));
    // Не JSON или без `results` — ошибка.
    assert!(p.parse(b"<html>", 8).is_err());
    assert!(p.parse(b"{\"answers\": []}", 8).is_err());
}

#[test]
fn searxng_request_url() {
    let p = Searxng::new(url("http://127.0.0.1:8888/"));
    assert_eq!(p.name(), "searxng");
    assert_eq!(p.endpoint().as_str(), "http://127.0.0.1:8888/");
    assert_eq!(
        p.request("rust tokio", 8).as_str(),
        "http://127.0.0.1:8888/search?q=rust+tokio&format=json"
    );
    // Экземпляр под путём; запрос и фрагмент базового адреса не переносятся.
    let p = Searxng::new(url("https://h.example/searx/?x=1#f"));
    assert_eq!(
        p.request("a&b=c", 8).as_str(),
        "https://h.example/searx/search?q=a%26b%3Dc&format=json"
    );
    let p = Searxng::new(url("https://h.example/searx"));
    assert_eq!(
        p.request("q", 8).as_str(),
        "https://h.example/searx/search?q=q&format=json"
    );
}

#[test]
fn subjects_have_no_hosts() {
    // Правила по домену к поиску не применяются (D105): хостов нет, хост не «неизвестен».
    let s = offline().subjects(&json!({"query": "x"}));
    assert!(s.network);
    assert!(s.hosts.is_empty());
    assert!(!s.hosts_unknown);
}

#[test]
fn contract_matches_d123() {
    let t = offline();
    let s = t.spec();
    assert_eq!(s.schema.name, "web_search");
    assert_eq!(s.kind, ToolKind::Read);
    assert_eq!(s.class, EffectClass::ReadOnly);
    assert_eq!(s.risk, RiskClass::Network);
    assert_eq!(s.min_tier, Tier::Standard);
    assert_eq!(s.timeout, Duration::from_secs(30));
    assert_eq!(s.output_limit, Some(32 * 1024));
    assert!(s.tainted);
    assert_eq!(s.approval, Approval::Mode);
    assert_eq!(
        s.roles,
        [RunRole::Top, RunRole::Orchestrator, RunRole::Worker]
    );
    assert!(s.schema.description.is_ascii());
    // Тексты для модели — `runtime/web.search.*`, ASCII (D94).
    let texts = t.texts();
    assert!(!texts.is_empty());
    for (name, text) in texts {
        assert!(name.starts_with("runtime/web.search."), "{name}");
        assert!(text.is_ascii(), "{name}: {text}");
    }
}

/// Трейт поставщика — объект (`Arc<dyn SearchProvider>`, D125).
#[test]
fn provider_is_object_safe() {
    let _: Arc<dyn SearchProvider> = Arc::new(Searxng::new(url("http://127.0.0.1:8888/")));
}
