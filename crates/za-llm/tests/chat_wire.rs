//! Коннектор Chat Completions: тело запроса, декодер стрима, насос SSE (задача 4).

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use support::{FakeServer, Reply, fixture};
use za_llm::__test::{
    ChatDecoder, ChatDialect, ChatWire, Connector, MessageAssembler, OpenedStream, WireCtx,
};
use za_llm::http::{HttpTransport, SseEvent, Timeouts};
use za_llm::{
    BackendKind, ChatRequest, CostSource, Effort, ErrorClass, FinishReason, Message, ModelError,
    Part, ProviderCapabilities, ReasoningMode, ReplayBlob, Role, Secret, StreamEvent, ToolCall,
    ToolChoice, ToolSchema, Wire, default_caps,
};

const LLAMA_ROUTE: &str = "POST /v1/chat/completions";
const API_ROUTE: &str = "POST /chat/completions";

fn wire(dialect: ChatDialect, base: &str) -> ChatWire {
    ChatWire {
        dialect,
        base_url: base.to_owned(),
        model: "m".to_owned(),
    }
}

fn kind(d: ChatDialect) -> BackendKind {
    match d {
        ChatDialect::LlamaCpp => BackendKind::LlamaCpp,
        ChatDialect::Vllm => BackendKind::Vllm,
        ChatDialect::OpenRouter => BackendKind::OpenRouter,
        ChatDialect::OpenAi => BackendKind::OpenAiChat,
        ChatDialect::Generic => BackendKind::OpenAiCompatible,
    }
}

fn ctx(caps: &ProviderCapabilities) -> WireCtx<'_> {
    WireCtx {
        caps,
        slot: None,
        secret: None,
        thinking: None,
        max_output_tokens: None,
    }
}

fn tool(name: &str) -> ToolSchema {
    ToolSchema {
        name: name.into(),
        description: format!("Инструмент {name}."),
        parameters: json!({"type": "object", "properties": {}}),
    }
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        parts: vec![Part::Text(text.into())],
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        system: "Ты агент.".into(),
        messages: vec![user("Привет")],
        ..ChatRequest::default()
    }
}

fn body(d: ChatDialect, req: &ChatRequest) -> Value {
    let caps = default_caps(kind(d));
    wire(d, "http://x").build_body(req, &ctx(&caps)).unwrap()
}

#[test]
fn openai_sampling_follows_profile() {
    let mut req = request();
    req.sampling.temperature = Some(0.5);
    req.sampling.top_p = Some(0.9);
    let with = |d: ChatDialect, model: &str| {
        let caps = default_caps(kind(d));
        let t = za_llm::probe::openai_thinking(model);
        let mut c = ctx(&caps);
        c.thinking = Some(&t);
        wire(d, "http://x").build_body(&req, &c).unwrap()
    };
    // Рассуждающая модель OpenAI: поля не уходят.
    let b = with(ChatDialect::OpenAi, "gpt-6-astra");
    assert!(
        b.get("temperature").is_none() && b.get("top_p").is_none(),
        "{b}"
    );
    // Модель без рассуждений: уходят.
    let b = with(ChatDialect::OpenAi, "gpt-4.1");
    assert_eq!(b["temperature"], 0.5);
    assert_eq!(b["top_p"], 0.9);
    // До зонда — как в запросе.
    assert_eq!(body(ChatDialect::OpenAi, &req)["temperature"], 0.5);
    // llama.cpp, vLLM, совместимый — как есть, профиль не смотрится.
    for d in [
        ChatDialect::LlamaCpp,
        ChatDialect::Vllm,
        ChatDialect::Generic,
    ] {
        let b = with(d, "gpt-6-astra");
        assert_eq!(b["temperature"], 0.5, "{d:?}");
        assert_eq!(b["top_p"], 0.9, "{d:?}");
    }
}

fn transport() -> HttpTransport {
    HttpTransport::local(Timeouts {
        connect: Duration::from_secs(2),
        first_byte: Duration::from_secs(5),
        idle: Duration::from_secs(5),
    })
}

/// Открывает стрим коннектора `d` к поддельному серверу с одним ответом.
async fn open_with(d: ChatDialect, reply: Reply) -> (FakeServer, Result<OpenedStream, ModelError>) {
    let route = if d == ChatDialect::LlamaCpp {
        LLAMA_ROUTE
    } else {
        API_ROUTE
    };
    let srv = FakeServer::start(vec![(route, vec![reply])]).await;
    let caps = default_caps(kind(d));
    let w = Connector::Chat(wire(d, &srv.base_url));
    let r = w.open(&transport(), &request(), &ctx(&caps)).await;
    (srv, r)
}

/// Все элементы канала до его закрытия.
async fn drain(mut s: OpenedStream) -> Vec<Result<StreamEvent, ModelError>> {
    let mut out = Vec::new();
    while let Some(x) = tokio::time::timeout(Duration::from_secs(5), s.events.recv())
        .await
        .expect("канал не закрылся за 5 с")
    {
        out.push(x);
    }
    out
}

async fn events_of(d: ChatDialect, fixture_path: &str) -> Vec<StreamEvent> {
    let (_srv, r) = open_with(d, Reply::sse(vec![fixture(fixture_path)])).await;
    drain(r.unwrap())
        .await
        .into_iter()
        .map(|x| x.unwrap())
        .collect()
}

fn finished(evs: &[StreamEvent]) -> (&Message, &FinishReason, &za_llm::Usage) {
    match evs.last() {
        Some(StreamEvent::Finished {
            message,
            finish,
            usage,
        }) => (message, finish, usage),
        other => panic!("последнее событие не Finished: {other:?}"),
    }
}

/// Значение поля из чанка фикстуры, где оно есть.
fn fixture_number(path: &str, key: &str) -> f64 {
    let text = String::from_utf8(fixture(path)).unwrap();
    for line in text.lines() {
        if let Some(d) = line.strip_prefix("data: ")
            && let Ok(v) = serde_json::from_str::<Value>(d)
            && let Some(n) = v["timings"][key].as_f64()
        {
            return n;
        }
    }
    panic!("в {path} нет timings.{key}");
}

// ---------- Тело запроса ----------

#[test]
fn body_llama_has_slot_cache_and_single_system() {
    let caps = default_caps(BackendKind::LlamaCpp);
    let mut c = ctx(&caps);
    c.slot = Some(2);
    let b = wire(ChatDialect::LlamaCpp, "http://x")
        .build_body(&request(), &c)
        .unwrap();
    assert_eq!(b["id_slot"], 2);
    assert_eq!(b["cache_prompt"], true);
    assert_eq!(b["return_progress"], true);
    assert_eq!(b["reasoning_format"], "auto");
    assert_eq!(b["stream"], true);
    assert_eq!(b["stream_options"]["include_usage"], true);
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "Ты агент.");
    assert_eq!(msgs.iter().filter(|m| m["role"] == "system").count(), 1);
}

#[test]
fn urls_follow_base_convention() {
    assert_eq!(
        wire(ChatDialect::LlamaCpp, "http://127.0.0.1:8080/").url(),
        "http://127.0.0.1:8080/v1/chat/completions"
    );
    assert_eq!(
        wire(ChatDialect::OpenRouter, "https://openrouter.ai/api/v1").url(),
        "https://openrouter.ai/api/v1/chat/completions"
    );
}

#[test]
fn body_tools_order_is_canonical() {
    let mut a = request();
    a.tools = vec![tool("b"), tool("a")];
    let mut b = request();
    b.tools = vec![tool("a"), tool("b")];
    let ba = body(ChatDialect::Vllm, &a);
    let bb = body(ChatDialect::Vllm, &b);
    assert_eq!(ba["tools"], bb["tools"]);
    let names: Vec<&str> = ba["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a", "b"]);
    assert_eq!(ba["tools"][0]["type"], "function");
    assert_eq!(ba["tool_choice"], "auto");
    // strict — только при caps.strict_schema и совместимой схеме: у `{"type":"object",
    // "properties":{}}` нет `additionalProperties: false`.
    assert!(ba["tools"][0]["function"].get("strict").is_none());
    assert!(
        body(ChatDialect::OpenAi, &a)["tools"][0]["function"]
            .get("strict")
            .is_none()
    );
    assert_eq!(ba["parallel_tool_calls"], true);
    assert_eq!(
        body(ChatDialect::LlamaCpp, &a)["parallel_tool_calls"],
        false
    );
}

fn strict_of(parameters: Value) -> Option<Value> {
    let mut req = request();
    req.tools = vec![ToolSchema {
        name: "t".into(),
        description: "d".into(),
        parameters,
    }];
    body(ChatDialect::OpenAi, &req)["tools"][0]["function"]
        .get("strict")
        .cloned()
}

#[test]
fn strict_only_for_compatible_schema() {
    let ok = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "opts": {"type": "object", "properties": {"n": {"type": "integer"}},
                     "required": ["n"], "additionalProperties": false},
            "list": {"type": "array", "items": {"$ref": "#/$defs/item"}}
        },
        "required": ["path", "opts", "list"],
        "additionalProperties": false,
        "$defs": {"item": {"type": "object", "properties": {"x": {"type": "string"}},
                           "required": ["x"], "additionalProperties": false}}
    });
    assert_eq!(strict_of(ok.clone()), Some(json!(true)));
    let empty = json!({"type": "object", "properties": {}, "additionalProperties": false});
    assert_eq!(strict_of(empty), Some(json!(true)));
    // Не все свойства в `required`.
    let mut bad = ok.clone();
    bad["required"] = json!(["path", "opts"]);
    assert_eq!(strict_of(bad), None);
    // Вложенный объект без `additionalProperties: false`.
    let mut bad = ok.clone();
    bad["properties"]["opts"]
        .as_object_mut()
        .unwrap()
        .remove("additionalProperties");
    assert_eq!(strict_of(bad), None);
    // Нарушение в `$defs`.
    let mut bad = ok.clone();
    bad["$defs"]["item"]["required"] = json!([]);
    assert_eq!(strict_of(bad), None);
    // Нарушение в `items` и `anyOf`.
    let mut bad = ok.clone();
    bad["properties"]["list"]["items"] = json!({"type": "object", "properties": {"y": {}}});
    assert_eq!(strict_of(bad), None);
    let mut bad = ok;
    bad["properties"]["path"] = json!({"anyOf": [{"type": "string"}, {"type": "object"}]});
    assert_eq!(strict_of(bad), None);
    // Корень не объект.
    assert_eq!(strict_of(json!({"type": "string"})), None);
    // Белый список ключей strict-режима (Structured Outputs).
    let with_prop = |p: Value| {
        json!({"type": "object", "properties": {"v": p}, "required": ["v"],
               "additionalProperties": false})
    };
    let ok_sub = json!({"type": "object", "properties": {}, "additionalProperties": false});
    assert_eq!(
        strict_of(with_prop(
            json!({"anyOf": [{"type": "string"}, {"type": "null"}]})
        )),
        Some(json!(true))
    );
    assert_eq!(
        strict_of(with_prop(
            json!({"oneOf": [ok_sub.clone(), ok_sub.clone()]})
        )),
        None
    );
    assert_eq!(strict_of(with_prop(json!({"allOf": [ok_sub]}))), None);
    assert_eq!(
        strict_of(with_prop(json!({"type": "string", "not": {"const": "x"}}))),
        None
    );
    assert_eq!(
        strict_of(with_prop(
            json!({"type": "string", "if": {"const": "a"}, "then": {"const": "a"}})
        )),
        None
    );
    assert_eq!(
        strict_of(with_prop(
            json!({"type": "object", "patternProperties": {"^x": {"type": "string"}},
                                   "additionalProperties": false})
        )),
        None
    );
    // Кортежные `items` и подсхема без `type`/`$ref`/`anyOf`/`enum`/`const`.
    assert_eq!(
        strict_of(with_prop(
            json!({"type": "array", "items": [{"type": "string"}]})
        )),
        None
    );
    assert_eq!(
        strict_of(with_prop(json!({"description": "что угодно"}))),
        None
    );
    assert_eq!(strict_of(with_prop(json!({}))), None);
    // `default` в документации strict-режима не указан: консервативно несовместим.
    assert_eq!(
        strict_of(with_prop(json!({"type": "string", "default": "x"}))),
        None
    );
    // Корень `anyOf` и внешний `$ref`.
    let mut root_any = with_prop(json!({"type": "string"}));
    root_any["anyOf"] = json!([{"type": "object"}]);
    assert_eq!(strict_of(root_any), None);
    assert_eq!(
        strict_of(with_prop(json!({"$ref": "https://example.com/s.json"}))),
        None
    );
    // Вложенность объектов больше 10.
    let mut deep = json!({"type": "string"});
    for _ in 0..11 {
        deep = with_prop(deep);
    }
    assert_eq!(strict_of(deep), None);
    let mut deep = json!({"type": "string"});
    for _ in 0..10 {
        deep = with_prop(deep);
    }
    assert_eq!(strict_of(deep), Some(json!(true)));
}

#[test]
fn tool_messages_and_calls() {
    let mut req = request();
    req.tools = vec![tool("read_file")];
    req.tool_choice = ToolChoice::Named("read_file".into());
    req.messages.push(Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall(ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: json!({"path": "a.txt"}),
            raw_arguments: None,
        })],
    });
    req.messages.push(Message {
        role: Role::Tool,
        parts: vec![Part::ToolResult {
            call_id: "c1".into(),
            content: "текст".into(),
            is_error: false,
        }],
    });
    let b = body(ChatDialect::OpenAi, &req);
    let m = &b["messages"];
    assert_eq!(m[2]["role"], "assistant");
    assert_eq!(m[2]["content"], Value::Null);
    assert_eq!(m[2]["tool_calls"][0]["id"], "c1");
    assert_eq!(m[2]["tool_calls"][0]["type"], "function");
    assert_eq!(
        m[2]["tool_calls"][0]["function"]["arguments"],
        r#"{"path":"a.txt"}"#
    );
    assert_eq!(
        m[3],
        json!({"role": "tool", "tool_call_id": "c1", "content": "текст"})
    );
    assert_eq!(
        b["tool_choice"],
        json!({"type": "function", "function": {"name": "read_file"}})
    );
}

#[test]
fn reasoning_is_replayed_only_when_roundtrip() {
    let mut req = request();
    req.messages.push(Message {
        role: Role::Assistant,
        parts: vec![
            Part::Reasoning {
                text: "думаю".into(),
                replay: None,
            },
            Part::Text("ответ".into()),
        ],
    });
    let mut caps = default_caps(BackendKind::LlamaCpp);
    caps.reasoning_roundtrip = true;
    let w = wire(ChatDialect::LlamaCpp, "http://x");
    let b = w.build_body(&req, &ctx(&caps)).unwrap();
    assert_eq!(b["messages"][2]["reasoning_content"], "думаю");
    caps.reasoning_roundtrip = false;
    let b = w.build_body(&req, &ctx(&caps)).unwrap();
    assert!(b["messages"][2].get("reasoning_content").is_none());
    let mut caps = default_caps(BackendKind::Vllm);
    caps.reasoning_roundtrip = true;
    let b = wire(ChatDialect::Vllm, "http://x")
        .build_body(&req, &ctx(&caps))
        .unwrap();
    assert_eq!(b["messages"][2]["reasoning"], "думаю");
}

#[test]
fn openrouter_replays_reasoning_details_of_same_model() {
    let details = json!([{"type": "reasoning.text", "text": "x", "signature": "s", "index": 0}]);
    let mut req = request();
    req.messages.push(Message {
        role: Role::Assistant,
        parts: vec![Part::Reasoning {
            text: "x".into(),
            replay: Some(ReplayBlob {
                wire: Wire::ChatCompletions,
                model: "m".into(),
                data: details.clone(),
            }),
        }],
    });
    let b = body(ChatDialect::OpenRouter, &req);
    assert_eq!(b["messages"][2]["reasoning_details"], details);
    // Данные другой модели не возвращаются.
    if let Part::Reasoning {
        replay: Some(r), ..
    } = &mut req.messages[1].parts[0]
    {
        r.model = "other".into();
    }
    let b = body(ChatDialect::OpenRouter, &req);
    assert!(b["messages"][2].get("reasoning_details").is_none());
    assert_eq!(b["messages"][2]["reasoning"], "x");
}

#[test]
fn required_tool_choice_without_capability_is_rejected_before_send() {
    let mut req = request();
    req.tools = vec![tool("a")];
    req.tool_choice = ToolChoice::Required;
    let mut caps = default_caps(BackendKind::OpenAiCompatible);
    caps.tool_choice_required = false;
    let err = wire(ChatDialect::Generic, "http://x")
        .build_body(&req, &ctx(&caps))
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::BadRequest);
    assert_eq!(err.http_status, None);
    caps.tool_choice_required = true;
    let b = wire(ChatDialect::Generic, "http://x")
        .build_body(&req, &ctx(&caps))
        .unwrap();
    assert_eq!(b["tool_choice"], "required");
}

#[test]
fn llama_reasoning_effort_and_budget() {
    let mut req = request();
    req.reasoning = ReasoningMode::Effort(Effort::Max);
    assert_eq!(
        body(ChatDialect::LlamaCpp, &req)["reasoning_effort"],
        "high"
    );
    // Бюджет на запрос: `reasoning_budget_tokens` llama-server (решение контроллера к D36).
    req.reasoning = ReasoningMode::Budget(4096);
    let b = body(ChatDialect::LlamaCpp, &req);
    assert!(b.get("reasoning_effort").is_none());
    assert!(b.get("reasoning_budget").is_none());
    assert_eq!(b["reasoning_budget_tokens"], 4096);
    req.reasoning = ReasoningMode::Off;
    let b = body(ChatDialect::LlamaCpp, &req);
    assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
    assert_eq!(b["reasoning_effort"], "none");
    req.reasoning = ReasoningMode::Default;
    let b = body(ChatDialect::LlamaCpp, &req);
    for k in [
        "reasoning_effort",
        "chat_template_kwargs",
        "reasoning_budget_tokens",
    ] {
        assert!(b.get(k).is_none(), "{k}");
    }
}

#[test]
fn reasoning_per_cloud_and_vllm_dialect() {
    let mut req = request();
    req.reasoning = ReasoningMode::Budget(10_000);
    assert_eq!(body(ChatDialect::Vllm, &req)["reasoning_effort"], "medium");
    assert_eq!(
        body(ChatDialect::OpenAi, &req)["reasoning_effort"],
        "medium"
    );
    assert_eq!(
        body(ChatDialect::OpenRouter, &req)["reasoning"],
        json!({"max_tokens": 10_000})
    );
    req.reasoning = ReasoningMode::Effort(Effort::Max);
    assert_eq!(body(ChatDialect::OpenAi, &req)["reasoning_effort"], "high");
    assert_eq!(body(ChatDialect::Generic, &req)["reasoning_effort"], "high");
    assert_eq!(
        body(ChatDialect::OpenRouter, &req)["reasoning"],
        json!({"effort": "max"})
    );
    req.reasoning = ReasoningMode::Off;
    assert_eq!(
        body(ChatDialect::Vllm, &req)["chat_template_kwargs"]["enable_thinking"],
        false
    );
    // Облако: при `Off` полей рассуждения нет (как у Responses, D36).
    let b = body(ChatDialect::OpenAi, &req);
    assert!(b.get("reasoning_effort").is_none());
    let b = body(ChatDialect::OpenRouter, &req);
    assert!(b.get("reasoning").is_none() && b.get("reasoning_effort").is_none());
    req.max_output_tokens = Some(100);
    assert_eq!(
        body(ChatDialect::OpenAi, &req)["max_completion_tokens"],
        100
    );
    assert_eq!(body(ChatDialect::Vllm, &req)["max_tokens"], 100);
    assert!(body(ChatDialect::OpenAi, &req).get("max_tokens").is_none());
}

#[test]
fn sampling_floats_are_short() {
    let mut req = request();
    req.sampling.temperature = Some(0.1);
    let b = body(ChatDialect::Vllm, &req);
    assert_eq!(b["temperature"].to_string(), "0.1");
}

#[tokio::test]
async fn openrouter_asks_usage() {
    let req = request();
    assert_eq!(
        body(ChatDialect::OpenRouter, &req)["usage"]["include"],
        true
    );
    let srv = FakeServer::start(vec![(
        API_ROUTE,
        vec![Reply::sse(vec![fixture("chat/openrouter_cost.sse")])],
    )])
    .await;
    let caps = default_caps(BackendKind::OpenRouter);
    let key = Secret::new("sk-or-test".into());
    let mut c = ctx(&caps);
    c.secret = Some(&key);
    let s = wire(ChatDialect::OpenRouter, &srv.base_url)
        .open(&transport(), &req, &c)
        .await
        .unwrap();
    drain(s).await;
    let r = &srv.requests(API_ROUTE)[0];
    assert_eq!(r.header("x-title"), Some("zero-agent"));
    assert_eq!(r.header("authorization"), Some("Bearer sk-or-test"));
    assert_eq!(r.json()["usage"]["include"], true);
}

// ---------- Сборка сообщения ----------

#[test]
fn assembler_orders_parts_and_keeps_raw() {
    let mut a = MessageAssembler::new();
    for ev in [
        StreamEvent::ReasoningDelta("a".into()),
        StreamEvent::TextDelta("b".into()),
        StreamEvent::ToolCallStart {
            index: 1,
            id: "c2".into(),
            name: "g".into(),
        },
        StreamEvent::ToolCallStart {
            index: 0,
            id: "c1".into(),
            name: "f".into(),
        },
        StreamEvent::ToolCallArgs {
            index: 0,
            fragment: "{}".into(),
        },
        StreamEvent::ToolCallArgs {
            index: 1,
            fragment: "{\"x\":".into(),
        },
    ] {
        a.on_event(&ev);
    }
    let m = a.finish();
    assert_eq!(m.role, Role::Assistant);
    assert_eq!(
        m.parts,
        vec![
            Part::Reasoning {
                text: "a".into(),
                replay: None
            },
            Part::Text("b".into()),
            Part::ToolCall(ToolCall {
                id: "c1".into(),
                name: "f".into(),
                arguments: json!({}),
                raw_arguments: Some("{}".into()),
            }),
            Part::ToolCall(ToolCall {
                id: "c2".into(),
                name: "g".into(),
                arguments: Value::Null,
                raw_arguments: Some("{\"x\":".into()),
            }),
        ]
    );
}

// ---------- Стрим ----------

#[tokio::test]
async fn llama_text_stream_and_cached_tokens() {
    let path = "chat/llama_text.sse";
    let evs = events_of(ChatDialect::LlamaCpp, path).await;
    let text: String = evs
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Привет!");
    let (msg, finish, usage) = finished(&evs);
    assert_eq!(msg.parts, vec![Part::Text("Привет!".into())]);
    assert_eq!(*finish, FinishReason::Stop);
    assert_eq!(usage.cached, fixture_number(path, "cache_n") as u64);
    let gpu = fixture_number(path, "prompt_ms") + fixture_number(path, "predicted_ms");
    assert_eq!(usage.gpu_ms, Some(gpu.round() as u64));
    assert_eq!((usage.input, usage.output), (120, 3));
}

#[tokio::test]
async fn tool_call_fragments_are_assembled() {
    let evs = events_of(ChatDialect::LlamaCpp, "chat/llama_tool_calls.sse").await;
    assert!(evs.iter().any(|e| matches!(
        e,
        StreamEvent::ToolCallStart { index: 0, name, .. } if name == "read_file"
    )));
    let (msg, finish, usage) = finished(&evs);
    let Part::ToolCall(c) = &msg.parts[0] else {
        panic!("{msg:?}")
    };
    assert_eq!(c.arguments, json!({"path": "a.txt"}));
    assert_eq!(*finish, FinishReason::ToolCalls);
    // `usage.prompt_tokens_details.cached_tokens` приоритетнее `timings.cache_n`.
    assert_eq!(usage.cached, 256);
}

#[tokio::test]
async fn parallel_tool_calls_keep_index() {
    let evs = events_of(ChatDialect::OpenAi, "chat/openai_parallel_tools.sse").await;
    let starts: Vec<(u32, &str)> = evs
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallStart { index, id, .. } => Some((*index, id.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(starts, [(0, "call_A1"), (1, "call_B2")]);
    let (msg, finish, usage) = finished(&evs);
    let calls: Vec<&ToolCall> = msg
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::ToolCall(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0].id, calls[1].id);
    assert_eq!(
        (calls[0].name.as_str(), &calls[0].arguments),
        ("get_weather", &json!({"city": "Москва"}))
    );
    assert_eq!(
        (calls[1].name.as_str(), &calls[1].arguments),
        ("get_time", &json!({"tz": "Europe/Moscow"}))
    );
    assert_eq!(*finish, FinishReason::ToolCalls);
    assert_eq!((usage.input, usage.cached, usage.output), (82, 64, 40));
}

#[tokio::test]
async fn vllm_reasoning_field_named_reasoning() {
    let evs = events_of(ChatDialect::Vllm, "chat/vllm_reasoning.sse").await;
    assert!(evs.contains(&StreamEvent::ReasoningDelta("Считаю: ".into())));
    let (msg, _, _) = finished(&evs);
    assert_eq!(
        msg.parts,
        vec![
            Part::Reasoning {
                text: "Считаю: 2+2=4.".into(),
                replay: None
            },
            Part::Text("4".into()),
        ]
    );
}

#[tokio::test]
async fn openrouter_cost_is_provider_cost() {
    let evs = events_of(ChatDialect::OpenRouter, "chat/openrouter_cost.sse").await;
    let (msg, finish, usage) = finished(&evs);
    assert_eq!(usage.cost_usd, Some(0.00042));
    assert_eq!(usage.cost_source, CostSource::Provider);
    assert_eq!(usage.reasoning, 2);
    assert_eq!(*finish, FinishReason::Stop);
    // `reasoning_details` склеены по `index` и возвращаются как ReplayBlob модели запроса.
    let Part::Reasoning {
        text,
        replay: Some(r),
    } = &msg.parts[0]
    else {
        panic!("{msg:?}")
    };
    assert_eq!(text, "Коротко.");
    assert_eq!((r.wire, r.model.as_str()), (Wire::ChatCompletions, "m"));
    assert_eq!(
        r.data,
        json!([{"type": "reasoning.text", "text": "Коротко.", "signature": "EqQBCkgIBxABGAIiQL",
                "format": "anthropic-claude-v1", "index": 0}])
    );
}

#[tokio::test]
async fn progress_events_from_llama() {
    let evs = events_of(ChatDialect::LlamaCpp, "chat/llama_reasoning_progress.sse").await;
    let first_other = evs
        .iter()
        .position(|e| !matches!(e, StreamEvent::Progress { .. }))
        .unwrap();
    assert!(first_other > 0, "нет Progress в начале");
    let mut last = 0;
    for e in &evs[..first_other] {
        let StreamEvent::Progress { processed, total } = *e else {
            unreachable!()
        };
        assert!(processed >= last && processed <= total, "{e:?}");
        last = processed;
    }
    assert!(matches!(evs[first_other], StreamEvent::ReasoningDelta(_)));
}

#[tokio::test]
async fn error_object_midstream_is_classified() {
    let (_srv, r) = open_with(
        ChatDialect::LlamaCpp,
        Reply::sse(vec![fixture("chat/error_midstream.sse")]),
    )
    .await;
    let items = drain(r.unwrap()).await;
    assert!(matches!(items[0], Ok(StreamEvent::TextDelta(ref t)) if t == "Нач"));
    let err = items.last().unwrap().as_ref().unwrap_err();
    assert_eq!(err.class, ErrorClass::Overloaded, "{err:?}");
    assert_eq!(items.iter().filter(|x| x.is_err()).count(), 1);
}

#[tokio::test]
async fn http_error_is_err_from_open() {
    let (srv, r) = open_with(
        ChatDialect::Vllm,
        Reply::Json(503, json!({"error": {"message": "busy"}})),
    )
    .await;
    let err = r.unwrap_err();
    assert_eq!(err.class, ErrorClass::Overloaded);
    assert_eq!(err.http_status, Some(503));
    assert_eq!(srv.requests(API_ROUTE).len(), 1);
}

#[tokio::test]
async fn limits_are_returned_with_stream() {
    let srv = FakeServer::start(vec![(
        API_ROUTE,
        vec![Reply::Raw(
            200,
            vec![
                ("content-type".into(), "text/event-stream".into()),
                ("x-ratelimit-remaining-requests".into(), "0".into()),
            ],
            fixture("chat/openai_parallel_tools.sse"),
        )],
    )])
    .await;
    let caps = default_caps(BackendKind::OpenAiChat);
    let s = wire(ChatDialect::OpenAi, &srv.base_url)
        .open(&transport(), &request(), &ctx(&caps))
        .await
        .unwrap();
    assert_eq!(s.limits.remaining_requests, Some(0));
}

#[tokio::test]
async fn dropping_receiver_closes_connection() {
    let chunk = |i: usize| {
        format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{i}\"}},\"finish_reason\":null}}]}}\n\n"
        )
        .into_bytes()
    };
    let (srv, r) = open_with(
        ChatDialect::LlamaCpp,
        Reply::Sse {
            chunks: (0..20).map(chunk).collect(),
            delay: Duration::from_millis(50),
            cut_after: None,
            then_hang: false,
        },
    )
    .await;
    let mut s = r.unwrap();
    assert!(matches!(
        s.events.recv().await,
        Some(Ok(StreamEvent::TextDelta(_)))
    ));
    drop(s);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while srv.disconnects(LLAMA_ROUTE) == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(srv.disconnects(LLAMA_ROUTE), 1);
}

#[tokio::test]
async fn unparseable_arguments_go_to_raw() {
    let body = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c\",",
        "\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\":\"}}]},",
        "\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (_srv, r) = open_with(
        ChatDialect::Generic,
        Reply::sse(vec![body.as_bytes().to_vec()]),
    )
    .await;
    let evs: Vec<StreamEvent> = drain(r.unwrap())
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    let (msg, _, _) = finished(&evs);
    let Part::ToolCall(c) = &msg.parts[0] else {
        panic!("{msg:?}")
    };
    assert_eq!(c.raw_arguments.as_deref(), Some("{\"a\":"));
    assert_eq!(c.arguments, Value::Null);
}

// ---------- Конец потока (решение контроллера для задач 4–6) ----------

/// Тело ответа целиком, без chunked: конец тела — конец соединения.
async fn raw_items(body: &str) -> Vec<Result<StreamEvent, ModelError>> {
    let (_srv, r) = open_with(
        ChatDialect::Vllm,
        Reply::Raw(
            200,
            vec![("content-type".into(), "text/event-stream".into())],
            body.as_bytes().to_vec(),
        ),
    )
    .await;
    drain(r.unwrap()).await
}

const TEXT: &str =
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n";
const STOP: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}";

fn assert_finished_ok(items: &[Result<StreamEvent, ModelError>]) {
    match items.last() {
        Some(Ok(StreamEvent::Finished {
            message, finish, ..
        })) => {
            assert_eq!(message.parts, vec![Part::Text("ok".into())]);
            assert_eq!(*finish, FinishReason::Stop);
        }
        other => panic!("ожидался Finished: {other:?}"),
    }
    assert!(items.iter().all(Result::is_ok));
}

#[tokio::test]
async fn done_without_trailing_newline_is_normal_end() {
    let items = raw_items(&format!("{TEXT}{STOP}\n\ndata: [DONE]")).await;
    assert_finished_ok(&items);
}

#[tokio::test]
async fn final_chunk_without_trailing_newline_is_normal_end() {
    let items = raw_items(&format!("{TEXT}{STOP}")).await;
    assert_finished_ok(&items);
}

#[tokio::test]
async fn eof_after_finish_reason_without_done_is_normal_end() {
    let items = raw_items(&format!("{TEXT}{STOP}\n\n")).await;
    assert_finished_ok(&items);
}

#[tokio::test]
async fn cut_json_line_is_stream_cut() {
    let items = raw_items(&format!("{TEXT}{}", &STOP[..40])).await;
    let err = items.last().unwrap().as_ref().unwrap_err();
    assert_eq!(err.class, ErrorClass::StreamCut, "{err:?}");
    // Целый чанк без finish_reason в конце тела — тоже обрыв.
    let items = raw_items(TEXT.trim_end()).await;
    let err = items.last().unwrap().as_ref().unwrap_err();
    assert_eq!(err.class, ErrorClass::StreamCut, "{err:?}");
    // Штатный конец тела без finish_reason — обрыв.
    let items = raw_items(TEXT).await;
    let err = items.last().unwrap().as_ref().unwrap_err();
    assert_eq!(err.class, ErrorClass::StreamCut, "{err:?}");
}

#[test]
fn decoder_ignores_events_after_finished() {
    let mut d = ChatDecoder::new(ChatDialect::Generic);
    let ev = |data: &str| SseEvent {
        event: None,
        data: data.into(),
        id: None,
    };
    assert!(d.on_event(&ev(&STOP[6..])).unwrap().is_empty());
    let out = d.on_event(&ev("[DONE]")).unwrap();
    assert!(matches!(out[..], [StreamEvent::Finished { .. }]));
    assert!(d.is_done());
    assert!(d.on_event(&ev(&TEXT[6..])).unwrap().is_empty());
    let bad = d.on_event(&ev("not json"));
    assert!(bad.unwrap().is_empty(), "после Finished разбор не нужен");
    let mut d = ChatDecoder::new(ChatDialect::Generic);
    assert_eq!(
        d.on_event(&ev("{oops")).unwrap_err().class,
        ErrorClass::Malformed
    );
}

fn sse(data: &str) -> SseEvent {
    SseEvent {
        event: None,
        data: data.into(),
        id: None,
    }
}

fn final_message(evs: &[StreamEvent]) -> Message {
    finished(evs).0.clone()
}

#[test]
fn same_index_with_new_id_is_new_call() {
    let mut d = ChatDecoder::new(ChatDialect::Generic);
    let mut evs = Vec::new();
    for data in [
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"f","arguments":"{\"x\":1}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"b","function":{"name":"g","arguments":"{\"y\":"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"2}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"c","function":{"name":"h","arguments":"{}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ] {
        evs.extend(d.on_event(&sse(data)).unwrap());
    }
    let starts: Vec<(u32, &str)> = evs
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallStart { index, id, .. } => Some((*index, id.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(starts, [(0, "a"), (1, "b"), (2, "c")]);
    let calls: Vec<(String, Value)> = final_message(&evs)
        .parts
        .into_iter()
        .filter_map(|p| match p {
            Part::ToolCall(c) => Some((c.name, c.arguments)),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        [
            ("f".to_owned(), json!({"x": 1})),
            ("g".to_owned(), json!({"y": 2})),
            ("h".to_owned(), json!({})),
        ]
    );
}

#[test]
fn late_id_continues_the_same_call() {
    let mut d = ChatDecoder::new(ChatDialect::Generic);
    let mut evs = Vec::new();
    for data in [
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":"{\"x\":"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"real","function":{"arguments":"1}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"real","function":{"arguments":""}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"other","function":{"name":"g","arguments":"{}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ] {
        evs.extend(d.on_event(&sse(data)).unwrap());
    }
    let starts = evs
        .iter()
        .filter(|e| matches!(e, StreamEvent::ToolCallStart { .. }))
        .count();
    assert_eq!(starts, 2, "{evs:?}");
    let calls: Vec<(String, Value)> = final_message(&evs)
        .parts
        .into_iter()
        .filter_map(|p| match p {
            Part::ToolCall(c) => Some((c.name, c.arguments)),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        [
            ("f".to_owned(), json!({"x": 1})),
            ("g".to_owned(), json!({})),
        ]
    );
}

#[test]
fn missing_call_id_gets_unique_fallback() {
    let data = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":""}}]}}]}"#;
    let id_of = || {
        let mut d = ChatDecoder::new(ChatDialect::Generic);
        match d.on_event(&sse(data)).unwrap().into_iter().next() {
            Some(StreamEvent::ToolCallStart { id, .. }) => id,
            other => panic!("{other:?}"),
        }
    };
    let (a, b) = (id_of(), id_of());
    assert!(a.starts_with("call_") && !a.is_empty());
    assert_ne!(a, b);
}

#[test]
fn empty_arguments_are_empty_object() {
    let mut a = MessageAssembler::new();
    a.on_event(&StreamEvent::ToolCallStart {
        index: 0,
        id: "c".into(),
        name: "now".into(),
    });
    let Part::ToolCall(c) = &a.finish().parts[0] else {
        panic!()
    };
    assert_eq!(c.arguments, json!({}));
    assert_eq!(c.raw_arguments.as_deref(), Some(""));
}

#[test]
fn midstream_error_string_codes() {
    let class = |err: &str| {
        let mut d = ChatDecoder::new(ChatDialect::OpenRouter);
        d.on_event(&sse(&format!(r#"{{"error":{err}}}"#)))
            .unwrap_err()
            .class
    };
    assert_eq!(
        class(r#"{"code":"rate_limit_exceeded","message":"slow down"}"#),
        ErrorClass::RateLimited
    );
    assert_eq!(
        class(r#"{"code":"server_error","message":"boom"}"#),
        ErrorClass::Server
    );
    assert_eq!(
        class(r#"{"code":"context_length_exceeded","message":"too long"}"#),
        ErrorClass::ContextOverflow
    );
    assert_eq!(
        class(r#"{"type":"overloaded_error","message":"x"}"#),
        ErrorClass::Overloaded
    );
    assert_eq!(class(r#"{"code":"502","message":"x"}"#), ErrorClass::Server);
    assert_eq!(class(r#"{"message":"???"}"#), ErrorClass::Server);
}

#[tokio::test]
async fn midstream_error_hides_key() {
    let key = "sk-or-secret-777";
    let body = format!("data: {{\"error\":{{\"code\":401,\"message\":\"bad key {key}\"}}}}\n\n");
    let srv = FakeServer::start(vec![(API_ROUTE, vec![Reply::sse(vec![body.into_bytes()])])]).await;
    let caps = default_caps(BackendKind::OpenRouter);
    let secret = Secret::new(key.into());
    let mut c = ctx(&caps);
    c.secret = Some(&secret);
    let s = wire(ChatDialect::OpenRouter, &srv.base_url)
        .open(&transport(), &request(), &c)
        .await
        .unwrap();
    let items = drain(s).await;
    let err = items.last().unwrap().as_ref().unwrap_err();
    assert_eq!(err.class, ErrorClass::Auth);
    assert!(!err.message.contains(key), "{}", err.message);
    assert!(err.message.contains("***"), "{}", err.message);
}

// ---------- Роль System в истории (R1-B1, D10) ----------

#[test]
fn history_system_omitted() {
    let mut req = request();
    req.messages = vec![
        user("a"),
        Message {
            role: Role::System,
            parts: vec![Part::Text("x".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![Part::Text("b".into())],
        },
    ];
    for d in [
        ChatDialect::LlamaCpp,
        ChatDialect::Vllm,
        ChatDialect::OpenRouter,
        ChatDialect::OpenAi,
        ChatDialect::Generic,
    ] {
        let b = body(d, &req);
        let msgs = b["messages"].as_array().unwrap();
        assert!(
            msgs.iter().all(|m| m["content"] != "x"),
            "{d:?}: служебное сообщение в теле"
        );
        // system остаётся единственным и первым (CTX-004).
        assert_eq!(msgs.len(), 3, "{d:?}");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["content"], "a");
        assert_eq!(msgs[2]["content"], "b");
    }
}
