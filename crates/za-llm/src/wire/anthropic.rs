//! Anthropic Messages (MDL-001, MDL-002, D15, D36).
//!
//! Тело запроса собирается из канонического [`ChatRequest`]: `system` на верхнем
//! уровне, результаты инструментов — блоками `tool_result` в сообщении `user`,
//! рассуждение той же модели возвращается блоками `thinking` без изменений. Стрим SSE
//! переводится в [`StreamEvent`] декодером [`AnthropicDecoder`]; итоговое сообщение
//! собирает общий [`MessageAssembler`] (D35).
//!
//! Решения по актуальным моделям (справочник Claude API, 2026-10; платформенная
//! документация, проверено 2026-11-09):
//! - на `anthropic-version: 2026-10-21` поля `temperature`, `top_p`, `top_k` дают 400
//!   на всех моделях — не отправляются (D15);
//! - у адаптивных моделей `budget_tokens` и (у части) `{"type":"disabled"}` дают 400:
//!   мышление задаётся `thinking: {type: "adaptive"}` и `output_config.effort`, а
//!   `ReasoningMode::Off` — без поля `thinking` и с наименьшим усилием `low`;
//! - принудительный `tool_choice` (`any`, `tool`) у Opus 5.5, Sonnet 5.5, Fable 5.1 и
//!   при включённом мышлении даёт 400: он отправляется только старым моделям без
//!   мышления в запросе, иначе — `auto` и инструкция модели.

use std::collections::{BTreeMap, HashSet};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::{
    MessageAssembler, OpenedStream, STREAM_CHANNEL, StreamDecoder, WireCtx, bad_request, pump,
    u64_at,
};
use crate::caps::{ThinkingSupport, Wire};
use crate::error::{ErrorClass, ModelError};
use crate::http::{HttpTransport, Secret, SseEvent, classify_status, parse_rate_limits};
use crate::prefix::{PromptBlocks, render_tools};
use crate::types::{
    ChatRequest, Effort, FinishReason, Message, Part, ReasoningMode, ReplayBlob, Role, StreamEvent,
    ToolCall, ToolChoice, Usage,
};

/// Версия API (D15).
pub const ANTHROPIC_VERSION: &str = "2026-10-21";

/// `max_tokens` без значения в запросе и в профиле (D36).
const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Наименьший `budget_tokens` у моделей с `enabled`.
const MIN_BUDGET: u32 = 1024;

/// Запас `max_tokens` сверх бюджета мышления на сам ответ.
const ANSWER_RESERVE: u32 = 1024;

/// Коннектор Anthropic Messages.
#[derive(Debug, Clone)]
pub struct AnthropicWire {
    /// Корень версии API, `https://api.anthropic.com/v1` (D37).
    pub base_url: String,
    pub model: String,
}

/// Как выражено мышление в теле запроса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Thinking {
    /// Поля `thinking` нет.
    Omitted,
    Adaptive,
    /// `{"type":"enabled","budget_tokens":n}`.
    Enabled(u32),
}

impl AnthropicWire {
    /// Адрес запроса: `{base}/messages` (D37).
    pub fn url(&self) -> String {
        format!("{}/messages", self.base_url.trim_end_matches('/'))
    }

    /// Заголовки: `x-api-key` (если ключ задан) и `anthropic-version`.
    /// `content-type: application/json` ставит транспорт вместе с телом.
    pub fn headers(&self, ctx: &WireCtx<'_>) -> Result<HeaderMap, ModelError> {
        let mut h = HeaderMap::new();
        if let Some(s) = ctx.secret {
            h.insert(HeaderName::from_static("x-api-key"), s.header_value("")?);
        }
        h.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        Ok(h)
    }

    /// Тело запроса. Принудительный выбор без инструментов, неизвестное имя в
    /// `ToolChoice::Named` и повтор имени инструмента — `BadRequest` до отправки.
    pub fn build_body(&self, req: &ChatRequest, ctx: &WireCtx<'_>) -> Result<Value, ModelError> {
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        let cache = req.cache_key.is_some();
        if !req.system.is_empty() {
            let system = if cache {
                json!([{"type": "text", "text": req.system,
                        "cache_control": {"type": "ephemeral"}}])
            } else {
                json!(req.system)
            };
            body.insert("system".into(), system);
        }

        let thinking = self.thinking(&mut body, &req.reasoning, ctx.thinking);
        let mut messages = self.messages(req);
        if req.tools.is_empty() {
            if matches!(req.tool_choice, ToolChoice::Required | ToolChoice::Named(_)) {
                return Err(bad_request("принудительный tool_choice без инструментов"));
            }
        } else {
            body.insert("tools".into(), tools(req, ctx.caps.strict_schema, cache)?);
            let forced_ok = ctx.caps.tool_choice_required
                && ctx.thinking.is_some_and(|t| !t.adaptive)
                && thinking == Thinking::Omitted;
            let (choice, instruction) = tool_choice(req, forced_ok)?;
            let mut choice = choice;
            if !ctx.caps.parallel_tools && choice["type"] != "none" {
                choice["disable_parallel_tool_use"] = json!(true);
            }
            body.insert("tool_choice".into(), choice);
            if let Some(text) = instruction {
                append_user_text(&mut messages, text);
            }
        }
        body.insert("messages".into(), Value::Array(messages));

        let mut max_tokens = req
            .max_output_tokens
            .or(ctx.max_output_tokens)
            .unwrap_or(DEFAULT_MAX_TOKENS);
        if let Thinking::Enabled(budget) = thinking {
            // `budget_tokens` должен быть меньше `max_tokens`.
            max_tokens = max_tokens.max(budget.saturating_add(ANSWER_RESERVE));
        }
        body.insert("max_tokens".into(), json!(max_tokens));
        body.insert("stream".into(), json!(true));

        let s = &req.sampling;
        if s.temperature.is_some() || s.top_p.is_some() {
            tracing::debug!(
                "temperature/top_p не отправляются Anthropic: на {ANTHROPIC_VERSION} это 400"
            );
        }
        if s.seed.is_some() || s.presence_penalty.is_some() {
            tracing::debug!("seed/presence_penalty Anthropic не поддерживает, не отправляются");
        }
        if !s.stop.is_empty() {
            body.insert("stop_sequences".into(), json!(s.stop));
        }
        Ok(Value::Object(body))
    }

    /// Отправляет запрос и запускает насос SSE (D2, D35).
    pub async fn open(
        &self,
        http: &HttpTransport,
        req: &ChatRequest,
        ctx: &WireCtx<'_>,
    ) -> Result<OpenedStream, ModelError> {
        let body = self.build_body(req, ctx)?;
        let headers = self.headers(ctx)?;
        let secrets: Vec<&Secret> = ctx.secret.into_iter().collect();
        let sse = http.post_sse(&self.url(), headers, &body, &secrets).await?;
        let limits = parse_rate_limits(&sse.headers);
        let (tx, events) = mpsc::channel(STREAM_CHANNEL);
        let decoder = AnthropicDecoder::new(self.model.clone());
        tokio::spawn(pump(sse, decoder, tx, ctx.secret.cloned()));
        Ok(OpenedStream { limits, events })
    }

    /// Поля мышления (D15, D36). Способ — по профилю: `adaptive` из Models API;
    /// без профиля — `enabled` с бюджетом, как у старых моделей.
    fn thinking(
        &self,
        body: &mut Map<String, Value>,
        mode: &ReasoningMode,
        support: Option<&ThinkingSupport>,
    ) -> Thinking {
        let adaptive = support.is_some_and(|t| t.adaptive);
        let effort_ok = support.is_some_and(|t| t.effort);
        let mut put_effort = |e: Effort| {
            if effort_ok {
                body.insert("output_config".into(), json!({"effort": e.as_str()}));
            }
        };
        let thinking = match mode {
            ReasoningMode::Default => Thinking::Omitted,
            ReasoningMode::Off if adaptive => {
                // `{"type":"disabled"}` у Opus 5.5 и Sonnet 5.5 — 400: поле не
                // отправляется, глубина снижается до наименьшего усилия.
                put_effort(Effort::Low);
                Thinking::Omitted
            }
            // Старые модели без поля `thinking` не думают.
            ReasoningMode::Off => Thinking::Omitted,
            ReasoningMode::Effort(e) if adaptive => {
                put_effort(*e);
                Thinking::Adaptive
            }
            ReasoningMode::Budget(n) if adaptive => {
                put_effort(Effort::nearest(*n));
                Thinking::Adaptive
            }
            ReasoningMode::Effort(e) => Thinking::Enabled(e.budget_tokens()),
            ReasoningMode::Budget(n) => Thinking::Enabled((*n).max(MIN_BUDGET)),
        };
        match thinking {
            Thinking::Omitted => {}
            Thinking::Adaptive => {
                body.insert("thinking".into(), json!({"type": "adaptive"}));
            }
            Thinking::Enabled(n) => {
                body.insert(
                    "thinking".into(),
                    json!({"type": "enabled", "budget_tokens": n}),
                );
            }
        }
        thinking
    }

    /// Сообщения. Подряд идущие сообщения одной роли сливаются в одно: так все
    /// результаты инструментов одного хода уходят одним сообщением `user`.
    fn messages(&self, req: &ChatRequest) -> Vec<Value> {
        let mut out: Vec<(&'static str, Vec<Value>)> = Vec::new();
        for m in &req.messages {
            let (role, blocks) = match m.role {
                Role::Assistant => ("assistant", self.assistant_blocks(m)),
                Role::User | Role::Tool => ("user", user_blocks(m)),
            };
            if blocks.is_empty() {
                continue;
            }
            match out.last_mut() {
                Some((r, prev)) if *r == role => prev.extend(blocks),
                _ => out.push((role, blocks)),
            }
        }
        out.into_iter()
            .map(|(role, content)| json!({"role": role, "content": content}))
            .collect()
    }

    /// Блоки ассистента. Если в сообщении есть рассуждение этой модели с данными
    /// повтора, блоки идут в исходном порядке ответа, `thinking` и
    /// `redacted_thinking` — без изменений. Иначе — текст, затем вызовы.
    fn assistant_blocks(&self, m: &Message) -> Vec<Value> {
        let text: String = m
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        let calls: Vec<&ToolCall> = m
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::ToolCall(c) => Some(c),
                _ => None,
            })
            .collect();
        let layout = m.parts.iter().find_map(|p| match p {
            Part::Reasoning {
                replay: Some(b), ..
            } if self.replay_matches(b) => b.data.get("blocks").and_then(Value::as_array),
            _ => None,
        });
        let mut out = Vec::new();
        let mut text_done = text.is_empty();
        let mut used: HashSet<&str> = HashSet::new();
        if let Some(layout) = layout {
            let layout_text: String = layout
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect();
            // Текст не меняли: блоки текста повторяются как были (их может быть
            // несколько); иначе весь текст — на месте первого блока текста.
            let verbatim_text = layout_text == text;
            for b in layout {
                match b["type"].as_str() {
                    Some("thinking" | "redacted_thinking") => out.push(b.clone()),
                    Some("text") if verbatim_text => {
                        if b["text"].as_str().is_some_and(|t| !t.is_empty()) {
                            out.push(b.clone());
                        }
                        text_done = true;
                    }
                    Some("text") if !text_done => {
                        out.push(text_block(&text));
                        text_done = true;
                    }
                    Some("tool_use") => {
                        let id = b["id"].as_str().unwrap_or_default();
                        if let Some(c) = calls.iter().find(|c| c.id == id)
                            && used.insert(c.id.as_str())
                        {
                            out.push(tool_use_block(c));
                        }
                    }
                    _ => {}
                }
            }
        }
        if !text_done {
            out.push(text_block(&text));
        }
        for c in calls {
            if !used.contains(c.id.as_str()) {
                out.push(tool_use_block(c));
            }
        }
        out
    }

    /// Данные повтора действительны только для того же протокола и той же модели.
    fn replay_matches(&self, b: &ReplayBlob) -> bool {
        b.wire == Wire::AnthropicMessages && b.model == self.model
    }
}

/// Блоки сообщения пользователя или результатов инструментов: сначала
/// `tool_result`, затем текст (API требует результаты первыми).
fn user_blocks(m: &Message) -> Vec<Value> {
    let mut results = Vec::new();
    let mut texts = Vec::new();
    for p in &m.parts {
        match p {
            Part::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                let mut b = json!({"type": "tool_result", "tool_use_id": call_id,
                                   "content": content});
                if *is_error {
                    b["is_error"] = json!(true);
                }
                results.push(b);
            }
            // Пустой блок текста API отвергает.
            Part::Text(t) if !t.is_empty() => texts.push(text_block(t)),
            Part::Text(_) => {}
            Part::Reasoning { .. } | Part::ToolCall(_) => {
                tracing::debug!("рассуждение и вызов в сообщении пользователя пропущены");
            }
        }
    }
    results.extend(texts);
    results
}

fn text_block(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

/// `tool_use` с аргументами-объектом. Не разобранные аргументы (`Null`) берутся из
/// исходной строки, если она — объект JSON; иначе `{}`.
fn tool_use_block(c: &ToolCall) -> Value {
    let input = match &c.arguments {
        Value::Object(_) => c.arguments.clone(),
        _ => c
            .raw_arguments
            .as_deref()
            .and_then(|r| serde_json::from_str::<Value>(r).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| {
                tracing::debug!(call = %c.id, "аргументы вызова не объект JSON, отправлено {{}}");
                json!({})
            }),
    };
    json!({"type": "tool_use", "id": c.id, "name": c.name, "input": input})
}

/// Дописывает текст в конец последнего сообщения пользователя (или новым
/// сообщением, если последним идёт ассистент).
fn append_user_text(messages: &mut Vec<Value>, text: String) {
    if let Some(last) = messages.last_mut()
        && last["role"] == "user"
        && let Some(content) = last["content"].as_array_mut()
    {
        content.push(text_block(&text));
        return;
    }
    messages.push(json!({"role": "user", "content": [text_block(&text)]}));
}

/// `tool_choice` и инструкция для эмуляции принудительного выбора. `forced_ok` —
/// модель принимает `any`/`tool` (старая модель, мышление в запросе выключено).
fn tool_choice(req: &ChatRequest, forced_ok: bool) -> Result<(Value, Option<String>), ModelError> {
    Ok(match &req.tool_choice {
        ToolChoice::Auto => (json!({"type": "auto"}), None),
        ToolChoice::None => (json!({"type": "none"}), None),
        ToolChoice::Required if forced_ok => (json!({"type": "any"}), None),
        ToolChoice::Required => (
            json!({"type": "auto"}),
            Some("Ответь вызовом одного из доступных инструментов.".to_owned()),
        ),
        ToolChoice::Named(n) => {
            if !req.tools.iter().any(|t| &t.name == n) {
                return Err(bad_request(format!(
                    "tool_choice ссылается на неизвестный инструмент {n}"
                )));
            }
            if forced_ok {
                (json!({"type": "tool", "name": n}), None)
            } else {
                (
                    json!({"type": "auto"}),
                    Some(format!("Ответь вызовом инструмента `{n}`.")),
                )
            }
        }
    })
}

/// Инструменты в порядке и нормализации `render_tools` (CTX-001); с `cache` — метка
/// кэша на последнем.
fn tools(req: &ChatRequest, strict: bool, cache: bool) -> Result<Value, ModelError> {
    let mut seen = HashSet::new();
    if let Some(dup) = req.tools.iter().find(|t| !seen.insert(t.name.as_str())) {
        return Err(bad_request(format!(
            "инструмент {} описан дважды",
            dup.name
        )));
    }
    let rendered = render_tools(&PromptBlocks {
        tools: req.tools.clone(),
        ..PromptBlocks::default()
    });
    let Value::Array(arr) = serde_json::from_str(&rendered)
        .map_err(|e| bad_request(format!("описания инструментов не JSON: {e}")))?
    else {
        return Err(bad_request("описания инструментов не массив"));
    };
    let mut budget = StrictBudget::default();
    let mut out: Vec<Value> = arr
        .into_iter()
        .map(|t| {
            let schema = t.get("parameters").cloned().unwrap_or_else(|| json!({}));
            let name = t.get("name").cloned().unwrap_or(Value::Null);
            let mut o = json!({
                "name": name,
                "description": t.get("description").cloned().unwrap_or(Value::Null),
                "input_schema": schema,
            });
            if strict {
                if strict_compatible_root(&o["input_schema"])
                    && budget.take(count_unions(&o["input_schema"]))
                {
                    o["strict"] = json!(true);
                } else {
                    tracing::debug!(
                        tool = name.as_str().unwrap_or_default(),
                        "схема несовместима со strict-режимом Anthropic, strict не отправляется"
                    );
                }
            }
            o
        })
        .collect();
    if cache && let Some(last) = out.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    Ok(Value::Array(out))
}

/// Пределы strict-режима на запрос (Structured outputs, «JSON Schema limitations»):
/// не больше 20 strict-инструментов и 16 параметров-объединений (`anyOf`, массив
/// типов) во всех strict-схемах.
#[derive(Debug, Default)]
struct StrictBudget {
    tools: usize,
    unions: usize,
}

impl StrictBudget {
    const MAX_TOOLS: usize = 20;
    const MAX_UNIONS: usize = 16;

    fn take(&mut self, unions: usize) -> bool {
        if self.tools < Self::MAX_TOOLS && self.unions + unions <= Self::MAX_UNIONS {
            self.tools += 1;
            self.unions += unions;
            true
        } else {
            false
        }
    }
}

/// Ключи подсхемы, допустимые в strict-режиме Anthropic: белый список по разделу
/// «JSON Schema limitations» (Structured outputs). Числовые (`minimum`, `maximum`,
/// `multipleOf`) и строковые (`minLength`, `maxLength`) ограничения не поддерживаются,
/// `oneOf`, `not`, `if`/`then`/`else`, `patternProperties` и прочее не названо — схема
/// с ними несовместима.
const STRICT_KEYS: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "anyOf",
    "allOf",
    "$ref",
    "$defs",
    "definitions",
    "description",
    "title",
    "default",
    "pattern",
    "format",
    "minItems",
];

/// Поддерживаемые строковые форматы.
const STRICT_FORMATS: &[&str] = &[
    "date-time",
    "time",
    "date",
    "duration",
    "email",
    "hostname",
    "uri",
    "ipv4",
    "ipv6",
    "uuid",
];

/// Схема совместима со strict-режимом Anthropic. Проверка консервативна (лишний
/// отказ безопасен: схема уходит без `strict`): корень — объект; во всех подсхемах
/// только [`STRICT_KEYS`]; у каждого объекта `additionalProperties: false` и `required`
/// со всеми ключами `properties` (необязательные параметры ограничены 24 на запрос);
/// `$ref` только локальный и не внутри `$defs`/`definitions` (рекурсия не
/// поддерживается); `allOf` без `$ref`; `enum` только из простых значений;
/// `minItems` 0 или 1; `format` из списка. Схема не переписывается.
fn strict_compatible_root(s: &Value) -> bool {
    let Value::Object(m) = s else {
        return false;
    };
    m.get("type") == Some(&json!("object")) && strict_compatible(s, false)
}

fn strict_compatible(s: &Value, in_defs: bool) -> bool {
    let Value::Object(m) = s else {
        return false;
    };
    if !m.keys().all(|k| STRICT_KEYS.contains(&k.as_str())) {
        return false;
    }
    if let Some(r) = m.get("$ref")
        && (in_defs || !r.as_str().is_some_and(|r| r.starts_with('#')))
    {
        return false;
    }
    if let Some(e) = m.get("enum")
        && !e
            .as_array()
            .is_some_and(|xs| xs.iter().all(|x| !x.is_object() && !x.is_array()))
    {
        return false;
    }
    if let Some(n) = m.get("minItems")
        && !matches!(n.as_u64(), Some(0 | 1))
    {
        return false;
    }
    if let Some(f) = m.get("format")
        && !f.as_str().is_some_and(|f| STRICT_FORMATS.contains(&f))
    {
        return false;
    }
    let object = match m.get("type") {
        Some(Value::String(t)) => t == "object",
        Some(Value::Array(ts)) => ts.iter().any(|t| t == "object"),
        _ => m.contains_key("properties"),
    };
    if object {
        if m.get("additionalProperties") != Some(&Value::Bool(false)) {
            return false;
        }
        let empty = Map::new();
        let props = match m.get("properties") {
            None => &empty,
            Some(Value::Object(p)) => p,
            Some(_) => return false,
        };
        let required: HashSet<&str> = m
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if !props.keys().all(|k| required.contains(k.as_str())) {
            return false;
        }
        if !props.values().all(|p| strict_compatible(p, in_defs)) {
            return false;
        }
    }
    if let Some(items) = m.get("items")
        && !strict_compatible(items, in_defs)
    {
        return false;
    }
    for k in ["anyOf", "allOf"] {
        if let Some(any) = m.get(k) {
            let Value::Array(xs) = any else {
                return false;
            };
            // `allOf` вместе с `$ref` не поддерживается.
            if k == "allOf" && xs.iter().any(|x| x.get("$ref").is_some()) {
                return false;
            }
            if !xs.iter().all(|x| strict_compatible(x, in_defs)) {
                return false;
            }
        }
    }
    for k in ["$defs", "definitions"] {
        if let Some(defs) = m.get(k) {
            let Value::Object(d) = defs else {
                return false;
            };
            if !d.values().all(|x| strict_compatible(x, true)) {
                return false;
            }
        }
    }
    true
}

/// Число параметров-объединений в схеме: `anyOf` и массив в `type`.
fn count_unions(s: &Value) -> usize {
    match s {
        Value::Object(m) => {
            let own = usize::from(m.contains_key("anyOf"))
                + usize::from(m.get("type").is_some_and(Value::is_array));
            own + m.values().map(count_unions).sum::<usize>()
        }
        Value::Array(xs) => xs.iter().map(count_unions).sum(),
        _ => 0,
    }
}

/// Блок содержимого ответа по `index` события.
#[derive(Debug)]
enum Block {
    Text(String),
    Thinking {
        thinking: String,
        signature: String,
    },
    /// `redacted_thinking` — целиком, как пришёл.
    Redacted(Value),
    ToolUse {
        /// Номер вызова в сообщении (0, 1, …).
        call: u32,
        id: String,
        /// Пришёл хотя бы один непустой `partial_json`.
        streamed: bool,
        /// `input` из `content_block_start`, если он не пустой.
        initial: Option<Value>,
    },
    /// Блок, который коннектор не переводит (`server_tool_use` и т. п.).
    Other,
}

/// Декодер стрима Anthropic Messages. `Finished` — на `message_stop`.
#[derive(Debug)]
pub struct AnthropicDecoder {
    /// Модель запроса: для `ReplayBlob`.
    model: String,
    asm: MessageAssembler,
    usage: Usage,
    blocks: BTreeMap<u32, Block>,
    calls: u32,
    finish: Option<FinishReason>,
    done: bool,
}

impl AnthropicDecoder {
    pub fn new(model: String) -> Self {
        Self {
            model,
            asm: MessageAssembler::new(),
            usage: Usage::default(),
            blocks: BTreeMap::new(),
            calls: 0,
            finish: None,
            done: false,
        }
    }

    /// `Finished` уже отправлен.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Событие SSE → события стрима. Данные не JSON — `Malformed`; событие `error` —
    /// ошибка по `error.type` (`overloaded_error` → `Overloaded`).
    pub fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<StreamEvent>, ModelError> {
        if self.done {
            return Ok(Vec::new());
        }
        let data = ev.data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let v: Value = serde_json::from_str(data).map_err(|e| {
            ModelError::new(
                ErrorClass::Malformed,
                format!("событие Anthropic не JSON: {e}"),
            )
        })?;
        let kind = v
            .get("type")
            .and_then(Value::as_str)
            .or(ev.event.as_deref())
            .unwrap_or_default();
        let mut out = Vec::new();
        match kind {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    self.read_usage(u);
                }
            }
            "content_block_start" => self.block_start(&v, &mut out),
            "content_block_delta" => self.block_delta(&v, &mut out),
            "content_block_stop" => self.block_stop(&v, &mut out),
            "message_delta" => {
                if let Some(r) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.finish = Some(finish_reason(r));
                    if r == "refusal" {
                        let details = v
                            .pointer("/delta/stop_details")
                            .map(Value::to_string)
                            .unwrap_or_default();
                        tracing::info!(
                            details = details.as_str(),
                            "модель отказалась отвечать (stop_reason refusal)"
                        );
                    }
                }
                if let Some(u) = v.get("usage") {
                    self.read_usage(u);
                }
            }
            "message_stop" => out.push(self.finished()),
            "ping" => {}
            "error" => return Err(stream_error(v.get("error").unwrap_or(&Value::Null))),
            other => tracing::debug!(event = other, "событие Anthropic пропущено"),
        }
        for e in &out {
            self.asm.on_event(e);
        }
        Ok(out)
    }

    /// Тело кончилось без `message_stop` — обрыв.
    pub fn finish_at_eof(&mut self) -> Result<Vec<StreamEvent>, ModelError> {
        if self.done {
            return Ok(Vec::new());
        }
        Err(ModelError::new(
            ErrorClass::StreamCut,
            "стрим Anthropic закрыт до message_stop",
        ))
    }

    /// Тело кончилось на строке без перевода строки: законченный `message_stop` —
    /// штатный конец, иначе `cut`.
    pub fn on_unterminated(
        &mut self,
        ev: &SseEvent,
        cut: ModelError,
    ) -> Result<Vec<StreamEvent>, ModelError> {
        if serde_json::from_str::<Value>(ev.data.trim()).is_err() {
            return Err(cut);
        }
        let out = self.on_event(ev)?;
        if self.done { Ok(out) } else { Err(cut) }
    }

    fn index(v: &Value) -> u32 {
        u64_at(v, &["index"]).map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
    }

    fn block_start(&mut self, v: &Value, out: &mut Vec<StreamEvent>) {
        let index = Self::index(v);
        let cb = v.get("content_block").unwrap_or(&Value::Null);
        let s = |k: &str| cb.get(k).and_then(Value::as_str).unwrap_or_default();
        let block = match s("type") {
            "text" => {
                let t = s("text");
                if !t.is_empty() {
                    out.push(StreamEvent::TextDelta(t.to_owned()));
                }
                Block::Text(t.to_owned())
            }
            "thinking" => {
                let t = s("thinking");
                if !t.is_empty() {
                    out.push(StreamEvent::ReasoningDelta(t.to_owned()));
                }
                Block::Thinking {
                    thinking: t.to_owned(),
                    signature: s("signature").to_owned(),
                }
            }
            "redacted_thinking" => Block::Redacted(cb.clone()),
            "tool_use" => {
                let call = self.calls;
                self.calls += 1;
                out.push(StreamEvent::ToolCallStart {
                    index: call,
                    id: s("id").to_owned(),
                    name: s("name").to_owned(),
                });
                let initial = cb
                    .get("input")
                    .filter(|i| i.as_object().is_some_and(|o| !o.is_empty()))
                    .cloned();
                Block::ToolUse {
                    call,
                    id: s("id").to_owned(),
                    streamed: false,
                    initial,
                }
            }
            other => {
                tracing::debug!(block = other, "блок ответа Anthropic не переводится");
                Block::Other
            }
        };
        self.blocks.insert(index, block);
    }

    fn block_delta(&mut self, v: &Value, out: &mut Vec<StreamEvent>) {
        let index = Self::index(v);
        let d = v.get("delta").unwrap_or(&Value::Null);
        let s = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or_default();
        match (self.blocks.get_mut(&index), s("type")) {
            (Some(Block::Text(buf)), "text_delta") => {
                let t = s("text");
                if !t.is_empty() {
                    buf.push_str(t);
                    out.push(StreamEvent::TextDelta(t.to_owned()));
                }
            }
            (Some(Block::Thinking { thinking, .. }), "thinking_delta") => {
                let t = s("thinking");
                if !t.is_empty() {
                    thinking.push_str(t);
                    out.push(StreamEvent::ReasoningDelta(t.to_owned()));
                }
            }
            (Some(Block::Thinking { signature, .. }), "signature_delta") => {
                signature.push_str(s("signature"));
            }
            (Some(Block::ToolUse { call, streamed, .. }), "input_json_delta") => {
                let f = s("partial_json");
                if !f.is_empty() {
                    *streamed = true;
                    out.push(StreamEvent::ToolCallArgs {
                        index: *call,
                        fragment: f.to_owned(),
                    });
                }
            }
            (_, kind) => tracing::debug!(index, delta = kind, "дельта Anthropic пропущена"),
        }
    }

    /// Вызов без фрагментов аргументов, но с `input` в начале блока: аргументы — он.
    fn block_stop(&mut self, v: &Value, out: &mut Vec<StreamEvent>) {
        if let Some(Block::ToolUse {
            call,
            streamed: false,
            initial: Some(input),
            ..
        }) = self.blocks.get(&Self::index(v))
        {
            out.push(StreamEvent::ToolCallArgs {
                index: *call,
                fragment: input.to_string(),
            });
        }
    }

    fn read_usage(&mut self, u: &Value) {
        if let Some(n) = u64_at(u, &["input_tokens"]) {
            self.usage.input = n;
        }
        if let Some(n) = u64_at(u, &["cache_read_input_tokens"]) {
            self.usage.cached = n;
        }
        if let Some(n) = u64_at(u, &["cache_creation_input_tokens"]) {
            self.usage.cache_write = n;
        }
        if let Some(n) = u64_at(u, &["output_tokens"]) {
            self.usage.output = n;
        }
    }

    /// Данные повтора: блоки ответа в исходном порядке — `thinking` и
    /// `redacted_thinking` целиком, текст целиком, вызов — ссылкой по `id`;
    /// `signature` — подпись первого блока `thinking`. `None`, если мышления не было.
    fn replay(&self) -> Option<ReplayBlob> {
        let mut layout = Vec::new();
        let mut signature = Value::Null;
        let mut thought = false;
        for b in self.blocks.values() {
            match b {
                Block::Text(t) => layout.push(text_block(t)),
                Block::Thinking {
                    thinking,
                    signature: sig,
                } => {
                    thought = true;
                    if signature.is_null() {
                        signature = json!(sig);
                    }
                    layout.push(json!({"type": "thinking", "thinking": thinking,
                                       "signature": sig}));
                }
                Block::Redacted(v) => {
                    thought = true;
                    layout.push(v.clone());
                }
                Block::ToolUse { id, .. } => layout.push(json!({"type": "tool_use", "id": id})),
                Block::Other => {}
            }
        }
        thought.then(|| ReplayBlob {
            wire: Wire::AnthropicMessages,
            model: self.model.clone(),
            data: json!({"signature": signature, "blocks": layout}),
        })
    }

    /// Итоговое событие; дальше декодер ничего не отдаёт.
    fn finished(&mut self) -> StreamEvent {
        self.done = true;
        let mut asm = std::mem::take(&mut self.asm);
        if let Some(blob) = self.replay() {
            asm.set_reasoning_replay(blob);
        }
        let finish = self.finish.take().unwrap_or_else(|| {
            tracing::warn!("стрим Anthropic закончен без stop_reason");
            FinishReason::Other("missing".to_owned())
        });
        StreamEvent::Finished {
            message: asm.finish(),
            finish,
            usage: std::mem::take(&mut self.usage),
        }
    }
}

impl StreamDecoder for AnthropicDecoder {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<StreamEvent>, ModelError> {
        AnthropicDecoder::on_event(self, ev)
    }
    fn finish_at_eof(&mut self) -> Result<Vec<StreamEvent>, ModelError> {
        AnthropicDecoder::finish_at_eof(self)
    }
    fn on_unterminated(
        &mut self,
        ev: &SseEvent,
        cut: ModelError,
    ) -> Result<Vec<StreamEvent>, ModelError> {
        AnthropicDecoder::on_unterminated(self, ev, cut)
    }
    fn is_done(&self) -> bool {
        AnthropicDecoder::is_done(self)
    }
}

/// `stop_reason` → причина завершения. `refusal` — отказ модели, не ошибка;
/// `pause_turn` (пауза серверного цикла инструментов) и прочие — как есть.
fn finish_reason(r: &str) -> FinishReason {
    match r {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "tool_use" => FinishReason::ToolCalls,
        "max_tokens" => FinishReason::Length,
        "refusal" => FinishReason::Refusal,
        other => FinishReason::Other(other.to_owned()),
    }
}

/// Событие `error` в стриме: класс по `error.type` (типы ошибок API Anthropic и их
/// HTTP-статусы), неизвестный тип — `Server`.
fn stream_error(err: &Value) -> ModelError {
    let status = match err.get("type").and_then(Value::as_str).unwrap_or_default() {
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "billing_error" => 402,
        "permission_error" => 403,
        "not_found_error" => 404,
        "request_too_large" => 413,
        "rate_limit_error" => 429,
        "timeout_error" => 504,
        "overloaded_error" => 529,
        _ => 500,
    };
    let mut e = classify_status(status, &HeaderMap::new(), &err.to_string());
    e.message = format!("ошибка в стриме: {}", e.message);
    // Статус ответа был 2xx: код ошибки — из тела.
    e.http_status = None;
    e
}
