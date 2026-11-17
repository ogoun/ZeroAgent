//! Порт памяти ZeroAIMemory для цикла: `ZamMemory: za_core::memory::MemoryPort` (R1-M D138,
//! D136, D139–D142, D168, D169).
//!
//! - **Сессия run** регистрируется один раз на run перед первым запросом (D139): ответ со
//!   статусом — сервер жив, регистрация не повторяется (сессию создаст и первый запрос с
//!   `X-Zam-Session`); не соединились — уровень L0.
//! - **Чтения** — `/recall` ([`crate::recall`]): дайджест шага 0 и его повтор, пакет
//!   восстановления, подсказки к вводу и к ошибке инструмента. Текст укладывается в бюджет
//!   ([`crate::fit`]) в режиме идей порта; ввод подсказок маскируется до отправки. Очистку
//!   невидимых символов, маску ответа и экранирование делает драйвер (`za_agent::memory`):
//!   здесь текст ответа второй раз не чистится.
//! - **Без повторов ссылок** (D140): порт помнит ссылки, показанные в текущей эпохе контекста,
//!   и не показывает их снова — между дайджестом, подсказками и повтором. Дайджест шага 0
//!   (первое сообщение run) и пакет восстановления начинают новую эпоху: прежний текст уже вне
//!   контекста. Память — только в процессе: после resume порт её не знает (повтор может
//!   показать строку ещё раз).
//! - **Уровень** — `watch` ([`ZamMemory::level_watch`]): нет соединения, обрыв или «не ZAM» —
//!   L0; после L0 — проба `/api/v1/health` не чаще раза в 30 с при следующем обращении (D136),
//!   время — [`Clock`] (D165). Медленный ответ (`Timeout`) и ошибка со статусом уровень не
//!   меняют.
//! - **Режим идей** — [`ZamParts::ideas`] (его выводит бинарник из `MemoryState.ideas`,
//!   [`crate::ideas::ideas_mode`]); без флага `idea_review` у сервера — всегда `Wait`. Признак
//!   режима из ответа (`items[].reason: idea_review`) порт не хранит, а отдаёт полем `ideas`
//!   значения эффекта (D169).
//! - **Отчёты** `memory.recall` (D153) копятся в порту ([`ZamMemory::drain_reports`]): журнал
//!   run пишет владелец порта.
//! - Запись (`session_note`, `files_changed`, `end_session`) — умолчания порта до задачи 9.

use std::collections::{BTreeSet, VecDeque};
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
use crate::ideas::{FEATURE_IDEA_REVIEW, IdeasMode};
use crate::recall::{
    Answer, Ask, DIGEST_TIMEOUT_MS, FEATURE_RECALL_TIMEOUT, RECALL_PATH, RETRY_TIMEOUT_MS,
    RecallReport, error_field, outcome, prompt_field,
};
use crate::session::{RunBinding, register, session_id};
use crate::transport::{Ctx, Method, ZamClient};

/// Повторная проба после перехода в L0 — не чаще (D136).
pub const PROBE_INTERVAL: Duration = Duration::from_secs(30);
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
    /// Режим идей run (D148, D169).
    pub ideas: IdeasMode,
}

/// Изменяемое состояние порта.
#[derive(Debug)]
struct State {
    /// Флаги сервера по последнему удачному обнаружению.
    features: BTreeSet<String>,
    /// Последняя проба или переход в L0 (D136).
    probed_at: Instant,
    /// Зарегистрированные сессии run (D139).
    registered: BTreeSet<String>,
    /// Ссылки, показанные в текущей эпохе контекста (D140).
    shown: BTreeSet<String>,
    /// Выключенные типы подсказок по последнему `session_start`.
    hooks_off: Vec<String>,
    reports: VecDeque<RecallReport>,
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

    /// Режим идей укладки: без флага `idea_review` у сервера — `Wait` (D169).
    fn mode(&self, features: &BTreeSet<String>) -> IdeasMode {
        if features.contains(FEATURE_IDEA_REVIEW) {
            self.parts.ideas
        } else {
            IdeasMode::Wait
        }
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
        self.state().features = info.features;
        self.level.send_replace(d.level);
        tracing::info!(level = d.level.as_str(), "память: сервер снова доступен");
        true
    }

    /// Нет соединения, обрыв или «не ZAM» — L0 с повторной пробой через [`PROBE_INTERVAL`].
    fn drop_to_l0(&self, e: &ZamError) {
        self.state().probed_at = self.parts.clock.now();
        if self.level.send_replace(Level::L0) != Level::L0 {
            tracing::warn!(error = %e, "zero-agent: память недоступна; записи — в очередь");
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

    /// Регистрация сессии `sid` один раз на run (D139). `Some(итог)` — запрос `/recall` не
    /// идёт.
    async fn ensure_session(&self, sid: &str, cx: &Ctx<'_>) -> Option<Recall> {
        if self.state().registered.contains(sid) {
            return None;
        }
        match register(&self.parts.client, &self.parts.binding, cx).await {
            Ok(()) => {}
            Err(e) if e.kind == ErrorKind::Timeout => return Some(Recall::Timeout),
            Err(e) if e.is_unreachable() => {
                self.drop_to_l0(&e);
                return Some(Recall::Unavailable);
            }
            Err(e) => tracing::warn!(error = %e, "память: сессия run не зарегистрирована"),
        }
        self.state().registered.insert(sid.to_owned());
        None
    }

    fn report(&self, mut r: RecallReport) {
        let mut st = self.state();
        r.hooks_off = st.hooks_off.clone();
        if st.reports.len() >= REPORTS_MAX {
            st.reports.pop_front();
        }
        st.reports.push_back(r);
    }

    /// Запрос `/recall` run в бюджете `budget` и укладка ответа.
    async fn ask(&self, run: RunId, ask: Ask, budget: u32) -> RecallReply {
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
        if let Some(r) = self.ensure_session(&sid, &cx).await {
            return r.into();
        }
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
        let v = match res {
            Ok((_, v)) => v,
            Err(e) => {
                let recall = self.failed(ask.event, &e);
                self.report(RecallReport::new(ask.event, recall.state(), ms));
                return recall.into();
            }
        };
        let Some(answer) = Answer::parse(&v) else {
            tracing::warn!(event = ask.event, "память: ответ /recall без text");
            self.report(RecallReport::new(
                ask.event,
                Recall::Unavailable.state(),
                ms,
            ));
            return Recall::Unavailable.into();
        };
        let mode = self.mode(&features);
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
        self.report(RecallReport::new(ask.event, recall.state(), ms).with_answer(&fitted, &answer));
        RecallReply {
            recall,
            ideas: (!answer.review_refs.is_empty()).then_some(IdeasSignal::Review),
        }
    }
}

impl MemoryPort for ZamMemory {
    fn level(&self) -> MemoryLevel {
        match self.current() {
            Level::L0 => MemoryLevel::L0,
            Level::L1 => MemoryLevel::L1,
            Level::L2 => MemoryLevel::L2,
        }
    }

    fn snapshot(&self, run: RunId, budget_tokens: u32) -> BoxFuture<'_, RecallReply> {
        Box::pin(async move {
            // Первое сообщение run — новый контекст: показанное другим run не в счёт.
            self.state().shown.clear();
            self.ask(run, Ask::digest(DIGEST_TIMEOUT_MS), budget_tokens)
                .await
        })
    }

    /// Снимок сессии перед компакцией — задача 9; здесь ничего.
    fn before_compaction<'a>(&'a self, _run: RunId, _next_step: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn after_compaction(&self, run: RunId, budget_tokens: u32) -> BoxFuture<'_, Recall> {
        Box::pin(async move {
            // Новая эпоха: показанное до компакции уже вне контекста.
            self.state().shown.clear();
            self.ask(run, Ask::recovery(), budget_tokens).await.recall
        })
    }

    fn prompt_hint<'a>(
        &'a self,
        run: RunId,
        text: &'a str,
        budget: u32,
    ) -> BoxFuture<'a, RecallReply> {
        Box::pin(async move {
            let prompt = prompt_field(&self.parts.masker, text);
            self.ask(run, Ask::prompt(prompt), budget).await
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
            self.ask(run, Ask::post_tool(tool, error), budget)
                .await
                .recall
        })
    }

    fn retry_digest(&self, run: RunId, budget: u32) -> BoxFuture<'_, Recall> {
        Box::pin(async move {
            self.ask(run, Ask::digest(RETRY_TIMEOUT_MS), budget)
                .await
                .recall
        })
    }
}
