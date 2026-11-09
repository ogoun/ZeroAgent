//! Поддельный HTTP-сервер (axum) для тестов транспорта и коннекторов.
//!
//! Маршрут задаётся строкой `"МЕТОД /путь"`; ответы маршрута отдаются по очереди,
//! последний повторяется. Сервер записывает запросы и считает ответы SSE, которые
//! клиент бросил до последнего чанка (счётчик в `Drop` тела ответа).

// Каждый тестовый крейт пользуется своей частью поддержки.
#![allow(dead_code)]

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use http_body::Frame;
use serde_json::Value;
use tokio::sync::mpsc;

/// Множитель времени тестов в реальном времени: `ZA_TEST_TIME_SCALE` (целое 1–8,
/// по умолчанию 1; в CI — 4). Умножаются все длительности теста — пределы ожидания,
/// таймауты, паузы сервера и окна проверок, — поэтому их соотношения не меняются, а
/// запас на медленной машине растёт. Предел 8: задержки поддельного сервера при зонде
/// (до 600 мс) должны оставаться под пределом запроса зонда в коде (5 с), который
/// множитель не меняет.
pub fn time_scale() -> u32 {
    static SCALE: OnceLock<u32> = OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("ZA_TEST_TIME_SCALE")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map_or(1, |n| n.clamp(1, 8))
    })
}

/// Длительность с множителем [`time_scale`].
pub fn scaled(d: Duration) -> Duration {
    d * time_scale()
}

/// Ответ поддельного сервера.
#[derive(Debug, Clone)]
pub enum Reply {
    Json(u16, Value),
    JsonWithHeaders(u16, Vec<(String, String)>, Value),
    /// Стрим `text/event-stream`: чанки с паузой `delay` между ними.
    /// `cut_after: Some(k)` — после k чанков соединение рвётся;
    /// `then_hang` — после последнего чанка ответ висит без конца.
    Sse {
        chunks: Vec<Vec<u8>>,
        delay: Duration,
        cut_after: Option<usize>,
        then_hang: bool,
    },
    Raw(u16, Vec<(String, String)>, Vec<u8>),
    /// Заголовков нет вовсе: обработчик не отвечает никогда.
    Hang,
    /// Тот же ответ после паузы (заголовки приходят не сразу).
    Delayed(Duration, Box<Reply>),
}

impl Reply {
    /// SSE без паузы, обрыва и зависания.
    pub fn sse(chunks: Vec<Vec<u8>>) -> Self {
        Reply::Sse {
            chunks,
            delay: Duration::ZERO,
            cut_after: None,
            then_hang: false,
        }
    }
}

/// Записанный запрос.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub path_and_query: String,
    /// Имена заголовков в нижнем регистре.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("тело запроса не JSON")
    }
}

struct Route {
    replies: Vec<Reply>,
    served: usize,
    requests: Vec<RecordedRequest>,
    disconnects: Arc<AtomicUsize>,
}

#[derive(Default)]
struct State {
    routes: HashMap<String, Route>,
    total: usize,
    unauthorized: usize,
    /// `Some(k)` — каждый запрос без `Authorization: Bearer k` получает 401 (как
    /// llama-server с `--api-key`) и в маршрут не записывается.
    bearer: Option<String>,
}

/// Поддельный сервер на `127.0.0.1:<свободный порт>`; живёт до конца теста.
pub struct FakeServer {
    pub base_url: String,
    state: Arc<Mutex<State>>,
}

impl FakeServer {
    pub async fn start(routes: Vec<(&'static str, Vec<Reply>)>) -> Self {
        Self::start_inner(routes, None).await
    }

    /// Сервер, требующий `Authorization: Bearer <key>` на всех маршрутах.
    pub async fn start_with_bearer(routes: Vec<(&'static str, Vec<Reply>)>, key: &str) -> Self {
        Self::start_inner(routes, Some(key.to_owned())).await
    }

    /// Запросы, отвергнутые с 401 из-за отсутствия или неверного ключа.
    pub fn unauthorized(&self) -> usize {
        self.state.lock().unwrap().unauthorized
    }

    async fn start_inner(routes: Vec<(&'static str, Vec<Reply>)>, bearer: Option<String>) -> Self {
        let mut st = State {
            bearer,
            ..State::default()
        };
        for (key, replies) in routes {
            assert!(!replies.is_empty(), "маршрут {key} без ответов");
            st.routes.insert(
                key.to_owned(),
                Route {
                    replies,
                    served: 0,
                    requests: Vec::new(),
                    disconnects: Arc::new(AtomicUsize::new(0)),
                },
            );
        }
        let state = Arc::new(Mutex::new(st));
        let handler_state = state.clone();
        let app = Router::new().fallback(move |req: Request| {
            let state = handler_state.clone();
            async move { handle(state, req).await }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        FakeServer {
            base_url: format!("http://{addr}"),
            state,
        }
    }

    /// Адрес маршрута: `base_url` + путь.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    pub fn requests(&self, route: &str) -> Vec<RecordedRequest> {
        let st = self.state.lock().unwrap();
        st.routes
            .get(route)
            .map(|r| r.requests.clone())
            .unwrap_or_default()
    }

    /// Все запросы, в том числе к неизвестным маршрутам.
    pub fn total_requests(&self) -> usize {
        self.state.lock().unwrap().total
    }

    /// Ответы SSE маршрута, тело которых уничтожено до последнего чанка.
    pub fn disconnects(&self, route: &str) -> usize {
        let st = self.state.lock().unwrap();
        st.routes
            .get(route)
            .map(|r| r.disconnects.load(Ordering::SeqCst))
            .unwrap_or(0)
    }
}

async fn handle(state: Arc<Mutex<State>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let key = format!("{} {}", parts.method, parts.uri.path());
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_default();
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(n, v)| {
            (
                n.as_str().to_owned(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let body = axum::body::to_bytes(body, 64 * 1024 * 1024)
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();

    let picked = {
        let mut st = state.lock().unwrap();
        st.total += 1;
        let denied = st
            .bearer
            .as_ref()
            .is_some_and(|k| !headers_ok(&headers, &format!("Bearer {k}")));
        if denied {
            st.unauthorized += 1;
            return respond(
                401,
                &[],
                Body::from(
                    r#"{"error":{"code":401,"message":"Invalid API Key","type":"authentication_error"}}"#,
                ),
            );
        }
        st.routes.get_mut(&key).map(|r| {
            r.requests.push(RecordedRequest {
                path_and_query,
                headers,
                body,
            });
            let i = r.served.min(r.replies.len() - 1);
            r.served += 1;
            (r.replies[i].clone(), r.disconnects.clone())
        })
    };
    let Some((reply, disconnects)) = picked else {
        return respond(404, &[], Body::from(r#"{"error":"нет маршрута"}"#));
    };
    let mut reply = reply;
    while let Reply::Delayed(d, inner) = reply {
        tokio::time::sleep(d).await;
        reply = *inner;
    }
    match reply {
        Reply::Json(status, v) => json(status, &[], &v),
        Reply::JsonWithHeaders(status, h, v) => json(status, &h, &v),
        Reply::Raw(status, h, bytes) => respond(status, &h, Body::from(bytes)),
        Reply::Hang => std::future::pending::<Response>().await,
        Reply::Delayed(..) => unreachable!("пауза снята выше"),
        Reply::Sse {
            chunks,
            delay,
            cut_after,
            then_hang,
        } => {
            let (tx, rx) = mpsc::channel(1);
            tokio::spawn(feed(chunks, delay, cut_after, then_hang, tx));
            let body = SseBody {
                rx,
                complete: false,
                disconnects,
            };
            respond(
                200,
                &[("content-type".into(), "text/event-stream".into())],
                Body::new(body),
            )
        }
    }
}

fn headers_ok(headers: &[(String, String)], want: &str) -> bool {
    headers
        .iter()
        .any(|(n, v)| n.eq_ignore_ascii_case("authorization") && v == want)
}

fn json(status: u16, headers: &[(String, String)], v: &Value) -> Response {
    let mut h = vec![("content-type".to_owned(), "application/json".to_owned())];
    h.extend_from_slice(headers);
    respond(status, &h, Body::from(serde_json::to_vec(v).unwrap()))
}

fn respond(status: u16, headers: &[(String, String)], body: Body) -> Response {
    let mut resp = Response::new(body);
    *resp.status_mut() = StatusCode::from_u16(status).unwrap();
    for (n, v) in headers {
        resp.headers_mut().insert(
            HeaderName::from_bytes(n.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    resp
}

/// Элемент тела SSE от генератора.
enum Item {
    Chunk {
        bytes: Bytes,
        last: bool,
    },
    /// Оборвать соединение.
    Cut,
}

/// Генератор чанков: пауза `delay` перед каждым чанком, кроме первого.
async fn feed(
    chunks: Vec<Vec<u8>>,
    delay: Duration,
    cut_after: Option<usize>,
    then_hang: bool,
    tx: mpsc::Sender<Item>,
) {
    let n = chunks.len();
    for (i, c) in chunks.into_iter().enumerate() {
        if cut_after == Some(i) {
            let _ = tx.send(Item::Cut).await;
            return;
        }
        if i > 0 && !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let item = Item::Chunk {
            bytes: Bytes::from(c),
            last: i + 1 == n,
        };
        if tx.send(item).await.is_err() {
            return;
        }
    }
    if then_hang {
        // Тело висит, пока клиент не уйдёт.
        tx.closed().await;
    }
}

/// Тело ответа SSE. В `Drop` до последнего чанка считает отключение клиента.
struct SseBody {
    rx: mpsc::Receiver<Item>,
    /// Последний чанк отдан или сервер сам оборвал ответ.
    complete: bool,
    disconnects: Arc<AtomicUsize>,
}

impl HttpBody for SseBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        match self.rx.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                self.complete = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(Item::Chunk { bytes, last })) => {
                if last {
                    self.complete = true;
                }
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            Poll::Ready(Some(Item::Cut)) => {
                self.complete = true;
                Poll::Ready(Some(Err(std::io::Error::other("обрыв соединения"))))
            }
        }
    }
}

impl Drop for SseBody {
    fn drop(&mut self) {
        if !self.complete {
            self.disconnects.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Байты фикстуры из `tests/fixtures/<path>`.
pub fn fixture(path: &str) -> Vec<u8> {
    let full = format!("{}/tests/fixtures/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&full).unwrap_or_else(|e| panic!("фикстура {full}: {e}"))
}

/// Режет байты на чанки по `n` байт, в том числе посреди символа UTF-8.
pub fn split_every(bytes: &[u8], n: usize) -> Vec<Vec<u8>> {
    assert!(n > 0);
    bytes.chunks(n).map(<[u8]>::to_vec).collect()
}
