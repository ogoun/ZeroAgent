//! Приёмка R1-B1: команды `run` и `answer` из отдельного процесса.
//!
//! Каждый тест получает свои `ZA_HOME` и каталог проекта внутри `CARGO_TARGET_TMPDIR`.
//! Поддельный OpenAI-совместимый llama-server — axum в процессе теста (многопоточный
//! runtime, пока основной поток ждёт бинарник). Процесс бинарника, не завершившийся за
//! 60 с × `ZA_TEST_TIME_SCALE`, убивается, тест падает с понятной паникой. Порогов
//! времени, которые на деле мерили бы задержку журнала, нет.

use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use http_body::Frame;
use serde_json::{Value, json};
use za_protocol::{EventKind, RunId, Seq};
use za_store::ProjectStore;
use zero_agent::home::project_id;
use zero_agent::journal::open_retrying;

/// Путь запроса модели у llama-server.
const CHAT: &str = "/v1/chat/completions";

// ---- Время ----

/// Множитель времени тестов: `ZA_TEST_TIME_SCALE` (целое 1–6, по умолчанию 1).
fn time_scale() -> u32 {
    static SCALE: OnceLock<u32> = OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("ZA_TEST_TIME_SCALE")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map_or(1, |n| n.clamp(1, 6))
    })
}

/// Предел жизни процесса бинарника.
fn process_limit() -> Duration {
    Duration::from_secs(60) * time_scale()
}

// ---- Поддельный llama-server ----

/// Ответ на запрос модели номер `n` (с 0) и его тело.
type ChatHandler = fn(usize, &Value) -> Response;

/// Поддельный сервер: runtime живёт, пока жив сервер.
struct Fake {
    base: String,
    /// Тела запросов модели в порядке поступления.
    bodies: Arc<Mutex<Vec<Value>>>,
    _rt: tokio::runtime::Runtime,
}

impl Fake {
    fn start(chat: ChatHandler) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let bodies = Arc::new(Mutex::new(Vec::<Value>::new()));
        let shared = bodies.clone();
        let app = Router::new().fallback(move |m: Method, uri: Uri, body: Bytes| {
            let shared = shared.clone();
            async move {
                match (m.as_str(), uri.path()) {
                    ("GET", "/props") => json_reply(
                        StatusCode::OK,
                        // Один слот: `id_slot` тела запроса одинаков до и после сбоя.
                        json!({
                            "default_generation_settings": {"n_ctx": 32768},
                            "total_slots": 1,
                            "model_path": "/models/Qwen3.8-27B-Q4_K_M.gguf",
                            "build_info": "b7100-abc",
                        }),
                    ),
                    ("POST", "/apply-template") => {
                        json_reply(StatusCode::OK, json!({"prompt": "шаблон"}))
                    }
                    ("POST", p) if p.starts_with("/slots/") => {
                        json_reply(StatusCode::OK, json!({}))
                    }
                    ("POST", CHAT) => {
                        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        let n = {
                            let mut b = shared.lock().unwrap();
                            b.push(v.clone());
                            b.len() - 1
                        };
                        chat(n, &v)
                    }
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
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
            bodies,
            _rt: rt,
        }
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
}

fn json_reply(status: StatusCode, v: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        v.to_string(),
    )
        .into_response()
}

fn sse(v: &Value) -> String {
    format!("data: {v}\n\n")
}

fn chunk(delta: Value) -> String {
    sse(
        &json!({"choices": [{"index": 0, "finish_reason": null, "delta": delta}],
                "object": "chat.completion.chunk", "model": "qwen"}),
    )
}

/// Конец ответа: `finish_reason`, usage и `[DONE]`.
fn tail(finish: &str) -> String {
    let mut s = sse(
        &json!({"choices": [{"index": 0, "finish_reason": finish, "delta": {}}],
                "object": "chat.completion.chunk", "model": "qwen"}),
    );
    s.push_str(&sse(
        &json!({"choices": [], "object": "chat.completion.chunk", "model": "qwen",
        "usage": {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15},
        "timings": {"cache_n": 0, "prompt_n": 12, "predicted_n": 3}}),
    ));
    s.push_str("data: [DONE]\n\n");
    s
}

fn sse_reply(body: String) -> Response {
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

/// Ответ текстом из частей, `finish_reason = stop`.
fn text_reply(parts: &[&str]) -> Response {
    let mut body = chunk(json!({"role": "assistant", "content": null}));
    for p in parts {
        body.push_str(&chunk(json!({"content": p})));
    }
    body.push_str(&tail("stop"));
    sse_reply(body)
}

/// Тело SSE: части с паузами перед ними.
struct PacedBody {
    parts: Vec<(Duration, Bytes)>,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl HttpBody for PacedBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.parts.is_empty() {
            return Poll::Ready(None);
        }
        let delay = self.parts[0].0;
        let sleep = self
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(delay)));
        match sleep.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) => {
                self.sleep = None;
                let (_, b) = self.parts.remove(0);
                Poll::Ready(Some(Ok(Frame::data(b))))
            }
        }
    }
}

/// «Пари», через 500 мс «ж» и конец: первая `model.delta` уходит на шину раньше
/// второго чанка (склейка сбрасывается через 200 мс, D8).
fn paced_paris(_: usize, _: &Value) -> Response {
    let first =
        chunk(json!({"role": "assistant", "content": null})) + &chunk(json!({"content": "Пари"}));
    let second = chunk(json!({"content": "ж"})) + &tail("stop");
    let body = PacedBody {
        parts: vec![
            (Duration::ZERO, Bytes::from(first)),
            (Duration::from_millis(500), Bytes::from(second)),
        ],
        sleep: None,
    };
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::new(body),
    )
        .into_response()
}

// ---- Окружение и запуск бинарника ----

struct Env {
    home: PathBuf,
    project: PathBuf,
}

/// Чистые `ZA_HOME` и каталог проекта; `config` — текст `config.toml` (пустой — без файла).
fn env(name: &str, config: &str) -> Env {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("r1b-loop")
        .join(name);
    let _ = std::fs::remove_dir_all(&root);
    let e = Env {
        home: root.join("home"),
        project: root.join("project"),
    };
    std::fs::create_dir_all(&e.home).unwrap();
    std::fs::create_dir_all(&e.project).unwrap();
    if !config.is_empty() {
        std::fs::write(e.home.join("config.toml"), config).unwrap();
    }
    e
}

fn llama_config(base: &str) -> String {
    format!(
        "[models]\ndefault = \"local\"\n\n[[models.endpoints]]\nid = \"local\"\nbackend = \"llama_cpp\"\nbase_url = \"{base}\"\n"
    )
}

/// Итог процесса бинарника.
#[derive(Debug)]
struct Out {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Out {
    fn code(&self) -> Option<i32> {
        self.status.code()
    }
}

fn read_all(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        buf
    })
}

impl Env {
    fn project_arg(&self) -> &str {
        self.project.to_str().unwrap()
    }

    /// Запуск бинарника с пределом [`process_limit`]; `LLAMA_URL`, `ZA_LOG`, `RUST_LOG`
    /// и ключи окружения тестов не наследуются. Кэши cargo и uv (категории монитора
    /// ресурсов) — пустые каталоги теста.
    fn run(&self, args: &[&str]) -> Out {
        self.run_with(args, &[])
    }

    /// [`Env::run`] с дополнительными переменными окружения.
    fn run_with(&self, args: &[&str], vars: &[(&str, &str)]) -> Out {
        let caches = self.home.with_extension("caches");
        let _ = std::fs::create_dir_all(caches.join("cargo"));
        let _ = std::fs::create_dir_all(caches.join("uv"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_zero-agent"));
        cmd.args(args)
            .env("CARGO_HOME", caches.join("cargo"))
            .env("UV_CACHE_DIR", caches.join("uv"))
            .env("ZA_HOME", &self.home)
            .env_remove("LLAMA_URL")
            .env_remove("ZA_LOG")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, _) in std::env::vars() {
            if k.starts_with("ZA_SECRET_") {
                cmd.env_remove(k);
            }
        }
        for (k, v) in vars {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let out = read_all(child.stdout.take().unwrap());
        let err = read_all(child.stderr.take().unwrap());
        let until = Instant::now() + process_limit();
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            if Instant::now() >= until {
                let _ = child.kill();
                let _ = child.wait();
                let err = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
                panic!(
                    "zero-agent {args:?} не завершился за {:?} и убит; stderr: {err}",
                    process_limit()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        Out {
            status,
            stdout: String::from_utf8(out.join().unwrap()).unwrap(),
            stderr: String::from_utf8(err.join().unwrap()).unwrap(),
        }
    }

    /// `run --project <dir>` и аргументы.
    fn run_cmd(&self, extra: &[&str]) -> Out {
        let mut args = vec!["run", "--project", self.project_arg()];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    fn journal_dir(&self) -> PathBuf {
        self.home
            .join("projects")
            .join(project_id(&self.project).unwrap())
    }

    fn store(&self) -> ProjectStore {
        open_retrying(&self.journal_dir()).unwrap()
    }

    /// События run по порядку `seq`.
    fn events(&self, run: RunId) -> Vec<(EventKind, Value)> {
        let store = self.store();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(store.read_run(run, Seq(0), usize::MAX))
            .unwrap()
            .into_iter()
            .map(|e| (e.kind, e.payload))
            .collect()
    }
}

/// id run из первой строки stderr `run <id>`.
fn run_id(o: &Out) -> RunId {
    let first = o.stderr.lines().next().unwrap_or_default();
    let id = first
        .strip_prefix("run ")
        .unwrap_or_else(|| panic!("первая строка stderr не `run <id>`: {o:?}"));
    RunId(uuid::Uuid::parse_str(id.trim()).unwrap())
}

fn count(events: &[(EventKind, Value)], kind: EventKind) -> usize {
    events.iter().filter(|(k, _)| *k == kind).count()
}

/// `needle` — подпоследовательность `hay`.
fn is_subsequence(hay: &[&str], needle: &[&str]) -> bool {
    let mut it = hay.iter();
    needle.iter().all(|n| it.any(|h| h == n))
}

fn journal_exists(e: &Env) -> bool {
    Path::new(&e.journal_dir()).exists()
}

// ---- Тесты ----

fn paris(_: usize, _: &Value) -> Response {
    text_reply(&["Пари", "ж"])
}

#[test]
fn run_answers_and_journals() {
    let srv = Fake::start(paris);
    let e = env("answers", &llama_config(&srv.base));
    let o = e.run_cmd(&["Ответь одним словом: столица Франции?"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert_eq!(o.stdout, "Париж\n", "{o:?}");
    assert!(o.stderr.starts_with("run "), "{o:?}");
    let events = e.events(run_id(&o));
    let kinds: Vec<&str> = events.iter().map(|(k, _)| k.as_str()).collect();
    assert!(
        is_subsequence(
            &kinds,
            &[
                "run.created",
                "run.status",
                "turn.started",
                "model.request",
                "model.delta",
                "model.usage",
                "turn.finished",
                "run.status",
            ]
        ),
        "{kinds:?}"
    );
    let last = events
        .iter()
        .rev()
        .find(|(k, _)| *k == EventKind::RunStatus)
        .unwrap();
    assert_eq!(last.1["to"], "done", "{last:?}");
}

/// Первый запрос модели — 429 с `Retry-After: 3600` (R1-A D19: без повторов), затем «ок».
fn refuse_then_ok(n: usize, _: &Value) -> Response {
    if n == 0 {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::RETRY_AFTER, "3600"),
            ],
            json!({"error": {"message": "slow down", "type": "rate_limit"}}).to_string(),
        )
            .into_response();
    }
    text_reply(&["ок"])
}

#[test]
fn provider_refusal_pauses_then_resume_finishes() {
    let srv = Fake::start(refuse_then_ok);
    let e = env("refusal", &llama_config(&srv.base));
    let o = e.run_cmd(&["Задача"]);
    assert_eq!(o.code(), Some(4), "{o:?}");
    assert!(o.stderr.contains("provider"), "{o:?}");
    let id = run_id(&o).to_string();
    let r = e.run_cmd(&["--resume", &id]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(r.stdout, "ок\n", "{r:?}");
    assert_eq!(srv.bodies().len(), 2);
}

#[test]
fn crash_mid_turn_resumes_with_identical_prefix() {
    let srv = Fake::start(paced_paris);
    let e = env("crash", &llama_config(&srv.base));
    let o = e.run_cmd(&["--crash-at", "after-delta:1", "Столица Франции?"]);
    assert_ne!(o.code(), Some(0), "{o:?}");
    assert!(!o.stderr.contains("ответьте:"), "{o:?}");
    // Сбой посреди хода: дельта в журнале, итога хода ещё нет.
    let before = e.events(run_id(&o));
    assert!(count(&before, EventKind::ModelDelta) >= 1, "{before:?}");
    assert_eq!(count(&before, EventKind::ModelUsage), 0, "{before:?}");
    let id = run_id(&o).to_string();
    let r = e.run_cmd(&["--resume", &id]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(r.stdout, "Париж\n", "{r:?}");
    let bodies = srv.bodies();
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    // R0: прямое сравнение префикса до и после сбоя — тела байт-в-байт.
    assert_eq!(
        serde_json::to_string(&bodies[0]).unwrap(),
        serde_json::to_string(&bodies[1]).unwrap()
    );
}

#[test]
fn resume_unknown_run_fails_without_journal() {
    let e = env("unknown", "");
    let id = uuid::Uuid::now_v7().to_string();
    let o = e.run_cmd(&["--resume", &id]);
    assert_eq!(o.code(), Some(1), "{o:?}");
    assert!(!journal_exists(&e), "{}", e.journal_dir().display());
    // `answer` тоже не создаёт журнал.
    let a = e.run(&[
        "answer",
        "--project",
        e.project_arg(),
        "--interrupt",
        "input-x",
        "--text",
        "1",
    ]);
    assert_eq!(a.code(), Some(1), "{a:?}");
    assert!(!journal_exists(&e));
}

/// Запрос 1 — пустой ответ (пустой ход top, D18 — nudge), запрос 2 — «ок».
fn empty_then_ok(n: usize, _: &Value) -> Response {
    if n == 0 {
        text_reply(&[])
    } else {
        text_reply(&["ок"])
    }
}

#[test]
fn resume_reads_budget_from_flags() {
    let srv = Fake::start(empty_then_ok);
    let e = env("budget", &llama_config(&srv.base));
    let o = e.run_cmd(&["--max-steps", "1", "Задача"]);
    assert_eq!(o.code(), Some(4), "{o:?}");
    assert!(o.stderr.contains("budget"), "{o:?}");
    let run = run_id(&o);
    let exceeded: Vec<Value> = e
        .events(run)
        .into_iter()
        .filter(|(k, _)| *k == EventKind::BudgetExceeded)
        .map(|(_, p)| p)
        .collect();
    assert_eq!(exceeded.len(), 1, "{exceeded:?}");
    assert_eq!(exceeded[0], json!({"kind": "steps", "used": 1, "limit": 1}));
    let id = run.to_string();
    // Resume без флагов сохраняет бюджет run; `[budgets.run]` при resume не перечитывается
    // (решение контроллера, I2 ревью задачи 16): предел из нового раздела не действует.
    let cfg = format!("{}\n[budgets.run]\nsteps = 100\n", llama_config(&srv.base));
    std::fs::write(e.home.join("config.toml"), cfg).unwrap();
    let again = e.run_cmd(&["--resume", &id]);
    assert_eq!(again.code(), Some(4), "{again:?}");
    assert!(again.stderr.contains("budget"), "{again:?}");
    assert_eq!(count(&e.events(run), EventKind::BudgetExceeded), 2);
    assert_eq!(srv.bodies().len(), 1, "модель после бюджета не вызывается");
    // С флагом — флаг поверх текущего бюджета run.
    let r = e.run_cmd(&["--resume", &id, "--max-steps", "5"]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(r.stdout, "ок\n", "{r:?}");
    assert_eq!(count(&e.events(run), EventKind::BudgetExceeded), 2);
}

/// Предел активного времени (`duration_ms`) из CLI продолжается флагом `--max-duration`
/// (финальное ревью R1-B1, I1): подсказка называет флаг вида, исчерпавшего предел.
#[test]
fn resume_extends_duration_budget() {
    let srv = Fake::start(empty_then_ok);
    // Предел 0 мс: первая граница (активного времени ещё нет) пропускает ход, вторая —
    // после хода модели — превышает предел.
    let cfg = format!(
        "{}\n[budgets.run]\nduration_ms = 0\n",
        llama_config(&srv.base)
    );
    let e = env("duration", &cfg);
    let o = e.run_cmd(&["Задача"]);
    assert_eq!(o.code(), Some(4), "{o:?}");
    assert!(o.stderr.contains("--max-duration"), "{o:?}");
    let run = run_id(&o);
    let exceeded: Vec<Value> = e
        .events(run)
        .into_iter()
        .filter(|(k, _)| *k == EventKind::BudgetExceeded)
        .map(|(_, p)| p)
        .collect();
    assert_eq!(exceeded.len(), 1, "{exceeded:?}");
    assert_eq!(exceeded[0]["kind"], "duration", "{exceeded:?}");
    assert_eq!(exceeded[0]["limit"], 0, "{exceeded:?}");
    let id = run.to_string();
    // Без флага предел сохраняется.
    let again = e.run_cmd(&["--resume", &id]);
    assert_eq!(again.code(), Some(4), "{again:?}");
    let r = e.run_cmd(&["--resume", &id, "--max-duration", "600"]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(r.stdout, "ок\n", "{r:?}");
}

/// Первая строка stderr — `run <id>` раньше любых предупреждений (D28): `--agents-md`
/// без файла предупреждает уже после неё.
#[test]
fn run_line_precedes_warnings() {
    let srv = Fake::start(paris);
    let e = env("first_line", &llama_config(&srv.base));
    let o = e.run_cmd(&["--agents-md", "Столица Франции?"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(o.stderr.starts_with("run "), "{o:?}");
    assert!(o.stderr.contains("AGENTS.md"), "{o:?}");
    // Resume к недоступному эндпоинту: логи шлюза — после `run <id>`.
    let id = run_id(&o).to_string();
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    std::fs::write(e.home.join("config.toml"), llama_config(&base)).unwrap();
    // `ZA_LOG=debug`: логи шлюза и зонда идут в stderr до продолжения run.
    let r = e.run_with(
        &["run", "--project", e.project_arg(), "--resume", &id],
        &[("ZA_LOG", "debug")],
    );
    assert!(r.stderr.lines().count() > 2, "логов шлюза нет: {r:?}");
    // Run уже `done`: resume — ошибка, но первая строка — `run <id>`.
    assert_eq!(r.code(), Some(1), "{r:?}");
    assert_eq!(
        r.stderr.lines().next(),
        Some(format!("run {id}").as_str()),
        "{r:?}"
    );
}

#[test]
fn ci_uses_current_checkout() {
    let ci = include_str!("../../../.github/workflows/ci.yml");
    assert!(!ci.contains("actions/checkout@v4"));
}

/// Запрос 1 — вызов `ask_user{"question":"Сколько?"}`, далее — «готово».
fn ask_then_done(n: usize, _: &Value) -> Response {
    if n > 0 {
        return text_reply(&["готово"]);
    }
    let mut body = chunk(json!({"role": "assistant", "content": null}));
    body.push_str(&chunk(
        json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
        "function": {"name": "ask_user", "arguments": "{\"question\":\"Сколько?\"}"}}]}),
    ));
    body.push_str(&tail("tool_calls"));
    sse_reply(body)
}

/// id прерывания из подсказки `answer --interrupt <id>` в stderr.
fn interrupt_id(o: &Out) -> String {
    let at = o
        .stderr
        .find("--interrupt ")
        .unwrap_or_else(|| panic!("нет подсказки answer: {o:?}"));
    o.stderr[at + "--interrupt ".len()..]
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned()
}

#[test]
fn ask_user_round_trip() {
    let srv = Fake::start(ask_then_done);
    let e = env("ask_user", &llama_config(&srv.base));
    let o = e.run_cmd(&["--tier", "standard", "Посчитай"]);
    assert_eq!(o.code(), Some(3), "{o:?}");
    assert!(o.stderr.contains("answer --interrupt input-"), "{o:?}");
    let int = interrupt_id(&o);
    let a = e.run(&[
        "answer",
        "--project",
        e.project_arg(),
        "--interrupt",
        &int,
        "--text",
        "42",
    ]);
    assert_eq!(a.code(), Some(0), "{a:?}");
    let id = run_id(&o).to_string();
    let r = e.run_cmd(&["--resume", &id]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(r.stdout, "готово\n", "{r:?}");
    let bodies = srv.bodies();
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    let tool_results: Vec<&Value> = bodies[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(tool_results.len(), 1, "{:?}", bodies[1]);
    assert_eq!(tool_results[0]["content"], "42", "{:?}", tool_results[0]);
}

#[test]
fn inspect_and_stats_read_journal() {
    // R1-B2 D52, D53: `inspect` и `stats` только читают журнал run.
    let srv = Fake::start(paris);
    let e = env("inspect", &llama_config(&srv.base));
    let o = e.run_cmd(&["Ответь одним словом: столица Франции?"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let run = run_id(&o).to_string();
    let p = e.project_arg();
    let before = e.events(run_id(&o)).len();

    let t = e.run(&["inspect", "--project", p, "--run", &run]);
    assert_eq!(t.code(), Some(0), "{t:?}");
    assert!(t.stdout.contains(", ход 1, модель local,"), "{t:?}");
    assert!(t.stdout.contains("first_message"), "{t:?}");

    let j = e.run(&[
        "inspect",
        "--project",
        p,
        "--run",
        &run,
        "--turn",
        "1",
        "--json",
    ]);
    assert_eq!(j.code(), Some(0), "{j:?}");
    let v: Value = serde_json::from_str(&j.stdout).unwrap();
    assert_eq!(v["turn"], 1);
    assert_eq!(v["input"], 12);
    assert_eq!(v["n_ctx"], 32768);
    assert_eq!(v["blocks"].as_array().unwrap().len(), 8);

    // Тело провайдеру — то же, что ушло серверу, кроме слота (выбирается при отправке).
    let r = e.run(&["inspect", "--project", p, "--run", &run, "--raw"]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    let mut rendered: Value = serde_json::from_str(r.stdout.trim()).unwrap();
    let mut sent = srv.bodies()[0].clone();
    for b in [&mut rendered, &mut sent] {
        b.as_object_mut().unwrap().remove("id_slot");
    }
    assert_eq!(rendered, sent);

    let s = e.run(&["stats", "--project", p, "--run", &run, "--json"]);
    assert_eq!(s.code(), Some(0), "{s:?}");
    let m: Value = serde_json::from_str(&s.stdout).unwrap();
    assert_eq!(
        (m["turns"].clone(), m["steps"].clone()),
        (json!(1), json!(1))
    );
    assert_eq!(
        (m["input"].clone(), m["output"].clone()),
        (json!(12), json!(3))
    );
    let s = e.run(&["stats", "--project", p, "--run", &run]);
    assert_eq!(s.code(), Some(0), "{s:?}");
    assert!(s.stdout.contains("ходов 1, шагов 1"), "{s:?}");

    // Нет хода — код 1; журнал не изменился.
    let n = e.run(&["inspect", "--project", p, "--run", &run, "--turn", "5"]);
    assert_eq!(n.code(), Some(1), "{n:?}");
    assert_eq!(e.events(run_id(&o)).len(), before);
    // Неизвестный run — код 1.
    let other = uuid::Uuid::now_v7().to_string();
    let u = e.run(&["stats", "--project", p, "--run", &other]);
    assert_eq!(u.code(), Some(1), "{u:?}");

    // Проект без журнала: код 1, каталог журнала не создаётся.
    let fresh = env("inspect_fresh", "");
    for cmd in ["inspect", "stats"] {
        let o = fresh.run(&[cmd, "--project", fresh.project_arg(), "--run", &other]);
        assert_eq!(o.code(), Some(1), "{o:?}");
        assert!(o.stderr.contains("не найден"), "{o:?}");
        assert!(!journal_exists(&fresh));
    }
}
