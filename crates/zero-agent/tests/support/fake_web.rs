//! Поддельный HTTP-сервер на `127.0.0.1` (R1-C2, D121): сайт или свой экземпляр SearXNG.
//! Ответ строит обработчик теста по пути с запросом (`/search?q=…&format=json`); каждый
//! принятый запрос пишется в журнал сервера ([`FakeHttp::requests`]) — тест «подключения не
//! было» смотрит на пустой журнал, а не на таймер. Настоящий интернет тесты не используют.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

/// Ответ по пути с запросом (`/page`, `/search?q=x&format=json`).
pub type HttpHandler = fn(&str) -> Response;

/// Сервер: runtime живёт, пока жив сервер.
pub struct FakeHttp {
    /// `http://127.0.0.1:<порт>`.
    pub base: String,
    pub port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    _rt: tokio::runtime::Runtime,
}

impl FakeHttp {
    pub fn start(handler: HttpHandler) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = requests.clone();
        let app = Router::new().fallback(move |uri: Uri| {
            let log = log.clone();
            async move {
                let target = uri
                    .path_and_query()
                    .map(|p| p.as_str().to_owned())
                    .unwrap_or_else(|| uri.path().to_owned());
                log.lock().unwrap().push(target.clone());
                handler(&target)
            }
        });
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        rt.spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base: format!("http://{addr}"),
            port: addr.port(),
            requests,
            _rt: rt,
        }
    }

    /// Принятые запросы по порядку: путь с запросом.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// Ответ с телом и типом.
pub fn reply(status: StatusCode, content_type: &str, body: impl Into<String>) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, content_type.to_owned())],
        body.into(),
    )
        .into_response()
}

/// Выдача SearXNG `format=json`: `(title, url, content)`.
pub fn searx_results(hits: &[(&str, &str, &str)]) -> Response {
    let results: Vec<serde_json::Value> = hits
        .iter()
        .map(|(t, u, c)| serde_json::json!({"title": t, "url": u, "content": c}))
        .collect();
    reply(
        StatusCode::OK,
        "application/json",
        serde_json::json!({"query": "q", "results": results}).to_string(),
    )
}

/// Порт, на котором никто не слушает: сервер на нём только что закрыт.
pub fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}
