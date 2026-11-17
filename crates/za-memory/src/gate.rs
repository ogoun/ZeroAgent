//! Шлюз записи — одна функция решения по всем путям (R1-M D163, D145; D170 и П6 изменений по
//! ZAM 0.0.12-m6.5).
//!
//! [`decide`] — чистая функция: через неё идёт каждая запись модели и рантайма (записи журнала,
//! заметки — создание, замена, PATCH, — `supersede`, `retract`, `cache_put`, `propose`,
//! `decide`, `answer`, `review`, `claim`, `release`, `next_step`). Она же строит тело запроса
//! REST: оно — функция аргументов вызова и состояния run, без времени, теги отсортированы
//! (повтор с тем же ключом даёт то же тело, иначе ZAM ответил бы 422 `idempotency-mismatch`).
//! Шлюз ([`crate::gateway`]) до неё читает доверие цели (один запрос на вызов) и проверяет
//! `evidence` по журналу run; маску он накладывает после неё.
//!
//! **Правила** (строка на правило — тест `gate_table`):
//! 1. **`cache_put`:** из tainted-контекста — отказ; любое поле `promote` — отказ.
//! 2. **Frontmatter заметок:** строки `certainty:`, `evidence:`, `verified_at:`,
//!    `verified_at_commit:` из текста модели удаляются с пометкой; `certainty` заметки ставит
//!    только шлюз — из аргументов `certainty` и `evidence`. PATCH-операции, задевающие
//!    frontmatter, — отказ. `relations:` из tainted-контекста снимаются с пометкой.
//! 3. **Доверенные цели:** правка (`append`, PATCH, PUT заметки), `supersede`, `retract` записи
//!    или заметки с `certainty` ≥ `theorem` из tainted-контекста, повышение `certainty` и
//!    разбор идеи (D170) из tainted-контекста — отказ [`TAINTED`] (inbox подтверждения — R1-D).
//! 4. **Подделка человека:** `idea origin: human`, `answer by: human`, `decide by: human`,
//!    `by: human` у правки (П6), `data.decision.decided_by: human`, `certainty: axiom` — отказ
//!    [`HUMAN_ONLY`]; `answer` и `decide` получают `by: agent` (у токена `by` обязателен).
//!    `by: agent` у правки шлюз не шлёт — это умолчание сервера.
//! 5. **Теги** `src:*` и `tainted` от модели снимаются, шлюз ставит свои: у `insight` и
//!    `changelog` — `data.origin {session, run, source_kind, tainted}`; у прочих видов — тег
//!    `src:<source_kind>` и поле `message`; в tainted-контексте — тег `tainted` у всех.
//! 6. **`theorem`:** `evidence.checked` — `call:<call_id>` вызова этого run, завершённого без
//!    ошибки (`evidence_ok`); иначе понижение до `hypothesis` с пометкой. В tainted-контексте —
//!    только `hypothesis`, триггеры снимаются с пометкой.
//! 7. **Область:** запись в `rules`, недоверенный проект, пространство вне [`ScopeGuard`] — отказ;
//!    без решения — запись только с `scope: "global"` (в Knowledge), `changelog`, `release` и
//!    `report` — отказ [`NEEDS_SOLUTION`] (D135).
//!
//! **Отмывание** (гипотеза из tainted-run, прочитанная в чистом run, становится решением) —
//! принятый риск до CR-19 (ADR 0007). Тексты отказов и пометок уходят модели: английские,
//! ASCII-пунктуация (D94); список — [`text_samples`].

use std::time::Instant;

use serde_json::{Map, Value, json};
use za_protocol::{EffectKey, RunId, taint_origin};

use crate::scope::{KNOWLEDGE, RULES, ScopeAccess, ScopeGuard, split_path};

/// Отказ: подделка решения человека (D145, D163 правило 4).
pub const HUMAN_ONLY: &str = "axiom and by: human need the user's explicit instruction";
/// Отказ: правка доверенной записи из tainted-контекста (D163 правило 3, D170).
pub const TAINTED: &str = "needs human confirmation (tainted context)";
/// Отказ: `changelog`, `release`, `report` без решения (D135).
pub const NEEDS_SOLUTION: &str = "needs a memory solution; ask the user to run zero-agent memory init (changelog, release and report are kept only in the solution space)";
/// Отказ: запись без решения без явного `scope: "global"` (D135).
pub const NO_SOLUTION: &str = "this project has no memory solution: set scope \"global\" to write to personal knowledge, or ask the user to run zero-agent memory init";
/// Отказ: запись в Rules (D163 правило 7).
pub const RULES_READ_ONLY: &str = "memory space rules is read-only for the agent";
/// Отказ: выжимка из tainted-контекста (D163 правило 1).
pub const CACHE_TAINTED: &str =
    "cache summaries are not written from a tainted context; needs human confirmation";
/// Отказ: `promote` выжимки (D163 правило 1).
pub const PROMOTE: &str =
    "promote is not available: write a note or an insight with the summary instead";
/// Отказ: PATCH-операция задевает frontmatter заметки (D163 правило 2).
pub const FRONTMATTER_PATCH: &str = "patch ops must not touch the note frontmatter; the note certainty comes from the certainty and evidence arguments";
/// Пометка: сняты теги происхождения модели (D163 правило 5).
pub const NOTE_PROVENANCE: &str = "tags src:* and tainted are set by the runtime; removed";
/// Пометка: tainted-контекст — только `hypothesis` (D145).
pub const NOTE_TAINTED_HYPOTHESIS: &str = "tainted context: certainty set to hypothesis";
/// Пометка: tainted-контекст — триггеры сняты (D145).
pub const NOTE_TAINTED_TRIGGERS: &str = "tainted context: triggers removed";
/// Пометка: `theorem` без проверенного вызова этого run (D163 правило 6).
pub const NOTE_EVIDENCE: &str = "certainty lowered to hypothesis: evidence.checked must be call:<call_id> of a call of this run that finished without error";
/// Пометка: строки статуса во frontmatter заметки сняты (D163 правило 2).
pub const NOTE_FRONTMATTER: &str = "frontmatter lines certainty, evidence, verified_at and verified_at_commit removed: the note certainty comes from the certainty and evidence arguments";
/// Пометка: `relations:` во frontmatter из tainted-контекста сняты (D163 правило 2).
pub const NOTE_TAINTED_RELATIONS: &str = "tainted context: frontmatter relations removed";

/// Виды записей журнала, которые ZAM ведёт только в пространстве решения (D135).
const SOLUTION_ONLY: [&str; 3] = ["changelog", "release", "report"];
/// Виды с происхождением в `data.origin` (MEM-076): у прочих схемы `data` строгие.
const ORIGIN_IN_DATA: [&str; 2] = ["insight", "changelog"];
/// Ключи статуса во frontmatter заметки, которые ставит только шлюз (D163 правило 2).
const STATUS_KEYS: [&str; 4] = ["certainty", "evidence", "verified_at", "verified_at_commit"];
/// Ключ связей графа во frontmatter (снимается в tainted-контексте).
const RELATIONS_KEY: &str = "relations";
/// Префикс ссылки на вызов в `evidence.checked` (D145).
pub const CALL_PREFIX: &str = "call:";
/// Тег недоверенного происхождения (MEM-076, MEM-077).
pub const TAG_TAINTED: &str = "tainted";
/// Префикс тегов происхождения (MEM-076).
pub const TAG_SRC: &str = "src:";
/// Имя агента у захвата задачи.
const AGENT: &str = "zero-agent";

/// Контекст вызова записи: кто пишет и в каком состоянии run.
#[derive(Debug, Clone)]
pub struct WriteCx {
    pub run: RunId,
    /// Сессия run `za-<project_id>-<run>` (`X-Zam-Session`).
    pub session: String,
    /// Ключ эффекта вызова: он же `Idempotency-Key` (D145).
    pub key: EffectKey,
    /// Tainted-контекст (`ToolCx.facts.untrusted`, D162): то же определение, что
    /// `Exposure.untrusted` R1-C2.
    pub untrusted: bool,
    /// Метки происхождения run (`taint_origin`).
    pub origins: Vec<String>,
    /// Область (`X-Zam-Area`); нет — область привязки run.
    pub area: Option<String>,
    /// Дедлайн вызова (`ToolCx.deadline`); нет — тайм-аут инструмента от начала записи.
    pub deadline: Option<Instant>,
    /// Модель явно попросила Knowledge (`scope: "global"`, D135).
    pub scope_global: bool,
}

/// Операция записи. Тела — аргументы вызова в форме REST ZAM: шлюз берёт из них только
/// известные поля и ставит свои (происхождение, `by`, `certainty` заметки).
#[derive(Debug, Clone, PartialEq)]
pub enum WriteOp {
    /// `POST /spaces/{s}/entries`: тело записи журнала; `space` — пространство (нет — решение
    /// run или Knowledge при `scope_global`).
    Log(Value),
    /// `POST /nodes` или (с `base_rev`) `PUT /nodes/{id}`: путь `[<пространство>:/]<путь>`,
    /// тело `{content, l0?, message?, certainty?, evidence?, base_rev?}`.
    Note { path: String, body: Value },
    /// `PATCH /nodes/{id} {base_rev, ops}`.
    NodePatch { id: String, rev: i64, ops: Value },
    /// `PATCH /entries/{id}` с `If-Match: <rev>`.
    Patch { id: String, rev: i64, body: Value },
    /// `POST /entries/{id}/claim`.
    Claim { id: String, ttl_min: Option<u32> },
    /// `POST /entries/{id}/release`.
    Release { id: String },
    /// `POST /entries/{id}/supersede`: тело новой записи и `reason` (MEM-078).
    Supersede { id: String, body: Value },
    /// `POST /entries/{id}/retract {reason}`.
    Retract { id: String, reason: String },
    /// `POST /cache`.
    CachePut(Value),
    /// `POST /entries/{id}/answer`.
    Answer { id: String, body: Value },
    /// `POST /entries/{id}/propose`.
    Propose { id: String, body: Value },
    /// `POST /entries/{id}/decide`.
    Decide { id: String, body: Value },
    /// `POST /entries/{id}/review` (не через очередь, D171).
    Review { id: String, body: Value },
    /// `PATCH /entries/{id} {next_step}` — рантайм (D149).
    NextStep { id: String, text: String },
}

impl WriteOp {
    /// Машинное имя операции (поле `op` события `memory.write`, D153).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Log(_) => "log",
            Self::Note { .. } => "note",
            Self::NodePatch { .. } => "node_patch",
            Self::Patch { .. } => "patch",
            Self::Claim { .. } => "claim",
            Self::Release { .. } => "release",
            Self::Supersede { .. } => "supersede",
            Self::Retract { .. } => "retract",
            Self::CachePut(_) => "cache_put",
            Self::Answer { .. } => "answer",
            Self::Propose { .. } => "propose",
            Self::Decide { .. } => "decide",
            Self::Review { .. } => "review",
            Self::NextStep { .. } => "next_step",
        }
    }

    /// Id цели операции над существующей записью или заметкой.
    pub fn target_id(&self) -> Option<&str> {
        match self {
            Self::Log(_) | Self::Note { .. } | Self::CachePut(_) => None,
            Self::NodePatch { id, .. }
            | Self::Patch { id, .. }
            | Self::Claim { id, .. }
            | Self::Release { id }
            | Self::Supersede { id, .. }
            | Self::Retract { id, .. }
            | Self::Answer { id, .. }
            | Self::Propose { id, .. }
            | Self::Decide { id, .. }
            | Self::Review { id, .. }
            | Self::NextStep { id, .. } => Some(id),
        }
    }

    /// Вид записи журнала (`kind` тела `Log`).
    pub fn kind(&self) -> Option<&str> {
        match self {
            Self::Log(v) => v.get("kind").and_then(Value::as_str),
            _ => None,
        }
    }
}

/// Доверие цели правки по ответу `GET /entries/{id}` или `GET /nodes/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetTrust {
    /// `certainty` записи или заметки (`axiom`, `theorem`, `hypothesis`; нет — `None`).
    pub certainty: Option<String>,
    /// Пространство цели (`space_id` ответа).
    pub space: Option<String>,
    /// Вид записи журнала (`insight`, `term`, …); у заметки — `None`.
    pub kind: Option<String>,
    /// Теги происхождения цели (`src:*`, `tainted`): правка тегов их не теряет.
    pub tags: Vec<String>,
}

impl TargetTrust {
    /// Цель доверенная: `certainty` ≥ `theorem`.
    pub fn trusted(&self) -> bool {
        matches!(self.certainty.as_deref(), Some("theorem" | "axiom"))
    }
}

/// Решение шлюза.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Запись как есть (тело — после правил: происхождение, `by`, отсортированные теги).
    Allow(Value),
    /// Запись с понижением или снятыми полями; `notes` — пометки модели.
    Downgrade { body: Value, notes: Vec<String> },
    /// Отказ до сети; текст — модели.
    Refuse(String),
}

impl Verdict {
    /// Тело и пометки разрешённой записи; `Err` — текст отказа.
    pub fn into_parts(self) -> Result<(Value, Vec<String>), String> {
        match self {
            Self::Allow(b) => Ok((b, Vec::new())),
            Self::Downgrade { body, notes } => Ok((body, notes)),
            Self::Refuse(r) => Err(r),
        }
    }
}

/// Вид источника для `data.origin.source_kind` и тега `src:` (словарь CR-19: `agent`, `web`,
/// `tool`, `file`): чистый контекст — `agent`; иначе — самое опасное происхождение run.
pub fn source_kind(cx: &WriteCx) -> &'static str {
    if !cx.untrusted {
        return "agent";
    }
    let has = |o: &str| cx.origins.iter().any(|x| x == o);
    if has(taint_origin::WEB) {
        "web"
    } else if has(taint_origin::NETWORK) || has(taint_origin::TOOL) {
        "tool"
    } else if has(taint_origin::OUTSIDE_PROJECT) || has(taint_origin::UNTRUSTED_PROJECT) {
        "file"
    } else {
        "tool"
    }
}

/// Происхождение записи (MEM-076) без времени: повтор даёт то же тело (D145).
pub fn origin_value(cx: &WriteCx) -> Value {
    json!({
        "session": cx.session,
        "run": cx.run.to_string(),
        "source_kind": source_kind(cx),
        "tainted": cx.untrusted,
    })
}

/// Строка происхождения в поле `message` у видов без `data.origin` (MEM-076).
fn origin_message(cx: &WriteCx) -> String {
    let tainted = if cx.untrusted { ", tainted" } else { "" };
    format!(
        "zero-agent: run {}, session {}, source {}{tainted}",
        cx.run,
        cx.session,
        source_kind(cx)
    )
}

/// Решение о записи `op` (D163). `target` — доверие цели (шлюз читает его до вызова, у
/// операций над существующей целью), `guard` — область run (D164), `evidence_ok` — в журнале
/// run есть вызов из `evidence.checked`, завершённый без ошибки.
pub fn decide(
    op: &WriteOp,
    target: Option<&TargetTrust>,
    cx: &WriteCx,
    guard: &ScopeGuard,
    evidence_ok: bool,
) -> Verdict {
    if !guard.writable() {
        return Verdict::Refuse(crate::scope::READ_ONLY.to_owned());
    }
    let mut notes = Vec::new();
    let trusted_target = target.is_some_and(TargetTrust::trusted);
    let body = match op {
        WriteOp::Log(v) => log(v, cx, guard, evidence_ok, &mut notes),
        WriteOp::Note { path, body } => {
            if cx.untrusted && trusted_target {
                Err(TAINTED.to_owned())
            } else {
                note(path, body, cx, guard, evidence_ok, &mut notes)
            }
        }
        WriteOp::NodePatch { rev, ops, .. } => {
            if cx.untrusted && trusted_target {
                Err(TAINTED.to_owned())
            } else {
                node_patch(*rev, ops)
            }
        }
        WriteOp::Patch { body, .. } => patch(body, target, cx, evidence_ok, &mut notes),
        WriteOp::Claim { ttl_min, .. } => {
            let mut m = Map::new();
            if let Some(t) = ttl_min {
                m.insert("ttl_min".into(), json!(t));
            }
            m.insert("agent".into(), json!(AGENT));
            Ok(Value::Object(m))
        }
        WriteOp::Release { .. } => Ok(json!({})),
        WriteOp::Supersede { body, .. } => supersede(body, target, cx, evidence_ok, &mut notes),
        WriteOp::Retract { reason, .. } => {
            if cx.untrusted && trusted_target {
                Err(TAINTED.to_owned())
            } else {
                Ok(json!({"reason": reason.trim()}))
            }
        }
        WriteOp::CachePut(v) => cache_put(v, cx, guard),
        WriteOp::Answer { body, .. } => by_agent(body, true, guard),
        WriteOp::Decide { body, .. } => by_agent(body, true, guard),
        WriteOp::Propose { body, .. } => by_agent(body, false, guard),
        WriteOp::Review { body, .. } => review(body, cx, guard),
        WriteOp::NextStep { text, .. } => Ok(json!({"next_step": text.trim()})),
    };
    match body {
        Err(reason) => Verdict::Refuse(reason),
        Ok(body) if notes.is_empty() => Verdict::Allow(body),
        Ok(body) => Verdict::Downgrade { body, notes },
    }
}

/// Тело — объект JSON.
fn object(v: &Value, what: &str) -> Result<Map<String, Value>, String> {
    v.as_object()
        .cloned()
        .ok_or_else(|| format!("{what}: arguments must be a JSON object"))
}

fn str_field<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str).map(str::trim)
}

/// Подделка слов человека (правило 4, П6): `certainty: axiom`, `by: human`, `data.origin:
/// human` (идея), `data.decision.decided_by: human`.
fn human_forgery(m: &Map<String, Value>) -> bool {
    let data = m.get("data");
    str_field(m, "certainty") == Some("axiom")
        || str_field(m, "by") == Some("human")
        || data
            .and_then(|d| d.get("origin"))
            .and_then(Value::as_str)
            .map(str::trim)
            == Some("human")
        || data
            .and_then(|d| d.get("decision"))
            .and_then(|d| d.get("decided_by"))
            .and_then(Value::as_str)
            .map(str::trim)
            == Some("human")
}

/// Тег происхождения (`src:*`, `tainted`) — их ставит только шлюз.
fn is_provenance_tag(t: &str) -> bool {
    let t = t.trim().to_ascii_lowercase();
    t == TAG_TAINTED || t.starts_with(TAG_SRC)
}

/// Снимает теги происхождения модели (правило 5) с пометкой [`NOTE_PROVENANCE`].
fn strip_provenance(m: &mut Map<String, Value>, notes: &mut Vec<String>) {
    let Some(Value::Array(tags)) = m.get_mut("tags") else {
        return;
    };
    let before = tags.len();
    tags.retain(|t| !t.as_str().is_some_and(is_provenance_tag));
    if tags.len() != before {
        note_once(notes, NOTE_PROVENANCE);
    }
}

/// Добавляет теги и упорядочивает набор: строки без пробелов по краям, без пустых и повторов,
/// по возрастанию (детерминированное тело, D145).
fn put_tags(m: &mut Map<String, Value>, extra: &[String]) {
    let mut tags: Vec<String> = match m.get("tags") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_owned())
            .collect(),
        _ => Vec::new(),
    };
    tags.extend(extra.iter().cloned());
    tags.retain(|t| !t.is_empty());
    tags.sort();
    tags.dedup();
    if tags.is_empty() {
        m.remove("tags");
    } else {
        m.insert("tags".into(), json!(tags));
    }
}

fn note_once(notes: &mut Vec<String>, n: &str) {
    if !notes.iter().any(|x| x == n) {
        notes.push(n.to_owned());
    }
}

/// Значение аргумента `space`: строка или нет его (`null` — нет).
fn space_arg(v: Option<Value>) -> Result<Option<String>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err("memory scope: space must be a string".to_owned()),
    }
}

/// Пространство записи (правило 7, D135): явное (`space` тела) или по контексту — Knowledge
/// при `scope_global`, иначе решение run.
fn resolve_space(
    explicit: Option<&str>,
    scope_global: bool,
    guard: &ScopeGuard,
    kind: Option<&str>,
) -> Result<String, String> {
    let solution_only = kind.is_some_and(|k| SOLUTION_ONLY.contains(&k));
    let space = match explicit.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_owned(),
        None if scope_global => KNOWLEDGE.to_owned(),
        None => match guard.solution() {
            Some(g) => g.to_owned(),
            None if solution_only => return Err(NEEDS_SOLUTION.to_owned()),
            None => return Err(NO_SOLUTION.to_owned()),
        },
    };
    if space == RULES {
        return Err(RULES_READ_ONLY.to_owned());
    }
    guard.check_space(&space, ScopeAccess::Write)?;
    if solution_only && space == KNOWLEDGE {
        return Err(NEEDS_SOLUTION.to_owned());
    }
    Ok(space)
}

/// `certainty` записи после правил 3 и 6: tainted-контекст — `hypothesis`, триггеры снимаются;
/// `theorem` без проверенного вызова — `hypothesis`.
fn settle_certainty(
    m: &mut Map<String, Value>,
    cx: &WriteCx,
    evidence_ok: bool,
    notes: &mut Vec<String>,
) {
    let asked = str_field(m, "certainty").map(str::to_owned);
    if cx.untrusted {
        if asked.as_deref().is_some_and(|c| c != "hypothesis") {
            note_once(notes, NOTE_TAINTED_HYPOTHESIS);
        }
        m.insert("certainty".into(), json!("hypothesis"));
        if m.get("triggers")
            .is_some_and(|t| t.as_array().is_some_and(|a| !a.is_empty()))
        {
            note_once(notes, NOTE_TAINTED_TRIGGERS);
        }
        m.remove("triggers");
        return;
    }
    match asked.as_deref() {
        Some("theorem") if !evidence_ok => {
            m.insert("certainty".into(), json!("hypothesis"));
            note_once(notes, NOTE_EVIDENCE);
        }
        Some(_) => {}
        None => {
            m.insert("certainty".into(), json!("hypothesis"));
        }
    }
}

/// Происхождение (правило 5): `data.origin` у `insight`/`changelog`, у прочих — тег
/// `src:<kind>` и `message`; tainted — тег `tainted` у всех. `kept` — теги происхождения
/// цели правки.
fn mark_origin(m: &mut Map<String, Value>, kind: &str, cx: &WriteCx, kept: &[String]) {
    let mut extra: Vec<String> = kept.to_vec();
    if ORIGIN_IN_DATA.contains(&kind) {
        let mut data = m
            .get("data")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        data.insert("origin".into(), origin_value(cx));
        m.insert("data".into(), Value::Object(data));
    } else {
        extra.push(format!("{TAG_SRC}{}", source_kind(cx)));
        let prov = origin_message(cx);
        let msg = match str_field(m, "message").filter(|s| !s.is_empty()) {
            Some(own) => format!("{own}; {prov}"),
            None => prov,
        };
        m.insert("message".into(), json!(msg));
    }
    if cx.untrusted {
        extra.push(TAG_TAINTED.to_owned());
    }
    put_tags(m, &extra);
}

/// Запись журнала (`POST /spaces/{s}/entries`): итоговое тело с полем `space` — его шлюз
/// переносит в путь.
fn log(
    v: &Value,
    cx: &WriteCx,
    guard: &ScopeGuard,
    evidence_ok: bool,
    notes: &mut Vec<String>,
) -> Result<Value, String> {
    let mut m = object(v, "memory_log")?;
    let kind = str_field(&m, "kind").unwrap_or("").to_owned();
    let scope_global = cx.scope_global || str_field(&m, "scope") == Some("global");
    m.remove("scope");
    let explicit = space_arg(m.remove("space"))?;
    let space = resolve_space(explicit.as_deref(), scope_global, guard, Some(&kind))?;
    if human_forgery(&m) {
        return Err(HUMAN_ONLY.to_owned());
    }
    strip_provenance(&mut m, notes);
    if kind == "insight" || (kind == "term" && m.contains_key("certainty")) {
        settle_certainty(&mut m, cx, evidence_ok, notes);
    } else if cx.untrusted && m.contains_key("triggers") {
        m.remove("triggers");
        note_once(notes, NOTE_TAINTED_TRIGGERS);
    }
    mark_origin(&mut m, &kind, cx, &[]);
    m.insert("space".into(), json!(space));
    Ok(Value::Object(m))
}

/// Заметка (`POST /nodes` или `PUT /nodes/{id}`): итоговое тело с `space`, `path`, `content`.
fn note(
    path: &str,
    body: &Value,
    cx: &WriteCx,
    guard: &ScopeGuard,
    evidence_ok: bool,
    notes: &mut Vec<String>,
) -> Result<Value, String> {
    let mut m = object(body, "memory_write")?;
    let (in_path, rel) = split_path(path);
    let in_body = space_arg(m.remove("space"))?;
    let explicit = in_path.map(str::to_owned).or(in_body);
    let space = resolve_space(explicit.as_deref(), cx.scope_global, guard, None)?;
    if human_forgery(&m) {
        return Err(HUMAN_ONLY.to_owned());
    }
    let content = str_field_raw(&m, "content").unwrap_or("").to_owned();
    let (mut content, stripped) = strip_frontmatter(&content, cx.untrusted);
    if stripped.status {
        note_once(notes, NOTE_FRONTMATTER);
    }
    if stripped.relations {
        note_once(notes, NOTE_TAINTED_RELATIONS);
    }
    let asked = m
        .remove("certainty")
        .and_then(|c| c.as_str().map(|s| s.trim().to_owned()));
    let evidence = m.remove("evidence");
    let certainty = if cx.untrusted {
        if asked.as_deref().is_some_and(|c| c != "hypothesis") {
            note_once(notes, NOTE_TAINTED_HYPOTHESIS);
        }
        Some("hypothesis".to_owned())
    } else if asked.as_deref() == Some("theorem") && !evidence_ok {
        note_once(notes, NOTE_EVIDENCE);
        Some("hypothesis".to_owned())
    } else {
        asked
    };
    if let Some(c) = &certainty {
        let mut lines = vec![format!("certainty: {c}")];
        if c == "theorem"
            && let Some(ev) = evidence.as_ref().map(evidence_line)
        {
            lines.push(format!("evidence: {ev}"));
        }
        content = with_frontmatter(&content, &lines);
    }
    m.insert("content".into(), json!(content));
    m.insert("space".into(), json!(space));
    m.insert("path".into(), json!(rel));
    Ok(Value::Object(m))
}

/// Строка (без обрезки пробелов) поля `k`.
fn str_field_raw<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str)
}

/// Значение `evidence:` во frontmatter: `checked; method; result` строкой JSON (допустимая
/// строка YAML в двойных кавычках).
fn evidence_line(ev: &Value) -> String {
    let part = |k: &str| ev.get(k).and_then(Value::as_str).unwrap_or("").trim();
    let text = match ev.as_str() {
        Some(s) => s.trim().to_owned(),
        None => format!(
            "{}; {}; {}",
            part("checked"),
            part("method"),
            part("result")
        ),
    };
    Value::String(text).to_string()
}

/// Что снято из frontmatter.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Stripped {
    status: bool,
    relations: bool,
}

/// Начало и конец блока frontmatter (`---` … `---` в начале текста после BOM, как у comrak
/// ZAM): байтовые границы внутренних строк. Нет закрывающей строки — до конца текста
/// (консервативно: такой блок тоже снимается).
fn frontmatter_span(text: &str) -> Option<(usize, usize, usize)> {
    let bom = if text.starts_with('\u{feff}') { 3 } else { 0 };
    let rest = &text[bom..];
    let first_end = rest.find('\n')?;
    if rest[..first_end].trim_end_matches('\r') != "---" {
        return None;
    }
    let inner_start = bom + first_end + 1;
    let mut pos = inner_start;
    for line in text[inner_start..].split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Some((inner_start, pos, pos + line.len()));
        }
        pos += line.len();
    }
    Some((inner_start, text.len(), text.len()))
}

/// Ключ строки верхнего уровня frontmatter (как разбор ZAM: без отступа, до `:`), строчными и
/// без кавычек — для узнавания любых написаний.
fn fm_key(line: &str) -> Option<String> {
    if line.starts_with([' ', '\t', '#', '-']) {
        return None;
    }
    let (k, _) = line.split_once(':')?;
    Some(
        k.trim()
            .trim_matches(['"', '\''])
            .trim()
            .to_ascii_lowercase(),
    )
}

/// Снимает из frontmatter строки статуса (правило 2) и в tainted-контексте `relations:` —
/// вместе со строками продолжения (отступ, элементы списка `- `).
fn strip_frontmatter(text: &str, tainted: bool) -> (String, Stripped) {
    let mut out = Stripped::default();
    let Some((start, end, _)) = frontmatter_span(text) else {
        return (text.to_owned(), out);
    };
    let mut kept = String::with_capacity(text.len());
    kept.push_str(&text[..start]);
    let mut dropping = false;
    for line in text[start..end].split_inclusive('\n') {
        let bare = line.trim_end_matches(['\r', '\n']);
        match fm_key(bare) {
            Some(k) if STATUS_KEYS.contains(&k.as_str()) => {
                out.status = true;
                dropping = true;
                continue;
            }
            Some(k) if tainted && k == RELATIONS_KEY => {
                out.relations = true;
                dropping = true;
                continue;
            }
            Some(_) => dropping = false,
            None if dropping && bare.starts_with([' ', '\t', '-']) => continue,
            None => dropping = false,
        }
        kept.push_str(line);
    }
    kept.push_str(&text[end..]);
    (kept, out)
}

/// Текст с строками `lines` в начале frontmatter (нет блока — новый блок в начале текста,
/// после BOM).
fn with_frontmatter(text: &str, lines: &[String]) -> String {
    let add: String = lines.iter().map(|l| format!("{l}\n")).collect();
    match frontmatter_span(text) {
        Some((start, _, _)) => format!("{}{add}{}", &text[..start], &text[start..]),
        None => {
            let bom = if text.starts_with('\u{feff}') { 3 } else { 0 };
            format!("{}---\n{add}---\n{}", &text[..bom], &text[bom..])
        }
    }
}

/// Операция PATCH заметки создаёт или задевает frontmatter: её текст начинается строкой `---`.
fn touches_frontmatter(op: &Value) -> bool {
    let content = op.get("content").and_then(Value::as_str).unwrap_or("");
    let t = content.trim_start_matches(['\u{feff}', '\n', '\r', ' ', '\t']);
    t.lines().next().is_some_and(|l| l.trim_end() == "---")
}

/// PATCH заметки: `{base_rev, ops}`.
fn node_patch(rev: i64, ops: &Value) -> Result<Value, String> {
    let list = ops
        .as_array()
        .ok_or_else(|| "memory_write: ops must be an array".to_owned())?;
    if list.iter().any(touches_frontmatter) {
        return Err(FRONTMATTER_PATCH.to_owned());
    }
    Ok(json!({"base_rev": rev, "ops": ops}))
}

/// Правка записи (`PATCH /entries/{id}`).
fn patch(
    body: &Value,
    target: Option<&TargetTrust>,
    cx: &WriteCx,
    evidence_ok: bool,
    notes: &mut Vec<String>,
) -> Result<Value, String> {
    let mut m = object(body, "memory_update_entry")?;
    if human_forgery(&m) {
        return Err(HUMAN_ONLY.to_owned());
    }
    // `by: agent` — умолчание сервера: шлюз его не шлёт (П6).
    if str_field(&m, "by") == Some("agent") {
        m.remove("by");
    }
    let asked = str_field(&m, "certainty").map(str::to_owned);
    if cx.untrusted {
        if target.is_some_and(TargetTrust::trusted) {
            return Err(TAINTED.to_owned());
        }
        // Повышение `certainty` из tainted-контекста (MEM-077).
        if matches!(asked.as_deref(), Some("theorem")) {
            return Err(TAINTED.to_owned());
        }
        if m.contains_key("triggers") {
            m.remove("triggers");
            note_once(notes, NOTE_TAINTED_TRIGGERS);
        }
    } else if asked.as_deref() == Some("theorem") && !evidence_ok {
        m.insert("certainty".into(), json!("hypothesis"));
        note_once(notes, NOTE_EVIDENCE);
    }
    if m.contains_key("tags") {
        strip_provenance(&mut m, notes);
        let mut kept: Vec<String> = target
            .map(|t| {
                t.tags
                    .iter()
                    .filter(|x| is_provenance_tag(x))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if cx.untrusted {
            kept.push(TAG_TAINTED.to_owned());
        }
        put_tags(&mut m, &kept);
        // Пустой набор — снять все теги: поле остаётся пустым массивом.
        m.entry("tags").or_insert_with(|| json!([]));
    }
    Ok(Value::Object(m))
}

/// Замещение (`POST /entries/{id}/supersede`): новая запись и причина в `message` (MEM-078).
fn supersede(
    body: &Value,
    target: Option<&TargetTrust>,
    cx: &WriteCx,
    evidence_ok: bool,
    notes: &mut Vec<String>,
) -> Result<Value, String> {
    if cx.untrusted && target.is_some_and(TargetTrust::trusted) {
        return Err(TAINTED.to_owned());
    }
    let mut m = object(body, "supersede")?;
    if human_forgery(&m) {
        return Err(HUMAN_ONLY.to_owned());
    }
    let reason = m
        .remove("reason")
        .and_then(|r| r.as_str().map(|s| s.trim().to_owned()))
        .unwrap_or_default();
    // `scope` и `supersedes_id` задаёт замещаемая запись (иначе 422), `space` у тела нет.
    for k in ["scope", "supersedes_id", "space", "kind"] {
        m.remove(k);
    }
    strip_provenance(&mut m, notes);
    let kind = target
        .and_then(|t| t.kind.as_deref())
        .unwrap_or("insight")
        .to_owned();
    if kind == "insight" || (kind == "term" && m.contains_key("certainty")) {
        settle_certainty(&mut m, cx, evidence_ok, notes);
    }
    let own = str_field(&m, "message")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    m.remove("message");
    mark_origin(&mut m, &kind, cx, &[]);
    // Причина замещения — первой в `message`; строка происхождения (у видов без
    // `data.origin`) — после неё.
    let prov = str_field(&m, "message").map(str::to_owned);
    let msg = [Some(reason), own, prov]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    m.insert("message".into(), json!(msg));
    Ok(Value::Object(m))
}

/// Выжимка (`POST /cache`): правило 1 и область источников.
fn cache_put(v: &Value, cx: &WriteCx, guard: &ScopeGuard) -> Result<Value, String> {
    if cx.untrusted {
        return Err(CACHE_TAINTED.to_owned());
    }
    let m = object(v, "memory_cache_put")?;
    if m.contains_key("promote") {
        return Err(PROMOTE.to_owned());
    }
    guard.check_args("memory_cache_put", v, ScopeAccess::Read)?;
    if let Some(s) = str_field(&m, "solution") {
        guard.check_space(s, ScopeAccess::Write)?;
    }
    Ok(Value::Object(m))
}

/// `answer`, `decide` (`with_by`) и `propose`: `by: human` — отказ, `by` — `agent`; `space`
/// будущей задачи — в области run.
fn by_agent(body: &Value, with_by: bool, guard: &ScopeGuard) -> Result<Value, String> {
    let mut m = object(body, "memory_update_entry")?;
    if human_forgery(&m) {
        return Err(HUMAN_ONLY.to_owned());
    }
    if let Some(s) = m.get("space").filter(|v| !v.is_null()) {
        let s = s
            .as_str()
            .ok_or_else(|| "memory scope: space must be a string".to_owned())?;
        guard.check_space(s, ScopeAccess::Write)?;
    }
    if with_by {
        m.insert("by".into(), json!("agent"));
    } else {
        m.remove("by");
    }
    Ok(Value::Object(m))
}

/// Разбор идеи (D170, П10): из tainted-контекста — отказ; `space` — только решение run.
fn review(body: &Value, cx: &WriteCx, guard: &ScopeGuard) -> Result<Value, String> {
    if cx.untrusted {
        return Err(TAINTED.to_owned());
    }
    let mut m = object(body, "review")?;
    if human_forgery(&m) {
        return Err(HUMAN_ONLY.to_owned());
    }
    // У разбора нет поля `by` (сервер — 400): `by: agent` снимается.
    m.remove("by");
    if m.contains_key("space") {
        let mut probe = Map::new();
        probe.insert("action".into(), json!("review"));
        probe.insert("space".into(), m["space"].clone());
        guard.check_args(
            "memory_update_entry",
            &Value::Object(probe),
            ScopeAccess::Write,
        )?;
    }
    Ok(Value::Object(m))
}

/// Тексты шлюза для модели (хэш промптов, D94, D155; подключение — задача 8).
pub fn text_samples() -> Vec<(&'static str, String)> {
    [
        ("memory/gate.human_only", HUMAN_ONLY),
        ("memory/gate.tainted", TAINTED),
        ("memory/gate.needs_solution", NEEDS_SOLUTION),
        ("memory/gate.no_solution", NO_SOLUTION),
        ("memory/gate.rules_read_only", RULES_READ_ONLY),
        ("memory/gate.cache_tainted", CACHE_TAINTED),
        ("memory/gate.promote", PROMOTE),
        ("memory/gate.frontmatter_patch", FRONTMATTER_PATCH),
        ("memory/gate.note_provenance", NOTE_PROVENANCE),
        (
            "memory/gate.note_tainted_hypothesis",
            NOTE_TAINTED_HYPOTHESIS,
        ),
        ("memory/gate.note_tainted_triggers", NOTE_TAINTED_TRIGGERS),
        ("memory/gate.note_evidence", NOTE_EVIDENCE),
        ("memory/gate.note_frontmatter", NOTE_FRONTMATTER),
        ("memory/gate.note_tainted_relations", NOTE_TAINTED_RELATIONS),
        ("memory/scope.read_only", crate::scope::READ_ONLY),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_owned()))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_status_lines_stripped_with_continuations() {
        let text = "---\ntags: [a]\ncertainty: axiom\nevidence: |\n  long\n  text\nverified_at: 2026-11-01\n\"Certainty\": theorem\nrelations:\n- links_to [[X]]\n---\n# T\ncertainty: in body stays\n";
        let (out, s) = strip_frontmatter(text, true);
        assert!(s.status && s.relations);
        assert_eq!(out, "---\ntags: [a]\n---\n# T\ncertainty: in body stays\n");
        let (same, s) = strip_frontmatter("# no frontmatter\ncertainty: x\n", true);
        assert_eq!(same, "# no frontmatter\ncertainty: x\n");
        assert_eq!(s, Stripped::default());
    }

    #[test]
    fn frontmatter_lines_inserted() {
        let l = vec!["certainty: hypothesis".to_owned()];
        assert_eq!(
            with_frontmatter("# T\n", &l),
            "---\ncertainty: hypothesis\n---\n# T\n"
        );
        assert_eq!(
            with_frontmatter("---\ntags: [a]\n---\nx", &l),
            "---\ncertainty: hypothesis\ntags: [a]\n---\nx"
        );
        assert_eq!(
            with_frontmatter("\u{feff}# T", &l),
            "\u{feff}---\ncertainty: hypothesis\n---\n# T"
        );
    }

    #[test]
    fn patch_op_frontmatter_detected() {
        assert!(touches_frontmatter(
            &json!({"op": "prepend", "content": "---\ncertainty: axiom\n---\n"})
        ));
        assert!(touches_frontmatter(
            &json!({"op": "append", "content": "\n\n---\nx\n"})
        ));
        assert!(!touches_frontmatter(
            &json!({"op": "append", "content": "text\n---\n"})
        ));
    }
}
