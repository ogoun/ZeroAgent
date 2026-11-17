//! Транспорт клиента памяти (R1-M D133, D137): заголовки, редиректы, ошибки REST и MCP,
//! маска токена. Сервер — только поддельный ZAM на `127.0.0.1` (`test-support/fake_zam.rs`).

#[path = "../../../test-support/fake_zam.rs"]
mod fake_zam;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use fake_zam::{FakeZam, WAIT, within};
use serde_json::json;
use za_memory::ErrorKind::*;
use za_memory::{
    Ctx, ErrorKind, MemoryConfig, Method, Token, ZamClient, ZamError, parse_mcp_error,
};

const LIMIT: Duration = Duration::from_secs(10);

/// Хвост фиктивного токена: его не должно быть в `Debug` и логах.
const TAIL: &str = "Tr4nsp0rtTa1lQw8Er5Ty2Ui9Op3As6Df1Gh7";

/// Фиктивный токен — из частей во время выполнения (GitHub push protection).
fn token_raw() -> String {
    ["zam", "_", "abc", TAIL].concat()
}

fn client_with(cfg: MemoryConfig, token: Option<Token>) -> ZamClient {
    ZamClient::new(&cfg, token).unwrap()
}

fn client(url: &str) -> ZamClient {
    client_with(MemoryConfig::new(url), Some(Token::new(token_raw())))
}

#[test]
fn error_kind_table() {
    for (status, ty, kind) in [
        (400, "about:blank", BadRequest),
        (400, "/problems/by-required", BadRequest),
        (401, "about:blank", Auth),
        (403, "/problems/solution-access", Forbidden),
        (403, "/problems/idea-human-owned", Forbidden),
        (404, "/problems/session-not-found", NotFound),
        (408, "/problems/timeout", RequestTimeout),
        (409, "/problems/possible-duplicate", Conflict),
        (409, "/problems/already-answered", Conflict),
        (409, "/problems/idea-mode", Conflict),
        (428, "/problems/precondition-required", PreconditionRequired),
        (422, "/problems/idempotency-mismatch", Unprocessable),
        (422, "/problems/certainty", Unprocessable),
        (413, "/problems/too-large", TooLarge),
        (429, "/problems/rate-limited", RateLimited),
        (503, "/problems/busy", Busy),
        (503, "/problems/vault-locked", VaultLocked),
        (503, "about:blank", Server),
        (500, "about:blank", Server),
        (507, "about:blank", Server),
        (507, "/problems/insufficient-storage", Server),
        (400, "/problems/session-required", SessionRequired),
        (307, "about:blank", Protocol),
        (406, "about:blank", Protocol),
    ] {
        let body = json!({"type": ty, "title": "T", "status": status, "detail": "d", "code": "c1"});
        let e = ZamError::from_problem(
            status,
            "application/problem+json",
            body.to_string().as_bytes(),
            None,
        );
        assert_eq!(e.kind, kind, "{status} {ty}");
        assert_eq!(e.status, Some(status));
        assert_eq!(e.problem_type.as_deref(), Some(ty));
        assert_eq!(e.title, "T");
        assert_eq!(e.detail, "d");
        assert_eq!(e.code.as_deref(), Some("c1"));
        assert_eq!(
            ErrorKind::from_status(status, Some(ty)),
            kind,
            "{status} {ty}"
        );
    }
    // Тело не JSON (прокси, HTML): вид — по коду, текст HTML не цитируется.
    let e = ZamError::from_problem(502, "text/html", b"<html>bad gateway</html>", None);
    assert_eq!(e.kind, Server);
    assert!(!e.to_string().contains("<html>"), "{e}");
    assert_eq!(e.problem_type, None);
}

#[tokio::test]
async fn redirect_not_followed() {
    let a = FakeZam::start().await;
    let mut b = FakeZam::start().await;
    a.redirect_to(&b.url());
    let c = client(&a.url());
    let e = within(
        WAIT,
        "get",
        c.get_json("/api/v1/spaces", &Ctx::default(), LIMIT),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, Protocol);
    assert_eq!(e.title, "memory server redirected; not followed");
    let e = within(
        WAIT,
        "post",
        c.send_json(
            Method::POST,
            "/api/v1/recall",
            &json!({"event": "prompt"}),
            &Ctx::default(),
            None,
            None,
            LIMIT,
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, Protocol);
    // Второй сервер не получил ни одного запроса.
    assert!(b.seen().is_empty());
}

/// Буфер записей `tracing` теста.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn headers_sent_and_no_authorization_in_log() {
    let logs = Logs::default();
    let sub = tracing_subscriber::fmt()
        .with_writer({
            let logs = logs.clone();
            move || logs.clone()
        })
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(sub);

    let mut fake = FakeZam::start().await;
    fake.require_token(&token_raw());
    let cfg = MemoryConfig {
        machine: Some("pc-1".into()),
        ..MemoryConfig::new(&fake.url())
    };
    let c = client_with(cfg, Some(Token::new(token_raw())));
    let cx = Ctx {
        session: Some("za-0123456789abcdef-run"),
        solution: Some("01a1e164-0ff6-77dc-b4cb-84f08cc1f69e"),
        area: Some("core"),
    };
    within(WAIT, "get", c.get_json("/api/v1/spaces", &cx, LIMIT))
        .await
        .unwrap();
    let (status, _) = within(
        WAIT,
        "send",
        c.send_json(
            Method::POST,
            "/api/v1/recall",
            &json!({"event": "prompt"}),
            &cx,
            Some("idem-1"),
            Some(3),
            LIMIT,
        ),
    )
    .await
    .unwrap();
    assert_eq!(status, 200);
    // Ошибка 401 с неверным токеном тоже не несёт токена.
    let wrong = client_with(
        MemoryConfig::new(&fake.url()),
        Some(Token::new(["zam", "_", "wrong", TAIL].concat())),
    );
    let e = within(
        WAIT,
        "401",
        wrong.get_json("/api/v1/spaces", &Ctx::default(), LIMIT),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, Auth);
    assert!(!format!("{e} {e:?}").contains(TAIL));

    let seen = fake.seen();
    assert_eq!(seen.len(), 3, "{seen:?}");
    for s in &seen[..2] {
        assert!(s.authorized, "{s:?}");
        assert_eq!(
            s.header_str("x-zam-session").as_deref(),
            Some("za-0123456789abcdef-run")
        );
        assert_eq!(
            s.header_str("x-zam-solution").as_deref(),
            Some("01a1e164-0ff6-77dc-b4cb-84f08cc1f69e")
        );
        assert_eq!(s.header_str("x-zam-machine").as_deref(), Some("pc-1"));
        assert_eq!(s.header_str("x-zam-area").as_deref(), Some("core"));
        assert!(
            s.header_str("user-agent")
                .unwrap()
                .starts_with("zero-agent/"),
            "{s:?}"
        );
    }
    assert_eq!(
        seen[1].header_str("idempotency-key").as_deref(),
        Some("idem-1")
    );
    assert_eq!(seen[1].header_str("if-match").as_deref(), Some("3"));
    assert_eq!(seen[1].body, json!({"event": "prompt"}));
    // Без сессии, решения и области — заголовков нет, машина — всегда.
    assert!(seen[2].header("x-zam-session").is_none());
    assert!(seen[2].header("x-zam-area").is_none());
    assert!(seen[2].header("x-zam-solution").is_none());
    assert!(!seen[2].authorized);

    let shown = format!("{c:?}");
    assert!(!shown.contains(TAIL), "{shown}");
    assert!(shown.contains("zam_…"), "{shown}");
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(!text.contains(TAIL), "{text}");
}

#[tokio::test]
async fn area_and_machine_utf8_headers() {
    let mut fake = FakeZam::start().await;
    let cfg = MemoryConfig {
        machine: Some("ПК-1".into()),
        ..MemoryConfig::new(&fake.url())
    };
    let c = client_with(cfg, Some(Token::new(token_raw())));
    let cx = Ctx {
        area: Some("модуль-ядро"),
        ..Ctx::default()
    };
    within(WAIT, "get", c.get_json("/api/v1/spaces", &cx, LIMIT))
        .await
        .unwrap();
    let seen = fake.seen();
    assert_eq!(seen[0].header("x-zam-area"), Some("модуль-ядро".as_bytes()));
    assert_eq!(seen[0].header("x-zam-machine"), Some("ПК-1".as_bytes()));
    // Управляющий символ в области — ошибка до сети.
    let bad = Ctx {
        area: Some("a\nb"),
        ..Ctx::default()
    };
    let e = within(WAIT, "bad", c.get_json("/api/v1/spaces", &bad, LIMIT))
        .await
        .unwrap_err();
    assert_eq!(e.kind, BadRequest);
    assert!(fake.seen().is_empty());
}

#[tokio::test]
async fn mcp_sends_accept_and_session_arg() {
    let mut fake = FakeZam::start().await;
    fake.require_token(&token_raw());
    let c = client(&fake.url());
    let cx = Ctx {
        session: Some("za-s1"),
        ..Ctx::default()
    };
    for tool in ["memory_search", "memory_read", "memory_tree"] {
        let text = within(
            WAIT,
            tool,
            c.mcp_call(tool, json!({"query": "x"}), &cx, LIMIT),
        )
        .await
        .unwrap_or_else(|e| panic!("{tool}: {e}"));
        assert!(text.contains(tool), "{text}");
    }
    let seen = fake.seen();
    assert_eq!(seen.len(), 3);
    for s in &seen {
        assert_eq!(s.path, "/mcp");
        assert_eq!(
            s.header_str("accept").as_deref(),
            Some("application/json, text/event-stream")
        );
        assert_eq!(s.header_str("x-zam-session").as_deref(), Some("za-s1"));
        assert_eq!(s.body["method"], "tools/call");
        assert!(s.header("mcp-session-id").is_none());
    }
    assert_eq!(seen[0].body["params"]["arguments"]["session_id"], "za-s1");
    assert_eq!(seen[1].body["params"]["arguments"]["session_id"], "za-s1");
    assert!(
        seen[2].body["params"]["arguments"]
            .get("session_id")
            .is_none()
    );
    assert_eq!(seen[2].body["params"]["arguments"]["query"], "x");

    // Ошибка инструмента — HTTP 200 с `isError`: разбор по таблице форм.
    fake.mcp_error("Ошибка 404 Not Found: ресурс не найден\n~6 tok");
    let e = within(
        WAIT,
        "err",
        c.mcp_call("memory_read", json!({"ids": ["n:1"]}), &cx, LIMIT),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, NotFound);
    assert_eq!(e.status, Some(404));
    // Неизвестный инструмент — ошибка JSON-RPC.
    let e = within(
        WAIT,
        "rpc",
        c.mcp_call("memory_nosuch", json!({}), &cx, LIMIT),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, Protocol);
}

/// Без нужного `Accept` поддельный сервер, как ZAM, отвечает 406 — клиент его не получает.
#[tokio::test]
async fn fake_rejects_mcp_without_accept() {
    let fake = FakeZam::start().await;
    let host = fake.addr.to_string();
    // Сырой HTTP-запрос без `Accept`: проверка самой подделки, без клиента. Блокирующий
    // сокет — в `spawn_blocking`: поток рантайма остаётся серверу.
    let raw = tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(&host).unwrap();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_tree","arguments":{}}}"#;
        write!(
            s,
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    });
    let r = within(WAIT, "raw 406", raw).await.unwrap();
    assert!(r.starts_with("HTTP/1.1 406"), "{r}");
}

#[test]
fn mcp_is_error_forms() {
    let e = parse_mcp_error("Ошибка 400 Bad Request: передайте одну сессию\n~12 tok");
    assert_eq!(e.kind, BadRequest);
    assert_eq!(e.status, Some(400));
    assert_eq!(e.title, "Bad Request");
    assert_eq!(e.detail, "передайте одну сессию");

    let e = parse_mcp_error("Ошибка 409 Conflict: x (код conflict)\nтекущая ревизия 3\n~20 tok");
    assert_eq!(e.kind, Conflict);
    assert_eq!(e.status, Some(409));
    assert_eq!(e.code.as_deref(), Some("conflict"));
    assert!(e.detail.starts_with('x'), "{e:?}");
    assert!(e.detail.contains("текущая ревизия 3"), "{e:?}");
    assert!(!e.detail.contains("tok"), "{e:?}");

    // Код без подробности; многословный заголовок.
    let e = parse_mcp_error("Ошибка 422 Unprocessable Content (код evidence_required)\n~9 tok");
    assert_eq!(e.kind, Unprocessable);
    assert_eq!(e.title, "Unprocessable Content");
    assert_eq!(e.code.as_deref(), Some("evidence_required"));
    assert_eq!(e.detail, "");

    let e = parse_mcp_error("Ошибка аргументов: id обязателен\n~5 tok");
    assert_eq!(e.kind, BadRequest);
    assert_eq!(e.status, None);
    assert_eq!(e.detail, "id обязателен");

    let e = parse_mcp_error("Внутренняя ошибка MCP: boom\n~3 tok");
    assert_eq!(e.kind, Server);
    assert_eq!(e.detail, "boom");

    let e = parse_mcp_error("что-то иное");
    assert_eq!(e.kind, Protocol);
    assert_eq!(e.detail, "что-то иное");
    // Исходный текст — не длиннее 500 символов.
    let long = "я".repeat(2000);
    assert_eq!(parse_mcp_error(&long).detail.chars().count(), 500);
}

#[tokio::test]
async fn rate_limited_carries_retry_after() {
    let fake = FakeZam::start().await;
    fake.fail("/api/v1/spaces", 429, "/problems/rate-limited", Some(7));
    let c = client(&fake.url());
    let e = within(
        WAIT,
        "429",
        c.get_json("/api/v1/spaces", &Ctx::default(), LIMIT),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, RateLimited);
    assert_eq!(e.retry_after, Some(Duration::from_secs(7)));
    assert_eq!(e.status, Some(429));
}

/// Обрыв посреди запроса (M1 ревью задачи 2): сервер принял запрос целиком и закрыл
/// соединение без ответа, или оборвал тело ответа, — `Interrupted` («исход неизвестен»: запись
/// могла пройти), а не `Protocol`. «Не соединились» (закрытый порт) — по-прежнему `Offline`.
#[tokio::test]
async fn interrupted_mid_request_vs_offline() {
    let body = json!({"kind": "task", "title": "t"});
    let mut cut = fake_zam::CutServer::start(fake_zam::Cut::BeforeReply);
    let c = client(&cut.url());
    let e = within(
        WAIT,
        "cut before reply",
        c.send_json(
            Method::POST,
            "/api/v1/journal",
            &body,
            &Ctx::default(),
            Some("idem-cut-1"),
            None,
            LIMIT,
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, Interrupted, "{e:?}");
    assert_eq!(e.status, None);
    assert!(e.is_unreachable(), "{e:?}");
    // Запрос дошёл до сервера: это не «нет соединения».
    assert_eq!(cut.seen(), vec!["POST /api/v1/journal".to_owned()]);
    // MCP — тот же разбор.
    let e = within(
        WAIT,
        "cut mcp",
        c.mcp_call(
            "memory_search",
            json!({"query": "x"}),
            &Ctx::default(),
            LIMIT,
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, Interrupted, "{e:?}");
    assert_eq!(cut.seen(), vec!["POST /mcp".to_owned()]);

    // Обрыв тела ответа 200 (`Content-Length` больше отданного).
    let mut mid = fake_zam::CutServer::start(fake_zam::Cut::MidBody);
    let c = client(&mid.url());
    let e = within(
        WAIT,
        "cut mid body",
        c.get_json("/api/v1/spaces", &Ctx::default(), LIMIT),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, Interrupted, "{e:?}");
    assert_eq!(mid.seen(), vec!["GET /api/v1/spaces".to_owned()]);

    // Закрытый порт — не соединились: `Offline`, не `Interrupted`.
    let c = client(&fake_zam::closed_port_url());
    let e = within(
        WAIT,
        "offline",
        c.send_json(
            Method::POST,
            "/api/v1/journal",
            &body,
            &Ctx::default(),
            Some("idem-cut-2"),
            None,
            LIMIT,
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, Offline, "{e:?}");
    assert_eq!(ErrorKind::Interrupted.as_str(), "interrupted");
}

/// Нет соединения — `Offline`; срок вышел — `Timeout`; не JSON в ответе 2xx — `NotZam`.
#[tokio::test]
async fn offline_timeout_and_not_json() {
    let c = client(&fake_zam::closed_port_url());
    let e = within(
        WAIT,
        "offline",
        c.get_json("/api/v1/spaces", &Ctx::default(), LIMIT),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, Offline);
    // Сервер принимает соединение и молчит: срок запроса 200 мс.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", silent.local_addr().unwrap());
    let c = client(&url);
    let e = within(
        WAIT,
        "timeout",
        c.get_json(
            "/api/v1/spaces",
            &Ctx::default(),
            Duration::from_millis(200),
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, Timeout);
    drop(silent);
    // HTML на пути API (WebUI) — не ZAM.
    let fake = FakeZam::start().await;
    let c = client(&fake.url());
    let e = within(
        WAIT,
        "html",
        c.get_json("/api/v2/nothing-here", &Ctx::default(), LIMIT),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, NotFound, "{e:?}");
    let e = within(WAIT, "html", c.get_json("/health", &Ctx::default(), LIMIT))
        .await
        .unwrap_err();
    assert_eq!(e.kind, NotZam);
}

#[test]
fn token_debug_is_masked() {
    let t = Token::new(["zam", "_", "abcd1234efgh"].concat());
    assert!(!format!("{t:?}").contains("abcd1234efgh"));
    assert!(format!("{t:?}").contains("zam_…"));
}

/// Адрес — `http`/`https` с хостом, без учётных данных, запроса и фрагмента; машина —
/// до 128 печатных символов; ошибки не показывают значения.
#[test]
fn config_is_checked() {
    for bad in [
        "ftp://h:1",
        "http://",
        "not a url",
        "http://u:p@h:7700",
        "http://h:7700/?q=1",
        "http://h:7700/#f",
    ] {
        let e = ZamClient::new(&MemoryConfig::new(bad), None).unwrap_err();
        assert!(e.to_string().starts_with("memory.url"), "{bad}: {e}");
        assert!(!e.to_string().contains("u:p"), "{e}");
    }
    assert!(ZamClient::new(&MemoryConfig::new("http://h:7700/zam/"), None).is_ok());
    for bad in ["a\nb".to_owned(), "x".repeat(129), String::new()] {
        let cfg = MemoryConfig {
            machine: Some(bad.clone()),
            ..MemoryConfig::new("http://h:7700")
        };
        let e = ZamClient::new(&cfg, None).unwrap_err();
        assert!(e.to_string().starts_with("memory.machine"), "{bad:?}: {e}");
    }
    let ok = MemoryConfig {
        machine: Some("я".repeat(128)),
        ..MemoryConfig::new("http://h:7700")
    };
    assert!(ZamClient::new(&ok, None).is_ok());
    // Токен с переводом строки в заголовок не встанет.
    let e = ZamClient::new(
        &MemoryConfig::new("http://h:7700"),
        Some(Token::new("zam_x\ny".into())),
    )
    .unwrap_err();
    assert!(!e.to_string().contains("zam_x"), "{e}");
}

/// Ручные часы (D165): сон просыпается только от `advance`, без таймеров.
#[cfg(feature = "test-hooks")]
#[tokio::test]
async fn manual_clock_wakes_on_advance() {
    use za_memory::{Clock, ManualClock};
    let clock = Arc::new(ManualClock::new());
    let t0 = clock.now();
    let c2 = clock.clone();
    let sleeper = tokio::spawn(async move { c2.sleep(Duration::from_secs(30)).await });
    within(WAIT, "sleeper", clock.wait_sleepers(1)).await;
    clock.advance(Duration::from_secs(29));
    assert!(!sleeper.is_finished());
    assert_eq!(clock.sleepers(), 1);
    clock.advance(Duration::from_secs(1));
    within(WAIT, "wake", sleeper).await.unwrap();
    assert_eq!(clock.now() - t0, Duration::from_secs(30));
    // Нулевой сон — сразу.
    within(WAIT, "zero", clock.sleep(Duration::ZERO)).await;
}
