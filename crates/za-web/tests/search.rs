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
use support::{Server, redirect, scaled, serve, text, typed, within};
use tokio::time::Instant;
use url::Url;
use za_core::{Approval, RiskClass, Tool, ToolCx, ToolKind, ToolOutput, call_deadline};
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
    let run = RunId::new();
    ToolCx {
        run,
        project_dir: std::path::PathBuf::from("."),
        call_id: "c1".into(),
        effect_key: EffectKey::derive(run, 0, 0),
        paths: Vec::new(),
        deadline: Some(call_deadline(std::time::Instant::now(), scaled(LONG))),
        output_budget: None,
    }
}

async fn search(tool: &Arc<dyn Tool>, args: Value) -> ToolOutput {
    within(LONG * 2, "web_search", tool.call(args, cx())).await
}

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
    // Клиент поставщика — те же свойства, что у клиента страниц: без cookie и сжатия.
    assert!(seen[0].headers.get("accept-encoding").is_none());
    assert!(seen[0].headers.get("cookie").is_none());
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

    // count меньше 1 и пустой запрос — ошибки аргументов без обращения.
    let out = search(&t, json!({"query": "x", "count": 0})).await;
    assert_eq!(
        (out.is_error, out.content.as_str()),
        (true, "count must be at least 1")
    );
    let out = search(&t, json!({"query": "  "})).await;
    assert_eq!(
        (out.is_error, out.content.as_str()),
        (true, "query is empty")
    );
    let out = search(&t, json!({"count": 2})).await;
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

#[test]
fn snippet_html_stripped() {
    let p = Searxng::new(url("http://127.0.0.1:8888/"));
    let body = json!({"results": [
        {"title": "<b>Tokio</b> &amp; Rust", "url": "https://tokio.rs/", "content": "<strong>Tokio</strong> runtime"},
        {"title": "a < b", "url": "https://x.example/", "content": "1 &lt; 2 &#x26; 3 &#38; <span\nclass=\"h\">four</span>"},
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
                title: "Tokio & Rust".into(),
                url: "https://tokio.rs/".into(),
                snippet: "Tokio runtime".into(),
            },
            SearchHit {
                title: "a < b".into(),
                url: "https://x.example/".into(),
                snippet: "1 < 2 & 3 & four".into(),
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
