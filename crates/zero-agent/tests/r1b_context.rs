//! Приёмка R1-B2 «Контекст и отказы»: длинный run бинарника до компакции и команды
//! `inspect`, `stats` из отдельного процесса.
//!
//! Поддельный llama-server (`support::fake_llama`) сообщает в `/props` окно 8192,
//! `/tokenize` отвечает 404. Основной вариант расход в стриме не сообщает
//! (`usage.input == 0`): оценка контекста — весь запрос байтами / 4 (D41). Вариант с
//! расходом (`spend_of`: вход по байтам тела, кэш — общее начало с прошлым запросом)
//! проверяет, что защита от обрезки D43 работает и не срабатывает ложно. Резерв вывода на окне 8192 — 3072 (D40),
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

use support::fake_llama::{
    ChatHandler, FIXED, Fake, Spend, call_reply_with, llama_config, spend_of, text_reply,
    text_reply_with,
};
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

/// Номер хода запроса `seen.last()` (с 1): запросы без компакции, подряд идущие
/// одинаковые тела (повтор стрима MDL-018 после обрыва соединения) — один ход.
fn turn_of(seen: &[Value]) -> usize {
    let mut n = 0;
    let mut prev: Option<&Value> = None;
    for b in seen.iter().filter(|b| !is_compaction(b)) {
        if prev != Some(b) {
            n += 1;
        }
        prev = Some(b);
    }
    n
}

/// Сводка на запрос компакции; иначе первые [`ASKS`] ходов — `ask_user`, затем «готово».
/// `spend` — расход, который сервер сообщает в стриме.
fn long_run_with(seen: &[Value], spend: fn(&[Value]) -> Spend) -> Response {
    let u = spend(seen);
    if seen.last().is_some_and(is_compaction) {
        return text_reply_with(
            &[
                "## Decisions\nkeep asking\n## Files changed\n-\n## Errors\n-\n## Next step\nask the user again",
            ],
            u,
        );
    }
    let turn = turn_of(seen);
    if turn <= ASKS {
        call_reply_with(
            &format!("call_{turn}"),
            "ask_user",
            "{\"question\":\"дальше?\"}",
            u,
        )
    } else {
        text_reply_with(&["готово"], u)
    }
}

/// Сервер без расхода: `usage` и `timings` вырезает [`Fake`].
fn long_run(seen: &[Value]) -> Response {
    long_run_with(seen, |_| FIXED)
}

/// Сервер с правдоподобным расходом ([`spend_of`]): вход по байтам тела, кэш — общее
/// начало с прошлым запросом.
fn long_run_usage(seen: &[Value]) -> Response {
    long_run_with(seen, spend_of)
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

/// Текст сообщения тела запроса (строка или текстовые части).
fn text_at(m: &Value) -> String {
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

/// Длинный run: 6 ответов по 2400 байт, затем «готово»; события run и тела запросов.
fn long_scenario(
    name: &str,
    handler: ChatHandler,
    usage: bool,
) -> (Vec<(EventKind, Value)>, Vec<Value>) {
    let srv = Fake::start(N_CTX, usage, handler);
    let e = env(name, &llama_config(&srv.base));
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
    (e.events(run), srv.bodies())
}

/// Общие утверждения длинного run: компакция была и применилась, резерв 3072, паузы
/// `context` и подозрения обрезки нет, run `done`.
fn assert_compacted_and_done(events: &[(EventKind, Value)], bodies: &[Value]) {
    let compacted: Vec<&Value> = events
        .iter()
        .filter(|(k, _)| *k == EventKind::ContextCompacted)
        .map(|(_, p)| p)
        .collect();
    assert!(!compacted.is_empty(), "компакции нет: {events:?}");
    // Новая эпоха: первая компакция — эпоха 1 (удалений в сценарии нет, D45).
    assert_eq!(compacted[0]["epoch"], 1, "{:?}", compacted[0]);
    assert_eq!(count(events, EventKind::ContextPruned), 0);
    // Компакция применилась: следующий запрос хода короче запроса компакции, первое
    // сообщение после system — `<env>` и сводка, последнее — напоминание задачи.
    let at = bodies
        .iter()
        .position(is_compaction)
        .expect("сервер не получил запроса компакции");
    let comp = bodies[at]["messages"].as_array().unwrap();
    let next = bodies[at + 1..]
        .iter()
        .find(|b| !is_compaction(b))
        .expect("после компакции нет хода");
    let msgs = next["messages"].as_array().unwrap();
    assert!(msgs.len() < comp.len(), "{} ≥ {}", msgs.len(), comp.len());
    let first = msgs
        .iter()
        .find(|m| m["role"] == "user")
        .map(text_at)
        .unwrap_or_default();
    assert!(first.starts_with("<env>"), "{first}");
    assert!(first.contains("<summary"), "{first}");
    assert!(
        first.contains("## Next step\nask the user again"),
        "{first}"
    );
    let last = msgs.last().map(text_at).unwrap_or_default();
    assert!(
        msgs.iter().any(|m| text_at(m).contains("Current task:")),
        "нет рецитации: {last}"
    );
    // Резерв вывода хода — 3072 (D40, `Budget(2048)` + 1024), как у тира small.
    for b in bodies.iter().filter(|b| !is_compaction(b)) {
        assert_eq!(b["max_tokens"], 3072, "{}", b["max_tokens"]);
    }
    assert_eq!(context_pauses(events), 0, "{events:?}");
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

#[test]
fn long_run_compacts_and_finishes() {
    let (events, bodies) = long_scenario("long_run", long_run, false);
    assert_compacted_and_done(&events, &bodies);
    // Расход не сообщён: у хода `usage.input == 0`.
    assert!(
        events
            .iter()
            .filter(|(k, _)| *k == EventKind::ModelUsage)
            .all(|(_, p)| p["usage"]["input"] == 0),
        "сервер без usage, а расход есть"
    );
}

/// Тот же run, сервер сообщает правдоподобный расход: проверка обрезки D43 идёт (у
/// ходов `usage.input > 0`, у большинства запросов оценка больше 1024) и не срабатывает
/// ложно.
#[test]
fn long_run_with_usage_has_no_false_truncation() {
    let (events, bodies) = long_scenario("long_run_usage", long_run_usage, true);
    assert_compacted_and_done(&events, &bodies);
    let inputs: Vec<u64> = events
        .iter()
        .filter(|(k, _)| *k == EventKind::ModelUsage)
        .filter_map(|(_, p)| p["usage"]["input"].as_u64())
        .collect();
    assert!(
        inputs.iter().all(|&n| n > 0) && inputs.iter().filter(|&&n| n > 1024).count() >= 3,
        "{inputs:?}"
    );
}

fn paris(_: &[Value]) -> Response {
    text_reply(&["Пари", "ж"])
}

/// Два хода (пустой ответ — nudge D18, затем текст) с правдоподобным расходом: у второго
/// запроса есть кэш.
fn empty_then_paris(seen: &[Value]) -> Response {
    if seen.len() == 1 {
        text_reply_with(&[], spend_of(seen))
    } else {
        text_reply_with(&["Пари", "ж"], spend_of(seen))
    }
}

/// Строка таблицы `inspect` блока `name`: имя, байты и токены числами.
fn has_row(table: &str, name: &str) -> bool {
    table.lines().any(|l| {
        let mut w = l.split_whitespace();
        w.next() == Some(name)
            && w.next().is_some_and(|b| b.parse::<u64>().is_ok())
            && w.next().is_some_and(|t| t.parse::<u64>().is_ok())
    })
}

fn stats_json(e: &Env, run: &str) -> Value {
    let s = e.run(&[
        "stats",
        "--project",
        e.project_arg(),
        "--run",
        run,
        "--json",
    ]);
    assert_eq!(s.code(), Some(0), "{s:?}");
    serde_json::from_str(&s.stdout).unwrap()
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
        assert!(has_row(&t.stdout, block), "нет строки блока {block}: {t:?}");
    }

    // Сервер без usage: входа нет, доля кэша — 0 (ветка `input == 0` метрик).
    let m = stats_json(&e, &run);
    assert!(m["turns"].as_u64().is_some_and(|n| n > 0), "{m}");
    assert_eq!(m["input"], 0, "{m}");
    assert_eq!(m["cache_share"], 0.0, "{m}");

    // Сервер с расходом и кэшем: доля кэша больше 0 и не больше 1.
    let srv = Fake::start(N_CTX, true, empty_then_paris);
    let e = env("tables_cache", &llama_config(&srv.base));
    let o = e.run_cmd(&["--tier", "small", "Столица Франции?"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let m = stats_json(&e, &run_id(&o).to_string());
    assert_eq!(m["turns"], 2, "{m}");
    let share = m["cache_share"].as_f64().unwrap();
    assert!(share > 0.0 && share <= 1.0, "{m}");
}

// ---- Живой llama-server (вручную) ----

/// `(cached, input)` запросов ходов без компакции по порядку, кроме первого запроса run
/// и первого запроса после удаления или компакции (новая эпоха — ожидаемый промах кэша).
fn steady_turns(events: &[(EventKind, Value)]) -> Vec<(u64, u64)> {
    let mut fresh = true;
    let mut steady: HashMap<String, bool> = HashMap::new();
    let mut out = Vec::new();
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
                    out.push((
                        p["usage"]["cached"].as_u64().unwrap_or(0),
                        p["usage"]["input"].as_u64().unwrap_or(0),
                    ));
                }
            }
            _ => {}
        }
    }
    out
}

/// Доля кэша `Σ cached / Σ input` и число ходов.
fn share(turns: &[(u64, u64)]) -> (f64, usize) {
    let cached: u64 = turns.iter().map(|t| t.0).sum();
    let input: u64 = turns.iter().map(|t| t.1).sum();
    let s = if input == 0 {
        0.0
    } else {
        cached as f64 / input as f64
    };
    (s, turns.len())
}

/// Ответ живого сценария: `bytes` байт ASCII, свой для раунда `k`.
fn live_answer(k: usize, bytes: usize) -> String {
    let line = format!("answer {k:02}: the quick brown fox jumps over the lazy dog\n");
    line.repeat(bytes.div_ceil(line.len()))[..bytes].to_owned()
}

/// Коротких ответов фазы замера NFR-06.
const LIVE_SHORT: usize = 6;
/// Длина длинного ответа фазы компакции, байт.
const LIVE_LONG_BYTES: usize = 16000;
/// Предел длинных раундов до компакции.
const LIVE_LONG_MAX: usize = 6;

/// R1-B2 задача 9, шаг 5: живой llama-server (Qwen, `n_ctx` 32768), три фазы.
///
/// Окно 32768 — тир standard: резерв `8192 + 1024 = 9216` (D40), компакция при оценке
/// ≥ 0,75 × 32768 − 9216 = 15360, удаление старых результатов — при ≥ 12083, лимит
/// выгрузки 0,15 × 32768 × 4 = 19660 байт.
///
/// 1. **Замер NFR-06 — короткие ответы** (`LIVE_SHORT` раундов по ≈ 20 байт). Префикс
///    запроса — system, описания инструментов и первое сообщение, P ≈ 2500–3500 токенов.
///    Новое на ходу — только прошлый ответ ассистента (шаблон Qwen выбрасывает
///    рассуждение прошлого хода, совпадение кэша обрывается на этом сообщении), результат
///    `ask_user` и разметка шаблона: N ≈ 50–150 токенов. Доля кэша хода
///    ≈ 1 − N / (P + история) ≥ 1 − 150 / 2500 = 0,94 > 0,90 — цель достижима на
///    корректной реализации; первый запрос run (холодный кэш) в замер не входит.
/// 2. **Компакция — длинные ответы** по 16000 байт (≈ 3600–4000 токенов, ниже лимита
///    выгрузки). Удаление D45 их не трогает: результаты фазы 1 короче 1024 байт, а длинные
///    — в последних 4 ходах. Оценка ≈ P + 600 + k × 3800 ≥ 15360 при k = 4 (k = 3 — на
///    границе), окно не переполняется: 3500 + 600 + 4 × 4000 + 9216 = 29316 < 32768.
///    Не больше `LIVE_LONG_MAX` раундов.
/// 3. **После компакции** — снова короткие ответы (3 раунда), доля печатается, затем
///    `stop` и ответ `done`.
///
/// Доля фазы 1 утверждается (≥ 0,90), фазы 3 — печатается. Цифры — в stdout
/// (`--nocapture`), их переносят в `docs/journal.md`. Предел процесса — 60 с ×
/// `ZA_TEST_TIME_SCALE`: для 27B с мышлением нужен масштаб 6.
#[test]
#[ignore = "живой llama-server: LLAMA_TEST_URL=http://127.0.0.1:8080 ZA_TEST_TIME_SCALE=6 cargo test -p zero-agent --test r1b_context -- --ignored --nocapture"]
fn live_llama_long_dialog() {
    let Ok(url) = std::env::var("LLAMA_TEST_URL") else {
        panic!(
            "LLAMA_TEST_URL не задан: запуск — LLAMA_TEST_URL=http://127.0.0.1:8080 \
             ZA_TEST_TIME_SCALE=6 cargo test -p zero-agent --test r1b_context -- --ignored --nocapture"
        );
    };
    let e = env("live", &llama_config(url.trim_end_matches('/')));
    let task = "Call the ask_user tool with the question \"next?\" and wait for the answer. \
        After each answer call ask_user again with the same question. When the answer is \
        exactly \"stop\", reply with the single word \"done\" and call no tools.";
    let mut o = e.run_cmd(&[task]);
    let run = run_id(&o);
    let id = run.to_string();
    let mut round = 0;
    let answer = |o: &mut support::process::Out, text: String, what: &str| {
        assert_eq!(
            o.code(),
            Some(3),
            "{what}: модель не позвала ask_user: {o:?}"
        );
        let a = e.answer(&interrupt_id(o), &text);
        assert_eq!(a.code(), Some(0), "{a:?}");
        *o = e.run_cmd(&["--resume", &id]);
    };

    // Фаза 1: замер NFR-06.
    for _ in 0..LIVE_SHORT {
        round += 1;
        answer(&mut o, format!("answer {round:02}: ok"), "фаза 1");
    }
    let phase1 = steady_turns(&e.events(run));
    let (s1, n1) = share(&phase1);
    println!("фаза 1: доля кэша {:.1} % на {n1} ходах", s1 * 100.0);

    // Фаза 2: длинные ответы до компакции.
    let mut long = 0;
    while count(&e.events(run), EventKind::ContextCompacted) == 0 && long < LIVE_LONG_MAX {
        long += 1;
        round += 1;
        answer(&mut o, live_answer(round, LIVE_LONG_BYTES), "фаза 2");
    }
    let compactions = count(&e.events(run), EventKind::ContextCompacted);
    println!("фаза 2: длинных ответов {long}, компакций {compactions}");
    assert!(compactions >= 1, "компакции нет за {long} длинных ответов");

    // Фаза 3: короткие ответы после компакции, затем `stop`.
    let before = steady_turns(&e.events(run)).len();
    for _ in 0..3 {
        round += 1;
        answer(&mut o, format!("answer {round:02}: ok"), "фаза 3");
    }
    let after = steady_turns(&e.events(run));
    let (s3, n3) = share(&after[before..]);
    println!("фаза 3: доля кэша {:.1} % на {n3} ходах", s3 * 100.0);
    answer(&mut o, "stop".to_owned(), "stop");
    assert_eq!(o.code(), Some(0), "run не закончился: {o:?}");

    let p = e.project_arg();
    let t = e.run(&["inspect", "--project", p, "--run", &id]);
    assert_eq!(t.code(), Some(0), "{t:?}");
    println!("inspect:\n{}", t.stdout);
    let s = e.run(&["stats", "--project", p, "--run", &id]);
    assert_eq!(s.code(), Some(0), "{s:?}");
    println!("stats:\n{}", s.stdout);
    assert!(n1 > 0, "фаза 1 без ходов с расходом");
    assert!(s1 >= 0.9, "NFR-06: доля кэша фазы 1 {s1:.3} < 0,90");
}
