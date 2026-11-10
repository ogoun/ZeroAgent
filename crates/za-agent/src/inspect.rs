//! Инспектор контекста и метрики run по журналу (R1-B2 D52, D53; CTX-018, OPS-002,
//! OPS-003).
//!
//! [`inspect`] восстанавливает запрос хода точно: событие `model.request` хранит блоб
//! [`FrozenPrefix`], хэши блобов сообщений и `options` хода (D67), `cache_key` — id
//! проекта run. `request_hash` восстановленного запроса сверяется с записанным: расхождение
//! — [`InspectError::Bad`], а не правдоподобный, но чужой запрос. Разбивка — блоки раздела
//! 2.1 части 02 (байты, оценка токенов байты / 4 вверх, хэш), итог провайдера —
//! `usage.input` хода, предсказание следующего хода — по правилу счётчика D41.
//!
//! [`run_metrics`] считает метрики run по событиям журнала (источник истины, часть 01,
//! 6.1), стоимость — по таблице `usage`. Оба только читают журнал.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use za_llm::{
    ChatRequest, EndpointId, Message, PromptBlocks, ReasoningMode, Role, Sampling, ToolChoice,
    render_tools,
};
use za_protocol::{BlobHash, Event, EventKind, RunId, Seq};
use za_store::{ProjectStore, StoreError};

use crate::context::{TEMPLATE_TOKENS, message_text};
use crate::conversation::{Conversation, HistoryError, LoadError};
use crate::prompt::approx_tokens;
use crate::request::{FrozenPrefix, request_hash};
use crate::resume::RUNTIME_CONTROL;
use crate::turn::{COMPACT_KIND, MODEL_ERROR};

/// Страница чтения журнала.
const PAGE: usize = 1024;
/// Вид `context.alert` смены окна на resume (D6, D43).
const N_CTX_CHANGED: &str = "n_ctx_changed";
/// Исход отказа в конверте квитанции эффекта (`za-core`).
const OUTCOME_ERROR: &str = "error";

/// Стоимость блока запроса: байты UTF-8, оценка токенов (байты / 4 вверх, у сообщений —
/// плюс 4 токена шаблона на сообщение, D41) и BLAKE3 hex блока; у блоков из нескольких
/// сообщений (`dialog`, `control`) хэша нет.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockCost {
    pub name: &'static str,
    pub bytes: usize,
    pub tokens: u32,
    pub hash: Option<String>,
}

/// Запрос хода, восстановленный по журналу (D52).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Inspection {
    pub turn: u32,
    pub endpoint: EndpointId,
    /// `tools`, `core`, `adapter`, `overlay`, `agents_md` (у префикса без размеров —
    /// `system` одной строкой), `first_message`, `dialog`, `control`.
    pub blocks: Vec<BlockCost>,
    /// Оценка запроса целиком (D41 без базы): system, описания инструментов, сообщения.
    pub estimate: u32,
    /// `usage.input` хода; `None` — расхода в журнале нет (ход не дошёл до `Finished`).
    pub input: Option<u64>,
    /// Окно модели на момент хода.
    pub n_ctx: u32,
    /// Предсказание следующего запроса (D41): база `input + output` хода и сообщения
    /// после его ответа; без базы — запрос целиком.
    pub next_turn: u32,
    /// Запрос хода байт в байт (его `request_hash` совпал с записанным).
    #[serde(skip)]
    pub request: ChatRequest,
}

/// Метрики run по журналу (D53, OPS-002).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RunMetrics {
    /// Ходы: разные номера `turn` в `turn.finished` (повтор хода D69 — тот же ход).
    pub turns: u32,
    /// Шаги: эффекты `model` — разные `effect_key` в `model.request` без `kind: "compact"`.
    pub steps: u32,
    /// Суммы `model.usage` всех вызовов модели, включая компакцию.
    pub input: u64,
    pub cached: u64,
    /// `cached / input`; 0 при `input == 0`.
    pub cache_share: f64,
    pub output: u64,
    /// Время до первого токена, nearest-rank: `sorted[ceil(q × n) − 1]`.
    pub ttft_p50_ms: Option<u64>,
    pub ttft_p95_ms: Option<u64>,
    /// Средняя скорость вывода по вызовам, где она известна, токены/с.
    pub output_tps: Option<f64>,
    /// Ошибки модели по классам (`ErrorClass`, snake_case) из квитанций `model_error:`.
    pub errors: BTreeMap<String, u32>,
    /// Сумма `turn.finished.repairs`.
    pub repairs: u32,
    /// Запросы компакции (`model.request` с `kind: "compact"`, разные `effect_key`).
    pub compactions: u32,
    /// Стоимость по таблице `usage` (`usage_totals`).
    pub cost_usd: f64,
}

/// Ошибка инспектора.
#[derive(Debug, thiserror::Error)]
pub enum InspectError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Load(#[from] LoadError),
    #[error("событие model.request: {0}")]
    Bad(String),
}

/// `options` хода в `model.request` (D67).
#[derive(Deserialize)]
struct Options {
    tool_choice: ToolChoice,
    sampling: Sampling,
    max_output_tokens: Option<u32>,
    reasoning: ReasoningMode,
}

/// Запрос хода `turn` run (последний ход, если `turn` — `None`).
///
/// Ход N — последний `model.request` с `turn == N` без `kind: "compact"`: у повторённого
/// хода (D49, D58, D59) — последняя попытка. Нет такого хода — `Ok(None)`. Нет поля,
/// не разбираются `options`, `request_hash` не совпал — [`InspectError::Bad`]; нет блоба
/// — [`InspectError::Load`].
pub async fn inspect(
    store: &ProjectStore,
    run: RunId,
    turn: Option<u32>,
) -> Result<Option<Inspection>, InspectError> {
    let evs = run_events(store, run).await?;
    let requests: Vec<&Event> = evs
        .iter()
        .filter(|e| e.kind == EventKind::ModelRequest && !is_compact(e))
        .collect();
    let n = match turn.or_else(|| requests.iter().filter_map(|e| turn_of(e)).max()) {
        Some(n) => n,
        None => return Ok(None),
    };
    let Some(pos) = requests.iter().rposition(|e| turn_of(e) == Some(n)) else {
        return Ok(None);
    };
    let target = requests[pos];
    let p = &target.payload;
    let endpoint = EndpointId(str_field(p, "endpoint")?.to_owned());
    let refs = refs_of(p)?;
    let options: Options = serde_json::from_value(field(p, "options")?.clone())
        .map_err(|e| InspectError::Bad(format!("options не разбираются: {e}")))?;
    let frozen = load_frozen(store, &BlobHash(str_field(p, "frozen")?.to_owned())).await?;
    let latest = Latest::read(store, run).await?;
    let conv = Conversation::load(store, &refs).await?;
    let request = ChatRequest {
        system: frozen.system.clone(),
        tools: frozen.tools.clone(),
        messages: conv.messages().to_vec(),
        tool_choice: options.tool_choice,
        max_output_tokens: options.max_output_tokens,
        sampling: options.sampling,
        reasoning: options.reasoning,
        // `build_request`: ключ кэша — id проекта run (MDL-022).
        cache_key: Some(latest.project_id.clone()),
    };
    let recorded = str_field(p, "request")?;
    let hash = request_hash(&request)
        .map_err(|e| InspectError::Bad(format!("запрос не сериализуется: {e}")))?;
    if hash != recorded {
        return Err(InspectError::Bad(format!(
            "request_hash восстановленного запроса {hash} не совпадает с записанным {recorded}"
        )));
    }

    let key = p.get("effect_key").and_then(Value::as_str);
    let usage = key.and_then(|k| usage_of(&evs, k));
    let next_refs = match requests.get(pos + 1) {
        Some(e) => refs_of(&e.payload)?,
        None => latest.messages.clone(),
    };
    let next = Conversation::load(store, &next_refs).await?;
    let next_turn = predict(&frozen, &refs, usage, &next_refs, next.messages());
    Ok(Some(Inspection {
        turn: n,
        endpoint,
        blocks: blocks(&frozen, &refs, request.messages.as_slice()),
        estimate: whole(&frozen, &request.messages),
        input: usage.map(|u| u.input),
        n_ctx: n_ctx_at(&evs, target.seq, latest.n_ctx),
        next_turn,
        request,
    }))
}

/// Метрики run по журналу (D53). Run без событий — нули.
pub async fn run_metrics(store: &ProjectStore, run: RunId) -> Result<RunMetrics, StoreError> {
    let evs = run_events(store, run).await?;
    let mut m = RunMetrics::default();
    let (mut turns, mut steps, mut compactions) =
        (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    let (mut ttft, mut tps) = (Vec::new(), Vec::new());
    for e in &evs {
        let p = &e.payload;
        match e.kind {
            EventKind::TurnFinished if !is_compact(e) => {
                if let Some(t) = turn_of(e) {
                    turns.insert(t);
                }
                m.repairs = m.repairs.saturating_add(u32_of(p.get("repairs")));
            }
            EventKind::ModelRequest => {
                // Без `effect_key` каждый запрос — свой шаг.
                let key = p
                    .get("effect_key")
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("seq:{}", e.seq.0), str::to_owned);
                if is_compact(e) {
                    compactions.insert(key);
                } else {
                    steps.insert(key);
                }
            }
            EventKind::ModelUsage => {
                let u = &p["usage"];
                let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                m.input = m.input.saturating_add(get("input"));
                m.cached = m.cached.saturating_add(get("cached"));
                m.output = m.output.saturating_add(get("output"));
                ttft.extend(p.get("ttft_ms").and_then(Value::as_u64));
                tps.extend(p.get("output_tps").and_then(Value::as_f64));
            }
            EventKind::EffectReceipt => {
                if let Some(class) = model_error_class(store, p).await? {
                    *m.errors.entry(class).or_default() += 1;
                }
            }
            _ => {}
        }
    }
    m.turns = count(turns.len());
    m.steps = count(steps.len());
    m.compactions = count(compactions.len());
    m.cache_share = if m.input == 0 {
        0.0
    } else {
        m.cached as f64 / m.input as f64
    };
    ttft.sort_unstable();
    m.ttft_p50_ms = nearest_rank(&ttft, 50);
    m.ttft_p95_ms = nearest_rank(&ttft, 95);
    m.output_tps = (!tps.is_empty()).then(|| tps.iter().sum::<f64>() / tps.len() as f64);
    m.cost_usd = store.usage_totals(run).await?.cost_usd;
    Ok(m)
}

// ---- Журнал ----

/// Все события run по возрастанию `seq`.
async fn run_events(store: &ProjectStore, run: RunId) -> Result<Vec<Event>, StoreError> {
    let mut out: Vec<Event> = Vec::new();
    loop {
        let after = out.last().map_or(Seq(0), |e| e.seq);
        let page = store.read_run(run, after, PAGE).await?;
        let full = page.len() == PAGE;
        out.extend(page);
        if !full {
            return Ok(out);
        }
    }
}

fn is_compact(e: &Event) -> bool {
    e.payload.get("kind").and_then(Value::as_str) == Some(COMPACT_KIND)
}

fn turn_of(e: &Event) -> Option<u32> {
    e.payload
        .get("turn")
        .and_then(Value::as_u64)
        .and_then(|t| u32::try_from(t).ok())
}

fn u32_of(v: Option<&Value>) -> u32 {
    v.and_then(Value::as_u64)
        .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn field<'a>(p: &'a Value, k: &str) -> Result<&'a Value, InspectError> {
    p.get(k)
        .filter(|v| !v.is_null())
        .ok_or_else(|| InspectError::Bad(format!("нет поля {k}")))
}

fn str_field<'a>(p: &'a Value, k: &str) -> Result<&'a str, InspectError> {
    field(p, k)?
        .as_str()
        .ok_or_else(|| InspectError::Bad(format!("поле {k} не строка")))
}

/// Хэши блобов сообщений запроса.
fn refs_of(p: &Value) -> Result<Vec<BlobHash>, InspectError> {
    serde_json::from_value(field(p, "messages")?.clone())
        .map_err(|e| InspectError::Bad(format!("messages не разбираются: {e}")))
}

async fn load_frozen(store: &ProjectStore, h: &BlobHash) -> Result<FrozenPrefix, InspectError> {
    let raw = store
        .get_blob(h)
        .await?
        .ok_or_else(|| LoadError::History(HistoryError::MissingBlob(h.0.clone())))?;
    serde_json::from_slice(&raw).map_err(|e| {
        InspectError::Store(StoreError::Corrupt(format!(
            "блоб FrozenPrefix {}: {e}",
            h.0
        )))
    })
}

/// Расход вызова по ключу его эффекта (последний `model.usage` с этим ключом).
#[derive(Clone, Copy)]
struct Spent {
    input: u64,
    output: u64,
}

fn usage_of(evs: &[Event], key: &str) -> Option<Spent> {
    evs.iter()
        .rev()
        .find(|e| {
            e.kind == EventKind::ModelUsage
                && e.payload.get("effect_key").and_then(Value::as_str) == Some(key)
        })
        .map(|e| {
            let u = &e.payload["usage"];
            let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            Spent {
                input: get("input"),
                output: get("output"),
            }
        })
}

/// Последний снимок run: id проекта (ключ кэша запроса), окно и история.
struct Latest {
    project_id: String,
    n_ctx: u32,
    messages: Vec<BlobHash>,
}

impl Latest {
    async fn read(store: &ProjectStore, run: RunId) -> Result<Latest, StoreError> {
        let Some((_, snap)) = store.latest_snapshot(run).await? else {
            return Err(StoreError::Corrupt(format!("у run {run} нет снимка")));
        };
        let bad = |what: &str| StoreError::Corrupt(format!("снимок run {run}: {what}"));
        let setup = &snap["setup"];
        let project_id = setup["project_id"]
            .as_str()
            .ok_or_else(|| bad("нет setup.project_id"))?
            .to_owned();
        let n_ctx = setup["n_ctx"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| bad("нет setup.n_ctx"))?;
        let messages = serde_json::from_value(snap["messages"].clone())
            .map_err(|e| bad(&format!("история: {e}")))?;
        Ok(Latest {
            project_id,
            n_ctx,
            messages,
        })
    }
}

/// Окно на момент события `seq`: `from` первой смены окна после него (`context.alert
/// n_ctx_changed`, resume пишет её при каждой смене), иначе окно последнего снимка.
fn n_ctx_at(evs: &[Event], seq: Seq, current: u32) -> u32 {
    evs.iter()
        .find(|e| {
            e.seq > seq
                && e.kind == EventKind::ContextAlert
                && e.payload.get("kind").and_then(Value::as_str) == Some(N_CTX_CHANGED)
        })
        .and_then(|e| e.payload.get("from").and_then(Value::as_u64))
        .map_or(current, |n| u32::try_from(n).unwrap_or(u32::MAX))
}

/// Класс ошибки модели квитанции-отказа `model_error:` (D7); прочие квитанции — `None`.
async fn model_error_class(store: &ProjectStore, p: &Value) -> Result<Option<String>, StoreError> {
    let result = match (
        p.get("result"),
        p.get("result_blob").and_then(Value::as_str),
    ) {
        (Some(r), _) => r.clone(),
        (None, Some(h)) => match store.get_blob(&BlobHash(h.to_owned())).await? {
            Some(raw) => serde_json::from_slice(&raw)
                .map_err(|e| StoreError::Corrupt(format!("результат эффекта в блобе {h}: {e}")))?,
            None => return Err(StoreError::Corrupt(format!("нет блоба результата {h}"))),
        },
        (None, None) => return Ok(None),
    };
    if result.get("outcome").and_then(Value::as_str) != Some(OUTCOME_ERROR) {
        return Ok(None);
    }
    let Some(body) = result
        .get("message")
        .and_then(Value::as_str)
        .and_then(|m| m.strip_prefix(MODEL_ERROR))
    else {
        return Ok(None);
    };
    let class = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["class"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    Ok(Some(class))
}

/// `sorted[ceil(q × n) − 1]` для `q = pct / 100`; пусто — `None`.
fn nearest_rank(sorted: &[u64], pct: usize) -> Option<u64> {
    let rank = (pct * sorted.len()).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

// ---- Блоки и оценки ----

fn tokens_of_bytes(bytes: usize) -> u32 {
    u32::try_from(bytes.div_ceil(4)).unwrap_or(u32::MAX)
}

fn hex(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex().to_string()
}

fn rendered_tools(frozen: &FrozenPrefix) -> String {
    render_tools(&PromptBlocks {
        tools: frozen.tools.clone(),
        ..PromptBlocks::default()
    })
}

/// Оценка сообщения по D41: текст байты / 4 вверх и 4 токена шаблона.
fn message_tokens(m: &Message) -> u64 {
    u64::from(approx_tokens(&message_text(m))).saturating_add(TEMPLATE_TOKENS)
}

/// Управляющее сообщение рантайма — блок 8 «напоминания» (nudge, `reminder`, `resume`).
fn is_runtime_control(m: &Message) -> bool {
    m.role == Role::User && message_text(m).starts_with(RUNTIME_CONTROL)
}

fn messages_block(name: &'static str, msgs: &[&Message]) -> BlockCost {
    BlockCost {
        name,
        bytes: msgs.iter().map(|m| message_text(m).len()).sum(),
        tokens: clamp(msgs.iter().map(|m| message_tokens(m)).sum()),
        hash: None,
    }
}

fn clamp(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn blocks(frozen: &FrozenPrefix, refs: &[BlobHash], msgs: &[Message]) -> Vec<BlockCost> {
    let sized = |name, bytes: usize, hash: &str| BlockCost {
        name,
        bytes,
        tokens: tokens_of_bytes(bytes),
        hash: Some(hash.to_owned()),
    };
    let tools = rendered_tools(frozen);
    let mut out = vec![sized("tools", tools.len(), &frozen.blocks.tools)];
    match &frozen.sizes {
        Some(s) => {
            let b = &frozen.blocks;
            out.push(sized("core", s.core, &b.core));
            out.push(sized("adapter", s.adapter, &b.adapter));
            out.push(sized("overlay", s.overlay, &b.overlay));
            out.push(sized("agents_md", s.agents_md, &b.agents_md));
        }
        // Префикс R1-B1 без размеров: `system` нельзя разрезать по `\n\n` (D52).
        None => out.push(sized("system", frozen.system.len(), &hex(&frozen.system))),
    }
    let first = msgs.first().map(|m| BlockCost {
        hash: refs.first().map(|h| h.0.clone()),
        ..messages_block("first_message", &[m])
    });
    out.push(first.unwrap_or_else(|| messages_block("first_message", &[])));
    let rest = msgs.get(1..).unwrap_or_default();
    let (control, dialog): (Vec<&Message>, Vec<&Message>) =
        rest.iter().partition(|m| is_runtime_control(m));
    out.push(messages_block("dialog", &dialog));
    out.push(messages_block("control", &control));
    out
}

/// Запрос целиком (D41 без базы): system, описания инструментов, сообщения.
fn whole(frozen: &FrozenPrefix, msgs: &[Message]) -> u32 {
    let head = u64::from(approx_tokens(&frozen.system))
        .saturating_add(u64::from(approx_tokens(&rendered_tools(frozen))));
    clamp(
        msgs.iter()
            .map(message_tokens)
            .fold(head, u64::saturating_add),
    )
}

/// Предсказание следующего запроса после хода (D41). База — `input + output` хода при
/// `input > 0`; она покрывает историю хода и его ответ: следующая история продолжает
/// сообщения хода, за ними служебные `Role::System` и ответ ассистента (D10). Дельта —
/// сообщения после ответа. Иначе (ход без расхода или ответа, история переписана
/// удалением или компакцией) — следующий запрос целиком.
fn predict(
    frozen: &FrozenPrefix,
    refs: &[BlobHash],
    usage: Option<Spent>,
    next_refs: &[BlobHash],
    next: &[Message],
) -> u32 {
    let base = usage
        .filter(|u| u.input > 0)
        .map(|u| u.input.saturating_add(u.output));
    let covered = next_refs.starts_with(refs).then(|| {
        let mut i = refs.len();
        while next.get(i).is_some_and(|m| m.role == Role::System) {
            i += 1;
        }
        next.get(i)
            .filter(|m| m.role == Role::Assistant)
            .map(|_| i + 1)
    });
    match (base, covered.flatten()) {
        (Some(b), Some(len)) => clamp(
            next[len..]
                .iter()
                .map(message_tokens)
                .fold(b, u64::saturating_add),
        ),
        _ => whole(frozen, next),
    }
}
