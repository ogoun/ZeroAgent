//! Таксономия записи (R1-M D145; часть 04, §9.1; MEM-078; П8 и D172 изменений по ZAM
//! 0.0.12-m6.5): обязательные поля и формы по виду записи — до сети.
//!
//! Ошибка таксономии — ошибка инструмента до запроса: модель видит, какое поле исправить, а
//! сервер не получает заведомо негодное тело. Правила (строка на правило — тест
//! `taxonomy_table`, фикстуры `tests/fixtures/taxonomy/*.json`):
//!
//! - **`insight`:** `data.kind` — один из видов ZAM; триггеры — объекты `{type: file_glob |
//!   command_regex | error_regex | keyword, pattern, tools?}` (строка `file_glob:…` — ошибка
//!   формы); `data.evidence` — объект `{checked, method, result}`, у `certainty: theorem` —
//!   обязателен; эксперимент — с `data.experiment.result`; у решения поля ADR — только внутри
//!   `data.decision` (на верхнем уровне `data` — ошибка).
//! - **`term`:** `data.definition` от 1 до 300 символов; триггеров нет.
//! - **`changelog`:** `data.kind` — один из видов ZAM; **`release`** — `data.version`.
//! - **`task`:** поля сервера (`claimed_by`, `started_at`, `done_at`) — ошибка; `data.priority`
//!   (`low`/`normal`/`high`) — только при флаге `idea_review` (П8; на `0.0.11-m6` — 422).
//! - **`question`:** `data.options` — до 8.
//! - **`idea`:** поля, которые ведёт сервер (`review`, `reviews`, `returned_at`, `decided`,
//!   `prev_task_ids`, `task_id`, `question_id` и др.), — ошибка (П8); нет `data.origin` — шлюз
//!   ставит `agent` (своя мысль, D148); `origin: human` закрывает функция решения (D163).
//! - **`supersede` и `retract`** — только с `reason` не короче 10 символов (MEM-078).
//! - **Поиск:** `filters.status: returned` — только при флаге `idea_review` ([`check_filters`]).
//! - **Разбор идеи** (D172): исход из четырёх, `missing` только у `return` (и обязателен у него),
//!   `task` только у `plan`, без верхних `title` и `body`, ссылок `basis` и `similar` — до 50,
//!   `summary` — 200–8000 символов (сервер может требовать больше — тогда его 422), `missing` —
//!   до 2000.
//!
//! Нормализация (детерминированное тело, D145): теги без пробелов по краям, без пустых и
//! повторов, по возрастанию. Тексты ошибок уходят модели: английские, ASCII-пунктуация (D94).

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use crate::gate::WriteOp;
use crate::ideas::FEATURE_IDEA_REVIEW;

/// Виды записей журнала (`POST /spaces/{s}/entries`).
pub const KINDS: [&str; 8] = [
    "changelog",
    "insight",
    "release",
    "term",
    "task",
    "idea",
    "question",
    "report",
];
/// Виды `insight` (`data.kind`, `INSIGHT_KINDS`, `api/insights.rs@v0.0.12-m6.5`).
pub const INSIGHT_KINDS: [&str; 6] = [
    "gotcha",
    "failure",
    "discovery",
    "decision",
    "experiment",
    "pattern",
];
/// Виды `changelog` (`CHANGELOG_KINDS`, `api/entries.rs@v0.0.12-m6.5`).
pub const CHANGELOG_KINDS: [&str; 7] = [
    "feat", "fix", "refactor", "docs", "research", "ops", "other",
];
/// Итоги эксперимента.
pub const EXPERIMENT_RESULTS: [&str; 3] = ["confirmed", "refuted", "inconclusive"];
/// Типы триггеров (`NewTrigger` ZAM).
pub const TRIGGER_TYPES: [&str; 4] = ["file_glob", "command_regex", "error_regex", "keyword"];
/// Поля ADR решения — только внутри `data.decision` (`DECISION_KEYS` ZAM).
pub const DECISION_KEYS: [&str; 7] = [
    "context",
    "options",
    "chosen",
    "rationale",
    "consequences",
    "decided_by",
    "revisit_if",
];
/// Поля решения, которые на верхнем уровне `data` — ошибка (`DECISION_TOP_LEVEL` ZAM и
/// `decided_by`).
const DECISION_TOP_LEVEL: [&str; 6] = [
    "options",
    "chosen",
    "rationale",
    "consequences",
    "decided_by",
    "revisit_if",
];
/// Предел определения термина, символов (`DEFINITION_MAX_CHARS` ZAM).
pub const TERM_DEFINITION_MAX: usize = 300;
/// Ключи `data` термина.
const TERM_KEYS: [&str; 4] = ["aliases", "definition", "not_to_confuse", "code"];
/// Вариантов у вопроса.
pub const QUESTION_OPTIONS_MAX: usize = 8;
/// Вариантов у решения (`DECISION_OPTIONS_MAX` ZAM).
const DECISION_OPTIONS_MAX: usize = 8;
/// Приоритеты задачи и идеи.
pub const PRIORITIES: [&str; 3] = ["low", "normal", "high"];
/// Поля `data` идеи, которые ведёт сервер (`SERVER_KEYS`, `store/ideas.rs@v0.0.12-m6.5`).
pub const IDEA_SERVER_KEYS: [&str; 11] = [
    "task_id",
    "prev_task_ids",
    "question_id",
    "proposed_at",
    "decided",
    "adopted_at",
    "adopted_by",
    "done_at",
    "review",
    "reviews",
    "returned_at",
];
/// Поля `data` задачи, которые ведёт сервер (`store/tasks.rs@v0.0.12-m6.5`).
pub const TASK_SERVER_KEYS: [&str; 3] = ["claimed_by", "started_at", "done_at"];
/// Происхождения идеи.
const IDEA_ORIGINS: [&str; 2] = ["human", "agent"];
/// Статусы `certainty`.
pub const CERTAINTIES: [&str; 3] = ["axiom", "theorem", "hypothesis"];
/// Причина `supersede` и `retract` — не короче, символов (MEM-078).
pub const REASON_MIN_CHARS: usize = 10;
/// Статус идеи «на доработку» (M6.5): фильтр поиска — только при флаге `idea_review`.
pub const STATUS_RETURNED: &str = "returned";
/// Исходы разбора идеи (D172).
pub const REVIEW_OUTCOMES: [&str; 4] = ["plan", "return", "decline", "done"];
/// `summary` разбора, символов: нижняя граница — умолчание сервера `review_min_chars`.
pub const REVIEW_SUMMARY_MIN: usize = 200;
pub const REVIEW_SUMMARY_MAX: usize = 8000;
/// `missing` разбора, символов.
pub const REVIEW_MISSING_MAX: usize = 2000;
/// Ссылок в `basis` и `similar` разбора.
pub const REVIEW_REFS_MAX: usize = 50;
/// Срок захвата задачи, минут (`CLAIM_TTL_MAX_MIN` ZAM).
pub const CLAIM_TTL_MAX: u32 = 1440;
/// Операции PATCH заметки (`PatchOp` ZAM).
const PATCH_OPS: [&str; 5] = [
    "replace_section",
    "append_to_section",
    "insert_after_heading",
    "append",
    "prepend",
];
/// Решения по идее.
const IDEA_DECISIONS: [&str; 4] = ["accept", "defer", "decline", "reopen"];

/// Флаги сервера, от которых зависит таксономия.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    /// `idea_review` (`0.0.12-m6.5`): приоритет задачи, статус `returned`.
    pub idea_review: bool,
}

impl Flags {
    /// Флаги по `features` сервера.
    pub fn from_features(f: &BTreeSet<String>) -> Self {
        Self {
            idea_review: f.contains(FEATURE_IDEA_REVIEW),
        }
    }
}

/// Ошибка таксономии: текст модели.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TaxonomyError(pub String);

fn err<T>(s: impl Into<String>) -> Result<T, TaxonomyError> {
    Err(TaxonomyError(s.into()))
}

/// Тело записи после таксономии: нормализованное (теги, `data.origin` идеи) и пометки модели.
#[derive(Debug, Clone, PartialEq)]
pub struct Checked {
    pub body: Value,
    pub notes: Vec<String>,
}

/// Проверка и нормализация записи журнала (`memory_log`, `POST /spaces/{s}/entries`).
pub fn check_entry(v: &Value, flags: Flags) -> Result<Checked, TaxonomyError> {
    let Some(m) = v.as_object() else {
        return err("memory_log: arguments must be a JSON object");
    };
    let mut m = m.clone();
    let kind = match m.get("kind").and_then(Value::as_str).map(str::trim) {
        Some(k) if KINDS.contains(&k) => k.to_owned(),
        _ => return err(format!("kind: one of {}", KINDS.join(", "))),
    };
    check_title(&m)?;
    check_common(&m, &kind)?;
    let data = data_of(&m)?;
    match kind.as_str() {
        "insight" => check_insight(&m, &data)?,
        "term" => check_term(&data)?,
        "changelog" => {
            let k = data.get("kind").and_then(Value::as_str);
            if !k.is_some_and(|k| CHANGELOG_KINDS.contains(&k)) {
                return err(format!(
                    "data.kind: one of {} for a changelog",
                    CHANGELOG_KINDS.join(", ")
                ));
            }
        }
        "release" => {
            if !data
                .get("version")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
            {
                return err("data.version: required for a release");
            }
        }
        "task" => check_task_data(&data, flags)?,
        "question" => {
            if let Some(o) = data.get("options").filter(|v| !v.is_null()) {
                match o.as_array() {
                    Some(a) if a.len() <= QUESTION_OPTIONS_MAX => {}
                    Some(_) => {
                        return err(format!(
                            "data.options: at most {QUESTION_OPTIONS_MAX} options"
                        ));
                    }
                    None => return err("data.options: expected an array of {key, text}"),
                }
            }
        }
        "idea" => {
            check_idea_data(&data)?;
            if !data.contains_key("origin") {
                let mut d = data.clone();
                d.insert("origin".into(), json!("agent"));
                m.insert("data".into(), Value::Object(d));
            }
        }
        _ => {}
    }
    normalize_tags(&mut m);
    Ok(Checked {
        body: Value::Object(m),
        notes: Vec::new(),
    })
}

/// Проверка и нормализация операции записи на месте (`op` после неё — то, что идёт в функцию
/// решения); `Ok` — пометки модели.
pub fn check(op: &mut WriteOp, flags: Flags) -> Result<Vec<String>, TaxonomyError> {
    match op {
        WriteOp::Log(v) => {
            let c = check_entry(v, flags)?;
            *v = c.body;
            Ok(c.notes)
        }
        WriteOp::Supersede { body, .. } => {
            let Some(m) = body.as_object_mut() else {
                return err("supersede: arguments must be a JSON object");
            };
            check_reason(m.get("reason").and_then(Value::as_str), "supersede")?;
            check_title(m)?;
            let data = data_of(m)?;
            if data.contains_key("definition") {
                check_term(&data)?;
            } else {
                check_insight(m, &data)?;
            }
            check_triggers(m)?;
            check_certainty(m)?;
            normalize_tags(m);
            Ok(Vec::new())
        }
        WriteOp::Retract { reason, .. } => {
            check_reason(Some(reason), "retract")?;
            Ok(Vec::new())
        }
        WriteOp::Patch { body, .. } => {
            let Some(m) = body.as_object_mut() else {
                return err("memory_update_entry: arguments must be a JSON object");
            };
            if m.is_empty() {
                return err("memory_update_entry: nothing to change");
            }
            check_triggers(m)?;
            check_certainty(m)?;
            if let Some(d) = m.get("data").filter(|v| !v.is_null()) {
                let Some(d) = d.as_object() else {
                    return err("data: expected an object");
                };
                server_keys(d, &IDEA_SERVER_KEYS, "idea")?;
                server_keys(d, &TASK_SERVER_KEYS, "task")?;
                check_evidence(d)?;
            }
            if m.get("certainty").and_then(Value::as_str) == Some("theorem") {
                let has = m
                    .get("data")
                    .and_then(|d| d.get("evidence"))
                    .is_some_and(Value::is_object);
                if !has {
                    return err(
                        "certainty theorem needs data.evidence {checked: \"call:<call_id>\", method, result}",
                    );
                }
            }
            if m.contains_key("tags") {
                normalize_tags(m);
            }
            Ok(Vec::new())
        }
        WriteOp::NextStep { text, .. } => {
            if text.trim().is_empty() {
                return err("next_step: must not be empty");
            }
            Ok(Vec::new())
        }
        WriteOp::CachePut(v) => {
            check_cache(v)?;
            Ok(Vec::new())
        }
        WriteOp::Note { path, body } => {
            if crate::scope::split_path(path).1.is_empty() {
                return err("path: required");
            }
            let Some(m) = body.as_object() else {
                return err("memory_write: arguments must be a JSON object");
            };
            if !m.get("content").is_some_and(Value::is_string) {
                return err("content: required string");
            }
            check_certainty(m)?;
            if m.get("certainty").and_then(Value::as_str) == Some("theorem")
                && !m.get("evidence").is_some_and(evidence_shape)
            {
                return err(
                    "certainty theorem needs evidence {checked: \"call:<call_id>\", method, result}",
                );
            }
            Ok(Vec::new())
        }
        WriteOp::NodePatch { ops, .. } => {
            let Some(list) = ops.as_array().filter(|a| !a.is_empty()) else {
                return err("ops: a non-empty array");
            };
            for (i, o) in list.iter().enumerate() {
                let name = o.get("op").and_then(Value::as_str);
                if !name.is_some_and(|n| PATCH_OPS.contains(&n)) {
                    return err(format!("ops[{i}].op: one of {}", PATCH_OPS.join(", ")));
                }
                if !o.get("content").is_some_and(Value::is_string) {
                    return err(format!("ops[{i}].content: required string"));
                }
            }
            Ok(Vec::new())
        }
        WriteOp::Answer { body, .. } => {
            let has = |k: &str| {
                body.get(k)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty())
            };
            if !has("text") && !has("option") {
                return err("answer: text or option is required");
            }
            Ok(Vec::new())
        }
        WriteOp::Decide { body, .. } => {
            let d = body.get("decision").and_then(Value::as_str);
            if !d.is_some_and(|d| IDEA_DECISIONS.contains(&d)) {
                return err(format!("decision: one of {}", IDEA_DECISIONS.join(", ")));
            }
            Ok(Vec::new())
        }
        WriteOp::Review { body, .. } => {
            check_review(body)?;
            Ok(Vec::new())
        }
        WriteOp::Claim { ttl_min, .. } => {
            if ttl_min.is_some_and(|t| !(1..=CLAIM_TTL_MAX).contains(&t)) {
                return err(format!("ttl_min: from 1 to {CLAIM_TTL_MAX}"));
            }
            Ok(Vec::new())
        }
        WriteOp::Propose { .. } | WriteOp::Release { .. } => Ok(Vec::new()),
    }
}

/// Фильтры поиска (`memory_search`): статус `returned` — только при флаге `idea_review` (П8).
pub fn check_filters(filters: &Value, flags: Flags) -> Result<(), TaxonomyError> {
    if flags.idea_review {
        return Ok(());
    }
    let returned = match filters.get("status") {
        Some(Value::String(s)) => s.trim() == STATUS_RETURNED,
        Some(Value::Array(a)) => a
            .iter()
            .any(|s| s.as_str().map(str::trim) == Some(STATUS_RETURNED)),
        _ => false,
    };
    if returned {
        return err(
            "filters.status: returned needs a memory server with idea review (0.0.12-m6.5 or newer)",
        );
    }
    Ok(())
}

/// Заголовок записи — непустая строка.
fn check_title(m: &Map<String, Value>) -> Result<(), TaxonomyError> {
    match m.get("title").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => Ok(()),
        _ => err("title: required non-empty string"),
    }
}

/// Поля, общие для видов: `body` и `message` — строки, `tags` — массив строк, `force` —
/// булево, `certainty` — только у `insight` и `term`, триггеры — только у `insight`.
fn check_common(m: &Map<String, Value>, kind: &str) -> Result<(), TaxonomyError> {
    for k in ["body", "message", "area"] {
        if m.get(k).is_some_and(|v| !v.is_null() && !v.is_string()) {
            return err(format!("{k}: expected a string"));
        }
    }
    if let Some(t) = m.get("tags").filter(|v| !v.is_null()) {
        let ok = t.as_array().is_some_and(|a| a.iter().all(Value::is_string));
        if !ok {
            return err("tags: expected an array of strings");
        }
    }
    if m.get("force")
        .is_some_and(|v| !v.is_null() && !v.is_boolean())
    {
        return err("force: expected true or false");
    }
    if m.get("certainty").is_some_and(|v| !v.is_null()) && !matches!(kind, "insight" | "term") {
        return err(format!(
            "certainty: only an insight has certainty, not a {kind}"
        ));
    }
    check_certainty(m)?;
    if m.get("triggers").is_some_and(|v| !v.is_null()) && kind != "insight" {
        return err(format!(
            "triggers: only an insight has triggers, not a {kind}"
        ));
    }
    check_triggers(m)
}

/// `data` — объект (нет — пустой).
fn data_of(m: &Map<String, Value>) -> Result<Map<String, Value>, TaxonomyError> {
    match m.get("data") {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(o)) => Ok(o.clone()),
        Some(_) => err("data: expected an object"),
    }
}

/// `certainty` — один из статусов ZAM (`axiom` закрывает функция решения).
fn check_certainty(m: &Map<String, Value>) -> Result<(), TaxonomyError> {
    match m.get("certainty") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(c)) if CERTAINTIES.contains(&c.trim()) => Ok(()),
        Some(_) => err(format!("certainty: one of {}", CERTAINTIES.join(", "))),
    }
}

/// Триггеры — объекты `{type, pattern, tools?}`.
fn check_triggers(m: &Map<String, Value>) -> Result<(), TaxonomyError> {
    let Some(t) = m.get("triggers").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let Some(list) = t.as_array() else {
        return err("triggers: expected an array of {type, pattern, tools?}");
    };
    for (i, tr) in list.iter().enumerate() {
        let shape = "{type: file_glob|command_regex|error_regex|keyword, pattern, tools?}";
        let Some(o) = tr.as_object() else {
            return err(format!(
                "triggers[{i}]: expected an object {shape}, not a string"
            ));
        };
        if let Some(k) = o
            .keys()
            .find(|k| !["type", "pattern", "tools"].contains(&k.as_str()))
        {
            return err(format!(
                "triggers[{i}].{k}: unknown field; expected {shape}"
            ));
        }
        if !o
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| TRIGGER_TYPES.contains(&t))
        {
            return err(format!(
                "triggers[{i}].type: one of {}",
                TRIGGER_TYPES.join(", ")
            ));
        }
        if !o
            .get("pattern")
            .and_then(Value::as_str)
            .is_some_and(|p| !p.trim().is_empty())
        {
            return err(format!("triggers[{i}].pattern: required non-empty string"));
        }
        if let Some(tools) = o.get("tools").filter(|v| !v.is_null())
            && !tools
                .as_array()
                .is_some_and(|a| a.iter().all(Value::is_string))
        {
            return err(format!("triggers[{i}].tools: expected an array of strings"));
        }
    }
    Ok(())
}

/// `evidence` — объект `{checked, method, result}` с непустыми строками.
fn evidence_shape(v: &Value) -> bool {
    v.as_object().is_some_and(|o| {
        ["checked", "method", "result"].iter().all(|k| {
            o.get(*k)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
        })
    })
}

/// `data.evidence`, если задан, — объект `{checked, method, result}`.
fn check_evidence(data: &Map<String, Value>) -> Result<(), TaxonomyError> {
    match data.get("evidence") {
        None | Some(Value::Null) => Ok(()),
        Some(e) if evidence_shape(e) => Ok(()),
        Some(_) => {
            err("data.evidence: expected an object {checked: \"call:<call_id>\", method, result}")
        }
    }
}

/// `insight`: вид, эксперимент, доказательство, решение.
fn check_insight(m: &Map<String, Value>, data: &Map<String, Value>) -> Result<(), TaxonomyError> {
    let kind = data.get("kind").and_then(Value::as_str);
    if !kind.is_some_and(|k| INSIGHT_KINDS.contains(&k)) {
        return err(format!(
            "data.kind: one of {} for an insight",
            INSIGHT_KINDS.join(", ")
        ));
    }
    check_evidence(data)?;
    if m.get("certainty").and_then(Value::as_str) == Some("theorem")
        && !data.get("evidence").is_some_and(evidence_shape)
    {
        return err(
            "certainty theorem needs data.evidence {checked: \"call:<call_id>\", method, result}",
        );
    }
    match data.get("experiment") {
        None | Some(Value::Null) => {
            if kind == Some("experiment") {
                return err(format!(
                    "data.experiment.result: one of {} for an experiment",
                    EXPERIMENT_RESULTS.join(", ")
                ));
            }
        }
        Some(Value::Object(e)) => {
            let r = e.get("result").and_then(Value::as_str);
            let need = kind == Some("experiment") || e.get("result").is_some_and(|v| !v.is_null());
            if need && !r.is_some_and(|r| EXPERIMENT_RESULTS.contains(&r)) {
                return err(format!(
                    "data.experiment.result: one of {}",
                    EXPERIMENT_RESULTS.join(", ")
                ));
            }
        }
        Some(_) => return err("data.experiment: expected an object"),
    }
    if kind == Some("decision") {
        if let Some(k) = DECISION_TOP_LEVEL.iter().find(|k| data.contains_key(**k)) {
            return err(format!(
                "data.{k}: decision fields go inside data.decision {{options, chosen, rationale, decided_by, revisit_if, ...}}"
            ));
        }
        let Some(d) = data.get("decision").and_then(Value::as_object) else {
            return err("data.decision: required object for a decision");
        };
        if let Some(k) = d.keys().find(|k| !DECISION_KEYS.contains(&k.as_str())) {
            return err(format!(
                "data.decision.{k}: unknown field; allowed {}",
                DECISION_KEYS.join(", ")
            ));
        }
        if let Some(o) = d.get("options").filter(|v| !v.is_null()) {
            let ok = o
                .as_array()
                .is_some_and(|a| a.len() <= DECISION_OPTIONS_MAX && a.iter().all(Value::is_string));
            if !ok {
                return err(format!(
                    "data.decision.options: at most {DECISION_OPTIONS_MAX} strings"
                ));
            }
        }
        if let Some(b) = d.get("decided_by").filter(|v| !v.is_null())
            && !b.as_str().is_some_and(|b| IDEA_ORIGINS.contains(&b))
        {
            return err("data.decision.decided_by: human or agent");
        }
    }
    Ok(())
}

/// `term`: определение 1–300 символов, известные поля.
fn check_term(data: &Map<String, Value>) -> Result<(), TaxonomyError> {
    if let Some(k) = data.keys().find(|k| !TERM_KEYS.contains(&k.as_str())) {
        return err(format!(
            "data.{k}: unknown field for a term; allowed {}",
            TERM_KEYS.join(", ")
        ));
    }
    let n = data
        .get("definition")
        .and_then(Value::as_str)
        .map_or(0, |d| d.trim().chars().count());
    if !(1..=TERM_DEFINITION_MAX).contains(&n) {
        return err(format!(
            "data.definition: required, 1 to {TERM_DEFINITION_MAX} characters"
        ));
    }
    Ok(())
}

/// Поля, которые ведёт сервер, — ошибка.
fn server_keys(data: &Map<String, Value>, keys: &[&str], what: &str) -> Result<(), TaxonomyError> {
    match keys.iter().find(|k| data.contains_key(**k)) {
        Some(k) => err(format!(
            "data.{k}: kept by the memory server for a {what}; remove it"
        )),
        None => Ok(()),
    }
}

/// `data` задачи: поля сервера, приоритет (П8).
fn check_task_data(data: &Map<String, Value>, flags: Flags) -> Result<(), TaxonomyError> {
    server_keys(data, &TASK_SERVER_KEYS, "task")?;
    if let Some(p) = data.get("priority").filter(|v| !v.is_null()) {
        if !flags.idea_review {
            return err(
                "data.priority: a task priority needs a memory server with idea review (0.0.12-m6.5 or newer); remove it",
            );
        }
        if !p.as_str().is_some_and(|p| PRIORITIES.contains(&p.trim())) {
            return err(format!("data.priority: one of {}", PRIORITIES.join(", ")));
        }
    }
    Ok(())
}

/// `data` идеи: поля сервера (П8), происхождение, приоритет.
fn check_idea_data(data: &Map<String, Value>) -> Result<(), TaxonomyError> {
    server_keys(data, &IDEA_SERVER_KEYS, "idea")?;
    if let Some(o) = data.get("origin").filter(|v| !v.is_null())
        && !o.as_str().is_some_and(|o| IDEA_ORIGINS.contains(&o.trim()))
    {
        return err("data.origin: agent (your own idea) or human");
    }
    if let Some(p) = data.get("priority").filter(|v| !v.is_null())
        && !p.as_str().is_some_and(|p| PRIORITIES.contains(&p.trim()))
    {
        return err(format!("data.priority: one of {}", PRIORITIES.join(", ")));
    }
    Ok(())
}

/// Причина `supersede` и `retract` (MEM-078).
fn check_reason(reason: Option<&str>, what: &str) -> Result<(), TaxonomyError> {
    let n = reason.map_or(0, |r| r.trim().chars().count());
    if n < REASON_MIN_CHARS {
        return err(format!(
            "{what}: reason is required, at least {REASON_MIN_CHARS} characters (why the old record is outdated)"
        ));
    }
    Ok(())
}

/// Выжимка: `query`, `summary`, хотя бы один источник с `id`.
fn check_cache(v: &Value) -> Result<(), TaxonomyError> {
    let text = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    if !text("query") {
        return err("query: required non-empty string");
    }
    if !text("summary") {
        return err("summary: required non-empty string");
    }
    let ok = v.get("sources").and_then(Value::as_array).is_some_and(|a| {
        !a.is_empty()
            && a.iter().all(|s| {
                s.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.trim().is_empty())
            })
    });
    if !ok {
        return err("sources: at least one {id, rev?, section?}");
    }
    Ok(())
}

/// Разбор идеи (D172): локальные проверки до сети.
fn check_review(body: &Value) -> Result<(), TaxonomyError> {
    let Some(m) = body.as_object() else {
        return err("review: arguments must be a JSON object");
    };
    let given = |k: &str| match m.get(k) {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(_) => true,
    };
    if given("title") || given("body") {
        return err("review: the plan goes to task.body and the task title to task.title");
    }
    let outcome = m.get("outcome").and_then(Value::as_str).map(str::trim);
    let Some(outcome) = outcome.filter(|o| REVIEW_OUTCOMES.contains(o)) else {
        return err(format!("outcome: one of {}", REVIEW_OUTCOMES.join(", ")));
    };
    let chars = |k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .map_or(0, |s| s.trim().chars().count())
    };
    let summary = chars("summary");
    if !(REVIEW_SUMMARY_MIN..=REVIEW_SUMMARY_MAX).contains(&summary) {
        return err(format!(
            "summary: {REVIEW_SUMMARY_MIN} to {REVIEW_SUMMARY_MAX} characters"
        ));
    }
    if given("missing") {
        if outcome != "return" {
            return err("missing: only with outcome return");
        }
        if chars("missing") > REVIEW_MISSING_MAX {
            return err(format!("missing: at most {REVIEW_MISSING_MAX} characters"));
        }
    } else if outcome == "return" {
        return err("missing: required with outcome return");
    }
    if given("task") && outcome != "plan" {
        return err("task: only with outcome plan");
    }
    for k in ["basis", "similar"] {
        if let Some(v) = m.get(k).filter(|v| !v.is_null()) {
            let ok = v
                .as_array()
                .is_some_and(|a| a.len() <= REVIEW_REFS_MAX && a.iter().all(Value::is_string));
            if !ok {
                return err(format!("{k}: at most {REVIEW_REFS_MAX} references"));
            }
        }
    }
    Ok(())
}

/// Теги без пробелов по краям, без пустых и повторов, по возрастанию.
fn normalize_tags(m: &mut Map<String, Value>) {
    let Some(Value::Array(a)) = m.get("tags") else {
        return;
    };
    let mut tags: Vec<String> = a
        .iter()
        .filter_map(Value::as_str)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    tags.sort();
    tags.dedup();
    m.insert("tags".into(), json!(tags));
}
