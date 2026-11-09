//! Коннектор OpenAI Responses: тело запроса, декодер стрима, насос SSE (задача 6).

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use support::{FakeServer, Reply, fixture};
use za_llm::__test::{Connector, OpenedStream, ResponsesDecoder, ResponsesWire, WireCtx};
use za_llm::http::{HttpTransport, SseEvent, Timeouts};
use za_llm::{
    BackendKind, ChatRequest, Effort, ErrorClass, FinishReason, Message, ModelError, Part,
    ProviderCapabilities, ReasoningMode, ReplayBlob, Role, Sampling, Secret, StreamEvent,
    ThinkingSupport, ToolCall, ToolChoice, ToolSchema, Wire, default_caps,
};

const ROUTE: &str = "POST /v1/responses";
const MODEL: &str = "gpt-5.5";
const ENCRYPTED: &str =
    "gAAAAABpZt3kE9c1xQv7Lr0m2YwNfS8uJpHq4aKdTiBoG6eWlXzCnMyRsVOjU5_IhP-0123456789==";

fn wire(base: &str) -> ResponsesWire {
    ResponsesWire {
        base_url: base.to_owned(),
        model: MODEL.to_owned(),
    }
}

fn caps() -> ProviderCapabilities {
    default_caps(BackendKind::OpenAiResponses)
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

fn body(req: &ChatRequest) -> Value {
    let caps = caps();
    wire("http://x").build_body(req, &ctx(&caps)).unwrap()
}

fn body_with(req: &ChatRequest, thinking: &ThinkingSupport) -> Value {
    let caps = caps();
    let mut c = ctx(&caps);
    c.thinking = Some(thinking);
    wire("http://x").build_body(req, &c).unwrap()
}

#[test]
fn profile_gates_reasoning_include_and_sampling() {
    let mut req = request();
    req.reasoning = ReasoningMode::Effort(Effort::Max);
    req.sampling = Sampling {
        temperature: Some(0.5),
        top_p: Some(0.9),
        ..Sampling::default()
    };
    // Рассуждающая модель с `max`: без сэмплирования, `include` есть.
    let gpt6 = za_llm::probe::openai_thinking("gpt-6-astra");
    let b = body_with(&req, &gpt6);
    assert_eq!(b["reasoning"]["effort"], "max");
    assert_eq!(b["include"], json!(["reasoning.encrypted_content"]));
    assert!(b.get("temperature").is_none() && b.get("top_p").is_none());
    // `xhigh` без `max`.
    let gpt52 = za_llm::probe::openai_thinking("gpt-5.2");
    assert_eq!(body_with(&req, &gpt52)["reasoning"]["effort"], "xhigh");
    // Ни того, ни другого — `high`.
    let gpt5 = za_llm::probe::openai_thinking("gpt-5-mini");
    assert_eq!(body_with(&req, &gpt5)["reasoning"]["effort"], "high");
    // Модель без рассуждений: ни `reasoning`, ни `include`, сэмплирование уходит.
    let gpt41 = za_llm::probe::openai_thinking("gpt-4.1");
    let b = body_with(&req, &gpt41);
    assert!(b.get("reasoning").is_none(), "{b}");
    assert!(b.get("include").is_none(), "{b}");
    assert_eq!(b["temperature"], 0.5);
    assert_eq!(b["top_p"], 0.9);
    // До зонда — как раньше: `include`, `temperature`, `Max` → `high`.
    let b = body(&req);
    assert_eq!(b["reasoning"]["effort"], "high");
    assert_eq!(b["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(b["temperature"], 0.5);
}

fn transport() -> HttpTransport {
    HttpTransport::cloud(Timeouts {
        connect: support::scaled(Duration::from_secs(10)),
        first_byte: support::scaled(Duration::from_secs(10)),
        idle: support::scaled(Duration::from_secs(10)),
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
    let w = Connector::Responses(wire(&srv.url("/v1")));
    let r = w.open(&transport(), &request(), &c).await;
    (srv, r)
}

async fn drain(mut s: OpenedStream) -> Vec<Result<StreamEvent, ModelError>> {
    let mut out = Vec::new();
    while let Some(x) =
        tokio::time::timeout(support::scaled(Duration::from_secs(30)), s.events.recv())
            .await
            .expect("канал не закрылся")
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

/// События декодера по порядку; ошибка — паника.
fn decode(evs: &[Value]) -> Vec<StreamEvent> {
    let mut d = ResponsesDecoder::new(MODEL.into());
    evs.iter()
        .flat_map(|e| d.on_event(&sse(e.clone())).unwrap())
        .collect()
}

// ---------- Тело запроса ----------

#[test]
fn body_is_stateless() {
    let b = body(&request());
    assert_eq!(b["store"], false);
    assert_eq!(b["stream"], true);
    assert_eq!(b["model"], MODEL);
    assert!(
        b["include"]
            .as_array()
            .unwrap()
            .contains(&json!("reasoning.encrypted_content"))
    );
    assert_eq!(b["instructions"], "Ты агент.");
    assert_eq!(
        b["input"],
        json!([{"type": "message", "role": "user", "content": "Привет"}])
    );
    assert!(b.get("previous_response_id").is_none());
    assert!(b.get("messages").is_none());
}

#[test]
fn reasoning_field_only_when_requested() {
    let mut req = request();
    for mode in [ReasoningMode::Default, ReasoningMode::Off] {
        req.reasoning = mode;
        assert!(body(&req).get("reasoning").is_none(), "{:?}", req.reasoning);
    }
    req.reasoning = ReasoningMode::Effort(Effort::Low);
    assert_eq!(
        body(&req)["reasoning"],
        json!({"effort": "low", "summary": "auto"})
    );
    req.reasoning = ReasoningMode::Effort(Effort::Max);
    assert_eq!(body(&req)["reasoning"]["effort"], "high");
    req.reasoning = ReasoningMode::Budget(20000);
    assert_eq!(body(&req)["reasoning"]["effort"], "high");
    req.reasoning = ReasoningMode::Budget(3000);
    assert_eq!(body(&req)["reasoning"]["effort"], "low");
}

#[test]
fn tool_call_and_output_items() {
    let mut req = request();
    req.tools = vec![tool("read_file")];
    req.messages.push(Message {
        role: Role::Assistant,
        parts: vec![
            Part::Text("Читаю.".into()),
            Part::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "a"}),
                raw_arguments: None,
            }),
        ],
    });
    req.messages.push(Message {
        role: Role::Tool,
        parts: vec![Part::ToolResult {
            call_id: "call_1".into(),
            content: "содержимое".into(),
            is_error: false,
        }],
    });
    let b = body(&req);
    let input = b["input"].as_array().unwrap();
    assert_eq!(input.len(), 4, "{input:?}");
    assert_eq!(
        input[1],
        json!({"type": "message", "role": "assistant", "content": "Читаю."})
    );
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "call_1");
    assert_eq!(input[2]["name"], "read_file");
    assert_eq!(input[2]["arguments"], r#"{"path":"a"}"#);
    assert_eq!(
        input[3],
        json!({"type": "function_call_output", "call_id": "call_1", "output": "содержимое"})
    );
}

#[test]
fn tools_tool_choice_and_parallel() {
    let mut req = request();
    req.tools = vec![tool("read_file"), tool("write_file")];
    req.tool_choice = ToolChoice::Named("write_file".into());
    let b = body(&req);
    assert_eq!(
        b["tools"][0],
        json!({"type": "function", "name": "read_file", "description": "Инструмент read_file.",
               "parameters": {"type": "object", "properties": {}}, "strict": false})
    );
    assert_eq!(
        b["tool_choice"],
        json!({"type": "function", "name": "write_file"})
    );
    assert_eq!(b["parallel_tool_calls"], true);
    for (choice, want) in [
        (ToolChoice::Auto, "auto"),
        (ToolChoice::None, "none"),
        (ToolChoice::Required, "required"),
    ] {
        req.tool_choice = choice;
        assert_eq!(body(&req)["tool_choice"], want);
    }

    let mut caps = caps();
    caps.parallel_tools = false;
    let b = wire("http://x").build_body(&req, &ctx(&caps)).unwrap();
    assert_eq!(b["parallel_tool_calls"], false);

    // Без инструментов полей инструментов нет.
    let b = body(&request());
    assert!(b.get("tools").is_none());
    assert!(b.get("tool_choice").is_none());
    assert!(b.get("parallel_tool_calls").is_none());
}

#[test]
fn tool_choice_errors_before_send() {
    let caps = caps();
    let w = wire("http://x");
    let mut req = request();
    req.tool_choice = ToolChoice::Required;
    assert_eq!(
        w.build_body(&req, &ctx(&caps)).unwrap_err().class,
        ErrorClass::BadRequest
    );
    req.tools = vec![tool("a")];
    req.tool_choice = ToolChoice::Named("b".into());
    assert_eq!(
        w.build_body(&req, &ctx(&caps)).unwrap_err().class,
        ErrorClass::BadRequest
    );
    req.tools = vec![tool("a"), tool("a")];
    req.tool_choice = ToolChoice::Auto;
    assert_eq!(
        w.build_body(&req, &ctx(&caps)).unwrap_err().class,
        ErrorClass::BadRequest
    );
}

fn strict_of(parameters: Value, strict_caps: bool) -> Value {
    let mut caps = caps();
    caps.strict_schema = strict_caps;
    let mut req = request();
    req.tools = vec![ToolSchema {
        name: "t".into(),
        description: "d".into(),
        parameters,
    }];
    wire("http://x").build_body(&req, &ctx(&caps)).unwrap()["tools"][0]["strict"].clone()
}

#[test]
fn strict_only_for_compatible_schema() {
    let ok = json!({"type": "object", "properties": {"p": {"type": "string"}},
                    "required": ["p"], "additionalProperties": false});
    assert_eq!(strict_of(ok.clone(), true), true);
    // Без strict у эндпоинта и для несовместимой схемы — явный `false`: пропуск поля
    // в Responses включает серверную нормализацию схемы в strict.
    assert_eq!(strict_of(ok, false), false);
    let optional = json!({"type": "object", "properties": {"p": {"type": "string"}},
                          "additionalProperties": false});
    assert_eq!(strict_of(optional, true), false);
    let one_of = json!({"type": "object", "properties": {"p": {"oneOf": [{"type": "string"}]}},
                        "required": ["p"], "additionalProperties": false});
    assert_eq!(strict_of(one_of, true), false);
}

#[test]
fn sampling_cache_key_and_max_output() {
    let mut req = request();
    req.sampling = Sampling {
        temperature: Some(0.1),
        top_p: Some(0.9),
        seed: Some(7),
        presence_penalty: Some(0.5),
        stop: vec!["</end>".into()],
    };
    req.cache_key = Some("sess-1".into());
    req.max_output_tokens = Some(2048);
    let b = body(&req);
    assert_eq!(b["temperature"], json!(0.1));
    assert_eq!(b["top_p"], json!(0.9));
    // Responses не принимает seed, presence_penalty и stop.
    for k in ["seed", "presence_penalty", "stop"] {
        assert!(b.get(k).is_none(), "{k}");
    }
    assert_eq!(b["prompt_cache_key"], "sess-1");
    assert_eq!(b["max_output_tokens"], 2048);
    assert!(body(&request()).get("prompt_cache_key").is_none());
    assert!(body(&request()).get("max_output_tokens").is_none());
}

#[test]
fn foreign_reasoning_blob_is_dropped() {
    let mut req = request();
    for blob in [
        ReplayBlob {
            wire: Wire::Responses,
            model: "gpt-4.1".into(),
            data: json!({"type": "reasoning", "id": "rs_x", "summary": [],
                         "encrypted_content": "zzz"}),
        },
        ReplayBlob {
            wire: Wire::AnthropicMessages,
            model: MODEL.into(),
            data: json!({"signature": "s"}),
        },
    ] {
        req.messages.push(Message {
            role: Role::Assistant,
            parts: vec![
                Part::Reasoning {
                    text: "мысль".into(),
                    replay: Some(blob),
                },
                Part::Text("ответ".into()),
            ],
        });
    }
    let b = body(&req);
    let input = b["input"].as_array().unwrap();
    assert!(input.iter().all(|i| i["type"] != "reasoning"), "{input:?}");
    assert_eq!(input.len(), 3);
}

// ---------- Стрим ----------

#[tokio::test]
async fn text_stream_and_headers() {
    let key = Secret::new("sk-test-123".into());
    let (srv, r) = open_with(Reply::sse(vec![fixture("responses/text.sse")]), Some(&key)).await;
    let rs = drain(r.unwrap()).await;
    let evs: Vec<StreamEvent> = rs.into_iter().map(Result::unwrap).collect();
    assert_eq!(evs[0], StreamEvent::TextDelta("Прив".into()));
    assert_eq!(evs[1], StreamEvent::TextDelta("ет!".into()));
    let (msg, finish, _) = finished(&evs);
    assert_eq!(finish, &FinishReason::Stop);
    assert_eq!(msg.parts, vec![Part::Text("Привет!".into())]);
    let reqs = srv.requests(ROUTE);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path_and_query, "/v1/responses");
    assert_eq!(reqs[0].header("authorization"), Some("Bearer sk-test-123"));
    assert_eq!(reqs[0].json()["store"], false);
}

#[tokio::test]
async fn function_call_stream() {
    let evs = events_of("responses/function_call.sse").await;
    assert!(evs.contains(&StreamEvent::ToolCallStart {
        index: 0,
        id: "call_Q7rT2mX9".into(),
        name: "read_file".into(),
    }));
    let args: String = evs
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallArgs { fragment, .. } => Some(fragment.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(args, r#"{"path":"README.md"}"#);
    let (msg, finish, usage) = finished(&evs);
    assert_eq!(finish, &FinishReason::ToolCalls);
    assert_eq!(usage.input, 310);
    assert_eq!(usage.output, 18);
    let [Part::ToolCall(c)] = msg.parts.as_slice() else {
        panic!("ожидался один ToolCall: {:?}", msg.parts);
    };
    assert_eq!(c.id, "call_Q7rT2mX9");
    assert_eq!(c.name, "read_file");
    assert_eq!(c.arguments, json!({"path": "README.md"}));
}

#[tokio::test]
async fn encrypted_reasoning_is_kept_for_replay() {
    let evs = events_of("responses/reasoning_encrypted.sse").await;
    assert!(evs.contains(&StreamEvent::ReasoningDelta(
        "**Чтение файла**\n\nНужно ".into()
    )));
    let (msg, finish, usage) = finished(&evs);
    assert_eq!(finish, &FinishReason::ToolCalls);
    assert_eq!(usage.reasoning, 64);
    let Part::Reasoning {
        text,
        replay: Some(blob),
    } = &msg.parts[0]
    else {
        panic!("первая часть не Reasoning с replay: {:?}", msg.parts[0]);
    };
    assert_eq!(text, "**Чтение файла**\n\nНужно прочитать README.");
    assert_eq!(blob.wire, Wire::Responses);
    assert_eq!(blob.model, MODEL);
    assert_eq!(blob.data["encrypted_content"], ENCRYPTED);
    assert_eq!(blob.data["id"], "rs_03Rs");
    let Part::ToolCall(c) = &msg.parts[1] else {
        panic!("вторая часть не ToolCall: {:?}", msg.parts[1]);
    };
    assert_eq!(c.id, "call_Rs4kP1");

    // Следующий запрос: элемент reasoning целиком (из output_item.done), за ним вызов
    // с исходной строкой аргументов, затем результат.
    let mut req = request();
    req.tools = vec![tool("read_file")];
    req.messages.push(msg.clone());
    req.messages.push(Message {
        role: Role::Tool,
        parts: vec![Part::ToolResult {
            call_id: "call_Rs4kP1".into(),
            content: "# zero-agent".into(),
            is_error: false,
        }],
    });
    let b = body(&req);
    let input = b["input"].as_array().unwrap();
    assert_eq!(input.len(), 4, "{input:?}");
    assert_eq!(
        input[1],
        json!({"id": "rs_03Rs", "type": "reasoning",
               "summary": [{"type": "summary_text",
                            "text": "**Чтение файла**\n\nНужно прочитать README."}],
               "encrypted_content": ENCRYPTED})
    );
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "call_Rs4kP1");
    assert_eq!(input[2]["arguments"], r#"{"path": "README.md"}"#);
    assert_eq!(input[3]["type"], "function_call_output");
}

#[tokio::test]
async fn usage_from_completed() {
    let evs = events_of("responses/text.sse").await;
    let (_, _, usage) = finished(&evs);
    assert_eq!(usage.input, 1500);
    assert_eq!(usage.cached, 1280);
    assert_eq!(usage.output, 40);
    assert_eq!(usage.reasoning, 32);
}

#[tokio::test]
async fn failed_event_is_error() {
    let rs = results_of("responses/failed.sse").await;
    let last = rs.last().unwrap().as_ref().unwrap_err();
    assert_eq!(last.class, ErrorClass::Server);
    assert!(last.message.contains("server_error"), "{}", last.message);
    assert_eq!(last.http_status, None);
    assert_eq!(rs.iter().filter(|r| r.is_err()).count(), 1);
}

#[tokio::test]
async fn http_error_is_err_from_open() {
    let reply = Reply::Json(
        429,
        json!({"error": {"message": "Rate limit reached", "type": "requests",
                         "code": "rate_limit_exceeded"}}),
    );
    let (_srv, r) = open_with(reply, None).await;
    assert_eq!(r.unwrap_err().class, ErrorClass::RateLimited);
}

#[tokio::test]
async fn completed_without_trailing_newline_is_clean_end() {
    let mut bytes = fixture("responses/text.sse");
    while bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    let (_srv, r) = open_with(Reply::sse(vec![bytes]), None).await;
    let rs = drain(r.unwrap()).await;
    assert!(rs.iter().all(Result::is_ok), "{rs:?}");
    assert!(matches!(rs.last(), Some(Ok(StreamEvent::Finished { .. }))));
}

#[tokio::test]
async fn eof_before_completed_is_stream_cut() {
    let text = String::from_utf8(fixture("responses/text.sse")).unwrap();
    let cut = text
        .split("event: response.completed")
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
fn incomplete_is_length_with_usage() {
    let evs = decode(&[
        json!({"type": "response.output_text.delta", "item_id": "m", "output_index": 0,
               "content_index": 0, "delta": "Обре"}),
        json!({"type": "response.incomplete", "response": {"status": "incomplete",
               "incomplete_details": {"reason": "max_output_tokens"},
               "usage": {"input_tokens": 10, "output_tokens": 16,
                         "output_tokens_details": {"reasoning_tokens": 12}}}}),
    ]);
    let (msg, finish, usage) = finished(&evs);
    assert_eq!(finish, &FinishReason::Length);
    assert_eq!(usage.output, 16);
    assert_eq!(usage.reasoning, 12);
    assert_eq!(msg.parts, vec![Part::Text("Обре".into())]);

    let evs = decode(&[json!({"type": "response.incomplete", "response": {
        "incomplete_details": {"reason": "content_filter"}}})]);
    assert_eq!(finished(&evs).1, &FinishReason::ContentFilter);
}

#[test]
fn decoder_error_event_and_failed_codes() {
    let mut d = ResponsesDecoder::new(MODEL.into());
    let e = d
        .on_event(&sse(json!({"type": "error", "code": "rate_limit_exceeded",
                               "message": "slow down", "param": null})))
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::RateLimited);

    for (code, want) in [
        ("server_error", ErrorClass::Server),
        ("rate_limit_exceeded", ErrorClass::RateLimited),
        ("invalid_prompt", ErrorClass::BadRequest),
        ("bio_policy", ErrorClass::ContentFilter),
        ("vector_store_timeout", ErrorClass::Timeout),
    ] {
        let mut d = ResponsesDecoder::new(MODEL.into());
        let e = d
            .on_event(&sse(json!({"type": "response.failed", "response": {
                "status": "failed", "error": {"code": code, "message": "m"}}})))
            .unwrap_err();
        assert_eq!(e.class, want, "{code}");
    }

    let mut d = ResponsesDecoder::new(MODEL.into());
    let garbage = SseEvent {
        event: None,
        data: "{не json".into(),
        id: None,
    };
    assert_eq!(
        d.on_event(&garbage).unwrap_err().class,
        ErrorClass::Malformed
    );
}

#[test]
fn arguments_from_done_when_no_deltas() {
    let evs = decode(&[
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"id": "fc_1", "type": "function_call", "call_id": "call_a",
                        "name": "ls", "arguments": ""}}),
        json!({"type": "response.output_item.done", "output_index": 0,
               "item": {"id": "fc_1", "type": "function_call", "call_id": "call_a",
                        "name": "ls", "arguments": "{\"dir\":\".\"}"}}),
        json!({"type": "response.completed", "response": {"usage": {}}}),
    ]);
    let (msg, finish, _) = finished(&evs);
    assert_eq!(finish, &FinishReason::ToolCalls);
    let [Part::ToolCall(c)] = msg.parts.as_slice() else {
        panic!("{:?}", msg.parts);
    };
    assert_eq!(c.arguments, json!({"dir": "."}));
}

#[test]
fn several_reasoning_items_keep_order_on_replay() {
    let rs = |id: &str| json!({"id": id, "type": "reasoning", "summary": [], "encrypted_content": format!("enc-{id}")});
    let call = |i: u32, id: &str| {
        json!({"id": format!("fc_{id}"), "type": "function_call", "call_id": id,
               "name": "ls", "arguments": "{}"})
        .as_object()
        .map(|o| (i, Value::Object(o.clone())))
        .unwrap()
    };
    let (i1, c1) = call(1, "call_1");
    let (i3, c3) = call(3, "call_2");
    let evs = decode(&[
        json!({"type": "response.output_item.done", "output_index": 0, "item": rs("rs_1")}),
        json!({"type": "response.output_item.added", "output_index": i1, "item": c1}),
        json!({"type": "response.output_item.done", "output_index": i1, "item": c1}),
        json!({"type": "response.output_item.done", "output_index": 2, "item": rs("rs_2")}),
        json!({"type": "response.output_item.added", "output_index": i3, "item": c3}),
        json!({"type": "response.output_item.done", "output_index": i3, "item": c3}),
        json!({"type": "response.completed", "response": {}}),
    ]);
    let (msg, _, _) = finished(&evs);
    let mut req = request();
    req.tools = vec![tool("ls")];
    req.messages.push(msg.clone());
    let b = body(&req);
    let kinds: Vec<String> = b["input"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|i| {
            format!(
                "{}:{}",
                i["type"].as_str().unwrap(),
                i["id"].as_str().or(i["call_id"].as_str()).unwrap()
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "reasoning:rs_1",
            "function_call:call_1",
            "reasoning:rs_2",
            "function_call:call_2"
        ]
    );
}

#[test]
fn refusal_is_finish_reason() {
    let evs = decode(&[
        json!({"type": "response.refusal.delta", "item_id": "m", "output_index": 0,
               "content_index": 0, "delta": "Не могу помочь."}),
        json!({"type": "response.completed", "response": {}}),
    ]);
    let (msg, finish, _) = finished(&evs);
    assert_eq!(finish, &FinishReason::Refusal);
    assert_eq!(msg.parts, vec![Part::Text("Не могу помочь.".into())]);
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
    let b = body(&req);
    let input = b["input"].as_array().unwrap();
    assert!(
        !b.to_string().contains(r#""x""#),
        "служебное сообщение в теле: {b}"
    );
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["content"], "a");
    assert_eq!(input[1]["content"], "b");
}
