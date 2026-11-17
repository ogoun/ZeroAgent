//! Инструмент `web_fetch` (R1-C2 D110, D112, D113, D123, D127) против поддельного сервера
//! на `127.0.0.1:0` под именем `site.test` (`with_test_resolve`, признак `test-hooks`) и
//! заглушки извлекателя (`support::extractor`). Настоящий интернет не используется.
//!
//! Golden описаний `web_fetch` и `web_search` — `tests/golden/web_tools.json`, пишется только
//! с `ZA_UPDATE_GOLDEN=1`.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::response::Response;
use serde_json::{Value, json};
use support::extractor::StubExtractor;
use support::{Server, drip, redirect, scaled, serve, text, typed, within};
use za_core::extract::{ExtractError, Extractor};
use za_core::paths::Roots;
use za_core::{
    Approval, DIGEST_RESERVE, NetRequest, RiskClass, Tool, ToolCx, ToolKind, ToolOutput,
    call_deadline,
};
use za_protocol::{EffectClass, EffectKey, RunId, RunRole, Tier, canonical_json, net_outcome};
use za_web::{Searxng, WebConfig, WebGateway, web_fetch_tool, web_search_tool};

const LONG: Duration = Duration::from_secs(20);

fn gateway(s: &Server) -> Arc<WebGateway> {
    let cfg = WebConfig {
        ports: vec![s.port()],
        host_interval: Duration::ZERO,
        ..WebConfig::default()
    };
    Arc::new(
        WebGateway::new(cfg)
            .unwrap()
            .with_test_resolve(vec![("site.test".into(), s.addr)]),
    )
}

/// Шлюз без сервера: вызовы отвергаются до соединения.
fn offline() -> Arc<WebGateway> {
    Arc::new(WebGateway::new(WebConfig::default()).unwrap())
}

fn cx(deadline: Option<std::time::Instant>) -> ToolCx {
    let run = RunId::new();
    ToolCx {
        run,
        project_dir: PathBuf::from("."),
        call_id: "c1".into(),
        effect_key: EffectKey::derive(run, 0, 0),
        paths: Vec::new(),
        // Корни run обязательны (по умолчанию закрыто): граница каталога теста.
        roots: Arc::new(Roots::boundary(Path::new(".")).unwrap()),
        deadline,
        output_budget: None,
    }
}

/// Вызов с дедлайном слота (`call_deadline` от старта под пределом теста).
async fn fetch(tool: &Arc<dyn Tool>, args: Value) -> ToolOutput {
    fetch_within(tool, args, None).await
}

/// Вызов с пределом результата слота `output_budget` (I2 ревью задачи 8).
async fn fetch_within(tool: &Arc<dyn Tool>, args: Value, budget: Option<usize>) -> ToolOutput {
    let d = call_deadline(std::time::Instant::now(), scaled(LONG));
    let cx = ToolCx {
        output_budget: budget,
        ..cx(Some(d))
    };
    within(LONG * 2, "web_fetch", tool.call(args, cx)).await
}

fn page_url(s: &Server, path: &str) -> String {
    format!("http://site.test:{}{path}", s.port())
}

/// Сайт: `robots.txt` — 404 (всё разрешено), остальные пути — `route`.
async fn site<F>(route: F) -> Server
where
    F: Fn(&str) -> axum::response::Response + Send + Sync + 'static,
{
    serve(move |p| {
        if p == "/robots.txt" {
            text(404, "")
        } else {
            route(p)
        }
    })
    .await
}

/// m2 ревью задачи 14: извлекателя нет (бинарник не знает своего пути) — `web_fetch` всё
/// равно работает: текст, JSON и XML отдаёт как есть, а на HTML и PDF честно отвечает, что
/// извлечения нет (`za_core::extract::Unavailable`, разбора в процессе агента нет, D127).
#[tokio::test]
async fn fetch_without_extractor_reads_text_and_refuses_html() {
    let s = site(|p| match p {
        "/t" => typed(200, "text/plain; charset=utf-8", b"plain words".to_vec()),
        "/h" => typed(
            200,
            "text/html; charset=utf-8",
            b"<html><body><p>Hi</p></body></html>".to_vec(),
        ),
        _ => typed(200, "application/pdf", b"%PDF-1.4\n%fake\n".to_vec()),
    })
    .await;
    let tool = web_fetch_tool(gateway(&s), Arc::new(za_core::extract::Unavailable));
    // Копия PDF ложится в `.zero-agent/work/tmp/<run>` проекта вызова — каталог теста.
    let root = project("no_extractor");
    let run = RunId::new();
    let call = |path: &str| fetch_in(&tool, &root, run, json!({"url": page_url(&s, path)}));
    let out = call("/t").await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.ends_with("plain words"), "{}", out.content);
    let out = call("/h").await;
    assert!(out.is_error);
    assert_eq!(
        out.content,
        "html: extraction is unavailable in this session"
    );
    let out = call("/p").await;
    assert!(out.is_error);
    assert_eq!(
        out.content,
        "pdf: extraction is unavailable in this session"
    );
}

#[tokio::test]
async fn fetch_html_page() {
    // Страница в windows-1251: извлекатель получает её уже в UTF-8 (D109).
    let html = "<html><head><title>T</title></head><body><p>Привет, мир</p></body></html>";
    let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode(html);
    let bytes = bytes.into_owned();
    let s = site(move |_| typed(200, "text/html; charset=windows-1251", bytes.clone())).await;
    let ex = StubExtractor::new();
    let tool = web_fetch_tool(gateway(&s), ex.clone());
    let url = page_url(&s, "/a");
    let out = fetch(&tool, json!({"url": url})).await;

    assert!(!out.is_error, "{}", out.content);
    let head = format!("url: {url}\nstatus: 200\ntype: text/html\ntitle: T\n\n");
    assert!(out.content.starts_with(&head), "{}", out.content);
    assert!(out.content.ends_with("Привет, мир"), "{}", out.content);
    let seen = ex.html_requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].html, html, "HTML уходит извлекателю в UTF-8");
    assert_eq!(seen[0].base_url, url);
    assert_eq!(out.taint.as_deref(), Some("web"));
    // robots.txt и страница (D112).
    assert_eq!(out.requests.len(), 2, "{:?}", out.requests);
    assert!(out.requests[0].url.ends_with("/robots.txt"));
    assert_eq!(out.requests[1].url, url);
    assert_eq!(out.requests[1].status, Some(200));
}

#[tokio::test]
async fn simplified_extraction_is_marked_in_result() {
    // Извлекатель вернул упрощённый разбор (D127, ревью задачи 7): шапка несёт пометку.
    let s = site(|_| {
        typed(
            200,
            "text/html",
            b"<html><body><p>deep</p></body></html>".to_vec(),
        )
    })
    .await;
    let tool = web_fetch_tool(gateway(&s), StubExtractor::simplifying());
    let out = fetch(&tool, json!({"url": page_url(&s, "/a")})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("[simplified extraction:"),
        "нет пометки упрощения: {}",
        out.content
    );
    // Пометка — в шапке, до текста страницы.
    let marker = out.content.find("[simplified extraction:").unwrap();
    let body = out.content.find("deep").unwrap();
    assert!(
        marker < body,
        "пометка должна быть в шапке: {}",
        out.content
    );
    assert_eq!(out.taint.as_deref(), Some("web"));
}

#[tokio::test]
async fn extractor_failure_is_tool_error() {
    let s = site(|_| typed(200, "text/html", b"<p>x</p>".to_vec())).await;
    let ex = StubExtractor::failing(ExtractError::Failed("crashed".into()));
    let tool = web_fetch_tool(gateway(&s), ex);
    let out = fetch(&tool, json!({"url": page_url(&s, "/a")})).await;
    assert!(out.is_error);
    assert_eq!(out.content, "html: extractor failed: crashed");
    assert_eq!(out.taint.as_deref(), Some("web"));
    assert_eq!(out.requests.len(), 2);
}

#[tokio::test]
async fn slow_server_timeout_keeps_requests() {
    // Тело по байту раз в 50 мс, без конца; дедлайн вызова — 2 с (× масштаб времени).
    let s = site(|_| drip(Duration::from_millis(50))).await;
    let tool = web_fetch_tool(gateway(&s), StubExtractor::new());
    let budget = scaled(Duration::from_secs(2));
    let deadline = std::time::Instant::now() + budget;
    // Внешний предел — как тайм-аут слота: дедлайн плюс запас. Вызов возвращается сам.
    let out = tokio::time::timeout(
        budget + scaled(Duration::from_secs(10)),
        tool.call(json!({"url": page_url(&s, "/a")}), cx(Some(deadline))),
    )
    .await
    .expect("внешний тайм-аут не должен срабатывать: дедлайн соблюдает шлюз");
    assert!(out.is_error);
    assert!(out.content.starts_with("timeout after "), "{}", out.content);
    // Журнал обращений не пуст (D112): robots.txt и прерванный запрос страницы.
    assert!(!out.requests.is_empty());
    let last = out.requests.last().unwrap();
    assert_eq!(last.outcome, net_outcome::ERROR);
    assert_eq!(last.reason.as_deref(), Some("timeout"));
    assert!(last.bytes > 0, "{last:?}");
}

#[test]
fn requests_not_serialized() {
    let out = ToolOutput {
        requests: vec![NetRequest {
            method: "GET".into(),
            url: "http://site.test/?token=x".into(),
            outcome: net_outcome::OK.into(),
            ..NetRequest::default()
        }]
        .into(),
        ..ToolOutput::ok("x")
    };
    let v = serde_json::to_value(&out).unwrap();
    assert!(v.get("requests").is_none(), "{v}");
    assert!(!v.to_string().contains("token"), "{v}");
    // Квитанция читается обратно без записей.
    let back: ToolOutput = serde_json::from_value(v).unwrap();
    assert!(back.requests.is_empty());
    assert_eq!(back.content, "x");
}

#[tokio::test]
async fn fetch_404_is_error_with_status() {
    let s = site(|_| {
        typed(
            404,
            "text/html",
            b"<html><body>gone away</body></html>".to_vec(),
        )
    })
    .await;
    let tool = web_fetch_tool(gateway(&s), StubExtractor::new());
    let out = fetch(&tool, json!({"url": page_url(&s, "/missing")})).await;
    assert!(out.is_error);
    assert!(
        out.content.starts_with("HTTP 404 Not Found"),
        "{}",
        out.content
    );
    assert!(out.content.contains("gone away"), "{}", out.content);
}

#[tokio::test]
async fn error_body_is_cut_to_2_kib() {
    let body = "e".repeat(10_000);
    let s = site(move |_| typed(500, "text/plain", body.clone().into_bytes())).await;
    let tool = web_fetch_tool(gateway(&s), StubExtractor::new());
    let out = fetch(&tool, json!({"url": page_url(&s, "/a")})).await;
    assert!(out.is_error);
    let body = out
        .content
        .strip_prefix("HTTP 500 Internal Server Error\n\n")
        .expect("строка статуса и пустая строка");
    assert!((1..=2048).contains(&body.len()), "{}", body.len());
    assert!(body.chars().all(|c| c == 'e'));
}

#[tokio::test]
async fn fetch_redirect_other_host_text() {
    let s = site(|_| redirect("http://other.test/")).await;
    let tool = web_fetch_tool(gateway(&s), StubExtractor::new());
    let out = fetch(&tool, json!({"url": page_url(&s, "/a")})).await;
    assert_eq!(
        out.content,
        "redirected to http://other.test/; call web_fetch with it if it is the page you need"
    );
    assert_eq!(out.taint.as_deref(), Some("web"));
}

#[tokio::test]
async fn fetch_blocked_local() {
    let tool = web_fetch_tool(offline(), StubExtractor::new());
    let out = fetch(&tool, json!({"url": "http://127.0.0.1/"})).await;
    assert!(out.is_error);
    assert_eq!(out.content, "blocked: local or private address");
    assert_eq!(out.requests.len(), 1);
    assert_eq!(out.requests[0].outcome, net_outcome::BLOCKED);
}

#[tokio::test]
async fn invalid_arguments_and_url() {
    let tool = web_fetch_tool(offline(), StubExtractor::new());
    let out = fetch(&tool, json!({})).await;
    assert!(out.is_error);
    assert!(
        out.content.starts_with("invalid arguments: "),
        "{}",
        out.content
    );
    let out = fetch(&tool, json!({"url": "not a url"})).await;
    assert!(out.is_error);
    assert!(out.content.starts_with("invalid URL: "), "{}", out.content);
    assert!(out.requests.is_empty());
}

#[tokio::test]
async fn long_page_with_prompt_gets_ranked_digest() {
    // 400 КиБ текста: абзацы-заполнители и один абзац со словами запроса.
    let filler = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do\n\n";
    let needle = "zebra migration follows the seasonal rains across the plains";
    let mut body = filler.repeat(3300);
    body.push_str(needle);
    body.push_str("\n\n");
    body.push_str(&filler.repeat(3300));
    assert!(body.len() > 400 * 1024);
    let s = site(move |_| typed(200, "text/plain", body.clone().into_bytes())).await;
    let tool = web_fetch_tool(gateway(&s), StubExtractor::new());
    let url = page_url(&s, "/long");
    let out = fetch(&tool, json!({"url": url, "prompt": "zebra migration"})).await;
    assert!(!out.is_error);
    assert!(out.content.contains(needle));
    let digest = out.digest.expect("digest при prompt");
    assert!(digest.starts_with(&format!("url: {url}\nstatus: 200\ntype: text/plain\n\n")));
    assert!(digest.contains(needle), "{}", &digest[..200]);
    // Вызов вне пакета (`output_budget = None`): предел — `output_limit` (128 КиБ) за
    // вычетом запаса под строки слота (I2 ревью задачи 8).
    assert!(
        digest.len() <= 128 * 1024 - DIGEST_RESERVE,
        "{}",
        digest.len()
    );

    // Без prompt — digest нет: голова и хвост D44 делает пакет.
    let out = fetch(&tool, json!({"url": url})).await;
    assert!(out.digest.is_none());
}

/// Длина текста после `escape_untrusted` для ASCII: `<` и `>` становятся `&lt;` и `&gt;`.
fn escaped_ascii_len(s: &str) -> usize {
    s.len() + 3 * s.matches(['<', '>']).count()
}

/// I2 ревью задачи 8: бюджет `digest` — фактический предел слота `ToolCx::output_budget`, а
/// не 60 % `output_limit`. На окнах 16k (9830 байт) и 64k (39 321 байт) `digest` с запасом
/// под строки слота помещается в предел (слот не режет его головой и хвостом), а лучшее
/// совпадение из середины страницы в нём есть. Длина меряется после экранирования: слабые
/// совпадения несут `<` и `>`.
#[tokio::test]
async fn digest_fits_slot_budget_on_small_windows() {
    let filler = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do\n\n";
    let weak = |i: usize| format!("weather note {i}: <rains> expected later in the region\n\n");
    let needle = "zebra migration follows the seasonal rains across the plains";
    let mut body = String::new();
    for i in 0..3000 {
        body.push_str(filler);
        body.push_str(&weak(i));
        if i == 1500 {
            body.push_str(needle);
            body.push_str("\n\n");
        }
    }
    let weak_total = (0..3000).map(|i| weak(i).len()).sum::<usize>();
    assert!(
        weak_total > 2 * 39_321,
        "слабых совпадений больше любого бюджета"
    );
    let s = site(move |_| typed(200, "text/plain", body.clone().into_bytes())).await;
    let tool = web_fetch_tool(gateway(&s), StubExtractor::new());
    let url = page_url(&s, "/long");
    for budget in [9830usize, 39_321] {
        let args = json!({"url": url, "prompt": "zebra migration in the seasonal rains"});
        let out = fetch_within(&tool, args, Some(budget)).await;
        assert!(!out.is_error, "{}", out.content);
        let digest = out.digest.expect("digest при prompt");
        assert!(
            digest.starts_with(&format!("url: {url}\nstatus: 200\ntype: text/plain\n\n")),
            "{budget}"
        );
        assert!(digest.contains(needle), "{budget}: нет лучшего совпадения");
        let shown = escaped_ascii_len(&digest);
        assert!(
            shown + DIGEST_RESERVE <= budget,
            "{budget}: digest {shown} байт после экранирования"
        );
        // Слабые совпадения заполняют остаток, но не все: ранжирование работает.
        let weak_in = digest.matches("weather note").count();
        assert!(weak_in > 0 && weak_in < 3000, "{budget}: {weak_in}");
        assert!(
            shown > budget / 2,
            "{budget}: бюджет использован слабо ({shown})"
        );
    }
}

/// M3 ревью задачи 8: HTML из одной разметки — извлекатель вернул пустой текст, ответ
/// `no readable text on the page` (ветка после извлекателя).
#[tokio::test]
async fn markup_only_html_has_no_readable_text() {
    let html = "<html><body><div><span></span></div></body></html>";
    let s = site(move |_| typed(200, "text/html", html.as_bytes().to_vec())).await;
    let ex = StubExtractor::new();
    let tool = web_fetch_tool(gateway(&s), ex.clone());
    let url = page_url(&s, "/markup");
    let out = fetch(&tool, json!({"url": url})).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        format!("url: {url}\nstatus: 200\ntype: text/html\n\nno readable text on the page")
    );
    assert_eq!(ex.html_requests().len(), 1, "извлекатель вызван");
}

#[tokio::test]
async fn long_title_is_cut() {
    // Перенос задачи 7: заголовок враждебной страницы ограничен, переводы строк — пробелы.
    let title = format!("A\nstatus: 200\n{}", "t".repeat(5000));
    let html = format!("<html><title>{title}</title><p>body</p></html>");
    let s = site(move |_| typed(200, "text/html", html.clone().into_bytes())).await;
    let tool = web_fetch_tool(gateway(&s), StubExtractor::new());
    let out = fetch(&tool, json!({"url": page_url(&s, "/a")})).await;
    let line = out
        .content
        .lines()
        .find(|l| l.starts_with("title: "))
        .expect("строка title");
    assert!(line.starts_with("title: A status: 200 ttt"), "{line}");
    assert!(line.ends_with("..."), "{line}");
    assert!(
        line.chars().count() <= "title: ".len() + 200,
        "{}",
        line.len()
    );
    let status_lines = out.content.lines().filter(|l| l.starts_with("status: "));
    assert_eq!(status_lines.count(), 1, "{}", out.content);
}

#[tokio::test]
async fn binary_under_text_html_has_no_readable_text() {
    // Перенос задачи 7: двоичные данные с `Content-Type: text/html` — честный ответ, без
    // разбора извлекателем.
    let bytes: Vec<u8> = (0..4096u32).map(|i| (i * 7 + 3) as u8 | 0x80).collect();
    let s = site(move |_| typed(200, "text/html", bytes.clone())).await;
    let ex = StubExtractor::new();
    let tool = web_fetch_tool(gateway(&s), ex.clone());
    let url = page_url(&s, "/bin");
    let out = fetch(&tool, json!({"url": url})).await;
    assert_eq!(
        out.content,
        format!("url: {url}\nstatus: 200\ntype: text/html\n\nno readable text on the page")
    );
    assert!(ex.html_requests().is_empty(), "извлекатель не вызван");

    // Пустая страница — тот же ответ.
    let s = site(|_| typed(200, "text/plain", b"  \n\n ".to_vec())).await;
    let tool = web_fetch_tool(gateway(&s), StubExtractor::new());
    let url = page_url(&s, "/empty");
    let out = fetch(&tool, json!({"url": url})).await;
    assert!(
        out.content.ends_with("\n\nno readable text on the page"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn truncated_body_is_marked() {
    let s = site(|_| support::endless()).await;
    let cfg = WebConfig {
        ports: vec![s.port()],
        host_interval: Duration::ZERO,
        max_bytes: 64 * 1024,
        ..WebConfig::default()
    };
    let gw = Arc::new(
        WebGateway::new(cfg)
            .unwrap()
            .with_test_resolve(vec![("site.test".into(), s.addr)]),
    );
    let tool = web_fetch_tool(gw, StubExtractor::new());
    let out = fetch(&tool, json!({"url": page_url(&s, "/a")})).await;
    assert!(!out.is_error);
    assert!(
        out.content.ends_with("\n[truncated at 65536 bytes]"),
        "{}",
        &out.content[out.content.len() - 60..]
    );
}

/// M2 ревью задачи 8: двоичные данные под `text/html; charset=windows-1251` — в однобайтовой
/// кодировке байты ≥ 0x80 становятся буквами, но управляющие «двоичные» байты WHATWG
/// (0x00–0x08, 0x0B, 0x0E–0x1A, 0x1C–0x1F) остаются управляющими символами: их доля
/// выдаёт двоичные данные. Извлекатель не вызывается.
#[tokio::test]
async fn binary_under_single_byte_charset_has_no_readable_text() {
    // Каждый 20-й байт — NUL (5 %), остальные — «буквы» cp1251.
    let bytes: Vec<u8> = (0..4096u32)
        .map(|i| {
            if i % 20 == 0 {
                0
            } else {
                (i * 7 + 3) as u8 | 0xC0
            }
        })
        .collect();
    let s = site(move |_| typed(200, "text/html; charset=windows-1251", bytes.clone())).await;
    let ex = StubExtractor::new();
    let tool = web_fetch_tool(gateway(&s), ex.clone());
    let url = page_url(&s, "/bin");
    let out = fetch(&tool, json!({"url": url})).await;
    assert_eq!(
        out.content,
        format!("url: {url}\nstatus: 200\ntype: text/html\n\nno readable text on the page")
    );
    assert!(ex.html_requests().is_empty(), "извлекатель не вызван");
}

/// M5 ревью задачи 8: оборванное на `max_bytes` нечитаемое тело — `no readable text` с
/// пометкой обрыва: ответ не выглядит полным.
#[tokio::test]
async fn truncated_unreadable_body_keeps_marker() {
    let s = site(|_| typed(200, "text/plain", vec![0u8; 100_000])).await;
    let cfg = WebConfig {
        ports: vec![s.port()],
        host_interval: Duration::ZERO,
        max_bytes: 64 * 1024,
        ..WebConfig::default()
    };
    let gw = Arc::new(
        WebGateway::new(cfg)
            .unwrap()
            .with_test_resolve(vec![("site.test".into(), s.addr)]),
    );
    let tool = web_fetch_tool(gw, StubExtractor::new());
    let url = page_url(&s, "/bin");
    let out = fetch(&tool, json!({"url": url})).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        format!(
            "url: {url}\nstatus: 200\ntype: text/plain\n\nno readable text on the page\n\
             [truncated at 65536 bytes]"
        )
    );
}

#[test]
fn subjects_host_normalized() {
    let tool = web_fetch_tool(offline(), StubExtractor::new());
    let s = tool.subjects(&json!({"url": "http://Пример.РФ./x"}));
    assert!(s.network);
    assert_eq!(s.hosts, ["xn--e1afmkfd.xn--p1ai"]);
    assert!(!s.hosts_unknown);
    // Хост не определить — неизвестен (D104: строгое подмножество).
    let s = tool.subjects(&json!({"url": "::"}));
    assert!(s.network && s.hosts.is_empty() && s.hosts_unknown);
}

#[test]
fn contract_matches_d123() {
    let tool = web_fetch_tool(offline(), StubExtractor::new());
    let s = tool.spec();
    assert_eq!(s.schema.name, "web_fetch");
    assert_eq!(s.kind, ToolKind::Read);
    assert_eq!(s.class, EffectClass::ReadOnly);
    assert_eq!(s.risk, RiskClass::Network);
    assert_eq!(s.min_tier, Tier::Standard);
    assert_eq!(s.timeout, Duration::from_secs(60));
    assert_eq!(s.output_limit, Some(128 * 1024));
    assert!(s.tainted);
    assert_eq!(s.approval, Approval::Mode);
    assert_eq!(
        s.roles,
        [RunRole::Top, RunRole::Orchestrator, RunRole::Worker]
    );
    assert!(s.schema.description.is_ascii());
    assert_eq!(s.schema.parameters["type"], "object");
    assert_eq!(s.schema.parameters["required"], json!(["url"]));
    // Тексты для модели — `runtime/web.*`, ASCII (D94).
    let texts = tool.texts();
    assert!(!texts.is_empty());
    for (name, text) in texts {
        assert!(name.starts_with("runtime/web."), "{name}");
        assert!(text.is_ascii(), "{name}: {text}");
    }
}

/// Сравнивает `actual` с golden `tests/golden/<file>`; с `ZA_UPDATE_GOLDEN=1` — пишет.
fn check_golden(file: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(file);
    if std::env::var("ZA_UPDATE_GOLDEN").as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
    }
    let golden = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; ZA_UPDATE_GOLDEN=1", path.display()))
        .replace("\r\n", "\n");
    assert_eq!(golden, actual, "{file}: обновить — ZA_UPDATE_GOLDEN=1");
}

#[test]
fn golden_descriptions() {
    // Канонический JSON, как `za_llm::render_tools` (массив по имени, объекты
    // `{description, name, parameters}`): `za-web` не зависит от `za-llm` (политика
    // направлений), а замена CRLF не нужна — переводов строк CR в описании нет.
    let provider = Arc::new(Searxng::new(
        url::Url::parse("http://127.0.0.1:8888/").unwrap(),
    ));
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        web_fetch_tool(offline(), StubExtractor::new()),
        web_search_tool(offline(), provider),
    ];
    // Порядок — по имени, как у `render_tools` (M6 ревью задачи 8: с `web_search` он важен).
    tools.sort_by(|a, b| a.spec().schema.name.cmp(&b.spec().schema.name));
    let arr: Vec<Value> = tools
        .iter()
        .map(|t| {
            let s = &t.spec().schema;
            assert!(!s.description.contains('\r'));
            json!({"name": s.name, "description": s.description, "parameters": s.parameters})
        })
        .collect();
    let rendered = canonical_json(&Value::Array(arr));
    let v: Value = serde_json::from_str(&rendered).unwrap();
    check_golden(
        "web_tools.json",
        &format!("{}\n", serde_json::to_string_pretty(&v).unwrap()),
    );
}

/// Заглушка — `Arc<dyn Extractor>` (форма аргумента `web_fetch_tool`).
#[test]
fn stub_is_extractor() {
    let _: Arc<dyn Extractor> = StubExtractor::new();
}

// ---- PDF (D113, задача 10) ----

/// Байты «PDF» для заглушки: сигнатура и двоичный комментарий.
const PDF_BYTES: &[u8] = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n1 0 obj << >> endobj\n%%EOF\n";

/// Каталог проекта вызова: копия PDF кладётся в его `.zero-agent/work/tmp/<run>/`.
fn project(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("za-web-fetch")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Вызов с каталогом проекта `root` и run `run`.
async fn fetch_in(tool: &Arc<dyn Tool>, root: &Path, run: RunId, args: Value) -> ToolOutput {
    let d = call_deadline(std::time::Instant::now(), scaled(LONG));
    let cx = ToolCx {
        run,
        project_dir: root.to_path_buf(),
        effect_key: EffectKey::derive(run, 3, 1),
        ..cx(Some(d))
    };
    within(LONG * 2, "web_fetch", tool.call(args, cx)).await
}

#[tokio::test]
async fn fetch_pdf_uses_extractor() {
    let s = site(|_| typed(200, "application/pdf", PDF_BYTES.to_vec())).await;
    let ex = StubExtractor::with_pdf(&[(1, "alpha"), (2, "")], 30);
    let tool = web_fetch_tool(gateway(&s), ex.clone());
    let root = project("pdf_ok");
    let run = RunId::new();
    let out = fetch_in(&tool, &root, run, json!({"url": page_url(&s, "/doc.pdf")})).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.taint.as_deref(), Some("web"));
    assert_eq!(
        out.content,
        format!(
            "url: {}\nstatus: 200\ntype: application/pdf\n\n[pdf: 30 pages; showing 1-2]\n\
             --- page 1 ---\nalpha\n--- page 2 ---\n[page 2: no text layer]",
            page_url(&s, "/doc.pdf")
        )
    );
    // Первые 20 страниц; копия тела — в `work/tmp/<run>` проекта, после вызова её нет.
    let seen = ex.pdf_requests();
    assert_eq!(seen.len(), 1);
    let (req, len) = &seen[0];
    assert_eq!((req.first, req.count), (1, 20));
    assert_eq!(*len, Some(PDF_BYTES.len() as u64));
    let tmp = std::fs::canonicalize(root.join(format!(".zero-agent/work/tmp/{run}"))).unwrap();
    let parent = std::fs::canonicalize(req.path.parent().unwrap()).unwrap();
    assert_eq!(parent, tmp);
    assert!(!req.path.exists(), "копия не удалена");
    assert!(ex.html_requests().is_empty());
}

/// Тело PDF больше 32 МиБ: ответ обрезан, а обрезанный PDF не разбирается — отказ без
/// вызова извлекателя. Предел PDF — не `max_bytes` (5 МиБ): PDF в 6 МиБ без заголовка
/// типа (сигнатура в начале тела) разбирается целиком.
#[tokio::test]
async fn fetch_pdf_over_32_mib_refused() {
    let mut big = PDF_BYTES.to_vec();
    big.resize(33 * 1024 * 1024, b' ');
    let mut mid = PDF_BYTES.to_vec();
    mid.resize(6 * 1024 * 1024, b' ');
    let s = site(move |p| match p {
        "/big.pdf" => typed(200, "application/pdf", big.clone()),
        _ => Response::new(axum::body::Body::from(mid.clone())),
    })
    .await;
    let ex = StubExtractor::with_pdf(&[(1, "alpha")], 1);
    let tool = web_fetch_tool(gateway(&s), ex.clone());
    let root = project("pdf_big");
    let out = fetch_in(
        &tool,
        &root,
        RunId::new(),
        json!({"url": page_url(&s, "/big.pdf")}),
    )
    .await;
    assert!(out.is_error);
    assert_eq!(out.content, "pdf larger than 33554432 bytes");
    assert!(
        ex.pdf_requests().is_empty(),
        "обрезанный PDF не разбирается"
    );
    // Тело прочитано до предела PDF + 1 байт, а не до `max_bytes` (m1 ревью задачи 10).
    assert_eq!(page_request(&out, "/big.pdf").bytes, 32 * 1024 * 1024 + 1);
    let out = fetch_in(
        &tool,
        &root,
        RunId::new(),
        json!({"url": page_url(&s, "/mid")}),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("type: application/pdf\n"),
        "{}",
        out.content
    );
    let seen = ex.pdf_requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1, Some(6 * 1024 * 1024));
}

/// Запись журнала о запросе страницы (адрес оканчивается на `path`).
fn page_request<'a>(out: &'a ToolOutput, path: &str) -> &'a NetRequest {
    out.requests
        .iter()
        .find(|r| r.url.ends_with(path))
        .unwrap_or_else(|| panic!("нет запроса {path}: {:?}", out.requests))
}

/// m1 ревью задачи 10: тело с заголовком `application/pdf` читается дальше `max_bytes`
/// (5 МиБ) — PDF в 6 МиБ целиком доходит до извлекателя.
#[tokio::test]
async fn declared_pdf_is_read_past_max_bytes() {
    let mut mid = PDF_BYTES.to_vec();
    mid.resize(6 * 1024 * 1024, b' ');
    let s = site(move |_| typed(200, "application/pdf", mid.clone())).await;
    let ex = StubExtractor::with_pdf(&[(1, "alpha")], 1);
    let tool = web_fetch_tool(gateway(&s), ex.clone());
    let root = project("pdf_declared");
    let out = fetch_in(
        &tool,
        &root,
        RunId::new(),
        json!({"url": page_url(&s, "/declared.pdf")}),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        page_request(&out, "/declared.pdf").bytes,
        6 * 1024 * 1024,
        "тело обрезано на max_bytes"
    );
    let seen = ex.pdf_requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1, Some(6 * 1024 * 1024));
}

/// m9 ревью задачи 10: PDF с кодом не 2xx не разбирается, поэтому и читается не дальше
/// `max_bytes` — ни по заголовку `application/pdf`, ни по сигнатуре.
#[tokio::test]
async fn error_status_pdf_is_not_read_past_max_bytes() {
    let mut big = PDF_BYTES.to_vec();
    big.resize(6 * 1024 * 1024, b' ');
    let s = site(move |p| {
        let status = if p == "/gone.pdf" { 404 } else { 500 };
        match p {
            "/gone.pdf" | "/fail" => {
                let mut r = typed(status, "application/pdf", big.clone());
                if p == "/fail" {
                    r.headers_mut().remove(axum::http::header::CONTENT_TYPE);
                }
                r
            }
            _ => text(404, ""),
        }
    })
    .await;
    let ex = StubExtractor::with_pdf(&[(1, "alpha")], 1);
    let tool = web_fetch_tool(gateway(&s), ex.clone());
    let root = project("pdf_status");
    for (path, status) in [("/gone.pdf", 404), ("/fail", 500)] {
        let out = fetch_in(
            &tool,
            &root,
            RunId::new(),
            json!({"url": page_url(&s, path)}),
        )
        .await;
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.starts_with(&format!("HTTP {status}")),
            "{}",
            out.content
        );
        assert_eq!(
            page_request(&out, path).bytes,
            WebConfig::default().max_bytes,
            "{path}: тело прочитано дальше max_bytes"
        );
    }
    assert!(ex.pdf_requests().is_empty());
}

/// Ошибка извлечения PDF — `pdf: <причина>`, результат-ошибка.
#[tokio::test]
async fn fetch_pdf_extract_error_text() {
    let s = site(|_| typed(200, "application/pdf", PDF_BYTES.to_vec())).await;
    let ex = Arc::new(StubExtractor {
        pdf_error: Some(ExtractError::Encrypted),
        ..StubExtractor::default()
    });
    let tool = web_fetch_tool(gateway(&s), ex);
    let root = project("pdf_err");
    let out = fetch_in(
        &tool,
        &root,
        RunId::new(),
        json!({"url": page_url(&s, "/d.pdf")}),
    )
    .await;
    assert!(out.is_error);
    assert_eq!(out.content, "pdf: document is encrypted");
}
