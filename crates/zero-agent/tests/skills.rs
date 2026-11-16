//! Веб и скиллы в бинарнике (R1-C2, задача 14; D105, D111, D116, D122, D126, D169):
//! `skills list`, `doctor` с поддельным SearXNG на `127.0.0.1`, набор инструментов run
//! (поиск только при `[web.search] url`, проект выключает веб), каталог скиллов run
//! (проектные — только у доверенного проекта, событие `skill.catalog`), веб-инструменты
//! против поддельных сайта и SearXNG (`--web-resolve`, D128) и бюджет описаний набора
//! standard с вебом и каталогом (D117, D169).
//!
//! Настоящий интернет не используется: все серверы — на `127.0.0.1` в процессе теста.

mod support;

use std::path::Path;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::Response;
use serde_json::Value;
use support::fake_llama::{Fake, call_reply, llama_config, text_reply};
use support::fake_web::{FakeHttp, closed_port, reply, searx_results};
use support::process::{Env, env_in, run_id};
use za_protocol::EventKind;

const GROUP: &str = "zero-agent-skills";
/// Окно модели: тир standard по умолчанию (веб и `activate_skill` — от standard).
const N_CTX: u32 = 32_768;

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn skill_md(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\nSteps for {name}.\n")
}

fn answer_only(_: &[Value]) -> Response {
    text_reply(&["готово"])
}

/// Имена инструментов первого запроса модели.
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

/// Описание инструмента `name` в запросе.
fn tool_description(body: &Value, name: &str) -> Option<String> {
    body["tools"].as_array()?.iter().find_map(|t| {
        (t["function"]["name"] == name).then(|| {
            t["function"]["description"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
    })
}

/// Payload события `skill.catalog` run (одно на run).
fn catalog_event(e: &Env, run: za_protocol::RunId) -> Value {
    let evs: Vec<Value> = e
        .events(run)
        .into_iter()
        .filter(|(k, _)| *k == EventKind::SkillCatalog)
        .map(|(_, p)| p)
        .collect();
    assert_eq!(evs.len(), 1, "skill.catalog: {evs:?}");
    evs.into_iter().next().unwrap()
}

// ---- skills list (D122) ----

/// `skills list`: источники и пути, пропущенные с причиной; проектные — только у
/// доверенного проекта, недоверенный — строка «проект не доверен» и отличие доверия;
/// корень `extra` больше `[skills] max_entries` назван, следующий корень прочитан.
#[test]
fn skills_list_shows_sources_and_skipped() {
    let e = env_in(GROUP, "list", "");
    write(
        &e.home.join("skills/my/SKILL.md"),
        &skill_md("my", "Use when checking my own skill."),
    );
    write(
        &e.project.join(".agents/skills/bad/SKILL.md"),
        "---\nname: bad\ndescription: \n---\nbody\n",
    );
    write(
        &e.project.join(".agents/skills/deploy-notes/SKILL.md"),
        &skill_md("deploy-notes", "Use when deploying."),
    );
    e.trust();
    let o = e.run(&["skills", "list", "--project", e.project_arg()]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    for want in [
        "my  user",
        "tdd  bundled",
        "deploy-notes  project  .agents/skills/deploy-notes",
        ".agents/skills/bad: empty description",
    ] {
        assert!(o.stdout.contains(want), "нет {want:?}: {}", o.stdout);
    }
    assert!(!o.stdout.contains("проект не доверен"), "{}", o.stdout);
    // Без проекта — только источники вне проекта.
    let o = e.run(&["skills", "list"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(o.stdout.contains("tdd  bundled"), "{}", o.stdout);
    assert!(!o.stdout.contains("deploy-notes"), "{}", o.stdout);
    // Правка скилла снимает доверие (D83): проектных скиллов нет, отличие названо.
    write(
        &e.project.join(".agents/skills/deploy-notes/SKILL.md"),
        &skill_md("deploy-notes", "Use when deploying, edited."),
    );
    let o = e.run(&["skills", "list", "--project", e.project_arg()]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(o.stdout.contains("проект не доверен"), "{}", o.stdout);
    assert!(
        o.stdout.contains("изменены: .agents/skills"),
        "{}",
        o.stdout
    );
    assert!(!o.stdout.contains("deploy-notes  project"), "{}", o.stdout);
    // Проект без журнала (доверия не было) — тоже не доверен, журнал не создаётся.
    let fresh = env_in(GROUP, "list-fresh", "");
    let o = fresh.run(&["skills", "list", "--project", fresh.project_arg()]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(o.stdout.contains("проект не доверен"), "{}", o.stdout);
    assert!(!support::process::journal_exists(&fresh));

    // `[skills] max_entries` (перенос задачи 12): корень больше предела назван, следующий
    // корень `extra` прочитан (m2 ревью задачи 12).
    let root = support::process::unique_dir(GROUP, "list-extra");
    let (big, small) = (root.join("big"), root.join("small"));
    write(&big.join("a/SKILL.md"), &skill_md("big-a", "Use when big."));
    for i in 0..5 {
        write(&big.join(format!("f{i}.txt")), "x");
    }
    write(
        &small.join("b/SKILL.md"),
        &skill_md("small-b", "Use when small."),
    );
    let cfg = format!(
        "[skills]\nmax_entries = 4\ndirs = [{:?}, {:?}]\n",
        big.to_str().unwrap(),
        small.to_str().unwrap()
    );
    let e = env_in(GROUP, "list-limit", &cfg);
    let o = e.run(&["skills", "list"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(o.stdout.contains("small-b  extra"), "{}", o.stdout);
    assert!(!o.stdout.contains("big-a"), "{}", o.stdout);
    assert!(
        o.stdout.contains(&format!(
            "предупреждение: корень {} не прочитан: больше 4 записей (предел [skills] max_entries = 4)",
            big.display()
        )),
        "{}",
        o.stdout
    );
}

// ---- doctor (D111, D122) ----

/// Поддельный SearXNG: `/search` — выдача; `/limited/search` — 429 (`server.limiter`);
/// `/json-off/search` — 403 (формат json не включён).
fn searx(target: &str) -> Response {
    if target.starts_with("/limited/") {
        reply(
            StatusCode::TOO_MANY_REQUESTS,
            "text/plain",
            "Too Many Requests",
        )
    } else if target.starts_with("/json-off/") {
        reply(StatusCode::FORBIDDEN, "text/plain", "Forbidden")
    } else if target.starts_with("/search?") {
        searx_results(&[("Tokio spawn", "https://docs.example/spawn", "Spawns a task")])
    } else {
        reply(StatusCode::NOT_FOUND, "text/plain", "no")
    }
}

/// `doctor` с `[web.search] url` на поддельный экземпляр: «отвечает»; закрытый порт — «не
/// отвечает: <причина>»; без адреса — «поиск не настроен»; 429 и 403 — подсказки. Строки
/// веба, извлекателя и скиллов.
#[test]
fn doctor_reports_search() {
    let sx = FakeHttp::start(searx);
    let doctor = |name: &str, cfg: &str| {
        let e = env_in(GROUP, name, cfg);
        let o = e.run(&["doctor"]);
        assert_eq!(o.code(), Some(0), "{o:?}");
        o.stdout
    };
    let out = doctor(
        "doctor-ok",
        &format!("[web.search]\nurl = \"{}\"\n", sx.base),
    );
    assert!(
        out.contains(&format!("поиск: searxng {}/, отвечает", sx.base)),
        "{out}"
    );
    let probes = sx.requests();
    assert_eq!(probes.len(), 1, "{probes:?}");
    assert!(
        probes[0].starts_with("/search?q=") && probes[0].ends_with("&format=json"),
        "{probes:?}"
    );
    assert!(out.contains("веб включён"), "{out}");
    assert!(out.contains("подпроцесс extract отвечает"), "{out}");
    assert!(out.contains("bundled 14"), "{out}");

    let port = closed_port();
    let out = doctor(
        "doctor-closed",
        &format!("[web.search]\nurl = \"http://127.0.0.1:{port}\"\n"),
    );
    assert!(
        out.contains(&format!(
            "поиск: searxng http://127.0.0.1:{port}/, не отвечает: "
        )),
        "{out}"
    );

    let out = doctor("doctor-none", "");
    assert!(out.contains("поиск не настроен: [web.search] url"), "{out}");

    let out = doctor(
        "doctor-429",
        &format!("[web.search]\nurl = \"{}/limited/\"\n", sx.base),
    );
    assert!(
        out.contains("HTTP 429") && out.contains("server.limiter") && out.contains("pass_ip"),
        "{out}"
    );
    let out = doctor(
        "doctor-403",
        &format!("[web.search]\nurl = \"{}/json-off/\"\n", sx.base),
    );
    assert!(
        out.contains("HTTP 403") && out.contains("search.formats"),
        "{out}"
    );

    let out = doctor("doctor-off", "[web]\nenabled = false\n");
    assert!(out.contains("веб выключен"), "{out}");
}

// ---- Набор инструментов run (D105, D111, D131) ----

/// Без `[web.search] url` в наборе нет `web_search`, `web_fetch` есть; с адресом — оба, и
/// хост поставщика — в `policy.loaded.search_host`. Тексты веб-инструментов входят в хэш
/// промптов run при регистрации (D131): у run с поиском хэш другой.
#[test]
fn search_absent_without_url() {
    let srv = Fake::start(N_CTX, true, answer_only);
    let e = env_in(GROUP, "no-search", &llama_config(&srv.base));
    let o = e.run_cmd(&["Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let names = tool_names(&srv.bodies()[0]);
    assert!(names.iter().any(|n| n == "web_fetch"), "{names:?}");
    assert!(!names.iter().any(|n| n == "web_search"), "{names:?}");
    let plain = e.snapshot(run_id(&o))["frozen"]["texts_hash"].clone();
    let loaded = e.last_audit("policy.").unwrap();
    assert_eq!(loaded.payload["search_host"], Value::Null, "{loaded:?}");

    let cfg = format!(
        "{}\n[web.search]\nurl = \"http://127.0.0.1:{}\"\n",
        llama_config(&srv.base),
        closed_port()
    );
    let e = env_in(GROUP, "with-search", &cfg);
    let o = e.run_cmd(&["Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let names = tool_names(srv.bodies().last().unwrap());
    assert!(names.iter().any(|n| n == "web_search"), "{names:?}");
    assert!(names.iter().any(|n| n == "web_fetch"), "{names:?}");
    let with = e.snapshot(run_id(&o))["frozen"]["texts_hash"].clone();
    assert_ne!(plain, with, "тексты web_search не вошли в хэш промптов");
    let loaded = e.last_audit("policy.").unwrap();
    assert_eq!(loaded.payload["search_host"], "127.0.0.1", "{loaded:?}");
    assert_eq!(loaded.payload["web_disabled"], Value::Null, "{loaded:?}");
}

/// `[web] enabled = false` доверенного проекта: в наборе нет `web_fetch` и `web_search`,
/// `policy.loaded.web_disabled == "project"`; у недоверенного проекта его конфигурация не
/// читается — веб есть. Глобальный `[web] enabled = false` — `"config"`.
#[test]
fn project_disables_web_tools() {
    let srv = Fake::start(N_CTX, true, answer_only);
    let cfg = format!(
        "{}\n[web.search]\nurl = \"http://127.0.0.1:{}\"\n",
        llama_config(&srv.base),
        closed_port()
    );
    let e = env_in(GROUP, "project-off", &cfg);
    write(
        &e.project.join(".zero-agent/config.toml"),
        "[web]\nenabled = false\n",
    );
    // Недоверенный проект: конфигурация проекта не читается.
    let o = e.run_cmd(&["Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let names = tool_names(srv.bodies().last().unwrap());
    assert!(names.iter().any(|n| n == "web_fetch"), "{names:?}");
    let on = e.snapshot(run_id(&o))["frozen"]["texts_hash"].clone();
    e.trust();
    let o = e.run_cmd(&["Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(o.stderr.contains("веб выключен проектом"), "{o:?}");
    let names = tool_names(srv.bodies().last().unwrap());
    assert!(
        !names.iter().any(|n| n == "web_fetch" || n == "web_search"),
        "{names:?}"
    );
    assert_ne!(on, e.snapshot(run_id(&o))["frozen"]["texts_hash"].clone());
    let loaded = e.last_audit("policy.").unwrap();
    assert_eq!(loaded.payload["web_disabled"], "project", "{loaded:?}");
    assert_eq!(loaded.payload["search_host"], Value::Null, "{loaded:?}");

    let e = env_in(
        GROUP,
        "config-off",
        &format!("{}\n[web]\nenabled = false\n", llama_config(&srv.base)),
    );
    let o = e.run_cmd(&["Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let names = tool_names(srv.bodies().last().unwrap());
    assert!(!names.iter().any(|n| n.starts_with("web_")), "{names:?}");
    assert_eq!(
        e.last_audit("policy.").unwrap().payload["web_disabled"],
        "config"
    );
}

/// Модель: поиск, вопрос пользователю (пауза), после ответа — второй поиск, ответ. Решает по
/// числу результатов инструментов в последнем запросе (реплей после паузы её не путает).
fn search_pause_search(seen: &[Value]) -> Response {
    let results = seen.last().unwrap()["messages"]
        .as_array()
        .map_or(0, |m| m.iter().filter(|m| m["role"] == "tool").count());
    match results {
        0 => call_reply("s1", "web_search", "{\"query\":\"tokio spawn\"}"),
        1 => call_reply("q1", "ask_user", "{\"question\":\"Искать дальше?\"}"),
        2 => call_reply("s2", "web_search", "{\"query\":\"tokio join\"}"),
        _ => text_reply(&["готово"]),
    }
}

/// I1 ревью задачи 14: `[web.search] url` сменили во время паузы на другой хост — resume
/// ищет у нового поставщика и пишет новую запись `policy.loaded` с его `search_host` (D105,
/// D126: хост единственного исключения фильтра виден в аудите), хотя хэш политики тот же.
#[test]
fn resume_audits_new_search_host() {
    let a = FakeHttp::start(searx);
    let b = FakeHttp::start(searx);
    let srv = Fake::start(N_CTX, true, search_pause_search);
    let cfg = |url: &str| {
        format!(
            "{}\n[web.search]\nurl = \"{url}\"\n",
            llama_config(&srv.base)
        )
    };
    let e = env_in(GROUP, "resume-search-host", &cfg(&a.base));
    let first = e.run_cmd(&["Найди про tokio"]);
    assert_eq!(first.code(), Some(3), "{first:?}");
    let id = support::process::interrupt_id(&first);
    assert_eq!(e.answer(&id, "да").code(), Some(0));
    std::fs::write(
        e.home.join("config.toml"),
        cfg(&format!("http://search-b.test:{}", b.port)),
    )
    .unwrap();
    let resolve = format!("search-b.test=127.0.0.1:{}", b.port);
    let run = run_id(&first).to_string();
    let r = e.run_cmd(&["--web-resolve", &resolve, "--resume", &run]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(a.requests().len(), 1, "{:?}", a.requests());
    let probes = b.requests();
    assert_eq!(probes.len(), 1, "{probes:?}");
    assert!(probes[0].starts_with("/search?q=tokio+join"), "{probes:?}");
    let loaded = e.audit("policy.loaded");
    assert_eq!(loaded.len(), 2, "{loaded:?}");
    assert_eq!(loaded[0].payload["search_host"], "127.0.0.1", "{loaded:?}");
    assert_eq!(
        loaded[1].payload["search_host"], "search-b.test",
        "{loaded:?}"
    );
    assert_eq!(
        loaded[1].payload["hash"], loaded[0].payload["hash"],
        "{loaded:?}"
    );
}

// ---- Каталог скиллов run (D114, D116) ----

/// Недоверенный проект: в каталоге нет скиллов проекта (`skill.catalog.by_source.project
/// == 0`, описание `activate_skill` без них); после доверия — есть.
#[test]
fn untrusted_project_skills_absent() {
    let srv = Fake::start(N_CTX, true, answer_only);
    let e = env_in(GROUP, "untrusted-skills", &llama_config(&srv.base));
    write(
        &e.project.join(".agents/skills/deploy-notes/SKILL.md"),
        &skill_md("deploy-notes", "Use when deploying the service."),
    );
    let o = e.run_cmd(&["Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let ev = catalog_event(&e, run_id(&o));
    assert_eq!(ev["by_source"]["project"], 0, "{ev}");
    assert_eq!(ev["by_source"]["bundled"], 14, "{ev}");
    let desc = tool_description(srv.bodies().last().unwrap(), "activate_skill").unwrap();
    assert!(!desc.contains("deploy-notes"), "{desc}");
    assert!(desc.contains("<skill_catalog>"), "{desc}");

    e.trust();
    let o = e.run_cmd(&["Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let ev = catalog_event(&e, run_id(&o));
    assert_eq!(ev["by_source"]["project"], 1, "{ev}");
    let desc = tool_description(srv.bodies().last().unwrap(), "activate_skill").unwrap();
    assert!(desc.contains("deploy-notes"), "{desc}");
}

/// Доверие дано, затем правка `SKILL.md`: проект недоверен (D83), скиллов проекта нет.
/// Ветка D116 «tree changed since trust» — защита от гонки между проверкой доверия и
/// обходом: здесь она проверяется прямо, вызовом `project_skills` со старым набором.
#[test]
fn tree_changed_since_trust_drops_project_skills() {
    let srv = Fake::start(N_CTX, true, answer_only);
    let e = env_in(GROUP, "tree-changed", &llama_config(&srv.base));
    let skill = e.project.join(".claude/skills/notes/SKILL.md");
    write(&skill, &skill_md("notes", "Use when taking notes."));
    e.trust();
    let set = zero_agent::trust::trust_set(&e.project).unwrap();
    write(&skill, &skill_md("notes", "Use when taking notes, edited."));
    let o = e.run_cmd(&["Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(o.stderr.contains("проект не доверен"), "{o:?}");
    assert!(o.stderr.contains("изменены: .claude/skills"), "{o:?}");
    let ev = catalog_event(&e, run_id(&o));
    assert_eq!(ev["by_source"]["project"], 0, "{ev}");
    // Ветка гонки: набор доверия старый, дерево уже другое.
    let (project, skipped) = zero_agent::skills::project_skills(&e.project, &set).unwrap();
    assert!(project.is_none());
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert_eq!(skipped[0].path, ".claude/skills");
    assert_eq!(skipped[0].reason, zero_agent::skills::TREE_CHANGED);
    // Тот же набор, что сейчас, — дерево читается.
    let now = zero_agent::trust::trust_set(&e.project).unwrap();
    let (project, skipped) = zero_agent::skills::project_skills(&e.project, &now).unwrap();
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(project.unwrap().trees.len(), 1);
}

/// `[skills] enabled = false`: каталога нет — ни `activate_skill`, ни события
/// `skill.catalog`. (Пустой каталог при включённых скиллах в бинарнике не бывает: поставка
/// есть всегда; что у него нет capability, проверяет модульный тест `run.rs`, m7 ревью
/// задачи 13.)
#[test]
fn skills_disabled_have_no_tool() {
    let srv = Fake::start(N_CTX, true, answer_only);
    let e = env_in(
        GROUP,
        "skills-off",
        &format!("{}\n[skills]\nenabled = false\n", llama_config(&srv.base)),
    );
    let o = e.run_cmd(&["Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let names = tool_names(srv.bodies().last().unwrap());
    assert!(!names.iter().any(|n| n == "activate_skill"), "{names:?}");
    assert!(
        !e.events(run_id(&o))
            .iter()
            .any(|(k, _)| *k == EventKind::SkillCatalog)
    );
}

// ---- Веб-инструменты против поддельных серверов (D111, D112, D126, D128) ----

/// Поддельный сайт: `/robots.txt` — 404 (всё разрешено), `/page` — HTML.
fn site(target: &str) -> Response {
    match target {
        "/page" => reply(
            StatusCode::OK,
            "text/html; charset=utf-8",
            "<html><head><title>Spawn docs</title></head><body><h1>Spawning</h1><p>Use tokio::spawn for tasks.</p><script>evil()</script></body></html>",
        ),
        _ => reply(StatusCode::NOT_FOUND, "text/plain", "no"),
    }
}

/// Адрес страницы из задачи run (`http://docs.test:<порт>/page`).
fn page_url(seen: &[Value]) -> String {
    let first = serde_json::to_string(&seen[0]["messages"]).unwrap();
    let at = first
        .find("http://docs.test:")
        .expect("нет адреса в задаче");
    first[at..]
        .split([' ', '"', '\\'])
        .next()
        .unwrap()
        .to_owned()
}

/// Ход 1 — `web_search`, ход 2 — `web_fetch` страницы, ход 3 — ответ.
fn search_then_fetch(seen: &[Value]) -> Response {
    match seen.len() {
        1 => call_reply("s1", "web_search", "{\"query\":\"tokio spawn\"}"),
        2 => call_reply(
            "f1",
            "web_fetch",
            &format!("{{\"url\":\"{}\"}}", page_url(seen)),
        ),
        _ => text_reply(&["готово"]),
    }
}

/// `web_search` идёт к поддельному SearXNG по `[web.search] url` (исключение D126),
/// `web_fetch` — к поддельному сайту под именем `docs.test` (`--web-resolve`, D128); HTML
/// разбирает подпроцесс `extract html`; каждое обращение — строка stderr (D112).
#[test]
fn web_tools_reach_fake_servers() {
    let sx = FakeHttp::start(searx);
    let page = FakeHttp::start(site);
    let srv = Fake::start(N_CTX, true, search_then_fetch);
    let cfg = format!(
        "{}\n[web]\nports = [{}]\n\n[web.search]\nurl = \"{}\"\n",
        llama_config(&srv.base),
        page.port,
        sx.base
    );
    let e = env_in(GROUP, "web-live", &cfg);
    let url = format!("http://docs.test:{}/page", page.port);
    let resolve = format!("docs.test=127.0.0.1:{}", page.port);
    let o = e.run_cmd(&[
        "--web-resolve",
        &resolve,
        &format!("Найди про tokio spawn и открой {url}"),
    ]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let bodies = srv.bodies();
    assert_eq!(bodies.len(), 3, "{o:?}");
    let second = serde_json::to_string(&bodies[1]["messages"]).unwrap();
    assert!(second.contains("Tokio spawn"), "{second}");
    assert!(second.contains("[untrusted content: web]"), "{second}");
    let third = serde_json::to_string(&bodies[2]["messages"]).unwrap();
    assert!(third.contains("# Spawning"), "{third}");
    assert!(third.contains("Use tokio::spawn for tasks."), "{third}");
    assert!(!third.contains("evil()"), "{third}");
    // Поставщик — один запрос `/search?…&format=json`, `robots.txt` у него не спрашивается.
    let probes = sx.requests();
    assert_eq!(probes.len(), 1, "{probes:?}");
    assert!(probes[0].ends_with("&format=json"), "{probes:?}");
    assert_eq!(page.requests(), ["/robots.txt", "/page"]);
    assert!(
        o.stderr
            .contains(&format!("zero-agent: веб: GET {}/search?q=tokio", sx.base))
            && o.stderr
                .contains(&format!("zero-agent: веб: GET {url} → 200")),
        "{o:?}"
    );
    let reqs = e
        .events(run_id(&o))
        .into_iter()
        .filter(|(k, _)| *k == EventKind::WebRequest)
        .count();
    assert_eq!(reqs, 3, "поиск, robots.txt, страница");
}

/// D169: `--tier standard` на окне меньше 32k — предупреждение на старте run (run идёт);
/// тир по окну (small) и окно 32k — без него. `doctor` с `[context] tier = "standard"` на
/// таком окне — строка `window` с тем же предупреждением.
#[test]
fn standard_below_32k_warns_at_start() {
    let srv = Fake::start(16_384, true, answer_only);
    let e = env_in(GROUP, "window-16k", &llama_config(&srv.base));
    let o = e.run_cmd(&["--tier", "standard", "Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(
        o.stderr
            .contains("zero-agent: тир standard выбран на окне 16384 токенов, меньше 32768"),
        "{o:?}"
    );
    let o = e.run_cmd(&["Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(!o.stderr.contains("выбран на окне"), "{o:?}");
    let e = env_in(
        GROUP,
        "window-doctor",
        &format!(
            "{}\n[context]\ntier = \"standard\"\n",
            llama_config(&srv.base)
        ),
    );
    let o = e.run(&["doctor"]);
    assert!(
        o.stdout
            .contains("local: тир standard выбран на окне 16384"),
        "{}",
        o.stdout
    );
    let big = Fake::start(N_CTX, true, answer_only);
    let e = env_in(GROUP, "window-32k", &llama_config(&big.base));
    let o = e.run_cmd(&["--tier", "standard", "Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(!o.stderr.contains("выбран на окне"), "{o:?}");
}

/// Модель: шаг 1 — `glob` (чтение), затем ответ.
fn glob_then_answer(seen: &[Value]) -> Response {
    let called = seen.last().unwrap()["messages"]
        .as_array()
        .is_some_and(|m| m.iter().any(|m| m["role"] == "tool"));
    if called {
        text_reply(&["готово"])
    } else {
        call_reply("g1", "glob", "{\"pattern\":\"*.md\"}")
    }
}

/// m4 ревью задачи 14: run начат с тиром standard на окне 32k и встал на паузу бюджета на
/// границе шага (`--max-steps 1`); resume `--model` на модель с окном 16k — то же
/// предупреждение D169 (тир заморожен в run), и run продолжается у новой модели. Смена модели
/// возможна только на границе шага (`ChangeMidStep`), поэтому пауза — бюджет, а не вопрос.
#[test]
fn resume_with_smaller_model_warns() {
    let big = Fake::start(N_CTX, true, glob_then_answer);
    let small = Fake::start(16_384, true, glob_then_answer);
    let cfg = format!(
        "[models]
default = \"big\"

[[models.endpoints]]
id = \"big\"
backend = \"llama_cpp\"
base_url = \"{}\"

[[models.endpoints]]
id = \"small\"
backend = \"llama_cpp\"
base_url = \"{}\"
",
        big.base, small.base
    );
    let e = env_in(GROUP, "resume-small-model", &cfg);
    let first = e.run_cmd(&["--max-steps", "1", "Вопрос"]);
    assert_eq!(first.code(), Some(4), "{first:?}");
    assert!(!first.stderr.contains("выбран на окне"), "{first:?}");
    let run = run_id(&first).to_string();
    let r = e.run_cmd(&["--resume", &run, "--model", "small", "--max-steps", "5"]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert!(
        r.stderr
            .contains("zero-agent: тир standard выбран на окне 16384 токенов, меньше 32768"),
        "{r:?}"
    );
    assert_eq!(small.bodies().len(), 1, "resume не дошёл до модели small");
}

// ---- Бюджет описаний standard (D117, D169) ----

/// Извлекатель для описаний: не вызывается.
struct NoExtractor;

impl za_core::extract::Extractor for NoExtractor {
    fn html(
        &self,
        _req: za_core::extract::HtmlRequest,
    ) -> za_core::BoxFuture<
        'static,
        Result<za_core::extract::Markdown, za_core::extract::ExtractError>,
    > {
        Box::pin(async { Err(za_core::extract::ExtractError::Failed("not used".into())) })
    }

    fn pdf(
        &self,
        _req: za_core::extract::PdfRequest,
    ) -> za_core::BoxFuture<
        'static,
        Result<za_core::extract::PdfText, za_core::extract::ExtractError>,
    > {
        Box::pin(async { Err(za_core::extract::ExtractError::Failed("not used".into())) })
    }
}

/// Настоящий набор top standard (нативные инструменты, встроенные, веб из `zero_agent::web`
/// с поиском и каталог core pack) по частям D169 (Ruling m1 ревью задачи 14): описания без
/// каталога на 16k и 32k — не больше 1700 токенов (`STANDARD_TOOLS_LIMIT`), каталог — не
/// больше своей колонки окна `ContextBudget::catalog` (300, 600, 1500 на 16k, 32k, 64k).
/// Тест здесь: `za-agent` от `za-tools` и `za-web` не зависит.
#[test]
fn standard_tools_fit_slot_with_web_and_catalog() {
    let native = za_tools::native_capability(&za_tools::NativeDeps {
        runner: Arc::new(za_jail::DirectRunner),
        tracker: za_tools::ReadTracker::new(),
        shell: za_tools::Shell {
            program: "no-shell".into(),
            dialect: za_core::shell::Dialect::Bash,
        },
        home: za_tools::AgentHome {
            za_home: "no-home".into(),
            project_id: "budget".into(),
        },
        mask: None,
        extractor: Some(Arc::new(NoExtractor)),
    });
    let cfg = zero_agent::config::parse_config("[web.search]\nurl = \"http://127.0.0.1:8888\"\n")
        .unwrap();
    let gw = Arc::new(za_web::WebGateway::new(zero_agent::web::web_config(&cfg.web)).unwrap());
    let mut tools = native.tools.clone();
    tools.extend(zero_agent::web::web_tools(&gw, Arc::new(NoExtractor)));
    let catalog = za_skills::discover(&za_skills::DiscoverInput {
        bundled: za_skills::bundled(),
        ..za_skills::DiscoverInput::default()
    });
    assert_eq!(catalog.skills.len(), za_skills::PACK.len());
    let caps = [za_agent::skills_capability(Arc::new(catalog))];
    let cases = za_agent::standard_tools_cases(&tools, &caps).unwrap();
    let want = [
        ("top/standard/tools@16k", 1700),
        ("top/standard/catalog@16k", 300),
        ("top/standard/tools@32k", 1700),
        ("top/standard/catalog@32k", 600),
        ("top/standard/catalog@64k", 1500),
    ];
    let names: Vec<&str> = cases.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, want.map(|(n, _)| n));
    assert_eq!(za_agent::STANDARD_TOOLS_LIMIT, 1700);
    for (case, (name, limit)) in cases.iter().zip(want) {
        assert_eq!(case.limit, limit, "{name}: предел D169");
        let tokens = za_agent::approx_tokens(&case.text);
        assert!(tokens <= limit, "{name}: {tokens} > {limit}");
        if name.contains("/tools@") {
            for tool in ["web_fetch", "web_search", "activate_skill", "read", "glob"] {
                assert!(case.text.contains(tool), "{name}: нет {tool}");
            }
            assert!(!case.text.contains("<skill_catalog>"), "{name}");
        } else {
            assert!(
                case.text.starts_with("<skill_catalog>") && case.text.ends_with("</skill_catalog>"),
                "{name}: {}",
                case.text
            );
            for skill in za_skills::PACK {
                assert!(case.text.contains(skill), "{name}: нет {skill}");
            }
        }
        println!("{name}: {tokens}/{limit}");
    }
}
