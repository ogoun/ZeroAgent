//! Приёмка R1-B1: команды `run` и `answer` из отдельного процесса.
//!
//! Каждый тест получает свои `ZA_HOME` и каталог проекта внутри `CARGO_TARGET_TMPDIR`.
//! Поддельный OpenAI-совместимый llama-server — axum в процессе теста (многопоточный
//! runtime, пока основной поток ждёт бинарник; общий с приёмкой R1-B2 — `support`).
//! Процесс бинарника, не завершившийся за 60 с × `ZA_TEST_TIME_SCALE`, убивается, тест
//! падает с понятной паникой. Порогов времени, которые на деле мерили бы задержку
//! журнала, нет.

mod support;

use std::time::Duration;

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use za_protocol::EventKind;

use support::fake_llama::{Fake, call_reply, chunk, llama_config, paced_reply, tail, text_reply};
use support::process::{Env, count, env_in, interrupt_id, is_subsequence, journal_exists, run_id};

/// Окно `/props` сценариев R1-B1.
const N_CTX: u32 = 32768;

/// Сервер R1-B1: окно 32768, расход в стриме.
fn fake(chat: support::fake_llama::ChatHandler) -> Fake {
    Fake::start(N_CTX, true, chat)
}

/// Чистые `ZA_HOME` и каталог проекта; `config` — текст `config.toml` (пустой — без файла).
fn env(name: &str, config: &str) -> Env {
    env_in("r1b-loop", name, config)
}

/// «Пари», через 500 мс «ж» и конец: первая `model.delta` уходит на шину раньше
/// второго чанка (склейка сбрасывается через 200 мс, D8).
fn paced_paris(_: &[Value]) -> Response {
    let first =
        chunk(json!({"role": "assistant", "content": null})) + &chunk(json!({"content": "Пари"}));
    let second = chunk(json!({"content": "ж"})) + &tail("stop");
    paced_reply(vec![
        (Duration::ZERO, first),
        (Duration::from_millis(500), second),
    ])
}

// ---- Тесты ----

fn paris(_: &[Value]) -> Response {
    text_reply(&["Пари", "ж"])
}

#[test]
fn run_answers_and_journals() {
    let srv = fake(paris);
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
fn refuse_then_ok(seen: &[Value]) -> Response {
    if seen.len() == 1 {
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
    let srv = fake(refuse_then_ok);
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
    let srv = fake(paced_paris);
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
fn empty_then_ok(seen: &[Value]) -> Response {
    if seen.len() == 1 {
        text_reply(&[])
    } else {
        text_reply(&["ок"])
    }
}

#[test]
fn resume_reads_budget_from_flags() {
    let srv = fake(empty_then_ok);
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
    let srv = fake(empty_then_ok);
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
    let srv = fake(paris);
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
fn ask_then_done(seen: &[Value]) -> Response {
    if seen.len() > 1 {
        return text_reply(&["готово"]);
    }
    call_reply("call_1", "ask_user", "{\"question\":\"Сколько?\"}")
}

#[test]
fn ask_user_round_trip() {
    let srv = fake(ask_then_done);
    let e = env("ask_user", &llama_config(&srv.base));
    let o = e.run_cmd(&["--tier", "standard", "Посчитай"]);
    assert_eq!(o.code(), Some(3), "{o:?}");
    assert!(o.stderr.contains("answer --interrupt input-"), "{o:?}");
    let int = interrupt_id(&o);
    let a = e.answer(&int, "42");
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
    let srv = fake(paris);
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
