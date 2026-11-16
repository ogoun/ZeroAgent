//! Поддельный HTTP-сервер (axum на `127.0.0.1:0`) для тестов шлюза (D121).
//!
//! Сервер шлёт в канал каждый принятый запрос (путь с запросом и заголовки): тест
//! «подключения нет» проверяет пустой канал после завершения вызова, а не таймер.

#![allow(dead_code)]

pub mod extractor;

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use http_body::Frame;
use tokio::sync::mpsc;

/// Множитель времени тестов в реальном времени: `ZA_TEST_TIME_SCALE` (целое 1–6,
/// по умолчанию 1; в CI — 4).
pub fn time_scale() -> u32 {
    static SCALE: OnceLock<u32> = OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("ZA_TEST_TIME_SCALE")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map_or(1, |n| n.clamp(1, 6))
    })
}

/// Длительность с множителем [`time_scale`].
pub fn scaled(d: Duration) -> Duration {
    d * time_scale()
}

/// Ожидание под пределом `d` × [`time_scale`] с понятной паникой.
pub async fn within<F: Future>(d: Duration, what: &str, f: F) -> F::Output {
    let d = scaled(d);
    tokio::time::timeout(d, f)
        .await
        .unwrap_or_else(|_| panic!("{what}: не завершилось за {d:?}"))
}

/// Принятый сервером запрос.
#[derive(Debug, Clone)]
pub struct Seen {
    /// Путь с запросом (`/a?b=c`).
    pub path: String,
    pub headers: HeaderMap,
}

/// Запущенный поддельный сервер.
pub struct Server {
    pub addr: SocketAddr,
    pub rx: mpsc::UnboundedReceiver<Seen>,
}

impl Server {
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Все принятые к этому моменту запросы (канал опустошается).
    pub fn seen(&mut self) -> Vec<Seen> {
        let mut out = Vec::new();
        while let Ok(s) = self.rx.try_recv() {
            out.push(s);
        }
        out
    }

    /// Пути принятых запросов (канал опустошается).
    pub fn paths(&mut self) -> Vec<String> {
        self.seen().into_iter().map(|s| s.path).collect()
    }
}

/// Сервер, отвечающий `route(путь)` на любой запрос.
pub async fn serve<F>(route: F) -> Server
where
    F: Fn(&str) -> Response + Send + Sync + 'static,
{
    serve_with(move |p, _| route(p)).await
}

/// Сервер, отвечающий `route(путь, заголовки запроса)` на любой запрос.
pub async fn serve_with<F>(route: F) -> Server
where
    F: Fn(&str, &HeaderMap) -> Response + Send + Sync + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    let route = Arc::new(route);
    let app = Router::new().fallback(move |req: Request| {
        let tx = tx.clone();
        let route = route.clone();
        async move {
            let path = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str().to_string())
                .unwrap_or_default();
            let _ = tx.send(Seen {
                path: path.clone(),
                headers: req.headers().clone(),
            });
            route(&path, req.headers())
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server { addr, rx }
}

/// Ответ с кодом и текстом.
pub fn text(status: u16, body: &str) -> Response {
    let mut r = Response::new(Body::from(body.to_string()));
    *r.status_mut() = StatusCode::from_u16(status).unwrap();
    r
}

/// Ответ с кодом, заголовком `Content-Type` и телом из байт.
pub fn typed(status: u16, content_type: &str, body: Vec<u8>) -> Response {
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = StatusCode::from_u16(status).unwrap();
    with_header(r, header::CONTENT_TYPE.as_str(), content_type)
}

/// Редирект 302 на `location`.
pub fn redirect(location: &str) -> Response {
    with_header(text(302, ""), header::LOCATION.as_str(), location)
}

/// Ответ с добавленным заголовком.
pub fn with_header(mut r: Response, name: &str, value: &str) -> Response {
    r.headers_mut().append(
        HeaderName::from_bytes(name.as_bytes()).unwrap(),
        HeaderValue::from_str(value).unwrap(),
    );
    r
}

/// Бесконечное тело кусками по 8 КиБ без `Content-Length`.
pub fn endless() -> Response {
    Response::new(Body::new(Endless))
}

/// Тело по одному байту с паузой `every`, пока клиент не уйдёт.
pub fn drip(every: Duration) -> Response {
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    tokio::spawn(async move {
        loop {
            if tx.send(Bytes::from_static(b"x")).await.is_err() {
                return;
            }
            tokio::time::sleep(every).await;
        }
    });
    Response::new(Body::new(Drip(rx)))
}

static CHUNK: [u8; 8192] = [b'x'; 8192];

struct Endless;

impl HttpBody for Endless {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(&CHUNK)))))
    }
}

struct Drip(mpsc::Receiver<Bytes>);

impl HttpBody for Drip {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.0.poll_recv(cx).map(|b| b.map(|b| Ok(Frame::data(b))))
    }
}
