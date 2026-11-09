//! Коннектор Anthropic Messages: тело запроса, декодер стрима, насос SSE (задача 5).

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use support::{FakeServer, Reply, fixture};
use za_llm::__test::{
    AnthropicDecoder, AnthropicWire, CLEAR_AT_BETA, Connector, OpenedStream, WireCtx,
};
use za_llm::http::{HttpTransport, SseEvent, Timeouts};
use za_llm::{
    BackendKind, ChatRequest, Effort, ErrorClass, FinishReason, Message, ModelError, Part,
    ProviderCapabilities, ReasoningMode, ReplayBlob, Role, Sampling, Secret, StreamEvent,
    ThinkingSupport, ToolCall, ToolChoice, ToolSchema, Wire, default_caps,
};

const ROUTE: &str = "POST /v1/messages";
const MODEL: &str = "claude-opus-5-5";

fn wire(base: &str) -> AnthropicWire {
    AnthropicWire {
        base_url: base.to_owned(),
        model: MODEL.to_owned(),
    }
}

fn caps() -> ProviderCapabilities {
    default_caps(BackendKind::Anthropic)
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

const ADAPTIVE: ThinkingSupport = ThinkingSupport {
    adaptive: true,
    effort: true,
    budget: false,
    effort_xhigh: true,
    effort_max: true,
    sampling: false,
};
/// Старая модель: `enabled` с бюджетом, без adaptive и effort.
const OLD: ThinkingSupport = ThinkingSupport {
    adaptive: false,
    effort: false,
    budget: true,
    effort_xhigh: false,
    effort_max: false,
    sampling: false,
};

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

fn tool(name: &str) -> ToolSchema {
    ToolSchema {
        name: name.into(),
        description: format!("Инструмент {name}."),
        parameters: json!({"type": "object", "properties": {}}),
    }
}

fn body_with(req: &ChatRequest, thinking: Option<&ThinkingSupport>) -> Value {
    let caps = caps();
    let mut c = ctx(&caps);
    c.thinking = thinking;
    wire("http://x").build_body(req, &c).unwrap()
}

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read_file".into(),
        arguments: json!({"path": id}),
        raw_arguments: None,
    }
}

fn transport() -> HttpTransport {
    HttpTransport::cloud(Timeouts {
        connect: Duration::from_secs(2),
        first_byte: Duration::from_secs(5),
        idle: Duration::from_secs(5),
    })
}

async fn open_with(
    reply: Reply,
    secret: Option<&Secret>,
) -> (FakeServer, Result<OpenedStream, ModelError>) {
    let srv = FakeServer::start(vec![(ROUTE, vec![reply])]).await;
    let caps = caps();
    let mut c = ctx(&caps);
    c.secret = secret;
    let w = Connector::Anthropic(wire(&srv.url("/v1")));
    let r = w.open(&transport(), &request(), &c).await;
    (srv, r)
}

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

async fn results_of(path: &str) -> Vec<Result<StreamEvent, ModelError>> {
    let (_srv, r) = open_with(Reply::sse(vec![fixture(path)]), None).await;
    drain(r.unwrap()).await
}

async fn events_of(path: &str) -> Vec<StreamEvent> {
    results_of(path)
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

fn sse(data: Value) -> SseEvent {
    SseEvent {
        event: data["type"].as_str().map(str::to_owned),
        data: data.to_string(),
        id: None,
    }
}

// ---------- Тело запроса ----------

#[test]
fn body_has_no_sampling_fields_and_top_level_system() {
    let mut req = request();
    req.sampling = Sampling {
        temperature: Some(0.2),
        top_p: Some(0.9),
        seed: Some(7),
        presence_penalty: Some(0.1),
        stop: vec!["</end>".into()],
    };
    let b = body_with(&req, None);
    for k in ["temperature", "top_p", "top_k", "seed", "presence_penalty"] {
        assert!(b.get(k).is_none(), "поле {k} отправлено");
    }
    assert_eq!(b["system"], "Ты агент.");
    assert_eq!(b["stop_sequences"], json!(["</end>"]));
    assert_eq!(b["model"], MODEL);
    assert_eq!(b["stream"], true);
    let msgs = b["messages"].as_array().unwrap();
    assert!(msgs.iter().all(|m| m["role"] != "system"));
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(
        msgs[0]["content"][0],
        json!({"type": "text", "text": "Привет"})
    );
}

#[test]
fn adaptive_thinking_when_profile_says_so() {
    let mut req = request();
    req.reasoning = ReasoningMode::Effort(Effort::High);
    let b = body_with(&req, Some(&ADAPTIVE));
    assert_eq!(b["thinking"]["type"], "adaptive");
    assert_eq!(b["output_config"]["effort"], "high");
    assert!(b["thinking"].get("budget_tokens").is_none());
    assert!(!b.to_string().contains("budget_tokens"));
}

#[test]
fn budget_thinking_for_old_models() {
    // Отступление от брифа (решение контроллера): `budget_tokens` — только при профиле
    // без adaptive; до зонда (`thinking: None`) модель считается актуальной.
    let mut req = request();
    req.reasoning = ReasoningMode::Effort(Effort::High);
    let b = body_with(&req, Some(&OLD));
    assert_eq!(b["thinking"]["type"], "enabled");
    assert_eq!(b["thinking"]["budget_tokens"], 16384);
    assert!(b.get("output_config").is_none());
    // max_tokens поднимается выше бюджета.
    assert_eq!(b["max_tokens"], 16384 + 1024);
    // Бюджет не меньше 1024.
    req.reasoning = ReasoningMode::Budget(100);
    let b = body_with(&req, Some(&OLD));
    assert_eq!(b["thinking"]["budget_tokens"], 1024);
    assert_eq!(b["max_tokens"], 8192);
}

#[test]
fn max_effort_follows_profile() {
    let mut req = request();
    req.reasoning = ReasoningMode::Effort(Effort::Max);
    assert_eq!(
        body_with(&req, Some(&ADAPTIVE))["output_config"]["effort"],
        "max"
    );
    let xhigh = ThinkingSupport {
        effort_max: false,
        ..ADAPTIVE
    };
    assert_eq!(
        body_with(&req, Some(&xhigh))["output_config"]["effort"],
        "xhigh"
    );
    let high = ThinkingSupport {
        effort_xhigh: false,
        ..xhigh
    };
    assert_eq!(
        body_with(&req, Some(&high))["output_config"]["effort"],
        "high"
    );
    // До зонда — `max`.
    assert_eq!(body_with(&req, None)["output_config"]["effort"], "max");
}

#[test]
fn no_adaptive_no_budget_sends_no_thinking() {
    // Models API: ни adaptive, ни enabled (модель без мышления) — поле не отправляется.
    let none = ThinkingSupport {
        budget: false,
        ..OLD
    };
    let mut req = request();
    for mode in [
        ReasoningMode::Effort(Effort::High),
        ReasoningMode::Budget(4096),
    ] {
        req.reasoning = mode;
        let b = body_with(&req, Some(&none));
        assert!(b.get("thinking").is_none(), "{b}");
        assert!(!b.to_string().contains("budget_tokens"));
        assert_eq!(b["max_tokens"], 8192);
    }
}

#[test]
fn no_profile_means_adaptive() {
    let mut req = request();
    req.reasoning = ReasoningMode::Effort(Effort::High);
    let b = body_with(&req, None);
    assert_eq!(b["thinking"], json!({"type": "adaptive"}));
    assert_eq!(b["output_config"]["effort"], "high");
    assert!(!b.to_string().contains("budget_tokens"));
    assert_eq!(b["max_tokens"], 8192);
    req.reasoning = ReasoningMode::Budget(30000);
    assert_eq!(body_with(&req, None)["output_config"]["effort"], "max");
}

#[test]
fn budget_with_adaptive_becomes_nearest_effort() {
    let mut req = request();
    req.reasoning = ReasoningMode::Budget(10000);
    let b = body_with(&req, Some(&ADAPTIVE));
    assert_eq!(b["thinking"]["type"], "adaptive");
    assert_eq!(b["output_config"]["effort"], "medium");
}

#[test]
fn default_and_off_send_no_thinking() {
    let req = request();
    for t in [None, Some(&ADAPTIVE), Some(&OLD)] {
        let b = body_with(&req, t);
        assert!(b.get("thinking").is_none());
        assert!(b.get("output_config").is_none());
    }
    let mut off = request();
    off.reasoning = ReasoningMode::Off;
    let b = body_with(&off, Some(&OLD));
    assert!(b.get("thinking").is_none());
    assert!(b.get("output_config").is_none());
    // У адаптивных моделей (и до зонда) `disabled` даёт 400: поле не отправляется,
    // усилие — low.
    for t in [None, Some(&ADAPTIVE)] {
        let b = body_with(&off, t);
        assert!(b.get("thinking").is_none());
        assert_eq!(b["output_config"]["effort"], "low");
    }
    // Без поддержки effort output_config не отправляется.
    let no_effort = ThinkingSupport {
        effort: false,
        ..ADAPTIVE
    };
    assert!(
        body_with(&off, Some(&no_effort))
            .get("output_config")
            .is_none()
    );
}

#[test]
fn max_tokens_default_from_profile_or_8192() {
    let caps = caps();
    let mut c = ctx(&caps);
    c.max_output_tokens = Some(32000);
    let b = wire("http://x").build_body(&request(), &c).unwrap();
    assert_eq!(b["max_tokens"], 32000);
    c.max_output_tokens = None;
    let b = wire("http://x").build_body(&request(), &c).unwrap();
    assert_eq!(b["max_tokens"], 8192);
    let mut req = request();
    req.max_output_tokens = Some(500);
    c.max_output_tokens = Some(32000);
    assert_eq!(
        wire("http://x").build_body(&req, &c).unwrap()["max_tokens"],
        500
    );
}

#[test]
fn tool_results_of_one_turn_are_one_user_message() {
    let mut req = request();
    req.tools = vec![tool("read_file")];
    req.messages.push(Message {
        role: Role::Assistant,
        parts: vec![
            Part::ToolCall(call("toolu_a")),
            Part::ToolCall(call("toolu_b")),
        ],
    });
    for (id, err) in [("toolu_a", false), ("toolu_b", true)] {
        req.messages.push(Message {
            role: Role::Tool,
            parts: vec![Part::ToolResult {
                call_id: id.into(),
                content: format!("результат {id}"),
                is_error: err,
            }],
        });
    }
    let b = body_with(&req, None);
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    let asst = &msgs[1];
    assert_eq!(asst["role"], "assistant");
    assert_eq!(asst["content"][0]["type"], "tool_use");
    assert_eq!(asst["content"][0]["id"], "toolu_a");
    assert_eq!(asst["content"][0]["input"], json!({"path": "toolu_a"}));
    assert_eq!(asst["content"][1]["id"], "toolu_b");
    let last = msgs.last().unwrap();
    assert_eq!(last["role"], "user");
    let content = last["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "tool_result");
    assert_eq!(content[0]["tool_use_id"], "toolu_a");
    assert_eq!(content[0]["content"], "результат toolu_a");
    assert!(content[0].get("is_error").is_none());
    assert_eq!(content[1]["type"], "tool_result");
    assert_eq!(content[1]["tool_use_id"], "toolu_b");
    assert_eq!(content[1]["is_error"], true);
}

fn reasoning_message(model: &str) -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![
            Part::Reasoning {
                text: "Нужно прочитать README.".into(),
                replay: Some(ReplayBlob {
                    wire: Wire::AnthropicMessages,
                    model: model.into(),
                    data: json!({
                        "signature": "sig-1",
                        "blocks": [
                            {"type": "thinking", "thinking": "Нужно прочитать README.", "signature": "sig-1"},
                            {"type": "text", "text": "Читаю файл."},
                            {"type": "tool_use", "id": "toolu_a"}
                        ]
                    }),
                }),
            },
            Part::Text("Читаю файл.".into()),
            Part::ToolCall(call("toolu_a")),
        ],
    }
}

#[test]
fn foreign_reasoning_blob_is_sent_unchanged() {
    // Решение контроллера (отменяет фильтр плана): блоки мышления другой модели
    // передаются без изменений, нечитаемые API отбрасывает сам.
    let mut req = request();
    req.messages.push(reasoning_message("claude-opus-5"));
    let b = body_with(&req, None);
    let asst = &b["messages"][1]["content"];
    assert_eq!(
        asst[0],
        json!({"type": "thinking", "thinking": "Нужно прочитать README.", "signature": "sig-1"})
    );
    assert_eq!(asst[1], json!({"type": "text", "text": "Читаю файл."}));
    assert_eq!(asst[2]["type"], "tool_use");
    // Данные другого протокола не отправляются.
    let mut req = request();
    let mut m = reasoning_message(MODEL);
    if let Part::Reasoning {
        replay: Some(b), ..
    } = &mut m.parts[0]
    {
        b.wire = Wire::Responses;
    }
    req.messages.push(m);
    let s = body_with(&req, None)["messages"].to_string();
    assert!(!s.contains("sig-1"));
}

#[test]
fn own_reasoning_blob_is_sent_unchanged_in_original_order() {
    let mut req = request();
    req.messages.push(reasoning_message(MODEL));
    let b = body_with(&req, None);
    let asst = &b["messages"][1]["content"];
    assert_eq!(
        asst[0],
        json!({"type": "thinking", "thinking": "Нужно прочитать README.", "signature": "sig-1"})
    );
    assert_eq!(asst[1], json!({"type": "text", "text": "Читаю файл."}));
    assert_eq!(asst[2]["type"], "tool_use");
    assert_eq!(asst[2]["id"], "toolu_a");
    assert_eq!(asst[2]["name"], "read_file");
    assert_eq!(asst.as_array().unwrap().len(), 3);
}

#[test]
fn cache_key_marks_tools_and_system() {
    let mut req = request();
    req.tools = vec![tool("b"), tool("a")];
    let b = body_with(&req, None);
    assert_eq!(b["system"], "Ты агент.");
    assert!(!b.to_string().contains("cache_control"));
    req.cache_key = Some("sess-1".into());
    let b = body_with(&req, None);
    let tools = b["tools"].as_array().unwrap();
    // Порядок канонический (render_tools), метка только на последнем.
    assert_eq!(tools[0]["name"], "a");
    assert_eq!(tools[1]["name"], "b");
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(tools[1]["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(
        tools[1]["input_schema"],
        json!({"type": "object", "properties": {}})
    );
    assert_eq!(
        b["system"],
        json!([{"type": "text", "text": "Ты агент.", "cache_control": {"type": "ephemeral"}}])
    );
    assert!(!b.to_string().contains("sess-1"));
}

#[test]
fn tool_choice_auto_none_and_emulated_required() {
    let mut req = request();
    req.tools = vec![tool("read_file"), tool("write_file")];
    let b = body_with(&req, None);
    assert_eq!(b["tool_choice"], json!({"type": "auto"}));
    req.tool_choice = ToolChoice::None;
    assert_eq!(
        body_with(&req, None)["tool_choice"],
        json!({"type": "none"})
    );

    // Актуальные модели (adaptive) и профиль до зонда: принудительный выбор даёт 400,
    // поэтому `auto` и инструкция отдельным system-сообщением в конце `messages`;
    // существующие сообщения не меняются (preserved thinking).
    req.tool_choice = ToolChoice::Named("read_file".into());
    for t in [None, Some(&ADAPTIVE)] {
        let b = body_with(&req, t);
        assert_eq!(b["tool_choice"], json!({"type": "auto"}));
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(
            msgs[0],
            json!({"role": "user", "content": [{"type": "text", "text": "Привет"}]})
        );
        assert_eq!(msgs[1]["role"], "system");
        assert_eq!(msgs[1]["clear_at"], "next_user_message");
        assert!(msgs[1].get("cache_control").is_none());
        assert!(msgs[1]["content"].as_str().unwrap().contains("read_file"));
    }
    req.tool_choice = ToolChoice::Required;
    let b = body_with(&req, Some(&ADAPTIVE));
    assert_eq!(b["tool_choice"], json!({"type": "auto"}));
    assert_eq!(b["messages"][0]["content"].as_array().unwrap().len(), 1);
    assert_eq!(b["messages"][1]["role"], "system");

    // Старая модель без мышления в запросе: принудительный выбор отправляется.
    let b = body_with(&req, Some(&OLD));
    assert_eq!(b["tool_choice"], json!({"type": "any"}));
    assert_eq!(b["messages"][0]["content"].as_array().unwrap().len(), 1);
    req.tool_choice = ToolChoice::Named("read_file".into());
    assert_eq!(
        body_with(&req, Some(&OLD))["tool_choice"],
        json!({"type": "tool", "name": "read_file"})
    );
    // С мышлением принудительный выбор несовместим — эмуляция.
    req.reasoning = ReasoningMode::Effort(Effort::Low);
    assert_eq!(
        body_with(&req, Some(&OLD))["tool_choice"],
        json!({"type": "auto"})
    );
}

#[test]
fn instruction_without_mid_system_support_is_user_text() {
    let caps = caps();
    let c = ctx(&caps);
    let mut req = request();
    req.tools = vec![tool("read_file")];
    req.tool_choice = ToolChoice::Named("read_file".into());
    // Sonnet 5 и Haiku 4.5 не принимают system-сообщений в `messages`.
    for model in ["claude-sonnet-5", "claude-haiku-4-5"] {
        let w = AnthropicWire {
            base_url: "http://x".into(),
            model: model.into(),
        };
        let b = w.build_body(&req, &c).unwrap();
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "{model}");
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert!(content[1]["text"].as_str().unwrap().contains("read_file"));
    }
    // Снимок по дате и префикс Bedrock — та же модель.
    for model in ["claude-opus-5-5-20260401", "anthropic.claude-sonnet-5-5"] {
        let w = AnthropicWire {
            base_url: "http://x".into(),
            model: model.into(),
        };
        let b = w.build_body(&req, &c).unwrap();
        assert_eq!(b["messages"][1]["role"], "system", "{model}");
    }
    // Последним идёт ассистент: system-сообщение туда нельзя — новое сообщение user.
    req.messages.push(Message {
        role: Role::Assistant,
        parts: vec![Part::Text("Сейчас.".into())],
    });
    let b = wire("http://x").build_body(&req, &c).unwrap();
    let last = b["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["role"], "user");
}

#[tokio::test]
async fn tool_use_input_is_returned_byte_for_byte() {
    let raw = r#"{"z": 1,  "a" :"x",
 "m":[2, 1]}"#;
    let mut req = request();
    req.messages.push(Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall(ToolCall {
            id: "toolu_raw".into(),
            name: "read_file".into(),
            arguments: serde_json::from_str(raw).unwrap(),
            raw_arguments: Some(raw.into()),
        })],
    });
    // Аргументы изменены после разбора (ремонт нормализации): исходный текст не годится.
    let changed = ToolCall {
        id: "toolu_new".into(),
        name: "read_file".into(),
        arguments: json!({"b": 2, "a": 1}),
        raw_arguments: Some(r#"{"b": 2}"#.into()),
    };
    req.messages.push(Message {
        role: Role::Tool,
        parts: vec![Part::ToolResult {
            call_id: "toolu_raw".into(),
            content: "ok".into(),
            is_error: false,
        }],
    });
    req.messages.push(Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall(changed)],
    });
    let srv = FakeServer::start(vec![(
        ROUTE,
        vec![Reply::sse(vec![fixture("anthropic/text.sse")])],
    )])
    .await;
    let caps = caps();
    let w = wire(&srv.url("/v1"));
    drain(w.open(&transport(), &req, &ctx(&caps)).await.unwrap()).await;
    let sent = String::from_utf8(srv.requests(ROUTE)[0].body.clone()).unwrap();
    assert!(sent.contains(&format!(r#""input":{raw}"#)), "{sent}");
    assert!(sent.contains(r#""input":{"a":1,"b":2}"#), "{sent}");
    // Тело — тот же JSON, что `build_body`.
    let parsed: Value = serde_json::from_str(&sent).unwrap();
    assert_eq!(parsed, w.build_body(&req, &ctx(&caps)).unwrap());
}

#[test]
fn tool_choice_errors_before_send() {
    let caps = caps();
    let c = ctx(&caps);
    let mut req = request();
    req.tool_choice = ToolChoice::Required;
    let e = wire("http://x").build_body(&req, &c).unwrap_err();
    assert_eq!(e.class, ErrorClass::BadRequest);
    req.tools = vec![tool("a")];
    req.tool_choice = ToolChoice::Named("нет".into());
    let e = wire("http://x").build_body(&req, &c).unwrap_err();
    assert_eq!(e.class, ErrorClass::BadRequest);
    req.tool_choice = ToolChoice::Auto;
    req.tools = vec![tool("a"), tool("a")];
    let e = wire("http://x").build_body(&req, &c).unwrap_err();
    assert_eq!(e.class, ErrorClass::BadRequest);
}

#[test]
fn parallel_tools_off_disables_parallel_use() {
    let mut caps = caps();
    caps.parallel_tools = false;
    let mut req = request();
    req.tools = vec![tool("a")];
    let b = wire("http://x").build_body(&req, &ctx(&caps)).unwrap();
    assert_eq!(
        b["tool_choice"],
        json!({"type": "auto", "disable_parallel_tool_use": true})
    );
}

fn strict_of(parameters: Value) -> Option<Value> {
    let mut caps = caps();
    caps.strict_schema = true;
    let mut req = request();
    req.tools = vec![ToolSchema {
        name: "t".into(),
        description: "d".into(),
        parameters,
    }];
    wire("http://x").build_body(&req, &ctx(&caps)).unwrap()["tools"][0]
        .get("strict")
        .cloned()
}

#[test]
fn strict_only_for_anthropic_compatible_schema() {
    let ok = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "format": "uri", "description": "путь"},
            "list": {"type": "array", "minItems": 1, "items": {"$ref": "#/$defs/item"}},
            "mode": {"enum": ["r", "w"], "default": "r"}
        },
        "required": ["path", "list", "mode"],
        "additionalProperties": false,
        "$defs": {"item": {"type": "object", "properties": {"x": {"type": "string"}},
                           "required": ["x"], "additionalProperties": false}}
    });
    assert_eq!(strict_of(ok.clone()), Some(json!(true)));
    // Числовые и строковые ограничения не поддерживаются.
    let mut bad = ok.clone();
    bad["properties"]["path"]["maxLength"] = json!(10);
    assert_eq!(strict_of(bad), None);
    let mut bad = ok.clone();
    bad["properties"]["n"] = json!({"type": "integer", "minimum": 0});
    bad["required"] = json!(["path", "list", "mode", "n"]);
    assert_eq!(strict_of(bad), None);
    // minItems только 0 и 1.
    let mut bad = ok.clone();
    bad["properties"]["list"]["minItems"] = json!(2);
    assert_eq!(strict_of(bad), None);
    // Без additionalProperties: false.
    let mut bad = ok.clone();
    bad.as_object_mut().unwrap().remove("additionalProperties");
    assert_eq!(strict_of(bad), None);
    // Рекурсия через $defs.
    let mut bad = ok.clone();
    bad["$defs"]["item"]["properties"]["next"] = json!({"$ref": "#/$defs/item"});
    bad["$defs"]["item"]["required"] = json!(["x", "next"]);
    assert_eq!(strict_of(bad), None);
    // Без caps.strict_schema strict не отправляется вовсе.
    let mut req = request();
    req.tools = vec![ToolSchema {
        name: "t".into(),
        description: "d".into(),
        parameters: ok,
    }];
    assert!(body_with(&req, None)["tools"][0].get("strict").is_none());
}

// ---------- Стрим ----------

#[tokio::test]
async fn thinking_signature_and_tool_use_roundtrip() {
    let evs = events_of("anthropic/thinking_tool_use.sse").await;
    assert!(evs.contains(&StreamEvent::ReasoningDelta("Нужно прочитать ".into())));
    assert!(evs.contains(&StreamEvent::ToolCallStart {
        index: 0,
        id: "toolu_01T1x1fJ34qAmk2tNTrN7Up6".into(),
        name: "read_file".into(),
    }));
    let (msg, finish, usage) = finished(&evs);
    assert_eq!(finish, &FinishReason::ToolCalls);
    assert_eq!(usage.input, 472);
    assert_eq!(usage.output, 89);
    let Part::Reasoning {
        text,
        replay: Some(blob),
    } = &msg.parts[0]
    else {
        panic!("первая часть не Reasoning с replay: {:?}", msg.parts[0]);
    };
    assert_eq!(text, "Нужно прочитать README.");
    assert_eq!(blob.wire, Wire::AnthropicMessages);
    assert_eq!(blob.model, MODEL);
    assert_eq!(
        blob.data["signature"],
        "EqQBCgIYAhIM1gbcDa9GJwZA2b3hGgxBdjrkzLoky3dl1pkiMOYds"
    );
    assert_eq!(msg.parts[1], Part::Text("Читаю файл.".into()));
    let Part::ToolCall(c) = &msg.parts[2] else {
        panic!("третья часть не ToolCall: {:?}", msg.parts[2]);
    };
    assert!(c.id.starts_with("toolu_"));
    assert_eq!(c.name, "read_file");
    assert_eq!(c.arguments, json!({"path": "README.md"}));

    // Возврат: собранное сообщение уходит назад с тем же блоком thinking и порядком.
    let mut req = request();
    req.messages.push(msg.clone());
    let b = body_with(&req, Some(&ADAPTIVE));
    let asst = &b["messages"][1]["content"];
    assert_eq!(
        asst[0],
        json!({"type": "thinking", "thinking": "Нужно прочитать README.",
               "signature": "EqQBCgIYAhIM1gbcDa9GJwZA2b3hGgxBdjrkzLoky3dl1pkiMOYds"})
    );
    assert_eq!(asst[1], json!({"type": "text", "text": "Читаю файл."}));
    assert_eq!(asst[2]["id"], "toolu_01T1x1fJ34qAmk2tNTrN7Up6");
    assert_eq!(asst[2]["input"], json!({"path": "README.md"}));
    let caps2 = caps();
    let mut c2 = ctx(&caps2);
    c2.thinking = Some(&ADAPTIVE);
    let sent = wire("http://x")
        .body_text(&req, &c2)
        .unwrap()
        .get()
        .to_owned();
    assert!(sent.contains(r#""input":{"path": "README.md"}"#), "{sent}");
}

#[tokio::test]
async fn cache_read_tokens_map_to_cached() {
    let evs = events_of("anthropic/text.sse").await;
    let (msg, finish, usage) = finished(&evs);
    // `Usage.input` — весь вход: `input_tokens` 12 + чтение кэша 2048 + запись 100.
    assert_eq!(usage.input, 12 + 2048 + 100);
    assert_eq!(usage.cached, 2048);
    assert_eq!(usage.cache_write, 100);
    assert_eq!(usage.output, 15);
    assert_eq!(finish, &FinishReason::Stop);
    assert_eq!(msg.parts, vec![Part::Text("Привет!".into())]);
    assert_eq!(
        evs.iter()
            .filter(|e| matches!(e, StreamEvent::TextDelta(_)))
            .count(),
        2
    );
}

#[tokio::test]
async fn overloaded_event_midstream() {
    let rs = results_of("anthropic/overloaded_midstream.sse").await;
    assert_eq!(rs[0], Ok(StreamEvent::TextDelta("Нач".into())));
    let last = rs.last().unwrap().as_ref().unwrap_err();
    assert_eq!(last.class, ErrorClass::Overloaded);
    assert_eq!(rs.iter().filter(|r| r.is_err()).count(), 1);
}

#[tokio::test]
async fn refusal_is_finish_reason_not_error() {
    let rs = results_of("anthropic/refusal.sse").await;
    assert!(rs.iter().all(Result::is_ok), "{rs:?}");
    match rs.last() {
        Some(Ok(StreamEvent::Finished {
            finish, message, ..
        })) => {
            assert_eq!(finish, &FinishReason::Refusal);
            assert!(message.parts.is_empty());
        }
        other => panic!("последнее не Finished: {other:?}"),
    }
}

#[tokio::test]
async fn headers_carry_key_and_version() {
    let key = Secret::new("sk-ant-test-123".into());
    let (srv, r) = open_with(Reply::sse(vec![fixture("anthropic/text.sse")]), Some(&key)).await;
    drain(r.unwrap()).await;
    let reqs = srv.requests(ROUTE);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path_and_query, "/v1/messages");
    assert_eq!(reqs[0].header("x-api-key"), Some("sk-ant-test-123"));
    assert_eq!(reqs[0].header("anthropic-version"), Some("2026-10-21"));
    assert!(
        reqs[0]
            .header("content-type")
            .is_some_and(|c| c.starts_with("application/json"))
    );
    assert!(reqs[0].header("authorization").is_none());
    assert!(reqs[0].header("anthropic-beta").is_none());
    assert_eq!(reqs[0].json()["stream"], true);
}

#[tokio::test]
async fn http_error_is_err_from_open() {
    let reply = Reply::Json(
        529,
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    );
    let (_srv, r) = open_with(reply, None).await;
    assert_eq!(r.unwrap_err().class, ErrorClass::Overloaded);
}

#[tokio::test]
async fn message_stop_without_trailing_newline_is_clean_end() {
    let mut bytes = fixture("anthropic/text.sse");
    while bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    let (_srv, r) = open_with(Reply::sse(vec![bytes]), None).await;
    let rs = drain(r.unwrap()).await;
    assert!(rs.iter().all(Result::is_ok), "{rs:?}");
    assert!(matches!(rs.last(), Some(Ok(StreamEvent::Finished { .. }))));
}

#[tokio::test]
async fn eof_before_message_stop_is_stream_cut() {
    let text = String::from_utf8(fixture("anthropic/text.sse")).unwrap();
    let cut = text
        .split("event: message_delta")
        .next()
        .unwrap()
        .to_owned();
    let (_srv, r) = open_with(Reply::sse(vec![cut.into_bytes()]), None).await;
    let rs = drain(r.unwrap()).await;
    assert_eq!(
        rs.last().unwrap().as_ref().unwrap_err().class,
        ErrorClass::StreamCut
    );
}

// ---------- Декодер ----------

#[test]
fn decoder_stop_reasons() {
    for (reason, want) in [
        ("end_turn", FinishReason::Stop),
        ("stop_sequence", FinishReason::Stop),
        ("tool_use", FinishReason::ToolCalls),
        ("max_tokens", FinishReason::Length),
        ("refusal", FinishReason::Refusal),
        ("pause_turn", FinishReason::Other("pause_turn".into())),
    ] {
        let mut d = AnthropicDecoder::new(MODEL.into());
        d.on_event(&sse(json!({"type": "message_delta",
            "delta": {"stop_reason": reason}, "usage": {"output_tokens": 1}})))
            .unwrap();
        let evs = d.on_event(&sse(json!({"type": "message_stop"}))).unwrap();
        match evs.as_slice() {
            [StreamEvent::Finished { finish, .. }] => assert_eq!(finish, &want),
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn clear_at_instruction_carries_beta_header() {
    let srv = FakeServer::start(vec![(
        ROUTE,
        vec![Reply::sse(vec![fixture("anthropic/text.sse")])],
    )])
    .await;
    let caps = self::caps();
    let mut req = request();
    req.tools = vec![tool("read_file")];
    req.tool_choice = ToolChoice::Named("read_file".into());
    let w = wire(&srv.url("/v1"));
    drain(w.open(&transport(), &req, &ctx(&caps)).await.unwrap()).await;
    let r = &srv.requests(ROUTE)[0];
    assert_eq!(r.header("anthropic-beta"), Some(CLEAR_AT_BETA));
    assert_eq!(CLEAR_AT_BETA, "mid-conversation-system-clear-at-2026-08-21");
    let msgs = r.json()["messages"].clone();
    assert_eq!(
        msgs[1],
        json!({"role": "system", "clear_at": "next_user_message",
               "content": "Ответь вызовом инструмента `read_file`."})
    );
    // Погашенное сообщение из истории (R1-B пересылает его без изменений) тоже
    // требует заголовка; запасной путь без clear_at — без заголовка.
    let c = ctx(&caps);
    let hist = json!({"messages": [{"role": "user", "content": "a"},
        {"role": "system", "clear_at": "next_user_message", "content": "x"},
        {"role": "assistant", "content": "b"}]});
    let h = w.headers(&c, &hist).unwrap();
    assert_eq!(h.get("anthropic-beta").unwrap(), CLEAR_AT_BETA);
    let old = AnthropicWire {
        base_url: "http://x".into(),
        model: "claude-sonnet-5".into(),
    };
    let b = old.build_body(&req, &c).unwrap();
    assert!(old.headers(&c, &b).unwrap().get("anthropic-beta").is_none());
}

#[test]
fn context_window_stop_is_truncated_length() {
    // Обрезанный, но валидный ответ: Length, расход и частичный текст сохраняются.
    let mut d = AnthropicDecoder::new(MODEL.into());
    d.on_event(&sse(json!({"type": "message_start", "message": {"usage":
        {"input_tokens": 900, "output_tokens": 1}}})))
        .unwrap();
    d.on_event(&sse(json!({"type": "content_block_start", "index": 0,
        "content_block": {"type": "text", "text": ""}})))
        .unwrap();
    d.on_event(&sse(json!({"type": "content_block_delta", "index": 0,
        "delta": {"type": "text_delta", "text": "Начало ответа"}})))
        .unwrap();
    d.on_event(&sse(json!({"type": "message_delta",
        "delta": {"stop_reason": "model_context_window_exceeded"},
        "usage": {"output_tokens": 10}})))
        .unwrap();
    let evs = d.on_event(&sse(json!({"type": "message_stop"}))).unwrap();
    let Some(StreamEvent::Finished {
        message,
        finish,
        usage,
    }) = evs.last()
    else {
        panic!("{evs:?}");
    };
    assert_eq!(finish, &FinishReason::Length);
    assert_eq!(message.parts, vec![Part::Text("Начало ответа".into())]);
    assert_eq!(usage.input, 900);
    assert_eq!(usage.output, 10);
}

#[test]
fn thinking_tokens_go_to_usage_reasoning() {
    let mut d = AnthropicDecoder::new(MODEL.into());
    d.on_event(&sse(json!({"type": "message_start", "message": {"usage":
        {"input_tokens": 25, "output_tokens": 1}}})))
        .unwrap();
    d.on_event(&sse(
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
        "usage": {"output_tokens": 348, "output_tokens_details": {"thinking_tokens": 312}}}),
    ))
    .unwrap();
    let evs = d.on_event(&sse(json!({"type": "message_stop"}))).unwrap();
    let Some(StreamEvent::Finished { usage, .. }) = evs.last() else {
        panic!("{evs:?}");
    };
    assert_eq!(usage.output, 348);
    assert_eq!(usage.reasoning, 312);
    assert_eq!(usage.input, 25);
}

#[test]
fn decoder_error_types_and_garbage() {
    let mut d = AnthropicDecoder::new(MODEL.into());
    assert!(
        d.on_event(&sse(json!({"type": "ping"})))
            .unwrap()
            .is_empty()
    );
    for (t, class) in [
        ("overloaded_error", ErrorClass::Overloaded),
        ("rate_limit_error", ErrorClass::RateLimited),
        ("api_error", ErrorClass::Server),
        ("authentication_error", ErrorClass::Auth),
        ("invalid_request_error", ErrorClass::BadRequest),
    ] {
        let mut d = AnthropicDecoder::new(MODEL.into());
        let e = d
            .on_event(&sse(
                json!({"type": "error", "error": {"type": t, "message": "x"}}),
            ))
            .unwrap_err();
        assert_eq!(e.class, class, "{t}");
    }
    let mut d = AnthropicDecoder::new(MODEL.into());
    let e = d
        .on_event(&SseEvent {
            event: None,
            data: "{не json".into(),
            id: None,
        })
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::Malformed);
}

#[test]
fn decoder_redacted_thinking_goes_to_replay_whole() {
    let mut d = AnthropicDecoder::new(MODEL.into());
    let block = json!({"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix"});
    d.on_event(&sse(
        json!({"type": "content_block_start", "index": 0, "content_block": block}),
    ))
    .unwrap();
    d.on_event(&sse(json!({"type": "content_block_stop", "index": 0})))
        .unwrap();
    d.on_event(&sse(
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
    ))
    .unwrap();
    let evs = d.on_event(&sse(json!({"type": "message_stop"}))).unwrap();
    let Some(StreamEvent::Finished { message, .. }) = evs.last() else {
        panic!("{evs:?}");
    };
    let Part::Reasoning {
        replay: Some(blob), ..
    } = &message.parts[0]
    else {
        panic!("{:?}", message.parts);
    };
    assert_eq!(blob.data["blocks"][0], block);
}

// ---------- Роль System и служебные сообщения (R1-B1, D10) ----------

fn system_msg(text: &str) -> Message {
    Message {
        role: Role::System,
        parts: vec![Part::Text(text.into())],
    }
}

fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![Part::Text(text.into())],
    }
}

/// `user a, System x, assistant b, user c`.
fn history_with_system() -> ChatRequest {
    ChatRequest {
        system: "Ты агент.".into(),
        messages: vec![user("a"), system_msg("x"), assistant("b"), user("c")],
        ..ChatRequest::default()
    }
}

fn named_read_file() -> ChatRequest {
    let mut req = request();
    req.tools = vec![tool("read_file")];
    req.tool_choice = ToolChoice::Named("read_file".into());
    req
}

async fn events_for(model: &str, req: &ChatRequest) -> Vec<StreamEvent> {
    let srv = FakeServer::start(vec![(
        ROUTE,
        vec![Reply::sse(vec![fixture("anthropic/text.sse")])],
    )])
    .await;
    let caps = caps();
    let w = AnthropicWire {
        base_url: srv.url("/v1"),
        model: model.into(),
    };
    let s = w.open(&transport(), req, &ctx(&caps)).await.unwrap();
    drain(s).await.into_iter().map(|x| x.unwrap()).collect()
}

#[tokio::test]
async fn emulated_required_emits_service_message() {
    let evs = events_for(MODEL, &named_read_file()).await;
    assert_eq!(
        evs[0],
        StreamEvent::ServiceMessage(Message {
            role: Role::System,
            parts: vec![Part::Text("Ответь вызовом инструмента `read_file`.".into())],
        })
    );
    let n = evs
        .iter()
        .filter(|e| matches!(e, StreamEvent::ServiceMessage(_)))
        .count();
    assert_eq!(n, 1);
    // Без эмуляции служебных сообщений нет.
    let evs = events_for(MODEL, &request()).await;
    assert!(
        !evs.iter()
            .any(|e| matches!(e, StreamEvent::ServiceMessage(_)))
    );
}

#[test]
fn history_system_rendered_with_clear_at() {
    let caps = caps();
    let c = ctx(&caps);
    let w = wire("http://x");
    let b = w.build_body(&history_with_system(), &c).unwrap();
    assert_eq!(
        b["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "a"}]},
            {"role": "system", "clear_at": "next_user_message", "content": "x"},
            {"role": "assistant", "content": [{"type": "text", "text": "b"}]},
            {"role": "user", "content": [{"type": "text", "text": "c"}]},
        ])
    );
    let h = w.headers(&c, &b).unwrap();
    assert_eq!(h.get("anthropic-beta").unwrap(), CLEAR_AT_BETA);
    // Погашенное сообщение пересылается байт-в-байт (preserved thinking): текст
    // system-сообщения в теле следующего хода тот же, что отправила эмуляция.
    let first = named_read_file();
    let sent = w.body_text(&first, &c).unwrap();
    let parsed: Value = serde_json::from_str(sent.get()).unwrap();
    let service = parsed["messages"][1].to_string();
    assert!(sent.get().contains(&service));
    let mut next = first.clone();
    next.tool_choice = ToolChoice::Auto;
    next.messages.extend([
        system_msg("Ответь вызовом инструмента `read_file`."),
        assistant("b"),
        user("c"),
    ]);
    let resent = w.body_text(&next, &c).unwrap();
    assert!(resent.get().contains(&service), "{}", resent.get());
}

#[test]
fn history_system_does_not_merge_with_neighbours() {
    let caps = caps();
    let req = ChatRequest {
        messages: vec![user("a"), system_msg("x"), system_msg("y"), user("c")],
        ..ChatRequest::default()
    };
    let b = wire("http://x").build_body(&req, &ctx(&caps)).unwrap();
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[1]["content"], "x");
    assert_eq!(msgs[2]["content"], "y");
    assert_eq!(msgs[3]["role"], "user");
}

#[test]
fn history_system_omitted_for_model_without_mid_system() {
    let caps = caps();
    let c = ctx(&caps);
    let w = AnthropicWire {
        base_url: "http://x".into(),
        model: "claude-sonnet-5".into(),
    };
    let b = w.build_body(&history_with_system(), &c).unwrap();
    assert_eq!(
        b["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "a"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "b"}]},
            {"role": "user", "content": [{"type": "text", "text": "c"}]},
        ])
    );
    assert!(w.headers(&c, &b).unwrap().get("anthropic-beta").is_none());
}

#[tokio::test]
async fn text_fallback_emits_no_service_message() {
    let evs = events_for("claude-sonnet-5", &named_read_file()).await;
    let n = evs
        .iter()
        .filter(|e| matches!(e, StreamEvent::ServiceMessage(_)))
        .count();
    assert_eq!(n, 0);
    assert!(matches!(evs.last(), Some(StreamEvent::Finished { .. })));
}
