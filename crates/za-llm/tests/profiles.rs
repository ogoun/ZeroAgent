//! Конфигурация эндпоинтов, зонды возможностей и профили на связку (задача 9).

mod support;

use std::collections::HashMap;

use serde_json::{Value, json};
use support::{FakeServer, Reply, fixture};
use za_llm::config::{
    ConfigError, ConfigParseError, ConfigWarning, EndpointConfig, ModelsConfig, ParseErrorKind,
    Price, compute_cost,
};
use za_llm::http::{HttpTransport, Secret, SecretSource, Timeouts};
use za_llm::profile::{
    CONSERVATIVE_CONTEXT, EndpointState, ProbedEndpoint, ProfileStore, parse_context_limit,
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

fn ready(state: EndpointState) -> ProbedEndpoint {
    match state {
        EndpointState::Ready(p) => p,
        other => panic!("ожидался Ready: {other:?}"),
    }
}

/// Свой каталог хранилища `CARGO_TARGET_TMPDIR/profiles/<name>-<pid>-<n>`: два
/// одновременных прогона бинарника (R1-C1, гигиена тестов) не делят каталог.
fn store_dir(name: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("profiles")
        .join(format!("{name}-{}-{n}", std::process::id()));
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
    assert_eq!(p.facts.context_tokens, 32768);
    assert_eq!(
        p.caps().context_source,
        ContextSource::Runtime {
            path: "/props".into()
        }
    );
    assert_eq!(p.facts.slots, Some(4));
    assert_eq!(p.profile.bundle.quant.as_deref(), Some("Q4_K_M"));
    assert_eq!(
        p.profile.bundle.runtime_version.as_deref(),
        Some("b7100-abc")
    );
    assert_eq!(p.profile.bundle.runtime, BackendKind::LlamaCpp);
    assert_eq!(p.profile.bundle.model, "Qwen3.8-27B-Q4_K_M.gguf");
    assert_eq!(p.facts.endpoint, EndpointId("local".into()));
    // `chat_template_caps.supports_parallel_tool_calls` из `/props`.
    assert!(p.profile.caps.parallel_tools);
    assert_eq!(p.profile.max_output_tokens, None);
    assert_eq!(srv.requests("GET /props").len(), 1);
}

#[tokio::test]
async fn apply_template_decides_reasoning_roundtrip() {
    let srv = llama_server("probe/llama_props.json", vec!["P1", "P2"]).await;
    let p = ready(probe_endpoint(&llama_ep(&srv.base_url), &http(), &MapSecrets::default()).await);
    assert!(p.profile.caps.reasoning_roundtrip);
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
    assert!(!p.profile.caps.reasoning_roundtrip);
}

#[tokio::test]
async fn apply_template_failure_keeps_default_with_note() {
    let srv = FakeServer::start(vec![(
        "GET /props",
        vec![Reply::Json(200, fixture_json("probe/llama_props.json"))],
    )])
    .await;
    let p = ready(probe_endpoint(&llama_ep(&srv.base_url), &http(), &MapSecrets::default()).await);
    assert!(!p.profile.caps.reasoning_roundtrip);
    assert!(
        p.profile.notes.iter().any(|n| n.contains("apply-template")),
        "{:?}",
        p.profile.notes
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
    assert_eq!(p.facts.context_tokens, 65536);
    assert_eq!(p.profile.bundle.model, "Qwen/Qwen3.8-27B-AWQ");
    assert_eq!(p.profile.bundle.runtime_version.as_deref(), Some("0.11.2"));
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
    assert_eq!(p.facts.context_tokens, 1_000_000);
    assert_eq!(p.profile.max_output_tokens, Some(64000));
    assert_eq!(
        p.profile.thinking,
        ThinkingSupport {
            adaptive: true,
            effort: true,
            budget: false,
            effort_xhigh: true,
            effort_max: true,
            sampling: false,
        }
    );
    assert!(p.profile.caps.strict_schema);
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
    assert!(p.profile.caps.strict_schema);
    assert!(p.profile.caps.tool_choice_required);
    assert_eq!(p.facts.context_tokens, 131072);
    assert_eq!(
        p.profile.price,
        Some(Price {
            input_per_mtok: 3.0,
            cached_per_mtok: None,
            cache_write_per_mtok: None,
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
    assert_eq!(p.facts.context_tokens, 16384);
    assert_eq!(p.caps().context_source, ContextSource::OverflowProbe);
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
    assert_eq!(p.facts.context_tokens, 24576);
    assert_eq!(
        p.caps().context_source,
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
    assert_eq!(p.facts.context_tokens, CONSERVATIVE_CONTEXT);
    assert_eq!(p.facts.context_tokens, 8192);
    assert_eq!(p.caps().context_source, ContextSource::Conservative);
    assert!(
        p.facts.notes.iter().any(|n| n.contains("context_tokens")),
        "{:?}",
        p.facts.notes
    );
    // POST-запросов нет: всего один запрос — GET модели.
    assert_eq!(srv.total_requests(), 1);
    // Возможности рассуждения OpenAI — по имени модели (API их не сообщает).
    assert!(
        p.profile.thinking.effort && p.profile.thinking.effort_max && !p.profile.thinking.sampling
    );
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
    assert_eq!(p.facts.context_tokens, 1_000_000);
    assert_eq!(p.caps().context_source, ContextSource::Config);
    // Модель без рассуждений: sampling принимается, effort нет.
    assert!(!p.profile.thinking.effort && p.profile.thinking.sampling);
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
async fn openai_chat_profile_has_sampling_flag() {
    let srv = FakeServer::start(vec![(
        "GET /v1/models/o4-mini",
        vec![Reply::Json(
            200,
            json!({"id": "o4-mini", "object": "model"}),
        )],
    )])
    .await;
    let e = ep(
        json!({"id": "oai", "backend": "openai_chat", "base_url": srv.url("/v1"),
                      "model": "o4-mini", "key_ref": "openai"}),
    );
    let p = ready(probe_endpoint(&e, &http(), &MapSecrets::with("openai", "sk-oai")).await);
    assert!(!p.profile.thinking.sampling && p.profile.thinking.effort);
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
            reason: "ключ anthropic не настроен: задайте переменную ZA_SECRET_ANTHROPIC".into()
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
    assert_ne!(old.profile.bundle.key(), new.profile.bundle.key());
    assert_eq!(old.profile.bundle.key().len(), 16);
    assert_eq!(old.facts.bundle_key, old.profile.bundle.key());
    assert_eq!(new.profile.bundle.quant.as_deref(), Some("IQ3_XXS"));

    let store = ProfileStore::open(&store_dir("bundle_change")).unwrap();
    store.save_probed(&old).unwrap();
    store.save_probed(&new).unwrap();
    assert_eq!(
        store.load(&old.profile.bundle.key()),
        Some(old.profile.clone())
    );
    assert_eq!(store.load(&new.profile.bundle.key()), Some(new.profile));
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
    store.save_probed(&p).unwrap();
    let mut files = 0;
    for sub in ["profiles", "endpoints"] {
        for f in std::fs::read_dir(dir.join(sub)).unwrap() {
            let text = std::fs::read_to_string(f.unwrap().path()).unwrap();
            assert!(!text.contains(KEY), "{text}");
            files += 1;
        }
    }
    assert_eq!(files, 2);

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
fn key_refs_with_same_env_var_are_rejected() {
    // Финальное ревью M4: `a.b`, `a-b`, `A_B` дают одну переменную `ZA_SECRET_A_B` —
    // два эндпоинта молча делили бы один ключ.
    let c = cfg(json!({"endpoints": [
        {"id": "x", "backend": "openrouter", "model": "a/b", "key_ref": "team.a"},
        {"id": "y", "backend": "openrouter", "model": "c/d", "key_ref": "team-a"}
    ]}));
    let err = c.validate().unwrap_err();
    assert_eq!(
        err,
        ConfigError::KeyRefEnvCollision {
            first: EndpointId("x".into()),
            second: EndpointId("y".into())
        }
    );
    let text = err.to_string();
    assert!(!text.contains("team"), "значение key_ref в тексте: {text}");
    // Одинаковый key_ref у двух эндпоинтов — один ключ намеренно, не ошибка.
    let same = cfg(json!({"endpoints": [
        {"id": "x", "backend": "openrouter", "model": "a/b", "key_ref": "team-a"},
        {"id": "y", "backend": "openrouter", "model": "c/d", "key_ref": "team-a"}
    ]}));
    assert_eq!(same.validate(), Ok(vec![]));
}

#[test]
fn budget_without_cost_source_is_config_error() {
    // Финальное ревью I2: стоимость сообщает только OpenRouter; у прочих бэкендов без
    // `price` расход не учитывается и бюджет не наступил бы никогда.
    for backend in ["anthropic", "openai_responses", "openai_chat"] {
        let c = cfg(json!({"endpoints": [
            {"id": "c", "backend": backend, "model": "m", "key_ref": "k", "daily_budget_usd": 5.0}
        ]}));
        let err = c.validate().unwrap_err();
        assert_eq!(err, ConfigError::BudgetWithoutPrice(EndpointId("c".into())));
        let text = err.to_string();
        assert!(
            text.contains("price") && text.contains("daily_budget_usd"),
            "{text}"
        );
        // Текст целиком: без разрывов и двойных пробелов на переносах литерала.
        assert_eq!(
            text,
            concat!(
                "models.endpoints[c]: у группы задан daily_budget_usd, но стоимость запросов ",
                "неизвестна — этот бэкенд её не сообщает; задайте price (USD за миллион ",
                "токенов: input_per_mtok, output_per_mtok) или уберите бюджет"
            )
        );
        assert!(!text.contains("  "), "двойные пробелы: {text}");
    }
    let c = cfg(json!({"endpoints": [
        {"id": "v", "backend": "openai_compatible", "base_url": "https://api.example.com/v1",
         "daily_budget_usd": 5.0}
    ]}));
    assert_eq!(
        c.validate(),
        Err(ConfigError::BudgetWithoutPrice(EndpointId("v".into())))
    );
    // Эндпоинт группы без собственного бюджета тоже должен иметь источник стоимости:
    // его расход входит в бюджет группы.
    let c = cfg(json!({"endpoints": [
        {"id": "a1", "backend": "anthropic", "model": "m", "key_ref": "k", "daily_budget_usd": 5.0,
         "price": {"input_per_mtok": 3.0, "output_per_mtok": 15.0}},
        {"id": "a2", "backend": "anthropic", "model": "m2", "key_ref": "k"}
    ]}));
    assert_eq!(
        c.validate(),
        Err(ConfigError::BudgetWithoutPrice(EndpointId("a2".into())))
    );
    // С `price` и у OpenRouter (стоимость в `usage.cost`) — без ошибки; без бюджета
    // `price` не нужен.
    let ok = cfg(json!({"endpoints": [
        {"id": "a", "backend": "anthropic", "model": "m", "key_ref": "k", "daily_budget_usd": 5.0,
         "price": {"input_per_mtok": 3.0, "output_per_mtok": 15.0}},
        {"id": "or", "backend": "openrouter", "model": "a/b", "key_ref": "or", "daily_budget_usd": 2.0},
        {"id": "o", "backend": "openai_responses", "model": "m", "key_ref": "o"}
    ]}));
    assert_eq!(ok.validate(), Ok(vec![]));
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
        cache_write_per_mtok: None,
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

// ---------------------------------------------------------------------------
// Раунд исправлений 1.

#[tokio::test]
async fn cloud_backend_cannot_be_local_pool() {
    let c = cfg(json!({
        "default": "claude",
        "endpoints": [{"id": "claude", "backend": "anthropic", "model": "claude-opus-5-5",
                       "pool": "local"}]
    }));
    let err = c.validate().unwrap_err();
    assert_eq!(err, ConfigError::CloudPool(EndpointId("claude".into())));
    assert!(err.to_string().contains("pool"));
    // Даже без `validate` эндпоинт облачный: без ключа зонда нет.
    assert_eq!(c.endpoints[0].pool(), PoolKind::Cloud);
    assert_eq!(
        c.endpoints[0].group_key(),
        "cloud:anthropic:api.anthropic.com"
    );
    let srv = FakeServer::start(vec![(
        "GET /v1/models/claude-opus-5-5",
        vec![Reply::Json(200, fixture_json("probe/anthropic_model.json"))],
    )])
    .await;
    let e = ep(
        json!({"id": "claude", "backend": "anthropic", "base_url": srv.url("/v1"),
                      "model": "claude-opus-5-5", "pool": "local"}),
    );
    let st = probe_endpoint(&e, &http(), &MapSecrets::default()).await;
    assert!(matches!(st, EndpointState::Inactive { .. }), "{st:?}");
    assert_eq!(srv.total_requests(), 0);
    // Для локальных видов переопределение действует.
    let e = ep(json!({"id": "g", "backend": "openai_compatible",
                      "base_url": "https://gpu.lan/v1", "pool": "local"}));
    assert_eq!(e.pool(), PoolKind::Local);
}

/// Разбор TOML так, как это делает рабочий код: только `message()` и `span()`.
fn toml_models(src: &str) -> Result<ModelsConfig, ConfigParseError> {
    toml::from_str::<ModelsConfig>(src)
        .map_err(|e| ConfigParseError::from_message(e.message(), e.span().map(|r| r.start), src))
}

#[test]
fn toml_parse_errors_never_show_source() {
    const KEY: &str = "sk-literal-1";
    // Неизвестное поле `api_key`.
    let src = format!(
        "[[endpoints]]\nid = \"c\"\nbackend = \"anthropic\"\nmodel = \"m\"\napi_key = \"{KEY}\"\n"
    );
    // `Display` самой ошибки toml печатает строку источника — поэтому он не используется.
    let raw = toml::from_str::<ModelsConfig>(&src).unwrap_err();
    assert!(raw.to_string().contains(KEY));
    let e = toml_models(&src).unwrap_err();
    let shown = format!("{e} {e:?}");
    assert!(!shown.contains(KEY), "{shown}");
    assert!(shown.contains("api_key"), "{shown}");
    assert_eq!(e.line, Some(5));

    // Синтаксическая ошибка рядом с ключом: незакрытая строка и мусор.
    for src in [
        format!("[[endpoints]]\nid = \"c\"\napi_key = \"{KEY}\nbackend = \"anthropic\"\n"),
        format!("[[endpoints]]\napi_key = \"{KEY}\" ]]] = \n"),
        format!("[[endpoints]]\nid = \"c\"\nbackend = \"anthropic\"\ncontext_tokens = \"{KEY}\"\n"),
        format!("[[endpoints]]\nid = \"c\"\nbackend = \"{KEY}\"\n"),
    ] {
        let e = toml_models(&src).unwrap_err();
        let shown = format!("{e} {e:?}");
        assert!(!shown.contains(KEY), "{shown}");
        assert!(e.line.is_some(), "{shown}");
    }
}

#[test]
fn json_parse_errors_never_show_values() {
    const KEY: &str = "sk-literal-1";
    for src in [
        format!(r#"{{"endpoints": [{{"id": "c", "backend": "anthropic", "api_key": "{KEY}"}}]}}"#),
        format!(
            r#"{{"endpoints": [{{"id": "c", "backend": "anthropic", "context_tokens": "{KEY}"}}]}}"#
        ),
        format!(r#"{{"endpoints": [{{"id": "c", "backend": "{KEY}"}}]}}"#),
        format!(r#"{{"endpoints": [{{"id": "c", "key_ref": "{KEY}"#),
    ] {
        let e = ModelsConfig::from_json_str(&src).unwrap_err();
        let shown = format!("{e} {e:?}");
        assert!(!shown.contains(KEY), "{shown}");
        assert!(e.line.is_some(), "{shown}");
    }
}

async fn llama_with_ctx(n_ctx: u32, slots: u32) -> FakeServer {
    let mut props = fixture_json("probe/llama_props.json");
    props["default_generation_settings"]["n_ctx"] = json!(n_ctx);
    props["total_slots"] = json!(slots);
    FakeServer::start(vec![
        ("GET /props", vec![Reply::Json(200, props)]),
        (
            "POST /apply-template",
            vec![Reply::Json(200, json!({"prompt": "same"}))],
        ),
    ])
    .await
}

#[tokio::test]
async fn endpoints_of_one_bundle_keep_own_facts() {
    let s1 = llama_with_ctx(32768, 4).await;
    let s2 = llama_with_ctx(8192, 1).await;
    let e1 = ep(json!({"id": "big", "backend": "llama_cpp", "base_url": s1.base_url}));
    let e2 = ep(
        json!({"id": "small", "backend": "llama_cpp", "base_url": s2.base_url,
                       "context_tokens": 4096}),
    );
    let a = ready(probe_endpoint(&e1, &http(), &MapSecrets::default()).await);
    let b = ready(probe_endpoint(&e2, &http(), &MapSecrets::default()).await);
    assert_eq!(a.profile.bundle.key(), b.profile.bundle.key());

    let store = ProfileStore::open(&store_dir("one_bundle_two_endpoints")).unwrap();
    store.save_probed(&a).unwrap();
    store.save_probed(&b).unwrap();
    let fa = store.load_endpoint(&EndpointId("big".into())).unwrap();
    let fb = store.load_endpoint(&EndpointId("small".into())).unwrap();
    assert_eq!(
        (fa.context_tokens, fa.runtime_context, fa.slots),
        (32768, Some(32768), Some(4))
    );
    assert_eq!(
        fa.context_source,
        ContextSource::Runtime {
            path: "/props".into()
        }
    );
    assert_eq!(
        (fb.context_tokens, fb.runtime_context, fb.slots),
        (4096, Some(8192), Some(1))
    );
    assert_eq!(fb.context_source, ContextSource::Config);
    assert_eq!(fa.bundle_key, fb.bundle_key);
    assert_eq!(store.load(&fa.bundle_key), Some(b.profile.clone()));
    assert_eq!(b.caps().context_source, ContextSource::Config);
}

#[tokio::test]
async fn parallel_saves_do_not_corrupt() {
    let srv = llama_server("probe/llama_props.json", vec!["a", "b"]).await;
    let p = ready(probe_endpoint(&llama_ep(&srv.base_url), &http(), &MapSecrets::default()).await);
    let dir = store_dir("parallel");
    let store = ProfileStore::open(&dir).unwrap();
    std::thread::scope(|sc| {
        for _ in 0..6 {
            sc.spawn(|| {
                for _ in 0..10 {
                    store.save_probed(&p).unwrap();
                }
            });
        }
    });
    assert_eq!(store.load(&p.facts.bundle_key), Some(p.profile.clone()));
    assert_eq!(
        store.load_endpoint(&p.facts.endpoint),
        Some(p.facts.clone())
    );
    // Временных файлов не осталось.
    for sub in ["profiles", "endpoints"] {
        let names: Vec<String> = std::fs::read_dir(dir.join(sub))
            .unwrap()
            .map(|f| f.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }
}

#[tokio::test]
async fn corrupt_or_foreign_files_are_ignored() {
    let s1 = llama_server("probe/llama_props.json", vec!["a", "b"]).await;
    let s2 = llama_server("probe/llama_props_other_model.json", vec!["a", "b"]).await;
    let a = ready(probe_endpoint(&llama_ep(&s1.base_url), &http(), &MapSecrets::default()).await);
    let mut e2 = llama_ep(&s2.base_url);
    e2.id = EndpointId("other".into());
    let b = ready(probe_endpoint(&e2, &http(), &MapSecrets::default()).await);
    let dir = store_dir("corrupt");
    let store = ProfileStore::open(&dir).unwrap();
    store.save_probed(&a).unwrap();
    store.save_probed(&b).unwrap();
    let ka = a.facts.bundle_key.clone();
    let kb = b.facts.bundle_key.clone();
    let prof = |k: &str| dir.join("profiles").join(format!("{k}.json"));
    let epf = |k: &str| dir.join("endpoints").join(format!("{k}.json"));

    // Чужой профиль под именем другой связки.
    std::fs::copy(prof(&kb), prof(&ka)).unwrap();
    assert_eq!(store.load(&ka), None);
    // Повреждённый профиль.
    std::fs::write(prof(&kb), b"{\"bundle\": ").unwrap();
    assert_eq!(store.load(&kb), None);
    // Чужие и повреждённые факты эндпоинта.
    std::fs::copy(epf("other"), epf("local")).unwrap();
    assert_eq!(store.load_endpoint(&EndpointId("local".into())), None);
    std::fs::write(epf("other"), b"not json").unwrap();
    assert_eq!(store.load_endpoint(&EndpointId("other".into())), None);
    // Перезапись чинит.
    store.save_probed(&a).unwrap();
    assert_eq!(store.load(&ka), Some(a.profile.clone()));
    // Id с недопустимыми для имени файла символами не выходит за каталог.
    let mut odd = a.facts.clone();
    odd.endpoint = EndpointId("../evil/x".into());
    store.save_endpoint(&odd).unwrap();
    assert_eq!(store.load_endpoint(&odd.endpoint), Some(odd));
    assert!(!dir.join("evil").exists());
}

#[test]
fn local_address_ranges() {
    let pool = |u: &str| ep(json!({"id": "v", "backend": "vllm", "base_url": u}));
    for u in [
        "http://[::ffff:127.0.0.1]:8000/v1",
        "http://[::1]:8000/v1",
        "http://localhost:8000/v1",
    ] {
        assert!(pool(u).on_host(), "{u}");
        assert_eq!(pool(u).pool(), PoolKind::Local, "{u}");
    }
    for u in [
        "http://100.64.0.1:8000/v1",
        "http://100.127.255.254:8000/v1",
        "http://169.254.1.2:8000/v1",
        "http://[fe80::1]:8000/v1",
        "http://[::ffff:192.168.1.5]:8000/v1",
    ] {
        assert!(!pool(u).on_host(), "{u}");
        assert_eq!(pool(u).pool(), PoolKind::Local, "{u}");
    }
    for u in ["http://100.128.0.1:8000/v1", "http://8.8.8.8:8000/v1"] {
        assert_eq!(pool(u).pool(), PoolKind::Cloud, "{u}");
    }
}

#[tokio::test]
async fn model_name_is_escaped_in_path() {
    let srv = FakeServer::start(vec![(
        "GET /v1/models/org%2Fmodel%20x",
        vec![Reply::Json(200, json!({"id": "org/model x"}))],
    )])
    .await;
    let e = ep(
        json!({"id": "oai", "backend": "openai_chat", "base_url": srv.url("/v1"),
                      "model": "org/model x", "key_ref": "openai"}),
    );
    let p = ready(probe_endpoint(&e, &http(), &MapSecrets::with("openai", "sk-oai")).await);
    assert_eq!(p.profile.bundle.model, "org/model x");
}

#[tokio::test]
async fn openrouter_variant_suffix() {
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
                      "model": "qwen/qwen3.8-235b:nitro", "key_ref": "openrouter"}),
    );
    let p = ready(probe_endpoint(&e, &http(), &MapSecrets::with("openrouter", "k")).await);
    assert_eq!(p.facts.context_tokens, 131072);
    assert_eq!(p.profile.bundle.model, "qwen/qwen3.8-235b:nitro");
    let e = ep(
        json!({"id": "or", "backend": "openrouter", "base_url": srv.url("/api/v1"),
                      "model": "nope/x:nitro", "key_ref": "openrouter"}),
    );
    let st = probe_endpoint(&e, &http(), &MapSecrets::with("openrouter", "k")).await;
    assert!(matches!(st, EndpointState::Inactive { .. }), "{st:?}");
}

#[test]
fn cache_write_is_billed_by_its_price() {
    // Anthropic: вход 3.0, чтение 0.3, запись 3.75 (×1,25).
    let price = Price {
        input_per_mtok: 3.0,
        cached_per_mtok: Some(0.3),
        cache_write_per_mtok: Some(3.75),
        output_per_mtok: 15.0,
    };
    let u = Usage {
        input: 3_000_000,
        cached: 1_000_000,
        cache_write: 1_000_000,
        ..Usage::default()
    };
    assert!((compute_cost(&price, &u) - (3.0 + 0.3 + 3.75)).abs() < 1e-9);
    // Без цены записи — по цене входа.
    let price = Price {
        cache_write_per_mtok: None,
        ..price
    };
    assert!((compute_cost(&price, &u) - (3.0 + 0.3 + 3.0)).abs() < 1e-9);
}

#[test]
fn validate_ranges_and_credentials() {
    let one = |extra: Value| {
        let mut e = json!({"id": "l", "backend": "llama_cpp"});
        e.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        cfg(json!({"endpoints": [e]})).validate()
    };
    for (extra, field) in [
        (json!({"max_parallel": 0}), "max_parallel"),
        (json!({"context_tokens": 0}), "context_tokens"),
        (json!({"daily_budget_usd": -1.0}), "daily_budget_usd"),
        (
            json!({"price": {"input_per_mtok": -1.0, "output_per_mtok": 1.0}}),
            "price",
        ),
    ] {
        assert_eq!(
            one(extra),
            Err(ConfigError::BadValue(format!(
                "models.endpoints[l].{field}"
            )))
        );
    }
    for u in [
        "http://user:sk-x@127.0.0.1:8080",
        "http://127.0.0.1:8080/?key=sk-x",
    ] {
        let err = one(json!({"base_url": u})).unwrap_err();
        assert_eq!(err, ConfigError::BadBaseUrl(EndpointId("l".into())));
        assert!(!err.to_string().contains("sk-x"));
    }
    let c = cfg(json!({"roles": {"worker": "nope"},
                       "endpoints": [{"id": "l", "backend": "llama_cpp"}]}));
    let err = c.validate().unwrap_err();
    assert_eq!(
        err,
        ConfigError::UnknownRoleTarget {
            role: "worker".into(),
            id: EndpointId("nope".into())
        }
    );
    assert!(err.to_string().contains("models.roles.worker"));
}

#[tokio::test]
async fn literal_key_ref_without_validate_is_not_echoed() {
    let e = ep(json!({"id": "claude", "backend": "anthropic", "model": "m",
                      "key_ref": "sk-ant-api03-literal"}));
    let st = probe_endpoint(&e, &http(), &MapSecrets::default()).await;
    let shown = format!("{st:?}");
    assert!(matches!(st, EndpointState::Inactive { .. }), "{shown}");
    assert!(!shown.contains("sk-ant-api03-literal"), "{shown}");
}

// ---------------------------------------------------------------------------
// Раунд исправлений 2.

#[test]
fn parse_error_is_whitelist_only() {
    const SECRET: &str = "SECRET";
    let base = "[[endpoints]]\nid = \"c\"\nbackend = \"anthropic\"\n";
    // Три случая утечки санитайзера из ревью: кавычка внутри значения, обратная
    // кавычка внутри варианта, ключ как имя поля.
    let toml_cases = [
        format!("{base}context_tokens = 'sk-ab\"{SECRET}'\n"),
        format!("[[endpoints]]\nid = \"c\"\nbackend = 'sk-a`{SECRET}'\n"),
        format!("{base}sk-{SECRET} = 1\n"),
        format!("[sk-{SECRET}]\nx = 1\n"),
        format!("{base}price.sk-{SECRET} = 1\n"),
        format!("{base}pool = 'sk-a`{SECRET}`, expected'\n"),
    ];
    for src in &toml_cases {
        let e = toml_models(src).unwrap_err();
        let shown = format!("{e} {e:?}");
        assert!(!shown.contains(SECRET), "{src} → {shown}");
        assert!(e.line.is_some(), "{shown}");
    }
    let json_cases = [
        format!(
            r#"{{"endpoints": [{{"id": "c", "backend": "anthropic", "context_tokens": "sk-a\"{SECRET}"}}]}}"#
        ),
        format!(r#"{{"endpoints": [{{"id": "c", "backend": "sk-a`{SECRET}"}}]}}"#),
        format!(r#"{{"endpoints": [{{"id": "c", "backend": "anthropic", "sk-{SECRET}": 1}}]}}"#),
    ];
    for src in &json_cases {
        let e = ModelsConfig::from_json_str(src).unwrap_err();
        let shown = format!("{e} {e:?}");
        assert!(!shown.contains(SECRET), "{src} → {shown}");
    }
    // Категории и известные имена полей.
    let e = toml_models(&format!("{base}api_key = 'sk-x'\n")).unwrap_err();
    assert_eq!(
        (e.kind, e.field),
        (ParseErrorKind::UnknownField, Some("api_key"))
    );
    assert!(e.to_string().contains("key_ref"), "{e}");
    let e = toml_models(&format!("{base}sk-{SECRET} = 1\n")).unwrap_err();
    assert_eq!((e.kind, e.field), (ParseErrorKind::UnknownField, None));
    let e = toml_models(&format!("{base}context_tokens = 'x'\n")).unwrap_err();
    assert_eq!(e.kind, ParseErrorKind::InvalidType);
    let e = toml_models("[[endpoints]]\nid = \"c\"\nbackend = 'zzz'\n").unwrap_err();
    assert_eq!(e.kind, ParseErrorKind::UnknownVariant);
    let e = toml_models("[[endpoints]]\nid = \"c\"\n").unwrap_err();
    assert_eq!(
        (e.kind, e.field),
        (ParseErrorKind::MissingField, Some("backend"))
    );
    let e = toml_models("[[endpoints]]\nid = \"c\n").unwrap_err();
    assert_eq!(e.kind, ParseErrorKind::Syntax);
    let e = ModelsConfig::from_json_str(r#"{"endpoints": [{"id": "c""#).unwrap_err();
    assert_eq!(e.kind, ParseErrorKind::Syntax);
}

#[test]
fn validate_errors_show_only_id_format() {
    const SECRET: &str = "sk-ant-SECRET";
    let check = |c: ModelsConfig| {
        let err = c.validate().unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        err
    };
    let err = check(cfg(json!({"endpoints": [
        {"id": "ok", "backend": "llama_cpp"}, {"id": SECRET, "backend": "llama_cpp"}]})));
    assert_eq!(err, ConfigError::BadId(2));
    assert!(err.to_string().contains("№2"));
    check(cfg(
        json!({"default": SECRET, "endpoints": [{"id": "l", "backend": "llama_cpp"}]}),
    ));
    check(cfg(
        json!({"roles": {SECRET: "l"}, "endpoints": [{"id": "l", "backend": "llama_cpp"}]}),
    ));
    check(cfg(json!({"roles": {"worker": SECRET},
                     "endpoints": [{"id": "l", "backend": "llama_cpp"}]})));
    let err = check(cfg(
        json!({"endpoints": [{"id": "a b", "backend": "llama_cpp"}]}),
    ));
    assert_eq!(err, ConfigError::BadId(1));
}

#[tokio::test]
async fn async_save_runs_off_runtime_thread() {
    let srv = llama_server("probe/llama_props.json", vec!["a", "b"]).await;
    let p = ready(probe_endpoint(&llama_ep(&srv.base_url), &http(), &MapSecrets::default()).await);
    let store = ProfileStore::open(&store_dir("async_save")).unwrap();
    store.save_probed_async(p.clone()).await.unwrap();
    assert_eq!(store.load(&p.facts.bundle_key), Some(p.profile.clone()));
    assert_eq!(store.load_endpoint(&p.facts.endpoint), Some(p.facts));
}

#[tokio::test]
async fn endpoint_file_names_are_windows_safe() {
    let srv = llama_server("probe/llama_props.json", vec!["a", "b"]).await;
    let p = ready(probe_endpoint(&llama_ep(&srv.base_url), &http(), &MapSecrets::default()).await);
    let dir = store_dir("windows_names");
    let store = ProfileStore::open(&dir).unwrap();
    let ids = ["A", "a", "CON", "nul", "Com1", "lpt9", "com10", "worker-1"];
    for id in ids {
        let mut f = p.facts.clone();
        f.endpoint = EndpointId(id.into());
        f.context_tokens = id.len() as u32;
        store.save_endpoint(&f).unwrap();
    }
    for id in ids {
        let f = store.load_endpoint(&EndpointId(id.into())).unwrap();
        assert_eq!(f.context_tokens, id.len() as u32, "{id}");
    }
    let mut names: Vec<String> = std::fs::read_dir(dir.join("endpoints"))
        .unwrap()
        .map(|f| f.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names.len(), ids.len(), "{names:?}");
    for n in &names {
        let stem = n.strip_suffix(".json").unwrap();
        assert_eq!(stem, stem.to_ascii_lowercase(), "{n}");
        let reserved = ["con", "prn", "aux", "nul"].contains(&stem)
            || (stem.len() == 4 && (stem.starts_with("com") || stem.starts_with("lpt")));
        assert!(!reserved, "{n}");
    }
    assert!(names.contains(&"a.json".to_owned()));
    assert!(names.contains(&"com10.json".to_owned()));
    assert!(names.contains(&"worker-1.json".to_owned()));
}

#[tokio::test]
async fn openrouter_catalog_variant_is_not_replaced_by_base() {
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
                      "model": "qwen/qwen3.8-235b:free", "key_ref": "openrouter"}),
    );
    let st = probe_endpoint(&e, &http(), &MapSecrets::with("openrouter", "k")).await;
    match st {
        EndpointState::Inactive { reason } => assert!(reason.contains(":free"), "{reason}"),
        other => panic!("ожидался Inactive: {other:?}"),
    }
    for v in ["exacto", "floor"] {
        let e = ep(
            json!({"id": "or", "backend": "openrouter", "base_url": srv.url("/api/v1"),
                          "model": format!("qwen/qwen3.8-235b:{v}"), "key_ref": "openrouter"}),
        );
        let p = ready(probe_endpoint(&e, &http(), &MapSecrets::with("openrouter", "k")).await);
        assert_eq!(p.facts.context_tokens, 131072);
    }
}
