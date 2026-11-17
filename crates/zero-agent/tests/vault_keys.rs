//! Ключи моделей из Vault памяти (R1-M задача 5; D143, D144, D160; решение владельца 1).
//!
//! Бинарник — отдельным процессом с поддельными ZeroAIMemory (`test-support/fake_zam.rs`) и
//! OpenAI-совместимым сервером модели на `127.0.0.1`; ожидание Vault — по строке stderr
//! процесса («ждёт Vault памяти»), а не по таймеру. Функция ожидания — на ручных часах
//! (`ManualClock`, D165), отмена — `oneshot`. Ctrl+C из теста на Windows не посылается (задел
//! бы сам тест): во время ожидания процесс снимается `Child::kill`, а код 130 и Ctrl+C —
//! ручная проверка живой приёмки (задача 13) и `wait_cancelled_returns_cancelled`.
//!
//! Токен памяти и значения ключей — склейка из частей во время выполнения (GitHub push
//! protection); в выводе, журнале и файлах `ZA_HOME` их нет.

#[path = "../../../test-support/fake_zam.rs"]
mod fake_zam;
mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::IntoResponse;
use fake_zam::{FakeZam, WAIT, within};
use serde_json::{Value, json};
use support::fake_llama::{Fake, json_reply, text_reply};
use support::process::{Env, env_in, run_id};
use za_memory::{ManualClock, MemoryConfig, RunBinding, Token};
use za_protocol::EventKind;
use za_secrets::MaskCell;
use zero_agent::config::AgentConfig;
use zero_agent::memory::{KeyPlan, WaitError, memory_runtime, wait_for_keys};

/// GUID решения проекта (синтетический).
const SOLUTION: &str = "0192f1c4-7a1e-7c3b-9a55-2b1f6f0d9e77";
/// Ключ модели в `config.toml`.
const KEY_REF: &str = "demo-key";

/// Фиктивный токен памяти: `zam_` и 40 символов base62 — из частей.
fn token() -> String {
    ["zam", "_", "Vk7Lp2Qx9Rz4Ts1Uw6Ya", "3Bc8Dn5Ef0Gh2Jk4Lm6N"].concat()
}

/// Значение ключа в Vault.
fn vault_value() -> String {
    ["vk", "-synthetic-", "demo-", "0123456789abcdef"].concat()
}

/// Другое значение того же ключа в `ZA_SECRET_DEMO_KEY`.
fn env_value() -> String {
    ["env", "-synthetic-", "other-", "0123456789abcdef"].concat()
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// Запрос к поддельной модели: маршрут, `Authorization`, тело.
#[derive(Debug, Clone)]
struct Hit {
    route: String,
    auth: Option<String>,
    body: Value,
}

/// Поддельный OpenAI-совместимый сервер модели на `127.0.0.1`: `GET /v1/models` (модель `m`,
/// окно 32768) и стрим ответа `reply` на `POST /v1/chat/completions`.
struct Model {
    base: String,
    hits: Arc<Mutex<Vec<Hit>>>,
}

impl Model {
    fn start(rt: &tokio::runtime::Runtime, reply: String) -> Self {
        let hits = Arc::new(Mutex::new(Vec::<Hit>::new()));
        let shared = hits.clone();
        let reply = Arc::new(reply);
        let app = Router::new().fallback(move |m: Method, uri: Uri, h: HeaderMap, body: Bytes| {
            let shared = shared.clone();
            let reply = reply.clone();
            async move {
                let auth = h
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                shared.lock().unwrap().push(Hit {
                    route: format!("{m} {}", uri.path()),
                    auth,
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                });
                match (m.as_str(), uri.path()) {
                    ("GET", p) if p.ends_with("/models") => json_reply(
                        StatusCode::OK,
                        json!({"object": "list", "data": [
                            {"id": "m", "object": "model", "context_length": 32768}
                        ]}),
                    ),
                    ("POST", p) if p.ends_with("/chat/completions") => {
                        text_reply(&[reply.as_str()])
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
            hits,
        }
    }

    fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }

    /// Запросы модели (`chat/completions`).
    fn chats(&self) -> Vec<Hit> {
        self.hits()
            .into_iter()
            .filter(|h| h.route.ends_with("/chat/completions"))
            .collect()
    }
}

/// Эндпоинты модели с ключами: `cloud-demo` (`demo-key`) и `other` (`other-key`).
fn models_config(base: &str) -> String {
    format!(
        "[models]\n\n[[models.endpoints]]\nid = \"cloud-demo\"\nbackend = \"openai_compatible\"\nbase_url = \"{base}/v1\"\nmodel = \"m\"\nkey_ref = \"{KEY_REF}\"\n\n[[models.endpoints]]\nid = \"other\"\nbackend = \"openai_compatible\"\nbase_url = \"{base}/v1\"\nmodel = \"m\"\nkey_ref = \"other-key\"\n"
    )
}

fn memory_section(url: &str) -> String {
    format!("\n[memory]\nurl = \"{url}\"\n")
}

/// Сценарий: runtime поддельных серверов, ZAM с требуемым токеном, модель, окружение.
struct Case {
    rt: tokio::runtime::Runtime,
    zam: FakeZam,
    model: Model,
    e: Env,
}

impl Case {
    /// `memory` — с разделом `[memory]` на поддельный ZAM.
    fn new(name: &str, reply: &str, memory: bool) -> Self {
        let rt = rt();
        let zam = rt.block_on(FakeZam::start());
        zam.require_token(&token());
        let model = Model::start(&rt, reply.to_owned());
        let mut cfg = models_config(&model.base);
        if memory {
            cfg.push_str(&memory_section(&zam.url()));
        }
        let e = env_in("zero-agent-vault-keys", name, &cfg);
        Self { rt, zam, model, e }
    }

    /// Проект привязан к решению и доверен (D135).
    fn bind(&self) {
        let dir = self.e.project.join(".zeroaimemory");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("solution.toml"),
            format!("agent_solution_id = \"{SOLUTION}\"\nname = \"synthetic\"\n"),
        )
        .unwrap();
        self.e.trust();
    }

    fn run_args<'a>(&'a self, extra: &[&'a str]) -> Vec<&'a str> {
        let mut a = vec!["run", "--project", self.e.project_arg()];
        a.extend_from_slice(extra);
        a
    }

    /// Пути принятых ZAM запросов с `?reveal=`.
    fn reveal_requests(&mut self) -> Vec<String> {
        self.zam
            .seen()
            .into_iter()
            .map(|s| s.path)
            .filter(|p| p.contains("reveal="))
            .collect()
    }
}

/// Все файлы под `dir`.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files_under(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn assert_no_value(what: &str, text: &str, value: &str) {
    assert!(!text.contains(value), "{what}: значение ключа в выводе");
}

#[test]
fn run_waits_for_locked_vault_then_proceeds() {
    let c = Case::new("wait-locked", "готово", true);
    c.zam
        .add_vault(SOLUTION, KEY_REF, "token", Some(&vault_value()));
    c.zam.lock_vault("synthetic lock");
    c.bind();
    let tok = token();
    let args = c.run_args(&["--model", "cloud-demo", "задача"]);
    let mut r = c.e.spawn_with(&args, &[("ZA_MEMORY_TOKEN", &tok)]);
    let line = c.rt.block_on(r.line_with("(Ctrl+C — отмена)"));
    assert!(
        line.contains("zero-agent: ключ demo-key ждёт Vault памяти: Vault заблокирован"),
        "{line}"
    );
    assert!(
        line.contains("повтор через 2 с (Ctrl+C — отмена)"),
        "{line}"
    );
    // До раскрытия к модели нет ни одного запроса, раскрытий нет.
    assert!(c.model.hits().is_empty(), "{:?}", c.model.hits());
    assert!(c.zam.reveals().is_empty());
    c.zam.unlock_vault();
    let o = c.rt.block_on(r.wait());
    assert_eq!(o.code(), Some(0), "{o:?}");
    let run = run_id(&o);
    // Раскрытие одно, с сессией этого run.
    let reveals = c.zam.reveals();
    assert_eq!(reveals.len(), 1, "{reveals:?}");
    let session = reveals[0]
        .1
        .clone()
        .expect("раскрытие run без X-Zam-Session");
    assert!(
        session.starts_with("za-") && session.ends_with(&run.to_string()),
        "{session}"
    );
    // Строка ожидания — одна: причина не менялась.
    assert_eq!(o.stderr.matches("ждёт Vault памяти").count(), 1, "{o:?}");
    // Модель получила ключ из Vault.
    let chats = c.model.chats();
    assert!(!chats.is_empty());
    let want = format!("Bearer {}", vault_value());
    for h in c.model.hits() {
        assert_eq!(h.auth.as_deref(), Some(want.as_str()), "{}", h.route);
    }
    // Аудит и журнал — без значения.
    let audit = c.e.audit("secret.revealed");
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(
        audit[0].payload,
        json!({"name": KEY_REF, "scope": "solution", "item": reveals[0].0, "session": session})
    );
    let events = c.e.events(run);
    let revealed: Vec<&Value> = events
        .iter()
        .filter(|(k, _)| *k == EventKind::SecretRevealed)
        .map(|(_, p)| p)
        .collect();
    assert_eq!(
        revealed,
        [&json!({"name": KEY_REF, "scope": "solution", "item": reveals[0].0, "cached": false})]
    );
    let v = vault_value();
    assert_no_value("stdout", &o.stdout, &v);
    assert_no_value("stderr", &o.stderr, &v);
    assert_no_value("stderr токен", &o.stderr, &tok[4..]);
    for f in files_under(&c.e.home) {
        let data = std::fs::read(&f).unwrap();
        assert!(
            !contains(&data, v.as_bytes()) && !contains(&data, &tok.as_bytes()[4..]),
            "секрет в {}",
            f.display()
        );
    }
}

#[test]
fn revealed_value_masked_everywhere() {
    let v = vault_value();
    let mut c = Case::new("masked", &format!("ключ: {v} конец"), true);
    c.zam.add_vault(SOLUTION, KEY_REF, "token", Some(&v));
    // Файл инструкций доверенного проекта с раскрытым значением: маска файла — после
    // раскрытия ключей (D160).
    std::fs::write(
        c.e.project.join("CLAUDE.md"),
        format!("# Проект\n\nPASS={v}\n"),
    )
    .unwrap();
    c.bind();
    let tok = token();
    let o = c.e.run_with(
        &c.run_args(&["--model", "cloud-demo", "задача"]),
        &[("ZA_MEMORY_TOKEN", &tok)],
    );
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(
        o.stdout.contains("ключ: [secret masked: demo-key] конец"),
        "{o:?}"
    );
    assert!(
        o.stderr
            .contains("zero-agent: в файле инструкций проекта замаскировано секретов: 1"),
        "{o:?}"
    );
    assert_no_value("stdout", &o.stdout, &v);
    assert_no_value("stderr", &o.stderr, &v);
    let chats = c.model.chats();
    assert!(!chats.is_empty());
    for h in &chats {
        assert!(!h.body.to_string().contains(&v), "значение ушло модели");
    }
    let events = c.e.events(run_id(&o));
    let masked: Vec<&Value> = events
        .iter()
        .filter(|(k, _)| *k == EventKind::InputMasked)
        .map(|(_, p)| p)
        .collect();
    assert_eq!(masked, [&json!({"source": "agents_md", "masked": 1})]);
    for f in files_under(&c.e.home) {
        let data = std::fs::read(&f).unwrap();
        assert!(!contains(&data, v.as_bytes()), "значение в {}", f.display());
    }
    assert_eq!(c.reveal_requests().len(), 1);
}

#[test]
fn local_model_does_not_wait() {
    let local = Fake::start(32768, true, |_| text_reply(&["готово"]));
    let mut c = Case::new("local", "готово", true);
    let cfg = format!(
        "[models]\ndefault = \"local\"\n\n[[models.endpoints]]\nid = \"local\"\nbackend = \"llama_cpp\"\nbase_url = \"{}\"\n\n[[models.endpoints]]\nid = \"cloud-demo\"\nbackend = \"openai_compatible\"\nbase_url = \"{}/v1\"\nmodel = \"m\"\nkey_ref = \"{KEY_REF}\"\n{}",
        local.base,
        c.model.base,
        memory_section(&c.zam.url())
    );
    std::fs::write(c.e.home.join("config.toml"), cfg).unwrap();
    c.zam
        .add_vault("knowledge", KEY_REF, "token", Some(&vault_value()));
    c.zam.lock_vault("synthetic lock");
    let o =
        c.e.run_with(&c.run_args(&["задача"]), &[("ZA_MEMORY_TOKEN", &token())]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(!o.stderr.contains("ждёт Vault"), "{o:?}");
    assert!(c.reveal_requests().is_empty());
    // Облачной модели без ключа — ни одного запроса.
    assert!(c.model.hits().is_empty(), "{:?}", c.model.hits());
}

#[test]
fn env_secret_ignored_when_memory_configured() {
    let mut c = Case::new("env-ignored", "готово", true);
    // Проект не доверен: ключ — из личного Vault.
    c.zam
        .add_vault("knowledge", KEY_REF, "token", Some(&vault_value()));
    let (tok, env) = (token(), env_value());
    let o = c.e.run_with(
        &c.run_args(&["--model", "cloud-demo", "задача"]),
        &[("ZA_MEMORY_TOKEN", &tok), ("ZA_SECRET_DEMO_KEY", &env)],
    );
    assert_eq!(o.code(), Some(0), "{o:?}");
    let want = format!("Bearer {}", vault_value());
    let hits = c.model.hits();
    assert!(!hits.is_empty());
    for h in &hits {
        assert_eq!(h.auth.as_deref(), Some(want.as_str()), "{}", h.route);
    }
    assert_eq!(c.reveal_requests().len(), 1);
    let reveals = c.zam.reveals();
    let events = c.e.events(run_id(&o));
    assert!(
        events.iter().any(|(k, p)| *k == EventKind::SecretRevealed
            && *p == json!({"name": KEY_REF, "scope": "knowledge", "item": reveals[0].0, "cached": false})),
        "{events:?}"
    );
}

#[test]
fn killed_while_waiting_creates_no_run() {
    let c = Case::new("killed", "готово", true);
    c.zam
        .add_vault(SOLUTION, KEY_REF, "token", Some(&vault_value()));
    c.zam.lock_vault("synthetic lock");
    c.bind();
    let tok = token();
    let args = c.run_args(&["--model", "cloud-demo", "задача"]);
    let mut r = c.e.spawn_with(&args, &[("ZA_MEMORY_TOKEN", &tok)]);
    // Строка ожидания (а не «модель не готова» с той же причиной): процесс ждёт.
    c.rt.block_on(r.line_with("(Ctrl+C — отмена)"));
    let o = r.kill();
    let run = run_id(&o);
    // Журнал проекта (он есть: доверие записано) не держится на время ожидания, run не создан.
    let store = c.e.store();
    let got = c.rt.block_on(store.get_run(run)).unwrap();
    assert!(got.is_none(), "run создан до раскрытия ключа: {got:?}");
    assert!(c.model.hits().is_empty());
}

#[test]
fn without_memory_test_build_reads_env() {
    let c = Case::new("no-memory", "готово", false);
    let env = env_value();
    let o = c.e.run_with(
        &c.run_args(&["--model", "cloud-demo", "задача"]),
        &[("ZA_SECRET_DEMO_KEY", &env)],
    );
    assert_eq!(o.code(), Some(0), "{o:?}");
    let want = format!("Bearer {env}");
    let hits = c.model.hits();
    assert!(!hits.is_empty());
    for h in &hits {
        assert_eq!(h.auth.as_deref(), Some(want.as_str()), "{}", h.route);
    }
    assert!(c.zam.reveals().is_empty());
}

#[test]
fn secret_list_shows_vault_without_reveal() {
    let mut c = Case::new("secret-list", "готово", true);
    c.zam
        .add_vault(SOLUTION, KEY_REF, "token", Some(&vault_value()));
    c.zam
        .add_vault("knowledge", "other-key", "password", Some(&env_value()));
    c.zam.add_vault("knowledge", "searxng", "host", None);
    c.bind();
    let o = c.e.run_with(
        &["secret", "list", "--project", c.e.project_arg()],
        &[("ZA_MEMORY_TOKEN", &token())],
    );
    assert_eq!(o.code(), Some(0), "{o:?}");
    let line = |name: &str| {
        o.stdout
            .lines()
            .find(|l| l.split_whitespace().next() == Some(name))
            .unwrap_or_else(|| panic!("нет строки {name}: {}", o.stdout))
            .to_owned()
    };
    assert!(
        o.stdout
            .starts_with("источник секретов: Vault памяти (решение, затем личный)"),
        "{}",
        o.stdout
    );
    assert!(
        line(KEY_REF).contains("в Vault решения  token"),
        "{}",
        o.stdout
    );
    assert!(
        line("other-key").contains("в личном Vault  password"),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("записей окружения в Vault: 1"),
        "{}",
        o.stdout
    );
    assert!(
        c.reveal_requests().is_empty(),
        "secret list раскрывал Vault"
    );
    assert_no_value("stdout", &o.stdout, &vault_value());
    // Заблокированный Vault — пометка в строке, раскрытия нет.
    c.zam.lock_vault("synthetic lock");
    let o = c.e.run_with(
        &["secret", "list", "--project", c.e.project_arg()],
        &[("ZA_MEMORY_TOKEN", &token())],
    );
    assert!(
        o.stdout.contains("(Vault заблокирован: synthetic lock)"),
        "{}",
        o.stdout
    );
    assert!(c.reveal_requests().is_empty());
}

#[test]
fn probe_and_ask_reveal_without_session() {
    let mut c = Case::new("probe-ask", "готово", true);
    c.zam
        .add_vault("knowledge", KEY_REF, "token", Some(&vault_value()));
    let tok = token();
    let o = c.e.run_with(
        &["models", "probe", "cloud-demo", "--json"],
        &[("ZA_MEMORY_TOKEN", &tok)],
    );
    assert_eq!(o.code(), Some(0), "{o:?}");
    let o = c.e.run_with(
        &["ask", "--model", "cloud-demo", "Привет"],
        &[("ZA_MEMORY_TOKEN", &tok)],
    );
    assert_eq!(o.code(), Some(0), "{o:?}");
    let reveals = c.zam.reveals();
    assert_eq!(reveals.len(), 2, "{reveals:?}");
    assert!(reveals.iter().all(|(_, s)| s.is_none()), "{reveals:?}");
    // `models` ключи не раскрывает: облачный эндпоинт — `inactive` с причиной.
    c.zam.seen();
    let o = c.e.run_with(&["models"], &[("ZA_MEMORY_TOKEN", &tok)]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(
        o.stdout.contains("ключ demo-key не раскрыт"),
        "{}",
        o.stdout
    );
    assert!(c.reveal_requests().is_empty());
    // `models probe` при заблокированном Vault не ждёт: `inactive` с причиной, код 1.
    let fresh = Case::new("probe-locked", "готово", true);
    fresh
        .zam
        .add_vault("knowledge", KEY_REF, "token", Some(&vault_value()));
    fresh.zam.lock_vault("synthetic lock");
    let o = fresh.e.run_with(
        &["models", "probe", "cloud-demo"],
        &[("ZA_MEMORY_TOKEN", &tok)],
    );
    assert_eq!(o.code(), Some(1), "{o:?}");
    assert!(
        o.stdout
            .contains("inactive: ключ demo-key ждёт Vault памяти: Vault заблокирован"),
        "{o:?}"
    );
    assert!(!o.stderr.contains("повтор через"), "{o:?}");
}

// ---- Функция ожидания (без процесса, ручные часы D165) ----

fn untrusted_binding() -> RunBinding {
    RunBinding {
        project_id: "0123456789abcdef".to_owned(),
        solution: None,
        area: None,
        writable: false,
    }
}

fn memory_cfg(url: &str) -> AgentConfig {
    AgentConfig {
        memory: Some(MemoryConfig::new(url)),
        ..AgentConfig::default()
    }
}

#[tokio::test]
async fn wait_cancelled_returns_cancelled() {
    let mut zam = FakeZam::start().await;
    zam.require_token(&token());
    zam.add_vault("knowledge", KEY_REF, "token", Some(&vault_value()));
    zam.lock_vault("synthetic lock");
    let cell = Arc::new(MaskCell::default());
    let mem = memory_runtime(
        &memory_cfg(&zam.url()),
        untrusted_binding(),
        Some(Token::new(token())),
        &cell,
    )
    .await;
    let clock = Arc::new(ManualClock::new());
    let plan = KeyPlan {
        names: vec![KEY_REF.to_owned()],
    };
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let waiting = {
        let clock = clock.clone();
        tokio::spawn(async move {
            wait_for_keys(&mem, &plan, Some("za-synthetic"), clock.as_ref(), async {
                let _ = rx.await;
            })
            .await
        })
    };
    // Первая попытка: Vault заблокирован — пауза на часах.
    within(WAIT, "пауза ожидания", clock.wait_sleepers(1)).await;
    tx.send(()).unwrap();
    let r = within(WAIT, "отмена", waiting).await.unwrap();
    assert_eq!(r.unwrap_err(), WaitError::Cancelled);
    let reveals: Vec<String> = zam
        .seen()
        .into_iter()
        .map(|s| s.path)
        .filter(|p| p.contains("reveal="))
        .collect();
    assert!(reveals.is_empty(), "{reveals:?}");
    let v = vault_value();
    assert_eq!(cell.current().mask(&v), v.as_str());
}

/// Паузы ожидания — 2, затем 4 с на часах `Clock`; после разблокировки — одно раскрытие, ключ
/// в маске процесса.
#[tokio::test]
async fn wait_retries_on_clock_until_unlocked() {
    use std::time::Duration;

    let mut zam = FakeZam::start().await;
    zam.require_token(&token());
    zam.add_vault("knowledge", KEY_REF, "token", Some(&vault_value()));
    zam.lock_vault("synthetic lock");
    let cell = Arc::new(MaskCell::default());
    let mem = memory_runtime(
        &memory_cfg(&zam.url()),
        untrusted_binding(),
        Some(Token::new(token())),
        &cell,
    )
    .await;
    let clock = Arc::new(ManualClock::new());
    let plan = KeyPlan {
        names: vec![KEY_REF.to_owned()],
    };
    let waiting = {
        let clock = clock.clone();
        tokio::spawn(async move {
            wait_for_keys(
                &mem,
                &plan,
                None,
                clock.as_ref(),
                std::future::pending::<()>(),
            )
            .await
        })
    };
    let lists = |zam: &mut FakeZam| {
        zam.seen()
            .iter()
            .filter(|s| s.route().ends_with("/vault"))
            .count()
    };
    within(WAIT, "пауза 1", clock.wait_sleepers(1)).await;
    assert_eq!(lists(&mut zam), 1);
    clock.advance(Duration::from_secs(1));
    assert_eq!(clock.sleepers(), 1, "пауза короче 2 с");
    clock.advance(Duration::from_secs(1));
    within(WAIT, "пауза 2", clock.wait_sleepers(1)).await;
    assert_eq!(lists(&mut zam), 1, "вторая попытка — новый список");
    clock.advance(Duration::from_secs(3));
    assert_eq!(clock.sleepers(), 1, "вторая пауза короче 4 с");
    zam.unlock_vault();
    clock.advance(Duration::from_secs(1));
    let ring = within(WAIT, "ключ", waiting).await.unwrap().unwrap();
    assert_eq!(ring.values().get(KEY_REF), Some(&vault_value()));
    assert_eq!(ring.revealed.len(), 1);
    assert!(!ring.revealed[0].cached);
    assert_eq!(zam.reveals().len(), 1);
    assert_eq!(
        cell.current().mask(&vault_value()),
        "[secret masked: demo-key]"
    );
    assert!(!format!("{ring:?}").contains(&vault_value()));
}
