//! HTTP-транспорт коннекторов: таймауты (D26), прокси, SSE, ошибки по классам.
//!
//! Тело читается через `Response::chunk()` без функции `stream` reqwest (D2).
//! Таймаут до первого байта ограничивает ожидание заголовков ответа, idle-таймаут —
//! паузу перед каждым чанком тела. Drop [`SseResponse`] закрывает соединение.

use std::collections::VecDeque;
use std::time::Duration;

use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use serde_json::Value;

use super::limits::classify_status;
use super::secret::{Secret, redact};
use super::sse::{SseEvent, SseParser};
use crate::error::{ErrorClass, ModelError};

/// Сколько тела ответа с ошибкой читается для сообщения.
const ERROR_BODY_LIMIT: usize = 64 * 1024;
/// Предел тела JSON-ответа (`/models`, `/props` и т. п.).
const JSON_BODY_LIMIT: usize = 32 * 1024 * 1024;

/// Таймауты запроса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Установка соединения (TCP и TLS).
    pub connect: Duration,
    /// От отправки запроса до заголовков ответа.
    pub first_byte: Duration,
    /// Пауза между чанками тела.
    pub idle: Duration,
}

impl Timeouts {
    /// Локальный эндпоинт (D26): 2 с, 300 с, 90 с.
    pub fn local() -> Self {
        Self::secs(2, 300, 90)
    }

    /// Облачный эндпоинт (D26): 10 с, 120 с, 90 с.
    pub fn cloud() -> Self {
        Self::secs(10, 120, 90)
    }

    /// Зонды локальных серверов: 2 с, 5 с, 5 с (как `probe_llama` R0).
    pub fn probe_local() -> Self {
        Self::secs(2, 5, 5)
    }

    /// Зонды облака: 10 с, 10 с, 10 с.
    pub fn probe_cloud() -> Self {
        Self::secs(10, 10, 10)
    }

    const fn secs(connect: u64, first_byte: u64, idle: u64) -> Self {
        Self {
            connect: Duration::from_secs(connect),
            first_byte: Duration::from_secs(first_byte),
            idle: Duration::from_secs(idle),
        }
    }
}

/// HTTP-клиент коннекторов. Клонирование дешёвое: `reqwest::Client` разделяет пул
/// соединений через `Arc`.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    /// Ошибка сборки клиента (инициализация TLS) не паникует, а возвращается
    /// каждым запросом как `Transport`.
    client: Result<reqwest::Client, String>,
    timeouts: Timeouts,
}

impl HttpTransport {
    /// Локальный эндпоинт: прокси из окружения не используются (D26).
    pub fn local(t: Timeouts) -> Self {
        Self::build(t, true)
    }

    /// Облачный эндпоинт: прокси из окружения (`HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`).
    pub fn cloud(t: Timeouts) -> Self {
        Self::build(t, false)
    }

    fn build(t: Timeouts, local: bool) -> Self {
        let mut b = reqwest::Client::builder()
            .connect_timeout(t.connect)
            // Перенаправление API — ошибка конфигурации: POST по 3xx не повторяется,
            // ответ классифицируется как есть.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("zero-agent/", env!("CARGO_PKG_VERSION")));
        if local {
            b = b.no_proxy();
        }
        let client = b.build().map_err(|e| {
            let m = error_chain(&e);
            tracing::error!(error = %m, "HTTP-клиент не собран");
            m
        });
        Self {
            client,
            timeouts: t,
        }
    }

    pub fn timeouts(&self) -> Timeouts {
        self.timeouts
    }

    /// `POST` с телом JSON и ответом `text/event-stream`. Статус ≠ 2xx — `Err` по
    /// [`classify_status`] с телом, очищенным от ключей `redact`.
    pub async fn post_sse(
        &self,
        url: &str,
        mut headers: HeaderMap,
        body: &Value,
        redact: &[&Secret],
    ) -> Result<SseResponse, ModelError> {
        if !headers.contains_key(ACCEPT) {
            headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        }
        let req = self.client(redact)?.post(url).json(body);
        let resp = self.send(req, headers, redact).await?;
        let resp = self.check_status(resp, redact).await?;
        Ok(SseResponse {
            headers: resp.headers().clone(),
            resp,
            parser: SseParser::new(),
            pending: VecDeque::new(),
            idle: self.timeouts.idle,
            ended: false,
        })
    }

    /// `GET` с ответом JSON.
    pub async fn get_json(
        &self,
        url: &str,
        headers: HeaderMap,
        redact: &[&Secret],
    ) -> Result<Value, ModelError> {
        let req = self.client(redact)?.get(url);
        let resp = self.send(req, headers, redact).await?;
        self.read_json(resp, redact).await.map(|(v, _)| v)
    }

    /// `POST` с телом JSON и ответом JSON; вместе с телом — заголовки ответа (лимиты).
    pub async fn post_json(
        &self,
        url: &str,
        headers: HeaderMap,
        body: &Value,
        redact: &[&Secret],
    ) -> Result<(Value, HeaderMap), ModelError> {
        let req = self.client(redact)?.post(url).json(body);
        let resp = self.send(req, headers, redact).await?;
        self.read_json(resp, redact).await
    }

    fn client(&self, secrets: &[&Secret]) -> Result<&reqwest::Client, ModelError> {
        self.client.as_ref().map_err(|m| {
            ModelError::new(
                ErrorClass::Transport,
                format!("HTTP-клиент не собран: {}", redact(m, secrets)),
            )
        })
    }

    /// Отправка с таймаутом до заголовков ответа.
    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        mut headers: HeaderMap,
        secrets: &[&Secret],
    ) -> Result<reqwest::Response, ModelError> {
        mark_sensitive(&mut headers, secrets);
        let fut = req.headers(headers).send();
        match tokio::time::timeout(self.timeouts.first_byte, fut).await {
            Err(_) => Err(ModelError::new(
                ErrorClass::Timeout,
                format!(
                    "нет ответа за {} мс до первого байта",
                    self.timeouts.first_byte.as_millis()
                ),
            )),
            Ok(Err(e)) => Err(from_reqwest(&e, false, secrets)),
            Ok(Ok(r)) => Ok(r),
        }
    }

    /// Статус ≠ 2xx — ошибка с телом (до 64 КиБ), очищенным от ключей.
    async fn check_status(
        &self,
        mut resp: reqwest::Response,
        secrets: &[&Secret],
    ) -> Result<reqwest::Response, ModelError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let headers = resp.headers().clone();
        // Тело ошибки — для сообщения; не дочитанное до конца тело не мешает.
        let body = read_body(
            &mut resp,
            self.timeouts.idle,
            ERROR_BODY_LIMIT,
            true,
            secrets,
        )
        .await
        .unwrap_or_default();
        let text = redact(&String::from_utf8_lossy(&body), secrets);
        let err = classify_status(status.as_u16(), &headers, &text);
        tracing::debug!(status = status.as_u16(), class = ?err.class, "ответ HTTP с ошибкой");
        Err(err)
    }

    async fn read_json(
        &self,
        resp: reqwest::Response,
        secrets: &[&Secret],
    ) -> Result<(Value, HeaderMap), ModelError> {
        let mut resp = self.check_status(resp, secrets).await?;
        let headers = resp.headers().clone();
        let body = read_body(
            &mut resp,
            self.timeouts.idle,
            JSON_BODY_LIMIT,
            false,
            secrets,
        )
        .await?;
        let v = serde_json::from_slice(&body)
            .map_err(|e| ModelError::new(ErrorClass::Malformed, format!("тело не JSON: {e}")))?;
        Ok((v, headers))
    }
}

/// Ответ `text/event-stream`. Drop закрывает соединение.
pub struct SseResponse {
    /// Заголовки ответа (лимиты провайдера — [`super::parse_rate_limits`]).
    pub headers: HeaderMap,
    resp: reqwest::Response,
    parser: SseParser,
    pending: VecDeque<SseEvent>,
    idle: Duration,
    /// Поток кончился или дал ошибку: дальше только `Ok(None)`.
    ended: bool,
}

impl SseResponse {
    /// Следующее событие; `Ok(None)` — поток закрыт сервером штатно. Пауза перед
    /// каждым чанком не дольше idle-таймаута (`Timeout`); обрыв тела — `StreamCut`;
    /// невалидный UTF-8 или слишком длинная строка — `Malformed`. После ошибки
    /// следующие вызовы дают `Ok(None)`.
    pub async fn next_event(&mut self) -> Result<Option<SseEvent>, ModelError> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Ok(Some(ev));
            }
            if let Some(m) = self.parser.malformed() {
                let err = ModelError::new(ErrorClass::Malformed, m.to_owned());
                self.end();
                return Err(err);
            }
            if self.ended {
                return Ok(None);
            }
            match tokio::time::timeout(self.idle, self.resp.chunk()).await {
                Err(_) => {
                    self.end();
                    return Err(ModelError::new(
                        ErrorClass::Timeout,
                        format!("нет данных стрима {} мс", self.idle.as_millis()),
                    ));
                }
                Ok(Err(e)) => {
                    self.end();
                    return Err(from_reqwest(&e, true, &[]));
                }
                Ok(Ok(Some(bytes))) => {
                    let evs = self.parser.push(&bytes);
                    self.pending.extend(evs);
                }
                Ok(Ok(None)) => {
                    self.ended = true;
                    self.pending.extend(self.parser.finish());
                }
            }
        }
    }

    /// Конец потока после ошибки: разборщик больше не опрашивается.
    fn end(&mut self) {
        self.ended = true;
        self.pending.clear();
        self.parser = SseParser::new();
    }
}

/// Читает тело до `limit` байт с idle-таймаутом на чанк. `truncate_ok` — превышение
/// предела не ошибка (тело ошибки обрезается), иначе `Malformed`.
async fn read_body(
    resp: &mut reqwest::Response,
    idle: Duration,
    limit: usize,
    truncate_ok: bool,
    secrets: &[&Secret],
) -> Result<Vec<u8>, ModelError> {
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(idle, resp.chunk()).await {
            Err(_) => {
                return Err(ModelError::new(
                    ErrorClass::Timeout,
                    format!("нет данных тела {} мс", idle.as_millis()),
                ));
            }
            Ok(Err(e)) => return Err(from_reqwest(&e, true, secrets)),
            Ok(Ok(None)) => return Ok(out),
            Ok(Ok(Some(b))) => {
                out.extend_from_slice(&b);
                if out.len() > limit {
                    if truncate_ok {
                        out.truncate(limit);
                        return Ok(out);
                    }
                    return Err(ModelError::new(
                        ErrorClass::Malformed,
                        format!("тело ответа больше {limit} байт"),
                    ));
                }
            }
        }
    }
}

/// Заголовки, содержащие ключ, помечаются `sensitive` (не печатаются в `Debug`).
fn mark_sensitive(headers: &mut HeaderMap, secrets: &[&Secret]) {
    for v in headers.values_mut() {
        let bytes = v.as_bytes();
        if secrets.iter().any(|s| {
            let k = s.expose().as_bytes();
            !k.is_empty() && bytes.windows(k.len()).any(|w| w == k)
        }) {
            v.set_sensitive(true);
        }
    }
}

/// Ошибка reqwest → класс. `in_body` — ошибка при чтении тела: обрыв стрима.
fn from_reqwest(e: &reqwest::Error, in_body: bool, secrets: &[&Secret]) -> ModelError {
    // Сбой соединения, в том числе таймаут connect (на Windows закрытый порт
    // отвечает только по таймауту), — `Transport`: сервер недоступен.
    let class = if e.is_connect() {
        ErrorClass::Transport
    } else if e.is_timeout() {
        ErrorClass::Timeout
    } else if e.is_builder() {
        ErrorClass::BadRequest
    } else if in_body {
        ErrorClass::StreamCut
    } else {
        ErrorClass::Transport
    };
    ModelError::new(class, redact(&error_chain(e), secrets))
}

/// Текст ошибки вместе с цепочкой причин: `Display` reqwest без них неинформативен.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(c) = src {
        s.push_str(": ");
        s.push_str(&c.to_string());
        src = c.source();
    }
    s
}
