//! Ошибки клиента памяти (R1-M D137): вид по коду и `type` problem+json REST, таблица
//! четырёх форм текста ошибок MCP.
//!
//! Тексты ошибок — без тела запроса и без заголовков: только код, `title`, `detail` и `code`
//! ответа сервера (`detail` обрезается), у не-JSON тела — короткий `text/plain` или ничего.

use std::fmt;
use std::time::Duration;

/// Предел `detail` ошибки, символов.
const DETAIL_MAX: usize = 1000;
/// Предел исходного текста нераспознанной ошибки MCP, символов (D137).
const MCP_RAW_MAX: usize = 500;
/// Предел тела `text/plain` в `detail`, символов.
const PLAIN_MAX: usize = 300;

/// Вид ошибки сервера памяти (D137).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// Не соединились (в том числе не уложились в срок соединения): запрос до сервера не
    /// дошёл.
    Offline,
    /// Срок запроса вышел.
    Timeout,
    /// Соединение оборвалось после отправки, ответа со статусом нет (сброс, `IncompleteMessage`,
    /// обрыв тела ответа): исход неизвестен — сервер мог выполнить запрос. Очередь записей
    /// (задача 7) повторяет `Interrupted` только у записи с `Idempotency-Key`: повтор с тем же
    /// ключом не задваивает запись; без ключа — не повторяет.
    Interrupted,
    /// Отвечает не API ZeroAIMemory (HTML WebUI, не JSON, нет `version`).
    NotZam,
    /// Нарушение протокола: любой 3xx, неожиданный код, ошибка JSON-RPC, неразобранный ответ.
    Protocol,
    /// 401: токен неверен или отозван.
    Auth,
    /// 429 `/problems/rate-limited` (`Retry-After`).
    RateLimited,
    /// 403, в том числе `solution-access`, `idea-*`.
    Forbidden,
    /// 404, в том числе `session-not-found`.
    NotFound,
    /// 408 `/problems/timeout`: тело запроса не дошло в срок.
    RequestTimeout,
    /// 409: `conflict`, `possible-duplicate`, `task-claimed`, `already-answered`, `idea-*` и др.
    Conflict,
    /// 428: правка без `If-Match`.
    PreconditionRequired,
    /// 422: `unprocessable`, `idempotency-mismatch`, `certainty` (с `code`) и др.
    Unprocessable,
    /// 413.
    TooLarge,
    /// 503 `/problems/busy` (`Retry-After`).
    Busy,
    /// 503 `/problems/vault-locked`.
    VaultLocked,
    /// 400 `/problems/session-required`.
    SessionRequired,
    /// 400, в том числе `by-required`; неверные аргументы MCP.
    BadRequest,
    /// 500, 503 `about:blank`, 507 и прочие 5xx — временная ошибка сервера.
    Server,
}

impl ErrorKind {
    /// Машинное имя вида (журнал, события).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Timeout => "timeout",
            Self::Interrupted => "interrupted",
            Self::NotZam => "not_zam",
            Self::Protocol => "protocol",
            Self::Auth => "auth",
            Self::RateLimited => "rate_limited",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::RequestTimeout => "request_timeout",
            Self::Conflict => "conflict",
            Self::PreconditionRequired => "precondition_required",
            Self::Unprocessable => "unprocessable",
            Self::TooLarge => "too_large",
            Self::Busy => "busy",
            Self::VaultLocked => "vault_locked",
            Self::SessionRequired => "session_required",
            Self::BadRequest => "bad_request",
            Self::Server => "server",
        }
    }

    /// Вид по коду ответа и `type` problem+json (`crates/zam-server/src/error.rs` e391440):
    /// `type` различает только 400 `session-required` и 503 `busy`/`vault-locked`; прочие
    /// коды — по коду (у `about:blank` других признаков нет). 3xx и неожиданные коды —
    /// [`ErrorKind::Protocol`].
    pub fn from_status(status: u16, problem_type: Option<&str>) -> Self {
        let name = problem_type.and_then(problem_name);
        match status {
            400 if name == Some("session-required") => Self::SessionRequired,
            400 => Self::BadRequest,
            401 => Self::Auth,
            403 => Self::Forbidden,
            404 => Self::NotFound,
            408 => Self::RequestTimeout,
            409 => Self::Conflict,
            413 => Self::TooLarge,
            422 => Self::Unprocessable,
            428 => Self::PreconditionRequired,
            429 => Self::RateLimited,
            503 if name == Some("busy") => Self::Busy,
            503 if name == Some("vault-locked") => Self::VaultLocked,
            500..=599 => Self::Server,
            _ => Self::Protocol,
        }
    }
}

/// Id записи сервера: 1–64 символа, только буквы ASCII, цифры, `_` и `-` (ULID ZAM — 26
/// символов): в текст модели не попадёт чужая строка из тела ошибки.
fn is_plain_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Имя problem из `type`: последний сегмент после `/problems/`; `about:blank` — `None`.
fn problem_name(ty: &str) -> Option<&str> {
    ty.rsplit_once("/problems/").map(|(_, n)| n)
}

/// Ошибка обращения к серверу памяти (D137).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ZamError {
    pub kind: ErrorKind,
    /// Код HTTP; у ошибок соединения и форм MCP без кода — `None`.
    pub status: Option<u16>,
    /// `type` problem+json как пришёл.
    pub problem_type: Option<Box<str>>,
    pub title: String,
    pub detail: String,
    /// Машинный код (`code` problem+json, `(код …)` текста MCP).
    pub code: Option<String>,
    /// `Retry-After` в секундах (429, 503 `busy`).
    pub retry_after: Option<Duration>,
    /// Поля конфликта записи (409): кандидаты в дубли и текущая ревизия; нет — `None`. В
    /// куче: ошибка остаётся малой (`clippy::result_large_err`).
    pub conflict: Option<Box<Conflict>>,
}

/// Поля ответа 409, которые нужны шлюзу записи (D145, MEM-073).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Conflict {
    /// Id кандидатов в дубли (`candidates[].id` у `possible-duplicate`, до
    /// [`CANDIDATES_MAX`]): шлюз называет их модели. Только id, без заголовков записей.
    pub candidates: Vec<String>,
    /// Текущая ревизия цели (`rev` у `conflict`, `current_rev` у правки заметки): шлюз отдаёт
    /// её модели при конфликте правки секции и повторяет с ней `append`.
    pub rev: Option<i64>,
}

/// Предел id кандидатов в дубли в [`Conflict::candidates`] (как у сервера,
/// `DEDUP_CANDIDATES_MAX` ZAM).
pub const CANDIDATES_MAX: usize = 5;

impl fmt::Display for ZamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(s) = self.status {
            write!(f, "{s} ")?;
        }
        f.write_str(&self.title)?;
        if !self.detail.is_empty() {
            write!(f, ": {}", self.detail)?;
        }
        if let Some(c) = &self.code {
            write!(f, " (code {c})")?;
        }
        Ok(())
    }
}

impl ZamError {
    /// Ошибка вида `kind` с заголовком `title`, без кода и подробностей.
    pub fn new(kind: ErrorKind, title: impl Into<String>) -> Self {
        Self {
            kind,
            status: None,
            problem_type: None,
            title: title.into(),
            detail: String::new(),
            code: None,
            retry_after: None,
            conflict: None,
        }
    }

    /// Имя problem из `type` (`possible-duplicate`, `idea-mode`, …); `about:blank` и ошибки без
    /// `type` — `None`.
    pub fn problem(&self) -> Option<&str> {
        self.problem_type.as_deref().and_then(problem_name)
    }

    /// Id кандидатов в дубли (409 `possible-duplicate`).
    pub fn candidates(&self) -> &[String] {
        self.conflict.as_deref().map_or(&[], |c| &c.candidates)
    }

    /// Текущая ревизия цели из ответа 409.
    pub fn current_rev(&self) -> Option<i64> {
        self.conflict.as_deref().and_then(|c| c.rev)
    }

    /// С подробностью `detail` (обрезается до предела).
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = truncate(&detail.into(), DETAIL_MAX);
        self
    }

    /// Ответ с кодом `status` не 2xx: тело problem+json (или просто JSON с `title`,
    /// `detail`) — поля как есть; `text/plain` — короткая подробность; прочее тело (HTML
    /// прокси) не цитируется. Заголовок по умолчанию — каноническое имя кода.
    pub fn from_problem(
        status: u16,
        content_type: &str,
        body: &[u8],
        retry_after: Option<Duration>,
    ) -> Self {
        let parsed = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .filter(serde_json::Value::is_object);
        let str_of = |k: &str| {
            parsed
                .as_ref()
                .and_then(|v| v.get(k))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let problem_type: Option<Box<str>> = str_of("type").map(String::into_boxed_str);
        let title = str_of("title")
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| canonical(status).to_owned());
        let detail = match (&parsed, str_of("detail")) {
            (Some(_), d) => d.unwrap_or_default(),
            (None, _) if content_type.trim_start().starts_with("text/plain") => {
                let text = String::from_utf8_lossy(body);
                truncate(&one_line(&text), PLAIN_MAX)
            }
            (None, _) => String::new(),
        };
        let candidates: Vec<String> = parsed
            .as_ref()
            .and_then(|v| v.get("candidates"))
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.get("id").and_then(serde_json::Value::as_str))
                    .filter(|id| is_plain_id(id))
                    .take(CANDIDATES_MAX)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let rev = parsed.as_ref().and_then(|v| {
            v.get("rev")
                .or_else(|| v.get("current_rev"))
                .and_then(serde_json::Value::as_i64)
        });
        let conflict = (status == 409 && (!candidates.is_empty() || rev.is_some()))
            .then(|| Box::new(Conflict { candidates, rev }));
        Self {
            kind: ErrorKind::from_status(status, problem_type.as_deref()),
            status: Some(status),
            problem_type,
            title: truncate(&title, 200),
            detail: truncate(&detail, DETAIL_MAX),
            code: str_of("code").map(|c| truncate(&c, 100)),
            retry_after,
            conflict,
        }
    }

    /// Нет ответа ZAM: не соединились, срок вышел, соединение оборвалось без ответа, отвечает
    /// не ZAM.
    pub fn is_unreachable(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Offline | ErrorKind::Timeout | ErrorKind::Interrupted | ErrorKind::NotZam
        )
    }

    /// Заменяет `secret` в текстах ошибки на `[token]` (защита в глубину: сервер, который
    /// повторил бы заголовок в ответе, не выведет токен в журнал).
    pub(crate) fn scrub(mut self, secret: &str) -> Self {
        if secret.is_empty() {
            return self;
        }
        for s in [&mut self.title, &mut self.detail] {
            if s.contains(secret) {
                *s = s.replace(secret, "[token]");
            }
        }
        if let Some(c) = &mut self.code
            && c.contains(secret)
        {
            *c = c.replace(secret, "[token]");
        }
        self
    }
}

/// Каноническое имя кода HTTP — своей таблицей: вне `transport.rs` типов `reqwest` нет
/// (страж `cargo xtask policy`).
fn canonical(status: u16) -> &'static str {
    match status {
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        507 => "Insufficient Storage",
        _ => "Error",
    }
}

/// Текст одной строкой: переводы строк и табуляции — пробелы.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Первые `max` символов `s`.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => s[..i].to_owned(),
        None => s.to_owned(),
    }
}

/// Последняя строка `~N tok` ответа MCP снимается.
fn strip_tokens_line(text: &str) -> &str {
    let t = text.trim_end();
    match t.rsplit_once('\n') {
        Some((head, last)) if is_tokens_line(last) => head.trim_end(),
        None if is_tokens_line(t) => "",
        _ => t,
    }
}

fn is_tokens_line(line: &str) -> bool {
    line.trim()
        .strip_prefix('~')
        .and_then(|l| l.strip_suffix(" tok"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Текст ошибки инструмента MCP (`isError: true`, HTTP 200) в [`ZamError`] — таблица
/// четырёх форм `fail_text`/`problem_text` (`crates/zam-server/src/mcp/tools.rs:574–610`
/// e391440, без изменений в 4459dda). Последняя строка `~N tok` снимается:
///
/// 1. `Ошибка <код> <title>[: <detail>][ (код <c>)]` — вид по коду (`type` в тексте
///    теряется), `code` — из скобок; строки после первой (подсказка, текущая ревизия у 409)
///    — в `detail` целиком;
/// 2. `Ошибка аргументов: <m>` — [`ErrorKind::BadRequest`] без кода;
/// 3. `Внутренняя ошибка MCP: <m>` — [`ErrorKind::Server`];
/// 4. прочее — [`ErrorKind::Protocol`] с исходным текстом (до 500 символов).
///
/// Разбор текста — обход до CR-25 (машинный код ошибки в MCP).
pub fn parse_mcp_error(text: &str) -> ZamError {
    let body = strip_tokens_line(text);
    let (first, rest) = match body.split_once('\n') {
        Some((f, r)) => (f.trim_end(), r.trim()),
        None => (body.trim_end(), ""),
    };
    let join = |head: &str| -> String {
        match (head.is_empty(), rest.is_empty()) {
            (_, true) => head.to_owned(),
            (true, false) => rest.to_owned(),
            (false, false) => format!("{head}\n{rest}"),
        }
    };
    if let Some(m) = first.strip_prefix("Ошибка аргументов: ") {
        return ZamError::new(ErrorKind::BadRequest, "invalid MCP arguments").with_detail(join(m));
    }
    if let Some(m) = first.strip_prefix("Внутренняя ошибка MCP: ") {
        return ZamError::new(ErrorKind::Server, "internal MCP error").with_detail(join(m));
    }
    if let Some(e) = first
        .strip_prefix("Ошибка ")
        .and_then(|r| problem_line(r, &join))
    {
        return e;
    }
    ZamError::new(ErrorKind::Protocol, "unrecognized MCP error")
        .with_detail(truncate(text.trim(), MCP_RAW_MAX))
}

/// Форма 1 после `Ошибка `: `<код> <title>[: <detail>][ (код <c>)]`.
fn problem_line(r: &str, join: &dyn Fn(&str) -> String) -> Option<ZamError> {
    let (code_s, rest) = r.split_once(' ')?;
    if code_s.len() != 3 || !code_s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let status: u16 = code_s.parse().ok()?;
    let (rest, code) = match rest.strip_suffix(')').and_then(|r| r.rsplit_once(" (код ")) {
        Some((head, c)) if !c.is_empty() && !c.contains(' ') => (head, Some(c.to_owned())),
        _ => (rest, None),
    };
    let (title, detail) = match rest.split_once(": ") {
        Some((t, d)) => (t.trim(), d.trim()),
        None => (rest.trim(), ""),
    };
    let mut e =
        ZamError::new(ErrorKind::from_status(status, None), title).with_detail(join(detail));
    e.status = Some(status);
    e.code = code;
    Some(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_line_is_stripped_only_when_exact() {
        assert_eq!(strip_tokens_line("a\n~12 tok"), "a");
        assert_eq!(strip_tokens_line("a\n~12 tok\n"), "a");
        assert_eq!(strip_tokens_line("a\n~x tok"), "a\n~x tok");
        assert_eq!(strip_tokens_line("~3 tok"), "");
    }

    #[test]
    fn display_has_no_body_beyond_fields() {
        let e = ZamError::from_problem(
            401,
            "application/problem+json",
            r#"{"type":"about:blank","title":"Unauthorized","status":401,"detail":"требуется аутентификация"}"#
                .as_bytes(),
            None,
        );
        assert_eq!(e.to_string(), "401 Unauthorized: требуется аутентификация");
        let e = ZamError::from_problem(406, "text/plain", b"Not Acceptable:\n need both", None);
        assert_eq!(e.kind, ErrorKind::Protocol);
        assert_eq!(e.detail, "Not Acceptable: need both");
        let s = ZamError::new(ErrorKind::Auth, "x")
            .with_detail("echo zam_SECRET")
            .scrub("zam_SECRET");
        assert_eq!(s.detail, "echo [token]");
    }
}
