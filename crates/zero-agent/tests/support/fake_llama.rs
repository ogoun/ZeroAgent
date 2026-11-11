//! Поддельный OpenAI-совместимый llama-server — axum в процессе теста (многопоточный
//! runtime, пока основной поток ждёт бинарник).
//!
//! `/props` сообщает заданный `n_ctx` (R1-B2 D43: окно run — из зонда), один слот.
//! Ответы модели строит обработчик теста по телам всех запросов. Без `usage` сервер
//! вырезает из стрима события с `usage` и `timings`: расход хода неизвестен
//! (`usage.input == 0`, R1-B2 D41). Неизвестный путь, в том числе `/tokenize`, — 404:
//! счётчик контекста считает байты / 4.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use http_body::Frame;
use serde_json::{Value, json};

/// Путь запроса модели у llama-server.
pub const CHAT: &str = "/v1/chat/completions";

/// Ответ на очередной запрос модели по телам всех запросов до него включительно
/// (последнее — текущий).
pub type ChatHandler = fn(&[Value]) -> Response;

/// Поддельный сервер: runtime живёт, пока жив сервер.
pub struct Fake {
    pub base: String,
    /// Тела запросов модели в порядке поступления.
    bodies: Arc<Mutex<Vec<Value>>>,
    _rt: tokio::runtime::Runtime,
}

impl Fake {
    /// Сервер с окном `n_ctx` в `/props`; `usage = false` — стрим без расхода. Без
    /// расхода тело ответа буферизуется целиком: паузы [`paced_reply`] пропадают.
    pub fn start(n_ctx: u32, usage: bool, chat: ChatHandler) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let bodies = Arc::new(Mutex::new(Vec::<Value>::new()));
        let shared = bodies.clone();
        let app = Router::new().fallback(move |m: Method, uri: Uri, body: Bytes| {
            let shared = shared.clone();
            async move {
                match (m.as_str(), uri.path()) {
                    ("GET", "/props") => json_reply(
                        StatusCode::OK,
                        // Один слот: `id_slot` тела запроса одинаков до и после сбоя.
                        json!({
                            "default_generation_settings": {"n_ctx": n_ctx},
                            "total_slots": 1,
                            "model_path": "/models/Qwen3.8-27B-Q4_K_M.gguf",
                            "build_info": "b7100-abc",
                        }),
                    ),
                    ("POST", "/apply-template") => {
                        json_reply(StatusCode::OK, json!({"prompt": "шаблон"}))
                    }
                    ("POST", p) if p.starts_with("/slots/") => {
                        json_reply(StatusCode::OK, json!({}))
                    }
                    ("POST", CHAT) => {
                        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        let seen = {
                            let mut b = shared.lock().unwrap();
                            b.push(v);
                            b.clone()
                        };
                        let reply = chat(&seen);
                        if usage {
                            reply
                        } else {
                            without_usage(reply).await
                        }
                    }
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
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
            bodies,
            _rt: rt,
        }
    }

    pub fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
}

/// Стрим без событий с `usage` и `timings`; прочие ответы (ошибки) — как есть.
async fn without_usage(reply: Response) -> Response {
    let sse = reply
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"));
    if !sse {
        return reply;
    }
    let (mut parts, body) = reply.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let kept: String = text
        .split_inclusive("\n\n")
        .filter(|ev| {
            let Some(data) = ev.trim_end().strip_prefix("data: ") else {
                return true;
            };
            let v: Value = serde_json::from_str(data).unwrap_or(Value::Null);
            v.get("usage").is_none() && v.get("timings").is_none()
        })
        .collect();
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(kept))
}

pub fn json_reply(status: StatusCode, v: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        v.to_string(),
    )
        .into_response()
}

pub fn sse(v: &Value) -> String {
    format!("data: {v}\n\n")
}

pub fn chunk(delta: Value) -> String {
    sse(
        &json!({"choices": [{"index": 0, "finish_reason": null, "delta": delta}],
                "object": "chat.completion.chunk", "model": "qwen"}),
    )
}

/// Расход хода в стриме: `prompt_tokens`, из них из кэша (`timings.cache_n`), вывод.
#[derive(Debug, Clone, Copy)]
pub struct Spend {
    pub prompt: u64,
    pub cached: u64,
    pub output: u64,
}

/// Расход сценариев R1-B1: 2048 токенов входа без кэша, 3 — вывода. Вход — порядка
/// настоящего запроса с нативными инструментами (R1-C1 задача 13: оценка запроса больше
/// 1024, и прежние 12 токенов D43 счёл бы тихой обрезкой контекста сервером).
pub const FIXED: Spend = Spend {
    prompt: 2048,
    cached: 0,
    output: 3,
};

/// Правдоподобный расход запроса `seen.last()`: вход — байты JSON сообщений и описаний
/// инструментов / 4 (не меньше оценки клиента по тексту, D43 не подозревает обрезку),
/// из кэша — общее начало JSON сообщений с прошлым запросом / 4 (кэш префикса одного
/// слота llama-server).
pub fn spend_of(seen: &[Value]) -> Spend {
    let Some(cur) = seen.last() else {
        return FIXED;
    };
    let msgs = |b: &Value| serde_json::to_string(&b["messages"]).unwrap_or_default();
    let now = msgs(cur);
    let tools = serde_json::to_string(&cur["tools"]).unwrap_or_default();
    let prompt = ((now.len() + tools.len()) / 4) as u64;
    // Первый запрос — холодный кэш.
    let cached = seen.len().checked_sub(2).map_or(0, |i| {
        let before = msgs(&seen[i]);
        let common = before
            .bytes()
            .zip(now.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        ((common + tools.len()) / 4) as u64
    });
    Spend {
        prompt,
        cached: cached.min(prompt),
        output: 3,
    }
}

/// Конец ответа: `finish_reason`, usage [`FIXED`] и `[DONE]`.
pub fn tail(finish: &str) -> String {
    tail_with(finish, FIXED)
}

/// Конец ответа с расходом `u` (`usage` и `timings` llama.cpp) и `[DONE]`.
pub fn tail_with(finish: &str, u: Spend) -> String {
    let mut s = sse(
        &json!({"choices": [{"index": 0, "finish_reason": finish, "delta": {}}],
                "object": "chat.completion.chunk", "model": "qwen"}),
    );
    s.push_str(&sse(
        &json!({"choices": [], "object": "chat.completion.chunk", "model": "qwen",
        "usage": {"prompt_tokens": u.prompt, "completion_tokens": u.output,
                  "total_tokens": u.prompt + u.output},
        "timings": {"cache_n": u.cached, "prompt_n": u.prompt - u.cached,
                    "predicted_n": u.output}}),
    ));
    s.push_str("data: [DONE]\n\n");
    s
}

pub fn sse_reply(body: String) -> Response {
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

/// Ответ текстом из частей, `finish_reason = stop`, расход [`FIXED`].
pub fn text_reply(parts: &[&str]) -> Response {
    text_reply_with(parts, FIXED)
}

/// [`text_reply`] с расходом `u`.
pub fn text_reply_with(parts: &[&str], u: Spend) -> Response {
    let mut body = chunk(json!({"role": "assistant", "content": null}));
    for p in parts {
        body.push_str(&chunk(json!({"content": p})));
    }
    body.push_str(&tail_with("stop", u));
    sse_reply(body)
}

/// Ответ одним вызовом инструмента `name` с аргументами `args` (JSON-текст).
pub fn call_reply(id: &str, name: &str, args: &str) -> Response {
    call_reply_with(id, name, args, FIXED)
}

/// [`call_reply`] с расходом `u`.
pub fn call_reply_with(id: &str, name: &str, args: &str, u: Spend) -> Response {
    let mut body = chunk(json!({"role": "assistant", "content": null}));
    body.push_str(&chunk(
        json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
        "function": {"name": name, "arguments": args}}]}),
    ));
    body.push_str(&tail_with("tool_calls", u));
    sse_reply(body)
}

/// Ответ пакетом вызовов `(id, name, args)` (аргументы — JSON-текст) в одном ходе.
pub fn calls_reply(calls: &[(String, String, String)]) -> Response {
    let mut body = chunk(json!({"role": "assistant", "content": null}));
    for (i, (id, name, args)) in calls.iter().enumerate() {
        body.push_str(&chunk(
            json!({"tool_calls": [{"index": i, "id": id, "type": "function",
            "function": {"name": name, "arguments": args}}]}),
        ));
    }
    body.push_str(&tail("tool_calls"));
    sse_reply(body)
}

/// Тело SSE: части с паузами перед ними.
pub struct PacedBody {
    pub parts: Vec<(Duration, Bytes)>,
    pub sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl HttpBody for PacedBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.parts.is_empty() {
            return Poll::Ready(None);
        }
        let delay = self.parts[0].0;
        let sleep = self
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(delay)));
        match sleep.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) => {
                self.sleep = None;
                let (_, b) = self.parts.remove(0);
                Poll::Ready(Some(Ok(Frame::data(b))))
            }
        }
    }
}

/// Стрим из частей с паузами перед ними.
pub fn paced_reply(parts: Vec<(Duration, String)>) -> Response {
    let body = PacedBody {
        parts: parts
            .into_iter()
            .map(|(d, s)| (d, Bytes::from(s)))
            .collect(),
        sleep: None,
    };
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::new(body),
    )
        .into_response()
}

/// Конфигурация с одним эндпоинтом llama.cpp по адресу `base`.
pub fn llama_config(base: &str) -> String {
    format!(
        "[models]\ndefault = \"local\"\n\n[[models.endpoints]]\nid = \"local\"\nbackend = \"llama_cpp\"\nbase_url = \"{base}\"\n"
    )
}
