//! HTTP-транспорт клиента памяти (R1-M D133, D137) — третий держатель HTTP-клиента после
//! `za-llm` и `za-web` (ADR 0007); `reqwest::Client` строится только здесь
//! (`cargo xtask policy`), остальные модули крейта видят его через [`ZamClient`].
//!
//! - **Клиент:** `no_proxy()` всегда (переменные `HTTP_PROXY` не действуют: токен не уходит
//!   прокси), `redirect(Policy::none())` — любой 3xx — [`ErrorKind::Protocol`] «memory
//!   server redirected; not followed» (по умолчанию `reqwest` повторил бы 307 и 308 с телом
//!   и `X-Zam-*` на другой хост), без cookie, сжатия и HTTP/2 (функций нет, D129), rustls и
//!   webpki-roots, `User-Agent: zero-agent/<версия>`, соединение — 2 с.
//! - **Заголовки каждого запроса:** `Authorization: Bearer` — не в `default_headers`, а на
//!   запрос, с `HeaderValue::set_sensitive(true)`; `X-Zam-Machine` — всегда; `X-Zam-Session`,
//!   `X-Zam-Solution`, `X-Zam-Area` — из [`Ctx`]; значения не из ASCII — байтами UTF-8
//!   (`HeaderValue::from_bytes`). `GET /api/v1/health` — без токена и без `X-Zam-*` (D136).
//! - **Каналы** (D137): REST — всё, что делает рантайм; MCP `tools/call` без `initialize` и
//!   без `Mcp-Session-Id` (MCP 2026-07-28 без сессий) — только чтение модели. У MCP —
//!   `Accept: application/json, text/event-stream` (без него 406) и аргумент `session_id`,
//!   равный заголовку, у [`SESSION_ARG_TOOLS`]; у `memory_tree` аргумента нет, учёт — по
//!   заголовку. `session_id` вызывающего всегда снимается: сессию ставит только транспорт из
//!   `cx.session` (без сессии run — аргумента нет).
//! - **Сроки:** предел запроса — аргумент `limit` каждого вызова (константы ниже), он
//!   покрывает и отправку, и чтение тела; не успели — [`ErrorKind::Timeout`]. Отмена run
//!   бросает future — запрос обрывается.
//! - **Обрыв:** не соединились — [`ErrorKind::Offline`] (запрос не дошёл); соединение
//!   оборвалось после отправки без ответа со статусом или посреди тела —
//!   [`ErrorKind::Interrupted`] (исход неизвестен, повтор — только с `Idempotency-Key`).
//! - **Ошибки:** не 2xx — [`ZamError::from_problem`]; ошибка инструмента MCP (HTTP 200,
//!   `isError`) — [`parse_mcp_error`]; ошибка JSON-RPC — `Protocol`. Тексты ошибок — без
//!   тела запроса и без заголовков; значение токена, если сервер его повторил, заменяется.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, IF_MATCH, RETRY_AFTER,
};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::config::{ConfigError, MemoryConfig, Token, base_url, check_machine, host_machine};
use crate::error::{ErrorKind, ZamError, parse_mcp_error, truncate};

/// Метод HTTP для [`ZamClient::send_json`]: остальной крейт не называет `reqwest`.
pub use reqwest::Method;

/// Срок соединения (D137).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Срок `GET /api/v1/health` (D137).
pub const HEALTH_LIMIT: Duration = Duration::from_secs(3);
/// Срок чтения через MCP (D137).
pub const MCP_READ_LIMIT: Duration = Duration::from_secs(5);
/// Срок запросов Vault (D137).
pub const VAULT_LIMIT: Duration = Duration::from_secs(5);
/// Срок отправки снимка сессии (D137).
pub const SNAPSHOT_LIMIT: Duration = Duration::from_secs(10);
/// Срок проверок статуса (`/spaces`, `/solutions/{g}`, `/recall` с `record: false`).
pub const STATUS_LIMIT: Duration = Duration::from_secs(5);

/// `Accept` запросов MCP: без обоих типов ZAM отвечает 406.
pub const MCP_ACCEPT: &str = "application/json, text/event-stream";
/// Инструменты MCP с аргументом `session_id` (`SESSION_ARG_TOOLS`,
/// `crates/zam-server/src/mcp/tools.rs:531` e391440).
pub const SESSION_ARG_TOOLS: [&str; 3] = ["memory_search", "memory_read", "memory_recall"];
/// Аргумент сессии инструментов MCP: ставит только транспорт, из `cx.session`.
const SESSION_ARG: &str = "session_id";
/// Заголовок ошибки на любой 3xx (D133).
pub const REDIRECT_TITLE: &str = "memory server redirected; not followed";
/// Заголовок ошибки «отвечает не API ZeroAIMemory» (D136).
pub const NOT_ZAM_TITLE: &str = "not a ZeroAIMemory API";

/// Путь проверки сервера (D136): корневой `/health` отдаёт HTML WebUI.
pub const HEALTH_PATH: &str = "/api/v1/health";
/// Путь MCP (под базовым адресом, как и REST).
pub const MCP_PATH: &str = "/mcp";
/// `Accept` запросов REST.
const JSON: &str = "application/json";

/// Предел тела ответа 2xx, байт: самые большие ответы (`/read` до 64 КиБ текста, дайджест с
/// запасом D141) много меньше.
const BODY_MAX: usize = 8 * 1024 * 1024;
/// Предел читаемого тела ответа с ошибкой, байт.
const ERROR_BODY_MAX: usize = 64 * 1024;

const SESSION_HEADER: HeaderName = HeaderName::from_static("x-zam-session");
const SOLUTION_HEADER: HeaderName = HeaderName::from_static("x-zam-solution");
const MACHINE_HEADER: HeaderName = HeaderName::from_static("x-zam-machine");
const AREA_HEADER: HeaderName = HeaderName::from_static("x-zam-area");
const IDEMPOTENCY_HEADER: HeaderName = HeaderName::from_static("idempotency-key");

/// Контекст запроса: сессия run, решение и область проекта (заголовки `X-Zam-*`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Ctx<'a> {
    pub session: Option<&'a str>,
    pub solution: Option<&'a str>,
    pub area: Option<&'a str>,
}

/// Клиент сервера памяти. Клонирование дешёвое: пул соединений общий.
#[derive(Clone)]
pub struct ZamClient {
    http: reqwest::Client,
    /// Базовый адрес без последнего `/`.
    base: String,
    token: Option<Token>,
    machine: Option<HeaderValue>,
    /// Имя машины для `Debug`.
    machine_name: Option<String>,
}

impl fmt::Debug for ZamClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZamClient")
            .field("base", &self.base)
            .field("machine", &self.machine_name)
            .field("token", &self.token.as_ref().map(|_| "zam_…"))
            .finish()
    }
}

/// Номер запроса JSON-RPC (уникален в процессе).
static RPC_ID: AtomicU64 = AtomicU64::new(1);

impl ZamClient {
    /// Клиент по `[memory]` (D134): адрес — [`base_url`], машина — `cfg.machine` или имя
    /// хоста ([`host_machine`]; нет имени — заголовка нет), токен — значением.
    pub fn new(cfg: &MemoryConfig, token: Option<Token>) -> Result<Self, ConfigError> {
        let url = base_url(&cfg.url)?;
        let base = url.as_str().trim_end_matches('/').to_owned();
        let machine_name = match &cfg.machine {
            Some(m) => Some(check_machine(m)?),
            None => host_machine(),
        };
        let machine = match &machine_name {
            Some(m) => Some(
                HeaderValue::from_bytes(m.as_bytes())
                    .map_err(|_| ConfigError::Machine("недопустимые символы"))?,
            ),
            None => {
                tracing::warn!("имя машины не определено: X-Zam-Machine не шлётся");
                None
            }
        };
        if let Some(t) = &token {
            bearer(t).ok_or(ConfigError::Token)?;
        }
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("zero-agent/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ConfigError::Client(error_chain(&e)))?;
        Ok(Self {
            http,
            base,
            token,
            machine,
            machine_name,
        })
    }

    /// Есть ли токен.
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// Базовый адрес сервера (без последнего `/`).
    pub fn base(&self) -> &str {
        &self.base
    }

    /// `GET path` с токеном и заголовками `cx`; ответ 2xx — JSON.
    pub async fn get_json(
        &self,
        path: &str,
        cx: &Ctx<'_>,
        limit: Duration,
    ) -> Result<Value, ZamError> {
        let deadline = Instant::now() + limit;
        let req = self.request(Method::GET, path, cx, true, JSON)?;
        self.json_reply(req, deadline).await.map(|(_, v)| v)
    }

    /// `method path` с телом JSON, ключом идемпотентности и `If-Match: <rev>`; ответ 2xx —
    /// код и JSON (пустое тело, 204 — `Null`).
    #[allow(clippy::too_many_arguments)]
    pub async fn send_json(
        &self,
        method: Method,
        path: &str,
        body: &Value,
        cx: &Ctx<'_>,
        idem: Option<&str>,
        if_match: Option<i64>,
        limit: Duration,
    ) -> Result<(u16, Value), ZamError> {
        let deadline = Instant::now() + limit;
        let mut req = self.request(method, path, cx, true, JSON)?;
        if let Some(k) = idem {
            let v = HeaderValue::from_str(k).map_err(|_| bad_header("Idempotency-Key"))?;
            req = req.header(IDEMPOTENCY_HEADER, v);
        }
        if let Some(rev) = if_match {
            req = req.header(IF_MATCH, HeaderValue::from(rev));
        }
        self.json_reply(req.json(body), deadline).await
    }

    /// `GET /api/v1/health` без токена и без `X-Zam-*` (D136): ответ 2xx — JSON.
    pub(crate) async fn health(&self) -> Result<Value, ZamError> {
        let deadline = Instant::now() + HEALTH_LIMIT;
        let req = self.request(Method::GET, HEALTH_PATH, &Ctx::default(), false, JSON)?;
        self.json_reply(req, deadline).await.map(|(_, v)| v)
    }

    /// Вызов инструмента MCP `tool` (`tools/call`, D137): текст результата. Аргумент
    /// `session_id` из `args` снимается всегда; у [`SESSION_ARG_TOOLS`] он равен `cx.session`
    /// (нет сессии run — аргумента нет). Ошибка инструмента (`isError`) — по таблице форм
    /// [`parse_mcp_error`], ошибка JSON-RPC — `Protocol`.
    pub async fn mcp_call(
        &self,
        tool: &str,
        mut args: Value,
        cx: &Ctx<'_>,
        limit: Duration,
    ) -> Result<String, ZamError> {
        let deadline = Instant::now() + limit;
        let Value::Object(map) = &mut args else {
            return Err(ZamError::new(
                ErrorKind::BadRequest,
                "MCP arguments must be a JSON object",
            ));
        };
        // Сессия — только сессия run (`cx.session`): аргумент вызывающего (с задачи 8 —
        // модели) снимается всегда, иначе чтение легло бы в чужую сессию (M3 ревью задачи 2).
        map.remove(SESSION_ARG);
        if SESSION_ARG_TOOLS.contains(&tool)
            && let Some(s) = cx.session
        {
            map.insert(SESSION_ARG.into(), json!(s));
        }
        let id = RPC_ID.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        });
        let req = self
            .request(Method::POST, MCP_PATH, cx, true, MCP_ACCEPT)?
            .json(&body);
        let resp = self.send(req, deadline).await?;
        let resp = self.check_status(resp, deadline).await?;
        let sse = content_type(resp.headers()).starts_with("text/event-stream");
        let json_ct = is_json(resp.headers());
        let bytes = read_capped(resp, BODY_MAX, deadline).await?;
        let msg = if sse {
            sse_message(&bytes)
        } else if json_ct {
            serde_json::from_slice::<Value>(&bytes).ok()
        } else {
            return Err(ZamError::new(ErrorKind::NotZam, NOT_ZAM_TITLE));
        };
        let msg = msg.ok_or_else(|| ZamError::new(ErrorKind::Protocol, "invalid MCP reply"))?;
        self.mcp_result(&msg)
    }

    /// Текст результата JSON-RPC или ошибка.
    fn mcp_result(&self, msg: &Value) -> Result<String, ZamError> {
        if let Some(err) = msg.get("error") {
            let code = err.get("code").and_then(Value::as_i64);
            let message = err.get("message").and_then(Value::as_str).unwrap_or("");
            let mut e = ZamError::new(ErrorKind::Protocol, "MCP JSON-RPC error")
                .with_detail(truncate(message, 500));
            e.code = code.map(|c| c.to_string());
            return Err(self.scrubbed(e));
        }
        let Some(result) = msg.get("result") else {
            return Err(ZamError::new(
                ErrorKind::Protocol,
                "MCP reply without result",
            ));
        };
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|c| c.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(self.scrubbed(parse_mcp_error(&text)));
        }
        Ok(text)
    }

    /// Запрос `method path`: `Accept: <accept>` (один заголовок), машина, `X-Zam-*` из `cx`,
    /// токен (`auth`) — чувствительным заголовком. `auth == false` — запрос анонимный, без
    /// `X-Zam-*` (только `/api/v1/health`).
    fn request(
        &self,
        method: Method,
        path: &str,
        cx: &Ctx<'_>,
        auth: bool,
        accept: &'static str,
    ) -> Result<reqwest::RequestBuilder, ZamError> {
        let mut h = HeaderMap::new();
        h.insert(ACCEPT, HeaderValue::from_static(accept));
        if auth {
            if let Some(t) = &self.token {
                let v = bearer(t).ok_or_else(|| bad_header("Authorization"))?;
                h.insert(AUTHORIZATION, v);
            }
            if let Some(m) = &self.machine {
                h.insert(MACHINE_HEADER, m.clone());
            }
            for (name, value, shown) in [
                (SESSION_HEADER, cx.session, "X-Zam-Session"),
                (SOLUTION_HEADER, cx.solution, "X-Zam-Solution"),
                (AREA_HEADER, cx.area, "X-Zam-Area"),
            ] {
                if let Some(v) = value {
                    h.insert(name, attrib_value(v).ok_or_else(|| bad_header(shown))?);
                }
            }
        }
        let url = format!("{}{path}", self.base);
        Ok(self.http.request(method, url).headers(h))
    }

    /// Ответ 2xx как JSON: тело не JSON — [`ErrorKind::NotZam`], пустое — `Null`.
    async fn json_reply(
        &self,
        req: reqwest::RequestBuilder,
        deadline: Instant,
    ) -> Result<(u16, Value), ZamError> {
        let resp = self.send(req, deadline).await?;
        let resp = self.check_status(resp, deadline).await?;
        let status = resp.status().as_u16();
        let json_ct = is_json(resp.headers());
        let bytes = read_capped(resp, BODY_MAX, deadline).await?;
        if bytes.is_empty() {
            return Ok((status, Value::Null));
        }
        if !json_ct {
            return Err(ZamError::new(ErrorKind::NotZam, NOT_ZAM_TITLE));
        }
        let v = serde_json::from_slice(&bytes)
            .map_err(|_| ZamError::new(ErrorKind::Protocol, "invalid JSON from memory server"))?;
        Ok((status, v))
    }

    /// Отправка до срока: не соединились — `Offline`, срок вышел — `Timeout`, обрыв после
    /// отправки — `Interrupted` ([`transport_error`]).
    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        deadline: Instant,
    ) -> Result<reqwest::Response, ZamError> {
        match tokio::time::timeout_at(deadline, req.send()).await {
            Err(_) => Err(timeout()),
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Err(self.scrubbed(transport_error(e))),
        }
    }

    /// 2xx — ответ дальше; 3xx — `Protocol` без перехода; прочее — problem+json.
    async fn check_status(
        &self,
        resp: reqwest::Response,
        deadline: Instant,
    ) -> Result<reqwest::Response, ZamError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        if status.is_redirection() {
            let mut e = ZamError::new(ErrorKind::Protocol, REDIRECT_TITLE);
            e.status = Some(status.as_u16());
            return Err(e);
        }
        let retry_after = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let ct = content_type(resp.headers()).to_owned();
        let body = read_capped(resp, ERROR_BODY_MAX, deadline)
            .await
            .unwrap_or_default();
        Err(self.scrubbed(ZamError::from_problem(
            status.as_u16(),
            &ct,
            &body,
            retry_after,
        )))
    }

    fn scrubbed(&self, e: ZamError) -> ZamError {
        match &self.token {
            Some(t) => e.scrub(t.expose()),
            None => e,
        }
    }
}

/// `Bearer <токен>` чувствительным заголовком: значение не попадает в `Debug` заголовков.
fn bearer(t: &Token) -> Option<HeaderValue> {
    let mut v = HeaderValue::from_str(&format!("Bearer {}", t.expose())).ok()?;
    v.set_sensitive(true);
    Some(v)
}

/// Значение заголовка атрибуции: байты UTF-8, до 128 символов, без управляющих (как
/// принимает сервер, `attrib.rs` e391440).
fn attrib_value(s: &str) -> Option<HeaderValue> {
    check_machine(s).ok()?;
    HeaderValue::from_bytes(s.trim().as_bytes()).ok()
}

fn bad_header(name: &str) -> ZamError {
    ZamError::new(ErrorKind::BadRequest, format!("invalid {name} value"))
}

fn timeout() -> ZamError {
    ZamError::new(ErrorKind::Timeout, "memory server did not answer in time")
}

fn content_type(h: &HeaderMap) -> &str {
    h.get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim_start()
}

/// `application/json` или `…+json` (problem+json).
fn is_json(h: &HeaderMap) -> bool {
    let ct = content_type(h).to_ascii_lowercase();
    let mime = ct.split(';').next().unwrap_or("").trim();
    mime == "application/json" || (mime.starts_with("application/") && mime.ends_with("+json"))
}

/// Ошибка `reqwest` в [`ZamError`] (D137; M1 ревью задачи 2). Порядок проверок важен:
///
/// 1. не соединились (`is_connect`, в том числе срок соединения и TLS) — `Offline`: запрос
///    до сервера не дошёл;
/// 2. срок вышел — `Timeout`;
/// 3. соединение оборвалось после отправки, ответа со статусом нет — `Interrupted`, исход
///    неизвестен: `is_request` (сброс, `IncompleteMessage` — сервер закрыл соединение без
///    ответа), `is_body` и `is_decode` (обрыв тела: `Response::chunk` в reqwest 0.12
///    оборачивает ошибку тела в `Decode`, а JSON разбирается здесь, не в reqwest);
/// 4. прочее (сборка запроса) — `Protocol`.
///
/// Текст — цепочка причин без адреса.
fn transport_error(e: reqwest::Error) -> ZamError {
    let kind = if e.is_connect() {
        ErrorKind::Offline
    } else if e.is_timeout() {
        ErrorKind::Timeout
    } else if e.is_request() || e.is_body() || e.is_decode() {
        ErrorKind::Interrupted
    } else {
        ErrorKind::Protocol
    };
    let title = match kind {
        ErrorKind::Offline => "memory server unreachable",
        ErrorKind::Timeout => "memory server did not answer in time",
        ErrorKind::Interrupted => "memory server connection interrupted; outcome unknown",
        _ => "memory request failed",
    };
    ZamError::new(kind, title).with_detail(error_chain(&e.without_url()))
}

/// Текст ошибки и её причин через `: `.
fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![e.to_string()];
    let mut src = e.source();
    while let Some(s) = src {
        let t = s.to_string();
        if !parts.iter().any(|p| p.contains(&t)) {
            parts.push(t);
        }
        src = s.source();
    }
    parts.join(": ")
}

/// Тело ответа до `max` байт и до срока; больше — `Protocol`.
async fn read_capped(
    mut resp: reqwest::Response,
    max: usize,
    deadline: Instant,
) -> Result<Vec<u8>, ZamError> {
    let mut out = Vec::new();
    loop {
        let chunk = match tokio::time::timeout_at(deadline, resp.chunk()).await {
            Err(_) => return Err(timeout()),
            Ok(Err(e)) => return Err(transport_error(e)),
            Ok(Ok(c)) => c,
        };
        let Some(c) = chunk else {
            return Ok(out);
        };
        if out.len() + c.len() > max {
            return Err(ZamError::new(
                ErrorKind::Protocol,
                "memory server reply is too large",
            ));
        }
        out.extend_from_slice(&c);
    }
}

/// Первое сообщение JSON-RPC (`result` или `error`) в потоке SSE: строки `data:` события
/// склеиваются переводом строки.
fn sse_message(bytes: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(bytes);
    let mut data = String::new();
    let flush = |data: &mut String| -> Option<Value> {
        let v = serde_json::from_str::<Value>(data).ok();
        data.clear();
        v.filter(|v| v.get("result").is_some() || v.get("error").is_some())
    };
    for line in text.lines() {
        if line.is_empty() {
            if let Some(v) = flush(&mut data) {
                return Some(v);
            }
        } else if let Some(d) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(d.strip_prefix(' ').unwrap_or(d));
        }
    }
    flush(&mut data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_message_takes_first_reply() {
        let s = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"}\n\n\
                  data: {\"jsonrpc\":\"2.0\",\"id\":1,\n\
                  data: \"result\":{\"content\":[]}}\n\n";
        let v = sse_message(s).unwrap();
        assert_eq!(v["id"], 1);
        assert!(sse_message(b"data: nope\n\n").is_none());
    }

    #[test]
    fn json_content_types() {
        let mut h = HeaderMap::new();
        for (ct, ok) in [
            ("application/json", true),
            ("application/json; charset=utf-8", true),
            ("application/problem+json", true),
            ("text/html; charset=utf-8", false),
            ("text/plain", false),
        ] {
            h.insert(CONTENT_TYPE, HeaderValue::from_static(ct));
            assert_eq!(is_json(&h), ok, "{ct}");
        }
    }
}
