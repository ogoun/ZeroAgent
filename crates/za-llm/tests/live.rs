//! Живые проверки бэкендов через `Gateway` (задача 16). Все тесты `#[ignore]`: нужны
//! запущенный сервер или ключ облака, адреса и ключи — из переменных окружения:
//!
//! | Тест | Переменные |
//! |---|---|
//! | `live_llama` | `LLAMA_TEST_URL` (корень сервера), `ZA_TEST_LLAMA_MODEL` — необязательно |
//! | `live_vllm` | `VLLM_TEST_URL` (корень версии API, `…/v1`), `ZA_TEST_VLLM_MODEL` |
//! | `live_openrouter` | `OPENROUTER_TEST_KEY`, `ZA_TEST_OPENROUTER_MODEL` |
//! | `live_anthropic` | `ANTHROPIC_TEST_KEY`, `ZA_TEST_ANTHROPIC_MODEL` |
//! | `live_openai` | `OPENAI_TEST_KEY`, `ZA_TEST_OPENAI_MODEL` (OpenAI Responses) |
//!
//! Без переменных тест печатает «пропущен» и проходит. Запуск:
//! `LLAMA_TEST_URL=http://127.0.0.1:8080 cargo test -p za-llm --test live -- --ignored`.
//!
//! Каждый тест задаёт короткий вопрос с одним инструментом `echo` и проверяет: вызов
//! `echo` есть — нативный или извлечённый нормализацией из текста (MDL-005); `usage.input
//! > 0`. Для llama.cpp второй такой же запрос даёт `usage.cached > 0` (MDL-011).

#[path = "../../../test-support/unique_dir.rs"]
mod unique_dir;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use za_llm::config::ModelsConfig;
use za_llm::http::{Secret, SecretSource};
use za_llm::normalize::normalize;
use za_llm::profile::ProfileStore;
use za_llm::{
    ChatRequest, EndpointId, Gateway, GatewayOptions, Message, ModelCall, ModelPort, ModelResponse,
    Part, RequestMeta, Role, ToolChoice, ToolSchema,
};
use za_protocol::{Priority, RunId};

struct MapSecrets(HashMap<String, String>);

impl SecretSource for MapSecrets {
    fn get(&self, key_ref: &str) -> Option<Secret> {
        self.0.get(key_ref).cloned().map(Secret::new)
    }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn echo_tool() -> ToolSchema {
    ToolSchema {
        name: "echo".into(),
        description: "Вернуть переданный текст без изменений.".into(),
        parameters: json!({
            "type": "object",
            "properties": {"text": {"type": "string", "description": "Текст"}},
            "required": ["text"],
            "additionalProperties": false
        }),
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        system: "Ты проверочный агент. Отвечай только вызовом инструмента.".into(),
        tools: vec![echo_tool()],
        messages: vec![Message {
            role: Role::User,
            parts: vec![Part::Text(
                "Вызови инструмент echo с аргументом text = \"привет\".".into(),
            )],
        }],
        tool_choice: ToolChoice::Auto,
        max_output_tokens: Some(2048),
        ..ChatRequest::default()
    }
}

/// Gateway с одним эндпоинтом `live`; ждёт зонда и требует `ready`.
async fn gateway(name: &str, endpoint: Value, keys: &[(&str, String)]) -> Gateway {
    let cfg: ModelsConfig = serde_json::from_value(json!({"endpoints": [endpoint]})).unwrap();
    let dir = unique_dir::unique_dir("live", name);
    let secrets = MapSecrets(
        keys.iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
    );
    let g = Gateway::start(
        cfg,
        Arc::new(secrets),
        None,
        ProfileStore::open(&dir).unwrap(),
        GatewayOptions::default(),
    )
    .await
    .unwrap();
    assert!(
        g.wait_probed(Duration::from_secs(60)).await,
        "зонд не завершён"
    );
    let v = &g.endpoints()[0];
    assert_eq!(v.state, "ready", "эндпоинт не готов: {v:?}");
    eprintln!(
        "{name}: модель {:?}, контекст {:?}, слотов {:?}",
        v.model, v.context_tokens, v.slots
    );
    g
}

/// Один запрос; печатает время до первого текста или вызова и usage.
async fn ask(g: &Gateway, name: &str) -> ModelResponse {
    let call = ModelCall {
        endpoint: EndpointId("live".into()),
        meta: RequestMeta {
            run_id: RunId::new(),
            priority: Priority::Interactive,
            session: None,
            max_wait: None,
        },
        request: request(),
    };
    let started = Instant::now();
    let mut s = g.stream(call).await.unwrap();
    let mut first: Option<Duration> = None;
    let resp = loop {
        let ev = s.next().await.expect("стрим кончился без итога").unwrap();
        match ev {
            za_llm::StreamEvent::TextDelta(_)
            | za_llm::StreamEvent::ReasoningDelta(_)
            | za_llm::StreamEvent::ToolCallStart { .. } => {
                first.get_or_insert_with(|| started.elapsed());
            }
            za_llm::StreamEvent::Finished {
                message,
                finish,
                usage,
            } => {
                break ModelResponse {
                    message,
                    finish,
                    usage,
                };
            }
            _ => {}
        }
    };
    eprintln!(
        "{name}: первый токен через {:?}, всего {:?}, usage {:?}",
        first,
        started.elapsed(),
        resp.usage
    );
    resp
}

/// Вызов `echo` — нативный или из текста; `usage.input > 0`.
fn check(name: &str, r: &ModelResponse) {
    let out = normalize(&r.message, &[echo_tool()], &[]);
    let call = out
        .calls
        .iter()
        .find(|c| c.call.name == "echo")
        .unwrap_or_else(|| panic!("{name}: нет вызова echo: {:?} / {out:?}", r.message));
    eprintln!(
        "{name}: вызов {:?} из {:?}",
        call.call.arguments, call.source
    );
    assert!(r.usage.input > 0, "{name}: usage {:?}", r.usage);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "нужен llama-server: LLAMA_TEST_URL"]
async fn live_llama() {
    let Some(url) = var("LLAMA_TEST_URL") else {
        eprintln!("пропущен: нет LLAMA_TEST_URL");
        return;
    };
    let mut ep = json!({"id": "live", "backend": "llama_cpp", "base_url": url});
    if let Some(m) = var("ZA_TEST_LLAMA_MODEL") {
        ep["model"] = json!(m);
    }
    let g = gateway("llama", ep, &[]).await;
    let first = ask(&g, "llama").await;
    check("llama", &first);
    // MDL-011: тот же промпт в том же слоте читается из кэша.
    let second = ask(&g, "llama").await;
    check("llama", &second);
    assert!(
        second.usage.cached > 0,
        "второй запрос без кэша: {:?}",
        second.usage
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "нужен vLLM: VLLM_TEST_URL, ZA_TEST_VLLM_MODEL"]
async fn live_vllm() {
    let (Some(url), Some(model)) = (var("VLLM_TEST_URL"), var("ZA_TEST_VLLM_MODEL")) else {
        eprintln!("пропущен: нет VLLM_TEST_URL или ZA_TEST_VLLM_MODEL");
        return;
    };
    let ep = json!({"id": "live", "backend": "vllm", "base_url": url, "model": model});
    let g = gateway("vllm", ep, &[]).await;
    check("vllm", &ask(&g, "vllm").await);
}

/// Облачный бэкенд с ключом `key_env` и моделью `model_env`.
async fn cloud(name: &str, backend: &str, key_env: &str, model_env: &str) {
    let (Some(key), Some(model)) = (var(key_env), var(model_env)) else {
        eprintln!("пропущен: нет {key_env} или {model_env}");
        return;
    };
    let ep = json!({"id": "live", "backend": backend, "model": model, "key_ref": "live"});
    let g = gateway(name, ep, &[("live", key)]).await;
    check(name, &ask(&g, name).await);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "нужен ключ: OPENROUTER_TEST_KEY, ZA_TEST_OPENROUTER_MODEL"]
async fn live_openrouter() {
    cloud(
        "openrouter",
        "openrouter",
        "OPENROUTER_TEST_KEY",
        "ZA_TEST_OPENROUTER_MODEL",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "нужен ключ: ANTHROPIC_TEST_KEY, ZA_TEST_ANTHROPIC_MODEL"]
async fn live_anthropic() {
    cloud(
        "anthropic",
        "anthropic",
        "ANTHROPIC_TEST_KEY",
        "ZA_TEST_ANTHROPIC_MODEL",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "нужен ключ: OPENAI_TEST_KEY, ZA_TEST_OPENAI_MODEL"]
async fn live_openai() {
    cloud(
        "openai",
        "openai_responses",
        "OPENAI_TEST_KEY",
        "ZA_TEST_OPENAI_MODEL",
    )
    .await;
}
