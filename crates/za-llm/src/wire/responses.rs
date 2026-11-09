//! OpenAI Responses (`POST {base}/responses`, MDL-001).
//!
//! Работа без состояния на стороне провайдера: `store: false`, вся история — в `input`,
//! рассуждение возвращается элементами `reasoning` с `encrypted_content`
//! (`include: ["reasoning.encrypted_content"]`). Элемент берётся из
//! `response.output_item.done`: в `response.output_item.added` он может быть неполным
//! (типы SDK `openai-python`, `ResponseOutputItemAddedEvent`). Итоговое сообщение
//! собирает общий [`MessageAssembler`] (D35).

use std::collections::{BTreeMap, HashMap, HashSet};

use reqwest::header::{AUTHORIZATION, HeaderMap};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::chat::{effort_no_max, f32_json, rendered_tools, stream_error, strict_compatible_root};
use super::{
    MessageAssembler, OpenedStream, STREAM_CHANNEL, StreamDecoder, WireCtx, bad_request, pump,
    u64_at,
};
use crate::caps::{ThinkingSupport, Wire};
use crate::error::{ErrorClass, ModelError};
use crate::http::{HttpTransport, Secret, SseEvent, classify_status, parse_rate_limits};
use crate::types::{
    ChatRequest, Effort, FinishReason, Message, Part, ReasoningMode, ReplayBlob, Role, StreamEvent,
    ToolCall, ToolChoice, Usage,
};

/// Коннектор OpenAI Responses.
#[derive(Debug, Clone)]
pub struct ResponsesWire {
    /// Корень версии API, `https://api.openai.com/v1` (D37).
    pub base_url: String,
    pub model: String,
}

impl ResponsesWire {
    /// Адрес запроса: `{base}/responses` (D37).
    pub fn url(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }

    /// Заголовки: `Authorization: Bearer <ключ>`, если ключ задан.
    pub fn headers(&self, ctx: &WireCtx<'_>) -> Result<HeaderMap, ModelError> {
        let mut h = HeaderMap::new();
        if let Some(s) = ctx.secret {
            h.insert(AUTHORIZATION, s.header_value("Bearer ")?);
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
        if !req.system.is_empty() {
            body.insert("instructions".into(), json!(req.system));
        }
        body.insert("input".into(), Value::Array(self.input(req)));
        if req.tools.is_empty() {
            if matches!(req.tool_choice, ToolChoice::Required | ToolChoice::Named(_)) {
                return Err(bad_request("принудительный tool_choice без инструментов"));
            }
        } else {
            body.insert("tools".into(), tools(req, ctx.caps.strict_schema)?);
            body.insert("tool_choice".into(), tool_choice(req)?);
            body.insert("parallel_tool_calls".into(), json!(ctx.caps.parallel_tools));
        }
        if let Some(n) = req.max_output_tokens {
            body.insert("max_output_tokens".into(), json!(n));
        }
        let s = &req.sampling;
        // Рассуждающие модели OpenAI отвечают 400 «Unsupported parameter» на
        // `temperature`/`top_p`: поля уходят только модели, которая их принимает
        // (профиль, задача 9); до зонда — как в запросе.
        let thinking = ctx.thinking;
        if thinking.is_none_or(|t| t.sampling) {
            if let Some(t) = s.temperature {
                body.insert("temperature".into(), f32_json(t));
            }
            if let Some(p) = s.top_p {
                body.insert("top_p".into(), f32_json(p));
            }
        } else if s.temperature.is_some() || s.top_p.is_some() {
            tracing::debug!(
                model = self.model.as_str(),
                "temperature/top_p модель не принимает, не отправляются"
            );
        }
        if s.seed.is_some() || s.presence_penalty.is_some() || !s.stop.is_empty() {
            tracing::debug!("seed/presence_penalty/stop Responses не принимает, не отправляются");
        }
        // Модель без рассуждений (профиль: `effort = false`) отвечает 400 на `reasoning`
        // и на `include: ["reasoning.encrypted_content"]`.
        let reasons = thinking.is_none_or(|t| t.effort);
        match reasoning_effort(&req.reasoning, thinking) {
            Some(effort) if reasons => {
                body.insert(
                    "reasoning".into(),
                    json!({"effort": effort, "summary": "auto"}),
                );
            }
            Some(_) => tracing::debug!(
                model = self.model.as_str(),
                "модель без рассуждений: поле reasoning не отправляется"
            ),
            None => {}
        }
        body.insert("stream".into(), json!(true));
        body.insert("store".into(), json!(false));
        // При `store: false` элементы `reasoning` и так несут `encrypted_content`
        // («Reasoning» → stateless mode: «doesn't require it»); поле оставлено для
        // совместимых реализаций Responses, но только рассуждающим моделям.
        if reasons {
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
        if let Some(k) = &req.cache_key {
            body.insert("prompt_cache_key".into(), json!(k));
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
        let decoder = ResponsesDecoder::new(self.model.clone());
        tokio::spawn(pump(sse, decoder, tx, ctx.secret.cloned()));
        Ok(OpenedStream { limits, events })
    }

    /// Элементы `input`. Результаты инструментов — `function_call_output` в порядке
    /// частей, текст пользователя — `message` после них. Сообщение ассистента —
    /// элементы `reasoning` (из своего [`ReplayBlob`]), `message` и `function_call` в
    /// порядке ответа.
    fn input(&self, req: &ChatRequest) -> Vec<Value> {
        let mut out = Vec::new();
        for m in &req.messages {
            match m.role {
                Role::User | Role::Tool => user_items(m, &mut out),
                Role::Assistant => self.assistant_items(m, &mut out),
            }
        }
        out
    }

    fn assistant_items(&self, m: &Message, out: &mut Vec<Value>) {
        let mut text = String::new();
        let mut calls: Vec<&ToolCall> = Vec::new();
        let mut replay: Option<&Value> = None;
        for p in &m.parts {
            match p {
                Part::Text(t) => text.push_str(t),
                Part::ToolCall(c) => calls.push(c),
                Part::Reasoning { replay: r, .. } => match r {
                    Some(b) if self.replay_matches(b) => {
                        replay.get_or_insert(&b.data);
                    }
                    Some(b) => tracing::debug!(
                        wire = ?b.wire,
                        model = b.model.as_str(),
                        "чужое рассуждение не возвращается Responses: encrypted_content \
                         действителен только для той же модели"
                    ),
                    // Без элемента провайдера рассуждение вернуть нечем: у `reasoning`
                    // обязателен `id`.
                    None => {}
                },
                Part::ToolResult { .. } => {
                    tracing::debug!("результат инструмента в сообщении ассистента пропущен");
                }
            }
        }
        let mut text_sent = text.is_empty();
        let mut sent: HashSet<&str> = HashSet::new();
        let push_text = |out: &mut Vec<Value>, sent: &mut bool| {
            if !*sent {
                out.push(json!({"type": "message", "role": "assistant", "content": text}));
                *sent = true;
            }
        };
        match replay {
            Some(Value::Array(layout)) => {
                // Несколько элементов `reasoning`: порядок ответа, вызовы и текст —
                // на своих местах (за `reasoning` должен идти его элемент).
                for e in layout {
                    match e.get("type").and_then(Value::as_str) {
                        Some("reasoning") => out.push(e.clone()),
                        Some("message") => push_text(out, &mut text_sent),
                        Some("function_call") => {
                            let id = e.get("call_id").and_then(Value::as_str);
                            if let Some(c) = calls.iter().find(|c| Some(c.id.as_str()) == id)
                                && sent.insert(c.id.as_str())
                            {
                                out.push(function_call(c));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some(item) => out.push(item.clone()),
            None => {}
        }
        push_text(out, &mut text_sent);
        for c in calls {
            if sent.insert(c.id.as_str()) {
                out.push(function_call(c));
            }
        }
    }

    /// Данные повтора действительны только для того же протокола и той же модели.
    fn replay_matches(&self, b: &ReplayBlob) -> bool {
        b.wire == Wire::Responses && b.model == self.model
    }
}

/// Сообщение пользователя или результатов инструментов.
fn user_items(m: &Message, out: &mut Vec<Value>) {
    let mut text = String::new();
    let mut has_results = false;
    for p in &m.parts {
        match p {
            Part::Text(t) => text.push_str(t),
            Part::ToolResult {
                call_id, content, ..
            } => {
                has_results = true;
                out.push(json!({"type": "function_call_output", "call_id": call_id,
                                "output": content}));
            }
            Part::Reasoning { .. } | Part::ToolCall(_) => {
                tracing::debug!("рассуждение или вызов в сообщении пользователя пропущены");
            }
        }
    }
    let emit = match m.role {
        Role::Tool => !text.is_empty(),
        _ => !text.is_empty() || !has_results,
    };
    if emit {
        out.push(json!({"type": "message", "role": "user", "content": text}));
    }
}

/// Элемент `function_call`. Аргументы — исходная строка провайдера, если разбор её не
/// изменил (байты истории стабильны для кэша префикса), иначе сериализация.
fn function_call(c: &ToolCall) -> Value {
    let arguments = match (&c.arguments, &c.raw_arguments) {
        (Value::Null, Some(raw)) => raw.clone(),
        (Value::Null, None) => "{}".to_owned(),
        (v, Some(raw)) if serde_json::from_str::<Value>(raw).ok().as_ref() == Some(v) => {
            raw.clone()
        }
        (v, _) => v.to_string(),
    };
    json!({"type": "function_call", "call_id": c.id, "name": c.name, "arguments": arguments})
}

/// `reasoning.effort` (D36): только при `Effort` и `Budget`; `Max` → `high`.
/// `Off` поля не даёт: модели без рассуждений отвечают 400 на само поле, а `none`
/// понимают не все модели; отсечение по профилю — задача 9.
fn reasoning_effort(
    mode: &ReasoningMode,
    support: Option<&ThinkingSupport>,
) -> Option<&'static str> {
    // `Max` — наибольший уровень модели по профилю (`max`, иначе `xhigh`), иначе и до
    // зонда — `high`: не все модели принимают каждое значение («Reasoning»).
    let effort = |e: Effort| match (e, support) {
        (Effort::Max, Some(t)) => t.max_effort(),
        (e, _) => effort_no_max(e),
    };
    match mode {
        ReasoningMode::Default => None,
        ReasoningMode::Off => {
            tracing::debug!("ReasoningMode::Off у Responses: поле reasoning не отправляется");
            None
        }
        ReasoningMode::Effort(e) => Some(effort(*e)),
        ReasoningMode::Budget(n) => Some(effort(Effort::nearest(*n))),
    }
}

/// Инструменты Responses: `{type, name, description, parameters, strict}`. `strict`
/// всегда явный: `true` только для схемы из белого списка strict-режима, иначе
/// `false` — без поля Responses сам нормализует схему в strict, когда может
/// (руководство «Function calling»), и необязательные поля становятся обязательными.
fn tools(req: &ChatRequest, strict: bool) -> Result<Value, ModelError> {
    let out = rendered_tools(req)?
        .into_iter()
        .map(|t| {
            let on = strict && t.get("parameters").is_some_and(strict_compatible_root);
            if strict && !on {
                let name = t.get("name").and_then(Value::as_str).unwrap_or_default();
                tracing::debug!(
                    tool = name,
                    "схема несовместима со strict-режимом, strict: false"
                );
            }
            let mut f = Map::new();
            f.insert("type".into(), json!("function"));
            for k in ["name", "description", "parameters"] {
                f.insert(k.into(), t.get(k).cloned().unwrap_or(Value::Null));
            }
            f.insert("strict".into(), json!(on));
            Value::Object(f)
        })
        .collect();
    Ok(Value::Array(out))
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
            json!({"type": "function", "name": n})
        }
    })
}

/// Элемент ответа в порядке `output_index`: для данных повтора.
#[derive(Debug)]
enum OutputItem {
    /// `reasoning` целиком из `response.output_item.done`.
    Reasoning(Value),
    Message,
    Call(String),
}

/// Начатый вызов функции.
#[derive(Debug)]
struct Call {
    /// `index` в событиях стрима.
    index: u32,
    /// Фрагменты аргументов уже отданы.
    streamed: bool,
}

/// Декодер стрима Responses. `Finished` — на `response.completed` или
/// `response.incomplete`; `response.failed` и `error` — ошибка.
#[derive(Debug)]
pub struct ResponsesDecoder {
    /// Модель запроса: для `ReplayBlob`.
    model: String,
    asm: MessageAssembler,
    usage: Usage,
    /// Элементы ответа по `output_index`.
    items: BTreeMap<u64, OutputItem>,
    /// Вызовы по `output_index`.
    calls: BTreeMap<u64, Call>,
    /// `item_id` → `output_index`.
    ids: HashMap<String, u64>,
    /// Последняя часть сводки рассуждения: `(item_id, summary_index)`.
    summary: Option<(String, u64)>,
    refusal: bool,
    done: bool,
}

impl ResponsesDecoder {
    pub fn new(model: String) -> Self {
        Self {
            model,
            asm: MessageAssembler::new(),
            usage: Usage::default(),
            items: BTreeMap::new(),
            calls: BTreeMap::new(),
            ids: HashMap::new(),
            summary: None,
            refusal: false,
            done: false,
        }
    }

    /// `Finished` уже отправлен.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Событие SSE → события стрима. Данные не JSON — `Malformed`; `response.failed`
    /// и `error` — ошибка по коду ([`failed_error`]).
    pub fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<StreamEvent>, ModelError> {
        if self.done {
            return Ok(Vec::new());
        }
        let data = ev.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(Vec::new());
        }
        let v: Value = serde_json::from_str(data).map_err(|e| {
            ModelError::new(
                ErrorClass::Malformed,
                format!("событие Responses не JSON: {e}"),
            )
        })?;
        let kind = v
            .get("type")
            .and_then(Value::as_str)
            .or(ev.event.as_deref())
            .unwrap_or_default();
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default();
        let mut out = Vec::new();
        match kind {
            "response.output_text.delta" => {
                self.message_at(&v);
                push_text(&mut out, s("delta"));
            }
            "response.refusal.delta" => {
                self.message_at(&v);
                self.refusal = true;
                push_text(&mut out, s("delta"));
            }
            "response.reasoning_summary_text.delta" => {
                let part = (
                    s("item_id").to_owned(),
                    u64_at(&v, &["summary_index"]).unwrap_or(0),
                );
                let d = s("delta");
                if !d.is_empty() {
                    // Новая часть сводки отделяется пустой строкой.
                    if self.summary.as_ref().is_some_and(|p| p != &part) {
                        out.push(StreamEvent::ReasoningDelta("\n\n".to_owned()));
                    }
                    self.summary = Some(part);
                    out.push(StreamEvent::ReasoningDelta(d.to_owned()));
                }
            }
            "response.output_item.added" => self.item_added(&v, &mut out),
            "response.function_call_arguments.delta" => {
                if let Some(c) = self.call_of(&v) {
                    let d = s("delta");
                    if !d.is_empty() {
                        c.streamed = true;
                        out.push(StreamEvent::ToolCallArgs {
                            index: c.index,
                            fragment: d.to_owned(),
                        });
                    }
                }
            }
            "response.function_call_arguments.done" => {
                let args = s("arguments").to_owned();
                if let Some(c) = self.call_of(&v) {
                    unstreamed_args(c, &args, &mut out);
                }
            }
            "response.output_item.done" => self.item_done(&v, &mut out),
            "response.completed" => {
                self.read_usage(&v);
                let finish = if self.refusal {
                    FinishReason::Refusal
                } else if self.calls.is_empty() {
                    FinishReason::Stop
                } else {
                    FinishReason::ToolCalls
                };
                out.push(self.finished(finish));
            }
            "response.incomplete" => {
                self.read_usage(&v);
                let reason = v
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let finish = match reason {
                    "content_filter" => FinishReason::ContentFilter,
                    // `max_output_tokens` и прочие (`max_messages`, `steered`) —
                    // обрезанный, но валидный ответ.
                    _ => FinishReason::Length,
                };
                out.push(self.finished(finish));
            }
            "response.failed" => {
                return Err(failed_error(
                    v.pointer("/response/error").unwrap_or(&Value::Null),
                ));
            }
            "error" => {
                let err = v.get("error").filter(|e| e.is_object()).unwrap_or(&v);
                return Err(failed_error(err));
            }
            "response.reasoning_text.delta" => {
                tracing::debug!("полный текст рассуждения Responses не переводится");
            }
            _ => {}
        }
        for e in &out {
            self.asm.on_event(e);
        }
        Ok(out)
    }

    /// Тело кончилось без `response.completed` — обрыв.
    pub fn finish_at_eof(&mut self) -> Result<Vec<StreamEvent>, ModelError> {
        if self.done {
            return Ok(Vec::new());
        }
        Err(ModelError::new(
            ErrorClass::StreamCut,
            "стрим Responses закрыт до response.completed",
        ))
    }

    /// Тело кончилось на строке без перевода строки: законченное финальное событие —
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

    fn output_index(v: &Value) -> Option<u64> {
        u64_at(v, &["output_index"])
    }

    /// Текст на `output_index` — элемент `message`.
    fn message_at(&mut self, v: &Value) {
        if let Some(i) = Self::output_index(v) {
            self.items.entry(i).or_insert(OutputItem::Message);
        }
    }

    /// Вызов события по `output_index`, иначе по `item_id`.
    fn call_of(&mut self, v: &Value) -> Option<&mut Call> {
        let i = Self::output_index(v).or_else(|| {
            v.get("item_id")
                .and_then(Value::as_str)
                .and_then(|id| self.ids.get(id).copied())
        })?;
        self.calls.get_mut(&i)
    }

    /// Новый вызов: `ToolCallStart` с `call_id` как `id`.
    fn start_call(&mut self, i: u64, item: &Value, out: &mut Vec<StreamEvent>) {
        if self.calls.contains_key(&i) {
            return;
        }
        let s = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or_default();
        let index = u32::try_from(self.calls.len()).unwrap_or(u32::MAX);
        self.calls.insert(
            i,
            Call {
                index,
                streamed: false,
            },
        );
        self.items
            .insert(i, OutputItem::Call(s("call_id").to_owned()));
        out.push(StreamEvent::ToolCallStart {
            index,
            id: s("call_id").to_owned(),
            name: s("name").to_owned(),
        });
    }

    fn item_added(&mut self, v: &Value, out: &mut Vec<StreamEvent>) {
        let item = v.get("item").unwrap_or(&Value::Null);
        let Some(i) = Self::output_index(v) else {
            return;
        };
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            self.ids.insert(id.to_owned(), i);
        }
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => self.start_call(i, item, out),
            Some("message") => {
                self.items.entry(i).or_insert(OutputItem::Message);
            }
            Some("reasoning") => {} // полный элемент — в `output_item.done`
            other => tracing::debug!(item = ?other, "элемент ответа Responses не переводится"),
        }
    }

    fn item_done(&mut self, v: &Value, out: &mut Vec<StreamEvent>) {
        let item = v.get("item").unwrap_or(&Value::Null);
        let Some(i) = Self::output_index(v) else {
            return;
        };
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                self.items.insert(i, OutputItem::Reasoning(item.clone()));
            }
            Some("function_call") => {
                self.start_call(i, item, out);
                let args = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if let Some(c) = self.calls.get_mut(&i) {
                    unstreamed_args(c, &args, out);
                }
            }
            Some("message") => {
                self.items.entry(i).or_insert(OutputItem::Message);
            }
            _ => {}
        }
    }

    fn read_usage(&mut self, v: &Value) {
        let Some(u) = v.pointer("/response/usage").filter(|u| u.is_object()) else {
            return;
        };
        self.usage.input = u64_at(u, &["input_tokens"]).unwrap_or(0);
        self.usage.cached = u64_at(u, &["input_tokens_details", "cached_tokens"]).unwrap_or(0);
        self.usage.cache_write =
            u64_at(u, &["input_tokens_details", "cache_write_tokens"]).unwrap_or(0);
        self.usage.output = u64_at(u, &["output_tokens"]).unwrap_or(0);
        self.usage.reasoning =
            u64_at(u, &["output_tokens_details", "reasoning_tokens"]).unwrap_or(0);
    }

    /// Данные повтора. Один `reasoning` первым элементом — сам элемент; несколько или
    /// не первым — массив элементов в порядке ответа: `reasoning` целиком, вместо
    /// прочих — `{"type":"message"}` и `{"type":"function_call","call_id":…}`.
    fn replay(&mut self) -> Option<ReplayBlob> {
        let items = std::mem::take(&mut self.items);
        let reasoning = items
            .values()
            .filter(|x| matches!(x, OutputItem::Reasoning(_)))
            .count();
        let data = match items.into_values().collect::<Vec<_>>() {
            _ if reasoning == 0 => return None,
            v if reasoning == 1 && matches!(v.first(), Some(OutputItem::Reasoning(_))) => {
                match v.into_iter().next() {
                    Some(OutputItem::Reasoning(item)) => item,
                    _ => return None,
                }
            }
            v => Value::Array(
                v.into_iter()
                    .map(|x| match x {
                        OutputItem::Reasoning(item) => item,
                        OutputItem::Message => json!({"type": "message"}),
                        OutputItem::Call(id) => json!({"type": "function_call", "call_id": id}),
                    })
                    .collect(),
            ),
        };
        Some(ReplayBlob {
            wire: Wire::Responses,
            model: self.model.clone(),
            data,
        })
    }

    /// Итоговое событие; дальше декодер ничего не отдаёт.
    fn finished(&mut self, finish: FinishReason) -> StreamEvent {
        self.done = true;
        let mut asm = std::mem::take(&mut self.asm);
        if let Some(blob) = self.replay() {
            asm.set_reasoning_replay(blob);
        }
        StreamEvent::Finished {
            message: asm.finish(),
            finish,
            usage: std::mem::take(&mut self.usage),
        }
    }
}

impl StreamDecoder for ResponsesDecoder {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<StreamEvent>, ModelError> {
        ResponsesDecoder::on_event(self, ev)
    }
    fn finish_at_eof(&mut self) -> Result<Vec<StreamEvent>, ModelError> {
        ResponsesDecoder::finish_at_eof(self)
    }
    fn on_unterminated(
        &mut self,
        ev: &SseEvent,
        cut: ModelError,
    ) -> Result<Vec<StreamEvent>, ModelError> {
        ResponsesDecoder::on_unterminated(self, ev, cut)
    }
    fn is_done(&self) -> bool {
        ResponsesDecoder::is_done(self)
    }
}

fn push_text(out: &mut Vec<StreamEvent>, t: &str) {
    if !t.is_empty() {
        out.push(StreamEvent::TextDelta(t.to_owned()));
    }
}

/// Аргументы целиком, если фрагментов не было (часть совместимых серверов шлёт их
/// только в `.done`).
fn unstreamed_args(c: &mut Call, args: &str, out: &mut Vec<StreamEvent>) {
    if !c.streamed && !args.is_empty() {
        c.streamed = true;
        out.push(StreamEvent::ToolCallArgs {
            index: c.index,
            fragment: args.to_owned(),
        });
    }
}

/// Коды `ResponseError` клиентской стороны (типы SDK `openai-python`, `response_error.py`).
const CLIENT_CODES: &[&str] = &[
    "invalid_prompt",
    "data_residency_mismatch",
    "invalid_image",
    "invalid_image_format",
    "invalid_base64_image",
    "invalid_image_url",
    "image_too_large",
    "image_too_small",
    "image_parse_error",
    "invalid_image_mode",
    "image_file_too_large",
    "unsupported_image_media_type",
    "empty_image_file",
    "failed_to_download_image",
    "image_file_not_found",
];

/// Ошибка `response.failed` (`response.error`) или события `error`: класс по `code`.
/// Нарушение политики (`*_policy*`) — `ContentFilter`; неизвестный код — разбор
/// Chat Completions (`context_length…`, `rate_limit…`, иначе `Server`).
fn failed_error(err: &Value) -> ModelError {
    let code = err.get("code").and_then(Value::as_str).unwrap_or_default();
    let status = match code {
        "server_error" => 500,
        "rate_limit_exceeded" => 429,
        "vector_store_timeout" => 408,
        c if c.contains("policy") => {
            return ModelError::new(ErrorClass::ContentFilter, format!("ошибка в стриме: {err}"));
        }
        c if CLIENT_CODES.contains(&c) => 400,
        _ => return stream_error(err),
    };
    let mut e = classify_status(status, &HeaderMap::new(), &err.to_string());
    e.message = format!("ошибка в стриме: {}", e.message);
    // Статус ответа был 2xx: код ошибки — из тела.
    e.http_status = None;
    e
}
