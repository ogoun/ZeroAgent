//! Конфигурация эндпоинтов, зонды возможностей и профили на связку (задача 9).

mod support;

use std::collections::HashMap;

use serde_json::{Value, json};
use support::{FakeServer, Reply, fixture};
use za_llm::config::{
    ConfigError, ConfigWarning, EndpointConfig, ModelsConfig, Price, compute_cost,
};
use za_llm::http::{HttpTransport, Secret, SecretSource, Timeouts};
use za_llm::profile::{
    CONSERVATIVE_CONTEXT, EndpointState, ModelProfile, ProfileStore, parse_context_limit,
    quant_from_filename, resolve_context,
};
use za_llm::{
    BackendKind, ContextSource, EndpointId, PoolKind, ThinkingSupport, Usage, probe_endpoint,
};
use za_protocol::RunRole;

/// Ключи теста по `key_ref`.
#[derive(Default)]
struct MapSecrets(HashMap<String, String>);

impl MapSecrets {
    fn with(key_ref: &str, key: &str) -> Self {
        Self(HashMap::from([(key_ref.to_owned(), key.to_owned())]))
    }
}

impl SecretSource for MapSecrets {
    fn get(&self, key_ref: &str) -> Option<Secret> {
        self.0.get(key_ref).cloned().map(Secret::new)
    }
}

fn ep(v: Value) -> EndpointConfig {
    serde_json::from_value(v).unwrap()
}

fn http() -> HttpTransport {
    HttpTransport::local(Timeouts::probe_local())
}

fn fixture_json(path: &str) -> Value {
    serde_json::from_slice(&fixture(path)).unwrap()
}

fn ready(state: EndpointState) -> ModelProfile {
    match state {
        EndpointState::Ready(p) => p,
        other => panic!("ожидался Ready: {other:?}"),
    }
}

fn store_dir(name: &str) -> std::path::PathBuf {
    let d = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("profiles")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    d
}

async fn llama_server(props: &str, prompts: Vec<&str>) -> FakeServer {
    FakeServer::start(vec![
        ("GET /props", vec![Reply::Json(200, fixture_json(props))]),
        (
            "POST /apply-template",
            prompts
                .into_iter()
                .map(|p| Reply::Json(200, json!({ "prompt": p })))
                .collect(),
        ),
    ])
    .await
}

fn llama_ep(base: &str) -> EndpointConfig {
    ep(json!({"id": "local", "backend": "llama_cpp", "base_url": base}))
}

#[tokio::test]
async fn llama_profile_from_props() {
    let srv = llama_server("probe/llama_props.json", vec!["a", "b"]).await;
    let p = ready(probe_endpoint(&llama_ep(&srv.base_url), &http(), &MapSecrets::default()).await);
    assert_eq!(p.context_tokens, 32768);
    assert_eq!(
        p.caps.context_source,
        ContextSource::Runtime {
            path: "/props".into()
        }
    );
    assert_eq!(p.slots, Some(4));
    assert_eq!(p.bundle.quant.as_deref(), Some("Q4_K_M"));
    assert_eq!(p.bundle.runtime_version.as_deref(), Some("b7100-abc"));
    assert_eq!(p.bundle.runtime, BackendKind::LlamaCpp);
    assert_eq!(p.bundle.model, "Qwen3.8-27B-Q4_K_M.gguf");
    assert_eq!(p.endpoint, EndpointId("local".into()));
    // `chat_template_caps.supports_parallel_tool_calls` из `/props`.
    assert!(p.caps.parallel_tools);
    assert_eq!(p.max_output_tokens, None);
    assert_eq!(srv.requests("GET /props").len(), 1);
}

#[tokio::test]
async fn apply_template_decides_reasoning_roundtrip() {
    let srv = llama_server("probe/llama_props.json", vec!["P1", "P2"]).await;
    let p = ready(probe_endpoint(&llama_ep(&srv.base_url), &http(), &MapSecrets::default()).await);
    assert!(p.caps.reasoning_roundtrip);
    let reqs = srv.requests("POST /apply-template");
    assert_eq!(reqs.len(), 2);
    // Два диалога различаются только рассуждением прошлого ответа ассистента.
    let (mut a, mut b) = (reqs[0].json(), reqs[1].json());
    assert_ne!(a, b);
    for v in [&mut a, &mut b] {
        for m in v["messages"].as_array_mut().unwrap() {
            if let Some(o) = m.as_object_mut() {
                o.remove("reasoning_content");
            }
        }
    }
    assert_eq!(a, b);

    let srv = llama_server("probe/llama_props.json", vec!["same", "same"]).await;
    let p = ready(probe_endpoint(&llama_ep(&srv.base_url), &http(), &MapSecrets::default()).await);
    assert!(!p.caps.reasoning_roundtrip);
}

#[tokio::test]
async fn apply_template_failure_keeps_default_with_note() {
    let srv = FakeServer::start(vec![(
        "GET /props",
        vec![Reply::Json(200, fixture_json("probe/llama_props.json"))],
    )])
    .await;
    let p = ready(probe_endpoint(&llama_ep(&srv.base_url), &http(), &MapSecrets::default()).await);
    assert!(!p.caps.reasoning_roundtrip);
    assert!(
        p.notes.iter().any(|n| n.contains("apply-template")),
        "{:?}",
        p.notes
    );
}

#[tokio::test]
async fn vllm_max_model_len() {
    let srv = FakeServer::start(vec![
        (
            "GET /v1/models",
            vec![Reply::Json(200, fixture_json("probe/vllm_models.json"))],
        ),
        (
            "GET /version",
            vec![Reply::Json(200, json!({"version": "0.11.2"}))],
        ),
    ])
    .await;
    let e = ep(json!({"id": "v", "backend": "vllm", "base_url": srv.url("/v1")}));
    let p = ready(probe_endpoint(&e, &http(), &MapSecrets::default()).await);
    assert_eq!(p.context_tokens, 65536);
    assert_eq!(p.bundle.model, "Qwen/Qwen3.8-27B-AWQ");
    assert_eq!(p.bundle.runtime_version.as_deref(), Some("0.11.2"));
    assert_eq!(srv.requests("GET /version").len(), 1);
    assert_eq!(srv.requests("GET /v1/version").len(), 0);
}

#[tokio::test]
async fn anthropic_models_api_capabilities() {
    let srv = FakeServer::start(vec![(
        "GET /v1/models/claude-opus-5-5",
        vec![Reply::Json(200, fixture_json("probe/anthropic_model.json"))],
    )])
    .await;
    let e = ep(
        json!({"id": "claude", "backend": "anthropic", "base_url": srv.url("/v1"),
                      "model": "claude-opus-5-5", "key_ref": "anthropic"}),
    );
    let p = ready(probe_endpoint(&e, &http(), &MapSecrets::with("anthropic", "sk-ant-k1")).await);
    assert_eq!(p.context_tokens, 1_000_000);
    assert_eq!(p.max_output_tokens, Some(64000));
    assert_eq!(
        p.thinking,
        ThinkingSupport {
            adaptive: true,
            effort: true,
            budget: false,
            effort_xhigh: true,
            effort_max: true,
            sampling: false,
        }
    );
    assert!(p.caps.strict_schema);
    let reqs = srv.requests("GET /v1/models/claude-opus-5-5");
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].header("x-api-key"), Some("sk-ant-k1"));
    assert_eq!(reqs[0].header("anthropic-version"), Some("2026-10-21"));
}

#[tokio::test]
async fn openrouter_supported_parameters() {
    let srv = FakeServer::start(vec![(
        "GET /api/v1/models",
        vec![Reply::Json(
            200,
            fixture_json("probe/openrouter_models.json"),
        )],
    )])
    .await;
    let e = ep(
        json!({"id": "or", "backend": "openrouter", "base_url": srv.url("/api/v1"),
                      "model": "qwen/qwen3.8-235b", "key_ref": "openrouter"}),
    );
    let p = ready(probe_endpoint(&e, &http(), &MapSecrets::with("openrouter", "sk-or-1")).await);
    assert!(p.caps.strict_schema);
    assert!(p.caps.tool_choice_required);
    assert_eq!(p.context_tokens, 131072);
    assert_eq!(
        p.price,
        Some(Price {
            input_per_mtok: 3.0,
            cached_per_mtok: None,
            output_per_mtok: 15.0
        })
    );
    let reqs = srv.requests("GET /api/v1/models");
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].header("authorization"), Some("Bearer sk-or-1"));
}

#[tokio::test]
async fn overflow_probe_reads_limit_from_error() {
    let srv = FakeServer::start(vec![
        (
            "GET /v1/models",
            vec![Reply::Json(200, fixture_json("probe/compatible_models.json"))],
        ),
        (
            "POST /v1/chat/completions",
            vec![Reply::Json(
                400,
                json!({"error": {"message": "This model's maximum context length is 16384 tokens. However, you requested 500001 tokens.", "type": "invalid_request_error"}}),
            )],
        ),
    ])
    .await;
    let e = ep(json!({"id": "c", "backend": "openai_compatible", "base_url": srv.url("/v1")}));
    let p = ready(probe_endpoint(&e, &http(), &MapSecrets::default()).await);
    assert_eq!(p.context_tokens, 16384);
    assert_eq!(p.caps.context_source, ContextSource::OverflowProbe);
    let reqs = srv.requests("POST /v1/chat/completions");
    assert_eq!(reqs.len(), 1);
    let body = reqs[0].json();
    assert_eq!(body["max_tokens"], 1);
    assert_eq!(body["model"], "local-model");
    let content = body["messages"][0]["content"].as_str().unwrap();
    assert_eq!(content.len(), za_llm::profile::OVERFLOW_PROBE_CHARS);
}

#[tokio::test]
async fn compatible_context_from_models() {
    let srv = FakeServer::start(vec![(
        "GET /v1/models",
        vec![Reply::Json(
            200,
            json!({"data": [{"id": "m", "context_window": 24576}]}),
        )],
    )])
    .await;
    let e = ep(json!({"id": "c", "backend": "openai_compatible", "base_url": srv.url("/v1")}));
    let p = ready(probe_endpoint(&e, &http(), &MapSecrets::default()).await);
    assert_eq!(p.context_tokens, 24576);
    assert_eq!(
        p.caps.context_source,
        ContextSource::Runtime {
            path: "/models".into()
        }
    );
    assert_eq!(srv.total_requests(), 1);
}

#[tokio::test]
async fn cloud_never_overflow_probes() {
    let srv = FakeServer::start(vec![(
        "GET /v1/models/gpt-6-astra",
        vec![Reply::Json(
            200,
            json!({"id": "gpt-6-astra", "object": "model", "created": 1, "owned_by": "openai"}),
        )],
    )])
    .await;
    let e = ep(
        json!({"id": "oai", "backend": "openai_responses", "base_url": srv.url("/v1"),
                      "model": "gpt-6-astra", "key_ref": "openai"}),
    );
    let p = ready(probe_endpoint(&e, &http(), &MapSecrets::with("openai", "sk-oai")).await);
    assert_eq!(p.context_tokens, CONSERVATIVE_CONTEXT);
    assert_eq!(p.context_tokens, 8192);
    assert_eq!(p.caps.context_source, ContextSource::Conservative);
    assert!(
        p.notes.iter().any(|n| n.contains("context_tokens")),
        "{:?}",
        p.notes
    );
    // POST-запросов нет: всего один запрос — GET модели.
    assert_eq!(srv.total_requests(), 1);
    // Возможности рассуждения OpenAI — по имени модели (API их не сообщает).
    assert!(p.thinking.effort && p.thinking.effort_max && !p.thinking.sampling);
}

#[tokio::test]
async fn openai_context_from_config() {
    let srv = FakeServer::start(vec![(
        "GET /v1/models/gpt-4.1",
        vec![Reply::Json(
            200,
            json!({"id": "gpt-4.1", "object": "model"}),
        )],
    )])
    .await;
    let e = ep(
        json!({"id": "oai", "backend": "openai_responses", "base_url": srv.url("/v1"),
                      "model": "gpt-4.1", "key_ref": "openai", "context_tokens": 1000000}),
    );
    let p = ready(probe_endpoint(&e, &http(), &MapSecrets::with("openai", "sk-oai")).await);
    assert_eq!(p.context_tokens, 1_000_000);
    assert_eq!(p.caps.context_source, ContextSource::Config);
    // Модель без рассуждений: sampling принимается, effort нет.
    assert!(!p.thinking.effort && p.thinking.sampling);
}

#[tokio::test]
async fn cloud_model_not_found_is_inactive() {
    let srv = FakeServer::start(vec![(
        "GET /v1/models/nope",
        vec![Reply::Json(
            404,
            json!({"error": {"message": "The model `nope` does not exist"}}),
        )],
    )])
    .await;
    let e = ep(
        json!({"id": "oai", "backend": "openai_chat", "base_url": srv.url("/v1"),
                      "model": "nope", "key_ref": "openai"}),
    );
    let st = probe_endpoint(&e, &http(), &MapSecrets::with("openai", "sk-oai")).await;
    assert!(matches!(st, EndpointState::Inactive { .. }), "{st:?}");
}

#[tokio::test]
async fn cloud_without_key_is_inactive_and_silent() {
    let srv = FakeServer::start(vec![(
        "GET /v1/models/claude-opus-5-5",
        vec![Reply::Json(200, fixture_json("probe/anthropic_model.json"))],
    )])
    .await;
    let e = ep(
        json!({"id": "claude", "backend": "anthropic", "base_url": srv.url("/v1"),
                      "model": "claude-opus-5-5", "key_ref": "anthropic"}),
    );
    let st = probe_endpoint(&e, &http(), &MapSecrets::default()).await;
    assert_eq!(
        st,
        EndpointState::Inactive {
            reason: "ключ anthropic не настроен".into()
        }
    );
    assert_eq!(srv.total_requests(), 0);
}

#[tokio::test]
async fn unreachable_server() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let e = llama_ep(&format!("http://127.0.0.1:{port}"));
    let st = probe_endpoint(&e, &http(), &MapSecrets::default()).await;
    assert!(matches!(st, EndpointState::Unreachable { .. }), "{st:?}");
}

#[tokio::test]
async fn bundle_change_gives_new_profile() {
    let srv1 = llama_server("probe/llama_props.json", vec!["a", "b"]).await;
    let old =
        ready(probe_endpoint(&llama_ep(&srv1.base_url), &http(), &MapSecrets::default()).await);
    let srv2 = llama_server("probe/llama_props_other_model.json", vec!["a", "b"]).await;
    let new =
        ready(probe_endpoint(&llama_ep(&srv2.base_url), &http(), &MapSecrets::default()).await);
    assert_ne!(old.bundle.key(), new.bundle.key());
    assert_eq!(old.bundle.key().len(), 16);
    assert_eq!(new.bundle.quant.as_deref(), Some("IQ3_XXS"));

    let store = ProfileStore::open(&store_dir("bundle_change")).unwrap();
    store.save(&old).unwrap();
    store.save(&new).unwrap();
    assert_eq!(store.load(&old.bundle.key()), Some(old.clone()));
    assert_eq!(store.load(&new.bundle.key()), Some(new));
    // Ключ вне формата не выходит за каталог.
    assert_eq!(store.load("../x"), None);
    assert_eq!(store.load("0123456789abcdef"), None);
}

#[tokio::test]
async fn profile_file_never_contains_key() {
    // Провайдер эхом возвращает ключ в ответе модели и в теле ошибки.
    const KEY: &str = "sk-secret-zz9-plural-z-alpha";
    let srv = FakeServer::start(vec![
        (
            "GET /v1/models/gpt-6-astra",
            vec![Reply::Json(
                200,
                json!({"id": "gpt-6-astra", "object": "model", "owned_by": KEY}),
            )],
        ),
        (
            "GET /v1/models/broken",
            vec![Reply::Json(
                500,
                json!({"error": {"message": format!("bad key {KEY}")}}),
            )],
        ),
    ])
    .await;
    let secrets = MapSecrets::with("openai", KEY);
    let e = ep(
        json!({"id": "oai", "backend": "openai_responses", "base_url": srv.url("/v1"),
                      "model": "gpt-6-astra", "key_ref": "openai"}),
    );
    assert!(!format!("{e:?}").contains(KEY));
    let p = ready(probe_endpoint(&e, &http(), &secrets).await);
    assert!(!format!("{p:?}").contains(KEY));

    let dir = store_dir("never_contains_key");
    let store = ProfileStore::open(&dir).unwrap();
    store.save(&p).unwrap();
    for f in std::fs::read_dir(&dir).unwrap() {
        let text = std::fs::read_to_string(f.unwrap().path()).unwrap();
        assert!(!text.contains(KEY), "{text}");
    }

    let e = ep(
        json!({"id": "oai", "backend": "openai_responses", "base_url": srv.url("/v1"),
                      "model": "broken", "key_ref": "openai"}),
    );
    let st = probe_endpoint(&e, &http(), &secrets).await;
    assert!(matches!(st, EndpointState::Unreachable { .. }), "{st:?}");
    assert!(!format!("{st:?}").contains(KEY), "{st:?}");
}

#[test]
fn literal_api_key_in_config_is_rejected() {
    let err = serde_json::from_value::<EndpointConfig>(json!({
        "id": "claude", "backend": "anthropic", "model": "claude-opus-5-5",
        "api_key": "sk-literal-1"
    }))
    .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("api_key"), "{text}");
    assert!(!text.contains("sk-literal-1"), "{text}");
}

#[test]
fn bad_key_ref_is_rejected_without_echo() {
    let cfg: ModelsConfig = serde_json::from_value(json!({
        "endpoints": [{"id": "claude", "backend": "anthropic", "model": "claude-opus-5-5",
                       "key_ref": "sk-ant-api03-literal"}]
    }))
    .unwrap();
    let err = cfg.validate().unwrap_err();
    assert_eq!(err, ConfigError::BadKeyRef("claude".into()));
    assert!(!err.to_string().contains("sk-ant-api03-literal"));
    assert!(err.to_string().contains("key_ref"));
}

fn cfg(v: Value) -> ModelsConfig {
    serde_json::from_value(v).unwrap()
}

#[test]
fn validate_rejects_cloud_default() {
    let c = cfg(json!({
        "default": "claude",
        "endpoints": [{"id": "claude", "backend": "anthropic", "model": "claude-opus-5-5",
                       "key_ref": "anthropic"}]
    }));
    let err = c.validate().unwrap_err();
    assert_eq!(err, ConfigError::CloudDefault(EndpointId("claude".into())));
    assert!(err.to_string().contains("models.default"));
}

#[test]
fn validate_requires_base_url_for_vllm() {
    for backend in ["vllm", "openai_compatible"] {
        let c = cfg(json!({"endpoints": [{"id": "x", "backend": backend}]}));
        assert_eq!(
            c.validate().unwrap_err(),
            ConfigError::MissingBaseUrl(EndpointId("x".into()))
        );
    }
}

#[test]
fn validate_other_errors() {
    let c = cfg(json!({"endpoints": [
        {"id": "a", "backend": "llama_cpp"}, {"id": "a", "backend": "llama_cpp"}]}));
    assert_eq!(
        c.validate().unwrap_err(),
        ConfigError::DuplicateId(EndpointId("a".into()))
    );
    let c = cfg(json!({"default": "nope", "endpoints": [{"id": "a", "backend": "llama_cpp"}]}));
    assert_eq!(
        c.validate().unwrap_err(),
        ConfigError::UnknownRef(EndpointId("nope".into()))
    );
    let c = cfg(json!({"endpoints": [{"id": "or", "backend": "openrouter", "key_ref": "or"}]}));
    assert_eq!(
        c.validate().unwrap_err(),
        ConfigError::MissingModel(EndpointId("or".into()))
    );
    let c = cfg(json!({"endpoints": [{"id": "v", "backend": "vllm", "base_url": "not a url"}]}));
    assert_eq!(
        c.validate().unwrap_err(),
        ConfigError::BadBaseUrl(EndpointId("v".into()))
    );
}

#[test]
fn roles_use_run_role_names() {
    let c = cfg(json!({"default": "local", "roles": {"worker": "local"},
                       "endpoints": [{"id": "local", "backend": "llama_cpp"}]}));
    assert_eq!(c.validate(), Ok(vec![]));
    assert_eq!(c.role(RunRole::Worker), Some(&EndpointId("local".into())));
    assert_eq!(c.role(RunRole::Top), None);

    let c = cfg(json!({"roles": {"helper": "local"},
                       "endpoints": [{"id": "local", "backend": "llama_cpp"}]}));
    let err = c.validate().unwrap_err();
    assert_eq!(err, ConfigError::UnknownRole("helper".into()));
    assert!(err.to_string().contains("models.roles.helper"));

    let c = cfg(json!({"endpoints": [{"id": "local", "backend": "llama_cpp"}]}));
    assert!(c.roles.is_empty());
    assert_eq!(ModelsConfig::default().validate(), Ok(vec![]));
}

#[test]
fn group_budget_is_minimum_with_warning() {
    let c = cfg(json!({"endpoints": [
        {"id": "or1", "backend": "openrouter", "model": "a/b", "key_ref": "or", "daily_budget_usd": 5.0},
        {"id": "or2", "backend": "openrouter", "model": "c/d", "key_ref": "or", "daily_budget_usd": 3.0}
    ]}));
    let group = "cloud:openrouter:openrouter.ai";
    assert_eq!(c.endpoints[0].group_key(), group);
    assert_eq!(c.group_budget(group), Some(3.0));
    assert_eq!(
        c.validate(),
        Ok(vec![ConfigWarning::BudgetMismatch {
            group: group.into(),
            used: 3.0
        }])
    );
    assert_eq!(c.group_budget("cloud:anthropic:api.anthropic.com"), None);
}

#[test]
fn pool_by_host() {
    let e = ep(json!({"id": "v", "backend": "vllm", "base_url": "http://10.0.0.5:8000/v1"}));
    assert_eq!(e.pool(), PoolKind::Local);
    assert!(!e.on_host());
    assert_eq!(e.group_key(), "v");

    let e = ep(json!({"id": "v", "backend": "vllm", "base_url": "http://127.0.0.1:8000/v1"}));
    assert_eq!(e.pool(), PoolKind::Local);
    assert!(e.on_host());

    let e = ep(
        json!({"id": "g", "backend": "openai_compatible", "base_url": "https://api.example.com/v1"}),
    );
    assert_eq!(e.pool(), PoolKind::Cloud);
    assert_eq!(e.group_key(), "cloud:openai_compatible:api.example.com");

    let e = ep(json!({"id": "v6", "backend": "vllm", "base_url": "http://[fd00::5]:8000/v1"}));
    assert_eq!(e.pool(), PoolKind::Local);
    let e = ep(json!({"id": "lh", "backend": "llama_cpp", "base_url": "http://localhost:8080"}));
    assert!(e.on_host());
    // Облачный бэкенд на loopback остаётся облачным (D25); переопределение — из конфигурации.
    let e = ep(json!({"id": "a", "backend": "anthropic", "base_url": "http://127.0.0.1:1/v1"}));
    assert_eq!(e.pool(), PoolKind::Cloud);
    let e = ep(json!({"id": "g", "backend": "openai_compatible",
                      "base_url": "https://gpu.lan/v1", "pool": "local"}));
    assert_eq!(e.pool(), PoolKind::Local);
}

#[test]
fn default_base_urls() {
    let url = |b: &str| ep(json!({"id": "x", "backend": b})).effective_base_url();
    assert_eq!(url("anthropic"), "https://api.anthropic.com/v1");
    assert_eq!(url("openai_responses"), "https://api.openai.com/v1");
    assert_eq!(url("openai_chat"), "https://api.openai.com/v1");
    assert_eq!(url("openrouter"), "https://openrouter.ai/api/v1");
    assert_eq!(url("llama_cpp"), "http://127.0.0.1:8080");
    let e = ep(json!({"id": "x", "backend": "vllm", "base_url": "http://h:1/v1/"}));
    assert_eq!(e.effective_base_url(), "http://h:1/v1");
}

#[test]
fn context_limit_patterns() {
    for (m, n) in [
        ("This model's maximum context length is 16384 tokens", 16384),
        (
            "the request exceeds the available context size (8192 tokens)",
            8192,
        ),
        ("prompt is too long: 250000 tokens > 200000 maximum", 200000),
    ] {
        assert_eq!(parse_context_limit(m), Some(n), "{m}");
    }
    assert_eq!(parse_context_limit("internal error"), None);
}

#[test]
fn context_resolution_order() {
    assert_eq!(
        resolve_context(Some(4096), Some((32768, "/props".into())), Some(1)),
        (4096, ContextSource::Config)
    );
    assert_eq!(
        resolve_context(None, Some((32768, "/props".into())), Some(1)),
        (
            32768,
            ContextSource::Runtime {
                path: "/props".into()
            }
        )
    );
    assert_eq!(
        resolve_context(None, None, Some(16384)),
        (16384, ContextSource::OverflowProbe)
    );
    assert_eq!(
        resolve_context(None, None, None),
        (8192, ContextSource::Conservative)
    );
}

#[test]
fn quant_names() {
    assert_eq!(
        quant_from_filename("Qwen3.8-27B-Q4_K_M.gguf").as_deref(),
        Some("Q4_K_M")
    );
    assert_eq!(
        quant_from_filename("gemma-4-12b-it-IQ3_XXS.gguf").as_deref(),
        Some("IQ3_XXS")
    );
    assert_eq!(quant_from_filename("model.gguf"), None);
    assert_eq!(
        quant_from_filename("C:\\m\\Llama-3.3-70B-Instruct-Q8_0-00001-of-00002.gguf").as_deref(),
        Some("Q8_0")
    );
    assert_eq!(
        quant_from_filename("phi-4-BF16.gguf").as_deref(),
        Some("BF16")
    );
}

#[test]
fn computed_cost() {
    let price = Price {
        input_per_mtok: 3.0,
        cached_per_mtok: None,
        output_per_mtok: 15.0,
    };
    let u = Usage {
        input: 1_000_000,
        output: 500_000,
        ..Usage::default()
    };
    assert!((compute_cost(&price, &u) - 10.5).abs() < 1e-9);
    // `input` включает кэш: кэшированная часть идёт по своей цене.
    let price = Price {
        cached_per_mtok: Some(0.3),
        ..price
    };
    let u = Usage {
        input: 1_000_000,
        cached: 500_000,
        ..Usage::default()
    };
    assert!((compute_cost(&price, &u) - (1.5 + 0.15)).abs() < 1e-9);
}
