//! Приёмка R1-B2 «Контекст и отказы»: длинный run бинарника до компакции и команды
//! `inspect`, `stats` из отдельного процесса.
//!
//! Поддельный llama-server (`support::fake_llama`) сообщает в `/props` окно 8192, расход
//! в стриме не сообщает (`usage.input == 0`), `/tokenize` отвечает 404: оценка
//! контекста — весь запрос байтами / 4 (D41). Резерв вывода на окне 8192 — 3072 (D40),
//! компакция при оценке ≥ 0,75 × 8192 − 3072 = 3072. Каждый процесс бинарника — под
//! пределом 60 с × `ZA_TEST_TIME_SCALE`; сигналов по таймеру и порогов времени нет.
//!
//! Длинный run — тир standard, а не small (бриф задачи 9): `ask_user` есть только с
//! тира standard (часть 02, таблица инструментов; R1-B1 D17), а другого встроенного
//! способа удлинять историю одного run у бинарника R1-B нет. Бюджет мышления standard
//! (`Effort(Medium)` = 8192) выше потолка `max(1024, 8192 / 4)`, ход идёт с
//! `Budget(2048)`, резерв `max(…, 2048 + 1024)` — тот же 3072, расчёт брифа не меняется.
//!
//! Шаг с живым llama-server (`live_llama_long_dialog`) помечен `#[ignore]` и берёт адрес
//! из `LLAMA_TEST_URL`:
//! `LLAMA_TEST_URL=http://127.0.0.1:8080 ZA_TEST_TIME_SCALE=6 cargo test -p zero-agent
//! --test r1b_context -- --ignored --nocapture`.

mod support;

use std::collections::HashMap;

use axum::response::Response;
use serde_json::Value;
use za_protocol::EventKind;

use support::fake_llama::{Fake, call_reply, llama_config, text_reply};
use support::process::{Env, count, env_in, interrupt_id, run_id};

/// Окно `/props` сценариев.
const N_CTX: u32 = 8192;

/// Ходов с вызовом `ask_user` до ответа «готово».
const ASKS: usize = 6;

/// Длина ответа пользователя, байт ASCII: ниже лимита выгрузки 0,15 × 8192 × 4 = 4915.
const ANSWER_BYTES: usize = 2400;

fn env(name: &str, config: &str) -> Env {
    env_in("r1b-context", name, config)
}

/// Текст последнего сообщения тела запроса (строка или текстовые части).
fn last_text(body: &Value) -> String {
    let Some(content) = body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .map(|m| &m["content"])
    else {
        return String::new();
    };
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Запрос компакции: последнее сообщение — рубрика `action="compact"` (D46).
fn is_compaction(body: &Value) -> bool {
    last_text(body).contains("action=\"compact\"")
}

/// Сводка на запрос компакции; иначе первые [`ASKS`] ходов — `ask_user`, затем «готово».
fn long_run(seen: &[Value]) -> Response {
    if seen.last().is_some_and(is_compaction) {
        return text_reply(&[
            "## Decisions\nkeep asking\n## Files changed\n-\n## Errors\n-\n## Next step\nask the user again",
        ]);
    }
    let turn = seen.iter().filter(|b| !is_compaction(b)).count();
    if turn <= ASKS {
        call_reply(
            &format!("call_{turn}"),
            "ask_user",
            "{\"question\":\"дальше?\"}",
        )
    } else {
        text_reply(&["готово"])
    }
}

/// Ответ пользователя номер `k`: 2400 байт ASCII, свой для каждого `k`.
fn answer_text(k: usize) -> String {
    let line = format!("answer {k:02}: the quick brown fox jumps over the lazy dog\n");
    line.repeat(ANSWER_BYTES.div_ceil(line.len()))[..ANSWER_BYTES].to_owned()
}

/// Паузы с причиной `context` в журнале.
fn context_pauses(events: &[(EventKind, Value)]) -> usize {
    events
        .iter()
        .filter(|(k, p)| *k == EventKind::RunStatus && p["reason"] == "context")
        .count()
}

#[test]
fn long_run_compacts_and_finishes() {
    let srv = Fake::start(N_CTX, false, long_run);
    let e = env("long_run", &llama_config(&srv.base));
    let mut o = e.run_cmd(&["--tier", "standard", "Ask the user until they are done"]);
    assert_eq!(o.code(), Some(3), "{o:?}");
    let run = run_id(&o);
    let id = run.to_string();
    for k in 1..=ASKS {
        let int = interrupt_id(&o);
        let a = e.answer(&int, &answer_text(k));
        assert_eq!(a.code(), Some(0), "ответ {k}: {a:?}");
        o = e.run_cmd(&["--resume", &id]);
        let want = if k < ASKS { 3 } else { 0 };
        assert_eq!(o.code(), Some(want), "resume после ответа {k}: {o:?}");
    }
    assert_eq!(o.stdout, "готово\n", "{o:?}");

    let events = e.events(run);
    assert!(
        count(&events, EventKind::ContextCompacted) >= 1,
        "компакции нет: {events:?}"
    );
    let bodies = srv.bodies();
    assert!(
        bodies.iter().any(is_compaction),
        "сервер не получил запроса компакции"
    );
    // Резерв вывода хода — 3072 (D40, `Budget(2048)` + 1024), как у тира small.
    for b in bodies.iter().filter(|b| !is_compaction(b)) {
        assert_eq!(b["max_tokens"], 3072, "{}", b["max_tokens"]);
    }
    // Расход не сообщён: на `/tokenize` 404 и без usage — подозрения обрезки нет (D43).
    assert_eq!(context_pauses(&events), 0, "{events:?}");
    let alerts: Vec<&Value> = events
        .iter()
        .filter(|(k, _)| *k == EventKind::ContextAlert)
        .map(|(_, p)| p)
        .collect();
    assert!(
        alerts.iter().all(|p| p["kind"] != "truncation_suspected"),
        "{alerts:?}"
    );
    let last = events
        .iter()
        .rev()
        .find(|(k, _)| *k == EventKind::RunStatus)
        .unwrap();
    assert_eq!(last.1["to"], "done", "{last:?}");
}

fn paris(_: &[Value]) -> Response {
    text_reply(&["Пари", "ж"])
}

#[test]
fn inspect_and_stats_print_tables() {
    let srv = Fake::start(N_CTX, false, paris);
    let e = env("tables", &llama_config(&srv.base));
    let o = e.run_cmd(&["--tier", "small", "Столица Франции?"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let run = run_id(&o).to_string();
    let p = e.project_arg();

    let t = e.run(&["inspect", "--project", p, "--run", &run]);
    assert_eq!(t.code(), Some(0), "{t:?}");
    for block in ["tools", "core", "first_message", "dialog"] {
        assert!(t.stdout.contains(block), "нет блока {block}: {t:?}");
    }

    let s = e.run(&["stats", "--project", p, "--run", &run, "--json"]);
    assert_eq!(s.code(), Some(0), "{s:?}");
    let m: Value = serde_json::from_str(&s.stdout).unwrap();
    assert!(m["turns"].as_u64().is_some_and(|n| n > 0), "{m}");
    assert!(m["cache_share"].as_f64().is_some_and(|x| x >= 0.0), "{m}");
}

// ---- Живой llama-server (вручную) ----

/// Доля кэша на ходах без компакции (NFR-06): `cached / input` запросов ходов, кроме
/// первого запроса run и первого запроса после удаления или компакции (новая эпоха —
/// ожидаемый промах кэша).
fn steady_cache_share(events: &[(EventKind, Value)]) -> (u64, u64) {
    let mut fresh = true;
    let mut steady: HashMap<String, bool> = HashMap::new();
    let (mut cached, mut input) = (0u64, 0u64);
    for (k, p) in events {
        match k {
            EventKind::ContextCompacted | EventKind::ContextPruned => fresh = true,
            EventKind::ModelRequest if p["kind"] != "compact" => {
                if let Some(key) = p["effect_key"].as_str() {
                    steady.insert(key.to_owned(), !fresh);
                }
                fresh = false;
            }
            EventKind::ModelUsage => {
                let key = p["effect_key"].as_str().unwrap_or_default();
                if steady.get(key).copied().unwrap_or(false) {
                    cached += p["usage"]["cached"].as_u64().unwrap_or(0);
                    input += p["usage"]["input"].as_u64().unwrap_or(0);
                }
            }
            _ => {}
        }
    }
    (cached, input)
}

/// R1-B2 задача 9, шаг 5: длинный диалог на живом llama-server до компакции, `inspect`,
/// `stats` и доля кэша (цель NFR-06 ≥ 90 %). Модель просят звать `ask_user`, пока
/// пользователь не ответит `stop`; ответы — по 12000 байт (ниже лимита выгрузки окна
/// 32768). Цифры — в stdout (`--nocapture`), их переносят в `docs/journal.md`.
#[test]
#[ignore = "нужен живой llama-server: LLAMA_TEST_URL=http://127.0.0.1:8080"]
fn live_llama_long_dialog() {
    let url = std::env::var("LLAMA_TEST_URL").expect("LLAMA_TEST_URL не задан");
    let e = env("live", &llama_config(url.trim_end_matches('/')));
    let task = "Call the ask_user tool with the question \"next?\" and wait for the answer. \
        After each answer call ask_user again with the same question. When the answer is \
        exactly \"stop\", reply with the single word \"done\" and call no tools.";
    let mut o = e.run_cmd(&[task]);
    let run = run_id(&o);
    let id = run.to_string();
    let mut rounds = 0;
    while o.code() == Some(3) && rounds < 24 {
        rounds += 1;
        let compacted = count(&e.events(run), EventKind::ContextCompacted) > 0;
        let text = if compacted && rounds > 2 {
            "stop".to_owned()
        } else {
            let line = format!("answer {rounds:02}: the quick brown fox jumps over the lazy dog\n");
            line.repeat(12000usize.div_ceil(line.len()))[..12000].to_owned()
        };
        let a = e.answer(&interrupt_id(&o), &text);
        assert_eq!(a.code(), Some(0), "{a:?}");
        o = e.run_cmd(&["--resume", &id]);
    }
    println!("раундов ask_user: {rounds}; итог run: {:?}", o.code());
    assert_eq!(o.code(), Some(0), "run не закончился: {o:?}");
    let events = e.events(run);
    let compactions = count(&events, EventKind::ContextCompacted);
    println!("компакций: {compactions}");
    assert!(compactions >= 1, "компакции нет за {rounds} раундов");

    let p = e.project_arg();
    let t = e.run(&["inspect", "--project", p, "--run", &id]);
    assert_eq!(t.code(), Some(0), "{t:?}");
    println!("inspect:\n{}", t.stdout);
    let s = e.run(&["stats", "--project", p, "--run", &id]);
    assert_eq!(s.code(), Some(0), "{s:?}");
    println!("stats:\n{}", s.stdout);
    let (cached, input) = steady_cache_share(&events);
    assert!(input > 0, "нет ходов без компакции с расходом");
    let share = cached as f64 / input as f64;
    println!(
        "доля кэша на ходах без компакции: {:.1} % ({cached}/{input})",
        share * 100.0
    );
    assert!(share >= 0.9, "NFR-06: доля кэша {share:.3} < 0,90");
}
