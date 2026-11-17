//! Порт памяти ZeroAIMemory для цикла: `ZamMemory: za_core::memory::MemoryPort` (R1-M D138,
//! D136, D139–D142, D168, D169).
//!
//! - **Сессия run** регистрируется перед первым запросом run (D139). Регистрация не блокирует
//!   `/recall`: без неё сервер создаёт сессию неявно по `X-Zam-Session`
//!   (`api/sessions.rs:197-209@v0.0.12-m6.5`), теряются только метаданные (`agent`, `solution`,
//!   `area`, `agent_type`). Итог регистрации (I2 ревью задачи 4):
//!   - временная ошибка со статусом (`Busy`, `Server`, `RateLimited`) — `/recall` идёт,
//!     регистрация повторяется при следующем вызове порта, но не раньше `Retry-After`;
//!   - срок вышел (`Timeout`) — итог `Timeout` без `/recall` (сервер молчит — второй запрос
//!     ждал бы ещё срок), повтор при следующем вызове;
//!   - нет соединения, обрыв, «не ZAM» — L0 и `Unavailable`, повтор после пробы;
//!   - прочий отказ (4xx, нарушение протокола) — без повтора, `/recall` идёт, предупреждение
//!     [`WARN_SESSION_NOT_REGISTERED`] — в отчёте `memory.recall` этого запроса.
//! - **Чтения** — `/recall` ([`crate::recall`]): дайджест шага 0 и его повтор, пакет
//!   восстановления, подсказки к вводу и к ошибке инструмента. Текст укладывается в бюджет
//!   ([`crate::fit`]); ввод подсказок маскируется до отправки. Очистку невидимых символов, маску
//!   ответа и экранирование делает драйвер (`za_agent::memory`): здесь текст ответа второй раз
//!   не чистится.
//! - **Режим идей укладки** (D169, I3 ревью задачи 4): без флага `idea_review` у сервера —
//!   всегда `Wait`; признак того же ответа (элемент `items[]` с `reason: idea_review`) —
//!   `Review`; иначе — признак run, который передал драйвер (аргумент `ideas` методов порта,
//!   `MemoryState.ideas`); нет и его — `Wait`. Порт режим не хранит: признак выводится из
//!   журнала, после resume он тот же. Признак ответа порт отдаёт полем `ideas` значения эффекта.
//!   Подсказка к ошибке инструмента (`post_tool`) идей не несёт (на теге `v0.0.12-m6.5`
//!   `idea_cand` есть только у дайджеста, `prompt` и пакета `idea_review`) — её режим `Wait`.
//! - **Без повторов ссылок** (D140): порт помнит ссылки, показанные в текущей эпохе контекста,
//!   и не показывает их снова — между дайджестом, подсказками и повтором. Дайджест шага 0
//!   (первое сообщение run) и пакет восстановления начинают новую эпоху: прежний текст уже вне
//!   контекста. Память — только в процессе: после resume порт её не знает (повтор может
//!   показать строку ещё раз).
//! - **Уровень** — `watch` ([`ZamMemory::level_watch`]): нет соединения, обрыв или «не ZAM» —
//!   L0; три `Timeout` подряд без ответа сервера (хост за фильтром, который молча отбрасывает
//!   пакеты; Minor 7 ревью задачи 4) — тоже L0, любой ответ сервера счёт сбрасывает. После L0 —
//!   проба `/api/v1/health` не чаще раза в 30 с при следующем обращении (D136), время —
//!   [`Clock`] (D165). Ошибка со статусом уровень не меняет.
//! - **Отчёты** `memory.recall` (D153) копятся в порту ([`ZamMemory::drain_reports`]): журнал
//!   run пишет владелец порта.
//! - Запись (`session_note`, `files_changed`, `end_session`) — умолчания порта до задачи 9.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use za_core::BoxFuture;
use za_core::memory::{IdeasSignal, MemoryLevel, MemoryPort, Recall, RecallReply};
use za_protocol::RunId;
use za_secrets::SecretMasker;
use za_store::ProjectStore;

use crate::clock::Clock;
use crate::discovery::{Discovery, Level, discover};
use crate::error::{ErrorKind, ZamError};
use crate::fit::fit_with;
use crate::ideas::{IdeasMode, ideas_mode};
use crate::recall::{
    Answer, Ask, DIGEST_TIMEOUT_MS, FEATURE_RECALL_TIMEOUT, RECALL_PATH, RETRY_TIMEOUT_MS,
    RecallReport, error_field, outcome, prompt_field,
};
use crate::session::{RunBinding, WARN_SESSION_NOT_REGISTERED, register, session_id};
use crate::transport::{Ctx, Method, ZamClient};

/// Повторная проба после перехода в L0 — не чаще (D136).
pub const PROBE_INTERVAL: Duration = Duration::from_secs(30);
/// `Timeout` подряд без ответа сервера до перехода в L0 («мёртвый хост», Minor 7 ревью
/// задачи 4).
pub const DEAD_HOST_TIMEOUTS: u32 = 3;
/// Сколько отчётов `memory.recall` порт держит до [`ZamMemory::drain_reports`]; старые
/// вытесняются.
const REPORTS_MAX: usize = 256;

/// Части порта памяти, которые собирает бинарник (задачи 5, 12).
pub struct ZamParts {
    pub client: Arc<ZamClient>,
    /// Обнаружение на старте команды (D136).
    pub discovery: Discovery,
    pub binding: RunBinding,
    /// Маска процесса: ввод подсказок до отправки (D142).
    pub masker: Arc<SecretMasker>,
    /// Хранилище проекта: outbox записей (задача 7); чтениям не нужно.
    pub store: Option<ProjectStore>,
    pub clock: Arc<dyn Clock>,
}

/// Изменяемое состояние порта.
#[derive(Debug)]
struct State {
    /// Флаги сервера по последнему удачному обнаружению.
    features: BTreeSet<String>,
    /// Последняя проба или переход в L0 (D136).
    probed_at: Instant,
    /// Сессии run, регистрация которых закончена: успехом или отказом без повтора (D139).
    registered: BTreeSet<String>,
    /// Повтор регистрации сессии — не раньше (`Retry-After` временной ошибки).
    register_after: BTreeMap<String, Instant>,
    /// `Timeout` подряд без ответа сервера.
    timeouts: u32,
    /// Ссылки, показанные в текущей эпохе контекста (D140).
    shown: BTreeSet<String>,
    /// Выключенные типы подсказок по последнему `session_start`.
    hooks_off: Vec<String>,
    reports: VecDeque<RecallReport>,
}

/// Итог регистрации сессии для запроса `/recall`.
enum Registration {
    /// Запрос идёт; `Some` — предупреждение в отчёт этого запроса.
    Go(Option<String>),
    /// Запроса не будет: итог вызова.
    Stop(Recall),
}

/// Порт памяти ZeroAIMemory.
pub struct ZamMemory {
    parts: ZamParts,
    level: watch::Sender<Level>,
    state: Mutex<State>,
}

impl ZamMemory {
    pub fn new(p: ZamParts) -> Self {
        let features = p
            .discovery
            .info
            .as_ref()
            .map(|i| i.features.clone())
            .unwrap_or_default();
        let state = State {
            features,
            probed_at: p.clock.now(),
            registered: BTreeSet::new(),
            register_after: BTreeMap::new(),
            timeouts: 0,
            shown: BTreeSet::new(),
            hooks_off: Vec::new(),
            reports: VecDeque::new(),
        };
        let (level, _) = watch::channel(p.discovery.level);
        Self {
            parts: p,
            level,
            state: Mutex::new(state),
        }
    }

    /// Подписка на уровень памяти: смену видит бинарник (D137).
    pub fn level_watch(&self) -> watch::Receiver<Level> {
        self.level.subscribe()
    }

    /// Накопленные отчёты `memory.recall` (D153), порт их забывает.
    pub fn drain_reports(&self) -> Vec<RecallReport> {
        self.state().reports.drain(..).collect()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn current(&self) -> Level {
        *self.level.borrow()
    }

    /// Можно ли обращаться к серверу: при L0 — проба не чаще раза в [`PROBE_INTERVAL`].
    async fn available(&self) -> bool {
        if self.current() != Level::L0 {
            return true;
        }
        let now = self.parts.clock.now();
        {
            let mut st = self.state();
            if now.saturating_duration_since(st.probed_at) < PROBE_INTERVAL {
                return false;
            }
            // Слот пробы занят сразу: параллельный вызов не пробует второй раз.
            st.probed_at = now;
        }
        let d = discover(&self.parts.client).await;
        let Some(info) = d.info.filter(|_| d.level != Level::L0) else {
            self.state().probed_at = self.parts.clock.now();
            return false;
        };
        {
            let mut st = self.state();
            st.features = info.features;
            st.timeouts = 0;
        }
        self.level.send_replace(d.level);
        tracing::info!(level = d.level.as_str(), "память: сервер снова доступен");
        true
    }

    /// Нет соединения, обрыв, «не ZAM» или мёртвый хост — L0 с повторной пробой через
    /// [`PROBE_INTERVAL`].
    fn drop_to_l0(&self, e: &ZamError) {
        {
            let mut st = self.state();
            st.probed_at = self.parts.clock.now();
            st.timeouts = 0;
        }
        if self.level.send_replace(Level::L0) != Level::L0 {
            tracing::warn!(error = %e, "zero-agent: память недоступна; записи — в очередь");
        }
    }

    /// Счёт мёртвого хоста по итогу запроса: `Timeout` без ответа сервера — +1, на
    /// [`DEAD_HOST_TIMEOUTS`] подряд — L0; любой ответ сервера (успех или ошибка со статусом)
    /// счёт сбрасывает. Нет соединения, обрыв, «не ZAM» переводят в L0 сами ([`Self::failed`]).
    fn note_outcome(&self, e: Option<&ZamError>) {
        let dead = {
            let mut st = self.state();
            match e {
                Some(e) if e.kind == ErrorKind::Timeout => {
                    st.timeouts += 1;
                    st.timeouts >= DEAD_HOST_TIMEOUTS
                }
                _ => {
                    st.timeouts = 0;
                    false
                }
            }
        };
        if let Some(e) = e.filter(|_| dead) {
            tracing::warn!(
                timeouts = DEAD_HOST_TIMEOUTS,
                "память: сервер не отвечает в срок подряд — уровень L0"
            );
            self.drop_to_l0(e);
        }
    }

    /// Итог ошибки запроса: срок вышел — `Timeout`; сервер недоступен — `Unavailable` и L0;
    /// ошибка со статусом — `Unavailable`, уровень прежний.
    fn failed(&self, event: &str, e: &ZamError) -> Recall {
        match e.kind {
            ErrorKind::Timeout => Recall::Timeout,
            ErrorKind::Offline | ErrorKind::Interrupted | ErrorKind::NotZam => {
                self.drop_to_l0(e);
                Recall::Unavailable
            }
            _ => {
                tracing::warn!(event, error = %e, "память: /recall не удался");
                Recall::Unavailable
            }
        }
    }

    /// Регистрация сессии `sid` (D139, I2 ревью задачи 4): до успеха или отказа без повтора,
    /// временная ошибка — повтор при следующем вызове (не раньше `Retry-After`).
    async fn ensure_session(&self, sid: &str, cx: &Ctx<'_>) -> Registration {
        {
            let st = self.state();
            if st.registered.contains(sid) {
                return Registration::Go(None);
            }
            if st
                .register_after
                .get(sid)
                .is_some_and(|at| self.parts.clock.now() < *at)
            {
                return Registration::Go(None);
            }
        }
        let r = register(&self.parts.client, sid, &self.parts.binding, cx).await;
        self.note_outcome(r.as_ref().err());
        let e = match r {
            Ok(()) => {
                let mut st = self.state();
                st.register_after.remove(sid);
                st.registered.insert(sid.to_owned());
                return Registration::Go(None);
            }
            Err(e) => e,
        };
        match e.kind {
            ErrorKind::Timeout => Registration::Stop(Recall::Timeout),
            ErrorKind::Offline | ErrorKind::Interrupted | ErrorKind::NotZam => {
                self.drop_to_l0(&e);
                Registration::Stop(Recall::Unavailable)
            }
            ErrorKind::Busy | ErrorKind::Server | ErrorKind::RateLimited => {
                tracing::warn!(error = %e, "память: сессия run не зарегистрирована, повтор при следующем запросе");
                if let Some(d) = e.retry_after {
                    let at = self.parts.clock.now() + d;
                    self.state().register_after.insert(sid.to_owned(), at);
                }
                Registration::Go(None)
            }
            _ => {
                tracing::warn!(error = %e, "память: сервер отказал в регистрации сессии run");
                let mut st = self.state();
                st.register_after.remove(sid);
                st.registered.insert(sid.to_owned());
                let warning = match e.status {
                    Some(s) => format!("{WARN_SESSION_NOT_REGISTERED}: {s} {}", e.kind.as_str()),
                    None => format!("{WARN_SESSION_NOT_REGISTERED}: {}", e.kind.as_str()),
                };
                Registration::Go(Some(warning))
            }
        }
    }

    /// Отчёт `memory.recall` с выключенными типами подсказок и предупреждением клиента.
    fn report(&self, mut r: RecallReport, warning: Option<String>) {
        let mut st = self.state();
        r.hooks_off = st.hooks_off.clone();
        r.warnings.extend(warning);
        if st.reports.len() >= REPORTS_MAX {
            st.reports.pop_front();
        }
        st.reports.push_back(r);
    }

    /// Запрос `/recall` run в бюджете `budget` и укладка ответа; `ideas` — признак режима
    /// идей run от драйвера.
    async fn ask(
        &self,
        run: RunId,
        ask: Ask,
        budget: u32,
        ideas: Option<IdeasSignal>,
    ) -> RecallReply {
        if budget == 0 {
            return Recall::Capped.into();
        }
        if !self.available().await {
            return Recall::Unavailable.into();
        }
        let b = &self.parts.binding;
        let sid = session_id(&b.project_id, run);
        let cx = Ctx {
            session: Some(&sid),
            solution: b.solution.as_deref(),
            area: b.area.as_deref(),
        };
        let warning = match self.ensure_session(&sid, &cx).await {
            Registration::Go(w) => w,
            Registration::Stop(r) => return r.into(),
        };
        let features = self.state().features.clone();
        let body = ask.body(&sid, b, budget, features.contains(FEATURE_RECALL_TIMEOUT));
        let started = self.parts.clock.now();
        let res = self
            .parts
            .client
            .send_json(
                Method::POST,
                RECALL_PATH,
                &body,
                &cx,
                None,
                None,
                ask.limit(),
            )
            .await;
        let ms = u64::try_from(
            self.parts
                .clock
                .now()
                .saturating_duration_since(started)
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        self.note_outcome(res.as_ref().err());
        let v = match res {
            Ok((_, v)) => v,
            Err(e) => {
                let recall = self.failed(ask.event, &e);
                self.report(RecallReport::new(ask.event, recall.state(), ms), warning);
                return recall.into();
            }
        };
        let Some(answer) = Answer::parse(&v) else {
            tracing::warn!(event = ask.event, "память: ответ /recall без text");
            self.report(
                RecallReport::new(ask.event, Recall::Unavailable.state(), ms),
                warning,
            );
            return Recall::Unavailable.into();
        };
        let mode = fit_mode(&features, &answer, ideas);
        let fitted = {
            let mut st = self.state();
            if let Some(off) = &answer.hooks_off {
                st.hooks_off = off.clone();
            }
            let f = fit_with(
                &answer.text,
                budget,
                answer.tokens,
                mode,
                &answer.review_refs,
                &st.shown,
            );
            st.shown.extend(f.refs.iter().cloned());
            f
        };
        let recall = outcome(&fitted, answer.timed_out());
        self.report(
            RecallReport::new(ask.event, recall.state(), ms).with_answer(&fitted, &answer),
            warning,
        );
        RecallReply {
            recall,
            ideas: (!answer.review_refs.is_empty()).then_some(IdeasSignal::Review),
        }
    }
}

/// Режим укладки ответа (D169): признак того же ответа (`items[].reason: idea_review`) важнее
/// признака run `ideas`; без флага `idea_review` у сервера — `Wait` ([`ideas_mode`]).
fn fit_mode(features: &BTreeSet<String>, answer: &Answer, ideas: Option<IdeasSignal>) -> IdeasMode {
    let visible = if answer.review_refs.is_empty() {
        ideas
    } else {
        Some(IdeasSignal::Review)
    };
    ideas_mode(features, visible.map(IdeasSignal::as_str))
}

impl MemoryPort for ZamMemory {
    fn level(&self) -> MemoryLevel {
        match self.current() {
            Level::L0 => MemoryLevel::L0,
            Level::L1 => MemoryLevel::L1,
            Level::L2 => MemoryLevel::L2,
        }
    }

    fn snapshot(
        &self,
        run: RunId,
        budget_tokens: u32,
        ideas: Option<IdeasSignal>,
    ) -> BoxFuture<'_, RecallReply> {
        Box::pin(async move {
            // Первое сообщение run — новый контекст: показанное другим run не в счёт.
            self.state().shown.clear();
            self.ask(run, Ask::digest(DIGEST_TIMEOUT_MS), budget_tokens, ideas)
                .await
        })
    }

    /// Снимок сессии перед компакцией — задача 9; здесь ничего.
    fn before_compaction<'a>(&'a self, _run: RunId, _next_step: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn after_compaction(
        &self,
        run: RunId,
        budget_tokens: u32,
        ideas: Option<IdeasSignal>,
    ) -> BoxFuture<'_, Recall> {
        Box::pin(async move {
            // Новая эпоха: показанное до компакции уже вне контекста.
            self.state().shown.clear();
            self.ask(run, Ask::recovery(), budget_tokens, ideas)
                .await
                .recall
        })
    }

    fn prompt_hint<'a>(
        &'a self,
        run: RunId,
        text: &'a str,
        budget: u32,
        ideas: Option<IdeasSignal>,
    ) -> BoxFuture<'a, RecallReply> {
        Box::pin(async move {
            let prompt = prompt_field(&self.parts.masker, text);
            self.ask(run, Ask::prompt(prompt), budget, ideas).await
        })
    }

    fn tool_error_hint<'a>(
        &'a self,
        run: RunId,
        tool: &'a str,
        error: &'a str,
        budget: u32,
    ) -> BoxFuture<'a, Recall> {
        Box::pin(async move {
            let error = error_field(&self.parts.masker, error);
            // `post_tool` идей не несёт: режим укладки — `Wait`.
            self.ask(run, Ask::post_tool(tool, error), budget, None)
                .await
                .recall
        })
    }

    fn retry_digest(
        &self,
        run: RunId,
        budget: u32,
        ideas: Option<IdeasSignal>,
    ) -> BoxFuture<'_, RecallReply> {
        Box::pin(async move {
            self.ask(run, Ask::digest(RETRY_TIMEOUT_MS), budget, ideas)
                .await
        })
    }
}
