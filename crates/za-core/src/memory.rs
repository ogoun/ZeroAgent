//! Порт памяти цикла (часть 04 ТЗ, MEM-020; R1-M D138): то, что цикл знает о памяти, без
//! транспорта. Реализация ZeroAIMemory (`za-memory`) и цикл (`za-agent`) встречаются
//! здесь: `za-agent` не зависит от клиента памяти, клиент — от цикла.
//!
//! Внешнее значение входит в цикл только эффектом (D9): итоги чтения [`Recall`] и
//! [`RecallReply`] пишутся в квитанцию значением `{text, state}` (D140) и при реплее
//! читаются из неё. Признак режима идей [`IdeasSignal`] — необязательное поле `ideas`
//! того же значения (D169). Квитанция R1-B `{text}` без `state` читается: текст —
//! `hits`, `null` — `unavailable`.
//!
//! [`NoMemory`] — режим L0 (N-16): память недоступна, чтения — [`Recall::Unavailable`].

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use za_protocol::RunId;

use crate::BoxFuture;

/// Событие `/recall` подсказки к вводу пользователя (D142): значение аргумента `event`
/// эффекта `memory.prompt`.
pub const PROMPT_EVENT: &str = "prompt";
/// Событие `/recall` подсказки к ошибке инструмента (D142): значение аргумента `event`
/// эффекта `memory.tool_error`.
pub const POST_TOOL_EVENT: &str = "post_tool";

/// Уровень доступной памяти (часть 04, §3; D136): L0 — офлайн или не ZeroAIMemory, L1 —
/// сервер без `features` (`0.0.10-m5.7`), L2 — `features` с `m6` (уровни L2 и L3 части 04
/// на `0.0.11-m6` слились).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MemoryLevel {
    L0,
    L1,
    L2,
}

/// Итог чтения памяти: «пусто», «недоступна», «не успели» и «потолок» различаются
/// (MEM-002, MEM-034, MEM-122; D168).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Recall {
    /// Текст в пределах переданного бюджета.
    Hits(String),
    /// Текст есть, но сервер предупредил `recall timeout` (D168): текст показывается
    /// сразу, ставится один отложенный повтор (D140).
    Partial(String),
    /// Память ответила, но нечего показать: текста в промпте нет (MEM-030).
    Empty,
    /// Память недоступна (L0, ошибка соединения, отозванный токен): явный отказ.
    Unavailable,
    /// Ответ не пришёл в срок (MEM-024) или пришёл с `recall timeout` без текста: один
    /// отложенный повтор вне горячего пути (MEM-034).
    Timeout,
    /// Потолок подсказок эпохи исчерпан (D168): запроса к памяти не было.
    Capped,
}

/// Значения поля `state` (D140).
const HITS: &str = "hits";
const PARTIAL: &str = "partial";
const EMPTY: &str = "empty";
const UNAVAILABLE: &str = "unavailable";
const TIMEOUT: &str = "timeout";
const CAPPED: &str = "capped";

impl Recall {
    /// Текст для промпта: у `Hits` и `Partial`, не из одних пробельных символов.
    pub fn text(&self) -> Option<&str> {
        match self {
            Recall::Hits(t) | Recall::Partial(t) if !t.trim().is_empty() => Some(t),
            _ => None,
        }
    }

    /// Поле `state` значения эффекта (D140): `hits`, `partial`, `empty`, `unavailable`,
    /// `timeout`, `capped`.
    pub fn state(&self) -> &'static str {
        match self {
            Recall::Hits(_) => HITS,
            Recall::Partial(_) => PARTIAL,
            Recall::Empty => EMPTY,
            Recall::Unavailable => UNAVAILABLE,
            Recall::Timeout => TIMEOUT,
            Recall::Capped => CAPPED,
        }
    }

    /// Нужен ли отложенный повтор (D140, D168): `timeout` и `partial`.
    pub fn needs_retry(&self) -> bool {
        matches!(self, Recall::Timeout | Recall::Partial(_))
    }

    /// Тот же итог с текстом `f(text)` (очистка и маска до квитанции, D140).
    pub fn map_text(self, f: impl FnOnce(String) -> String) -> Recall {
        match self {
            Recall::Hits(t) => Recall::Hits(f(t)),
            Recall::Partial(t) => Recall::Partial(f(t)),
            other => other,
        }
    }

    /// Значение эффекта `{text, state}` (D140); у итогов без текста `text` — `null`.
    pub fn to_value(&self) -> Value {
        let text = match self {
            Recall::Hits(t) | Recall::Partial(t) => Value::String(t.clone()),
            _ => Value::Null,
        };
        json!({ "text": text, "state": self.state() })
    }

    /// Итог из значения эффекта. Без `state` (квитанция R1-B): текст — `Hits`, `null` —
    /// `Unavailable`. Незнакомое состояние и не объект — `Unavailable`: без текста, а не
    /// догадка.
    pub fn from_value(v: &Value) -> Recall {
        let text = || {
            v.get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        match v.get("state").and_then(Value::as_str) {
            None => match v.get("text").and_then(Value::as_str) {
                Some(t) => Recall::Hits(t.to_owned()),
                None => Recall::Unavailable,
            },
            Some(HITS) => Recall::Hits(text()),
            Some(PARTIAL) => Recall::Partial(text()),
            Some(EMPTY) => Recall::Empty,
            Some(TIMEOUT) => Recall::Timeout,
            Some(CAPPED) => Recall::Capped,
            Some(_) => Recall::Unavailable,
        }
    }
}

/// Признак режима идей в ответе памяти (D169): `review` — разбор идей агентом включён,
/// `propose` — выключен. Нейтральный тип: режим `IdeasMode` выводит `za-memory`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdeasSignal {
    Review,
    Propose,
}

impl IdeasSignal {
    /// Значение поля `ideas` (D169).
    pub fn as_str(self) -> &'static str {
        match self {
            IdeasSignal::Review => "review",
            IdeasSignal::Propose => "propose",
        }
    }

    /// Признак из значения поля; незнакомое — `None`.
    pub fn parse(s: &str) -> Option<IdeasSignal> {
        match s {
            "review" => Some(IdeasSignal::Review),
            "propose" => Some(IdeasSignal::Propose),
            _ => None,
        }
    }
}

/// Ответ памяти на `session_start` и `prompt` (D169): итог чтения и признак режима идей,
/// если сервер его дал. Значение эффекта — [`Recall::to_value`] и поле `ideas` только при
/// признаке: квитанции без признака не отличаются от прежних.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecallReply {
    pub recall: Recall,
    pub ideas: Option<IdeasSignal>,
}

impl From<Recall> for RecallReply {
    fn from(recall: Recall) -> Self {
        Self {
            recall,
            ideas: None,
        }
    }
}

impl RecallReply {
    /// Значение эффекта `{text, state, ideas?}`.
    pub fn to_value(&self) -> Value {
        let mut v = self.recall.to_value();
        if let Some(s) = self.ideas {
            v["ideas"] = json!(s.as_str());
        }
        v
    }

    /// Ответ из значения эффекта: [`Recall::from_value`] и необязательный признак.
    pub fn from_value(v: &Value) -> RecallReply {
        RecallReply {
            recall: Recall::from_value(v),
            ideas: v
                .get("ideas")
                .and_then(Value::as_str)
                .and_then(IdeasSignal::parse),
        }
    }
}

/// Команда оболочки run для снимка сессии (D149): текст и итог.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandNote {
    pub cmd: String,
    pub failed: bool,
}

/// Нейтральный снимок сессии (D138, D149): собирает `za-agent` (или бинарник для
/// `catch_up`), отправляет порт.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionNote {
    /// Повод — константа `za_protocol::snapshot_trigger`.
    pub trigger: &'static str,
    /// Статус run.
    pub status: &'static str,
    pub summary: Option<String>,
    pub prompts: Vec<String>,
    pub files: Vec<String>,
    pub commands: Vec<CommandNote>,
    pub claimed: Vec<String>,
    /// Только непустой текст `## Next step` текущей сводки (D149).
    pub next_step: Option<String>,
    pub taken_at: Option<jiff::Timestamp>,
}

/// Итог отправки снимка сессии: отправлен ли, предупреждения сервера, поправленные
/// записи.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoteOutcome {
    pub sent: bool,
    pub warnings: Vec<String>,
    pub patched: Vec<String>,
}

/// Порт памяти для цикла (D138). Новые моменты обмена с памятью добавляются методами с
/// реализацией по умолчанию («недоступна» или «ничего»): прежние реализации не ломаются.
pub trait MemoryPort: Send + Sync {
    /// Уровень памяти; L0 — цикл пишет `memory.unavailable` на шаге 0 (D24).
    fn level(&self) -> MemoryLevel;

    /// Замороженный снимок для первого сообщения (MEM-031): `recall` события
    /// `session_start` в пределах `budget_tokens` ([`memory_budget`], MEM-060) и признак
    /// режима идей (D169).
    fn snapshot(&self, run: RunId, budget_tokens: u32) -> BoxFuture<'_, RecallReply>;

    /// CTX-013 перед компакцией: `next_step`, очередь файлов, снимок сессии.
    fn before_compaction<'a>(&'a self, run: RunId, next_step: &'a str) -> BoxFuture<'a, ()>;

    /// MEM-050: `recall` события `recovery` после компакции, не больше `budget_tokens`.
    fn after_compaction(&self, run: RunId, budget_tokens: u32) -> BoxFuture<'_, Recall>;

    /// Подсказка к вводу пользователя (D142, событие [`PROMPT_EVENT`]) в пределах
    /// `budget` и признак режима идей (D169).
    fn prompt_hint<'a>(
        &'a self,
        _run: RunId,
        _text: &'a str,
        _budget: u32,
    ) -> BoxFuture<'a, RecallReply> {
        Box::pin(async { Recall::Unavailable.into() })
    }

    /// Подсказка к ошибке инструмента `tool` (D142, событие [`POST_TOOL_EVENT`]).
    fn tool_error_hint<'a>(
        &'a self,
        _run: RunId,
        _tool: &'a str,
        _error: &'a str,
        _budget: u32,
    ) -> BoxFuture<'a, Recall> {
        Box::pin(async { Recall::Unavailable })
    }

    /// Отложенный повтор дайджеста шага 0 (D140, MEM-034).
    fn retry_digest(&self, _run: RunId, _budget: u32) -> BoxFuture<'_, Recall> {
        Box::pin(async { Recall::Unavailable })
    }

    /// Снимок сессии (D149).
    fn session_note(&self, _run: RunId, _note: SessionNote) -> BoxFuture<'_, NoteOutcome> {
        Box::pin(async { NoteOutcome::default() })
    }

    /// Изменённые файлы run (`files/changed`).
    fn files_changed(&self, _run: RunId, _paths: Vec<String>) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    /// Конец сессии run: `ended` — run завершён, а не на паузе (D139).
    fn end_session(&self, _run: RunId, _ended: bool) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// L0: память недоступна (N-16). Чтения — [`Recall::Unavailable`] (MEM-122), признака
/// режима идей нет; прочие методы — умолчания порта.
#[derive(Copy, Clone, Debug, Default)]
pub struct NoMemory;

impl MemoryPort for NoMemory {
    fn level(&self) -> MemoryLevel {
        MemoryLevel::L0
    }

    fn snapshot(&self, _run: RunId, _budget_tokens: u32) -> BoxFuture<'_, RecallReply> {
        Box::pin(async { Recall::Unavailable.into() })
    }

    fn before_compaction<'a>(&'a self, _run: RunId, _next_step: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn after_compaction(&self, _run: RunId, _budget_tokens: u32) -> BoxFuture<'_, Recall> {
        Box::pin(async { Recall::Unavailable })
    }
}

/// Бюджет снимка памяти по окну контекста (MEM-030), токены:
/// меньше 16384 — 200, меньше 32768 — 400, меньше 65536 — 800, иначе 1500.
pub fn memory_budget(n_ctx: u32) -> u32 {
    if n_ctx < 16_384 {
        200
    } else if n_ctx < 32_768 {
        400
    } else if n_ctx < 65_536 {
        800
    } else {
        1500
    }
}
