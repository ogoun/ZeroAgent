//! Шлюз записи в память — единственный писатель модели и рантайма (R1-M D145, D163, D164,
//! D165; D169–D171 изменений по ZAM 0.0.12-m6.5).
//!
//! **Путь записи** [`WriteGateway::write`]:
//! 1. **Таксономия** ([`crate::taxonomy`]) — ошибка формы до сети ([`WriteOutcome::Refused`]).
//! 2. **Функция решения** ([`gate::decide`]) без цели: отказ без цели окончателен (цель только
//!    добавляет отказы — правило 3), сеть не трогается. Недоверенный проект, область вне
//!    [`ScopeGuard`], `rules`, подделка человека, выжимка из tainted-контекста — здесь.
//! 3. **Доверие цели** — один запрос на вызов (`GET /entries/{id}`, `GET /nodes/{id}` или
//!    `GET /nodes/by-path`), только когда оно нужно: правка из tainted-контекста, правка тегов
//!    (теги происхождения цели сохраняются), `supersede` (вид цели), замена заметки (id узла),
//!    `next_step` (ревизия).
//! 4. **Доказательство** `theorem` — по журналу run: `tool.finished` вызова из
//!    `evidence.checked` без ошибки (D163 правило 6).
//! 5. **Функция решения с целью**, затем **маска** процесса ([`MaskCell::current`] в момент
//!    записи, D160) по всем строкам тела — до отправки и до очереди (MEM-074).
//! 6. **Отправка:**
//!    - **с ключом** (`Idempotency-Key` = ключ эффекта, D145): записи журнала, `supersede`,
//!      `POST /cache`, заметки при флаге `idempotent_nodes` — через [`WriteQueue`] с дедлайном
//!      вызова минус 2 с (D165): очередь отвечает итогом строки или `Queued`. До outbox
//!      (задача 7) — [`DirectQueue`]: отправка сразу, без хранения, нет связи или обрыв —
//!      ошибка `memory unavailable`;
//!    - **разбор идеи** (D171) — с ключом, но мимо очереди: у `review` нет `If-Match`, досылка
//!      через дни применила бы разбор к переписанной идее. L0 или нет связи — отказ
//!      `memory unavailable: review is not queued`; исход неизвестен, затем 422
//!      `idempotency-mismatch` или 409 (`transition-not-allowed`, `idea-mode`) — перечитать
//!      идею: наш ключ в `data.review.key` или `data.reviews[].key` — [`WriteOutcome::Unchanged`];
//!    - **без ключа** (`PATCH /entries/{id}`, `claim`, `release`, `answer`, `propose`,
//!      `decide`, `retract`, `next_step`, заметки без `idempotent_nodes`) — под мьютексом
//!      решения в процессе, «со сверкой»: исход неизвестен — перечитать цель, она уже в нужном
//!      состоянии — успех без второго запроса; у правки с `If-Match` повтор безопасен, 409 после
//!      своей правки — успех после перечитывания; 428 — перечитать ревизию и повторить раз;
//!      409 у `append` и `next_step` — повторить раз с текущей ревизией. При L0 — отказ
//!      `memory unavailable: <op> is not queued` (D165).
//! 7. **Повторы** (D165) — только при L1+ и до дедлайна: `Busy` (`Retry-After` ≤ 5 с),
//!    `RateLimited`, 408 — паузы 2, 4, 8 с через [`Clock`]. Нет соединения — L0 общей связи
//!    ([`Link`]) без повтора: «при известном L0 — без попыток».
//! 8. **Итог:** `Written`, `Unchanged` (повтор по ключу), `Queued`, `Refused`; предупреждения —
//!    пометки шлюза, затем `warnings` ответа сервера (`secrets_detected`, `term_alias`, …).
//!    409 `possible-duplicate` — отказ с текстом модели «possible duplicate of <ref>; append,
//!    supersede or set force». 409 `idea-mode` — ошибка [`ZamError`] с этим типом
//!    ([`is_idea_mode`]): признак режима идей (D169) вызывающему; режим меняет задача 8.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use za_core::BoxFuture;
use za_protocol::{EventKind, Seq};
use za_secrets::{MaskCell, SecretMasker};
use za_store::ProjectStore;

use crate::error::{ErrorKind, ZamError};
use crate::gate::{self, CALL_PREFIX, TargetTrust, Verdict, WriteCx, WriteOp, decide};
use crate::link::Link;
use crate::scope::ScopeGuard;
use crate::session::RunBinding;
use crate::taxonomy::{self, Flags};
use crate::transport::{Ctx, Method, STATUS_LIMIT};

/// Флаг сервера: ключ идемпотентности у `POST /nodes`, `PUT` и `PATCH /nodes/{id}` (M6).
pub const FEATURE_IDEMPOTENT_NODES: &str = "idempotent_nodes";
/// Запас до дедлайна вызова (D165): запись не успела — строка остаётся в очереди.
pub const DEADLINE_MARGIN: Duration = Duration::from_secs(2);
/// Тайм-аут инструментов записи (таблица D147): дедлайн, если вызов его не задал.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(40);
/// Тайм-аут `memory_cache_put` (таблица D147).
pub const CACHE_TIMEOUT: Duration = Duration::from_secs(10);
/// Паузы повторов при L1+ (D165).
pub const RETRY_PAUSES: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];
/// `Busy` повторяется, только если `Retry-After` не больше (D165).
pub const BUSY_RETRY_MAX: Duration = Duration::from_secs(5);
/// Начало отказа «запись не встаёт в очередь» (D165, D171).
pub const UNAVAILABLE: &str = "memory unavailable";

const ENTRIES: &str = "/api/v1/entries";
const NODES: &str = "/api/v1/nodes";
const CACHE: &str = "/api/v1/cache";
/// Событий журнала run за одно чтение при проверке доказательства.
const EVIDENCE_PAGE: usize = 512;

/// Строка очереди записи (задача 7 — outbox): всё, что нужно отправителю, без сети.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedWrite {
    /// `Idempotency-Key` — ключ эффекта вызова (D145).
    pub key: String,
    /// Операция (`log`, `supersede`, `cache_put`, `note`, `node_patch`).
    pub op: &'static str,
    /// Вид записи журнала.
    pub kind: Option<String>,
    /// Метод HTTP (`POST`, `PUT`, `PATCH`).
    pub method: String,
    /// Путь REST от базового адреса (`/api/v1/…`).
    pub path: String,
    /// Тело после функции решения и маски (D145, MEM-074).
    pub body: Value,
    /// `If-Match` (замена заметки).
    pub if_match: Option<i64>,
    /// `X-Zam-Session`.
    pub session: String,
    /// `X-Zam-Solution`.
    pub solution: Option<String>,
    /// `X-Zam-Area`.
    pub area: Option<String>,
}

/// Итог строки очереди.
#[derive(Debug, Clone, PartialEq)]
pub enum Submit {
    /// Ответ сервера (код и тело) или ошибка отправки.
    Sent(Result<(u16, Value), ZamError>),
    /// Строка осталась в очереди (дедлайн вышел или известен L0): уйдёт позже.
    Queued { outbox: i64 },
}

/// Очередь записей с ключом (D165): транзакционный outbox — задача 7; до неё —
/// [`DirectQueue`].
pub trait WriteQueue: Send + Sync {
    /// Ставит строку и ждёт её итога не дольше `deadline`.
    fn submit(&self, row: QueuedWrite, deadline: Option<Instant>) -> BoxFuture<'_, Submit>;
}

/// Очередь без хранения (до задачи 7): строка отправляется сразу, по одной, без повторов.
/// Нет связи или обрыв — ошибка [`UNAVAILABLE`]: строку сохранить негде.
pub struct DirectQueue {
    link: Arc<Link>,
    lock: tokio::sync::Mutex<()>,
}

impl DirectQueue {
    pub fn new(link: Arc<Link>) -> Self {
        Self {
            link,
            lock: tokio::sync::Mutex::new(()),
        }
    }
}

impl WriteQueue for DirectQueue {
    fn submit(&self, row: QueuedWrite, deadline: Option<Instant>) -> BoxFuture<'_, Submit> {
        Box::pin(async move {
            if !self.link.available().await {
                return Submit::Sent(Err(unavailable_error(&ZamError::new(
                    ErrorKind::Offline,
                    "memory server unreachable",
                ))));
            }
            let _turn = self.lock.lock().await;
            let deadline = deadline.unwrap_or_else(|| self.link.clock().now() + WRITE_TIMEOUT);
            let Some(limit) = remaining(self.link.as_ref(), deadline) else {
                return Submit::Sent(Err(deadline_error()));
            };
            let Ok(method) = Method::from_bytes(row.method.as_bytes()) else {
                return Submit::Sent(Err(ZamError::new(
                    ErrorKind::Protocol,
                    "invalid HTTP method in a queued write",
                )));
            };
            let cx = Ctx {
                session: Some(&row.session),
                solution: row.solution.as_deref(),
                area: row.area.as_deref(),
            };
            let r = self
                .link
                .client()
                .send_json(
                    method,
                    &row.path,
                    &row.body,
                    &cx,
                    Some(&row.key),
                    row.if_match,
                    limit,
                )
                .await;
            self.link.note_outcome(r.as_ref().err());
            match r {
                Err(e) if matches!(e.kind, ErrorKind::Offline | ErrorKind::NotZam) => {
                    self.link.drop_to_l0(&e);
                    Submit::Sent(Err(unavailable_error(&e)))
                }
                Err(e) if e.kind == ErrorKind::Interrupted => {
                    Submit::Sent(Err(unavailable_error(&e)))
                }
                r => Submit::Sent(r),
            }
        })
    }
}

/// Итог записи.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteOutcome {
    /// Сервер принял запись: ссылка (`i:<id>`, `n:<id>` …), ревизия, предупреждения (пометки
    /// шлюза и `warnings` ответа).
    Written {
        reference: String,
        rev: i64,
        warnings: Vec<String>,
    },
    /// Повтор с тем же ключом: запись уже есть.
    Unchanged {
        reference: String,
        warnings: Vec<String>,
    },
    /// Строка в outbox, уйдёт позже (D165).
    Queued { outbox: i64, warnings: Vec<String> },
    /// Шлюз отказал; текст — модели.
    Refused(String),
}

/// Части шлюза, которые собирает бинарник (задача 12).
pub struct GatewayParts {
    /// Связь с сервером — та же, что у порта ([`crate::port::ZamMemory::link`]): клиент,
    /// часы, уровень, флаги.
    pub link: Arc<Link>,
    pub binding: RunBinding,
    /// Маска процесса (D160): `current()` в момент каждой записи.
    pub masker: Arc<MaskCell>,
    /// Очередь записей с ключом (D165).
    pub queue: Arc<dyn WriteQueue>,
    /// Журнал проекта: проверка `evidence` по `tool.finished` (D163 правило 6).
    pub store: Option<ProjectStore>,
}

/// Шлюз записи run.
pub struct WriteGateway {
    link: Arc<Link>,
    binding: RunBinding,
    guard: ScopeGuard,
    masker: Arc<MaskCell>,
    queue: Arc<dyn WriteQueue>,
    store: Option<ProjectStore>,
    /// Мьютекс решения в процессе (D145): записи без ключа — по одной.
    lane: Arc<tokio::sync::Mutex<()>>,
    /// Записи этого шлюза, ждущие мьютекса решения.
    waiting: AtomicUsize,
}

/// Мьютекс решения в процессе: один на сервер и решение (Knowledge — без решения).
fn lane(base: &str, solution: Option<&str>) -> Arc<tokio::sync::Mutex<()>> {
    static LANES: OnceLock<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let key = format!("{base}|{}", solution.unwrap_or(crate::scope::KNOWLEDGE));
    let mut map = LANES
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    map.entry(key).or_default().clone()
}

/// Счёт ждущих мьютекса: снимается и при брошенном future.
struct Waiting<'a>(&'a AtomicUsize);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Запрос операции после функции решения.
#[derive(Debug, Clone)]
struct Req {
    method: Method,
    path: String,
    body: Value,
    if_match: Option<i64>,
    /// Идёт с ключом через очередь.
    keyed: bool,
}

/// Цель операции по ответу сервера.
#[derive(Debug, Clone, Default)]
struct Target {
    trust: TargetTrust,
    /// Id узла (замена заметки по пути).
    id: Option<String>,
    rev: Option<i64>,
}

/// Чтение цели не удалось.
enum Fetch {
    /// Связи нет: запись не встаёт в очередь.
    Unavailable,
    Error(ZamError),
}

impl WriteGateway {
    pub fn new(p: GatewayParts) -> Self {
        let guard = ScopeGuard::new(&p.binding);
        let lane = lane(p.link.client().base(), guard.solution());
        Self {
            link: p.link,
            binding: p.binding,
            guard,
            masker: p.masker,
            queue: p.queue,
            store: p.store,
            lane,
            waiting: AtomicUsize::new(0),
        }
    }

    /// Область run (D164): ей же инструменты проверяют аргументы чтения (задача 8).
    pub fn guard(&self) -> &ScopeGuard {
        &self.guard
    }

    /// Записи этого шлюза, ждущие мьютекса решения (тесты порядка записей).
    #[cfg(feature = "test-hooks")]
    pub fn lane_waiters(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }

    /// Запись `op` в контексте `cx` (см. модуль).
    pub async fn write(&self, op: WriteOp, cx: &WriteCx) -> Result<WriteOutcome, ZamError> {
        let mut op = op;
        let flags = Flags::from_features(&self.link.features());
        let mut notes = match taxonomy::check(&mut op, flags) {
            Ok(n) => n,
            Err(e) => return Ok(WriteOutcome::Refused(e.0)),
        };
        // Отказ без цели окончателен: цель только добавляет отказы (правило 3) — сети нет.
        let pre = match decide(&op, None, cx, &self.guard, false) {
            Verdict::Refuse(r) => return Ok(WriteOutcome::Refused(r)),
            Verdict::Allow(b) | Verdict::Downgrade { body: b, .. } => b,
        };
        let deadline = self.deadline(&op, cx);
        let target = if needs_target(&op, cx) {
            match self.target(&op, &pre, cx, deadline).await {
                Ok(t) => Some(t),
                // `supersede` идёт с ключом и встаёт в очередь (D165): в чистом контексте цель
                // нужна только для вида (место происхождения) — без неё `insight`. Из
                // tainted-контекста без доверия цели — отказ (правило 3).
                Err(Fetch::Unavailable)
                    if matches!(op, WriteOp::Supersede { .. }) && !cx.untrusted =>
                {
                    None
                }
                Err(Fetch::Unavailable) => {
                    return Ok(WriteOutcome::Refused(not_queued(op.name())));
                }
                Err(Fetch::Error(e)) => return Err(e),
            }
        } else {
            None
        };
        let evidence_ok = self.evidence_ok(&op, cx).await;
        let verdict = decide(
            &op,
            target.as_ref().map(|t| &t.trust),
            cx,
            &self.guard,
            evidence_ok,
        );
        let (mut body, more) = match verdict.into_parts() {
            Ok(p) => p,
            Err(r) => return Ok(WriteOutcome::Refused(r)),
        };
        notes.extend(more);
        mask_value(&self.masker.current(), &mut body);
        let req = self.request(&op, body, target.as_ref())?;
        if matches!(op, WriteOp::Review { .. }) {
            return self.review(&op, req, cx, deadline, notes).await;
        }
        if req.keyed {
            let row = QueuedWrite {
                key: cx.key.0.clone(),
                op: op.name(),
                kind: op.kind().map(str::to_owned),
                method: req.method.as_str().to_owned(),
                path: req.path.clone(),
                body: req.body.clone(),
                if_match: req.if_match,
                session: cx.session.clone(),
                solution: self.binding.solution.clone(),
                area: self.area(cx).map(str::to_owned),
            };
            return match self.queue.submit(row, Some(deadline)).await {
                Submit::Queued { outbox } => Ok(WriteOutcome::Queued {
                    outbox,
                    warnings: notes,
                }),
                Submit::Sent(Ok((status, v))) => Ok(done(&op, &req, status, &v, notes)),
                Submit::Sent(Err(e)) if e.problem() == Some("possible-duplicate") => {
                    Ok(WriteOutcome::Refused(duplicate_text(&op, &e)))
                }
                Submit::Sent(Err(e)) => Err(e),
            };
        }
        self.direct(&op, req, cx, deadline, notes).await
    }

    /// Дедлайн записи (D165): дедлайн вызова минус [`DEADLINE_MARGIN`]; без него — тайм-аут
    /// инструмента от текущего момента минус запас.
    fn deadline(&self, op: &WriteOp, cx: &WriteCx) -> Instant {
        let now = self.link.clock().now();
        let timeout = if matches!(op, WriteOp::CachePut(_)) {
            CACHE_TIMEOUT
        } else {
            WRITE_TIMEOUT
        };
        let end = cx.deadline.unwrap_or(now + timeout);
        end.checked_sub(DEADLINE_MARGIN).unwrap_or(now)
    }

    fn area<'a>(&'a self, cx: &'a WriteCx) -> Option<&'a str> {
        cx.area.as_deref().or(self.binding.area.as_deref())
    }

    fn ctx<'a>(&'a self, cx: &'a WriteCx) -> Ctx<'a> {
        Ctx {
            session: Some(&cx.session),
            solution: self.binding.solution.as_deref(),
            area: self.area(cx),
        }
    }

    /// Мьютекс решения с учётом ждущих.
    async fn lock_lane(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let waiting = Waiting(&self.waiting);
        let g = self.lane.lock().await;
        drop(waiting);
        g
    }

    /// Запрос с телом до дедлайна; итог отмечается в связи (счёт мёртвого хоста).
    async fn send(
        &self,
        req: &Req,
        cx: &WriteCx,
        key: Option<&str>,
        deadline: Instant,
    ) -> Result<(u16, Value), ZamError> {
        let Some(limit) = remaining(&self.link, deadline) else {
            return Err(deadline_error());
        };
        let r = self
            .link
            .client()
            .send_json(
                req.method.clone(),
                &req.path,
                &req.body,
                &self.ctx(cx),
                key,
                req.if_match,
                limit,
            )
            .await;
        self.link.note_outcome(r.as_ref().err());
        r
    }

    /// `GET path` до дедлайна (не дольше срока проверок статуса).
    async fn get(&self, path: &str, cx: &WriteCx, deadline: Instant) -> Result<Value, ZamError> {
        let Some(limit) = remaining(&self.link, deadline) else {
            return Err(deadline_error());
        };
        let r = self
            .link
            .client()
            .get_json(path, &self.ctx(cx), limit.min(STATUS_LIMIT))
            .await;
        self.link.note_outcome(r.as_ref().err());
        r
    }

    /// Доверие цели (D163): один запрос на вызов.
    async fn target(
        &self,
        op: &WriteOp,
        pre: &Value,
        cx: &WriteCx,
        deadline: Instant,
    ) -> Result<Target, Fetch> {
        if !self.link.available().await {
            return Err(Fetch::Unavailable);
        }
        let path = match op {
            WriteOp::Note { .. } => {
                let space = pre.get("space").and_then(Value::as_str).unwrap_or("");
                let rel = pre.get("path").and_then(Value::as_str).unwrap_or("");
                format!(
                    "{NODES}/by-path?space={}&path={}",
                    enc(space),
                    enc_path(rel)
                )
            }
            WriteOp::NodePatch { id, .. } => format!("{NODES}/{}", enc(id)),
            _ => match op.target_id() {
                Some(id) => format!("{ENTRIES}/{}", enc(id)),
                None => return Ok(Target::default()),
            },
        };
        let v = match self.get(&path, cx, deadline).await {
            Ok(v) => v,
            Err(e) if matches!(e.kind, ErrorKind::Offline | ErrorKind::NotZam) => {
                self.link.drop_to_l0(&e);
                return Err(Fetch::Unavailable);
            }
            Err(e) => return Err(Fetch::Error(e)),
        };
        let v = v.get("node").filter(|n| n.is_object()).unwrap_or(&v);
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let tags = v
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Target {
            trust: TargetTrust {
                certainty: s("certainty"),
                space: s("space_id"),
                kind: s("kind").filter(|k| k != "note" && k != "folder"),
                tags,
            },
            id: s("id"),
            rev: v
                .get("rev")
                .or_else(|| v.get("current_rev"))
                .and_then(Value::as_i64),
        })
    }

    /// Доказательство `theorem` (D163 правило 6): в журнале run есть `tool.finished` вызова из
    /// `evidence.checked` без ошибки.
    async fn evidence_ok(&self, op: &WriteOp, cx: &WriteCx) -> bool {
        let Some(call) = claimed_call(op) else {
            return false;
        };
        let Some(store) = &self.store else {
            return false;
        };
        let mut after = Seq(0);
        loop {
            let batch = match store.read_run(cx.run, after, EVIDENCE_PAGE).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "память: журнал run не прочитан, theorem понижается");
                    return false;
                }
            };
            let Some(last) = batch.last() else {
                return false;
            };
            after = last.seq;
            let found = batch.iter().any(|e| {
                e.kind == EventKind::ToolFinished
                    && e.payload.get("call_id").and_then(Value::as_str) == Some(call.as_str())
                    && e.payload.get("is_error").and_then(Value::as_bool) == Some(false)
            });
            if found {
                return true;
            }
        }
    }

    /// Запрос REST по телу функции решения.
    fn request(&self, op: &WriteOp, body: Value, target: Option<&Target>) -> Result<Req, ZamError> {
        let nodes_keyed = self.link.has(FEATURE_IDEMPOTENT_NODES);
        let entry = |id: &str, tail: &str| format!("{ENTRIES}/{}{tail}", enc(id));
        let mut body = body;
        let req = |method: Method, path: String, body: Value, if_match, keyed| Req {
            method,
            path,
            body,
            if_match,
            keyed,
        };
        Ok(match op {
            WriteOp::Log(_) => {
                let space = take_str(&mut body, "space").unwrap_or_default();
                req(
                    Method::POST,
                    format!("/api/v1/spaces/{}/entries", enc(&space)),
                    body,
                    None,
                    true,
                )
            }
            WriteOp::Note { .. } => {
                let base = body
                    .as_object_mut()
                    .and_then(|m| m.remove("base_rev"))
                    .and_then(|v| v.as_i64());
                match base {
                    Some(rev) => {
                        let id = target.and_then(|t| t.id.clone()).ok_or_else(|| {
                            ZamError::new(ErrorKind::NotFound, "note not found by path")
                        })?;
                        if let Some(m) = body.as_object_mut() {
                            m.remove("space");
                            m.remove("path");
                        }
                        req(
                            Method::PUT,
                            format!("{NODES}/{}", enc(&id)),
                            body,
                            Some(rev),
                            nodes_keyed,
                        )
                    }
                    None => {
                        if let Some(m) = body.as_object_mut() {
                            m.insert("kind".into(), json!("note"));
                        }
                        req(Method::POST, NODES.to_owned(), body, None, nodes_keyed)
                    }
                }
            }
            WriteOp::NodePatch { id, .. } => req(
                Method::PATCH,
                format!("{NODES}/{}", enc(id)),
                body,
                None,
                nodes_keyed,
            ),
            WriteOp::Patch { id, rev, .. } => req(
                Method::PATCH,
                entry(id, ""),
                body,
                (*rev > 0).then_some(*rev),
                false,
            ),
            WriteOp::NextStep { id, .. } => req(
                Method::PATCH,
                entry(id, ""),
                body,
                target.and_then(|t| t.rev),
                false,
            ),
            WriteOp::Claim { id, .. } => req(Method::POST, entry(id, "/claim"), body, None, false),
            WriteOp::Release { id } => req(Method::POST, entry(id, "/release"), body, None, false),
            WriteOp::Retract { id, .. } => {
                req(Method::POST, entry(id, "/retract"), body, None, false)
            }
            WriteOp::Answer { id, .. } => {
                req(Method::POST, entry(id, "/answer"), body, None, false)
            }
            WriteOp::Propose { id, .. } => {
                req(Method::POST, entry(id, "/propose"), body, None, false)
            }
            WriteOp::Decide { id, .. } => {
                req(Method::POST, entry(id, "/decide"), body, None, false)
            }
            WriteOp::Review { id, .. } => {
                req(Method::POST, entry(id, "/review"), body, None, false)
            }
            WriteOp::Supersede { id, .. } => {
                req(Method::POST, entry(id, "/supersede"), body, None, true)
            }
            WriteOp::CachePut(_) => req(Method::POST, CACHE.to_owned(), body, None, true),
        })
    }

    /// Запись без ключа: мьютекс решения, сверка, повторы (см. модуль).
    async fn direct(
        &self,
        op: &WriteOp,
        mut req: Req,
        cx: &WriteCx,
        deadline: Instant,
        notes: Vec<String>,
    ) -> Result<WriteOutcome, ZamError> {
        if !self.link.available().await {
            return Ok(WriteOutcome::Refused(not_queued(op.name())));
        }
        let _turn = self.lock_lane().await;
        let mut pauses = RETRY_PAUSES.iter();
        let mut unknown = false;
        let mut rebased = false;
        let mut reread = false;
        loop {
            let e = match self.send(&req, cx, None, deadline).await {
                Ok((status, v)) => return Ok(done(op, &req, status, &v, notes)),
                Err(e) => e,
            };
            match e.kind {
                ErrorKind::Offline | ErrorKind::NotZam => {
                    self.link.drop_to_l0(&e);
                    if unknown {
                        return Err(e);
                    }
                    return Ok(WriteOutcome::Refused(not_queued(op.name())));
                }
                ErrorKind::Busy | ErrorKind::RateLimited | ErrorKind::RequestTimeout => {
                    match self.pause(&e, &mut pauses, deadline) {
                        Some(p) => {
                            self.link.clock().sleep(p).await;
                            continue;
                        }
                        None => return Err(e),
                    }
                }
                ErrorKind::Interrupted | ErrorKind::Timeout if !unknown => {
                    unknown = true;
                    // Исход неизвестен. С `If-Match` повтор безопасен: своя правка даст 409, а
                    // 409 — перечитывание. Без него — сначала перечитать (D145).
                    if req.if_match.is_none()
                        && let Some(w) = self.verify(op, &req, cx, deadline, &notes).await
                    {
                        return Ok(w);
                    }
                    continue;
                }
                ErrorKind::Conflict if e.problem() == Some("possible-duplicate") => {
                    return Ok(WriteOutcome::Refused(duplicate_text(op, &e)));
                }
                ErrorKind::Conflict if is_plain_conflict(&e) || unknown => {
                    // 409 после собственной правки — успех после перечитывания (D145). Без
                    // неизвестного исхода сверка — только по состоянию цели: авторство ревизии
                    // заметки и вхождение дописанного текста свою правку не доказывают.
                    if (unknown || state_verifiable(op, &req))
                        && let Some(w) = self.verify(op, &req, cx, deadline, &notes).await
                    {
                        return Ok(w);
                    }
                    if !rebased && rebases(op, &req) {
                        rebased = true;
                        if let Some(rev) = self.current_rev(op, &e, cx, deadline).await {
                            req.if_match = Some(rev);
                            continue;
                        }
                    }
                    return Err(e);
                }
                ErrorKind::PreconditionRequired if !reread => {
                    reread = true;
                    if let Some(rev) = self.current_rev(op, &e, cx, deadline).await {
                        req.if_match = Some(rev);
                        continue;
                    }
                    return Err(e);
                }
                _ => return Err(e),
            }
        }
    }

    /// Пауза повтора (D165): `Busy` — только с `Retry-After` ≤ 5 с; пауза — шаг 2, 4, 8 с или
    /// `Retry-After`, что больше; не успевает до дедлайна — без повтора.
    fn pause<'a>(
        &self,
        e: &ZamError,
        pauses: &mut impl Iterator<Item = &'a Duration>,
        deadline: Instant,
    ) -> Option<Duration> {
        if e.kind == ErrorKind::Busy && e.retry_after.is_some_and(|r| r > BUSY_RETRY_MAX) {
            return None;
        }
        let step = *pauses.next()?;
        let p = e.retry_after.map_or(step, |r| r.max(step));
        (self.link.clock().now() + p < deadline).then_some(p)
    }

    /// Текущая ревизия цели: из 409 (`rev`), иначе перечитыванием.
    async fn current_rev(
        &self,
        op: &WriteOp,
        e: &ZamError,
        cx: &WriteCx,
        deadline: Instant,
    ) -> Option<i64> {
        if let Some(r) = e.current_rev() {
            return Some(r);
        }
        let id = op.target_id()?;
        let v = self
            .get(&format!("{ENTRIES}/{}", enc(id)), cx, deadline)
            .await
            .ok()?;
        v.get("rev").and_then(Value::as_i64)
    }

    /// Сверка с целью (D145): цель уже в нужном состоянии — `Written` без второго запроса.
    async fn verify(
        &self,
        op: &WriteOp,
        req: &Req,
        cx: &WriteCx,
        deadline: Instant,
        notes: &[String],
    ) -> Option<WriteOutcome> {
        let written = |reference: String, rev: i64| WriteOutcome::Written {
            reference,
            rev,
            warnings: notes.to_vec(),
        };
        match op {
            WriteOp::Note { .. } | WriteOp::NodePatch { .. } => {
                let (id, rev) = match (op, req.method.as_str()) {
                    (WriteOp::Note { .. }, "POST") => {
                        let space = req.body.get("space").and_then(Value::as_str)?;
                        let path = req.body.get("path").and_then(Value::as_str)?;
                        let v = self
                            .get(
                                &format!(
                                    "{NODES}/by-path?space={}&path={}",
                                    enc(space),
                                    enc_path(path)
                                ),
                                cx,
                                deadline,
                            )
                            .await
                            .ok()?;
                        let node = v.get("node").filter(|n| n.is_object()).unwrap_or(&v);
                        (node.get("id").and_then(Value::as_str)?.to_owned(), 1)
                    }
                    (WriteOp::NodePatch { id, rev, .. }, _) => (id.clone(), rev + 1),
                    _ => {
                        let id = req.path.rsplit('/').next()?.to_owned();
                        (id, req.if_match? + 1)
                    }
                };
                let revs = self
                    .get(&format!("{NODES}/{}/revisions", enc(&id)), cx, deadline)
                    .await
                    .ok()?;
                let ours = revs.as_array()?.iter().any(|r| {
                    r.get("rev").and_then(Value::as_i64) == Some(rev)
                        && r.get("session_id").and_then(Value::as_str) == Some(cx.session.as_str())
                });
                ours.then(|| written(format!("n:{id}"), rev))
            }
            _ => {
                let id = op.target_id()?;
                let cur = self
                    .get(&format!("{ENTRIES}/{}", enc(id)), cx, deadline)
                    .await
                    .ok()?;
                satisfied(op, &req.body, &cur, cx).then(|| {
                    written(
                        reference(&cur, None),
                        cur.get("rev").and_then(Value::as_i64).unwrap_or(0),
                    )
                })
            }
        }
    }

    /// Разбор идеи (D171): с ключом, мимо очереди.
    async fn review(
        &self,
        op: &WriteOp,
        req: Req,
        cx: &WriteCx,
        deadline: Instant,
        notes: Vec<String>,
    ) -> Result<WriteOutcome, ZamError> {
        if !self.link.available().await {
            return Ok(WriteOutcome::Refused(not_queued(op.name())));
        }
        let _turn = self.lock_lane().await;
        let mut pauses = RETRY_PAUSES.iter();
        let mut unknown = false;
        loop {
            let e = match self.send(&req, cx, Some(&cx.key.0), deadline).await {
                Ok((_, v)) => {
                    let reference = reference(&v, None);
                    if v.get("replay").and_then(Value::as_bool) == Some(true) {
                        return Ok(WriteOutcome::Unchanged {
                            reference,
                            warnings: notes,
                        });
                    }
                    return Ok(WriteOutcome::Written {
                        reference,
                        rev: rev_of(&v),
                        warnings: with_server_warnings(notes, &v),
                    });
                }
                Err(e) => e,
            };
            match e.kind {
                ErrorKind::Offline | ErrorKind::NotZam => {
                    self.link.drop_to_l0(&e);
                    if unknown {
                        return Err(e);
                    }
                    return Ok(WriteOutcome::Refused(not_queued(op.name())));
                }
                ErrorKind::Busy | ErrorKind::RateLimited | ErrorKind::RequestTimeout => {
                    match self.pause(&e, &mut pauses, deadline) {
                        Some(p) => {
                            self.link.clock().sleep(p).await;
                            continue;
                        }
                        None => return Err(e),
                    }
                }
                ErrorKind::Interrupted | ErrorKind::Timeout if !unknown => {
                    unknown = true;
                    continue;
                }
                _ if unknown && review_mismatch(&e) => {
                    let id = op.target_id().unwrap_or_default();
                    let idea = self
                        .get(&format!("{ENTRIES}/{}", enc(id)), cx, deadline)
                        .await;
                    return match idea {
                        Ok(v) if review_key_found(&v, &cx.key.0) => Ok(WriteOutcome::Unchanged {
                            reference: reference(&v, None),
                            warnings: notes,
                        }),
                        _ => Err(e),
                    };
                }
                _ => return Err(e),
            }
        }
    }
}

/// Нужна ли цель (см. модуль, шаг 3).
fn needs_target(op: &WriteOp, cx: &WriteCx) -> bool {
    match op {
        WriteOp::Patch { body, .. } => cx.untrusted || body.get("tags").is_some(),
        WriteOp::NodePatch { .. } | WriteOp::Retract { .. } => cx.untrusted,
        WriteOp::Supersede { .. } | WriteOp::NextStep { .. } => true,
        WriteOp::Note { body, .. } => body.get("base_rev").is_some_and(|v| !v.is_null()),
        _ => false,
    }
}

/// Id вызова из `evidence.checked` записи с `certainty: theorem` (без префикса `call:`).
fn claimed_call(op: &WriteOp) -> Option<String> {
    let (body, evidence) = match op {
        WriteOp::Log(b) | WriteOp::Supersede { body: b, .. } | WriteOp::Patch { body: b, .. } => {
            (b, b.get("data").and_then(|d| d.get("evidence")))
        }
        WriteOp::Note { body, .. } => (body, body.get("evidence")),
        _ => return None,
    };
    if body.get("certainty").and_then(Value::as_str) != Some("theorem") {
        return None;
    }
    let checked = evidence?.get("checked")?.as_str()?.trim();
    let call = checked.strip_prefix(CALL_PREFIX)?.trim();
    (!call.is_empty()).then(|| call.to_owned())
}

/// Цель уже в нужном состоянии (D145): поля правки совпадают с перечитанной записью.
fn satisfied(op: &WriteOp, body: &Value, cur: &Value, cx: &WriteCx) -> bool {
    let data = cur.get("data").cloned().unwrap_or(Value::Null);
    let status = cur.get("status").and_then(Value::as_str);
    let claimed = data
        .get("claimed_by")
        .and_then(|c| c.get("session"))
        .and_then(Value::as_str);
    match op {
        WriteOp::Patch { .. } => body.as_object().is_some_and(|m| {
            m.iter().all(|(k, v)| match k.as_str() {
                "status" | "title" | "body" | "certainty" | "area" => {
                    cur.get(k).unwrap_or(&Value::Null) == v
                }
                "next_step" | "blocked_by" => data.get(k).unwrap_or(&Value::Null) == v,
                "append_body" => {
                    let add = v.as_str().map(str::trim).unwrap_or("");
                    cur.get("body")
                        .and_then(Value::as_str)
                        .is_some_and(|b| b.contains(add))
                }
                "data" => v.as_object().is_some_and(|d| {
                    d.iter()
                        .all(|(k, v)| data.get(k).unwrap_or(&Value::Null) == v)
                }),
                "tags" => {
                    let set = |x: &Value| -> Vec<String> {
                        let mut t: Vec<String> = x
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default();
                        t.sort();
                        t
                    };
                    set(cur.get("tags").unwrap_or(&Value::Null)) == set(v)
                }
                _ => true,
            })
        }),
        WriteOp::NextStep { text, .. } => {
            data.get("next_step").and_then(Value::as_str) == Some(text.trim())
        }
        WriteOp::Claim { .. } => claimed == Some(cx.session.as_str()),
        WriteOp::Release { .. } => claimed != Some(cx.session.as_str()),
        WriteOp::Answer { .. } => {
            let ans = data.get("answer").cloned().unwrap_or(Value::Null);
            let same = |k: &str| {
                body.get(k)
                    .and_then(Value::as_str)
                    .is_none_or(|v| ans.get(k).and_then(Value::as_str) == Some(v))
            };
            status == Some("answered")
                && ans.get("by").and_then(Value::as_str) == Some("agent")
                && same("option")
                && same("text")
        }
        WriteOp::Propose { .. } => status == Some("proposed"),
        WriteOp::Decide { .. } => {
            let want = match body.get("decision").and_then(Value::as_str) {
                Some("accept") => "accepted",
                Some("defer") => "deferred",
                Some("decline") => "declined",
                _ => return false,
            };
            status == Some(want)
        }
        WriteOp::Retract { .. } => status == Some("retracted"),
        _ => false,
    }
}

/// 409 `/problems/conflict` (устаревшая ревизия, уже отозвана, …), а не именованный конфликт
/// (`task-claimed`, `idea-mode`, `already-answered` …).
fn is_plain_conflict(e: &ZamError) -> bool {
    matches!(e.problem(), Some("conflict") | None)
}

/// Сверка по состоянию цели (статус, `next_step`, захват, поля правки) — не по авторству
/// ревизии заметки и не по вхождению дописанного текста.
fn state_verifiable(op: &WriteOp, req: &Req) -> bool {
    match op {
        WriteOp::Note { .. } | WriteOp::NodePatch { .. } => false,
        WriteOp::Patch { .. } => !req
            .body
            .as_object()
            .is_some_and(|m| m.contains_key("append_body")),
        _ => true,
    }
}

/// 409 правки, после которого допустим один повтор с текущей ревизией (D145): `append` и
/// `next_step` не теряют чужих правок.
fn rebases(op: &WriteOp, req: &Req) -> bool {
    match op {
        WriteOp::NextStep { .. } => true,
        WriteOp::Patch { .. } => req.body.as_object().is_some_and(|m| {
            m.contains_key("append_body")
                && m.keys()
                    .all(|k| matches!(k.as_str(), "append_body" | "message" | "refs"))
        }),
        _ => false,
    }
}

/// Ответ разбора после неизвестного исхода, при котором надо перечитать идею (D171).
fn review_mismatch(e: &ZamError) -> bool {
    e.problem() == Some("idempotency-mismatch")
        || e.problem() == Some("idea-mode")
        || e.code.as_deref() == Some("transition-not-allowed")
}

/// Наш ключ — в `data.review.key` или `data.reviews[].key` идеи (D171).
fn review_key_found(idea: &Value, key: &str) -> bool {
    let idea = idea.get("entry").filter(|e| e.is_object()).unwrap_or(idea);
    let data = idea.get("data").cloned().unwrap_or(Value::Null);
    data.get("review")
        .and_then(|r| r.get("key"))
        .and_then(Value::as_str)
        == Some(key)
        || data
            .get("reviews")
            .and_then(Value::as_array)
            .is_some_and(|a| {
                a.iter()
                    .any(|r| r.get("key").and_then(Value::as_str) == Some(key))
            })
}

/// Признак режима идей (D169): 409 `/problems/idea-mode` у `propose` или `review`.
pub fn is_idea_mode(e: &ZamError) -> bool {
    e.kind == ErrorKind::Conflict && e.problem() == Some("idea-mode")
}

/// Итог удачного ответа.
fn done(op: &WriteOp, req: &Req, status: u16, v: &Value, notes: Vec<String>) -> WriteOutcome {
    let reference = reference(v, op.kind());
    let repeat = status == 200
        && req.method == Method::POST
        && matches!(
            op,
            WriteOp::Log(_) | WriteOp::Supersede { .. } | WriteOp::Note { .. }
        );
    if repeat {
        return WriteOutcome::Unchanged {
            reference,
            warnings: with_server_warnings(notes, v),
        };
    }
    WriteOutcome::Written {
        reference,
        rev: rev_of(v),
        warnings: with_server_warnings(notes, v),
    }
}

/// Пометки шлюза, затем строки `warnings` ответа (у ответов-обёрток — и у `entry`).
fn with_server_warnings(mut notes: Vec<String>, v: &Value) -> Vec<String> {
    for src in [Some(v), v.get("entry")].into_iter().flatten() {
        if let Some(a) = src.get("warnings").and_then(Value::as_array) {
            for w in a.iter().filter_map(Value::as_str) {
                if !notes.iter().any(|n| n == w) {
                    notes.push(w.to_owned());
                }
            }
        }
    }
    notes
}

/// Буква ссылки записи по виду (`ref_letter`, `entries/mod.rs@v0.0.12-m6.5`).
fn letter(kind: &str) -> &'static str {
    match kind {
        "insight" => "i",
        "changelog" => "c",
        "release" => "r",
        "term" => "t",
        "task" => "k",
        "question" => "q",
        "report" => "p",
        "idea" => "w",
        _ => "e",
    }
}

/// Ссылка на записанное: поле `ref` (выжимка), запись (`<буква>:<id>`), узел (`n:<id>`); у
/// ответов-обёрток (`{entry, …}`) — по `entry`.
fn reference(v: &Value, kind: Option<&str>) -> String {
    if let Some(r) = v.get("ref").and_then(Value::as_str) {
        return r.to_owned();
    }
    let v = v.get("entry").filter(|e| e.is_object()).unwrap_or(v);
    let id = v.get("id").and_then(Value::as_str).unwrap_or("?");
    if v.get("current_rev").is_some() || v.get("path").is_some() {
        return format!("n:{id}");
    }
    let kind = v.get("kind").and_then(Value::as_str).or(kind).unwrap_or("");
    format!("{}:{id}", letter(kind))
}

/// Ревизия записанного (`rev` записи, `current_rev` узла); у ответов-обёрток — по `entry`.
fn rev_of(v: &Value) -> i64 {
    let v = v.get("entry").filter(|e| e.is_object()).unwrap_or(v);
    v.get("rev")
        .or_else(|| v.get("current_rev"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// Текст модели на 409 `possible-duplicate` (D145).
fn duplicate_text(op: &WriteOp, e: &ZamError) -> String {
    let l = letter(op.kind().unwrap_or("insight"));
    let refs: Vec<String> = e
        .candidates()
        .iter()
        .map(|id| format!("{l}:{id}"))
        .collect();
    let what = if refs.is_empty() {
        "an existing record".to_owned()
    } else {
        refs.join(", ")
    };
    format!("possible duplicate of {what}; append, supersede or set force")
}

/// Отказ записи, которая не встаёт в очередь (D165, D171).
pub fn not_queued(op: &str) -> String {
    format!("{UNAVAILABLE}: {op} is not queued")
}

/// Ошибка очереди без хранения: связи нет или обрыв (до задачи 7).
fn unavailable_error(e: &ZamError) -> ZamError {
    let mut out = ZamError::new(e.kind, UNAVAILABLE).with_detail(e.to_string());
    out.status = e.status;
    out
}

fn deadline_error() -> ZamError {
    ZamError::new(ErrorKind::Timeout, "memory write deadline passed")
}

/// Остаток до дедлайна по часам связи; вышел — `None`.
fn remaining(link: &Link, deadline: Instant) -> Option<Duration> {
    let d = deadline.saturating_duration_since(link.clock().now());
    (!d.is_zero()).then_some(d)
}

/// Маска всех строк тела (MEM-074): ключи — схема ZAM, не маскируются.
pub fn mask_value(m: &SecretMasker, v: &mut Value) {
    match v {
        Value::String(s) => {
            if let std::borrow::Cow::Owned(masked) = m.mask(s) {
                *s = masked;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| mask_value(m, x)),
        Value::Object(o) => o.values_mut().for_each(|x| mask_value(m, x)),
        _ => {}
    }
}

fn take_str(body: &mut Value, k: &str) -> Option<String> {
    body.as_object_mut()
        .and_then(|m: &mut Map<String, Value>| m.remove(k))
        .and_then(|v| v.as_str().map(str::to_owned))
}

/// Сегмент пути URL: всё, кроме неизменяемых символов RFC 3986, — `%XX`.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Путь узла в строке запроса: как [`enc`], но `/` остаётся.
fn enc_path(s: &str) -> String {
    s.split('/').map(enc).collect::<Vec<_>>().join("/")
}

/// Тексты шлюза для модели (хэш промптов, D94, D155; подключение — задача 8).
pub fn text_samples() -> Vec<(&'static str, String)> {
    let mut v = gate::text_samples();
    v.push(("memory/gateway.not_queued", not_queued("OP")));
    v.push((
        "memory/gateway.duplicate",
        "possible duplicate of i:ID; append, supersede or set force".to_owned(),
    ));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_segments_encoded() {
        assert_eq!(enc("01ABC"), "01ABC");
        assert_eq!(enc("a b/с"), "a%20b%2F%D1%81");
        assert_eq!(
            enc_path("Заметки/a b"),
            "%D0%97%D0%B0%D0%BC%D0%B5%D1%82%D0%BA%D0%B8/a%20b"
        );
    }

    #[test]
    fn references_by_kind_and_shape() {
        assert_eq!(reference(&json!({"id": "A", "kind": "task"}), None), "k:A");
        assert_eq!(reference(&json!({"id": "A"}), Some("insight")), "i:A");
        assert_eq!(
            reference(&json!({"id": "N", "current_rev": 2, "path": "a"}), None),
            "n:N"
        );
        assert_eq!(reference(&json!({"id": "C", "ref": "c_C"}), None), "c_C");
        assert_eq!(
            reference(
                &json!({"entry": {"id": "W", "kind": "idea"}, "replay": true}),
                None
            ),
            "w:W"
        );
    }
}
