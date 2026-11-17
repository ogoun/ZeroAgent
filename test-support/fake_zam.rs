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
//!   ([`Health`]), требуемый токен, 3xx на другой сервер, ответ-ошибка или иное тело по
//!   пути, решение и отказ в доступе к нему, блокировка Vault, выключенные типы подсказок,
//!   ошибка MCP; обрыв соединения — отдельный [`CutServer`].
//! - **Сессии и `/recall`** (задача 4): `POST /api/v1/sessions` со строгим телом, как
//!   `NewSession` ZAM (`deny_unknown_fields`, `api/sessions.rs@v0.0.12-m6.5`) — лишнее поле
//!   даёт 400; ответ `/recall` по событию задаёт тест ([`RecallAnswer`]: текст, `items`,
//!   `warnings`, `tokens`); выключенный тип подсказок — пустой `text` без сбора, `hooks` — только
//!   у `session_start`; задержка ответа `/recall` — до [`FakeZam::release_recall`] (ворота, а не
//!   таймер); дайджест формы `0.0.12-m6.5` в режиме `review` — [`digest_m65_review`].
//! - **«Недоступный» сервер** без гонки за порт (Minor 9 ревью задачи 4):
//!   [`FakeZam::refuse_connections`] — порт остаётся за сервером, а принятое соединение
//!   закрывается до чтения запроса; запроса в канале нет.
//!
//! - **Vault** (задача 5): записи по пространству (`knowledge` или GUID решения,
//!   [`FakeZam::add_vault`]) — список метаданных формы ZAM (`api/vault.rs@v0.0.12-m6.5`:
//!   `id`, `scope`, `title`, `data.type`, `rev`, `space`, `agent_solution_id`, верхние
//!   `locked` и `reason`); `?name=` и `?q=` молча игнорируются; `GET /api/v1/vault/{id}` —
//!   метаданные, с `?reveal=1` — и `secret` (`null` у записи окружения без значения) с
//!   `Cache-Control: no-store`; при блокировке ([`FakeZam::lock_vault`]) раскрытие — 503
//!   `/problems/vault-locked`, метаданные — 200 с `locked: true`. Раскрытия с `X-Zam-Session`
//!   — в [`FakeZam::reveals`]; 403 `solution-access` у Vault решения — [`FakeZam::forbid_solution`].
//!
//! - **Записи** (задача 6, формы `@v0.0.12-m6.5`): журнал `POST /api/v1/spaces/{s}/entries`
//!   с `Idempotency-Key` (201, повтор того же запроса — 200, другой запрос — 422
//!   `/problems/idempotency-mismatch`), 409 `possible-duplicate` с `candidates`
//!   ([`FakeZam::duplicate_title`]); `GET`/`PATCH /api/v1/entries/{id}` (без `If-Match` — 428,
//!   устаревшая ревизия — 409 `conflict` с `rev`, `by` — только у правки идеи); `claim`,
//!   `release`, `supersede` (с ключом), `retract`, `answer` с проверкой `by` (нет — 400
//!   `/problems/by-required`), `propose`, `decide`, `review` (409 `idea-mode` в обе стороны
//!   по [`FakeZam::set_idea_mode`], повтор по ключу — `replay: true`, ключ после правки идеи —
//!   422); `POST /api/v1/cache`; заметки `POST /api/v1/nodes` с ключом, `GET` по id и по
//!   пути, `PUT` с `If-Match`, `PATCH {base_rev, ops}`, ревизии с сессией автора. Подмены:
//!   обрыв соединения после записи ([`FakeZam::cut_after_write`]: запись сделана, тело ответа
//!   рвётся — у клиента `Interrupted`), правка идеи «человеком» после обрыва, одноразовый
//!   отказ ([`FakeZam::fail_once`]), предупреждения следующей записи, ворота записей
//!   ([`FakeZam::hold_writes`]).
//!
//! Задачи 7–9 дополняют сервер outbox-приёмником и снимками.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use http_body::Frame;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

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

    /// Сколько раз пришёл заголовок `name` (строчными): ловушка `RequestBuilder::header`
    /// добавляет второй, а не заменяет первый.
    pub fn header_count(&self, name: &str) -> usize {
        self.headers.iter().filter(|(k, _)| k == name).count()
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
    replies: BTreeMap<String, Value>,
    mcp_error: Option<String>,
    /// Ответы `/recall` по событию (`session_start`, `prompt`, …).
    recall: BTreeMap<String, RecallAnswer>,
    /// Тела принятых `POST /api/v1/sessions`.
    sessions: Vec<Value>,
    /// Записи Vault.
    vault: Vec<VaultRow>,
    /// Раскрытия `?reveal=1`: id записи и `X-Zam-Session`.
    reveals: Vec<(String, Option<String>)>,
    /// Записи журнала по id (форма `Entry` ZAM).
    entries: BTreeMap<String, Value>,
    /// Заметки по id.
    nodes: BTreeMap<String, NodeRow>,
    /// Ключи идемпотентности: отпечаток запроса и id цели.
    idem: BTreeMap<String, (String, String)>,
    /// Счётчик id.
    next_id: u64,
    /// Маршруты, у которых следующий ответ записи рвётся после записи.
    cut: BTreeSet<String>,
    /// Запись, которую «человек» правит сразу после обрыва (ревизия растёт).
    touch_after_cut: Option<String>,
    /// Одноразовые отказы по маршруту.
    fail_once: BTreeMap<String, VecDeque<Failure>>,
    /// Предупреждения следующего ответа записи.
    warnings_next: Vec<String>,
    /// Режим идей владельца базы: `review` (умолчание сервера) или `propose`.
    idea_mode: String,
    /// Заголовки, на которые запись журнала получает 409 `possible-duplicate`: id кандидата.
    duplicates: BTreeMap<String, String>,
    /// Тела принятых `POST /api/v1/cache`.
    caches: Vec<Value>,
}

/// Заметка поддельного сервера.
#[derive(Debug, Clone)]
struct NodeRow {
    id: String,
    space: String,
    path: String,
    content: String,
    rev: i64,
    /// Ревизии новыми первыми: `{rev, session_id, author_kind}`.
    revisions: Vec<Value>,
}

impl NodeRow {
    fn json(&self) -> Value {
        json!({
            "id": self.id,
            "space_id": space_id(&self.space),
            "kind": "note",
            "name": self.path.rsplit('/').next().unwrap_or(""),
            "path": self.path,
            "current_rev": self.rev,
            "certainty": frontmatter_certainty(&self.content),
        })
    }
}

/// Id пространства (26 символов, как у ZAM) по ссылке на него.
fn space_id(space: &str) -> String {
    let mut s: String = space
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    s.insert_str(0, "SP");
    s.truncate(26);
    while s.len() < 26 {
        s.push('0');
    }
    s
}

/// `certainty:` frontmatter заметки (первый блок `---`).
fn frontmatter_certainty(text: &str) -> Option<String> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    rest[..end]
        .lines()
        .find_map(|l| l.strip_prefix("certainty:"))
        .map(|c| c.trim().to_owned())
}

/// Запись Vault поддельного сервера.
#[derive(Debug, Clone)]
struct VaultRow {
    id: String,
    /// `knowledge` или GUID решения.
    space: String,
    title: String,
    item_type: String,
    secret: Option<String>,
}

impl VaultRow {
    /// Метаданные формы `meta_json` ZAM: без значения.
    fn meta(&self) -> Value {
        let solution = (self.space != "knowledge").then(|| self.space.clone());
        json!({
            "id": self.id,
            "scope": if solution.is_some() { "solution" } else { "knowledge" },
            "title": self.title,
            "data": {"type": self.item_type, "description": "synthetic"},
            "rev": 1,
            "author": {"kind": "user", "user_id": "u1", "token_id": null, "session_id": null, "machine": null},
            "created_at": "2026-11-17T00:00:00Z",
            "updated_at": "2026-11-17T00:00:00Z",
            "space": self.space,
            "agent_solution_id": solution,
        })
    }
}

/// Поля тела `POST /sessions` (`NewSession`, `api/sessions.rs@v0.0.12-m6.5`,
/// `deny_unknown_fields`): прочие — 400.
pub const SESSION_FIELDS: &[&str] = &[
    "id",
    "agent",
    "solution",
    "area",
    "parent_id",
    "agent_type",
    "tokens",
    "new_epoch",
    "holdout",
];

/// Ответ `/recall` события, заданный тестом: `text`, `items`, `warnings`, `tokens`,
/// `rules_tokens`. Прочие поля (`timings`, `hooks` у `session_start`) сервер добавляет сам.
#[derive(Debug, Clone, Default)]
pub struct RecallAnswer {
    pub text: String,
    pub items: Vec<Value>,
    pub warnings: Vec<String>,
    /// `tokens` ответа; `None` — `ceil(байты / 4)` текста.
    pub tokens: Option<u32>,
    pub rules_tokens: u32,
}

impl RecallAnswer {
    /// Ответ с текстом `text`.
    pub fn text(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            ..Self::default()
        }
    }

    /// С элементами `items[]` ([`recall_item`]).
    pub fn items(mut self, items: Vec<Value>) -> Self {
        self.items = items;
        self
    }

    /// С предупреждением `w` (`recall timeout`, `solution unavailable`, …).
    pub fn warning(mut self, w: &str) -> Self {
        self.warnings.push(w.to_owned());
        self
    }

    /// С полем `tokens` — счётом сервера своим токенизатором.
    pub fn tokens(mut self, n: u32) -> Self {
        self.tokens = Some(n);
        self
    }
}

/// Элемент `items[]` ответа `/recall` (`api/recall.rs@v0.0.12-m6.5`): ссылка, вид, заголовок,
/// надёжность и `reason`.
pub fn recall_item(id: &str, kind: &str, reason: &str) -> Value {
    json!({"id": id, "kind": kind, "title": "synthetic", "certainty": null, "reason": reason})
}

/// Синтетические ссылки дайджеста формы `@m6.5` ([`digest_m65_review`]).
pub const M65_TASK: &str = "k:01M8SYNTH00000000000000K01";
pub const M65_PLAN_NEXT: &str = "k:01M8SYNTH00000000000000K09";
pub const M65_IDEA_NEW: &str = "w:01M8SYNTH00000000000000W01";
pub const M65_IDEA_DEFERRED: &str = "w:01M8SYNTH00000000000000W02";
pub const M65_RULE: &str = "n:01M8SYNTH00000000000000R01";

/// Дайджест `session_start` формы `0.0.12-m6.5` в режиме `review` (П15; строки — по
/// `api/recall.rs@v0.0.12-m6.5`: `task_block`, `PLAN_LINE`, `IDEAS_REVIEW_HINT`, `idea_line`,
/// `idea_tail_line`, `rule_head`). Синтетический: записей рабочей памяти нет. Строка плана —
/// сразу за задачами без пустой строки; очередь разбора — `items[].reason: idea_review`,
/// хвост предложения — `reason: idea`.
pub fn digest_m65_review() -> RecallAnswer {
    let text = format!(
        "### Задачи\n\
         ZeroAIMemory: [in_progress] Синтетическая задача — далее: write test — {M65_TASK}\n\
         План из идей: 2 открытых, следующая — {M65_PLAN_NEXT} «Синтетический пункт плана» [high]\n\
         \n\
         ### Идеи на разбор\n\
         Непросмотренных идей: 1. Разбери, когда закончишь текущий шаг: memory_recall event=idea_review → memory_update_entry action=review.\n\
         ZeroAIMemory: идея «Синтетическая идея человека» [normal] — {M65_IDEA_NEW}\n\
         ZeroAIMemory: идея «Отложенная синтетическая идея» [low] — срок отсрочки вышел — {M65_IDEA_DEFERRED} (предложи человеку: action=propose — вопрос «начать / отложить / отказаться»; без ответа «начать» не начинай)\n\
         \n\
         ### Правила\n\
         proc/synthetic-rule [th] {M65_RULE}\n\
         \x20 Синтетическое правило: проверяй укладку дайджеста тестом.\n"
    );
    RecallAnswer::text(&text).items(vec![
        recall_item(M65_TASK, "task", "task"),
        recall_item(M65_IDEA_NEW, "idea", "idea_review"),
        recall_item(M65_IDEA_DEFERRED, "idea", "idea"),
    ])
}

/// Подсказка к `prompt` формы `0.0.12-m6.5` в режиме `review` (Δ11, `idea_review_prompt_line`):
/// строка идеи на разбор (`reason: idea_review`) и строка insight (`reason: hint`).
pub fn prompt_m65_review() -> RecallAnswer {
    let insight = "i:01M8SYNTH00000000000000I01";
    let text = format!(
        "ZeroAIMemory: к задаче подходит идея на разбор {M65_IDEA_NEW} «Синтетическая идея человека» — разбери, когда закончишь текущий шаг (memory_recall event=idea_review)\n\
         ZeroAIMemory: синтетическая ловушка укладки [th] {insight}\n"
    );
    RecallAnswer::text(&text).items(vec![
        recall_item(M65_IDEA_NEW, "idea", "idea_review"),
        recall_item(insight, "insight", "hint"),
    ])
}

/// Запущенный поддельный ZAM.
pub struct FakeZam {
    pub addr: SocketAddr,
    pub rx: mpsc::UnboundedReceiver<Seen>,
    state: Arc<Mutex<State>>,
    /// Ворота ответа `/recall`: `true` — ответ ждёт [`FakeZam::release_recall`].
    held: watch::Sender<bool>,
    /// Ворота записей: `true` — запись принята (она в канале), ответ ждёт
    /// [`FakeZam::release_writes`].
    held_writes: watch::Sender<bool>,
    /// `true` — принятые соединения закрываются до чтения запроса.
    refuse: Arc<AtomicBool>,
}

/// Слушатель поддельного сервера: при [`FakeZam::refuse_connections`] принятое соединение
/// закрывается сразу, до HTTP; порт остаётся занят сервером.
struct Gated {
    inner: tokio::net::TcpListener,
    refuse: Arc<AtomicBool>,
}

impl axum::serve::Listener for Gated {
    type Io = tokio::net::TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((s, _)) if self.refuse.load(Ordering::SeqCst) => drop(s),
                Ok(pair) => return pair,
                // Сбой `accept` (нехватка дескрипторов и т. п.) — повтор, как у axum.
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

impl FakeZam {
    /// Сервер на `127.0.0.1:0` с формой `0.0.11-m6`, без требуемого токена.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        Self::serve(listener)
    }

    fn serve(listener: tokio::net::TcpListener) -> Self {
        let state = Arc::new(Mutex::new(State {
            health: Health::M6,
            token: None,
            redirect: None,
            solutions: BTreeMap::new(),
            forbidden: BTreeMap::new(),
            vault_locked: None,
            hooks_off: Vec::new(),
            failures: BTreeMap::new(),
            replies: BTreeMap::new(),
            mcp_error: None,
            recall: BTreeMap::new(),
            sessions: Vec::new(),
            vault: Vec::new(),
            reveals: Vec::new(),
            entries: BTreeMap::new(),
            nodes: BTreeMap::new(),
            idem: BTreeMap::new(),
            next_id: 0,
            cut: BTreeSet::new(),
            touch_after_cut: None,
            fail_once: BTreeMap::new(),
            warnings_next: Vec::new(),
            idea_mode: "review".to_owned(),
            duplicates: BTreeMap::new(),
            caches: Vec::new(),
        }));
        let (held, gate) = watch::channel(false);
        let (held_writes, wgate) = watch::channel(false);
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = state.clone();
        let app = Router::new().fallback(move |req: Request| {
            let tx = tx.clone();
            let state = shared.clone();
            let gate = gate.clone();
            let wgate = wgate.clone();
            async move { handle(req, &tx, &state, gate, wgate).await }
        });
        let addr = listener.local_addr().unwrap();
        let refuse = Arc::new(AtomicBool::new(false));
        let gated = Gated {
            inner: listener,
            refuse: refuse.clone(),
        };
        tokio::spawn(async move {
            axum::serve(gated, app).await.unwrap();
        });
        Self {
            addr,
            rx,
            state,
            held,
            held_writes,
            refuse,
        }
    }

    /// `true` — сервер «недоступен»: соединение принимается и сразу закрывается, запрос не
    /// читается и в канал не попадает; `false` — обычная работа. Порт всё время за сервером,
    /// поэтому «сервер поднялся» не зависит от того, свободен ли порт.
    pub fn refuse_connections(&self, on: bool) {
        self.refuse.store(on, Ordering::SeqCst);
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

    /// Vault открыт: список — `locked: false`, раскрытие работает.
    pub fn unlock_vault(&self) {
        self.with(|s| s.vault_locked = None);
    }

    /// Запись Vault в пространстве `space` (`knowledge` или GUID решения): имя `title`, тип
    /// `item_type` (`token`, `host`, …), значение `secret` (`None` — запись окружения без
    /// значения). Итог — id записи.
    pub fn add_vault(
        &self,
        space: &str,
        title: &str,
        item_type: &str,
        secret: Option<&str>,
    ) -> String {
        self.with(|s| {
            let id = format!("01M8SYNTHVAULT{:012}", s.vault.len() + 1);
            s.vault.push(VaultRow {
                id: id.clone(),
                space: space.to_owned(),
                title: title.to_owned(),
                item_type: item_type.to_owned(),
                secret: secret.map(str::to_owned),
            });
            id
        })
    }

    /// Раскрытия (`GET /api/v1/vault/{id}?reveal=1`) с ответом 200: id записи и
    /// `X-Zam-Session`.
    pub fn reveals(&self) -> Vec<(String, Option<String>)> {
        self.with(|s| s.reveals.clone())
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

    /// Снимает подмену [`FakeZam::fail`] пути `route`: дальше — обычный ответ.
    pub fn clear_fail(&self, route: &str) {
        self.with(|s| {
            s.failures.remove(route);
        });
    }

    /// Путь `route` (без строки запроса) отвечает 200 с телом `body` вместо обычного ответа:
    /// форма, которой у сервера нет (поле пропало, тип другой).
    pub fn reply(&self, route: &str, body: Value) {
        let r = route.to_owned();
        self.with(|s| {
            s.replies.insert(r, body);
        });
    }

    /// Следующие вызовы инструментов MCP отвечают `isError: true` с текстом `text`.
    pub fn mcp_error(&self, text: &str) {
        let t = text.to_owned();
        self.with(|s| s.mcp_error = Some(t));
    }

    /// `/recall` события `event` отвечает `answer` (до замены).
    pub fn answer_recall(&self, event: &str, answer: RecallAnswer) {
        let e = event.to_owned();
        self.with(|s| {
            s.recall.insert(e, answer);
        });
    }

    /// Ответы `/recall` ждут [`FakeZam::release_recall`]: запрос принят (он уже в канале), ответа
    /// нет — клиент упирается в свой срок.
    pub fn hold_recall(&self) {
        self.held.send_replace(true);
    }

    /// Отпускает задержанные ответы `/recall`.
    pub fn release_recall(&self) {
        self.held.send_replace(false);
    }

    /// Тела принятых `POST /api/v1/sessions` (с верным телом).
    pub fn sessions(&self) -> Vec<Value> {
        self.with(|s| s.sessions.clone())
    }

    /// Запись журнала в пространстве `space` (GUID решения или `knowledge`) с полями `fields`
    /// поверх умолчаний (`kind`, `title`, `status` по виду, `rev: 1`). Итог — id.
    pub fn add_entry(&self, space: &str, fields: Value) -> String {
        self.with(|s| {
            let id = next_id(s, "ENTRY");
            let mut e = entry_json(&id, space, &fields, None);
            if let (Some(o), Some(f)) = (e.as_object_mut(), fields.as_object()) {
                for (k, v) in f {
                    o.insert(k.clone(), v.clone());
                }
            }
            s.entries.insert(id.clone(), e);
            id
        })
    }

    /// Запись журнала по id.
    pub fn entry(&self, id: &str) -> Option<Value> {
        self.with(|s| s.entries.get(id).cloned())
    }

    /// Все записи журнала.
    pub fn entries(&self) -> Vec<Value> {
        self.with(|s| s.entries.values().cloned().collect())
    }

    /// Заметка в пространстве `space` по пути `path` с текстом `content` и автором-сессией
    /// `session`. Итог — id.
    pub fn add_node(
        &self,
        space: &str,
        path: &str,
        content: &str,
        session: Option<&str>,
    ) -> String {
        self.with(|s| {
            let id = next_id(s, "NODE");
            s.nodes.insert(
                id.clone(),
                NodeRow {
                    id: id.clone(),
                    space: space.to_owned(),
                    path: path.to_owned(),
                    content: content.to_owned(),
                    rev: 1,
                    revisions: vec![revision(1, session)],
                },
            );
            id
        })
    }

    /// Текст и ревизия заметки по id.
    pub fn node(&self, id: &str) -> Option<(String, i64)> {
        self.with(|s| s.nodes.get(id).map(|n| (n.content.clone(), n.rev)))
    }

    /// Число заметок.
    pub fn node_count(&self) -> usize {
        self.with(|s| s.nodes.len())
    }

    /// Тела принятых выжимок `POST /api/v1/cache`.
    pub fn caches(&self) -> Vec<Value> {
        self.with(|s| s.caches.clone())
    }

    /// Режим идей владельца базы: `review` или `propose` (409 `idea-mode` в обе стороны, Δ7).
    pub fn set_idea_mode(&self, mode: &str) {
        let m = mode.to_owned();
        self.with(|s| s.idea_mode = m);
    }

    /// Запись журнала с заголовком `title` (без `force`) получает 409 `possible-duplicate` с
    /// кандидатом `candidate`.
    pub fn duplicate_title(&self, title: &str, candidate: &str) {
        let (t, c) = (title.to_owned(), candidate.to_owned());
        self.with(|s| {
            s.duplicates.insert(t, c);
        });
    }

    /// Следующая запись по маршруту `route` выполняется, а ответ рвётся посреди тела (у
    /// клиента — `Interrupted`, исход неизвестен).
    pub fn cut_after_write(&self, route: &str) {
        let r = route.to_owned();
        self.with(|s| {
            s.cut.insert(r);
        });
    }

    /// После обрыва запись `id` правит «человек»: ревизия растёт (повтор разбора по ключу
    /// получит 422).
    pub fn touch_after_cut(&self, id: &str) {
        let i = id.to_owned();
        self.with(|s| s.touch_after_cut = Some(i));
    }

    /// Следующий запрос к `route` получает problem+json с кодом и типом (один раз).
    pub fn fail_once(
        &self,
        route: &str,
        status: u16,
        problem_type: &str,
        retry_after: Option<u64>,
    ) {
        let f = Failure {
            status,
            problem_type: problem_type.to_owned(),
            detail: format!("fake {status}"),
            retry_after,
        };
        let r = route.to_owned();
        self.with(|s| s.fail_once.entry(r).or_default().push_back(f));
    }

    /// Предупреждения следующего удачного ответа записи (`warnings`).
    pub fn warn_next(&self, warnings: &[&str]) {
        let w: Vec<String> = warnings.iter().map(|w| (*w).to_owned()).collect();
        self.with(|s| s.warnings_next = w);
    }

    /// Ответы записей ждут [`FakeZam::release_writes`]: запрос принят (он уже в канале).
    pub fn hold_writes(&self) {
        self.held_writes.send_replace(true);
    }

    /// Отпускает задержанные ответы записей.
    pub fn release_writes(&self) {
        self.held_writes.send_replace(false);
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

async fn handle(
    req: Request,
    tx: &mpsc::UnboundedSender<Seen>,
    state: &Mutex<State>,
    mut gate: watch::Receiver<bool>,
    mut wgate: watch::Receiver<bool>,
) -> Response {
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
    let reply = state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replies
        .get(&route)
        .cloned();
    if let Some(v) = reply {
        return json_reply(200, v);
    }
    if route == "/mcp" {
        return mcp(&headers, &body, state);
    }
    if method == "POST" && route == "/api/v1/recall" {
        // Задержка — воротами теста, а не таймером: запрос уже в канале.
        let _ = gate.wait_for(|held| !*held).await;
    }
    if method != "GET" && is_memory_route(&route) {
        let _ = wgate.wait_for(|held| !*held).await;
    }
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    let once = s.fail_once.get_mut(&route).and_then(VecDeque::pop_front);
    if let Some(f) = once {
        let mut r = problem(f.status, &f.problem_type, &f.detail);
        if let Some(sec) = f.retry_after {
            r.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(sec));
        }
        return r;
    }
    if is_memory_route(&route) {
        let session = headers
            .get("x-zam-session")
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        let key = headers
            .get("idempotency-key")
            .and_then(|h| h.to_str().ok())
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty());
        let if_match = headers
            .get(header::IF_MATCH)
            .and_then(|h| h.to_str().ok())
            .map(|v| {
                v.trim()
                    .trim_start_matches("W/")
                    .trim_matches('"')
                    .to_owned()
            });
        let rq = MemReq {
            method: &method,
            route: &route,
            query: path.split_once('?').map_or("", |(_, q)| q),
            session: session.as_deref(),
            key: key.as_deref(),
            if_match: if_match.as_deref(),
            body: &body,
        };
        let resp = memory(&rq, &mut s);
        if method != "GET" && resp.status().is_success() && s.cut.remove(&route) {
            if let Some(id) = s.touch_after_cut.take()
                && let Some(e) = s.entries.get_mut(&id)
            {
                e["rev"] = json!(e["rev"].as_i64().unwrap_or(1) + 1);
            }
            return broken();
        }
        return resp;
    }
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
            let space = &r["/api/v1/spaces/".len()..r.len() - "/vault".len()];
            if let Some(st) = s.forbidden.get(space) {
                let mut v = problem_body(
                    403,
                    "/problems/solution-access",
                    &format!("нет доступа к решению {space}"),
                );
                v["state"] = json!(st);
                return problem_json(403, v);
            }
            let items: Vec<Value> = s
                .vault
                .iter()
                .filter(|row| row.space == space)
                .map(VaultRow::meta)
                .collect();
            let mut v = json!({"items": items});
            put_lock(&s.vault_locked, &mut v);
            json_reply(200, v)
        }
        ("GET", r) if r.starts_with("/api/v1/vault/") => {
            let id = &r["/api/v1/vault/".len()..];
            let Some(row) = s.vault.iter().find(|row| row.id == id).cloned() else {
                return problem(404, "about:blank", "ресурс не найден");
            };
            let query = path.split_once('?').map_or("", |(_, q)| q);
            let reveal = query
                .split('&')
                .any(|kv| kv == "reveal=1" || kv == "reveal=true");
            let mut v = row.meta();
            if !reveal {
                put_lock(&s.vault_locked, &mut v);
                return json_reply(200, v);
            }
            if let Some(reason) = &s.vault_locked {
                return problem(503, "/problems/vault-locked", reason);
            }
            let session = headers
                .get("x-zam-session")
                .and_then(|h| h.to_str().ok())
                .map(str::to_owned);
            s.reveals.push((row.id.clone(), session));
            v["secret"] = json!(row.secret);
            let mut r = json_reply(200, v);
            r.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            r
        }
        ("POST", "/api/v1/sessions") => {
            let Some(obj) = body.as_object() else {
                return problem(400, "about:blank", "тело — объект JSON");
            };
            if let Some(k) = obj.keys().find(|k| !SESSION_FIELDS.contains(&k.as_str())) {
                return problem(400, "about:blank", &format!("unknown field `{k}`"));
            }
            let id = body["id"].as_str().unwrap_or("generated").to_owned();
            s.sessions.push(body.clone());
            json_reply(201, json!({"id": id}))
        }
        ("POST", "/api/v1/recall") => {
            let event = body["event"].as_str().unwrap_or("").to_owned();
            let answer = s.recall.get(&event).cloned().unwrap_or_default();
            // Тип подсказок выключен у пользователя — пустой `text` без сбора.
            let off = s.hooks_off.contains(&event);
            let (text, items) = if off {
                (String::new(), Vec::new())
            } else {
                (answer.text.clone(), answer.items.clone())
            };
            let tokens = answer
                .tokens
                .unwrap_or_else(|| u32::try_from(text.len().div_ceil(4)).unwrap());
            let mut v = json!({
                "text": text,
                "items": items,
                "tokens": if off { 0 } else { tokens },
                "rules_tokens": answer.rules_tokens,
                "timings": {"collect_ms": 0, "record_ms": null, "total_ms": 0},
                "warnings": answer.warnings,
            });
            if event == "session_start" {
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

/// Запрос к маршрутам записей памяти.
struct MemReq<'a> {
    method: &'a str,
    route: &'a str,
    query: &'a str,
    session: Option<&'a str>,
    key: Option<&'a str>,
    if_match: Option<&'a str>,
    body: &'a Value,
}

/// Маршруты записей памяти (задача 6).
fn is_memory_route(route: &str) -> bool {
    route.starts_with("/api/v1/entries/")
        || route == "/api/v1/nodes"
        || route.starts_with("/api/v1/nodes/")
        || route == "/api/v1/cache"
        || (route.starts_with("/api/v1/spaces/") && route.ends_with("/entries"))
}

/// Следующий синтетический id (26 символов, как ULID ZAM).
fn next_id(s: &mut State, tag: &str) -> String {
    s.next_id += 1;
    let mut id = format!("01M8SYN{tag}");
    let n = s.next_id.to_string();
    while id.len() + n.len() < 26 {
        id.push('0');
    }
    id.push_str(&n);
    id
}

/// Ревизия заметки с автором-сессией.
fn revision(rev: i64, session: Option<&str>) -> Value {
    json!({"rev": rev, "session_id": session, "author_kind": "agent"})
}

/// Статус новой записи по виду.
fn initial_status(kind: &str) -> Value {
    match kind {
        "insight" | "term" => json!("active"),
        "task" | "question" => json!("open"),
        "idea" => json!("new"),
        _ => Value::Null,
    }
}

/// Запись формы `Entry` ZAM по телу создания.
fn entry_json(id: &str, space: &str, body: &Value, session: Option<&str>) -> Value {
    let kind = body["kind"].as_str().unwrap_or("insight").to_owned();
    let certainty = match kind.as_str() {
        "insight" => body
            .get("certainty")
            .cloned()
            .filter(|c| !c.is_null())
            .unwrap_or(json!("hypothesis")),
        _ => Value::Null,
    };
    json!({
        "id": id,
        "space_id": space_id(space),
        "kind": kind,
        "title": body["title"].as_str().unwrap_or(""),
        "body": body["body"].as_str().unwrap_or(""),
        "data": body.get("data").cloned().filter(Value::is_object).unwrap_or(json!({})),
        "tags": body.get("tags").cloned().filter(Value::is_array).unwrap_or(json!([])),
        "area": body.get("area").cloned().unwrap_or(Value::Null),
        "status": initial_status(&kind),
        "certainty": certainty,
        "rev": 1,
        "author": {"kind": "agent", "session_id": session},
    })
}

/// Ответ записи с предупреждениями следующего ответа (если заданы).
fn with_warnings(s: &mut State, status: u16, mut v: Value) -> Response {
    let w = std::mem::take(&mut s.warnings_next);
    if !w.is_empty() {
        v["warnings"] = json!(w);
    }
    json_reply(status, v)
}

/// Ключ идемпотентности: повтор того же запроса — `Ok(Some(id))`, другой запрос — 422.
fn idem_check(s: &State, rq: &MemReq<'_>) -> Result<Option<String>, Box<Response>> {
    let Some(k) = rq.key else { return Ok(None) };
    match s.idem.get(k) {
        None => Ok(None),
        Some((fp, id)) if *fp == fingerprint(rq) => Ok(Some(id.clone())),
        Some(_) => Err(Box::new(problem(
            422,
            "/problems/idempotency-mismatch",
            "ключ повторён с другим запросом",
        ))),
    }
}

/// Отпечаток запроса: метод, маршрут и тело (ключи `serde_json` упорядочены).
fn fingerprint(rq: &MemReq<'_>) -> String {
    format!("{} {} {}", rq.method, rq.route, rq.body)
}

fn remember(s: &mut State, rq: &MemReq<'_>, id: &str) {
    if let Some(k) = rq.key {
        s.idem
            .insert(k.to_owned(), (fingerprint(rq), id.to_owned()));
    }
}

/// Значение параметра строки запроса (с раскодированием `%XX`).
fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == name).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let hex = |c: u8| char::from(c).to_digit(16).map(|d| d as u8);
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Маршруты записей памяти: журнал, операции записей, выжимки, заметки (формы
/// `@v0.0.12-m6.5`, см. модуль).
fn memory(rq: &MemReq<'_>, s: &mut State) -> Response {
    let r = rq.route;
    if let Some(space) = r
        .strip_prefix("/api/v1/spaces/")
        .and_then(|x| x.strip_suffix("/entries"))
    {
        return match rq.method {
            "POST" => create_entry(rq, s, space),
            _ => problem(405, "about:blank", "метод не поддерживается"),
        };
    }
    if r == "/api/v1/cache" && rq.method == "POST" {
        s.caches.push(rq.body.clone());
        let id = next_id(s, "CACHE");
        return json_reply(
            201,
            json!({"id": id, "ref": format!("c_{id}"), "state": "valid"}),
        );
    }
    if r == "/api/v1/nodes" && rq.method == "POST" {
        return create_node(rq, s);
    }
    if r == "/api/v1/nodes/by-path" && rq.method == "GET" {
        let space = query_param(rq.query, "space").unwrap_or_default();
        let path = query_param(rq.query, "path").unwrap_or_default();
        return match s
            .nodes
            .values()
            .find(|n| n.space == space && n.path == path)
        {
            Some(n) => json_reply(200, json!({"node": n.json()})),
            None => problem(404, "about:blank", "ресурс не найден"),
        };
    }
    if let Some(rest) = r.strip_prefix("/api/v1/nodes/") {
        return node_route(rq, s, rest);
    }
    if let Some(rest) = r.strip_prefix("/api/v1/entries/") {
        let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
        if !s.entries.contains_key(id) {
            return problem(404, "about:blank", "ресурс не найден");
        }
        return match (rq.method, action) {
            ("GET", "") => json_reply(200, s.entries[id].clone()),
            ("PATCH", "") => patch_entry(rq, s, id),
            ("POST", "claim") => claim(rq, s, id),
            ("POST", "release") => {
                let e = s.entries.get_mut(id).unwrap();
                if e["data"]["claimed_by"]["session"].as_str() == rq.session {
                    e["data"].as_object_mut().unwrap().remove("claimed_by");
                }
                json_reply(200, e.clone())
            }
            ("POST", "supersede") => supersede(rq, s, id),
            ("POST", "retract") => {
                let reason = rq.body["reason"].as_str().unwrap_or("").trim().to_owned();
                if reason.is_empty() {
                    return problem(
                        422,
                        "/problems/unprocessable",
                        "reason: причина обязательна",
                    );
                }
                let e = s.entries.get_mut(id).unwrap();
                if e["status"] == "retracted" {
                    return problem(409, "/problems/conflict", "запись уже отозвана");
                }
                e["status"] = json!("retracted");
                e["retract_reason"] = json!(reason);
                e["rev"] = json!(e["rev"].as_i64().unwrap_or(1) + 1);
                json_reply(200, e.clone())
            }
            ("POST", "answer") => answer(rq, s, id),
            ("POST", "propose") => {
                let e = s.entries.get_mut(id).unwrap();
                if s.idea_mode == "review" && e["status"] == "new" && e["data"]["origin"] == "human"
                {
                    return problem(409, "/problems/idea-mode", "идею разбирает агент");
                }
                e["status"] = json!("proposed");
                e["rev"] = json!(e["rev"].as_i64().unwrap_or(1) + 1);
                let v =
                    json!({"entry": e.clone(), "question": {"id": "01M8SYNQUESTION0000000001"}});
                json_reply(200, v)
            }
            ("POST", "decide") => {
                if rq.body.get("by").is_none_or(Value::is_null) {
                    return problem(400, "/problems/by-required", "укажите by");
                }
                let status = match rq.body["decision"].as_str() {
                    Some("accept") => "accepted",
                    Some("defer") => "deferred",
                    Some("decline") => "declined",
                    _ => return problem(422, "/problems/unprocessable", "decision"),
                };
                let e = s.entries.get_mut(id).unwrap();
                e["status"] = json!(status);
                e["rev"] = json!(e["rev"].as_i64().unwrap_or(1) + 1);
                json_reply(200, json!({"entry": e.clone()}))
            }
            ("POST", "review") => review(rq, s, id),
            _ => problem(405, "about:blank", "метод не поддерживается"),
        };
    }
    problem(404, "about:blank", "ресурс не найден")
}

fn create_entry(rq: &MemReq<'_>, s: &mut State, space: &str) -> Response {
    match idem_check(s, rq) {
        Err(r) => return *r,
        Ok(Some(id)) => return json_reply(200, s.entries[&id].clone()),
        Ok(None) => {}
    }
    let kind = rq.body["kind"].as_str().unwrap_or("");
    if matches!(kind, "changelog" | "release" | "report") && space == "knowledge" {
        return problem(
            422,
            "/problems/unprocessable",
            "changelog и release ведутся только в пространстве решения",
        );
    }
    if kind == "idea" && rq.body["data"]["origin"].as_str().is_none() {
        return problem(
            422,
            "/problems/unprocessable",
            "data.origin: укажите human или agent",
        );
    }
    let title = rq.body["title"].as_str().unwrap_or("");
    if rq.body["force"] != json!(true)
        && let Some(c) = s.duplicates.get(title).cloned()
    {
        let mut v = problem_body(
            409,
            "/problems/possible-duplicate",
            "похоже на уже записанный insight (1 шт.)",
        );
        v["candidates"] = json!([{"id": c, "title": title, "status": "active", "certainty": "hypothesis", "reason": "title", "score": null}]);
        v["dedup"] = json!("degraded");
        return problem_json(409, v);
    }
    let id = next_id(s, "ENTRY");
    let e = entry_json(&id, space, rq.body, rq.session);
    s.entries.insert(id.clone(), e.clone());
    remember(s, rq, &id);
    with_warnings(s, 201, e)
}

fn patch_entry(rq: &MemReq<'_>, s: &mut State, id: &str) -> Response {
    let Some(im) = rq.if_match else {
        return problem(
            428,
            "/problems/precondition-required",
            "передайте If-Match: <rev> текущей ревизии",
        );
    };
    let Ok(rev) = im.parse::<i64>() else {
        return problem(400, "about:blank", "If-Match: номер ревизии");
    };
    let e = s.entries.get_mut(id).unwrap();
    let current = e["rev"].as_i64().unwrap_or(1);
    if rev != current {
        let mut v = problem_body(409, "/problems/conflict", "ревизия устарела");
        v["rev"] = json!(current);
        return problem_json(409, v);
    }
    let b = rq.body;
    if b.get("by").is_some_and(|v| !v.is_null()) {
        let edit = ["title", "body", "append_body"]
            .iter()
            .any(|k| b.get(*k).is_some());
        if e["kind"] != "idea" || !edit {
            return problem(422, "/problems/unprocessable", "by: только у правки идеи");
        }
    }
    for k in ["title", "body", "status", "certainty", "area"] {
        if let Some(v) = b.get(k) {
            e[k] = v.clone();
        }
    }
    if let Some(a) = b["append_body"].as_str() {
        let old = e["body"].as_str().unwrap_or("").to_owned();
        e["body"] = json!(format!("{old}\n\n{a}"));
    }
    if let Some(t) = b.get("tags") {
        e["tags"] = t.clone();
    }
    let data = e["data"].as_object_mut().unwrap();
    if let Some(d) = b["data"].as_object() {
        for (k, v) in d {
            if v.is_null() {
                data.remove(k);
            } else {
                data.insert(k.clone(), v.clone());
            }
        }
    }
    for k in ["next_step", "blocked_by"] {
        if let Some(v) = b.get(k) {
            data.insert(k.to_owned(), v.clone());
        }
    }
    e["rev"] = json!(current + 1);
    let v = e.clone();
    with_warnings(s, 200, v)
}

fn claim(rq: &MemReq<'_>, s: &mut State, id: &str) -> Response {
    let Some(session) = rq.session else {
        return problem(
            400,
            "about:blank",
            "захват требует сессии: заголовок X-Zam-Session",
        );
    };
    let e = s.entries.get_mut(id).unwrap();
    if let Some(other) = e["data"]["claimed_by"]["session"].as_str()
        && other != session
    {
        let mut v = problem_body(
            409,
            "/problems/task-claimed",
            "задача захвачена другой сессией",
        );
        v["claimed_by"] = e["data"]["claimed_by"].clone();
        return problem_json(409, v);
    }
    e["data"]["claimed_by"] = json!({
        "session": session,
        "agent": rq.body["agent"],
        "until": "2026-11-17T02:00:00Z",
    });
    json_reply(200, e.clone())
}

fn supersede(rq: &MemReq<'_>, s: &mut State, id: &str) -> Response {
    match idem_check(s, rq) {
        Err(r) => return *r,
        Ok(Some(new)) => {
            let v = json!({"entry": s.entries[&new].clone(), "superseded": s.entries[id].clone()});
            return json_reply(200, v);
        }
        Ok(None) => {}
    }
    let old = s.entries[id].clone();
    if old["status"] == "superseded" || old["status"] == "retracted" {
        return problem(
            409,
            "/problems/conflict",
            "запись уже замещена или отозвана",
        );
    }
    let new_id = next_id(s, "ENTRY");
    let mut body = rq.body.clone();
    body["kind"] = old["kind"].clone();
    let mut e = entry_json(&new_id, "", &body, rq.session);
    e["space_id"] = old["space_id"].clone();
    e["supersedes_id"] = json!(id);
    s.entries.insert(new_id.clone(), e.clone());
    let o = s.entries.get_mut(id).unwrap();
    o["status"] = json!("superseded");
    o["superseded_by_id"] = json!(new_id);
    let o = o.clone();
    remember(s, rq, &new_id);
    json_reply(201, json!({"entry": e, "superseded": o}))
}

fn answer(rq: &MemReq<'_>, s: &mut State, id: &str) -> Response {
    let Some(by) = rq.body["by"].as_str() else {
        return problem(
            400,
            "/problems/by-required",
            "ответ на вопрос токеном: укажите by: human или by: agent",
        );
    };
    let by = by.to_owned();
    let e = s.entries.get_mut(id).unwrap();
    if e["status"] != "open" {
        let mut v = problem_body(409, "/problems/already-answered", "вопрос уже отвечен");
        v["decision_id"] = json!("01M8SYNDECISION0000000001");
        return problem_json(409, v);
    }
    e["status"] = json!("answered");
    e["data"]["answer"] = json!({
        "text": rq.body["text"],
        "option": rq.body["option"],
        "by": by,
        "channel": rq.body.get("channel").cloned().unwrap_or(json!("chat")),
        "decision_id": "01M8SYNDECISION0000000001",
    });
    e["rev"] = json!(e["rev"].as_i64().unwrap_or(1) + 1);
    let v = json!({
        "entry": e.clone(),
        "decision": {"id": "01M8SYNDECISION0000000001", "kind": "insight"},
        "decision_id": "01M8SYNDECISION0000000001",
    });
    json_reply(200, v)
}

/// `POST /entries/{id}/review` (Δ2, Δ4): режим `propose` — 409 `idea-mode` раньше тела;
/// повтор по ключу — `replay: true`, пока идея не менялась после разбора, иначе 422; идея не
/// `new` — 409 `transition-not-allowed`.
fn review(rq: &MemReq<'_>, s: &mut State, id: &str) -> Response {
    if s.idea_mode == "propose" {
        return problem(
            409,
            "/problems/idea-mode",
            "разбор выключен (ideas.mode = propose)",
        );
    }
    if rq.body.get("by").is_some() {
        return problem(400, "about:blank", "unknown field `by`");
    }
    let e = s.entries.get_mut(id).unwrap();
    let rev = e["rev"].as_i64().unwrap_or(1);
    if let Some(k) = rq.key
        && let Some(r) = e["data"]["reviews"]
            .as_array()
            .and_then(|a| a.iter().find(|r| r["key"] == k).cloned())
    {
        if r["rev_after"].as_i64() == Some(rev) && r["outcome"] == rq.body["outcome"] {
            return json_reply(200, json!({"entry": e.clone(), "replay": true}));
        }
        return problem(
            422,
            "/problems/idempotency-mismatch",
            "ключ разбора повторён после правки идеи",
        );
    }
    if e["status"] != "new" {
        let mut v = problem_body(409, "/problems/conflict", "переход идеи не разрешён");
        v["code"] = json!("transition-not-allowed");
        return problem_json(409, v);
    }
    let outcome = rq.body["outcome"].as_str().unwrap_or("").to_owned();
    let status = match outcome.as_str() {
        "plan" => "accepted",
        "return" => "returned",
        "decline" => "declined",
        "done" => "done",
        _ => return problem(422, "/problems/unprocessable", "outcome"),
    };
    let item = json!({"key": rq.key, "outcome": outcome, "rev_after": rev + 1});
    e["data"]["review"] = item.clone();
    if !e["data"]["reviews"].is_array() {
        e["data"]["reviews"] = json!([]);
    }
    e["data"]["reviews"].as_array_mut().unwrap().push(item);
    e["status"] = json!(status);
    e["rev"] = json!(rev + 1);
    json_reply(200, json!({"entry": e.clone(), "replay": false}))
}

fn create_node(rq: &MemReq<'_>, s: &mut State) -> Response {
    match idem_check(s, rq) {
        Err(r) => return *r,
        Ok(Some(id)) => return json_reply(200, s.nodes[&id].json()),
        Ok(None) => {}
    }
    let space = rq.body["space"].as_str().unwrap_or("").to_owned();
    let path = rq.body["path"].as_str().unwrap_or("").to_owned();
    if s.nodes.values().any(|n| n.space == space && n.path == path) {
        return problem(409, "/problems/conflict", "путь занят");
    }
    let id = next_id(s, "NODE");
    let n = NodeRow {
        id: id.clone(),
        space,
        path,
        content: rq.body["content"].as_str().unwrap_or("").to_owned(),
        rev: 1,
        revisions: vec![revision(1, rq.session)],
    };
    let v = n.json();
    s.nodes.insert(id.clone(), n);
    remember(s, rq, &id);
    with_warnings(s, 201, v)
}

fn node_route(rq: &MemReq<'_>, s: &mut State, rest: &str) -> Response {
    let (id, tail) = rest.split_once('/').unwrap_or((rest, ""));
    if !s.nodes.contains_key(id) {
        return problem(404, "about:blank", "ресурс не найден");
    }
    match (rq.method, tail) {
        ("GET", "") => json_reply(200, s.nodes[id].json()),
        ("GET", "revisions") => json_reply(200, json!(s.nodes[id].revisions)),
        ("PUT", "") => {
            let Some(base) = rq.if_match.and_then(|v| v.parse::<i64>().ok()) else {
                return problem(428, "/problems/precondition-required", "передайте If-Match");
            };
            if let Ok(Some(_)) = idem_check(s, rq) {
                return json_reply(200, s.nodes[id].json());
            }
            let n = s.nodes.get_mut(id).unwrap();
            if base != n.rev {
                let mut v = problem_body(409, "/problems/conflict", "ревизия устарела");
                v["current_rev"] = json!(n.rev);
                return problem_json(409, v);
            }
            n.content = rq.body["content"].as_str().unwrap_or("").to_owned();
            n.rev += 1;
            n.revisions.insert(0, revision(n.rev, rq.session));
            let v = n.json();
            remember(s, rq, id);
            with_warnings(s, 200, v)
        }
        ("PATCH", "") => {
            if let Ok(Some(_)) = idem_check(s, rq) {
                return json_reply(200, s.nodes[id].json());
            }
            let base = rq.body["base_rev"].as_i64().unwrap_or(0);
            let n = s.nodes.get_mut(id).unwrap();
            if base != n.rev {
                let mut v = problem_body(
                    409,
                    "/problems/conflict",
                    "секция изменилась после base_rev",
                );
                v["current_rev"] = json!(n.rev);
                return problem_json(409, v);
            }
            for op in rq.body["ops"].as_array().cloned().unwrap_or_default() {
                n.content.push_str(op["content"].as_str().unwrap_or(""));
            }
            n.rev += 1;
            n.revisions.insert(0, revision(n.rev, rq.session));
            let v = n.json();
            remember(s, rq, id);
            with_warnings(s, 200, v)
        }
        _ => problem(405, "about:blank", "метод не поддерживается"),
    }
}

/// Тело ответа, которое рвётся после первого куска: запись уже сделана, исход у клиента
/// неизвестен (`Interrupted`).
struct Broken(bool);

impl http_body::Body for Broken {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if !self.0 {
            self.0 = true;
            return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"{\"id\":")))));
        }
        Poll::Ready(Some(Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "fake cut after write",
        ))))
    }
}

/// Ответ 200 с телом, оборванным после записи.
fn broken() -> Response {
    let mut r = Response::new(Body::new(Broken(false)));
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r
}

/// Поля блокировки ответа метаданных Vault: `locked` и при блокировке `reason`.
fn put_lock(locked: &Option<String>, v: &mut Value) {
    match locked {
        Some(reason) => {
            v["locked"] = json!(true);
            v["reason"] = json!(reason);
        }
        None => v["locked"] = json!(false),
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
