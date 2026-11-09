//! Chat Completions: llama.cpp, vLLM, OpenRouter, OpenAI и произвольный совместимый
//! сервер (MDL-001, MDL-002).
//!
//! Тело запроса собирается из канонического [`ChatRequest`] с поправками диалекта,
//! стрим SSE переводится в [`StreamEvent`] декодером [`ChatDecoder`]. Итоговое
//! сообщение собирает общий [`MessageAssembler`] (D35).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::{MessageAssembler, OpenedStream, STREAM_CHANNEL, WireCtx};
use crate::caps::{ReasoningField, Wire};
use crate::error::{ErrorClass, ModelError};
use crate::http::{
    HttpTransport, Secret, SseEvent, SseResponse, classify_status, parse_rate_limits, redact,
};
use crate::prefix::{PromptBlocks, render_tools};
use crate::types::{
    ChatRequest, CostSource, Effort, FinishReason, Part, ReasoningMode, ReplayBlob, Role,
    StreamEvent, ToolCall, ToolChoice, Usage,
};

/// Диалект Chat Completions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatDialect {
    LlamaCpp,
    Vllm,
    OpenRouter,
    OpenAi,
    /// Произвольный OpenAI-совместимый сервер.
    Generic,
}

/// Коннектор Chat Completions.
#[derive(Debug, Clone)]
pub struct ChatWire {
    pub dialect: ChatDialect,
    /// llama.cpp — корень сервера, прочие — корень версии API (D37).
    pub base_url: String,
    pub model: String,
}

impl ChatWire {
    /// Адрес запроса (D37): llama.cpp — `{base}/v1/chat/completions`,
    /// прочие — `{base}/chat/completions`.
    pub fn url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.dialect {
            ChatDialect::LlamaCpp => format!("{base}/v1/chat/completions"),
            _ => format!("{base}/chat/completions"),
        }
    }

    /// Заголовки запроса: `Authorization: Bearer <ключ>`, если ключ задан;
    /// у OpenRouter — `X-Title: zero-agent`.
    pub fn headers(&self, ctx: &WireCtx<'_>) -> Result<HeaderMap, ModelError> {
        let mut h = HeaderMap::new();
        if let Some(s) = ctx.secret {
            h.insert(AUTHORIZATION, s.header_value("Bearer ")?);
        }
        if self.dialect == ChatDialect::OpenRouter {
            h.insert(
                HeaderName::from_static("x-title"),
                HeaderValue::from_static("zero-agent"),
            );
        }
        Ok(h)
    }

    /// Тело запроса. `ToolChoice::Required` без `caps.tool_choice_required`,
    /// принудительный выбор без инструментов, неизвестное имя в `ToolChoice::Named` и
    /// повтор имени инструмента — `BadRequest` до отправки.
    pub fn build_body(&self, req: &ChatRequest, ctx: &WireCtx<'_>) -> Result<Value, ModelError> {
        if req.tool_choice == ToolChoice::Required && !ctx.caps.tool_choice_required {
            return Err(bad_request(
                "провайдер не поддерживает tool_choice = required",
            ));
        }
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("messages".into(), Value::Array(self.messages(req, ctx)));
        if req.tools.is_empty() {
            if matches!(req.tool_choice, ToolChoice::Required | ToolChoice::Named(_)) {
                return Err(bad_request("принудительный tool_choice без инструментов"));
            }
        } else {
            body.insert("tools".into(), tools(req, ctx.caps.strict_schema)?);
            body.insert("tool_choice".into(), tool_choice(req)?);
            body.insert("parallel_tool_calls".into(), json!(ctx.caps.parallel_tools));
        }
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({"include_usage": true}));
        if self.dialect == ChatDialect::OpenRouter {
            // Стоимость `usage.cost` в стриме (MDL-020, D16).
            body.insert("usage".into(), json!({"include": true}));
        }
        if let Some(n) = req.max_output_tokens {
            let key = match self.dialect {
                ChatDialect::OpenAi => "max_completion_tokens",
                _ => "max_tokens",
            };
            body.insert(key.into(), json!(n));
        }
        let s = &req.sampling;
        if let Some(t) = s.temperature {
            body.insert("temperature".into(), f32_json(t));
        }
        if let Some(p) = s.top_p {
            body.insert("top_p".into(), f32_json(p));
        }
        if let Some(seed) = s.seed {
            body.insert("seed".into(), json!(seed));
        }
        if let Some(p) = s.presence_penalty {
            body.insert("presence_penalty".into(), f32_json(p));
        }
        if !s.stop.is_empty() {
            body.insert("stop".into(), json!(s.stop));
        }
        if let (ChatDialect::OpenAi, Some(k)) = (self.dialect, &req.cache_key) {
            body.insert("prompt_cache_key".into(), json!(k));
        }
        if self.dialect == ChatDialect::LlamaCpp {
            if let Some(slot) = ctx.slot {
                body.insert("id_slot".into(), json!(slot));
            }
            body.insert("cache_prompt".into(), json!(true));
            body.insert("return_progress".into(), json!(true));
            body.insert("reasoning_format".into(), json!("auto"));
        }
        self.reasoning(&mut body, &req.reasoning);
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
        let decoder = ChatDecoder::new(self.dialect).with_model(self.model.clone());
        tokio::spawn(pump(sse, decoder, tx, ctx.secret.cloned()));
        Ok(OpenedStream { limits, events })
    }

    /// Сообщения: system первым и единственным (CTX-004), результаты инструментов —
    /// отдельными сообщениями `tool`.
    fn messages(&self, req: &ChatRequest, ctx: &WireCtx<'_>) -> Vec<Value> {
        let mut out = Vec::new();
        if !req.system.is_empty() {
            out.push(json!({"role": "system", "content": req.system}));
        }
        for m in &req.messages {
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut details: Option<&Value> = None;
            let mut calls = Vec::new();
            let mut has_results = false;
            for p in &m.parts {
                match p {
                    Part::Text(t) => text.push_str(t),
                    Part::Reasoning { text: r, replay } => {
                        reasoning.push_str(r);
                        if let Some(b) = replay.as_ref().filter(|b| self.replay_matches(b)) {
                            details = Some(&b.data);
                        }
                    }
                    Part::ToolCall(c) => calls.push(json!({
                        "id": c.id,
                        "type": "function",
                        "function": {"name": c.name, "arguments": arguments_string(c)},
                    })),
                    Part::ToolResult {
                        call_id, content, ..
                    } => {
                        has_results = true;
                        out.push(
                            json!({"role": "tool", "tool_call_id": call_id, "content": content}),
                        );
                    }
                }
            }
            let role = match m.role {
                Role::Assistant => "assistant",
                // Текст в сообщении результатов инструментов уходит от пользователя.
                Role::User | Role::Tool => "user",
            };
            let emit = match m.role {
                Role::Tool => !text.is_empty(),
                Role::User | Role::Assistant => {
                    !text.is_empty() || !calls.is_empty() || !has_results
                }
            };
            if !emit {
                continue;
            }
            let mut msg = Map::new();
            msg.insert("role".into(), json!(role));
            if text.is_empty() && !calls.is_empty() {
                msg.insert("content".into(), Value::Null);
            } else {
                msg.insert("content".into(), json!(text));
            }
            if !calls.is_empty() {
                msg.insert("tool_calls".into(), Value::Array(calls));
            }
            if m.role == Role::Assistant && ctx.caps.reasoning_roundtrip {
                match details {
                    Some(d) if self.dialect == ChatDialect::OpenRouter => {
                        msg.insert("reasoning_details".into(), d.clone());
                    }
                    _ if !reasoning.is_empty() => match ctx.caps.reasoning_field {
                        ReasoningField::ReasoningContent => {
                            msg.insert("reasoning_content".into(), json!(reasoning));
                        }
                        ReasoningField::Reasoning => {
                            msg.insert("reasoning".into(), json!(reasoning));
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            out.push(Value::Object(msg));
        }
        out
    }

    /// Данные повтора действительны только для того же протокола и той же модели.
    fn replay_matches(&self, b: &ReplayBlob) -> bool {
        b.wire == Wire::ChatCompletions && b.model == self.model
    }

    /// Поля режима рассуждения по диалекту (D36).
    fn reasoning(&self, body: &mut Map<String, Value>, mode: &ReasoningMode) {
        use ChatDialect as D;
        let no_thinking = || json!({"enable_thinking": false});
        match (self.dialect, mode) {
            (_, ReasoningMode::Default) => {}
            (D::LlamaCpp, ReasoningMode::Off) => {
                body.insert("chat_template_kwargs".into(), no_thinking());
                body.insert("reasoning_effort".into(), json!("none"));
            }
            (D::LlamaCpp, ReasoningMode::Budget(n)) => {
                // llama-server принимает бюджет на запрос: `reasoning_budget_tokens`
                // (синоним `thinking_budget_tokens`), иначе флаг `--reasoning-budget`.
                body.insert("reasoning_budget_tokens".into(), json!(n));
            }
            (D::Vllm | D::Generic, ReasoningMode::Off) => {
                body.insert("chat_template_kwargs".into(), no_thinking());
            }
            (D::OpenAi | D::OpenRouter, ReasoningMode::Off) => {
                // Как у Responses (D36): `none` понимают не все модели, а модели без
                // рассуждений (и с обязательным рассуждением у OpenRouter) отвечают 400
                // на само поле. Поле не отправляется; отсечение по профилю — задача 9.
                tracing::debug!(
                    "ReasoningMode::Off у облачного Chat Completions: поле не отправляется"
                );
            }
            (D::OpenRouter, ReasoningMode::Effort(e)) => {
                // OpenRouter принимает и `max`.
                body.insert("reasoning".into(), json!({"effort": e.as_str()}));
            }
            (D::OpenRouter, ReasoningMode::Budget(n)) => {
                body.insert("reasoning".into(), json!({"max_tokens": n}));
            }
            (D::LlamaCpp | D::Vllm | D::Generic | D::OpenAi, ReasoningMode::Effort(e)) => {
                body.insert("reasoning_effort".into(), json!(effort_no_max(*e)));
            }
            (D::Vllm | D::Generic | D::OpenAi, ReasoningMode::Budget(n)) => {
                body.insert(
                    "reasoning_effort".into(),
                    json!(effort_no_max(Effort::nearest(*n))),
                );
            }
        }
    }
}

/// `reasoning_effort` OpenAI-совместимых серверов не знает `max`: он отправляется как `high`.
fn effort_no_max(e: Effort) -> &'static str {
    match e {
        Effort::Max => Effort::High.as_str(),
        e => e.as_str(),
    }
}

/// Инструменты в порядке и нормализации `render_tools` (CTX-001).
fn tools(req: &ChatRequest, strict: bool) -> Result<Value, ModelError> {
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
    let out = arr
        .into_iter()
        .map(|t| {
            let mut f = Map::new();
            for k in ["name", "description", "parameters"] {
                f.insert(k.into(), t.get(k).cloned().unwrap_or(Value::Null));
            }
            if strict {
                if t.get("parameters").is_some_and(strict_compatible_root) {
                    f.insert("strict".into(), json!(true));
                } else {
                    let name = t.get("name").and_then(Value::as_str).unwrap_or_default();
                    tracing::debug!(
                        tool = name,
                        "схема несовместима со strict-режимом, strict не отправляется"
                    );
                }
            }
            json!({"type": "function", "function": f})
        })
        .collect();
    Ok(Value::Array(out))
}

/// Ключи подсхемы, допустимые в strict-режиме OpenAI (Structured Outputs, «Supported
/// schemas»): белый список. Любой другой ключ (`oneOf`, `allOf`, `not`, `if`/`then`/
/// `else`, `patternProperties`, `dependent*`, `minLength` и т. п.) — схема несовместима.
const STRICT_KEYS: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "anyOf",
    "$defs",
    "definitions",
    "$ref",
    "description",
    "title",
    "pattern",
    "format",
    "multipleOf",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minItems",
    "maxItems",
];

/// Предел вложенности объектов в strict-режиме OpenAI.
const STRICT_MAX_DEPTH: usize = 10;

/// Схема совместима со strict-режимом OpenAI. Корень — объект (не `anyOf`). Во всех
/// подсхемах только ключи [`STRICT_KEYS`]; у каждой есть `type`, `$ref` (локальный),
/// `anyOf`, `enum` или `const`; у каждого объекта `additionalProperties: false`, а
/// `required` перечисляет все ключи `properties`; `items` — одна схема, не кортеж;
/// вложенность объектов не больше 10. Проверка консервативна: лишний отказ безопасен,
/// схема просто уходит без `strict`. Схема не переписывается.
fn strict_compatible_root(s: &Value) -> bool {
    s.as_object()
        .is_some_and(|m| is_object_schema(m) && !m.contains_key("anyOf"))
        && strict_compatible(s, 1)
}

fn is_object_schema(m: &Map<String, Value>) -> bool {
    let typed = match m.get("type") {
        Some(Value::String(t)) => t == "object",
        Some(Value::Array(ts)) => ts.iter().any(|t| t == "object"),
        _ => false,
    };
    typed || m.contains_key("properties")
}

/// `depth` — число объектов на пути от корня, включая этот.
fn strict_compatible(s: &Value, depth: usize) -> bool {
    let Value::Object(m) = s else {
        return false;
    };
    if !m.keys().all(|k| STRICT_KEYS.contains(&k.as_str())) {
        return false;
    }
    if !["type", "$ref", "anyOf", "enum", "const"]
        .iter()
        .any(|k| m.contains_key(*k))
    {
        return false;
    }
    if let Some(r) = m.get("$ref")
        && !r.as_str().is_some_and(|r| r.starts_with('#'))
    {
        return false;
    }
    let object = is_object_schema(m);
    if object {
        if depth > STRICT_MAX_DEPTH || m.get("additionalProperties") != Some(&Value::Bool(false)) {
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
        if !props.values().all(|p| strict_compatible(p, depth + 1)) {
            return false;
        }
    }
    // Подсхемы `items` и `anyOf` — на том же уровне вложенности объектов.
    if let Some(items) = m.get("items")
        && !strict_compatible(items, depth)
    {
        return false;
    }
    if let Some(any) = m.get("anyOf") {
        let Value::Array(xs) = any else {
            return false;
        };
        if !xs.iter().all(|x| strict_compatible(x, depth)) {
            return false;
        }
    }
    for k in ["$defs", "definitions"] {
        if let Some(defs) = m.get(k) {
            let Value::Object(d) = defs else {
                return false;
            };
            if !d.values().all(|x| strict_compatible(x, 1)) {
                return false;
            }
        }
    }
    true
}

fn tool_choice(req: &ChatRequest) -> Result<Value, ModelError> {
    Ok(match &req.tool_choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Named(n) => {
            if !req.tools.iter().any(|t| &t.name == n) {
                return Err(bad_request(format!(
                    "tool_choice ссылается на неизвестный инструмент {n}"
                )));
            }
            json!({"type": "function", "function": {"name": n}})
        }
    })
}

/// Аргументы вызова строкой JSON. Не разобранные аргументы (`Null`) возвращаются
/// провайдеру так, как он их прислал.
fn arguments_string(c: &ToolCall) -> String {
    match (&c.arguments, &c.raw_arguments) {
        (Value::Null, Some(raw)) => raw.clone(),
        (Value::Null, None) => "{}".to_owned(),
        (v, _) => v.to_string(),
    }
}

/// `f32` в JSON без хвоста двоичного представления (`0.1`, а не `0.10000000149…`).
fn f32_json(v: f32) -> Value {
    v.to_string()
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map_or(Value::Null, Value::Number)
}

fn bad_request(m: impl Into<String>) -> ModelError {
    ModelError::new(ErrorClass::BadRequest, m)
}

/// Насос SSE: читает стрим, пишет события в канал и кончается по `tx.closed()` (D2).
async fn pump(
    mut sse: SseResponse,
    mut dec: ChatDecoder,
    tx: mpsc::Sender<Result<StreamEvent, ModelError>>,
    secret: Option<Secret>,
) {
    loop {
        let step = tokio::select! {
            biased;
            () = tx.closed() => {
                tracing::debug!("приёмник стрима закрыт, соединение закрывается");
                return;
            }
            r = sse.next_event() => r,
        };
        let result = match step {
            Ok(Some(ev)) => dec.on_event(&ev),
            Ok(None) => dec.finish_at_eof(),
            Err(cut) if cut.class == ErrorClass::StreamCut => match sse.take_unterminated() {
                Some(ev) => dec.on_unterminated(&ev, cut),
                None => Err(cut),
            },
            Err(e) => Err(e),
        };
        match result {
            Ok(evs) => {
                for ev in evs {
                    if tx.send(Ok(ev)).await.is_err() {
                        return;
                    }
                }
                if dec.is_done() {
                    return;
                }
            }
            Err(mut e) => {
                if let Some(s) = &secret {
                    e.message = redact(&e.message, &[s]);
                }
                let _ = tx.send(Err(e)).await;
                return;
            }
        }
    }
}

/// Начатый вызов инструмента в декодере.
#[derive(Debug)]
struct StartedCall {
    /// `index` в событиях стрима.
    index: u32,
    /// Последний `id` провайдера (или запасной).
    id: String,
    /// `id` запасной: провайдер его ещё не прислал.
    fallback: bool,
}

/// Декодер стрима Chat Completions: переводит чанки в [`StreamEvent`] и собирает
/// итог. `Finished` — на `[DONE]` или по концу стрима после `finish_reason`.
#[derive(Debug)]
pub struct ChatDecoder {
    /// Задел для разбора, зависящего от диалекта; сейчас все поля принимаются при
    /// любом диалекте (бриф: `reasoning_content` и `reasoning` — всегда).
    dialect: ChatDialect,
    /// Модель запроса: для `ReplayBlob` (`reasoning_details` OpenRouter).
    model: String,
    asm: MessageAssembler,
    /// Начатые вызовы по `index` провайдера.
    calls: BTreeMap<u32, StartedCall>,
    /// Индексы событий, уже занятые вызовами.
    used: BTreeSet<u32>,
    usage: Usage,
    /// В `usage` было `prompt_tokens_details.cached_tokens`.
    usage_cached: bool,
    has_usage: bool,
    /// Последний `timings` llama.cpp.
    timings: Option<Value>,
    finish: Option<FinishReason>,
    /// `reasoning_details` OpenRouter, склеенные по `index`.
    details: Vec<Value>,
    done: bool,
}

impl ChatDecoder {
    pub fn new(dialect: ChatDialect) -> Self {
        Self {
            dialect,
            model: String::new(),
            asm: MessageAssembler::new(),
            calls: BTreeMap::new(),
            used: BTreeSet::new(),
            usage: Usage::default(),
            usage_cached: false,
            has_usage: false,
            timings: None,
            finish: None,
            details: Vec::new(),
            done: false,
        }
    }

    /// Модель запроса; без неё `ReplayBlob` берёт имя модели из чанков.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn dialect(&self) -> ChatDialect {
        self.dialect
    }

    /// `Finished` уже отправлен: дальше события не нужны.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Событие SSE → события стрима. Чанк не JSON — `Malformed`; объект `error` в
    /// чанке — ошибка по `error.code` или типу ([`classify_status`]).
    pub fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<StreamEvent>, ModelError> {
        if self.done {
            return Ok(Vec::new());
        }
        let data = ev.data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        if data == "[DONE]" {
            return Ok(vec![self.finished()]);
        }
        let v: Value = serde_json::from_str(data).map_err(|e| {
            ModelError::new(
                ErrorClass::Malformed,
                format!("чанк Chat Completions не JSON: {e}"),
            )
        })?;
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            return Err(stream_error(err));
        }
        if self.model.is_empty()
            && let Some(m) = v.get("model").and_then(Value::as_str)
        {
            self.model = m.to_owned();
        }
        let mut out = Vec::new();
        if let Some(p) = v.get("prompt_progress") {
            out.push(StreamEvent::Progress {
                processed: u32_field(p, "processed"),
                total: u32_field(p, "total"),
            });
        }
        for choice in v
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(delta) = choice.get("delta") {
                self.delta(delta, &mut out);
            }
            if let Some(r) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish = Some(finish_reason(r));
            }
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.read_usage(u);
        }
        if let Some(t) = v.get("timings").filter(|t| t.is_object()) {
            self.timings = Some(t.clone());
        }
        for e in &out {
            self.asm.on_event(e);
        }
        Ok(out)
    }

    /// Тело кончилось штатно. После `finish_reason` — `Finished`, иначе `StreamCut`.
    pub fn finish_at_eof(&mut self) -> Result<Vec<StreamEvent>, ModelError> {
        if self.done {
            return Ok(Vec::new());
        }
        if self.finish.is_some() {
            return Ok(vec![self.finished()]);
        }
        Err(ModelError::new(
            ErrorClass::StreamCut,
            "стрим закрыт до finish_reason",
        ))
    }

    /// Тело кончилось на строке без перевода строки (`cut` — `StreamCut` транспорта).
    /// Законченная строка (`[DONE]` или целый JSON) разбирается; если после неё есть
    /// `finish_reason`, это штатный конец потока, иначе — `cut`.
    pub fn on_unterminated(
        &mut self,
        ev: &SseEvent,
        cut: ModelError,
    ) -> Result<Vec<StreamEvent>, ModelError> {
        let data = ev.data.trim();
        let complete = data == "[DONE]" || serde_json::from_str::<Value>(data).is_ok();
        if !complete {
            return Err(cut);
        }
        let mut out = self.on_event(ev)?;
        if !self.done {
            if self.finish.is_none() {
                return Err(cut);
            }
            out.push(self.finished());
        }
        Ok(out)
    }

    fn delta(&mut self, delta: &Value, out: &mut Vec<StreamEvent>) {
        // vLLM отдаёт `reasoning`, а старые версии — ещё и `reasoning_content` с тем же
        // текстом: берётся одно поле.
        let reasoning = ["reasoning_content", "reasoning"]
            .into_iter()
            .filter_map(|k| delta.get(k).and_then(Value::as_str))
            .find(|s| !s.is_empty());
        if let Some(r) = reasoning {
            out.push(StreamEvent::ReasoningDelta(r.to_owned()));
        }
        if let Some(items) = delta.get("reasoning_details").and_then(Value::as_array) {
            for it in items {
                merge_detail(&mut self.details, it);
            }
        }
        if let Some(t) = delta.get("content").and_then(Value::as_str)
            && !t.is_empty()
        {
            out.push(StreamEvent::TextDelta(t.to_owned()));
        }
        let calls = delta.get("tool_calls").and_then(Value::as_array);
        for (pos, tc) in calls.into_iter().flatten().enumerate() {
            let provider_index = tc
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|i| u32::try_from(i).ok())
                .unwrap_or(u32::try_from(pos).unwrap_or(u32::MAX));
            let f = tc.get("function");
            let new_id = tc
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            // Новый `id` на уже начатом `index` — другой вызов: часть совместимых
            // серверов шлёт все вызовы с `index: 0`. Но если у вызова пока запасной
            // `id`, первый пришедший `id` — его собственный, это продолжение.
            let fresh = match self.calls.get_mut(&provider_index) {
                None => true,
                Some(c) => match new_id {
                    Some(i) if c.fallback => {
                        // `ToolCallStart` уже ушёл с запасным `id`: он и остаётся
                        // в сообщении, пришедший запоминается для сравнения дальше.
                        c.id = i.to_owned();
                        c.fallback = false;
                        false
                    }
                    Some(i) => i != c.id,
                    None => false,
                },
            };
            let index = if fresh {
                let index = if self.used.contains(&provider_index) {
                    self.used.last().map_or(0, |m| m.saturating_add(1))
                } else {
                    provider_index
                };
                self.used.insert(index);
                let id = new_id.map_or_else(|| fallback_id(index), str::to_owned);
                self.calls.insert(
                    provider_index,
                    StartedCall {
                        index,
                        id: id.clone(),
                        fallback: new_id.is_none(),
                    },
                );
                let name = f
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                out.push(StreamEvent::ToolCallStart { index, id, name });
                index
            } else {
                self.calls
                    .get(&provider_index)
                    .map_or(provider_index, |c| c.index)
            };
            if let Some(a) = f.and_then(|f| f.get("arguments")).and_then(Value::as_str)
                && !a.is_empty()
            {
                out.push(StreamEvent::ToolCallArgs {
                    index,
                    fragment: a.to_owned(),
                });
            }
        }
    }

    fn read_usage(&mut self, u: &Value) {
        self.has_usage = true;
        self.usage.input = u64_at(u, &["prompt_tokens"]).unwrap_or(0);
        self.usage.output = u64_at(u, &["completion_tokens"]).unwrap_or(0);
        if let Some(c) = u64_at(u, &["prompt_tokens_details", "cached_tokens"]) {
            self.usage.cached = c;
            self.usage_cached = true;
        }
        self.usage.reasoning =
            u64_at(u, &["completion_tokens_details", "reasoning_tokens"]).unwrap_or(0);
        if let Some(c) = u.get("cost").and_then(Value::as_f64) {
            self.usage.cost_usd = Some(c);
            self.usage.cost_source = CostSource::Provider;
        }
    }

    /// Итоговое событие; дальше декодер ничего не отдаёт.
    fn finished(&mut self) -> StreamEvent {
        self.done = true;
        let mut usage = std::mem::take(&mut self.usage);
        if let Some(t) = &self.timings {
            let cache_n = u64_at(t, &["cache_n"]);
            if !self.usage_cached {
                usage.cached = cache_n.unwrap_or(0);
            }
            if !self.has_usage {
                usage.input = u64_at(t, &["prompt_n"]).unwrap_or(0) + cache_n.unwrap_or(0);
                usage.output = u64_at(t, &["predicted_n"]).unwrap_or(0);
            }
            let ms: Vec<f64> = ["prompt_ms", "predicted_ms"]
                .into_iter()
                .filter_map(|k| t.get(k).and_then(Value::as_f64))
                .collect();
            if !ms.is_empty() {
                let sum = ms.iter().sum::<f64>().max(0.0).round();
                // `as` насыщает: отрицательных после `max(0)` нет.
                usage.gpu_ms = Some(sum as u64);
            }
        }
        let mut asm = std::mem::take(&mut self.asm);
        if !self.details.is_empty() {
            asm.set_reasoning_replay(ReplayBlob {
                wire: Wire::ChatCompletions,
                model: self.model.clone(),
                data: Value::Array(std::mem::take(&mut self.details)),
            });
        }
        let finish = self.finish.take().unwrap_or_else(|| {
            tracing::warn!("стрим Chat Completions закончен без finish_reason");
            FinishReason::Other("missing".to_owned())
        });
        StreamEvent::Finished {
            message: asm.finish(),
            finish,
            usage,
        }
    }
}

/// Фрагмент `reasoning_details`: продолжение последнего элемента с тем же `index` и
/// `type` дописывает строки `text`, `summary`, `data`, прочие поля заменяет;
/// иначе — новый элемент.
fn merge_detail(details: &mut Vec<Value>, item: &Value) {
    if let (Some(Value::Object(last)), Value::Object(new)) = (details.last_mut(), item) {
        let same = |k: &str| new.get(k).is_some() && last.get(k) == new.get(k);
        if same("index") && same("type") {
            for (k, v) in new {
                match (last.get_mut(k), v) {
                    (Some(Value::String(old)), Value::String(add))
                        if matches!(k.as_str(), "text" | "summary" | "data") =>
                    {
                        old.push_str(add);
                    }
                    // `null` в продолжении не стирает уже пришедшее значение (подпись).
                    (Some(old), Value::Null) if !old.is_null() => {}
                    _ => {
                        last.insert(k.clone(), v.clone());
                    }
                }
            }
            return;
        }
    }
    details.push(item.clone());
}

/// `id` вызова, если провайдер его не прислал: уникален в процессе и между ходами
/// (время в наносекундах и счётчик), чтобы `tool_call_id` истории не совпадали.
fn fallback_id(index: u32) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("call_{nanos:x}_{n:x}_{index}")
}

fn finish_reason(r: &str) -> FinishReason {
    match r {
        "stop" => FinishReason::Stop,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_owned()),
    }
}

/// Ошибка внутри стрима `{"error": {...}}`: класс по числовому `code` как по
/// HTTP-статусу, иначе по строковому `code` или `type`; неизвестное — `Server`.
fn stream_error(err: &Value) -> ModelError {
    let code = err.get("code");
    let numeric = code
        .and_then(|c| {
            c.as_u64()
                .or_else(|| c.as_str().and_then(|s| s.parse().ok()))
        })
        .and_then(|c| u16::try_from(c).ok())
        .filter(|c| (400..600).contains(c));
    let status = numeric.unwrap_or_else(|| {
        let names = [code, err.get("type")]
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
            .join(" ");
        if names.contains("context_length") || names.contains("exceed_context") {
            400
        } else if names.contains("rate_limit") {
            429
        } else if names.contains("overloaded") || names.contains("unavailable") {
            503
        } else if names.contains("authentication")
            || names.contains("permission")
            || names.contains("api_key")
        {
            401
        } else if names.contains("not_found") {
            404
        } else if names.contains("invalid_request") {
            400
        } else {
            500
        }
    });
    let mut e = classify_status(status, &HeaderMap::new(), &err.to_string());
    e.message = format!("ошибка в стриме: {}", e.message);
    // Статус ответа был 2xx: код ошибки — из тела.
    e.http_status = None;
    e
}

fn u64_at(v: &Value, path: &[&str]) -> Option<u64> {
    let mut cur = v;
    for k in path {
        cur = cur.get(k)?;
    }
    cur.as_u64()
        .or_else(|| cur.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
}

fn u32_field(v: &Value, k: &str) -> u32 {
    u64_at(v, &[k]).map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
}
