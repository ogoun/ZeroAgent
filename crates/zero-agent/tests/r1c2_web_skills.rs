//! Приёмка R1-C2 «Скиллы и веб» (задача 15): сценарии бинарника `zero-agent` с настоящими
//! шлюзом `za-web`, извлекателем в подпроцессе (`extract html`) и каталогом скиллов поверх
//! поддельного llama-server (`support::fake_llama`).
//!
//! - `s1_quick_answer_with_links` — S1: `web_search` у поддельного SearXNG, два `web_fetch`
//!   по ссылкам выдачи, ответ с обеими ссылками; журнал `web.request`, метка `web`,
//!   `robots.txt` у сайтов и не у поставщика, HTML разобран подпроцессом;
//! - `s1_local_address_refused_without_connection`, `searxng_address_not_reachable_by_web_fetch`
//!   — отказ до соединения (D108, D126): у поддельного сервера ни одного запроса;
//! - `web_does_not_make_commands_ask` — чтение из сети не влияет на запуск команд (D101);
//! - `rule_of_two_logged_without_pause` — Rule of Two без вопросов (D106): журнал, аудит,
//!   строка stderr один раз; недоверенный контекст от проекта и от самой страницы;
//!   `rule_of_two_two_fetches_in_ask_mode` — два внешних вызова в режиме `ask`: две записи
//!   аудита, одна строка (m1 ревью задачи 15);
//! - `read_pdf_through_binary` — `read` PDF через подпроцесс `extract pdf` (таблица 5.2);
//! - `skill_activation_end_to_end`, `slash_name_loads_skill`, `router_card_on_small` —
//!   скиллы проекта: `activate_skill`, `/имя`, карточка роутера на small (D118, D119);
//! - `resume_after_crash_in_web_fetch_repeats_read_only`,
//!   `resume_after_receipt_does_not_touch_network` — resume веба (Review Focus 5, D112);
//! - `resume_keeps_frozen_web_tools` — resume run с веб-инструментами (m6 ревью задачи 14),
//!   `policy.loaded` на resume (I1 ревью задачи 14);
//! - `disabled_web_tool_explains_after_resume` — веб или поиск выключили во время паузы:
//!   вызов из замороженного префикса отвечает понятным текстом (Ruling «Задача 15»);
//! - `secret_in_fetch_url_masked` — значение `ZA_SECRET_*` в URL не попадает на диск;
//! - `proxy_env_is_ignored` — переменные прокси процесса не действуют (D107, D121).
//!
//! Настоящий интернет не используется: сайты, SearXNG и прокси — `axum` на `127.0.0.1` в
//! процессе теста, имена `docs.test` и `blog.test` шлюз разрешает через `--web-resolve`
//! (признак `test-hooks`, D128); SearXNG — по `[web.search] url` глобальной конфигурации
//! `ZA_HOME` теста (исключение D126). Тесты — обычные `#[test]`: помощники `support` сами
//! держат runtime tokio. Предел каждого процесса — 60 с × `ZA_TEST_TIME_SCALE`.
//!
//! Живой шаг (`live_llama_s1`) помечен `#[ignore]` и берёт адреса из `LLAMA_TEST_URL` и
//! `SEARXNG_TEST_URL`; окно llama-server — от 32768 токенов (`-c 32768`), иначе тир small и
//! веб-инструментов нет:
//! `LLAMA_TEST_URL=http://127.0.0.1:8080 SEARXNG_TEST_URL=http://127.0.0.1:8888
//! ZA_TEST_TIME_SCALE=6 cargo test -p zero-agent --features test-hooks --test r1c2_web_skills
//! -- --ignored --nocapture`.

mod support;

#[path = "../../../test-support/pdf.rs"]
mod pdf;

use std::path::{Path, PathBuf};

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use za_protocol::{EffectKey, EventKind, RunId};

use support::fake_llama::{Fake, call_reply, llama_config, text_reply};
use support::fake_web::{FakeHttp, proxy, reply, searx_results};
use support::process::{Env, Out, count, env_in, interrupt_id, run_id, unique_dir_in};

/// Каталог тестов в `CARGO_TARGET_TMPDIR`.
const GROUP: &str = "r1c2-web-skills";
/// Окно модели: тир standard по окну (веб и `activate_skill` — от standard).
const N_CTX: u32 = 32_768;
/// Окно tier small (роутер карточек, D119).
const N_CTX_SMALL: u32 = 16_384;
/// Первая строка результата веба (D110): одно происхождение, без `network` и `tool`.
const WEB_MARK: &str = "[untrusted content: web]";
/// Отказ фильтра адресов (D108).
const BLOCKED_LOCAL: &str = "blocked: local or private address";
/// Строка Rule of Two в stderr (D106, SEC-052).
const RULE_OF_TWO_LINE: &str = "zero-agent: в этом запуске сошлись недоверенное содержимое, ваши файлы вне проекта и обращение в сеть (SEC-052); вызов выполнен, запись в аудите";

// ---- Помощники ----

fn repo_root() -> PathBuf {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

/// Путь без `\\?\` (как его печатает и принимает бинарник).
fn shown(p: &Path) -> PathBuf {
    PathBuf::from(zero_agent::run::display_path(p))
}

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// Конфигурация: модель по адресу `llama`, порты сайтов в `[web] ports` (по умолчанию
/// только 80/443/8080/8443) и, если задан, адрес SearXNG в `[web.search] url`.
fn web_cfg(llama: &str, ports: &[u16], search: Option<&str>) -> String {
    let ports: Vec<String> = ports.iter().map(u16::to_string).collect();
    let mut cfg = format!(
        "{}\n[web]\nports = [{}]\n",
        llama_config(llama),
        ports.join(", ")
    );
    if let Some(url) = search {
        cfg.push_str(&format!("\n[web.search]\nurl = \"{url}\"\n"));
    }
    cfg
}

/// Аргументы `run --project P --web-resolve host=127.0.0.1:port …` и `extra`.
fn run_args<'a>(e: &'a Env, resolve: &'a [String], extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["run", "--project", e.project_arg()];
    for r in resolve {
        args.push("--web-resolve");
        args.push(r);
    }
    args.extend_from_slice(extra);
    args
}

/// Строки `--web-resolve` для имён поддельных сайтов.
fn resolve(names: &[(&str, u16)]) -> Vec<String> {
    names
        .iter()
        .map(|(h, p)| format!("{h}=127.0.0.1:{p}"))
        .collect()
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

/// Число результатов инструментов в запросе: модель тестов решает по нему, поэтому реплей
/// после сбоя или паузы её не путает.
fn tool_results(body: &Value) -> usize {
    messages(body)
        .iter()
        .filter(|m| m["role"] == "tool")
        .count()
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

/// Текст всех сообщений пользователя запроса.
fn user_text(body: &Value) -> String {
    messages(body)
        .iter()
        .filter(|m| m["role"] == "user")
        .map(content)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Значение `<key>=|…|` из задачи run (сообщения пользователя запроса).
fn task_arg(body: &Value, key: &str) -> String {
    let text = user_text(body);
    let start = format!("{key}=|");
    let at = text
        .find(&start)
        .unwrap_or_else(|| panic!("нет {start} в задаче: {text}"))
        + start.len();
    let end = at + text[at..].find('|').expect("нет конца значения");
    text[at..end].to_owned()
}

/// Первая ссылка с началом `prefix` в результатах инструментов запроса (выдача поиска).
fn link_in(body: &Value, prefix: &str) -> String {
    messages(body)
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(content)
        .find_map(|t| {
            t.find(prefix).map(|at| {
                t[at..]
                    .split(|c: char| c.is_whitespace() || c == '"')
                    .next()
                    .unwrap()
                    .to_owned()
            })
        })
        .unwrap_or_else(|| panic!("нет ссылки {prefix} в результатах"))
}

/// Имена инструментов запроса.
fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// События вида `kind` с полем `call_id == id`.
fn of_call(events: &[(EventKind, Value)], kind: EventKind, id: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|(k, p)| *k == kind && p["call_id"] == id)
        .map(|(_, p)| p.clone())
        .collect()
}

/// Payload событий вида `kind`.
fn payloads(events: &[(EventKind, Value)], kind: EventKind) -> Vec<Value> {
    events
        .iter()
        .filter(|(k, _)| *k == kind)
        .map(|(_, p)| p.clone())
        .collect()
}

/// Все записи аудита проекта по порядку.
fn all_audit(e: &Env) -> Vec<za_store::AuditEntry> {
    let store = e.store();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(store.audit_entries(0, usize::MAX)).unwrap()
}

/// Записанный результат (квитанция) эффекта вызова `id`: ключ — из `tool.started`.
fn receipt_of(e: &Env, run: RunId, id: &str) -> Value {
    let events = e.events(run);
    let started = of_call(&events, EventKind::ToolStarted, id);
    let key = started[0]["effect_key"]
        .as_str()
        .unwrap_or_else(|| panic!("нет effect_key у {id}: {}", started[0]))
        .to_owned();
    let store = e.store();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(store.effect(run, &EffectKey(key)))
        .unwrap()
        .unwrap_or_else(|| panic!("нет эффекта вызова {id}"))
        .result
        .unwrap_or_else(|| panic!("нет квитанции вызова {id}"))
}

/// Код выхода `std::process::abort()` (`--crash-at`): Windows — `__fastfail`
/// (STATUS_STACK_BUFFER_OVERRUN), Unix — сигнал SIGABRT, кода нет.
fn assert_aborted(o: &Out) {
    if cfg!(windows) {
        assert_eq!(o.code(), Some(0xC000_0409_u32 as i32), "{o:?}");
    } else {
        assert_eq!(o.code(), None, "{o:?}");
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
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

// ---- Поддельные сайты ----

/// Страница документации: заголовок, абзац и скрипт (скрипт в Markdown не попадает).
const DOCS_HTML: &str = "<html><head><title>Spawning | Tokio</title></head><body><h1>Spawning</h1><p>Use tokio::spawn to run a task in the background.</p><script>evil()</script></body></html>";
/// Запись блога.
const BLOG_HTML: &str = "<html><head><title>Notes on spawn</title></head><body><h2>Why spawn</h2><p>A spawned task needs a 'static future.</p></body></html>";

/// `docs.test`: `robots.txt` разрешает всё, `/spawn` — документация, `/x?…` и `/r` —
/// страница и редирект для теста секрета в URL.
fn docs_site(target: &str) -> Response {
    match target {
        "/robots.txt" => reply(StatusCode::OK, "text/plain", "User-agent: *\nAllow: /\n"),
        "/spawn" => reply(StatusCode::OK, "text/html; charset=utf-8", DOCS_HTML),
        t if t.starts_with("/x?") => reply(
            StatusCode::OK,
            "text/html; charset=utf-8",
            "<html><body><p>Parameter page.</p></body></html>",
        ),
        // Редирект того же хоста с секретом в запросе: адрес с секретом со стороны сети.
        "/r" => (
            StatusCode::FOUND,
            [(header::LOCATION, format!("/x?k={}", secret_value()))],
            "",
        )
            .into_response(),
        _ => reply(StatusCode::NOT_FOUND, "text/plain", "no"),
    }
}

/// `blog.test`: `robots.txt` нет (404 — всё разрешено), `/a` — запись.
fn blog_site(target: &str) -> Response {
    match target {
        "/a" => reply(StatusCode::OK, "text/html; charset=utf-8", BLOG_HTML),
        _ => reply(StatusCode::NOT_FOUND, "text/plain", "no"),
    }
}

/// Поддельный SearXNG: `/search?…` — выдача со ссылками `hits` (заголовок, адрес).
fn searx(hits: Vec<(String, String)>) -> FakeHttp {
    FakeHttp::start(move |target: &str| {
        if target.starts_with("/search?") {
            let rows: Vec<(&str, &str, &str)> = hits
                .iter()
                .map(|(t, u)| (t.as_str(), u.as_str(), "About tokio::spawn"))
                .collect();
            searx_results(&rows)
        } else {
            reply(StatusCode::NOT_FOUND, "text/plain", "no")
        }
    })
}

fn answer_only(_: &[Value]) -> Response {
    text_reply(&["готово"])
}

/// Модель: `web_fetch` адреса `URL=|…|` из задачи, затем ответ.
fn fetch_then_answer(seen: &[Value]) -> Response {
    let cur = seen.last().unwrap();
    if tool_results(cur) > 0 {
        return text_reply(&["готово"]);
    }
    let url = task_arg(cur, "URL");
    call_reply("f1", "web_fetch", &json!({ "url": url }).to_string())
}

// ---- S1 ----

/// Модель S1: поиск, страница документации и запись блога по ссылкам выдачи, ответ с обеими
/// ссылками.
fn s1_model(seen: &[Value]) -> Response {
    let cur = seen.last().unwrap();
    match tool_results(cur) {
        0 => call_reply("s1", "web_search", r#"{"query":"tokio spawn"}"#),
        1 => call_reply(
            "f1",
            "web_fetch",
            &json!({ "url": link_in(cur, "http://docs.test:") }).to_string(),
        ),
        2 => call_reply(
            "f2",
            "web_fetch",
            &json!({ "url": link_in(cur, "http://blog.test:") }).to_string(),
        ),
        _ => {
            let answer = format!(
                "Use tokio::spawn. Sources: {} and {}",
                link_in(cur, "http://docs.test:"),
                link_in(cur, "http://blog.test:")
            );
            text_reply(&[answer.as_str()])
        }
    }
}

/// S1 (часть 00): быстрый ответ со ссылками. Поиск у своего экземпляра SearXNG — один
/// запрос `format=json` без `robots.txt`; у каждого сайта — `robots.txt` и страница; три
/// вызова, у всех `tainted` и первая строка ровно `[untrusted content: web]`; HTML разобран
/// подпроцессом `extract html` (`za-web` HTML не разбирает, D127): Markdown со заголовком,
/// без скрипта, рабочий каталог подпроцесса создан и после run убран.
#[test]
fn s1_quick_answer_with_links() {
    let docs = FakeHttp::start(docs_site);
    let blog = FakeHttp::start(blog_site);
    let docs_url = format!("http://docs.test:{}/spawn", docs.port);
    let blog_url = format!("http://blog.test:{}/a", blog.port);
    let sx = searx(vec![
        ("Spawning | Tokio".into(), docs_url.clone()),
        ("Notes on spawn".into(), blog_url.clone()),
    ]);
    let srv = Fake::start(N_CTX, true, s1_model);
    let e = env_in(
        GROUP,
        "s1",
        &web_cfg(&srv.base, &[docs.port, blog.port], Some(&sx.base)),
    );
    let names = resolve(&[("docs.test", docs.port), ("blog.test", blog.port)]);
    let o = e.run(&run_args(
        &e,
        &names,
        &["Как запустить задачу в фоне в tokio? Ответь коротко со ссылками."],
    ));
    assert_eq!(o.code(), Some(0), "{o:?}");
    let run = run_id(&o);
    assert!(o.stdout.contains(&docs_url), "{o:?}");
    assert!(o.stdout.contains(&blog_url), "{o:?}");

    let events = e.events(run);
    let started: Vec<String> = payloads(&events, EventKind::ToolStarted)
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        started,
        ["web_search", "web_fetch", "web_fetch"],
        "{events:?}"
    );
    let bodies = srv.bodies();
    for id in ["s1", "f1", "f2"] {
        let fin = of_call(&events, EventKind::ToolFinished, id);
        assert_eq!(fin.len(), 1, "{id}: {events:?}");
        assert_eq!(fin[0]["tainted"], true, "{id}: {}", fin[0]);
        assert_eq!(fin[0]["is_error"], false, "{id}: {}", fin[0]);
        let r = result_of(&bodies, id);
        assert_eq!(r.lines().next(), Some(WEB_MARK), "{id}: {r}");
    }
    // Поиск, два robots.txt, две страницы.
    let reqs = payloads(&events, EventKind::WebRequest);
    assert_eq!(reqs.len(), 5, "{reqs:?}");
    assert!(reqs.iter().all(|r| r["outcome"] == "ok"), "{reqs:?}");
    let probes = sx.requests();
    assert_eq!(probes.len(), 1, "{probes:?}");
    assert!(
        probes[0].starts_with("/search?q=tokio") && probes[0].ends_with("&format=json"),
        "{probes:?}"
    );
    assert_eq!(docs.requests(), ["/robots.txt", "/spawn"]);
    assert_eq!(blog.requests(), ["/robots.txt", "/a"]);
    for url in [&docs_url, &blog_url] {
        assert!(
            o.stderr
                .contains(&format!("zero-agent: веб: GET {url} → 200")),
            "{o:?}"
        );
    }

    // HTML → Markdown в подпроцессе.
    let page = result_of(&bodies, "f1");
    assert!(page.contains(&format!("url: {docs_url}")), "{page}");
    assert!(page.contains("title: Spawning | Tokio"), "{page}");
    assert!(page.contains("# Spawning"), "{page}");
    assert!(page.contains("Use tokio::spawn to run a task"), "{page}");
    assert!(!page.contains("evil()"), "{page}");
    assert!(result_of(&bodies, "f2").contains("## Why spawn"));
    let tmp = e.project.join(".zero-agent/work/tmp");
    assert!(tmp.is_dir(), "рабочий каталог извлекателя не создан");
    assert!(!tmp.join(run.to_string()).exists(), "каталог run не убран");
}

/// Локальный адрес (D108): отказ до соединения — у сайта ни одного запроса, строка
/// `web.request` с `outcome = "blocked"`, строка «отказ» в stderr.
#[test]
fn s1_local_address_refused_without_connection() {
    let site = FakeHttp::start(docs_site);
    let srv = Fake::start(N_CTX, true, fetch_then_answer);
    let e = env_in(GROUP, "local", &web_cfg(&srv.base, &[site.port], None));
    let url = format!("http://127.0.0.1:{}/spawn", site.port);
    let o = e.run(&run_args(&e, &[], &[&format!("Открой URL=|{url}|")]));
    assert_eq!(o.code(), Some(0), "{o:?}");
    let r = result_of(&srv.bodies(), "f1");
    assert!(r.contains(BLOCKED_LOCAL), "{r}");
    assert!(site.requests().is_empty(), "{:?}", site.requests());
    let reqs = payloads(&e.events(run_id(&o)), EventKind::WebRequest);
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert_eq!(reqs[0]["outcome"], "blocked", "{}", reqs[0]);
    assert_eq!(reqs[0]["reason"], "local or private address", "{}", reqs[0]);
    assert!(
        o.stderr
            .contains("zero-agent: веб: отказ http://127.0.0.1:"),
        "{o:?}"
    );
}

/// Адрес своего экземпляра SearXNG (D126): исключение фильтра — только у `web_search`;
/// `web_fetch` на тот же адрес — отказ, у SearXNG ни одного запроса.
#[test]
fn searxng_address_not_reachable_by_web_fetch() {
    let sx = searx(vec![("x".into(), "https://example.org/".into())]);
    let srv = Fake::start(N_CTX, true, fetch_then_answer);
    let e = env_in(
        GROUP,
        "searx-fetch",
        &web_cfg(&srv.base, &[sx.port], Some(&sx.base)),
    );
    let url = format!("{}/search?q=x&format=json", sx.base);
    let o = e.run(&run_args(&e, &[], &[&format!("Открой URL=|{url}|")]));
    assert_eq!(o.code(), Some(0), "{o:?}");
    let bodies = srv.bodies();
    assert!(
        tool_names(&bodies[0]).iter().any(|n| n == "web_search"),
        "поиск не настроен"
    );
    let r = result_of(&bodies, "f1");
    assert!(r.contains(BLOCKED_LOCAL), "{r}");
    assert!(sx.requests().is_empty(), "{:?}", sx.requests());
    let reqs = payloads(&e.events(run_id(&o)), EventKind::WebRequest);
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert_eq!(reqs[0]["outcome"], "blocked", "{}", reqs[0]);
}

// ---- Чтение из сети и команды ----

/// Модель: страница, затем команда, затем ответ.
fn fetch_then_command(seen: &[Value]) -> Response {
    let cur = seen.last().unwrap();
    match tool_results(cur) {
        0 => call_reply(
            "f1",
            "web_fetch",
            &json!({ "url": task_arg(cur, "URL") }).to_string(),
        ),
        1 => call_reply("b1", "bash", r#"{"cmd":"cargo --version"}"#),
        _ => text_reply(&["готово"]),
    }
}

/// Решение заказчика (D101): метка `web` — только журнал, аудит и первая строка
/// результата; команда после чтения страницы в `auto` доверенного проекта идёт без вопроса.
/// Проект доверен заранее: иначе `bash` дал бы прерывание доверия (D83).
#[test]
fn web_does_not_make_commands_ask() {
    let site = FakeHttp::start(docs_site);
    let srv = Fake::start(N_CTX, true, fetch_then_command);
    let e = env_in(
        GROUP,
        "web-then-bash",
        &web_cfg(&srv.base, &[site.port], None),
    );
    e.trust();
    let url = format!("http://docs.test:{}/spawn", site.port);
    let names = resolve(&[("docs.test", site.port)]);
    let o = e.run(&run_args(
        &e,
        &names,
        &[
            "--mode",
            "auto",
            &format!("Прочитай URL=|{url}| и проверь cargo"),
        ],
    ));
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert_eq!(o.stdout, "готово\n", "{o:?}");
    let run = run_id(&o);
    let events = e.events(run);
    assert_eq!(count(&events, EventKind::InterruptOpened), 0, "{events:?}");
    assert_eq!(
        of_call(&events, EventKind::ToolFinished, "f1")[0]["tainted"],
        true
    );
    assert_eq!(
        of_call(&events, EventKind::ToolFinished, "b1").len(),
        1,
        "{events:?}"
    );
    assert!(result_of(&srv.bodies(), "b1").contains("cargo"));
    let snap = e.snapshot(run);
    assert!(
        snap["taint"].as_array().unwrap().contains(&json!("web")),
        "{snap}"
    );
    // Чужих файлов нет — трёх свойств нет.
    assert!(e.audit("rule_of_two").is_empty());
    assert!(!o.stderr.contains(RULE_OF_TWO_LINE), "{o:?}");
}

// ---- Rule of Two (D106) ----

/// Модель: чтение внешнего файла по гранту, затем страница, затем ответ.
fn read_then_fetch(seen: &[Value]) -> Response {
    let cur = seen.last().unwrap();
    match tool_results(cur) {
        0 => call_reply(
            "r1",
            "read",
            &json!({ "path": format!("{}/a.txt", task_arg(cur, "EXT")) }).to_string(),
        ),
        1 => call_reply(
            "w1",
            "web_fetch",
            &json!({ "url": task_arg(cur, "URL") }).to_string(),
        ),
        _ => text_reply(&["готово"]),
    }
}

/// Решение заказчика 2026-11-11 «не спрашивать»: недоверенное содержимое, файл вне
/// проекта по гранту `ro` и обращение в сеть — run завершается кодом 0 без паузы;
/// `exposure.changed` со всеми тремя флагами, запись аудита `rule_of_two` у `web_fetch`,
/// строка SEC-052 в stderr ровно один раз, причины `rule_of_two` в одобрениях нет.
///
/// Два источника недоверенного содержимого: недоверенный проект — свойство есть с первого
/// шага (после чтения файла снимок уже со всеми флагами, кроме `external`); доверенный
/// проект — свойство даёт сам `web_fetch` (его метка известна до вызова: `spec.tainted`,
/// `ToolJob::exposure_before`), после чтения файла снимок — только `private`. В обоих
/// случаях запись аудита — до вызова, отметка `rule_of_two` в `tool.started` (Ruling I1, m2
/// ревью задачи 5). Внешний каталог — `.tmp/r1c2-acceptance/<тест>/ext` (D121).
#[test]
fn rule_of_two_logged_without_pause() {
    let site = FakeHttp::start(docs_site);
    let srv = Fake::start(N_CTX, true, read_then_fetch);
    let url = format!("http://docs.test:{}/spawn", site.port);
    let names = resolve(&[("docs.test", site.port)]);
    for (name, trusted) in [
        ("rule-of-two-untrusted", false),
        ("rule-of-two-trusted", true),
    ] {
        let e = env_in(GROUP, name, &web_cfg(&srv.base, &[site.port], None));
        if trusted {
            e.trust();
        }
        let ext_root = unique_dir_in(&repo_root().join(".tmp/r1c2-acceptance"), name);
        let ext = shown(&std::fs::canonicalize(&ext_root).unwrap()).join("ext");
        write(&ext.join("a.txt"), "private note\n");
        let ext_arg = ext.to_string_lossy().replace('\\', "/");
        let grant = format!("{}=ro", ext.display());
        let task = format!("Сверь EXT=|{ext_arg}| с URL=|{url}|");
        let o = e.run(&run_args(
            &e,
            &names,
            &["--mode", "auto", "--grant", &grant, &task],
        ));
        assert_eq!(o.code(), Some(0), "{name}: {o:?}");
        assert_eq!(o.stdout, "готово\n", "{name}: {o:?}");
        let run = run_id(&o);
        let events = e.events(run);
        assert_eq!(
            count(&events, EventKind::InterruptOpened),
            0,
            "{name}: {events:?}"
        );
        assert!(
            result_of(&srv.bodies(), "r1").contains("private note"),
            "{name}"
        );

        let changed = payloads(&events, EventKind::ExposureChanged);
        let after_read = changed
            .iter()
            .find(|c| c["calls"] == json!(["r1"]))
            .unwrap_or_else(|| panic!("{name}: нет exposure.changed чтения: {changed:?}"));
        assert_eq!(after_read["private"], true, "{name}: {after_read}");
        assert_eq!(after_read["external"], false, "{name}: {after_read}");
        assert_eq!(after_read["untrusted"], !trusted, "{name}: {after_read}");
        let last = changed
            .last()
            .unwrap_or_else(|| panic!("{name}: нет exposure.changed"));
        for flag in ["untrusted", "private", "external"] {
            assert_eq!(last[flag], true, "{name}: {flag}: {last}");
        }
        assert_eq!(last["calls"], json!(["w1"]), "{name}: {last}");

        let audit = e.audit("rule_of_two");
        assert_eq!(audit.len(), 1, "{name}: {audit:?}");
        let p = &audit[0].payload;
        assert_eq!(p["call_id"], "w1", "{name}: {p}");
        assert_eq!(p["tool"], "web_fetch", "{name}: {p}");
        assert_eq!(
            p["exposure"],
            json!({"untrusted": true, "private": true, "external": true}),
            "{name}: {p}"
        );
        assert_eq!(audit[0].target, run.to_string(), "{name}");
        // Запись до вызова: отметка в `tool.started`, не в `tool.finished`.
        let started = &of_call(&events, EventKind::ToolStarted, "w1")[0];
        let finished = &of_call(&events, EventKind::ToolFinished, "w1")[0];
        assert_eq!(started["rule_of_two"], true, "{name}: {started}");
        assert!(finished.get("rule_of_two").is_none(), "{name}: {finished}");
        assert!(
            of_call(&events, EventKind::ToolStarted, "r1")[0]
                .get("rule_of_two")
                .is_none(),
            "{name}"
        );

        assert_eq!(
            o.stderr.matches(RULE_OF_TWO_LINE).count(),
            1,
            "{name}: {o:?}"
        );
        for a in all_audit(&e)
            .iter()
            .filter(|a| a.action.starts_with("approval."))
        {
            assert!(
                !a.payload.to_string().contains("rule_of_two"),
                "{name}: {a:?}"
            );
        }
    }
}

/// Модель: чтение внешнего файла по гранту, две разные страницы, ответ.
fn read_then_two_fetches(seen: &[Value]) -> Response {
    let cur = seen.last().unwrap();
    match tool_results(cur) {
        0 => call_reply(
            "r1",
            "read",
            &json!({ "path": format!("{}/a.txt", task_arg(cur, "EXT")) }).to_string(),
        ),
        1 => call_reply(
            "w1",
            "web_fetch",
            &json!({ "url": task_arg(cur, "URL") }).to_string(),
        ),
        2 => call_reply(
            "w2",
            "web_fetch",
            &json!({ "url": task_arg(cur, "URL2") }).to_string(),
        ),
        _ => text_reply(&["готово"]),
    }
}

/// m1 ревью задачи 15: два вызова с внешней связью при всех трёх свойствах в режиме по
/// умолчанию (`ask`, без `--mode`) — run без паузы (у `read` по гранту и `web_fetch` вид
/// `Read`, D123), две записи аудита `rule_of_two` (у каждого вызова) и одна строка SEC-052
/// на run (D106, Ruling I1 ревью задачи 5).
#[test]
fn rule_of_two_two_fetches_in_ask_mode() {
    let site = FakeHttp::start(docs_site);
    let srv = Fake::start(N_CTX, true, read_then_two_fetches);
    let url = format!("http://docs.test:{}/spawn", site.port);
    let url2 = format!("http://docs.test:{}/x?p=2", site.port);
    let names = resolve(&[("docs.test", site.port)]);
    let name = "rule-of-two-ask";
    let e = env_in(GROUP, name, &web_cfg(&srv.base, &[site.port], None));
    let ext_root = unique_dir_in(&repo_root().join(".tmp/r1c2-acceptance"), name);
    let ext = shown(&std::fs::canonicalize(&ext_root).unwrap()).join("ext");
    write(&ext.join("a.txt"), "private note\n");
    let ext_arg = ext.to_string_lossy().replace('\\', "/");
    let grant = format!("{}=ro", ext.display());
    let task = format!("Сверь EXT=|{ext_arg}| с URL=|{url}| и URL2=|{url2}|");
    let o = e.run(&run_args(&e, &names, &["--grant", &grant, &task]));
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert_eq!(o.stdout, "готово\n", "{o:?}");
    let events = e.events(run_id(&o));
    assert_eq!(count(&events, EventKind::InterruptOpened), 0, "{events:?}");
    assert_eq!(
        site.requests(),
        ["/robots.txt", "/spawn", "/x?p=2"],
        "обе страницы запрошены"
    );
    let audit = e.audit("rule_of_two");
    let calls: Vec<&str> = audit
        .iter()
        .map(|a| a.payload["call_id"].as_str().unwrap())
        .collect();
    assert_eq!(calls, ["w1", "w2"], "{audit:?}");
    for a in &audit {
        assert_eq!(
            a.payload["exposure"],
            json!({"untrusted": true, "private": true, "external": true}),
            "{a:?}"
        );
    }
    assert_eq!(o.stderr.matches(RULE_OF_TWO_LINE).count(), 1, "{o:?}");
}

// ---- read PDF (таблица 5.2, D113) ----

/// Модель: `read` PDF проекта, затем ответ.
fn read_pdf_then_answer(seen: &[Value]) -> Response {
    if tool_results(seen.last().unwrap()) > 0 {
        return text_reply(&["готово"]);
    }
    call_reply("p1", "read", r#"{"path":"doc.pdf"}"#)
}

/// m4 ревью задачи 15 (таблица 5.2): `read` PDF через бинарник — извлекатель подключён к
/// нативным инструментам run (`deps`, `NativeDeps.extractor`), текст страниц разбирает
/// подпроцесс `extract pdf` (D113); копия PDF — в `.zero-agent/work/tmp/<run>`, после run
/// каталога нет.
#[test]
fn read_pdf_through_binary() {
    let srv = Fake::start(N_CTX, true, read_pdf_then_answer);
    let e = env_in(GROUP, "read-pdf", &llama_config(&srv.base));
    std::fs::write(e.project.join("doc.pdf"), pdf::two_page_pdf()).unwrap();
    let o = e.run_cmd(&["Прочитай doc.pdf"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let p1 = result_of(&srv.bodies(), "p1");
    assert!(
        p1.contains("--- page 1 ---\nalpha") && p1.contains("--- page 2 ---\nbeta"),
        "{p1}"
    );
    assert!(!p1.contains("binary file"), "{p1}");
    let run = run_id(&o);
    let tmp = e.project.join(format!(".zero-agent/work/tmp/{run}"));
    assert!(!tmp.exists(), "{} не убран", tmp.display());
}

// ---- Скиллы (D116, D118, D119) ----

/// Скилл проекта: описание с редкими словами (роутер находит его по ним), тело с маркером.
const SKILL_MD: &str = "---\nname: deploy-notes\ndescription: Use when releasing the zebra billing service to staging.\n---\nRelease steps: run the zebra-billing checklist ZBX-4471 before the deploy.\n";
/// Маркер тела скилла.
const SKILL_BODY: &str = "ZBX-4471";

/// Доверенный проект со скиллом `.agents/skills/deploy-notes`.
fn skill_project(name: &str, llama: &str) -> Env {
    let e = env_in(GROUP, name, &llama_config(llama));
    write(
        &e.project.join(".agents/skills/deploy-notes/SKILL.md"),
        SKILL_MD,
    );
    e.trust();
    e
}

/// Payload единственного `skill.activated` run.
fn activated(e: &Env, run: RunId) -> Value {
    let evs = payloads(&e.events(run), EventKind::SkillActivated);
    assert_eq!(evs.len(), 1, "skill.activated: {evs:?}");
    evs.into_iter().next().unwrap()
}

/// Модель: `activate_skill deploy-notes`, затем ответ.
fn activate_model(seen: &[Value]) -> Response {
    if tool_results(seen.last().unwrap()) > 0 {
        return text_reply(&["готово"]);
    }
    call_reply("k1", "activate_skill", r#"{"name":"deploy-notes"}"#)
}

/// Модель загружает скилл проекта (D118): событие `skill.activated {by: "model"}`, строка
/// stderr, текст скилла в блоке `<skill_content>` — в истории следующего запроса.
#[test]
fn skill_activation_end_to_end() {
    let srv = Fake::start(N_CTX, true, activate_model);
    let e = skill_project("skill-activate", &srv.base);
    let o = e.run_cmd(&["Выпусти сервис"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let ev = activated(&e, run_id(&o));
    assert_eq!(ev["name"], "deploy-notes", "{ev}");
    assert_eq!(ev["source"], "project", "{ev}");
    assert_eq!(ev["by"], "model", "{ev}");
    assert!(
        o.stderr
            .contains("zero-agent: скилл deploy-notes (project)"),
        "{o:?}"
    );
    let bodies = srv.bodies();
    assert_eq!(bodies.len(), 2, "{o:?}");
    assert!(
        tool_names(&bodies[0]).iter().any(|n| n == "activate_skill"),
        "{:?}",
        tool_names(&bodies[0])
    );
    let r = result_of(&bodies, "k1");
    assert!(
        r.contains("<skill_content name=\"deploy-notes\" source=\"project\""),
        "{r}"
    );
    assert!(r.contains(SKILL_BODY), "{r}");
}

/// `/имя` пользователя (D119): полный текст скилла дописан к сообщению пользователя уже в
/// первом запросе, `skill.activated {by: "user"}`.
#[test]
fn slash_name_loads_skill() {
    let srv = Fake::start(N_CTX, true, answer_only);
    let e = skill_project("skill-slash", &srv.base);
    let o = e.run_cmd(&["/deploy-notes подготовь выпуск"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let ev = activated(&e, run_id(&o));
    assert_eq!(ev["by"], "user", "{ev}");
    let first = user_text(&srv.bodies()[0]);
    assert!(
        first.contains("<skill_content name=\"deploy-notes\" source=\"project\""),
        "{first}"
    );
    assert!(first.contains(SKILL_BODY), "{first}");
}

/// Роутер tier small (D119): у ввода с общими словами описания — карточка скилла в
/// сообщении пользователя (не полный текст), `skill.activated {by: "router"}`, строка
/// «карточка роутера»; `activate_skill` на small нет (min_tier standard).
#[test]
fn router_card_on_small() {
    let srv = Fake::start(N_CTX_SMALL, true, answer_only);
    let e = skill_project("skill-router", &srv.base);
    let o = e.run_cmd(&["release the zebra billing service today"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let ev = activated(&e, run_id(&o));
    assert_eq!(ev["by"], "router", "{ev}");
    assert!(
        o.stderr
            .contains("zero-agent: скилл deploy-notes (project), карточка роутера"),
        "{o:?}"
    );
    let bodies = srv.bodies();
    let first = user_text(&bodies[0]);
    assert!(
        first.contains("<skill_card name=\"deploy-notes\">"),
        "{first}"
    );
    assert!(!first.contains("<skill_content"), "{first}");
    assert!(
        !tool_names(&bodies[0]).iter().any(|n| n == "activate_skill"),
        "{:?}",
        tool_names(&bodies[0])
    );
}

// ---- Resume веба (Review Focus 5, D112) ----

/// Сбой процесса после вызова `web_fetch`, до квитанции (`--crash-at in-tool:1`): обращения
/// вызова уже в журнале; resume повторяет вызов без вопроса (класс `ReadOnly`) — сайт
/// получает страницу второй раз; квитанция повтора без `requests`.
#[test]
fn resume_after_crash_in_web_fetch_repeats_read_only() {
    let site = FakeHttp::start(docs_site);
    let srv = Fake::start(N_CTX, true, fetch_then_answer);
    let e = env_in(
        GROUP,
        "crash-in-fetch",
        &web_cfg(&srv.base, &[site.port], None),
    );
    let url = format!("http://docs.test:{}/spawn", site.port);
    let names = resolve(&[("docs.test", site.port)]);
    let task = format!("Открой URL=|{url}|");
    let o = e.run(&run_args(&e, &names, &["--crash-at", "in-tool:1", &task]));
    assert_aborted(&o);
    let run = run_id(&o);
    let events = e.events(run);
    assert_eq!(of_call(&events, EventKind::ToolStarted, "f1").len(), 1);
    assert!(of_call(&events, EventKind::ToolFinished, "f1").is_empty());
    assert_eq!(count(&events, EventKind::WebRequest), 2, "{events:?}");
    assert_eq!(site.requests(), ["/robots.txt", "/spawn"]);

    let id = run.to_string();
    let r = e.run(&run_args(&e, &names, &["--resume", &id]));
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(r.stdout, "готово\n", "{r:?}");
    let pages = site.requests().iter().filter(|p| *p == "/spawn").count();
    assert_eq!(pages, 2, "{:?}", site.requests());
    let events = e.events(run);
    assert_eq!(count(&events, EventKind::InterruptOpened), 0, "{events:?}");
    assert_eq!(of_call(&events, EventKind::ToolStarted, "f1").len(), 2);
    assert_eq!(of_call(&events, EventKind::ToolFinished, "f1").len(), 1);
    assert_eq!(count(&events, EventKind::WebRequest), 4, "{events:?}");
    assert!(result_of(&srv.bodies(), "f1").contains("# Spawning"));
    let receipt = receipt_of(&e, run, "f1");
    assert!(receipt.get("requests").is_none(), "{receipt}");
    assert!(!receipt.to_string().contains("\"requests\""), "{receipt}");
}

/// Сбой после квитанции `web_fetch` (`--crash-at after-boundary:2`: граница шага 2
/// записана, снимка шага 2 нет): resume реплеит шаг 1 по квитанциям — ни одного нового
/// запроса к сайту и ни одного нового `web.request`; модель получает тот же результат;
/// в квитанции нет `requests` (D112).
#[test]
fn resume_after_receipt_does_not_touch_network() {
    let site = FakeHttp::start(docs_site);
    let srv = Fake::start(N_CTX, true, fetch_then_answer);
    let e = env_in(
        GROUP,
        "crash-after-receipt",
        &web_cfg(&srv.base, &[site.port], None),
    );
    let url = format!("http://docs.test:{}/spawn", site.port);
    let names = resolve(&[("docs.test", site.port)]);
    let task = format!("Открой URL=|{url}|");
    let o = e.run(&run_args(
        &e,
        &names,
        &["--crash-at", "after-boundary:2", &task],
    ));
    assert_aborted(&o);
    let run = run_id(&o);
    let events = e.events(run);
    assert_eq!(of_call(&events, EventKind::ToolFinished, "f1").len(), 1);
    assert_eq!(count(&events, EventKind::WebRequest), 2, "{events:?}");
    assert_eq!(count(&events, EventKind::TurnStarted), 1, "{events:?}");
    assert_eq!(site.requests(), ["/robots.txt", "/spawn"]);
    let receipt = receipt_of(&e, run, "f1");
    assert!(receipt.get("requests").is_none(), "{receipt}");
    assert!(!receipt.to_string().contains("\"requests\""), "{receipt}");

    let id = run.to_string();
    let r = e.run(&run_args(&e, &names, &["--resume", &id]));
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(r.stdout, "готово\n", "{r:?}");
    assert_eq!(
        site.requests(),
        ["/robots.txt", "/spawn"],
        "сеть при реплее"
    );
    let events = e.events(run);
    assert_eq!(count(&events, EventKind::WebRequest), 2, "{events:?}");
    assert_eq!(of_call(&events, EventKind::ToolStarted, "f1").len(), 1);
    assert!(!r.stderr.contains("zero-agent: веб:"), "{r:?}");
    let bodies = srv.bodies();
    assert_eq!(bodies.len(), 2, "реплей шага 1 без запроса модели");
    assert!(result_of(&bodies, "f1").contains("# Spawning"));
}

// ---- Resume run с веб-инструментами (m6 ревью задачи 14) ----

/// Модель: поиск, вопрос пользователю (пауза), после ответа — второй поиск, ответ.
fn search_pause_search(seen: &[Value]) -> Response {
    match tool_results(seen.last().unwrap()) {
        0 => call_reply("s1", "web_search", r#"{"query":"tokio spawn"}"#),
        1 => call_reply("q1", "ask_user", r#"{"question":"Искать дальше?"}"#),
        2 => call_reply("s2", "web_search", r#"{"query":"tokio join"}"#),
        _ => text_reply(&["готово"]),
    }
}

/// Модель: страница, вопрос пользователю (пауза), после ответа — снова страница, ответ.
fn fetch_pause_fetch(seen: &[Value]) -> Response {
    let cur = seen.last().unwrap();
    match tool_results(cur) {
        0 => call_reply(
            "f1",
            "web_fetch",
            &json!({ "url": task_arg(cur, "URL") }).to_string(),
        ),
        1 => call_reply("q1", "ask_user", r#"{"question":"Открыть ещё раз?"}"#),
        2 => call_reply(
            "f2",
            "web_fetch",
            &json!({ "url": task_arg(cur, "URL") }).to_string(),
        ),
        _ => text_reply(&["готово"]),
    }
}

/// Пауза на `ask_user` (код 3), ответ и resume; итог resume.
fn pause_answer_resume(e: &Env, names: &[String], first: &Out) -> Out {
    assert_eq!(first.code(), Some(3), "{first:?}");
    let id = interrupt_id(first);
    assert!(id.starts_with("input-"), "{id}: {first:?}");
    let a = e.answer(&id, "да");
    assert_eq!(a.code(), Some(0), "{a:?}");
    let run = run_id(first).to_string();
    e.run(&run_args(e, names, &["--resume", &run]))
}

/// Seq событий run строго растут, номера ходов `turn.started` идут подряд с 1: журнал
/// продолжен тем же run, а не начат заново.
fn assert_continuous(e: &Env, run: RunId) {
    let store = e.store();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let events = rt
        .block_on(store.read_run(run, za_protocol::Seq(0), usize::MAX))
        .unwrap();
    assert!(events.iter().all(|ev| ev.run_id == run));
    assert!(
        events.windows(2).all(|w| w[0].seq < w[1].seq),
        "seq не растут"
    );
    let turns: Vec<u64> = events
        .iter()
        .filter(|ev| ev.kind == EventKind::TurnStarted)
        .map(|ev| ev.payload["turn"].as_u64().unwrap())
        .collect();
    let want: Vec<u64> = (1..=turns.len() as u64).collect();
    assert_eq!(turns, want, "ходы не подряд");
}

/// Resume run с веб-инструментами (m6 ревью задачи 14). Набор инструментов после resume —
/// из замороженного префикса run (`frozen.tools`), а не из нынешней конфигурации:
/// - поиск настроен с начала: после паузы и resume `web_search` работает (второй запрос к
///   поддельному SearXNG), набор инструментов в запросах модели тот же, журнал непрерывен;
/// - `[web.search] url` добавлен во время паузы: `web_search` посреди run не появляется,
///   к SearXNG ни одного запроса;
/// - веб выключен (`[web] enabled = false`) во время паузы: `web_fetch` после resume не
///   исполняется — сайт второго запроса страницы не получает.
///
/// Запись аудита `policy.loaded` при resume (I1 ревью задачи 14, проход fix-14; места
/// отмечены `AUDIT(fix-14)`): без изменений — дубликата нет; выключенный веб — новая запись
/// с `web_disabled` при том же хэше политики.
#[test]
fn resume_keeps_frozen_web_tools() {
    let site = FakeHttp::start(docs_site);
    let sx = searx(vec![(
        "Spawning | Tokio".into(),
        "https://docs.example/spawn".into(),
    )]);
    let names = resolve(&[("docs.test", site.port)]);
    let url = format!("http://docs.test:{}/spawn", site.port);

    // Поиск настроен с начала.
    let srv = Fake::start(N_CTX, true, search_pause_search);
    let e = env_in(
        GROUP,
        "resume-search",
        &web_cfg(&srv.base, &[site.port], Some(&sx.base)),
    );
    let first = e.run(&run_args(&e, &names, &["Найди про tokio"]));
    let run = run_id(&first);
    let before = tool_names(&srv.bodies()[0]);
    assert!(
        before.iter().any(|n| n == "web_search") && before.iter().any(|n| n == "web_fetch"),
        "{before:?}"
    );
    let r = pause_answer_resume(&e, &names, &first);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(r.stdout, "готово\n", "{r:?}");
    let bodies = srv.bodies();
    for b in &bodies {
        assert_eq!(
            tool_names(b),
            before,
            "набор инструментов после resume другой"
        );
    }
    let probes = sx.requests();
    assert_eq!(probes.len(), 2, "{probes:?}");
    assert!(probes[1].starts_with("/search?q=tokio+join"), "{probes:?}");
    assert!(result_of(&bodies, "s2").contains("Spawning | Tokio"));
    let events = e.events(run);
    let started: Vec<String> = payloads(&events, EventKind::ToolStarted)
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        started,
        ["web_search", "ask_user", "web_search"],
        "{events:?}"
    );
    assert_eq!(count(&events, EventKind::WebRequest), 2, "{events:?}");
    assert!(
        r.stderr.contains(&format!(
            "zero-agent: веб: GET {}/search?q=tokio+join",
            sx.base
        )),
        "{r:?}"
    );
    assert_continuous(&e, run);
    // AUDIT(fix-14): resume сверяет `policy.loaded` с действующим снимком (I1 ревью задачи
    // 14); ничего не менялось — дубликата нет, запись старта называет хост поставщика.
    let loaded = e.audit("policy.loaded");
    assert_eq!(loaded.len(), 1, "{loaded:?}");
    assert_eq!(loaded[0].payload["search_host"], "127.0.0.1", "{loaded:?}");
    assert_eq!(loaded[0].payload["web_disabled"], Value::Null, "{loaded:?}");

    // `[web.search] url` добавлен во время паузы: поиска в run нет.
    let srv = Fake::start(N_CTX, true, fetch_pause_fetch);
    let e = env_in(
        GROUP,
        "resume-search-added",
        &web_cfg(&srv.base, &[site.port], None),
    );
    let first = e.run(&run_args(&e, &names, &[&format!("Открой URL=|{url}|")]));
    let run = run_id(&first);
    let before = tool_names(&srv.bodies()[0]);
    assert!(!before.iter().any(|n| n == "web_search"), "{before:?}");
    std::fs::write(
        e.home.join("config.toml"),
        web_cfg(&srv.base, &[site.port], Some(&sx.base)),
    )
    .unwrap();
    let r = pause_answer_resume(&e, &names, &first);
    assert_eq!(r.code(), Some(0), "{r:?}");
    for b in &srv.bodies() {
        assert_eq!(tool_names(b), before, "новый инструмент посреди run");
    }
    // Два запроса — из первой части теста; новых нет.
    assert_eq!(
        sx.requests().len(),
        2,
        "к SearXNG обращались: {:?}",
        sx.requests()
    );
    assert!(result_of(&srv.bodies(), "f2").contains("# Spawning"));
    assert_continuous(&e, run);
    // AUDIT(fix-14): `web_search` в run нет, исключения D126 у run нет — `search_host`
    // по-прежнему пуст, новой записи нет.
    let loaded = e.audit("policy.loaded");
    assert_eq!(loaded.len(), 1, "{loaded:?}");
    assert_eq!(loaded[0].payload["search_host"], Value::Null, "{loaded:?}");

    // Веб выключен во время паузы: `web_fetch` после resume не исполняется.
    let site_off = FakeHttp::start(docs_site);
    let names_off = resolve(&[("docs.test", site_off.port)]);
    let url_off = format!("http://docs.test:{}/spawn", site_off.port);
    let srv = Fake::start(N_CTX, true, fetch_pause_fetch);
    let e = env_in(
        GROUP,
        "resume-web-off",
        &web_cfg(&srv.base, &[site_off.port], None),
    );
    let first = e.run(&run_args(
        &e,
        &names_off,
        &[&format!("Открой URL=|{url_off}|")],
    ));
    let run = run_id(&first);
    assert_eq!(site_off.requests(), ["/robots.txt", "/spawn"]);
    std::fs::write(
        e.home.join("config.toml"),
        format!("{}\n[web]\nenabled = false\n", llama_config(&srv.base)),
    )
    .unwrap();
    let r = pause_answer_resume(&e, &names_off, &first);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(
        site_off.requests(),
        ["/robots.txt", "/spawn"],
        "выключенный веб исполнен"
    );
    let f2 = result_of(&srv.bodies(), "f2");
    // Описание `web_fetch` остаётся в замороженном префиксе (кэш префикса), вызов не
    // исполняется — ни слота, ни сети (сейчас результат `unknown tool "web_fetch"`).
    assert!(
        !f2.contains("# Spawning") && f2.contains("web_fetch"),
        "{f2}"
    );
    let events = e.events(run);
    assert!(
        of_call(&events, EventKind::ToolStarted, "f2").is_empty(),
        "{events:?}"
    );
    assert_eq!(count(&events, EventKind::WebRequest), 2, "{events:?}");
    assert_continuous(&e, run);
    // AUDIT(fix-14): веб выключен между стартом и resume — новая запись `policy.loaded` с
    // `web_disabled = "config"` при том же хэше политики (I1 ревью задачи 14).
    let loaded = e.audit("policy.loaded");
    assert_eq!(loaded.len(), 2, "{loaded:?}");
    assert_eq!(loaded[0].payload["web_disabled"], Value::Null, "{loaded:?}");
    assert_eq!(loaded[1].payload["web_disabled"], "config", "{loaded:?}");
    assert_eq!(
        loaded[1].payload["hash"], loaded[0].payload["hash"],
        "{loaded:?}"
    );
}

/// Находка 1 приёмки (Ruling «Задача 15»): веб-инструмент остался в замороженном префиксе
/// (префикс не меняется — кэш), а конфигурация его выключила во время паузы. Вызов после
/// resume не исполняется (ни `tool.started`, ни сети) и отвечает модели понятным текстом
/// с именем инструмента, а не `unknown tool`; в stderr resume — строка о выключенном вебе:
/// - `[web] enabled = false` — `web_fetch: web tools are disabled by configuration since
///   this run started`;
/// - `[web.search] url` убран — `web_search: web search is not configured since this run
///   started`.
#[test]
fn disabled_web_tool_explains_after_resume() {
    let site = FakeHttp::start(docs_site);
    let names = resolve(&[("docs.test", site.port)]);
    let url = format!("http://docs.test:{}/spawn", site.port);
    let srv = Fake::start(N_CTX, true, fetch_pause_fetch);
    let e = env_in(
        GROUP,
        "disabled-web-text",
        &web_cfg(&srv.base, &[site.port], None),
    );
    let first = e.run(&run_args(&e, &names, &[&format!("Открой URL=|{url}|")]));
    let run = run_id(&first);
    std::fs::write(
        e.home.join("config.toml"),
        format!("{}\n[web]\nenabled = false\n", llama_config(&srv.base)),
    )
    .unwrap();
    let r = pause_answer_resume(&e, &names, &first);
    assert_eq!(r.code(), Some(0), "{r:?}");
    let f2 = result_of(&srv.bodies(), "f2");
    assert_eq!(
        f2,
        "web_fetch: web tools are disabled by configuration since this run started"
    );
    assert!(
        r.stderr.contains(
            "zero-agent: веб выключен после старта run ([web] enabled = false): вызовы web_fetch этого run не исполняются"
        ),
        "{r:?}"
    );
    assert_eq!(site.requests(), ["/robots.txt", "/spawn"]);
    let events = e.events(run);
    assert!(
        of_call(&events, EventKind::ToolStarted, "f2").is_empty(),
        "{events:?}"
    );

    // Поиск убран из конфигурации во время паузы: `web_fetch` работает, `web_search` — нет.
    let sx = searx(vec![(
        "Spawning | Tokio".into(),
        "https://docs.example/spawn".into(),
    )]);
    let srv = Fake::start(N_CTX, true, search_pause_search);
    let e = env_in(
        GROUP,
        "search-removed-text",
        &web_cfg(&srv.base, &[site.port], Some(&sx.base)),
    );
    let first = e.run(&run_args(&e, &names, &["Найди про tokio"]));
    std::fs::write(
        e.home.join("config.toml"),
        web_cfg(&srv.base, &[site.port], None),
    )
    .unwrap();
    let r = pause_answer_resume(&e, &names, &first);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(
        result_of(&srv.bodies(), "s2"),
        "web_search: web search is not configured since this run started"
    );
    assert!(
        r.stderr.contains(
            "zero-agent: поиск не настроен после старта run ([web.search] url): вызовы web_search этого run не исполняются"
        ),
        "{r:?}"
    );
    assert_eq!(sx.requests().len(), 1, "{:?}", sx.requests());
}

/// Модель: `activate_skill tdd` (`a1`), пауза на `ask_user`, затем `activate_skill review`
/// (`a2`), затем ответ.
fn activate_pause_activate(seen: &[Value]) -> Response {
    let cur = seen.last().unwrap();
    match tool_results(cur) {
        0 => call_reply("a1", "activate_skill", r#"{"name":"tdd"}"#),
        1 => call_reply("q1", "ask_user", r#"{"question":"Продолжить?"}"#),
        2 => call_reply("a2", "activate_skill", r#"{"name":"review"}"#),
        _ => text_reply(&["готово"]),
    }
}

/// Minor 4 финального ревью скиллов (по образцу веба, Ruling «Задача 15»): скиллы выключены
/// во время паузы (`[skills] enabled = false`), а `activate_skill` остался в замороженном
/// префиксе. Вызов после resume отвечает понятным текстом, а не `unknown tool`; в stderr
/// resume — строка о выключенных скиллах; загрузка до паузы в истории не тронута.
#[test]
fn disabled_skills_explain_after_resume() {
    let srv = Fake::start(N_CTX, true, activate_pause_activate);
    let e = env_in(GROUP, "disabled-skills-text", &llama_config(&srv.base));
    let first = e.run(&run_args(&e, &[], &["Проверь скилл"]));
    std::fs::write(
        e.home.join("config.toml"),
        format!("{}\n[skills]\nenabled = false\n", llama_config(&srv.base)),
    )
    .unwrap();
    let r = pause_answer_resume(&e, &[], &first);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert!(
        result_of(&srv.bodies(), "a1").contains("<skill_content name=\"tdd\" source=\"bundled\""),
        "{:?}",
        result_of(&srv.bodies(), "a1")
    );
    assert_eq!(
        result_of(&srv.bodies(), "a2"),
        "activate_skill: skills are disabled by configuration since this run started"
    );
    assert!(
        r.stderr.contains(
            "zero-agent: скиллы выключены после старта run ([skills] enabled = false): вызовы activate_skill этого run не исполняются"
        ),
        "{r:?}"
    );
}

// ---- Секрет в URL ----

/// Значение `ZA_SECRET_DEMO`: собирается из частей во время выполнения (сканеры
/// репозитория не видят его целиком), длина ≥ 8 (маска известных значений).
fn secret_value() -> String {
    ["zq7", "Vk2", "pLx", "93w", "Tn5"].concat()
}

/// Модель: страница с секретом в параметре запроса (`f1`), затем адрес `/r`, который сайт
/// перенаправляет на тот же хост с секретом в запросе (`f2`), затем ответ.
fn secret_model(seen: &[Value]) -> Response {
    let cur = seen.last().unwrap();
    match tool_results(cur) {
        0 => {
            let url = format!("{}/x?k={}", task_arg(cur, "SITE"), secret_value());
            call_reply("f1", "web_fetch", &json!({ "url": url }).to_string())
        }
        1 => {
            let url = format!("{}/r", task_arg(cur, "SITE"));
            call_reply("f2", "web_fetch", &json!({ "url": url }).to_string())
        }
        _ => text_reply(&["готово"]),
    }
}

/// Значение `ZA_SECRET_DEMO` в URL не попадает на диск: ни в журнал (события
/// `web.request`, `tool.started`, квитанции, блобы), ни в аудит, ни в каталог проекта, ни
/// в stdout, stderr и `journal export`. Два пути:
/// - секрет в аргументах модели (`f1`) маскируется ещё в ответе модели (маска стрима,
///   R1-C1 D90): шлюз получает и запрашивает адрес уже с маской — значение машину не
///   покидает;
/// - секрет в адресе со стороны сети (`f2`: редирект того же хоста на `/x?k=<значение>`):
///   шлюз идёт по нему, а событие `web.request` и строка `url:` результата — с маской
///   (`SecretMasker`, D112).
#[test]
fn secret_in_fetch_url_masked() {
    let site = FakeHttp::start(docs_site);
    let srv = Fake::start(N_CTX, true, secret_model);
    let e = env_in(GROUP, "secret-url", &web_cfg(&srv.base, &[site.port], None));
    let secret = secret_value();
    let base = format!("http://docs.test:{}", site.port);
    let names = resolve(&[("docs.test", site.port)]);
    let task = format!("Открой SITE=|{base}|");
    let args = run_args(&e, &names, &[&task]);
    let o = e.run_with(&args, &[("ZA_SECRET_DEMO", &secret)]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let got = site.requests();
    assert_eq!(got.len(), 4, "{got:?}");
    assert!(
        got[1].starts_with("/x?k=") && got[1].contains("masked") && !got[1].contains(&secret),
        "аргумент модели ушёл без маски: {got:?}"
    );
    assert_eq!(
        got[2..],
        ["/r".to_owned(), format!("/x?k={secret}")],
        "{got:?}"
    );
    let bodies = srv.bodies();
    assert!(result_of(&bodies, "f1").contains("Parameter page."));
    let f2 = result_of(&bodies, "f2");
    assert!(f2.contains("Parameter page."), "{f2}");
    assert!(
        f2.contains("[secret masked: ") && !f2.contains(&secret),
        "{f2}"
    );
    assert!(
        !o.stdout.contains(&secret) && !o.stderr.contains(&secret),
        "{o:?}"
    );
    let run = run_id(&o);
    let reqs = payloads(&e.events(run), EventKind::WebRequest);
    assert_eq!(reqs.len(), 4, "{reqs:?}");
    let hop = &reqs[3];
    assert!(
        hop["url"]
            .as_str()
            .unwrap()
            .contains("/x?k=[secret masked: "),
        "{hop}"
    );

    let needle = secret.as_bytes();
    let home_files = files_under(&e.home);
    assert!(
        home_files.iter().any(|f| f.ends_with("journal.db")),
        "{home_files:?}"
    );
    let project_files = files_under(&e.project);
    for f in home_files.iter().chain(&project_files) {
        let data = std::fs::read(f).unwrap();
        assert!(!contains(&data, needle), "секрет в {}", f.display());
    }
    let x = e.run_with(
        &[
            "journal",
            "export",
            "--project",
            e.project_arg(),
            "--run",
            &run.to_string(),
        ],
        &[("ZA_SECRET_DEMO", &secret)],
    );
    assert_eq!(x.code(), Some(0), "{x:?}");
    assert!(x.stdout.contains("web.request"), "{}", x.stdout.len());
    assert!(!x.stdout.contains(&secret), "секрет в journal export");
}

// ---- Прокси (D107, D121) ----

/// Модель: поиск, страница из задачи, ответ.
fn search_then_fetch(seen: &[Value]) -> Response {
    let cur = seen.last().unwrap();
    match tool_results(cur) {
        0 => call_reply("s1", "web_search", r#"{"query":"tokio spawn"}"#),
        1 => call_reply(
            "f1",
            "web_fetch",
            &json!({ "url": task_arg(cur, "URL") }).to_string(),
        ),
        _ => text_reply(&["готово"]),
    }
}

/// `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` (и строчные — `reqwest` на Unix читает и их)
/// процесса бинарника на поддельный прокси: `web_search` и `web_fetch` идут напрямую,
/// у прокси ни одного запроса. `NO_PROXY` процессу не передаётся — исключение сделало бы
/// проверку пустой. Окружение — через `Command::env` (без `std::env::set_var`).
#[test]
fn proxy_env_is_ignored() {
    let site = FakeHttp::start(docs_site);
    let sx = searx(vec![(
        "Spawning | Tokio".into(),
        "https://docs.example/spawn".into(),
    )]);
    let px = proxy();
    let srv = Fake::start(N_CTX, true, search_then_fetch);
    let e = env_in(
        GROUP,
        "proxy",
        &web_cfg(&srv.base, &[site.port], Some(&sx.base)),
    );
    let url = format!("http://docs.test:{}/spawn", site.port);
    let names = resolve(&[("docs.test", site.port)]);
    let task = format!("Найди и открой URL=|{url}|");
    let args = run_args(&e, &names, &[&task]);
    let vars: Vec<(&str, &str)> = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ]
    .iter()
    .map(|k| (*k, px.base.as_str()))
    .collect();
    let o = e.run_env(&args, &vars, &["NO_PROXY", "no_proxy"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(
        px.requests().is_empty(),
        "запросы через прокси: {:?}",
        px.requests()
    );
    assert_eq!(sx.requests().len(), 1, "{:?}", sx.requests());
    assert_eq!(site.requests(), ["/robots.txt", "/spawn"]);
    let bodies = srv.bodies();
    assert!(result_of(&bodies, "s1").contains("Spawning | Tokio"));
    assert!(result_of(&bodies, "f1").contains("# Spawning"));
}

// ---- Живой llama-server и свой SearXNG ----

/// Ручная проверка S1 (шаг 1): настоящая модель отвечает на вопрос с поиском через свой
/// экземпляр SearXNG и открытием страниц в настоящем интернете. Ссылки ответа проверяются
/// вручную; итог печатается для `docs/journal.md`.
///
/// Окно llama-server — от 32768 токенов (`-c 32768` или больше; m3 ревью задачи 15): на
/// меньшем окне тир run — small (`Tier::for_context`), а у `web_search` и `web_fetch`
/// `min_tier = standard` (D123) — веб-инструментов в run нет. Тест проверяет это первым,
/// с понятным сообщением.
#[test]
#[ignore = "живые llama-server (окно от 32768: -c 32768) и SearXNG: LLAMA_TEST_URL=http://127.0.0.1:8080 SEARXNG_TEST_URL=http://127.0.0.1:8888 ZA_TEST_TIME_SCALE=6 cargo test -p zero-agent --features test-hooks --test r1c2_web_skills -- --ignored --nocapture"]
fn live_llama_s1() {
    let need = |k: &str| {
        std::env::var(k).unwrap_or_else(|_| {
            panic!(
                "{k} не задан: запуск — LLAMA_TEST_URL=http://127.0.0.1:8080 \
                 SEARXNG_TEST_URL=http://127.0.0.1:8888 ZA_TEST_TIME_SCALE=6 cargo test -p \
                 zero-agent --features test-hooks --test r1c2_web_skills -- --ignored --nocapture"
            )
        })
    };
    let llama = need("LLAMA_TEST_URL");
    let searx = need("SEARXNG_TEST_URL");
    let cfg = format!(
        "{}\n[web.search]\nurl = \"{}\"\n",
        llama_config(llama.trim_end_matches('/')),
        searx.trim_end_matches('/')
    );
    let e = env_in(GROUP, "live-s1", &cfg);
    let o = e.run_with(
        &[
            "run",
            "--project",
            e.project_arg(),
            "How do I run a background task with tokio::spawn? Search the web, read one or two \
             pages and answer in three sentences with links to the sources.",
        ],
        &[("ZA_LOG", "info")],
    );
    println!("stdout:\n{}\nstderr:\n{}", o.stdout, o.stderr);
    let tools: Vec<String> = e.snapshot(run_id(&o))["frozen"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect();
    assert!(
        tools.iter().any(|n| n == "web_search"),
        "в run нет web_search: окно модели меньше 32768 (тир small) — запустите llama-server с -c 32768 или больше; инструменты run: {tools:?}"
    );
    assert_eq!(o.code(), Some(0), "{o:?}");
    let events = e.events(run_id(&o));
    let reqs = payloads(&events, EventKind::WebRequest);
    println!(
        "итог: вызовов {}, обращений {} (ok {}), ответ со ссылкой: {}",
        count(&events, EventKind::ToolStarted),
        reqs.len(),
        reqs.iter().filter(|r| r["outcome"] == "ok").count(),
        o.stdout.contains("http")
    );
    assert!(reqs.iter().any(|r| r["outcome"] == "ok"), "{reqs:?}");
    assert!(o.stdout.contains("http"), "в ответе нет ссылок");
}
