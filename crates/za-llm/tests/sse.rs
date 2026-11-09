//! HTTP-транспорт, разбор SSE, классификация ошибок, лимиты, секреты (задача 3).

mod support;

use std::time::{Duration, SystemTime};

use serde_json::json;
use support::{FakeServer, Reply, fixture, split_every};
use za_llm::ErrorClass;
use za_llm::http::{
    HeaderMap, HeaderValue, HttpTransport, Secret, SseEvent, SseParser, Timeouts, classify_status,
    parse_rate_limits, parse_retry_after, redact, secret_env_name,
};

const ROUTE: &str = "POST /v1/chat/completions";

fn short(idle_ms: u64, first_byte_ms: u64) -> Timeouts {
    Timeouts {
        connect: Duration::from_secs(2),
        first_byte: Duration::from_millis(first_byte_ms),
        idle: Duration::from_millis(idle_ms),
    }
}

fn parse_all(chunks: &[Vec<u8>]) -> Vec<SseEvent> {
    let mut p = SseParser::new();
    let mut out = Vec::new();
    for c in chunks {
        out.extend(p.push(c));
    }
    out.extend(p.finish());
    out
}

#[test]
fn cyrillic_split_across_chunks_is_intact() {
    let bytes = fixture("sse/cyrillic.sse");
    assert_eq!(bytes, "data: {\"t\":\"Привет, мир\"}\n\n".as_bytes());
    for n in 1..=7 {
        let ev = parse_all(&split_every(&bytes, n));
        assert_eq!(ev.len(), 1, "n = {n}");
        assert_eq!(ev[0].data, "{\"t\":\"Привет, мир\"}", "n = {n}");
    }
}

#[test]
fn crlf_and_multiline_data() {
    let bytes = fixture("sse/crlf_multiline.sse");
    assert_eq!(bytes, b"data: a\r\ndata: b\r\n\r\n");
    // В том числе граница чанка между `\r` и `\n`.
    for n in 1..=bytes.len() {
        let ev = parse_all(&split_every(&bytes, n));
        assert_eq!(ev.len(), 1, "n = {n}");
        assert_eq!(ev[0].data, "a\nb", "n = {n}");
    }
}

#[test]
fn bare_cr_is_line_end() {
    let ev = parse_all(&[b"data: a\rdata: b\r\r".to_vec()]);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].data, "a\nb");
}

#[test]
fn comments_and_event_field() {
    let mut p = SseParser::new();
    let ev = p.push(b": ping\n\nevent: message_start\ndata: {}\n\n");
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].event.as_deref(), Some("message_start"));
    assert_eq!(ev[0].data, "{}");
    assert_eq!(p.finish(), None);
}

#[test]
fn done_marker_is_plain_event() {
    let mut p = SseParser::new();
    let ev = p.push(b"data: [DONE]\n\n");
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].data, "[DONE]");
}

#[test]
fn finish_flushes_unterminated_event() {
    let mut p = SseParser::new();
    assert!(p.push(b"data: x").is_empty());
    assert_eq!(
        p.finish(),
        Some(SseEvent {
            event: None,
            data: "x".into(),
            id: None
        })
    );
    assert_eq!(p.finish(), None);
}

#[tokio::test]
async fn invalid_utf8_is_malformed() {
    let srv = FakeServer::start(vec![(
        ROUTE,
        vec![Reply::Raw(
            200,
            vec![("content-type".into(), "text/event-stream".into())],
            b"data: \xff\n\n".to_vec(),
        )],
    )])
    .await;
    let t = HttpTransport::local(short(2000, 2000));
    let mut resp = t
        .post_sse(
            &srv.url("/v1/chat/completions"),
            HeaderMap::new(),
            &json!({}),
            &[],
        )
        .await
        .unwrap();
    let err = resp.next_event().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Malformed, "{err:?}");
}

#[test]
fn retry_after_seconds_and_date() {
    // 2026-10-21T07:27:50Z
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_792_567_670);
    assert_eq!(parse_retry_after("7", now), Some(Duration::from_secs(7)));
    assert_eq!(
        parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT", now),
        Some(Duration::from_secs(10))
    );
    assert_eq!(parse_retry_after("мусор", now), None);
}

#[test]
fn status_classification() {
    let empty = HeaderMap::new();
    let cases: &[(u16, &str, ErrorClass)] = &[
        (401, "", ErrorClass::Auth),
        (403, "", ErrorClass::Auth),
        (404, "", ErrorClass::NotFound),
        (408, "", ErrorClass::Timeout),
        (503, "", ErrorClass::Overloaded),
        (529, "", ErrorClass::Overloaded),
        (500, "", ErrorClass::Server),
        (502, "", ErrorClass::Server),
        (504, "", ErrorClass::Server),
        (501, "", ErrorClass::BadRequest),
        (
            400,
            r#"{"error":{"type":"exceed_context_size_error"}}"#,
            ErrorClass::ContextOverflow,
        ),
        (
            400,
            "This model's maximum context length is 8192 tokens",
            ErrorClass::ContextOverflow,
        ),
        (400, "bad field", ErrorClass::BadRequest),
    ];
    for &(status, body, class) in cases {
        let e = classify_status(status, &empty, body);
        assert_eq!(e.class, class, "{status} {body}");
        assert_eq!(e.http_status, Some(status));
    }
    let e = classify_status(501, &empty, "");
    assert!(!e.class.retryable() && !e.class.counts_for_breaker());

    let mut h = HeaderMap::new();
    h.insert("retry-after", HeaderValue::from_static("3"));
    let e = classify_status(429, &h, "");
    assert_eq!(e.class, ErrorClass::RateLimited);
    assert_eq!(e.retry_after, Some(Duration::from_secs(3)));
    assert_eq!(e.http_status, Some(429));
}

#[test]
fn rate_limit_headers() {
    let mut h = HeaderMap::new();
    h.insert(
        "x-ratelimit-remaining-requests",
        HeaderValue::from_static("0"),
    );
    h.insert(
        "x-ratelimit-reset-requests",
        HeaderValue::from_static("6m0s"),
    );
    let l = parse_rate_limits(&h);
    assert_eq!(l.remaining_requests, Some(0));
    assert_eq!(l.reset_after, Some(Duration::from_secs(360)));

    let mut h = HeaderMap::new();
    h.insert(
        "anthropic-ratelimit-requests-remaining",
        HeaderValue::from_static("3"),
    );
    assert_eq!(parse_rate_limits(&h).remaining_requests, Some(3));

    let l = parse_rate_limits(&HeaderMap::new());
    assert_eq!(l.remaining_requests, None);
    assert_eq!(l.remaining_tokens, None);
    assert_eq!(l.reset_after, None);
}

#[test]
fn secret_never_printed() {
    let s = Secret::new("sk-live-123".into());
    assert_eq!(format!("{s:?}"), "***");
    assert_eq!(redact("echo: sk-live-123", &[&s]), "echo: ***");
    assert_eq!(s.expose(), "sk-live-123");
    assert_eq!(secret_env_name("open-router"), "ZA_SECRET_OPEN_ROUTER");
}

#[test]
fn redact_truncates_to_2_kib() {
    let long = "я".repeat(3000); // 6000 байт
    let r = redact(&long, &[]);
    assert!(r.len() <= 2048 + "…".len(), "{}", r.len());
    assert!(r.starts_with("яя"));
}

#[tokio::test]
async fn error_body_echoing_key_is_redacted() {
    let key = "sk-live-123";
    let srv = FakeServer::start(vec![(
        ROUTE,
        vec![Reply::Json(
            400,
            json!({"error": {"message": format!("bad header: Bearer {key}")}}),
        )],
    )])
    .await;
    let s = Secret::new(key.into());
    let mut h = HeaderMap::new();
    h.insert("authorization", s.header_value("Bearer ").unwrap());
    let t = HttpTransport::cloud(short(2000, 2000));
    let err = t
        .post_sse(&srv.url("/v1/chat/completions"), h, &json!({}), &[&s])
        .await
        .err()
        .unwrap();
    assert_eq!(err.class, ErrorClass::BadRequest);
    assert!(!err.message.contains(key), "{}", err.message);
    assert!(!format!("{err:?}").contains(key));
    assert!(err.message.contains("***"), "{}", err.message);
    // Ключ всё же ушёл в заголовке запроса.
    let reqs = srv.requests(ROUTE);
    assert_eq!(reqs[0].header("authorization"), Some("Bearer sk-live-123"));
}

#[tokio::test]
async fn idle_timeout_is_timeout_class() {
    let srv = FakeServer::start(vec![(
        ROUTE,
        vec![Reply::Sse {
            chunks: vec![b"data: 1\n\n".to_vec()],
            delay: Duration::ZERO,
            cut_after: None,
            then_hang: true,
        }],
    )])
    .await;
    let t = HttpTransport::local(short(200, 2000));
    let mut resp = t
        .post_sse(
            &srv.url("/v1/chat/completions"),
            HeaderMap::new(),
            &json!({}),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(resp.next_event().await.unwrap().unwrap().data, "1");
    let err = resp.next_event().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Timeout, "{err:?}");
}

#[tokio::test]
async fn first_byte_timeout_is_timeout_class() {
    let srv = FakeServer::start(vec![(ROUTE, vec![Reply::Hang])]).await;
    let t = HttpTransport::local(short(2000, 200));
    let started = std::time::Instant::now();
    let err = t
        .post_sse(
            &srv.url("/v1/chat/completions"),
            HeaderMap::new(),
            &json!({}),
            &[],
        )
        .await
        .err()
        .unwrap();
    assert_eq!(err.class, ErrorClass::Timeout, "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn cut_connection_is_stream_cut() {
    let srv = FakeServer::start(vec![(
        ROUTE,
        vec![Reply::Sse {
            chunks: vec![
                b"data: 1\n\n".to_vec(),
                b"data: 2\n\n".to_vec(),
                b"data: 3\n\n".to_vec(),
            ],
            delay: Duration::from_millis(20),
            cut_after: Some(2),
            then_hang: false,
        }],
    )])
    .await;
    let t = HttpTransport::local(short(2000, 2000));
    let mut resp = t
        .post_sse(
            &srv.url("/v1/chat/completions"),
            HeaderMap::new(),
            &json!({}),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(resp.next_event().await.unwrap().unwrap().data, "1");
    assert_eq!(resp.next_event().await.unwrap().unwrap().data, "2");
    let err = resp.next_event().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::StreamCut, "{err:?}");
    assert_eq!(srv.disconnects(ROUTE), 0);
}

#[tokio::test]
async fn clean_end_of_stream_is_none() {
    let srv = FakeServer::start(vec![(
        ROUTE,
        vec![Reply::sse(vec![b"data: 1\n\ndata: [DONE]\n\n".to_vec()])],
    )])
    .await;
    let t = HttpTransport::local(short(2000, 2000));
    let mut resp = t
        .post_sse(
            &srv.url("/v1/chat/completions"),
            HeaderMap::new(),
            &json!({"x": 1}),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(resp.next_event().await.unwrap().unwrap().data, "1");
    assert_eq!(resp.next_event().await.unwrap().unwrap().data, "[DONE]");
    assert_eq!(resp.next_event().await.unwrap(), None);
    let reqs = srv.requests(ROUTE);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].json(), json!({"x": 1}));
    assert_eq!(srv.total_requests(), 1);
}

#[tokio::test]
async fn fake_server_counts_disconnects() {
    let chunks = (0..10)
        .map(|i| format!("data: {i}\n\n").into_bytes())
        .collect();
    let srv = FakeServer::start(vec![(
        ROUTE,
        vec![Reply::Sse {
            chunks,
            delay: Duration::from_millis(50),
            cut_after: None,
            then_hang: false,
        }],
    )])
    .await;
    let t = HttpTransport::local(short(2000, 2000));
    let mut resp = t
        .post_sse(
            &srv.url("/v1/chat/completions"),
            HeaderMap::new(),
            &json!({}),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(resp.next_event().await.unwrap().unwrap().data, "0");
    drop(resp);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while srv.disconnects(ROUTE) == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(srv.disconnects(ROUTE), 1);
}

#[tokio::test]
async fn json_requests_and_limits() {
    let srv = FakeServer::start(vec![
        (
            "GET /v1/models",
            vec![Reply::Json(200, json!({"data": []}))],
        ),
        (
            "POST /v1/x",
            vec![
                Reply::JsonWithHeaders(
                    200,
                    vec![("x-ratelimit-remaining-tokens".into(), "42".into())],
                    json!({"ok": true}),
                ),
                Reply::Json(529, json!({"error": "overloaded"})),
            ],
        ),
    ])
    .await;
    let t = HttpTransport::cloud(short(2000, 2000));
    let v = t
        .get_json(&srv.url("/v1/models"), HeaderMap::new(), &[])
        .await
        .unwrap();
    assert_eq!(v, json!({"data": []}));
    let (v, h) = t
        .post_json(&srv.url("/v1/x"), HeaderMap::new(), &json!({}), &[])
        .await
        .unwrap();
    assert_eq!(v, json!({"ok": true}));
    assert_eq!(parse_rate_limits(&h).remaining_tokens, Some(42));
    let err = t
        .post_json(&srv.url("/v1/x"), HeaderMap::new(), &json!({}), &[])
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Overloaded);
    // Последний ответ повторяется.
    let err = t
        .post_json(&srv.url("/v1/x"), HeaderMap::new(), &json!({}), &[])
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Overloaded);
    assert_eq!(srv.total_requests(), 4);
}

#[tokio::test]
async fn connection_refused_is_transport() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let t = HttpTransport::local(Timeouts::local());
    let err = t
        .get_json(&format!("http://127.0.0.1:{port}/x"), HeaderMap::new(), &[])
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Transport, "{err:?}");
}

#[test]
fn timeout_presets() {
    let s = Duration::from_secs;
    assert_eq!(
        Timeouts::local(),
        Timeouts {
            connect: s(2),
            first_byte: s(300),
            idle: s(90)
        }
    );
    assert_eq!(
        Timeouts::cloud(),
        Timeouts {
            connect: s(10),
            first_byte: s(120),
            idle: s(90)
        }
    );
    assert_eq!(
        Timeouts::probe_local(),
        Timeouts {
            connect: s(2),
            first_byte: s(5),
            idle: s(5)
        }
    );
    assert_eq!(
        Timeouts::probe_cloud(),
        Timeouts {
            connect: s(10),
            first_byte: s(10),
            idle: s(10)
        }
    );
}

/// Тело кончилось штатно, но посреди строки SSE: это обрыв, а не событие.
#[tokio::test]
async fn clean_eof_mid_line_is_stream_cut() {
    let srv = FakeServer::start(vec![(
        ROUTE,
        vec![Reply::Raw(
            200,
            vec![("content-type".into(), "text/event-stream".into())],
            "data: 1\n\ndata: {\"t\":\"при".as_bytes().to_vec(),
        )],
    )])
    .await;
    let t = HttpTransport::local(short(2000, 2000));
    let mut resp = t
        .post_sse(
            &srv.url("/v1/chat/completions"),
            HeaderMap::new(),
            &json!({}),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(resp.next_event().await.unwrap().unwrap().data, "1");
    let err = resp.next_event().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::StreamCut, "{err:?}");
    assert!(err.class.retryable());
    assert_eq!(resp.next_event().await.unwrap(), None);
}

/// Ключ в query: ошибка чтения стрима его не показывает.
#[tokio::test]
async fn stream_error_hides_key_in_url() {
    let key = "sk-live-123";
    let srv = FakeServer::start(vec![(
        ROUTE,
        vec![Reply::Sse {
            chunks: vec![b"data: 1\n\n".to_vec(), b"data: 2\n\n".to_vec()],
            delay: Duration::ZERO,
            cut_after: Some(1),
            then_hang: false,
        }],
    )])
    .await;
    let s = Secret::new(key.into());
    let t = HttpTransport::cloud(short(2000, 2000));
    let mut resp = t
        .post_sse(
            &srv.url(&format!("/v1/chat/completions?key={key}")),
            HeaderMap::new(),
            &json!({}),
            &[&s],
        )
        .await
        .unwrap();
    assert_eq!(resp.next_event().await.unwrap().unwrap().data, "1");
    let err = resp.next_event().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::StreamCut, "{err:?}");
    assert!(!err.message.contains(key), "{}", err.message);
    assert!(srv.requests(ROUTE)[0].path_and_query.contains(key));
}

/// 200 с JSON вместо стрима (прокси, сервер без `stream`) — `Malformed`.
#[tokio::test]
async fn json_instead_of_stream_is_malformed() {
    let srv = FakeServer::start(vec![(ROUTE, vec![Reply::Json(200, json!({"ok": 1}))])]).await;
    let t = HttpTransport::local(short(2000, 2000));
    let err = t
        .post_sse(
            &srv.url("/v1/chat/completions"),
            HeaderMap::new(),
            &json!({}),
            &[],
        )
        .await
        .err()
        .unwrap();
    assert_eq!(err.class, ErrorClass::Malformed, "{err:?}");
    assert!(err.message.contains("text/event-stream"), "{}", err.message);
}

/// Сервер на голом TCP: читает начало запроса (ClientHello) и отвечает `reply`.
async fn raw_tcp_server(reply: &'static [u8]) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(reply).await;
                let _ = s.flush().await;
                tokio::time::sleep(Duration::from_millis(200)).await;
            });
        }
    });
    port
}

/// Ошибки TLS при соединении (рукопожатие отвергнуто, `https://` на порт без TLS) —
/// `Auth`, не `Transport`: повтор их не лечит, предохранитель выключает эндпоинт
/// (задача 10, отложено из ревью задачи 3).
#[tokio::test]
async fn tls_handshake_errors_are_not_retryable() {
    // Запись TLS alert: fatal (2), handshake_failure (40).
    let alert = raw_tcp_server(&[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28]).await;
    let plain = raw_tcp_server(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n").await;
    for port in [alert, plain] {
        let t = HttpTransport::local(short(2000, 2000));
        let err = t
            .post_sse(
                &format!("https://127.0.0.1:{port}/v1/chat/completions"),
                HeaderMap::new(),
                &json!({}),
                &[],
            )
            .await
            .err()
            .expect("ожидалась ошибка TLS");
        assert_eq!(err.class, ErrorClass::Auth, "{err:?}");
        assert!(err.message.starts_with("TLS: "), "{err:?}");
        assert!(!err.class.retryable() && !err.class.counts_for_breaker());
    }
}
