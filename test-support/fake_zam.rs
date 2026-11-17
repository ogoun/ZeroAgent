//! Поддельный сервер ZeroAIMemory для тестов (R1-M D154) — один на весь workspace.
//!
//! Подключается в тестовый крейт модулем, без отдельного крейта (как `unique_dir.rs`):
//!
//! ```ignore
//! #[path = "../../../test-support/fake_zam.rs"]
//! mod fake_zam;
//! ```
//!
//! Нужны dev-зависимости `axum`, `tokio` (`net`, `rt`, `sync`, `time`) и `serde_json`.
//!
//! - **Ожидание** — [`within`] × [`time_scale`] (`ZA_TEST_TIME_SCALE`, целое 1–6; образец —
//!   `crates/za-web/tests/support/mod.rs`): один помощник у всех тестов памяти.
//! - **Адрес** — только `127.0.0.1` со свободным портом: тесты не ходят ни на рабочий, ни
//!   на тестовый сервер памяти.
//! - **Канал.** Каждый принятый запрос уходит в `mpsc` ([`Seen`]: метод, путь с запросом,
//!   заголовки без `Authorization`, признак верного токена, тело JSON). «Запроса не было»
//!   тест проверяет пустым каналом после завершения вызова ([`FakeZam::seen`]), а не таймером.
//! - **Формы ответов — как у ZAM** `0.0.11-m6` (`e391440`) и `0.0.12-m6.5` (`4459dda`):
//!   `GET /health` в корне — HTML WebUI, API — только `/api/v1/health`; ошибки REST —
//!   `application/problem+json` с `type` `about:blank` у 400, 401, 403, 404, 500 и 503;
//!   `/mcp` без `Accept: application/json, text/event-stream` — 406; ошибки MCP — HTTP 200
//!   с `isError: true`, текстом `Ошибка <код> <title>: <detail>` и строкой `~N tok`;
//!   неизвестный инструмент — ошибка JSON-RPC; у Vault `?name=` молча игнорируется.
//! - **Подмены** включаются методами в тесте, без таймеров: форма `/api/v1/health`
//!   ([`Health`]), требуемый токен, 3xx на другой сервер, ответ-ошибка по пути, решение
//!   и отказ в доступе к нему, блокировка Vault, выключенные типы подсказок, ошибка MCP.
//!
//! Задачи 4–9 дополняют сервер сессиями, записями, ключами идемпотентности, снимками.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Предел ожидания вызова клиента на `127.0.0.1` (до множителя): больше сроков запросов
/// тестов (10 с), так что паника [`within`] значит зависание, а не медленный ответ.
pub const WAIT: Duration = Duration::from_secs(30);

/// Множитель времени тестов в реальном времени: `ZA_TEST_TIME_SCALE` (целое 1–6, по
/// умолчанию 1; в CI — 4).
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

/// Версия и коммит рабочего выпуска ZAM `0.0.11-m6`.
pub const VERSION_M6: &str = "0.0.11-m6";
pub const GIT_M6: &str = "e391440c0677";
/// Версия и коммит выпуска `0.0.12-m6.5` (тестовый стенд с 2026-11-17).
pub const VERSION_M65: &str = "0.0.12-m6.5";
pub const GIT_M65: &str = "4459ddae9052";
/// Последний выпуск без `features` (уровень L1).
pub const VERSION_M57: &str = "0.0.10-m5.7";

/// Флаги `features` выпуска `0.0.11-m6` (`api/health.rs` e391440).
pub const FEATURES_M6: &[&str] = &[
    "m6",
    "budget",
    "snippet",
    "focus",
    "digest",
    "hint_thresholds",
    "kind_boost",
    "attach_weight",
    "code_recv",
    "snapshot",
    "epochs",
    "unattributed",
    "holdout",
    "backup",
    "export",
    "import",
    "recall_timeout",
    "idempotent_nodes",
    "mcp_session_arg",
];

/// Флаги выпуска `0.0.12-m6.5`: те же и `idea_review` (`api/health.rs` 4459dda).
pub fn features_m65() -> Vec<&'static str> {
    let mut f = FEATURES_M6.to_vec();
    f.push("idea_review");
    f
}

/// Типы подсказок в ответе `/recall` `session_start` (`hooks`, R5 ZAM).
pub const HOOK_KINDS: &[&str] = &[
    "session_start",
    "prompt",
    "pre_tool",
    "post_tool",
    "checkpoint",
    "subagent_start",
    "file_context",
    "session_journal",
    "recovery",
];

/// Инструменты MCP ZAM (§13.1).
pub const MCP_TOOLS: &[&str] = &[
    "memory_search",
    "memory_read",
    "memory_cache_put",
    "memory_tree",
    "memory_write",
    "memory_log",
    "memory_update_entry",
    "memory_link",
    "memory_recall",
    "memory_vault",
    "memory_code",
];

/// Инструменты с аргументом `session_id` (`SESSION_ARG_TOOLS`, `mcp/tools.rs:531` e391440).
pub const SESSION_ARG_TOOLS: &[&str] = &["memory_recall", "memory_search", "memory_read"];

/// Ответ `/api/v1/health`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// `0.0.11-m6`: флаги [`FEATURES_M6`], `token_counter`, `tokenizer_id`.
    M6,
    /// `0.0.12-m6.5`: флаги [`features_m65`].
    M65,
    /// `0.0.10-m5.7`: без `features`, `token_counter` и `tokenizer_id`.
    NoFeatures,
    /// 200 `text/html` — как WebUI на любом пути вне API.
    Html,
    /// Ошибка с кодом (problem+json `about:blank`).
    Status(u16),
    /// Произвольное тело JSON с кодом 200.
    Json(Value),
}

/// Принятый сервером запрос.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    /// Путь с запросом (`/a?b=c`).
    pub path: String,
    /// Заголовки (имена строчными) без `Authorization`: значения — байты как пришли.
    pub headers: Vec<(String, Vec<u8>)>,
    /// `Authorization: Bearer <токен>` совпал с требуемым токеном.
    pub authorized: bool,
    /// Был ли заголовок `Authorization` вообще.
    pub has_authorization: bool,
    /// Тело как JSON; пустое или не JSON — `Null`.
    pub body: Value,
}

impl Seen {
    /// Байты заголовка `name` (строчными).
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_slice())
    }

    /// Заголовок `name` как UTF-8.
    pub fn header_str(&self, name: &str) -> Option<String> {
        self.header(name)
            .map(|v| String::from_utf8_lossy(v).into_owned())
    }

    /// Путь без строки запроса.
    pub fn route(&self) -> &str {
        self.path.split('?').next().unwrap_or("")
    }
}

/// Ответ-ошибка, заданная тестом для пути.
#[derive(Debug, Clone)]
struct Failure {
    status: u16,
    problem_type: String,
    detail: String,
    retry_after: Option<u64>,
}

#[derive(Debug)]
struct State {
    health: Health,
    token: Option<String>,
    redirect: Option<String>,
    solutions: BTreeMap<String, Value>,
    forbidden: BTreeMap<String, String>,
    vault_locked: Option<String>,
    hooks_off: Vec<String>,
    failures: BTreeMap<String, Failure>,
    mcp_error: Option<String>,
}

/// Запущенный поддельный ZAM.
pub struct FakeZam {
    pub addr: SocketAddr,
    pub rx: mpsc::UnboundedReceiver<Seen>,
    state: Arc<Mutex<State>>,
}

impl FakeZam {
    /// Сервер на `127.0.0.1:0` с формой `0.0.11-m6`, без требуемого токена.
    pub async fn start() -> Self {
        let state = Arc::new(Mutex::new(State {
            health: Health::M6,
            token: None,
            redirect: None,
            solutions: BTreeMap::new(),
            forbidden: BTreeMap::new(),
            vault_locked: None,
            hooks_off: Vec::new(),
            failures: BTreeMap::new(),
            mcp_error: None,
        }));
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = state.clone();
        let app = Router::new().fallback(move |req: Request| {
            let tx = tx.clone();
            let state = shared.clone();
            async move { handle(req, &tx, &state).await }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { addr, rx, state }
    }

    /// Базовый адрес `http://127.0.0.1:<порт>`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn set_health(&self, h: Health) {
        self.with(|s| s.health = h);
    }

    /// Всем путям, кроме `/api/v1/health`, нужен `Authorization: Bearer <token>`; иначе 401.
    pub fn require_token(&self, token: &str) {
        let t = token.to_owned();
        self.with(|s| s.token = Some(t));
    }

    /// Любой запрос, кроме `/api/v1/health`, получает 307 на `base` + тот же путь.
    pub fn redirect_to(&self, base: &str) {
        let b = base.to_owned();
        self.with(|s| s.redirect = Some(b));
    }

    /// Решение `guid` доступно: `GET /api/v1/solutions/{guid}` отвечает 200.
    pub fn add_solution(&self, guid: &str, name: &str, role: &str, access: &str) {
        let v = json!({
            "id": format!("sp-{guid}"),
            "kind": "solution",
            "name": name,
            "agent_solution_id": guid,
            "owner": {"id": "u1", "name": "owner"},
            "role": role,
            "access": access,
            "zz_future": 1,
        });
        let g = guid.to_owned();
        self.with(|s| {
            s.solutions.insert(g, v);
        });
    }

    /// Доступа к решению нет: 403 `/problems/solution-access` с `state`.
    pub fn forbid_solution(&self, guid: &str, state: &str) {
        let (g, st) = (guid.to_owned(), state.to_owned());
        self.with(|s| {
            s.forbidden.insert(g, st);
        });
    }

    /// Vault заблокирован: список отвечает 200 с `locked: true` и `reason`.
    pub fn lock_vault(&self, reason: &str) {
        let r = reason.to_owned();
        self.with(|s| s.vault_locked = Some(r));
    }

    /// Тип подсказок `kind` выключен у пользователя (`hooks.<kind> = false`).
    pub fn hook_off(&self, kind: &str) {
        let k = kind.to_owned();
        self.with(|s| s.hooks_off.push(k));
    }

    /// Путь `route` (без строки запроса) отвечает problem+json с кодом и типом.
    pub fn fail(&self, route: &str, status: u16, problem_type: &str, retry_after: Option<u64>) {
        let f = Failure {
            status,
            problem_type: problem_type.to_owned(),
            detail: format!("fake {status}"),
            retry_after,
        };
        let r = route.to_owned();
        self.with(|s| {
            s.failures.insert(r, f);
        });
    }

    /// Следующие вызовы инструментов MCP отвечают `isError: true` с текстом `text`.
    pub fn mcp_error(&self, text: &str) {
        let t = text.to_owned();
        self.with(|s| s.mcp_error = Some(t));
    }

    /// Все принятые к этому моменту запросы (канал опустошается).
    pub fn seen(&mut self) -> Vec<Seen> {
        let mut out = Vec::new();
        while let Ok(s) = self.rx.try_recv() {
            out.push(s);
        }
        out
    }
}

/// Как [`CutServer`] рвёт соединение.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// Запрос прочитан целиком, ответа нет: соединение закрыто (у клиента — `IncompleteMessage`).
    BeforeReply,
    /// Ответ `200` с `Content-Length` больше отданного тела, затем закрытие: обрыв тела.
    MidBody,
}

/// Сервер на `127.0.0.1:0`, который принимает запрос и рвёт соединение (D154: «обрыв
/// соединения после записи»; M1 ревью задачи 2). Блокирующий сокет — в своём потоке ОС, не
/// в рантайме теста. В канал уходит строка запроса (`POST /api/v1/x`) до обрыва: «запрос
/// дошёл до сервера» тест проверяет каналом, а не таймером.
pub struct CutServer {
    pub addr: SocketAddr,
    pub rx: std::sync::mpsc::Receiver<String>,
}

impl CutServer {
    pub fn start(cut: Cut) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { return };
                let Some(line) = read_request(&mut s) else {
                    continue;
                };
                if tx.send(line).is_err() {
                    return;
                }
                if cut == Cut::MidBody {
                    use std::io::Write;
                    let _ = s.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{\"text\":",
                    );
                    let _ = s.flush();
                }
                // Закрытие: запрос прочитан целиком, поэтому уходит FIN, а не RST.
                drop(s);
            }
        });
        Self { addr, rx }
    }

    /// Базовый адрес `http://127.0.0.1:<порт>`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Строки принятых к этому моменту запросов (канал опустошается).
    pub fn seen(&mut self) -> Vec<String> {
        self.rx.try_iter().collect()
    }
}

/// Строка запроса (`METHOD /path`) после чтения заголовков и тела `Content-Length`.
fn read_request(s: &mut std::net::TcpStream) -> Option<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = s.read(&mut chunk).ok().filter(|n| *n > 0)?;
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let length = head
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < head_end + length {
        let n = s.read(&mut chunk).ok().filter(|n| *n > 0)?;
        buf.extend_from_slice(&chunk[..n]);
    }
    let first = head.lines().next().unwrap_or("");
    Some(first.rsplit_once(' ').map_or(first, |(l, _)| l).to_owned())
}

/// Адрес `http://127.0.0.1:<порт>`, на котором никто не слушает: порт занят и освобождён.
pub fn closed_port_url() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    format!("http://{addr}")
}

async fn handle(req: Request, tx: &mpsc::UnboundedSender<Seen>, state: &Mutex<State>) -> Response {
    let method = req.method().as_str().to_owned();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_default();
    let headers = req.headers().clone();
    let bytes = axum::body::to_bytes(req.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let (token, redirect) = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        (s.token.clone(), s.redirect.clone())
    };
    let auth = headers
        .get(header::AUTHORIZATION)
        .map(|v| v.as_bytes().to_vec());
    let authorized = match (&token, &auth) {
        (Some(t), Some(a)) => a.as_slice() == format!("Bearer {t}").as_bytes(),
        (None, _) => true,
        (Some(_), None) => false,
    };
    let seen = Seen {
        method: method.clone(),
        path: path.clone(),
        headers: headers
            .iter()
            .filter(|(k, _)| *k != header::AUTHORIZATION)
            .map(|(k, v)| (k.as_str().to_owned(), v.as_bytes().to_vec()))
            .collect(),
        authorized,
        has_authorization: auth.is_some(),
        body: body.clone(),
    };
    let _ = tx.send(seen);
    let route = path.split('?').next().unwrap_or("").to_owned();

    if method == "GET" && route == "/api/v1/health" {
        let h = state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .health
            .clone();
        return health(&h);
    }
    if route == "/health" || (!route.starts_with("/api/") && route != "/mcp") {
        return html();
    }
    if let Some(base) = redirect {
        let mut r = text(307, "text/plain", "");
        r.headers_mut().insert(
            header::LOCATION,
            HeaderValue::from_str(&format!("{base}{path}")).unwrap(),
        );
        return r;
    }
    if !authorized {
        let mut r = problem(401, "about:blank", "требуется аутентификация");
        r.headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return r;
    }
    let failure = state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .failures
        .get(&route)
        .cloned();
    if let Some(f) = failure {
        let mut r = problem(f.status, &f.problem_type, &f.detail);
        if let Some(s) = f.retry_after {
            r.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(s));
        }
        return r;
    }
    if route == "/mcp" {
        return mcp(&headers, &body, state);
    }
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    match (method.as_str(), route.as_str()) {
        ("GET", "/api/v1/spaces") => json_reply(
            200,
            json!({
                "spaces": [{"id": "knowledge", "kind": "knowledge", "name": "Knowledge", "role": "personal"}],
                "pending": [],
            }),
        ),
        ("GET", r) if r.starts_with("/api/v1/solutions/") => {
            let g = &r["/api/v1/solutions/".len()..];
            if let Some(st) = s.forbidden.get(g) {
                let mut v = problem_body(
                    403,
                    "/problems/solution-access",
                    &format!("нет доступа к решению {g}: запрос ждёт подтверждения владельца"),
                );
                v["state"] = json!(st);
                v["agent_solution_id"] = json!(g);
                problem_json(403, v)
            } else if let Some(v) = s.solutions.get(g) {
                json_reply(200, v.clone())
            } else {
                problem(404, "about:blank", "ресурс не найден")
            }
        }
        ("GET", r) if r.starts_with("/api/v1/spaces/") && r.ends_with("/vault") => {
            // `?name=` и прочие параметры молча игнорируются (CR-24).
            let mut v = json!({"items": []});
            match &s.vault_locked {
                Some(reason) => {
                    v["locked"] = json!(true);
                    v["reason"] = json!(reason);
                }
                None => v["locked"] = json!(false),
            }
            json_reply(200, v)
        }
        ("POST", "/api/v1/recall") => {
            let mut v = json!({
                "text": "",
                "items": [],
                "tokens": 0,
                "rules_tokens": 0,
                "timings": {"collect_ms": 0, "record_ms": null, "total_ms": 0},
                "warnings": [],
            });
            if body["event"] == "session_start" {
                let hooks: serde_json::Map<String, Value> = HOOK_KINDS
                    .iter()
                    .map(|k| ((*k).to_owned(), json!(!s.hooks_off.iter().any(|o| o == k))))
                    .collect();
                v["hooks"] = Value::Object(hooks);
            }
            json_reply(200, v)
        }
        _ => problem(404, "about:blank", "ресурс не найден"),
    }
}

fn health(h: &Health) -> Response {
    let v = match h {
        Health::M6 => json!({
            "status": "ok",
            "version": VERSION_M6,
            "git": GIT_M6,
            "features": FEATURES_M6,
            "token_counter": "model",
            "tokenizer_id": "qwen3",
            "uptime_s": 12,
        }),
        Health::M65 => json!({
            "status": "ok",
            "version": VERSION_M65,
            "git": GIT_M65,
            "features": features_m65(),
            "token_counter": "model",
            "tokenizer_id": "qwen3",
            "uptime_s": 7,
            "zz_future_field": {"x": 1},
        }),
        Health::NoFeatures => json!({
            "status": "ok",
            "version": VERSION_M57,
            "git": "13b0bac",
            "uptime_s": 3,
        }),
        Health::Html => return html(),
        Health::Status(code) => return problem(*code, "about:blank", "fake health failure"),
        Health::Json(v) => v.clone(),
    };
    json_reply(200, v)
}

/// `POST /mcp`: проверка `Accept` (406), JSON-RPC `tools/call` и `initialize`.
fn mcp(headers: &HeaderMap, body: &Value, state: &Mutex<State>) -> Response {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let types: Vec<&str> = accept.split(',').map(|t| t.trim()).collect();
    if !(types.contains(&"application/json") && types.contains(&"text/event-stream")) {
        return text(
            406,
            "text/plain",
            "Not Acceptable: Client must accept both application/json and text/event-stream",
        );
    }
    let id = body["id"].clone();
    let rpc = |payload: (&str, Value)| {
        let mut v = json!({"jsonrpc": "2.0", "id": id});
        v[payload.0] = payload.1;
        json_reply(200, v)
    };
    match body["method"].as_str() {
        Some("initialize") => rpc((
            "result",
            json!({
                "protocolVersion": "2026-07-28",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "zam-fake", "version": VERSION_M6},
            }),
        )),
        Some("tools/call") => {
            let name = body["params"]["name"].as_str().unwrap_or("");
            let args = &body["params"]["arguments"];
            if !MCP_TOOLS.contains(&name) {
                return rpc((
                    "error",
                    json!({"code": -32602, "message": format!("tool not found: {name}")}),
                ));
            }
            let session = headers
                .get("x-zam-session")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let arg = args["session_id"].as_str().map(str::to_owned);
            if SESSION_ARG_TOOLS.contains(&name)
                && let (Some(a), Some(h)) = (&arg, &session)
                && a != h
            {
                return tool_result(
                    rpc,
                    "Ошибка 400 Bad Request: session_id и X-Zam-Session различаются: передайте одну сессию\n~14 tok",
                    true,
                );
            }
            let err = state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mcp_error
                .clone();
            match err {
                Some(t) => tool_result(rpc, &t, true),
                None => tool_result(rpc, &format!("# {name}\nfake result\n~4 tok"), false),
            }
        }
        _ => rpc((
            "error",
            json!({"code": -32601, "message": "method not found"}),
        )),
    }
}

fn tool_result(rpc: impl Fn((&str, Value)) -> Response, text: &str, is_error: bool) -> Response {
    rpc((
        "result",
        json!({"content": [{"type": "text", "text": text}], "isError": is_error}),
    ))
}

fn html() -> Response {
    text(
        200,
        "text/html; charset=utf-8",
        "<!doctype html><html><head><title>ZeroAIMemory</title></head><body><div id=\"app\"></div></body></html>",
    )
}

/// Тело problem+json, как `ApiError::into_response` ZAM.
fn problem_body(status: u16, problem_type: &str, detail: &str) -> Value {
    let title = StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("Error");
    json!({"type": problem_type, "title": title, "status": status, "detail": detail})
}

fn problem(status: u16, problem_type: &str, detail: &str) -> Response {
    problem_json(status, problem_body(status, problem_type, detail))
}

fn problem_json(status: u16, v: Value) -> Response {
    let mut r = json_reply(status, v);
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    r
}

fn json_reply(status: u16, v: Value) -> Response {
    text(status, "application/json", &v.to_string())
}

fn text(status: u16, content_type: &str, body: &str) -> Response {
    let mut r = Response::new(Body::from(body.to_owned()));
    *r.status_mut() = StatusCode::from_u16(status).unwrap();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).unwrap(),
    );
    r
}
