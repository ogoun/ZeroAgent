//! Сетевой шлюз (D107, D108, D112) против поддельных серверов на `127.0.0.1:0`.
//!
//! Шлюз видит сервер под именем `site.test` через `with_test_resolve` (признак
//! `test-hooks`, D128). Настоящий интернет не используется. Время — настоящее (сокеты):
//! `start_paused` здесь не применяется (D121), ожидания — под `within`.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use support::{Server, drip, endless, redirect, scaled, serve, text, with_header, within};
use tokio::time::Instant;
use url::Url;
use za_protocol::net_outcome;
use za_web::dns::{Addrs, Name, Resolve, Resolving};
use za_web::{FetchError, Fetched, WebConfig, WebGateway};

const LONG: Duration = Duration::from_secs(20);

fn config(ports: &[u16]) -> WebConfig {
    WebConfig {
        ports: ports.to_vec(),
        host_interval: Duration::ZERO,
        ..WebConfig::default()
    }
}

fn gateway(cfg: WebConfig, names: &[(&str, SocketAddr)]) -> WebGateway {
    WebGateway::new(cfg)
        .unwrap()
        .with_test_resolve(names.iter().map(|(n, a)| (n.to_string(), *a)).collect())
}

fn site(s: &Server) -> WebGateway {
    gateway(config(&[s.port()]), &[("site.test", s.addr)])
}

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

fn deadline() -> Instant {
    Instant::now() + scaled(LONG)
}

async fn get(gw: &WebGateway, u: &str) -> Fetched {
    within(LONG * 2, u, gw.get(&url(u), deadline())).await
}

#[tokio::test]
async fn port_outside_allowed_list_blocked() {
    let mut s = serve(|_| text(200, "x")).await;
    let gw = site(&s);
    let f = get(&gw, "http://site.test:25/").await;
    assert_eq!(
        f.result.err(),
        Some(FetchError::Blocked("port 25 is not allowed".into()))
    );
    assert_eq!(f.requests.len(), 1);
    assert_eq!(f.requests[0].outcome, net_outcome::BLOCKED);
    assert_eq!(
        f.requests[0].reason.as_deref(),
        Some("port 25 is not allowed")
    );
    assert!(s.paths().is_empty(), "подключений нет");
}

/// Заглушка внутреннего резолвера: публичный и частный адрес в одном ответе.
struct Mixed {
    addrs: Vec<SocketAddr>,
    calls: Arc<AtomicUsize>,
}

impl Resolve for Mixed {
    fn resolve(&self, _name: Name) -> Resolving {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let addrs = self.addrs.clone();
        Box::pin(async move { Ok(Box::new(addrs.into_iter()) as Addrs) })
    }
}

#[tokio::test]
async fn mixed_dns_answer_blocks_whole_name() {
    let mut s = serve(|_| text(200, "x")).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = Mixed {
        addrs: vec!["8.8.8.8:0".parse().unwrap(), s.addr],
        calls: calls.clone(),
    };
    let gw = WebGateway::new(config(&[s.port()]))
        .unwrap()
        .with_inner_resolver(Arc::new(inner));
    let f = get(&gw, &format!("http://mixed.test:{}/page", s.port())).await;
    assert_eq!(
        f.result.err(),
        Some(FetchError::Blocked("local or private address".into()))
    );
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "фильтр стоит поверх внутреннего"
    );
    // Отказ имени на `robots.txt` — и страница не запрашивается: обе записи `blocked`.
    let outcomes: Vec<&str> = f.requests.iter().map(|r| r.outcome.as_str()).collect();
    assert_eq!(outcomes, [net_outcome::BLOCKED, net_outcome::BLOCKED]);
    assert!(f.requests[0].url.ends_with("/robots.txt"));
    assert!(f.requests[1].url.ends_with("/page"));
    assert!(s.paths().is_empty(), "ни одного подключения");
}

#[tokio::test]
async fn redirect_to_other_host_is_not_followed() {
    let mut other = serve(|_| text(200, "other")).await;
    let target = format!("http://other.test:{}/x", other.port());
    let t = target.clone();
    let mut s = serve(move |p| match p {
        "/" => redirect(&t),
        _ => text(404, ""),
    })
    .await;
    let gw = gateway(
        config(&[s.port(), other.port()]),
        &[("site.test", s.addr), ("other.test", other.addr)],
    );
    let f = get(&gw, &format!("http://site.test:{}/", s.port())).await;
    assert_eq!(f.result.err(), Some(FetchError::Redirect(target.clone())));
    let last = f.requests.last().unwrap();
    assert_eq!(last.status, Some(302));
    assert_eq!(last.final_url.as_deref(), Some(target.as_str()));
    assert_eq!(s.paths(), ["/robots.txt", "/"]);
    assert!(
        other.rx.try_recv().is_err(),
        "other.test запросов не получил"
    );
}

#[tokio::test]
async fn redirect_to_loopback_literal_blocked() {
    // Свой порт сервер узнаёт после запуска.
    let port = Arc::new(std::sync::OnceLock::<u16>::new());
    let p = port.clone();
    let mut s = serve(move |path| match path {
        "/" => redirect(&format!("http://127.0.0.1:{}/loop", p.get().unwrap())),
        _ => text(404, ""),
    })
    .await;
    port.set(s.port()).unwrap();
    let gw = site(&s);
    let f = get(&gw, &format!("http://site.test:{}/", s.port())).await;
    let target = format!("http://127.0.0.1:{}/loop", s.port());
    assert_eq!(f.result.err(), Some(FetchError::Redirect(target)));
    assert_eq!(s.paths(), ["/robots.txt", "/"], "к /loop подключений нет");
}

#[tokio::test]
async fn same_host_redirect_followed_up_to_five() {
    let mut s = serve(|p| {
        if let Some(n) = p.strip_prefix("/r/") {
            let n: u32 = n.parse().unwrap();
            return redirect(&format!("/r/{}", n + 1));
        }
        if let Some(n) = p.strip_prefix("/c/") {
            let n: u32 = n.parse().unwrap();
            return if n < 5 {
                redirect(&format!("/c/{}", n + 1))
            } else {
                text(200, "done")
            };
        }
        match p {
            "/port" => redirect("http://site.test:25/"),
            _ => text(404, ""),
        }
    })
    .await;
    let gw = site(&s);
    let base = format!("http://site.test:{}", s.port());
    // Ровно пять переходов — страница получена.
    let f = get(&gw, &format!("{base}/c/0")).await;
    let body = f.result.unwrap();
    assert_eq!(body.status, 200);
    assert_eq!(body.final_url.as_str(), format!("{base}/c/5"));
    assert_eq!(body.bytes, b"done");
    assert_eq!(f.requests.len(), 7, "robots.txt и шесть запросов страницы");
    // Шестой переход — ошибка.
    s.paths();
    let f = get(&gw, &format!("{base}/r/0")).await;
    assert_eq!(
        f.result.err(),
        Some(FetchError::Network("too many redirects".into()))
    );
    let paths = s.paths();
    assert_eq!(paths, ["/r/0", "/r/1", "/r/2", "/r/3", "/r/4", "/r/5"]);
    // Тот же хост, но порт вне списка — отказ до соединения.
    let f = get(&gw, &format!("{base}/port")).await;
    assert_eq!(
        f.result.err(),
        Some(FetchError::Blocked("port 25 is not allowed".into()))
    );
    assert_eq!(f.requests.last().unwrap().outcome, net_outcome::BLOCKED);
    assert_eq!(s.paths(), ["/port"]);
}

#[tokio::test]
async fn body_cut_at_max_bytes() {
    let s = serve(|p| match p {
        "/inf" => endless(),
        _ => text(404, ""),
    })
    .await;
    let cfg = WebConfig {
        max_bytes: 64 * 1024,
        ..config(&[s.port()])
    };
    let gw = gateway(cfg, &[("site.test", s.addr)]);
    let f = get(&gw, &format!("http://site.test:{}/inf", s.port())).await;
    let body = f.result.unwrap();
    assert!(body.truncated);
    assert_eq!(body.bytes.len(), 65536);
    assert_eq!(f.requests.last().unwrap().bytes, 65536);
    // Тело ровно в предел с `Content-Length` — не обрезано.
    let exact = "y".repeat(65536);
    let s2 = serve(move |p| match p {
        "/exact" => text(200, &exact),
        _ => text(404, ""),
    })
    .await;
    let cfg = WebConfig {
        max_bytes: 64 * 1024,
        ..config(&[s2.port()])
    };
    let gw = gateway(cfg, &[("site.test", s2.addr)]);
    let f = get(&gw, &format!("http://site.test:{}/exact", s2.port())).await;
    let body = f.result.unwrap();
    assert!(!body.truncated);
    assert_eq!(body.bytes.len(), 65536);
}

#[tokio::test]
async fn no_cookies_and_no_accept_encoding() {
    let mut s = serve(|p| match p {
        "/set" => with_header(text(200, "set"), "set-cookie", "sid=1; Path=/"),
        "/check" => text(200, "check"),
        _ => text(404, ""),
    })
    .await;
    let gw = site(&s);
    let base = format!("http://site.test:{}", s.port());
    get(&gw, &format!("{base}/set")).await.result.unwrap();
    get(&gw, &format!("{base}/check")).await.result.unwrap();
    let seen = s.seen();
    let paths: Vec<&str> = seen.iter().map(|x| x.path.as_str()).collect();
    assert_eq!(paths, ["/robots.txt", "/set", "/check"]);
    for x in &seen {
        assert!(x.headers.get("cookie").is_none(), "{}: Cookie", x.path);
        assert!(
            x.headers.get("accept-encoding").is_none(),
            "{}: Accept-Encoding",
            x.path
        );
        let ua = x.headers.get("user-agent").unwrap().to_str().unwrap();
        assert!(ua.starts_with("zero-agent/"), "{ua}");
    }
}

#[tokio::test]
async fn content_encoding_rejected() {
    let s = serve(|p| match p {
        "/gz" => with_header(text(200, "\u{1f}\u{8b}junk"), "content-encoding", "gzip"),
        "/id" => with_header(text(200, "plain"), "content-encoding", "identity"),
        _ => text(404, ""),
    })
    .await;
    let gw = site(&s);
    let base = format!("http://site.test:{}", s.port());
    let f = get(&gw, &format!("{base}/gz")).await;
    let e = f.result.unwrap_err();
    assert_eq!(e, FetchError::Encoding("gzip".into()));
    assert_eq!(e.text(), "unsupported content encoding gzip");
    let last = f.requests.last().unwrap();
    assert_eq!(last.outcome, net_outcome::ERROR);
    assert_eq!(last.status, Some(200));
    // `identity` — без кодирования.
    let f = get(&gw, &format!("{base}/id")).await;
    assert_eq!(f.result.unwrap().bytes, b"plain");
}

#[tokio::test]
async fn deadline_returns_requests() {
    let s = serve(|p| match p {
        "/slow" => drip(Duration::from_millis(50)),
        _ => text(404, ""),
    })
    .await;
    let gw = site(&s);
    // Дедлайн вызова (D107): шлюз сам возвращает Timeout вместе с журналом обращений.
    // Предел 1 с × масштаб (бриф — 300 мс): запас для robots.txt под нагрузкой Windows.
    let dl = Instant::now() + scaled(Duration::from_secs(1));
    let u = url(&format!("http://site.test:{}/slow", s.port()));
    let f = within(LONG, "get с дедлайном", gw.get(&u, dl)).await;
    assert_eq!(f.result.err(), Some(FetchError::Timeout));
    assert!(Instant::now() >= dl);
    assert_eq!(f.requests.len(), 2, "{:?}", f.requests);
    assert!(f.requests[0].url.ends_with("/robots.txt"));
    assert_eq!(f.requests[0].outcome, net_outcome::OK);
    assert_eq!(f.requests[0].status, Some(404));
    assert!(f.requests[1].url.ends_with("/slow"));
    assert_eq!(f.requests[1].outcome, net_outcome::ERROR);
    assert_eq!(f.requests[1].reason.as_deref(), Some("timeout"));
    assert_eq!(f.requests[1].status, Some(200));
}

#[tokio::test]
async fn requests_to_same_host_are_serialized() {
    let mut s = serve(|p| match p {
        "/a" | "/b" => text(200, p),
        _ => text(404, ""),
    })
    .await;
    let gw = site(&s);
    let base = format!("http://site.test:{}", s.port());
    let (ua, ub) = (url(&format!("{base}/a")), url(&format!("{base}/b")));
    let (fa, fb) = within(LONG, "два get", async {
        tokio::join!(biased; gw.get(&ua, deadline()), gw.get(&ub, deadline()))
    })
    .await;
    fa.result.unwrap();
    fb.result.unwrap();
    // Запросы к одному хосту идут по одному: robots.txt — один раз, затем /a, затем /b.
    assert_eq!(s.paths(), ["/robots.txt", "/a", "/b"]);
    assert_eq!(fa.requests.len(), 2);
    assert_eq!(fb.requests.len(), 1, "robots.txt из кэша");
}

#[tokio::test]
async fn robots_5xx_disallows_and_4xx_allows() {
    let mut down = serve(|p| match p {
        "/robots.txt" => text(503, ""),
        _ => text(200, "page"),
    })
    .await;
    let gw = site(&down);
    let f = get(&gw, &format!("http://site.test:{}/page", down.port())).await;
    assert_eq!(f.result.err(), Some(FetchError::Robots("unavailable")));
    assert_eq!(down.paths(), ["/robots.txt"]);
    assert_eq!(f.requests[0].status, Some(503));
    assert_eq!(f.requests[1].outcome, net_outcome::BLOCKED);

    let mut missing = serve(|p| match p {
        "/robots.txt" => text(404, ""),
        _ => text(200, "page"),
    })
    .await;
    let gw = site(&missing);
    let f = get(&gw, &format!("http://site.test:{}/page", missing.port())).await;
    assert_eq!(f.result.unwrap().bytes, b"page");
    assert_eq!(missing.paths(), ["/robots.txt", "/page"]);

    // Правило `Disallow` — отказ без запроса страницы; robots.txt из кэша.
    let mut rules = serve(|p| match p {
        "/robots.txt" => text(200, "User-agent: *\nDisallow: /private\n"),
        _ => text(200, "page"),
    })
    .await;
    let gw = site(&rules);
    let base = format!("http://site.test:{}", rules.port());
    let f = get(&gw, &format!("{base}/private/x")).await;
    assert_eq!(f.result.err(), Some(FetchError::Robots("disallowed")));
    get(&gw, &format!("{base}/public")).await.result.unwrap();
    assert_eq!(rules.paths(), ["/robots.txt", "/public"]);

    // `robots = false` — robots.txt не запрашивается.
    let cfg = WebConfig {
        robots: false,
        ..config(&[down.port()])
    };
    let gw = gateway(cfg, &[("site.test", down.addr)]);
    let f = get(&gw, &format!("http://site.test:{}/page", down.port())).await;
    assert_eq!(f.result.unwrap().bytes, b"page");
    assert_eq!(down.paths(), ["/page"]);
}

/// M1, M3 ревью задачи 6: редирект самого `robots.txt` (RFC 9309, 2.3.1.2). Тот же хост —
/// исполняется (и `http` → `https`); другой хост — не исполняется, подключений к нему нет,
/// `robots.txt` «недоступен» (как 4xx: всё разрешено); 3xx без `Location` — как 5xx; цель,
/// которую отверг фильтр (порт), — запись `blocked`, подключений к ней нет.
#[tokio::test]
async fn robots_redirect_branches() {
    // Тот же хост: правила берутся из цели редиректа.
    let mut same = serve(|p| match p {
        "/robots.txt" => redirect("/rules.txt"),
        "/rules.txt" => text(200, "User-agent: *\nDisallow: /private\n"),
        _ => text(200, "page"),
    })
    .await;
    let gw = site(&same);
    let base = format!("http://site.test:{}", same.port());
    let f = get(&gw, &format!("{base}/private")).await;
    assert_eq!(f.result.err(), Some(FetchError::Robots("disallowed")));
    assert_eq!(same.paths(), ["/robots.txt", "/rules.txt"]);
    let r: Vec<(&str, Option<u16>, Option<&str>)> = f
        .requests
        .iter()
        .map(|r| (r.url.as_str(), r.status, r.final_url.as_deref()))
        .collect();
    let (robots, rules) = (format!("{base}/robots.txt"), format!("{base}/rules.txt"));
    let private = format!("{base}/private");
    assert_eq!(
        r,
        [
            (robots.as_str(), Some(302), Some(rules.as_str())),
            (rules.as_str(), Some(200), None),
            (private.as_str(), None, None),
        ]
    );
    assert_eq!(f.requests[2].outcome, net_outcome::BLOCKED);
    get(&gw, &format!("{base}/public")).await.result.unwrap();
    assert_eq!(same.paths(), ["/public"], "правила из кэша");

    // `http` → `https` того же хоста исполняется: TLS к простому серверу не проходит,
    // `robots.txt` недоступен — страница не запрашивается.
    let mut tls = serve(move |p| match p {
        "/robots.txt" => redirect("https://site.test/robots.txt"),
        _ => text(200, "page"),
    })
    .await;
    let gw = gateway(config(&[tls.port(), 443]), &[("site.test", tls.addr)]);
    let f = get(&gw, &format!("http://site.test:{}/page", tls.port())).await;
    assert_eq!(f.result.err(), Some(FetchError::Robots("unavailable")));
    assert_eq!(f.requests[1].url, "https://site.test/robots.txt");
    assert_eq!(f.requests[1].outcome, net_outcome::ERROR);
    assert!(!tls.paths().contains(&"/page".to_string()));

    // Другой хост: не исполняется, к нему подключений нет; страница получена.
    let mut other = serve(|_| text(200, "User-agent: *\nDisallow: /\n")).await;
    let target = format!("http://other.test:{}/robots.txt", other.port());
    let t = target.clone();
    let mut away = serve(move |p| match p {
        "/robots.txt" => redirect(&t),
        _ => text(200, "page"),
    })
    .await;
    let gw = gateway(
        config(&[away.port(), other.port()]),
        &[("site.test", away.addr), ("other.test", other.addr)],
    );
    let f = get(&gw, &format!("http://site.test:{}/page", away.port())).await;
    assert_eq!(f.result.unwrap().bytes, b"page");
    assert_eq!(away.paths(), ["/robots.txt", "/page"]);
    assert!(
        other.rx.try_recv().is_err(),
        "other.test запросов не получил"
    );
    assert_eq!(f.requests[0].final_url.as_deref(), Some(target.as_str()));
    assert_eq!(f.requests.len(), 2);

    // 3xx без `Location` — как 5xx: всё запрещено.
    let mut bare = serve(|p| match p {
        "/robots.txt" => text(302, ""),
        _ => text(200, "page"),
    })
    .await;
    let gw = site(&bare);
    let f = get(&gw, &format!("http://site.test:{}/page", bare.port())).await;
    assert_eq!(f.result.err(), Some(FetchError::Robots("unavailable")));
    assert_eq!(bare.paths(), ["/robots.txt"]);

    // Цель с портом вне списка: запись `blocked`, подключений к ней нет; `robots.txt`
    // недоступен — как 4xx, страница получена.
    let mut port = serve(|p| match p {
        "/robots.txt" => redirect("http://site.test:25/robots.txt"),
        _ => text(200, "page"),
    })
    .await;
    let gw = site(&port);
    let f = get(&gw, &format!("http://site.test:{}/page", port.port())).await;
    assert_eq!(f.result.unwrap().bytes, b"page");
    assert_eq!(port.paths(), ["/robots.txt", "/page"]);
    let blocked = &f.requests[1];
    assert_eq!(blocked.url, "http://site.test:25/robots.txt");
    assert_eq!(blocked.outcome, net_outcome::BLOCKED);
    assert_eq!(blocked.reason.as_deref(), Some("port 25 is not allowed"));
    assert_eq!(f.requests.len(), 3, "{:?}", f.requests);
}

/// M2 ревью задачи 6: userinfo из `Location` не попадает ни в журнал (`final_url`, `url`
/// отказа), ни в текст `Redirect` для модели.
#[tokio::test]
async fn redirect_userinfo_is_not_logged_or_shown() {
    let other = serve(|_| text(200, "other")).await;
    let oport = other.port();
    let port = Arc::new(std::sync::OnceLock::<u16>::new());
    let p = port.clone();
    let mut s = serve(move |path| match path {
        "/away" => redirect(&format!("http://u:pw@other.test:{oport}/x#frag")),
        "/same" => redirect(&format!("http://u:pw@site.test:{}/y", p.get().unwrap())),
        _ => text(404, ""),
    })
    .await;
    port.set(s.port()).unwrap();
    let gw = gateway(
        config(&[s.port(), oport]),
        &[("site.test", s.addr), ("other.test", other.addr)],
    );
    let base = format!("http://site.test:{}", s.port());
    let clean = format!("http://other.test:{oport}/x");
    let f = get(&gw, &format!("{base}/away")).await;
    let e = f.result.unwrap_err();
    assert_eq!(e, FetchError::Redirect(clean.clone()));
    assert!(!e.text().contains("pw"), "{}", e.text());
    assert_eq!(
        f.requests.last().unwrap().final_url.as_deref(),
        Some(clean.as_str())
    );
    // Тот же хост: цель с userinfo отвергает фильтр, запись отказа — без userinfo.
    let f = get(&gw, &format!("{base}/same")).await;
    assert_eq!(f.result.err(), Some(FetchError::Credentials));
    let y = format!("{base}/y");
    let last = &f.requests[f.requests.len() - 1];
    assert_eq!(last.url, y);
    assert_eq!(last.outcome, net_outcome::BLOCKED);
    assert_eq!(
        f.requests[f.requests.len() - 2].final_url.as_deref(),
        Some(y.as_str())
    );
    let all = format!("{:?}", f.requests);
    assert!(!all.contains("pw"), "{all}");
    assert_eq!(s.paths(), ["/robots.txt", "/away", "/same"]);
}

#[tokio::test]
async fn requests_logged_including_robots_and_blocked() {
    let mut s = serve(|p| match p {
        "/robots.txt" => text(200, "User-agent: *\nDisallow: /private\n"),
        _ => text(200, "page"),
    })
    .await;
    let gw = site(&s);
    let base = format!("http://site.test:{}", s.port());
    let f = get(&gw, &format!("{base}/page")).await;
    f.result.unwrap();
    let r: Vec<(&str, &str, Option<u16>)> = f
        .requests
        .iter()
        .map(|r| (r.url.as_str(), r.outcome.as_str(), r.status))
        .collect();
    let (robots, page) = (format!("{base}/robots.txt"), format!("{base}/page"));
    assert_eq!(
        r,
        [
            (robots.as_str(), net_outcome::OK, Some(200)),
            (page.as_str(), net_outcome::OK, Some(200)),
        ]
    );
    assert!(f.requests.iter().all(|r| r.method == "GET"));
    assert_eq!(f.requests[1].bytes, 4);
    // Отказ robots.txt — запись `blocked` без запроса.
    let f = get(&gw, &format!("{base}/private")).await;
    assert_eq!(f.requests.len(), 1);
    assert_eq!(f.requests[0].outcome, net_outcome::BLOCKED);
    assert_eq!(
        f.requests[0].reason.as_deref(),
        Some("robots.txt: disallowed")
    );
    // Отказ фильтра — запись `blocked` с причиной, ни одного подключения.
    let f = get(&gw, &format!("http://127.0.0.1:{}/", s.port())).await;
    assert_eq!(
        f.result.err(),
        Some(FetchError::Blocked("local or private address".into()))
    );
    assert_eq!(f.requests.len(), 1);
    assert_eq!(f.requests[0].outcome, net_outcome::BLOCKED);
    assert_eq!(
        f.requests[0].reason.as_deref(),
        Some("local or private address")
    );
    // Учётные данные в URL в журнал не попадают.
    let f = get(&gw, "http://user:pw@site.test/").await;
    assert_eq!(f.result.err(), Some(FetchError::Credentials));
    assert_eq!(f.requests[0].url, "http://site.test/");
    assert_eq!(s.paths(), ["/robots.txt", "/page"]);
}

#[tokio::test]
async fn provider_is_refused_until_task_9() {
    let mut s = serve(|_| text(200, "x")).await;
    let gw = site(&s);
    let u = url(&format!("http://site.test:{}/search?q=x", s.port()));
    let f = within(LONG, "get_provider", gw.get_provider(&u, deadline())).await;
    assert_eq!(
        f.result.err(),
        Some(FetchError::Blocked(
            "not the configured search provider".into()
        ))
    );
    assert_eq!(f.requests[0].outcome, net_outcome::BLOCKED);
    assert!(s.paths().is_empty());
}

#[test]
fn bad_config_rejected() {
    let cfg = WebConfig {
        requests_per_minute: 0,
        ..WebConfig::default()
    };
    assert!(WebGateway::new(cfg).is_err());
    let d = WebConfig::default();
    assert!(d.robots);
    assert_eq!(d.max_bytes, 5 * 1024 * 1024);
    assert_eq!(d.requests_per_minute, 60);
    assert_eq!(d.ports, [80, 443, 8080, 8443]);
    assert_eq!(d.host_interval, Duration::from_millis(500));
    assert_eq!(d.connect_timeout, Duration::from_secs(10));
    assert_eq!(d.search_endpoint, None);
}

/// Шлюз держат оба инструмента в `Arc` (D107), вызовы идут из задач tokio.
#[tokio::test]
async fn gateway_and_calls_are_send() {
    fn send_sync<T: Send + Sync>() {}
    fn send<T: Send>(_: &T) {}
    send_sync::<WebGateway>();
    let gw = WebGateway::new(WebConfig::default()).unwrap();
    let u = url("http://example.com/");
    // Future не опрашивается: обращения в сеть нет.
    let f = gw.get(&u, deadline());
    send(&f);
    let p = gw.get_provider(&u, deadline());
    send(&p);
}

/// Переменная окружения подпроцесса: адрес поддельной страницы для дочернего запуска.
const CHILD_SITE: &str = "ZA_WEB_TEST_PROXY_CHILD_SITE";

/// `HTTP_PROXY` и другие переменные прокси не действуют (`no_proxy()`, D107): прокси
/// разрешал бы имена мимо фильтра адресов. Заданы и строчные `http_proxy`, `https_proxy`,
/// `all_proxy`: `reqwest` на Unix читает и их (подозрение 3 отчёта исправления CI Linux,
/// fix-8); на Windows имена переменных без учёта регистра. Без `std::env::set_var`
/// (`unsafe_code = "forbid"`): тест перезапускает свой исполняемый файл с окружением через
/// `Command::env`; дочерний запуск делает запрос и проверяет, что ответила страница.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_env_is_ignored() {
    if let Ok(addr) = std::env::var(CHILD_SITE) {
        let addr: SocketAddr = addr.parse().unwrap();
        let gw = gateway(config(&[addr.port()]), &[("site.test", addr)]);
        let f = get(&gw, &format!("http://site.test:{}/p", addr.port())).await;
        assert_eq!(
            f.result.unwrap().bytes,
            b"page",
            "ответила страница, не прокси"
        );
        return;
    }
    let mut page = serve(|p| match p {
        "/p" => text(200, "page"),
        _ => text(404, ""),
    })
    .await;
    let mut proxy = serve(|_| text(200, "proxied")).await;
    let exe = std::env::current_exe().unwrap();
    let proxy_url = format!("http://{}", proxy.addr);
    let site_addr = page.addr.to_string();
    let out = within(
        LONG * 3,
        "дочерний запуск теста",
        tokio::task::spawn_blocking(move || {
            std::process::Command::new(exe)
                .args(["proxy_env_is_ignored", "--exact", "--test-threads=1"])
                .env(CHILD_SITE, site_addr)
                .env("HTTP_PROXY", &proxy_url)
                .env("HTTPS_PROXY", &proxy_url)
                .env("ALL_PROXY", &proxy_url)
                .env("http_proxy", &proxy_url)
                .env("https_proxy", &proxy_url)
                .env("all_proxy", &proxy_url)
                .env_remove("NO_PROXY")
                .env_remove("no_proxy")
                .output()
                .unwrap()
        }),
    )
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "дочерний запуск: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("1 passed"), "{stdout}");
    assert!(proxy.paths().is_empty(), "прокси запросов не получил");
    assert_eq!(page.paths(), ["/robots.txt", "/p"]);
}
