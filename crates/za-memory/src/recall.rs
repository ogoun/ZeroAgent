//! Запросы `POST /api/v1/recall` агента (R1-M D140, D141, D142, D168, D169) и разбор ответа.
//!
//! - **Без `output_limit`** ни у одного события (D141): укладку в бюджет делает клиент
//!   ([`crate::fit`]). Без `source`, `code_map` и `agent_block_version` (D140): так нет предела
//!   хука 7500 байт и напоминания о блоке инструкции ZAM, которого у агента нет.
//! - **Запас:** `budget_tokens = fetch_budget(b) = min(4 × b, 6000)` — сервер отбирает разделы в
//!   пределах `budget_tokens`, запас оставляет выбор клиенту; Rules сервер шлёт сверх него
//!   резервом. Принятый риск (ADR 0007): сервер отмечает показанным всё, что отдал.
//! - **События и сроки:** дайджест `session_start` — `timeout_ms: 1500`, его отложенный повтор —
//!   `2000`; `recovery` — `1500` (эпоху он начинает сам, `new_epoch` не нужен); подсказка к
//!   вводу `prompt` — первые 512 символов ввода после маски, `800`; подсказка к ошибке
//!   инструмента `post_tool` — первые 4 КиБ ошибки после маски и имя инструмента, `1000`.
//!   `timeout_ms` шлётся только при флаге `recall_timeout`; срок HTTP — `timeout_ms` + 1 с
//!   (D137).
//! - **Итог:** предупреждение `recall timeout` и текст после укладки — `Partial`, без текста —
//!   `Timeout`; иначе текст — `Hits`, пусто — `Empty`. Признак режима идей — элементы
//!   `items[]` с `reason: idea_review` (D169).

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::{Map, Value, json};
use za_core::memory::{POST_TOOL_EVENT, PROMPT_EVENT, Recall};
use za_secrets::SecretMasker;

use crate::discovery::hooks_off;
use crate::error::truncate;
use crate::fit::{Fitted, UNKNOWN_NAME, is_known_name};
use crate::ideas::REASON_IDEA_REVIEW;
use crate::session::RunBinding;

/// Путь `/recall`.
pub const RECALL_PATH: &str = "/api/v1/recall";
/// Событие дайджеста шага 0 и его повтора.
pub const SESSION_START_EVENT: &str = "session_start";
/// Событие пакета восстановления после компакции.
pub const RECOVERY_EVENT: &str = "recovery";
/// Флаг `features`: сервер принимает `timeout_ms` (M6 §16 п. 19, CR-7).
pub const FEATURE_RECALL_TIMEOUT: &str = "recall_timeout";
/// Предупреждение сервера: сбор не уложился в срок, отдано собранное.
pub const WARN_RECALL_TIMEOUT: &str = "recall timeout";

/// `timeout_ms` дайджеста `session_start` и пакета `recovery` (D140).
pub const DIGEST_TIMEOUT_MS: u64 = 1500;
/// `timeout_ms` отложенного повтора дайджеста (D140).
pub const RETRY_TIMEOUT_MS: u64 = 2000;
/// `timeout_ms` подсказки к вводу (D142).
pub const PROMPT_TIMEOUT_MS: u64 = 800;
/// `timeout_ms` подсказки к ошибке инструмента (D142).
pub const POST_TOOL_TIMEOUT_MS: u64 = 1000;
/// Запас срока HTTP сверх `timeout_ms` (D137).
pub const HTTP_SLACK: Duration = Duration::from_secs(1);

/// Символов ввода в `prompt` (D142; сервер ищет по первым 512).
pub const PROMPT_MAX_CHARS: usize = 512;
/// Байт ошибки в `error` (D142).
pub const ERROR_MAX_BYTES: usize = 4096;

/// Множитель запаса `budget_tokens` (D141).
const FETCH_FACTOR: u32 = 4;
/// Потолок `budget_tokens` запроса (D141).
const FETCH_MAX: u32 = 6000;

/// `budget_tokens` запроса для бюджета клиента `b`: `min(4 × b, 6000)` (D141).
pub fn fetch_budget(b: u32) -> u32 {
    b.saturating_mul(FETCH_FACTOR).min(FETCH_MAX)
}

/// Запрос `/recall`: событие, срок сбора и поля события.
#[derive(Debug, Clone)]
pub(crate) struct Ask {
    pub event: &'static str,
    pub timeout_ms: u64,
    fields: Map<String, Value>,
}

impl Ask {
    /// Дайджест `session_start` со сроком `timeout_ms` (шаг 0 или повтор).
    pub fn digest(timeout_ms: u64) -> Self {
        Self::new(SESSION_START_EVENT, timeout_ms)
    }

    /// Пакет восстановления после компакции.
    pub fn recovery() -> Self {
        Self::new(RECOVERY_EVENT, DIGEST_TIMEOUT_MS)
    }

    /// Подсказка к вводу: `prompt` — уже замаскированный и обрезанный ([`prompt_field`]).
    pub fn prompt(prompt: String) -> Self {
        let mut a = Self::new(PROMPT_EVENT, PROMPT_TIMEOUT_MS);
        a.fields.insert("prompt".into(), json!(prompt));
        a
    }

    /// Подсказка к ошибке инструмента `tool`: `error` — уже замаскированная и обрезанная
    /// ([`error_field`]).
    pub fn post_tool(tool: &str, error: String) -> Self {
        let mut a = Self::new(POST_TOOL_EVENT, POST_TOOL_TIMEOUT_MS);
        a.fields.insert("error".into(), json!(error));
        a.fields.insert("tool".into(), json!(tool));
        a
    }

    fn new(event: &'static str, timeout_ms: u64) -> Self {
        Self {
            event,
            timeout_ms,
            fields: Map::new(),
        }
    }

    /// Тело запроса сессии `session` с бюджетом клиента `budget`; `timeout_ms` — только если
    /// сервер его знает (`with_timeout`, флаг [`FEATURE_RECALL_TIMEOUT`]).
    pub fn body(
        &self,
        session: &str,
        binding: &RunBinding,
        budget: u32,
        with_timeout: bool,
    ) -> Value {
        let mut m = Map::new();
        m.insert("event".into(), json!(self.event));
        m.insert("session_id".into(), json!(session));
        if let Some(s) = &binding.solution {
            m.insert("solution".into(), json!(s));
        }
        if let Some(a) = &binding.area {
            m.insert("area".into(), json!(a));
        }
        m.insert("budget_tokens".into(), json!(fetch_budget(budget)));
        if with_timeout {
            m.insert("timeout_ms".into(), json!(self.timeout_ms));
        }
        m.extend(self.fields.clone());
        Value::Object(m)
    }

    /// Срок HTTP: `timeout_ms` + 1 с (D137).
    pub fn limit(&self) -> Duration {
        Duration::from_millis(self.timeout_ms) + HTTP_SLACK
    }
}

/// Поле `prompt`: маска, затем первые [`PROMPT_MAX_CHARS`] символов (маска раньше обрезки:
/// секрет на границе не уходит половиной).
pub(crate) fn prompt_field(masker: &SecretMasker, text: &str) -> String {
    let (masked, n) = masker.mask_counted(text);
    if n > 0 {
        tracing::debug!(
            masked = n,
            "память: в подсказке к вводу замаскированы секреты"
        );
    }
    truncate(&masked, PROMPT_MAX_CHARS)
}

/// Поле `error`: маска, затем первые [`ERROR_MAX_BYTES`] байт по границе символа.
pub(crate) fn error_field(masker: &SecretMasker, text: &str) -> String {
    let (masked, n) = masker.mask_counted(text);
    if n > 0 {
        tracing::debug!(
            masked = n,
            "память: в ошибке инструмента замаскированы секреты"
        );
    }
    let mut end = masked.len().min(ERROR_MAX_BYTES);
    while !masked.is_char_boundary(end) {
        end -= 1;
    }
    masked[..end].to_owned()
}

/// Разобранный ответ `/recall` (`{text, items, tokens, rules_tokens, timings, warnings,
/// hooks?}`, `api/recall.rs@v0.0.12-m6.5`); незнакомые поля пропускаются.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answer {
    pub text: String,
    /// Ссылки `items[]` с `reason: idea_review` (D169).
    pub review_refs: BTreeSet<String>,
    /// Счёт сервера своим токенизатором.
    pub tokens: Option<u32>,
    pub rules_tokens: u32,
    pub warnings: Vec<String>,
    /// Выключенные типы подсказок — только у `session_start` (`hooks`).
    pub hooks_off: Option<Vec<String>>,
}

impl Answer {
    /// Ответ из JSON; без строки `text` — `None` (не ответ ZAM).
    pub fn parse(v: &Value) -> Option<Self> {
        let text = v.get("text")?.as_str()?.to_owned();
        let review_refs = v
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|i| i.get("reason").and_then(Value::as_str) == Some(REASON_IDEA_REVIEW))
            .filter_map(|i| i.get("id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        let num = |k: &str| {
            v.get(k)
                .and_then(Value::as_u64)
                .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
        };
        let warnings = v
            .get("warnings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|w| truncate(w, 200))
            .collect();
        Some(Self {
            text,
            review_refs,
            tokens: num("tokens"),
            rules_tokens: num("rules_tokens").unwrap_or(0),
            warnings,
            hooks_off: v
                .get("hooks")
                .is_some_and(Value::is_object)
                .then(|| hooks_off(v)),
        })
    }

    /// Сервер не уложился в свой срок сбора.
    pub fn timed_out(&self) -> bool {
        self.warnings.iter().any(|w| w == WARN_RECALL_TIMEOUT)
    }
}

/// Итог по уложенному тексту (D168): `recall timeout` с текстом — `Partial`, без — `Timeout`.
pub(crate) fn outcome(fitted: &Fitted, timed_out: bool) -> Recall {
    match (fitted.text.trim().is_empty(), timed_out) {
        (true, true) => Recall::Timeout,
        (true, false) => Recall::Empty,
        (false, true) => Recall::Partial(fitted.text.clone()),
        (false, false) => Recall::Hits(fitted.text.clone()),
    }
}

/// Данные события `memory.recall` (D153, MEM-125) — расход памяти без текста: событие
/// `/recall`, итог, счёт выданного модели (после укладки), резерв Rules по счёту сервера,
/// снятые разделы, длительность, предупреждения сервера и выключенные типы подсказок
/// (из `session_start` этого run). Пишет их в журнал владелец порта (бинарник).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallReport {
    pub event: &'static str,
    /// `Recall::state()`: `hits`, `partial`, `empty`, `unavailable`, `timeout`.
    pub state: &'static str,
    pub tokens: u32,
    pub rules_tokens: u32,
    /// Имена снятых разделов; незнакомый раздел — `unknown` (его заголовок — текст памяти).
    pub omitted: Vec<String>,
    pub ms: u64,
    pub warnings: Vec<String>,
    pub hooks_off: Vec<String>,
}

impl RecallReport {
    /// Отчёт об ответе: снятые разделы без имён незнакомых.
    pub(crate) fn new(event: &'static str, state: &'static str, ms: u64) -> Self {
        Self {
            event,
            state,
            tokens: 0,
            rules_tokens: 0,
            omitted: Vec::new(),
            ms,
            warnings: Vec::new(),
            hooks_off: Vec::new(),
        }
    }

    /// С итогом укладки и ответом сервера.
    pub(crate) fn with_answer(mut self, fitted: &Fitted, answer: &Answer) -> Self {
        self.tokens = fitted.tokens;
        self.rules_tokens = answer.rules_tokens;
        self.omitted = fitted
            .omitted
            .iter()
            .map(|n| {
                if is_known_name(n) {
                    n.clone()
                } else {
                    UNKNOWN_NAME.to_owned()
                }
            })
            .collect();
        self.warnings = answer.warnings.clone();
        self
    }

    /// Значение события `memory.recall`.
    pub fn to_payload(&self) -> Value {
        json!({
            "event": self.event,
            "state": self.state,
            "tokens": self.tokens,
            "rules_tokens": self.rules_tokens,
            "omitted": self.omitted,
            "ms": self.ms,
            "warnings": self.warnings,
            "hooks_off": self.hooks_off,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> RunBinding {
        RunBinding {
            project_id: "0123456789abcdef".into(),
            solution: None,
            area: None,
            writable: false,
        }
    }

    #[test]
    fn body_without_binding_fields_and_timeout() {
        let b = Ask::recovery().body("za-x", &binding(), 200, false);
        assert_eq!(
            b,
            json!({"event": "recovery", "session_id": "za-x", "budget_tokens": 800})
        );
        assert_eq!(Ask::prompt("p".into()).limit(), Duration::from_millis(1800));
    }

    #[test]
    fn error_field_cuts_on_char_boundary() {
        let m = SecretMasker::default();
        let s = format!("a{}", "я".repeat(3000));
        let e = error_field(&m, &s);
        assert!(e.len() <= ERROR_MAX_BYTES && e.len() >= ERROR_MAX_BYTES - 1);
        assert_eq!(prompt_field(&m, &s).chars().count(), PROMPT_MAX_CHARS);
    }

    #[test]
    fn answer_parse_lenient() {
        let v = json!({
            "text": "t",
            "items": [{"id": "w:1", "reason": "idea_review"}, {"id": "k:2", "reason": "task"}],
            "tokens": 3,
            "warnings": ["recall timeout", 5],
            "zz": true,
        });
        let a = Answer::parse(&v).unwrap();
        assert_eq!(a.review_refs, ["w:1".to_owned()].into());
        assert_eq!(a.tokens, Some(3));
        assert!(a.timed_out());
        assert_eq!(a.hooks_off, None);
        assert!(Answer::parse(&json!({"items": []})).is_none());
    }
}
