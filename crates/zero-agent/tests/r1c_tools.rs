//! Приёмка R1-C1 «Инструменты и граница» (задача 14): сценарии бинарника `zero-agent` с
//! настоящими нативными инструментами (`za_tools::native_capability`), политикой,
//! чекпоинтами, доверием и маской секретов поверх поддельного llama-server
//! (`support::fake_llama`).
//!
//! - `s4_edit_and_test_loop` — S4 без памяти: поиск, чтение, проверка падает, правка,
//!   проверка проходит; чекпоинты git перед правкой и командами, восстановление,
//!   `audit --verify`, репозиторий ZeroAgent без `refs/zero-agent`;
//! - `ask_mode_approval_cycle` — режим `ask`: пауза на одобрении, `answer --approve`,
//!   resume с паузы `next = model` без `ChangeMidStep`, аудит `approval.resolved`;
//! - `untrusted_repo_asks_trust_once` — недоверенный проект: один вопрос доверия на
//!   пакет из двух команд; отказ — `denied: untrusted project` без нового вопроса;
//! - `crash_mid_bash_then_resume_asks_decision` — `--crash-at in-tool:N`: намерение без
//!   квитанции, resume — прерывание неизвестного исхода `unk-…`;
//! - `ac_sec_2_outside_access_always_journaled` — AC-SEC-2: 100 попыток наружу, у каждой
//!   след в журнале и строка «вне проекта» в stderr;
//! - `secrets_never_in_journal_or_spill` — значение `ZA_SECRET_T` из `.env` проекта не
//!   попадает ни в журнал, ни в выгрузку, ни в экспорт, ни в запросы к модели;
//! - `unc_path_rejected_without_io` — сетевой путь отклоняется до обращения к ФС.
//!
//! Проекты — `<ZeroAgent>/.tmp/r1c1-git/acceptance/<name>-<pid>-<n>` со своим `git init`
//! (идентичность и `core.autocrlf=false` явно), `ZA_HOME` — в `CARGO_TARGET_TMPDIR`.
//! `GIT_CEILING_DIRECTORIES` и `ZA_SECRET_*` передаются процессу `zero-agent` через
//! `Command::env`. Предел каждого процесса — 60 с × `ZA_TEST_TIME_SCALE`
//! (`support::process`); сигналы — по событиям журнала, порогов времени нет.
//!
//! Шаг с живым llama-server (`live_llama_s4_ask_mode`) помечен `#[ignore]` и берёт адрес
//! из `LLAMA_TEST_URL`:
//! `LLAMA_TEST_URL=http://127.0.0.1:8080 ZA_TEST_TIME_SCALE=6 cargo test -p zero-agent
//! --test r1c_tools -- --ignored --nocapture`.

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axum::response::Response;
use serde_json::{Value, json};
use za_protocol::EventKind;

use support::fake_llama::{Fake, call_reply, calls_reply, llama_config, text_reply};
use support::process::{
    Env, Out, count, interrupt_id, run_id, time_scale, unique_dir, unique_dir_in,
};

/// Окно `/props` сценариев.
const N_CTX: u32 = 32768;

/// Каталог тестов `ZA_HOME` и внешних каталогов в `CARGO_TARGET_TMPDIR`.
const SUITE: &str = "r1c-tools";

fn repo_root() -> PathBuf {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

/// Путь без `\\?\` (как его печатает и принимает бинарник).
fn shown(p: &Path) -> PathBuf {
    PathBuf::from(zero_agent::run::display_path(p))
}

/// Граница поиска репозитория git для тестов: git не видит репозиторий ZeroAgent.
fn ceiling() -> String {
    shown(&repo_root().join(".tmp"))
        .to_string_lossy()
        .into_owned()
}

/// git в каталоге `dir` с явной идентичностью и переводами строк.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.autocrlf=false",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CEILING_DIRECTORIES", ceiling())
        .output()
        .expect("git не запустился: нужен git в PATH");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Сценарий: поддельный сервер и окружение с git-проектом в `.tmp/r1c1-git/acceptance`.
struct Case {
    /// Поддельный сервер; у живого llama-server — `None`.
    srv: Option<Fake>,
    e: Env,
    /// Переменные процесса `zero-agent` (`GIT_CEILING_DIRECTORIES`, `ZA_SECRET_*`).
    vars: Vec<(String, String)>,
}

impl Case {
    fn new(name: &str, handler: support::fake_llama::ChatHandler) -> Self {
        let srv = Fake::start(N_CTX, true, handler);
        let base = srv.base.clone();
        Self::at(name, &base, Some(srv))
    }

    /// Окружение с моделью по адресу `base`.
    fn at(name: &str, base: &str, srv: Option<Fake>) -> Self {
        let home = unique_dir(SUITE, name).join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), llama_config(base)).unwrap();
        let project = unique_dir_in(&repo_root().join(".tmp/r1c1-git/acceptance"), name);
        let project = shown(&std::fs::canonicalize(&project).unwrap());
        git(&project, &["init", "-q"]);
        Self {
            srv,
            e: Env { home, project },
            vars: vec![("GIT_CEILING_DIRECTORIES".into(), ceiling())],
        }
    }

    /// Тела запросов к поддельному серверу.
    fn bodies(&self) -> Vec<Value> {
        self.srv.as_ref().expect("поддельный сервер").bodies()
    }

    fn var(mut self, k: &str, v: &str) -> Self {
        self.vars.push((k.into(), v.into()));
        self
    }

    fn za(&self, args: &[&str]) -> Out {
        let vars: Vec<(&str, &str)> = self
            .vars
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        self.e.run_with(args, &vars)
    }

    /// `run` с пределом чекпоинта git 20 с × `ZA_TEST_TIME_SCALE`: под нагрузкой станции
    /// чекпоинт не превращается в `slow`.
    fn run(&self, extra: &[&str]) -> Out {
        let ck = (20 * time_scale()).to_string();
        let mut args = vec![
            "run",
            "--project",
            self.e.project_arg(),
            "--checkpoint-timeout",
            &ck,
        ];
        args.extend_from_slice(extra);
        self.za(&args)
    }

    fn resume(&self, o: &Out) -> Out {
        self.run(&["--resume", &run_id(o).to_string()])
    }

    fn answer(&self, interrupt: &str, flag: &str) -> Out {
        let a = self.za(&[
            "answer",
            "--project",
            self.e.project_arg(),
            "--interrupt",
            interrupt,
            flag,
        ]);
        assert_eq!(a.code(), Some(0), "answer {interrupt} {flag}: {a:?}");
        a
    }

    fn trust(&self) {
        let o = self.za(&["trust", "--project", self.e.project_arg()]);
        assert_eq!(o.code(), Some(0), "trust: {o:?}");
    }

    fn write(&self, rel: &str, text: &str) {
        std::fs::write(self.e.project.join(rel), text).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.e.project.join(rel)).unwrap()
    }
}

/// Сообщения тела запроса.
fn messages(body: &Value) -> &[Value] {
    body["messages"].as_array().map_or(&[], Vec::as_slice)
}

/// Текст содержимого сообщения (строка или текстовые части).
fn content(m: &Value) -> String {
    match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Число результатов инструментов в теле запроса.
fn tool_results(body: Option<&Value>) -> usize {
    body.map_or(0, |b| {
        messages(b).iter().filter(|m| m["role"] == "tool").count()
    })
}

/// Результат вызова `id` в последнем запросе, где он есть.
fn result_of(bodies: &[Value], id: &str) -> String {
    bodies
        .iter()
        .rev()
        .flat_map(|b| messages(b).iter())
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == id)
        .map(content)
        .unwrap_or_else(|| panic!("нет результата вызова {id}"))
}

/// Вызовы `(id, name, args)` для [`calls_reply`].
fn call(id: impl Into<String>, name: &str, args: Value) -> (String, String, String) {
    (id.into(), name.to_owned(), args.to_string())
}

/// События вида `kind` с полем `call_id == id`.
fn of_call<'a>(events: &'a [(EventKind, Value)], kind: EventKind, id: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|(k, p)| *k == kind && p["call_id"] == id)
        .map(|(_, p)| p)
        .collect()
}

/// Строки stderr «вне проекта».
fn outside_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| l.starts_with("zero-agent: вне проекта: "))
        .collect()
}

// ---- S4 без памяти ----

/// Проверка проекта: печатает FAIL и выходит с кодом 1, пока в `calc.txt` нет
/// `result = 4`, иначе PASS и код 0. Оболочка `bash` — PowerShell на Windows (D85).
fn check_cmd() -> &'static str {
    if cfg!(windows) {
        "if ((Get-Content calc.txt -Raw) -match 'result = 4') { 'PASS' } else { 'FAIL'; exit 1 }"
    } else {
        "if grep -q 'result = 4' calc.txt; then echo PASS; else echo FAIL; exit 1; fi"
    }
}

const CALC_BEFORE: &str = "# сумма 2 + 2\nresult = 5\n";

/// Модель S4: search → read → bash check → edit → bash check → ответ.
fn s4_model(seen: &[Value]) -> Response {
    let check = json!({ "cmd": check_cmd() }).to_string();
    match tool_results(seen.last()) {
        0 => call_reply("c1", "search", r#"{"pattern":"result","path":"."}"#),
        1 => call_reply("c2", "read", r#"{"path":"calc.txt"}"#),
        2 => call_reply("c3", "bash", &check),
        3 => call_reply(
            "c4",
            "edit",
            r#"{"path":"calc.txt","old":"result = 5","new":"result = 4"}"#,
        ),
        4 => call_reply("c5", "bash", &check),
        _ => text_reply(&["исправлено"]),
    }
}

#[test]
fn s4_edit_and_test_loop() {
    let c = Case::new("s4", s4_model);
    c.write("calc.txt", CALC_BEFORE);
    git(&c.e.project, &["add", "calc.txt"]);
    git(&c.e.project, &["commit", "-q", "-m", "init"]);
    c.trust();

    let o = c.run(&[
        "--mode",
        "auto",
        "Исправь calc.txt, чтобы проверка проходила",
    ]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert_eq!(o.stdout, "исправлено\n", "{o:?}");
    assert_eq!(c.read("calc.txt"), "# сумма 2 + 2\nresult = 4\n");

    // Результаты инструментов, которые видела модель.
    let bodies = c.bodies();
    assert!(
        result_of(&bodies, "c1").contains("calc.txt:2:result = 5"),
        "{}",
        result_of(&bodies, "c1")
    );
    assert!(result_of(&bodies, "c2").contains("result = 5"));
    let fail = result_of(&bodies, "c3");
    assert!(
        fail.contains("exit code: 1") && fail.contains("FAIL"),
        "{fail}"
    );
    assert!(
        result_of(&bodies, "c4").contains("edited calc.txt"),
        "{}",
        result_of(&bodies, "c4")
    );
    let pass = result_of(&bodies, "c5");
    assert!(
        pass.contains("exit code: 0") && pass.contains("PASS"),
        "{pass}"
    );

    // Чекпоинт git перед каждой правкой и командой, после `tool.started`.
    let run = run_id(&o);
    let events = c.e.events(run);
    let mut ids = Vec::new();
    for call_id in ["c3", "c4", "c5"] {
        let started = events
            .iter()
            .position(|(k, p)| *k == EventKind::ToolStarted && p["call_id"] == call_id)
            .unwrap_or_else(|| panic!("нет tool.started {call_id}"));
        let finished = events
            .iter()
            .position(|(k, p)| *k == EventKind::ToolFinished && p["call_id"] == call_id)
            .unwrap_or_else(|| panic!("нет tool.finished {call_id}"));
        let cps: Vec<&Value> = events[started..finished]
            .iter()
            .filter(|(k, p)| *k == EventKind::CheckpointCreated && p["call_id"] == call_id)
            .map(|(_, p)| p)
            .collect();
        assert_eq!(cps.len(), 1, "{call_id}: {events:?}");
        assert_eq!(cps[0]["kind"], "git", "{call_id}: {}", cps[0]);
        ids.push(cps[0]["id"].as_str().unwrap().to_owned());
    }
    // У чтений чекпоинтов нет.
    assert_eq!(
        count(&events, EventKind::CheckpointCreated),
        3,
        "{events:?}"
    );

    let l = c.za(&["checkpoints", "list", "--project", c.e.project_arg()]);
    assert_eq!(l.code(), Some(0), "{l:?}");
    for id in &ids {
        assert!(l.stdout.contains(id.as_str()), "{id}: {l:?}");
    }

    // Чекпоинт самой правки (c4) снят до записи: восстановление возвращает файл до
    // правки (инвариант 10). Затем чекпоинт после правки (c5) — файл после неё, и первый
    // (c3, перед первой проверкой) — снова до правки.
    let restore = |id: &str| {
        let r = c.za(&["checkpoints", "restore", "--project", c.e.project_arg(), id]);
        assert_eq!(r.code(), Some(0), "restore {id}: {r:?}");
    };
    restore(&ids[1]);
    assert_eq!(c.read("calc.txt"), CALC_BEFORE);
    restore(&ids[2]);
    assert_eq!(
        c.read("calc.txt"),
        "# сумма 2 + 2
result = 4
"
    );
    restore(&ids[0]);
    assert_eq!(c.read("calc.txt"), CALC_BEFORE);

    let v = c.za(&["audit", "--project", c.e.project_arg(), "--verify"]);
    assert_eq!(v.code(), Some(0), "{v:?}");

    // Чекпоинты живут в своём репозитории агента: ни у ZeroAgent, ни у проекта нет
    // ссылок `refs/zero-agent`, история проекта не тронута.
    assert_eq!(git(&repo_root(), &["for-each-ref", "refs/zero-agent"]), "");
    assert_eq!(git(&c.e.project, &["for-each-ref", "refs/zero-agent"]), "");
    assert_eq!(
        git(&c.e.project, &["rev-list", "--count", "HEAD"]).trim(),
        "1"
    );
}

// ---- Режим ask ----

/// Модель: создание `note.txt` правкой, затем ответ.
fn note_model(seen: &[Value]) -> Response {
    if tool_results(seen.last()) > 0 {
        return text_reply(&["готово"]);
    }
    call_reply(
        "e1",
        "edit",
        r#"{"path":"note.txt","old":"","new":"hello\n"}"#,
    )
}

#[test]
fn ask_mode_approval_cycle() {
    let c = Case::new("ask", note_model);
    c.trust();
    let o = c.run(&["--mode", "ask", "Создай note.txt"]);
    assert_eq!(o.code(), Some(3), "{o:?}");
    assert!(o.stderr.contains("needs_approval"), "{o:?}");
    assert!(o.stderr.contains("zero-agent: вызов: edit"), "{o:?}");
    assert!(o.stderr.contains("zero-agent: причины: mode"), "{o:?}");
    assert!(!c.e.project.join("note.txt").exists());
    let id = interrupt_id(&o);
    assert!(id.starts_with("appr-"), "{id}: {o:?}");
    // Пауза посреди шага: resume реплеит шаг с узла `model`.
    let snap = c.e.snapshot(run_id(&o));
    assert_eq!(snap["next"], "model", "{snap}");

    c.answer(&id, "--approve");
    let r = c.resume(&o);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(r.stdout, "готово\n", "{r:?}");
    assert!(!r.stderr.contains("change_mid_step"), "{r:?}");
    assert_eq!(c.read("note.txt"), "hello\n");

    let a = c.e.last_audit("approval.").expect("аудит одобрения");
    assert_eq!(a.action, "approval.resolved", "{a:?}");
    assert_eq!(a.target, id, "{a:?}");
    let events = c.e.events(run_id(&o));
    assert_eq!(count(&events, EventKind::InterruptOpened), 1, "{events:?}");
}

// ---- Недоверенный проект ----

/// Модель: пакет из двух команд, затем ответ.
fn two_commands(seen: &[Value]) -> Response {
    if tool_results(seen.last()) > 0 {
        return text_reply(&["готово"]);
    }
    calls_reply(&[
        call("b1", "bash", json!({"cmd": "echo first-cmd"})),
        call("b2", "bash", json!({"cmd": "echo second-cmd"})),
    ])
}

#[test]
fn untrusted_repo_asks_trust_once() {
    // Доверие дано: один вопрос, обе команды исполнены.
    let c = Case::new("untrusted-approve", two_commands);
    let o = c.run(&["--mode", "auto", "Запусти две команды"]);
    assert_eq!(o.code(), Some(3), "{o:?}");
    assert!(o.stderr.contains("needs_trust"), "{o:?}");
    let id = interrupt_id(&o);
    assert!(id.starts_with("trust-"), "{id}: {o:?}");
    c.answer(&id, "--approve");
    let r = c.resume(&o);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(r.stdout, "готово\n", "{r:?}");
    let bodies = c.bodies();
    assert!(result_of(&bodies, "b1").contains("first-cmd"));
    assert!(result_of(&bodies, "b2").contains("second-cmd"));
    let events = c.e.events(run_id(&o));
    assert_eq!(count(&events, EventKind::InterruptOpened), 1, "{events:?}");
    assert_eq!(c.e.last_audit("trust.").unwrap().action, "trust.granted");
    // Доверие разово: следующий run того же проекта не спрашивает (решение заказчика (б)):
    // набор, записанный ответом, совпадает с пересчитанным при старте run.
    let n = c.run(&["--mode", "auto", "Запусти две команды ещё раз"]);
    assert_eq!(n.code(), Some(0), "{n:?}");
    assert!(!n.stderr.contains("проект не доверен"), "{n:?}");
    let events = c.e.events(run_id(&n));
    assert_eq!(count(&events, EventKind::InterruptOpened), 0, "{events:?}");
    assert_eq!(count(&events, EventKind::ToolStarted), 2, "{events:?}");
    assert_eq!(c.e.snapshot(run_id(&n))["trusted"], true);

    // Отказ в доверии: обе команды не исполнены, вопрос один.
    let d = Case::new("untrusted-deny", two_commands);
    let o = d.run(&["--mode", "auto", "Запусти две команды"]);
    assert_eq!(o.code(), Some(3), "{o:?}");
    let id = interrupt_id(&o);
    assert!(id.starts_with("trust-"), "{id}: {o:?}");
    d.answer(&id, "--deny");
    let r = d.resume(&o);
    assert_eq!(r.code(), Some(0), "{r:?}");
    let bodies = d.bodies();
    for call_id in ["b1", "b2"] {
        let res = result_of(&bodies, call_id);
        assert!(
            res.contains("denied: untrusted project"),
            "{call_id}: {res}"
        );
    }
    let events = d.e.events(run_id(&o));
    assert_eq!(count(&events, EventKind::InterruptOpened), 1, "{events:?}");
    assert_eq!(count(&events, EventKind::ToolStarted), 0, "{events:?}");
}

// ---- Чтение из сети и следующие команды ----

/// Модель: сетевая команда (`git fetch` без удалённых репозиториев — сети нет, но
/// команда сетевая, D77), затем команда внутри проекта, затем ответ.
fn network_then_command(seen: &[Value]) -> Response {
    match tool_results(seen.last()) {
        0 => call_reply("n1", "bash", r#"{"cmd":"git fetch"}"#),
        1 => call_reply("n2", "bash", r#"{"cmd":"echo after-network"}"#),
        _ => text_reply(&["готово"]),
    }
}

/// Решение заказчика 2026-11-11 (D82): метка `network` — происхождение данных; следующая
/// команда внутри проекта в `auto` идёт без вопроса.
#[test]
fn network_read_does_not_ask_next_command() {
    let c = Case::new("network", network_then_command);
    c.trust();
    let o = c.run(&["--mode", "auto", "Обнови и продолжи"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert_eq!(
        o.stdout,
        "готово
",
        "{o:?}"
    );
    let events = c.e.events(run_id(&o));
    assert_eq!(count(&events, EventKind::InterruptOpened), 0, "{events:?}");
    let f1 = of_call(&events, EventKind::ToolFinished, "n1");
    assert_eq!(f1.len(), 1, "{events:?}");
    assert_eq!(f1[0]["tainted"], true, "{}", f1[0]);
    let f2 = of_call(&events, EventKind::ToolFinished, "n2");
    assert_eq!(f2.len(), 1, "{events:?}");
    let bodies = c.bodies();
    assert!(
        result_of(&bodies, "n1").contains("[untrusted content: network]"),
        "{}",
        result_of(&bodies, "n1")
    );
    assert!(result_of(&bodies, "n2").contains("after-network"));
    let snap = c.e.snapshot(run_id(&o));
    assert!(
        snap["taint"]
            .as_array()
            .unwrap()
            .contains(&json!("network")),
        "{snap}"
    );
}

// ---- Сбой посреди команды ----

/// Модель: команда, пишущая `marker.txt`, затем ответ.
fn marker_model(seen: &[Value]) -> Response {
    if tool_results(seen.last()) > 0 {
        return text_reply(&["готово"]);
    }
    call_reply("m1", "bash", r#"{"cmd":"echo crashed > marker.txt"}"#)
}

#[test]
fn crash_mid_bash_then_resume_asks_decision() {
    let c = Case::new("crash", marker_model);
    c.trust();
    let o = c.run(&["--mode", "auto", "--crash-at", "in-tool:1", "Запиши marker"]);
    // `abort()`: Windows — `__fastfail`, код STATUS_STACK_BUFFER_OVERRUN (0xC0000409);
    // Unix — сигнал SIGABRT, кода нет.
    if cfg!(windows) {
        assert_eq!(o.code(), Some(0xC000_0409_u32 as i32), "{o:?}");
    } else {
        assert_eq!(o.code(), None, "{o:?}");
    }
    assert!(!o.stderr.contains("ответьте:"), "{o:?}");
    // Команда исполнена, квитанции нет: tool.started и чекпоинт есть, tool.finished нет.
    assert!(c.e.project.join("marker.txt").exists(), "{o:?}");
    let run = run_id(&o);
    let events = c.e.events(run);
    assert_eq!(of_call(&events, EventKind::ToolStarted, "m1").len(), 1);
    assert_eq!(
        of_call(&events, EventKind::CheckpointCreated, "m1").len(),
        1
    );
    assert!(of_call(&events, EventKind::ToolFinished, "m1").is_empty());

    // Resume не повторяет команду с неизвестным исходом, а спрашивает.
    std::fs::remove_file(c.e.project.join("marker.txt")).unwrap();
    let r = c.resume(&o);
    assert_eq!(r.code(), Some(3), "{r:?}");
    assert!(r.stderr.contains("needs_decision"), "{r:?}");
    let id = interrupt_id(&r);
    assert!(id.starts_with("unk-"), "{id}: {r:?}");
    assert!(r.stderr.contains("--done | --not-done"), "{r:?}");
    assert!(!c.e.project.join("marker.txt").exists());

    c.answer(&id, "--done");
    let f = c.resume(&o);
    assert_eq!(f.code(), Some(0), "{f:?}");
    assert_eq!(f.stdout, "готово\n", "{f:?}");
    assert!(
        !c.e.project.join("marker.txt").exists(),
        "команда с неизвестным исходом повторена"
    );
}

// ---- AC-SEC-2 ----

/// Попыток каждого вида.
const PER_KIND: usize = 20;

/// Внешний каталог из задачи: `ACSEC2 <набор> |<путь>|`.
fn task_marker(seen: &[Value]) -> (String, String) {
    let first = seen.first().expect("нет запроса");
    let text = messages(first)
        .iter()
        .filter(|m| m["role"] == "user")
        .map(content)
        .find(|t| t.contains("ACSEC2 "))
        .expect("нет задачи ACSEC2");
    let at = text.find("ACSEC2 ").unwrap() + "ACSEC2 ".len();
    let set = text[at..at + 1].to_owned();
    let rest = &text[at..];
    let a = rest.find('|').unwrap() + 1;
    let b = a + rest[a..].find('|').unwrap();
    (set, rest[a..b].to_owned())
}

/// Модель AC-SEC-2: набор A — 20 `read`, 20 `search`, 20 `glob` с абсолютным шаблоном;
/// набор B — 20 `edit` (создание файла) и 20 `bash "cat <внешний путь>"`. Один пакет,
/// затем ответ. Сервер общий для обоих run: задача читается из запроса текущего run.
fn acsec2_model(seen: &[Value]) -> Response {
    let cur = seen.last().unwrap();
    if tool_results(Some(cur)) > 0 {
        return text_reply(&["готово"]);
    }
    let (set, ext) = task_marker(std::slice::from_ref(cur));
    let mut calls = Vec::new();
    if set == "A" {
        for i in 0..PER_KIND {
            calls.push(call(
                format!("r{i}"),
                "read",
                json!({"path": format!("{ext}/f{i:02}.txt")}),
            ));
        }
        for i in 0..PER_KIND {
            calls.push(call(
                format!("s{i}"),
                "search",
                json!({"pattern": format!("data {i:02}"), "path": ext}),
            ));
        }
        for i in 0..PER_KIND {
            calls.push(call(
                format!("g{i}"),
                "glob",
                json!({"pattern": format!("{ext}/f{i:02}*.txt")}),
            ));
        }
    } else {
        for i in 0..PER_KIND {
            calls.push(call(
                format!("e{i}"),
                "edit",
                json!({"path": format!("{ext}/new{i:02}.txt"), "old": "", "new": format!("created {i:02}\n")}),
            ));
        }
        for i in 0..PER_KIND {
            calls.push(call(
                format!("b{i}"),
                "bash",
                json!({"cmd": format!("cat '{ext}/f{i:02}.txt'")}),
            ));
        }
    }
    calls_reply(&calls)
}

#[test]
fn ac_sec_2_outside_access_always_journaled() {
    let c = Case::new("acsec2", acsec2_model);
    c.trust();
    let ext_dir = unique_dir(SUITE, "acsec2-outside");
    for i in 0..PER_KIND {
        std::fs::write(
            ext_dir.join(format!("f{i:02}.txt")),
            format!("outside data {i:02}\n"),
        )
        .unwrap();
    }
    let ext_real = shown(&std::fs::canonicalize(&ext_dir).unwrap());
    // В аргументах модели — путь через `/` (`globset` понимает только его).
    let ext = ext_real.to_string_lossy().replace('\\', "/");
    let grant = format!("{}=ro", ext_real.display());

    // Набор A: 60 чтений по гранту ro — ни одного вопроса, у каждой попытки ровно одно
    // `boundary.outside` с `granted = true` и строка «вне проекта».
    let o = c.run(&[
        "--mode",
        "auto",
        "--grant",
        &grant,
        &format!("ACSEC2 A |{ext}|"),
    ]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let events = c.e.events(run_id(&o));
    assert_eq!(count(&events, EventKind::InterruptOpened), 0, "{events:?}");
    let mut paths_a = 0;
    for kind in ["r", "s", "g"] {
        for i in 0..PER_KIND {
            let id = format!("{kind}{i}");
            let b = of_call(&events, EventKind::BoundaryOutside, &id);
            assert_eq!(b.len(), 1, "{id}: {events:?}");
            let paths = b[0]["paths"].as_array().unwrap();
            assert!(!paths.is_empty(), "{id}: {}", b[0]);
            for p in paths {
                assert_eq!(p["granted"], true, "{id}: {}", b[0]);
                assert_eq!(p["access"], "read", "{id}: {}", b[0]);
            }
            paths_a += paths.len();
        }
    }
    assert_eq!(count(&events, EventKind::BoundaryOutside), 3 * PER_KIND);
    assert_eq!(outside_lines(&o.stderr).len(), paths_a, "{}", o.stderr);
    let bodies = c.bodies();
    for i in 0..PER_KIND {
        let r = result_of(&bodies, &format!("r{i}"));
        assert!(r.contains(&format!("outside data {i:02}")), "r{i}: {r}");
    }

    // Набор B: 20 правок и 20 команд без гранта — каждая попытка — прерывание с путём и
    // причиной `outside_project`, после одобрения — `boundary.outside` с
    // `granted = false`.
    let first = c.run(&["--mode", "auto", &format!("ACSEC2 B |{ext}|")]);
    let run = run_id(&first);
    let mut stderr = String::new();
    let mut o = first;
    let mut rounds = 0;
    // Общий предел набора B (81 процесс); у каждого процесса свой предел — 60 с × масштаб.
    let started = std::time::Instant::now();
    let total = std::time::Duration::from_secs(600) * time_scale();
    while o.code() == Some(3) {
        rounds += 1;
        assert!(rounds <= 2 * PER_KIND, "лишние вопросы: {o:?}");
        assert!(
            started.elapsed() < total,
            "набор B не уложился в {total:?}: {rounds} раундов"
        );
        stderr.push_str(&o.stderr);
        let id = interrupt_id(&o);
        assert!(id.starts_with("appr-"), "{id}: {o:?}");
        c.answer(&id, "--approve");
        o = c.run(&["--resume", &run.to_string()]);
    }
    stderr.push_str(&o.stderr);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert_eq!(rounds, 2 * PER_KIND);

    let events = c.e.events(run);
    let opened: Vec<&Value> = events
        .iter()
        .filter(|(k, _)| *k == EventKind::InterruptOpened)
        .map(|(_, p)| p)
        .collect();
    assert_eq!(opened.len(), 2 * PER_KIND, "{events:?}");
    let mut asked: BTreeMap<String, usize> = BTreeMap::new();
    for ev in &opened {
        let p = &ev["payload"];
        assert!(
            p["reasons"]
                .as_array()
                .unwrap()
                .contains(&json!("outside_project")),
            "{ev}"
        );
        assert!(
            p["paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["inside"] == false && x["path"].as_str().is_some()),
            "{ev}"
        );
        *asked
            .entry(p["call_id"].as_str().unwrap().to_owned())
            .or_default() += 1;
    }
    for (kind, file, access) in [("e", "new", "write"), ("b", "f", "read")] {
        for i in 0..PER_KIND {
            let id = format!("{kind}{i}");
            assert_eq!(asked.get(&id), Some(&1), "{id}: {asked:?}");
            let b = of_call(&events, EventKind::BoundaryOutside, &id);
            assert_eq!(b.len(), 1, "{id}: {events:?}");
            let paths = b[0]["paths"].as_array().unwrap();
            assert!(
                paths.iter().all(|p| p["granted"] == false),
                "{id}: {}",
                b[0]
            );
            assert!(
                paths.iter().any(|p| p["access"] == access),
                "{id}: {}",
                b[0]
            );
            // stderr: строка «вне проекта» у вопроса и у исполнения.
            let name = format!("{file}{i:02}.txt");
            let lines = outside_lines(&stderr)
                .into_iter()
                .filter(|l| l.contains(&name))
                .count();
            assert!(lines >= 2, "{id}: {lines} строк «вне проекта»\n{stderr}");
        }
    }
    assert_eq!(count(&events, EventKind::BoundaryOutside), 2 * PER_KIND);
    let bodies = c.bodies();
    for i in 0..PER_KIND {
        assert_eq!(
            std::fs::read_to_string(ext_dir.join(format!("new{i:02}.txt"))).unwrap(),
            format!("created {i:02}\n")
        );
        let r = result_of(&bodies, &format!("b{i}"));
        assert!(r.contains(&format!("outside data {i:02}")), "b{i}: {r}");
    }
}

// ---- Секреты ----

/// Значение секрета: не формат настоящего токена, длина ≥ 8 (маска известных значений).
const SECRET: &str = "zeta-acceptance-secret-7f3c9a1b";

/// Модель: `read .env` и `bash "cat .env"`, затем ответ.
fn env_model(seen: &[Value]) -> Response {
    if tool_results(seen.last()) > 0 {
        return text_reply(&["прочитано"]);
    }
    calls_reply(&[
        call("x1", "read", json!({"path": ".env"})),
        call("x2", "bash", json!({"cmd": "cat .env"})),
    ])
}

/// Все обычные файлы под `dir`.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let t = e.file_type().unwrap();
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() {
                out.push(e.path());
            }
        }
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn secrets_never_in_journal_or_spill() {
    let c = Case::new("secrets", env_model).var("ZA_SECRET_T", SECRET);
    // `.env` игнорируется git: в чекпоинт он не входит (D89).
    c.write(".gitignore", ".env\n");
    let mut env_text = format!("TOKEN={SECRET}\n");
    for i in 0..4000 {
        env_text.push_str(&format!(
            "FILLER_{i:04}=0123456789abcdefghijklmnopqrstuvwxyz\n"
        ));
    }
    env_text.push_str(&format!("TOKEN_AGAIN={SECRET}\n"));
    c.write(".env", &env_text);
    c.trust();

    let o = c.run(&["--mode", "auto", "Прочитай .env"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert_eq!(o.stdout, "прочитано\n", "{o:?}");
    assert!(!o.stderr.contains(SECRET), "{o:?}");
    let needle = SECRET.as_bytes();

    // Контекст модели.
    let bodies = c.bodies();
    for b in &bodies {
        assert!(!b.to_string().contains(SECRET), "секрет в запросе к модели");
    }
    let masked = result_of(&bodies, "x1");
    assert!(masked.contains("[secret masked: "), "{masked}");

    // Журнал проекта (payload событий, квитанции, блобы в строке и в файлах, WAL) и
    // всё прочее в ZA_HOME.
    let home_files = files_under(&c.e.home);
    assert!(
        home_files.iter().any(|f| f.ends_with("journal.db")),
        "{home_files:?}"
    );
    for f in &home_files {
        let data = std::fs::read(f).unwrap();
        assert!(!contains(&data, needle), "секрет в {}", f.display());
    }

    // Выгрузка длинного вывода: файлы есть, секрета в них нет, маска есть.
    let spill = c.e.project.join(".zero-agent/work/spill");
    let spilled = files_under(&spill);
    assert!(!spilled.is_empty(), "длинный вывод не выгружен");
    let mut any_mask = false;
    for f in files_under(&c.e.project.join(".zero-agent")) {
        let data = std::fs::read(&f).unwrap();
        assert!(!contains(&data, needle), "секрет в {}", f.display());
        any_mask |= contains(&data, b"[secret masked: ");
    }
    assert!(any_mask, "в выгрузке нет маски: {spilled:?}");

    // Выгрузка журнала run.
    let x = c.za(&[
        "journal",
        "export",
        "--project",
        c.e.project_arg(),
        "--run",
        &run_id(&o).to_string(),
    ]);
    assert_eq!(x.code(), Some(0), "{x:?}");
    assert!(!x.stdout.is_empty());
    assert!(!x.stdout.contains(SECRET), "секрет в journal export");
    assert!(x.stdout.contains("[secret masked: "), "{}", x.stdout.len());
}

// ---- Секреты в чекпоинтах (R1-C2 D103) ----

/// Модель: чтение `.env` и `config.txt`, затем правка обоих, затем ответ.
fn secret_edit_model(seen: &[Value]) -> Response {
    match tool_results(seen.last()) {
        0 => calls_reply(&[
            call("r1", "read", json!({"path": ".env"})),
            call("r2", "read", json!({"path": "config.txt"})),
        ]),
        2 => calls_reply(&[
            call(
                "e1",
                "edit",
                json!({"path": ".env", "old": "MODE=dev", "new": "MODE=prod"}),
            ),
            call(
                "e2",
                "edit",
                json!({"path": "config.txt", "old": "level = 1", "new": "level = 2"}),
            ),
        ]),
        _ => text_reply(&["готово"]),
    }
}

#[test]
fn edit_of_env_reports_no_checkpoint() {
    // `config.txt` — секрет по содержимому: в нём значение `ZA_SECRET_T` (фильтр бинарника).
    let c = Case::new("secret-edit", secret_edit_model).var("ZA_SECRET_T", SECRET);
    c.write(".env", "MODE=dev\n");
    c.write("config.txt", &format!("level = 1\ntoken = {SECRET}\n"));
    c.write("a.txt", "plain\n");
    git(&c.e.project, &["add", "a.txt"]);
    git(&c.e.project, &["commit", "-q", "-m", "init"]);
    c.trust();

    let o = c.run(&["--mode", "auto", "Поправь .env и config.txt"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert_eq!(o.stdout, "готово\n", "{o:?}");
    assert_eq!(c.read(".env"), "MODE=prod\n");

    let bodies = c.bodies();
    for id in ["e1", "e2"] {
        let r = result_of(&bodies, id);
        assert!(
            r.contains("[no checkpoint for this file: it looks like a secret]"),
            "{id}: {r}"
        );
    }
    let events = c.e.events(run_id(&o));
    for (id, path) in [("e1", ".env"), ("e2", "config.txt")] {
        let cps = of_call(&events, EventKind::CheckpointCreated, id);
        assert_eq!(cps.len(), 1, "{id}: {events:?}");
        assert_eq!(cps[0]["kind"], "git", "{id}: {}", cps[0]);
        assert_eq!(cps[0]["secret_skipped"], json!([path]), "{id}: {}", cps[0]);
    }
    // Значения секрета нет в каталоге чекпоинтов (свой репозиторий, кэш stat, индексы) и
    // в маркерах повтора правок `edits/*.json` (ревью задачи 2, сомнение 1: diff маскируется).
    let cps: Vec<PathBuf> = files_under(&c.e.home)
        .into_iter()
        .filter(|f| {
            f.components()
                .any(|p| p.as_os_str() == "checkpoints" || p.as_os_str() == "edits")
        })
        .collect();
    assert!(
        cps.iter().any(|f| f.ends_with("stat.json")),
        "нет каталога чекпоинтов: {cps:?}"
    );
    assert!(
        cps.iter()
            .any(|f| f.parent().is_some_and(|d| d.ends_with("edits"))
                && f.extension().is_some_and(|e| e == "json")),
        "нет маркеров правок: {cps:?}"
    );
    for f in &cps {
        let data = std::fs::read(f).unwrap();
        assert!(
            !contains(&data, SECRET.as_bytes()),
            "секрет в {}",
            f.display()
        );
    }
    // Восстановление чекпоинта перед правкой `config.txt`: секреты не тронуты и названы
    // (ревью задачи 2, I2): `--json`, stderr и аудит.
    let id = of_call(&events, EventKind::CheckpointCreated, "e2")[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = c.za(&[
        "checkpoints",
        "restore",
        "--project",
        c.e.project_arg(),
        &id,
        "--delete-new",
        "--json",
    ]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    let v: Value = serde_json::from_str(r.stdout.trim()).unwrap();
    assert_eq!(v["secret_untouched"], json!([".env", "config.txt"]), "{v}");
    assert_eq!(c.read(".env"), "MODE=prod\n");
    assert!(c.read("config.txt").contains("level = 2"), "{r:?}");
    let r = c.za(&[
        "checkpoints",
        "restore",
        "--project",
        c.e.project_arg(),
        &id,
    ]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert!(
        r.stderr
            .contains("не тронуты как секреты: .env, config.txt"),
        "{r:?}"
    );
}

// ---- Сетевой путь ----

/// Модель: `read \\192.0.2.1\x`, затем ответ.
fn unc_model(seen: &[Value]) -> Response {
    if tool_results(seen.last()) > 0 {
        return text_reply(&["готово"]);
    }
    call_reply("u1", "read", r#"{"path":"\\\\192.0.2.1\\x"}"#)
}

/// Windows: UNC-путь отклоняется лексически (D76), до обращения к ФС и без вопроса; на
/// Unix `\` — обычный символ имени: это файл внутри проекта, его нет. Проверка — по
/// результату и журналу, без замера времени.
#[test]
fn unc_path_rejected_without_io() {
    let c = Case::new("unc", unc_model);
    let o = c.run(&["--mode", "auto", "Прочитай сетевой файл"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let res = result_of(&c.bodies(), "u1");
    if cfg!(windows) {
        assert!(res.contains("network paths are not supported"), "{res}");
    } else {
        assert!(res.contains("not found"), "{res}");
    }
    let events = c.e.events(run_id(&o));
    assert_eq!(count(&events, EventKind::BoundaryOutside), 0, "{events:?}");
    assert_eq!(count(&events, EventKind::InterruptOpened), 0, "{events:?}");
    assert!(outside_lines(&o.stderr).is_empty(), "{o:?}");
}

// ---- Живой llama-server ----

/// Ручная проверка S4 без памяти (задача 14, шаг 3): настоящая модель чинит `calc.txt` в
/// своём git-проекте в режиме `ask`; каждый вопрос одобряется. Итог печатается для
/// `docs/journal.md`.
#[test]
#[ignore = "живой llama-server: LLAMA_TEST_URL=http://127.0.0.1:8080 ZA_TEST_TIME_SCALE=6 cargo test -p zero-agent --test r1c_tools -- --ignored --nocapture"]
fn live_llama_s4_ask_mode() {
    let Ok(url) = std::env::var("LLAMA_TEST_URL") else {
        panic!(
            "LLAMA_TEST_URL не задан: запуск — LLAMA_TEST_URL=http://127.0.0.1:8080 \
             ZA_TEST_TIME_SCALE=6 cargo test -p zero-agent --test r1c_tools -- --ignored --nocapture"
        );
    };
    let mut c = Case::at("live-s4", url.trim_end_matches('/'), None);
    c.write("calc.txt", CALC_BEFORE);
    git(&c.e.project, &["add", "calc.txt"]);
    git(&c.e.project, &["commit", "-q", "-m", "init"]);
    c.trust();
    c.vars.push(("ZA_LOG".into(), "info".into()));
    let task = format!(
        "The check command `{}` fails. Find the bug in calc.txt with the tools, fix it with \
         edit, run the check again with bash and reply with one short sentence when it passes.",
        check_cmd()
    );
    let mut o = c.run(&["--mode", "ask", &task]);
    let run = run_id(&o);
    let mut asked = 0;
    while o.code() == Some(3) && asked < 30 {
        asked += 1;
        let id = interrupt_id(&o);
        let flag = if id.starts_with("unk-") {
            "--not-done"
        } else {
            "--approve"
        };
        println!("вопрос {asked}: {id}");
        if id.starts_with("input-") {
            let a = c.za(&[
                "answer",
                "--project",
                c.e.project_arg(),
                "--interrupt",
                &id,
                "--text",
                "continue",
            ]);
            assert_eq!(a.code(), Some(0), "{a:?}");
        } else {
            c.answer(&id, flag);
        }
        o = c.run(&["--resume", &run.to_string()]);
    }
    let events = c.e.events(run);
    println!(
        "итог: код {:?}, вопросов {asked}, вызовов {}, чекпоинтов {}, ответ: {}",
        o.code(),
        count(&events, EventKind::ToolStarted),
        count(&events, EventKind::CheckpointCreated),
        o.stdout.trim()
    );
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(
        c.read("calc.txt").contains("result = 4"),
        "{}",
        c.read("calc.txt")
    );
    let v = c.za(&["audit", "--project", c.e.project_arg(), "--verify"]);
    assert_eq!(v.code(), Some(0), "{v:?}");
}
