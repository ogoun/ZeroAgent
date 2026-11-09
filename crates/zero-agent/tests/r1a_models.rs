//! Приёмка R1-A: команды `models`, `ask`, `resources`, `doctor` из отдельного процесса.
//!
//! Каждый тест получает свой `ZA_HOME` внутри `CARGO_TARGET_TMPDIR`. Поддельные
//! llama-server и провайдер облака — axum в процессе теста (многопоточный runtime,
//! пока основной поток ждёт бинарник). Облачных ключей и сети наружу нет.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// Ключ, который не должен появиться ни в одном выводе.
const KEY: &str = "sk-test-xyz";

/// Поддельный сервер: runtime живёт, пока жив сервер.
struct Fake {
    base: String,
    hits: Arc<AtomicUsize>,
    _rt: tokio::runtime::Runtime,
}

type Handler = fn(&Method, &str, &HeaderMap) -> Response;

impl Fake {
    fn start(handler: Handler) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = Router::new().fallback(move |m: Method, uri: Uri, h: HeaderMap| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                handler(&m, uri.path(), &h)
            }
        });
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        rt.spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base: format!("http://{addr}"),
            hits,
            _rt: rt,
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

/// Ответ JSON (функция `json` у axum не включена).
fn json_reply(status: StatusCode, v: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        v.to_string(),
    )
        .into_response()
}

fn sse_chunk(delta: Value) -> String {
    let v = json!({"choices": [{"index": 0, "finish_reason": null, "delta": delta}],
                   "object": "chat.completion.chunk", "model": "qwen"});
    format!("data: {v}\n\n")
}

/// llama-server: `/props`, `/apply-template`, `/slots/*`, стрим «Привет, мир».
fn llama(m: &Method, path: &str, _: &HeaderMap) -> Response {
    match (m.as_str(), path) {
        ("GET", "/props") => json_reply(
            StatusCode::OK,
            json!({
                "default_generation_settings": {"n_ctx": 32768},
                "total_slots": 2,
                "model_path": "/models/Qwen3.8-27B-Q4_K_M.gguf",
                "build_info": "b7100-abc",
            }),
        ),
        ("POST", "/apply-template") => json_reply(StatusCode::OK, json!({"prompt": "шаблон"})),
        ("POST", p) if p.starts_with("/slots/") => json_reply(StatusCode::OK, json!({})),
        ("POST", "/v1/chat/completions") => {
            let mut body = sse_chunk(json!({"role": "assistant", "content": null}));
            body.push_str(&sse_chunk(json!({"content": "Привет"})));
            body.push_str(&sse_chunk(json!({"content": ", мир"})));
            let fin = json!({"choices": [{"index": 0, "finish_reason": "stop", "delta": {}}],
                             "object": "chat.completion.chunk", "model": "qwen"});
            body.push_str(&format!("data: {fin}\n\n"));
            let usage = json!({"choices": [], "object": "chat.completion.chunk", "model": "qwen",
                               "usage": {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15},
                               "timings": {"cache_n": 0, "prompt_n": 12, "predicted_n": 3}});
            body.push_str(&format!("data: {usage}\n\ndata: [DONE]\n\n"));
            ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Провайдер, который возвращает ключ эхом в теле 401.
fn echo_key(_: &Method, _: &str, h: &HeaderMap) -> Response {
    let key = h
        .get("x-api-key")
        .or_else(|| h.get(header::AUTHORIZATION))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    json_reply(
        StatusCode::UNAUTHORIZED,
        json!({"type": "error", "error": {"type": "authentication_error",
               "message": format!("invalid x-api-key: {key}")}}),
    )
}

struct Env {
    home: PathBuf,
}

/// Чистый `ZA_HOME` с `config.toml` (пустая строка — без файла).
fn env(name: &str, config: &str) -> Env {
    let home = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("r1a-models")
        .join(name);
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    if !config.is_empty() {
        std::fs::write(home.join("config.toml"), config).unwrap();
    }
    Env { home }
}

impl Env {
    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[])
    }

    /// Запуск бинарника; `LLAMA_URL`, `ZA_LOG`, `RUST_LOG` и ключи окружения тестов не
    /// наследуются.
    fn run_with(&self, args: &[&str], vars: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_zero-agent"));
        cmd.args(args)
            .env("ZA_HOME", &self.home)
            .env_remove("LLAMA_URL")
            .env_remove("ZA_LOG")
            .env_remove("RUST_LOG");
        for (k, _) in std::env::vars() {
            if k.starts_with("ZA_SECRET_") {
                cmd.env_remove(k);
            }
        }
        for (k, v) in vars {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).unwrap()
}

fn json_out(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|e| panic!("{e}: {o:?}"))
}

fn llama_config(base: &str) -> String {
    format!(
        "[models]\ndefault = \"local\"\n\n[[models.endpoints]]\nid = \"local\"\nbackend = \"llama_cpp\"\nbase_url = \"{base}\"\n"
    )
}

fn cloud_config(base: &str) -> String {
    format!(
        "[[models.endpoints]]\nid = \"cloud\"\nbackend = \"anthropic\"\nbase_url = \"{base}/v1\"\nmodel = \"claude-sonnet-5-5\"\nkey_ref = \"anthropic\"\n"
    )
}

#[test]
fn ask_streams_text_from_fake_llama() {
    let srv = Fake::start(llama);
    let e = env("ask", &llama_config(&srv.base));
    let o = e.run(&["ask", "--model", "local", "Поздоровайся"]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(stdout(&o), "Привет, мир", "{o:?}");
    let err = stderr(&o);
    assert!(err.contains("usage"), "{err}");
    assert!(err.contains("\"input\":12"), "{err}");
}

#[test]
fn ask_uses_default_model_and_priority() {
    let srv = Fake::start(llama);
    let e = env("ask_default", &llama_config(&srv.base));
    let o = e.run(&["ask", "--priority", "worker", "Поздоровайся"]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(stdout(&o), "Привет, мир", "{o:?}");
}

#[test]
fn config_without_resources_section_uses_defaults() {
    let srv = Fake::start(llama);
    let e = env("no_resources", &llama_config(&srv.base));
    let o = e.run(&["models", "--json"]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let v = json_out(&o);
    assert_eq!(v["endpoints"][0]["id"], "local", "{v}");
    assert_eq!(v["endpoints"][0]["state"], "ready", "{v}");
    assert_eq!(v["endpoints"][0]["context_tokens"], 32768, "{v}");
}

#[test]
fn models_lists_cloud_without_key_as_inactive() {
    let srv = Fake::start(echo_key);
    let e = env(
        "cloud_no_key",
        &format!("[models]\n{}", cloud_config(&srv.base)),
    );
    let o = e.run(&["models", "--json"]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let v = json_out(&o);
    assert_eq!(v["endpoints"][0]["state"], "inactive", "{v}");
    assert!(
        v["endpoints"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("не настроен"),
        "{v}"
    );
    assert!(
        !stdout(&o).contains("sk-") && !stderr(&o).contains("sk-"),
        "{o:?}"
    );
    // MDL-021: без ключа облако не вызывается.
    assert_eq!(srv.hits(), 0);
    // `ask` к облаку без ключа — отказ без сети.
    let o = e.run(&["ask", "--model", "cloud", "Привет"]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert!(stderr(&o).contains("inactive"), "{o:?}");
    assert_eq!(srv.hits(), 0);
}

#[test]
fn config_with_cloud_default_fails() {
    let srv = Fake::start(echo_key);
    let cfg = format!("[models]\ndefault = \"cloud\"\n{}", cloud_config(&srv.base));
    let e = env("cloud_default", &cfg);
    for args in [
        &["models"][..],
        &["ask", "Привет"],
        &["resources", "--json"],
    ] {
        let o = e.run(args);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {o:?}");
        assert!(stderr(&o).contains("models.default"), "{args:?}: {o:?}");
    }
    assert_eq!(srv.hits(), 0);
}

#[test]
fn bad_resources_value_fails() {
    let e = env("bad_resources", "[resources]\nhysteresis = 5.0\n");
    let o = e.run(&["resources", "--json"]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert!(stderr(&o).contains("resources.hysteresis"), "{o:?}");
}

#[test]
fn resources_json_has_ram_total() {
    let e = env("resources", "");
    let o = e.run(&["resources", "--json"]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let v = json_out(&o);
    assert!(v["resources"]["ram"]["total"].as_u64().unwrap() > 0, "{v}");
    assert!(v["queue"]["groups"].is_array(), "{v}");
    assert!(v["resources"]["volumes"].is_array(), "{v}");
    // Человекочитаемый вывод.
    let o = e.run(&["resources"]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(stdout(&o).contains("RAM"), "{o:?}");
}

#[test]
fn doctor_reports_gpu_and_models_without_failing() {
    let srv = Fake::start(echo_key);
    // GPU нет: `nvidia-smi` указан несуществующий.
    let cfg = format!(
        "[resources]\nnvidia_smi = \"za-no-such-nvidia-smi\"\n\n[models]\n{}",
        cloud_config(&srv.base)
    );
    let e = env("doctor", &cfg);
    let o = e.run(&["doctor", "--json"]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let v = json_out(&o);
    assert_eq!(v["gpu"]["status"], "warn", "{v}");
    assert_eq!(v["models"][0][0], "cloud", "{v}");
    assert_eq!(v["models"][0][1]["status"], "warn", "{v}");
    let o = e.run(&["doctor"]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let out = stdout(&o);
    assert!(out.contains("gpu") && out.contains("cloud"), "{out}");
}

#[test]
fn doctor_fails_on_broken_config() {
    let e = env("doctor_broken", "[models\n");
    let o = e.run(&["doctor", "--json"]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    let v = json_out(&o);
    assert_eq!(v["models"][0][0], "config", "{v}");
    assert_eq!(v["models"][0][1]["status"], "fail", "{v}");
}

#[test]
fn models_probe_prints_profile() {
    let srv = Fake::start(llama);
    let e = env("probe", &llama_config(&srv.base));
    let o = e.run(&["models", "probe", "local", "--json"]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let v = json_out(&o);
    assert_eq!(v["state"], "ready", "{v}");
    assert_eq!(v["facts"]["context_tokens"], 32768, "{v}");
    assert_eq!(v["facts"]["slots"], 2, "{v}");
    assert_eq!(v["profile"]["bundle"]["quant"], "Q4_K_M", "{v}");
    assert!(
        v["profile"]["caps"]["reasoning_roundtrip"].is_boolean(),
        "{v}"
    );
    // Профиль сохранён (D13).
    let profiles = std::fs::read_dir(e.home.join("models").join("profiles"))
        .unwrap()
        .count();
    assert_eq!(profiles, 1);
    let o = e.run(&["models", "probe", "nope"]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
}

#[test]
fn secrets_never_in_output() {
    let srv = Fake::start(echo_key);
    let local = Fake::start(llama);
    let cfg = format!("{}\n{}", llama_config(&local.base), cloud_config(&srv.base));
    let e = env("secrets", &cfg);
    let vars = [("ZA_SECRET_ANTHROPIC", KEY)];
    let runs: &[&[&str]] = &[
        &["models"],
        &["models", "--json"],
        &["models", "probe", "cloud"],
        &["models", "probe", "cloud", "--json"],
        &["doctor"],
        &["doctor", "--json"],
        &["resources", "--json"],
        &["ask", "--model", "cloud", "Привет"],
    ];
    for args in runs {
        for log in ["warn", "debug"] {
            let o = e.run_with(args, &[vars[0], ("ZA_LOG", log)]);
            let all = format!("{}{}", stdout(&o), stderr(&o));
            assert!(!all.contains(KEY), "{args:?} {log}: {all}");
        }
    }
    // Ключ действительно дошёл до провайдера и вернулся эхом.
    assert!(srv.hits() > 0);
    // Профили на диске тоже без ключа.
    for f in walk(&e.home) {
        let text = std::fs::read(&f).unwrap();
        assert!(
            !String::from_utf8_lossy(&text).contains(KEY),
            "{}",
            f.display()
        );
    }
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn config_syntax_error_does_not_leak_key() {
    let cfg = format!(
        "[models]\n[[models.endpoints]]\nid = \"c\"\nbackend = \"anthropic\"\napi_key = \"{KEY}\n"
    );
    let e = env("syntax", &cfg);
    for args in [
        &["models"][..],
        &["doctor"],
        &["doctor", "--json"],
        &["resources"],
    ] {
        let o = e.run(args);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {o:?}");
        let all = format!("{}{}", stdout(&o), stderr(&o));
        assert!(!all.contains(KEY), "{args:?}: {all}");
        assert!(all.contains("строка"), "{args:?}: {all}");
    }
    // Ключ рядом с неизвестным полем.
    let cfg = format!(
        "[models]\n[[models.endpoints]]\nid = \"c\"\nbackend = \"anthropic\"\napi_key = \"{KEY}\"\n"
    );
    let e = env("unknown_field", &cfg);
    let o = e.run(&["models"]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    let all = format!("{}{}", stdout(&o), stderr(&o));
    assert!(!all.contains(KEY) && all.contains("api_key"), "{all}");
}
