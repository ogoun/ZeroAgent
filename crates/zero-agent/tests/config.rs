//! Чтение `ZA_HOME/config.toml` и запасной эндпоинт из `LLAMA_URL`; проектная
//! конфигурация, режим `bypass` и флаг `--grant` команды `run` (R1-C1 D78, D80).

use std::path::PathBuf;

use za_llm::{BackendKind, EndpointId};
use zero_agent::config::{AgentConfig, CONFIG_FILE, load_config_with};

mod support;
use support::process::unique_dir;

fn home(name: &str) -> PathBuf {
    unique_dir("config", name)
}

#[test]
fn missing_file_is_empty_config() {
    let cfg = load_config_with(&home("missing"), None).unwrap();
    assert_eq!(cfg, AgentConfig::default());
}

#[test]
fn llama_url_without_models_gives_local() {
    let cfg = load_config_with(&home("llama_url"), Some("http://127.0.0.1:8080")).unwrap();
    let m = cfg.models.unwrap();
    assert_eq!(m.default, Some(EndpointId("local".into())));
    assert_eq!(m.endpoints.len(), 1);
    assert_eq!(m.endpoints[0].backend, BackendKind::LlamaCpp);
}

#[test]
fn models_section_wins_over_llama_url() {
    let h = home("models_wins");
    std::fs::write(
        h.join(CONFIG_FILE),
        "[models]\n[[models.endpoints]]\nid = \"a\"\nbackend = \"llama_cpp\"\nbase_url = \"http://127.0.0.1:1\"\n",
    )
    .unwrap();
    let cfg = load_config_with(&h, Some("http://127.0.0.1:8080")).unwrap();
    let m = cfg.models.unwrap();
    assert_eq!(m.endpoints[0].id, EndpointId("a".into()));
    assert_eq!(m.default, None);
}

#[test]
fn resources_section_alone_keeps_llama_url_fallback() {
    let h = home("resources_only");
    std::fs::write(h.join(CONFIG_FILE), "[resources]\nvram_floor_mib = 512\n").unwrap();
    let cfg = load_config_with(&h, Some("http://127.0.0.1:8080")).unwrap();
    assert_eq!(cfg.resources.vram_floor_mib, 512);
    assert_eq!(cfg.resources.ram_floor_mib, 512);
    assert!(cfg.models.is_some());
}

#[test]
fn readme_models_example_is_valid() {
    // Пример раздела «Модели» в README проходит разбор и проверку (финальное ревью I2:
    // пример с бюджетом без `price` не действовал).
    let readme = include_str!("../../../README.md");
    let start = readme
        .find(
            "```toml
[models]",
        )
        .expect("пример [models] в README");
    let body = &readme[start
        + "```toml
"
        .len()..];
    let end = body.find("```").expect("конец примера");
    let cfg = zero_agent::config::parse_config(&body[..end]).unwrap();
    zero_agent::config::validate(&cfg).unwrap();
    assert!(
        cfg.models
            .unwrap()
            .endpoints
            .iter()
            .any(|e| e.daily_budget_usd.is_some())
    );
}

#[test]
fn budgets_section_parsed() {
    let cfg =
        zero_agent::config::parse_config("[budgets.run]\ntokens = 1000\nsteps = 20\n").unwrap();
    assert_eq!(
        cfg.budgets.run,
        za_protocol::Budget {
            tokens: Some(1000),
            steps: Some(20),
            ..Default::default()
        }
    );
    // Опечатка в имени предела — ошибка разбора, а не молча снятый предел; текст
    // ошибки без строки исходника (SEC-044).
    const SRC: &str = "[budgets.run]\nfoo = 1\n";
    let e = zero_agent::config::parse_config(SRC).unwrap_err();
    assert!(
        matches!(e, zero_agent::config::AppConfigError::Parse(_)),
        "{e}"
    );
    let shown = format!("{e} {e:?}");
    assert!(!shown.contains("foo = 1"), "{shown}");
    assert!(!shown.contains("foo"), "{shown}");
}

#[test]
fn readme_budgets_example_is_valid() {
    // Пример раздела `[budgets.run]` в README проходит разбор.
    let readme = include_str!("../../../README.md");
    let start = readme
        .find("```toml\n[budgets.run]")
        .expect("пример [budgets.run] в README");
    let body = &readme[start + "```toml\n".len()..];
    let end = body.find("```").expect("конец примера");
    let cfg = zero_agent::config::parse_config(&body[..end]).unwrap();
    zero_agent::config::validate(&cfg).unwrap();
    assert_ne!(cfg.budgets.run, za_protocol::Budget::default());
}

#[test]
fn readme_context_example_is_valid() {
    // Пример раздела `[context]` в README проходит разбор и проверку диапазонов.
    let readme = include_str!("../../../README.md");
    let start = readme
        .find("```toml\n[context]")
        .expect("пример [context] в README");
    let body = &readme[start + "```toml\n".len()..];
    let end = body.find("```").expect("конец примера");
    let cfg = zero_agent::config::parse_config(&body[..end]).unwrap();
    zero_agent::config::validate(&cfg).unwrap();
    assert_eq!(cfg.context.tier, Some(za_protocol::Tier::Standard));
}

/// R1-B2 D65: раздел `[context]` — значения, диапазоны и опечатка ключа.
#[test]
fn context_section_is_parsed_and_validated() {
    use za_protocol::Tier;
    use zero_agent::config::AppConfigError;

    let h = home("context_ok");
    std::fs::write(
        h.join(CONFIG_FILE),
        "[context]\ntier = \"standard\"\ncompact_at = 0.7\nprune_at = 0.6\nspill_share = 0.2\n",
    )
    .unwrap();
    let cfg = load_config_with(&h, None).unwrap();
    assert_eq!(cfg.context.tier, Some(Tier::Standard));
    assert_eq!(cfg.context.compact_at, 0.7);
    assert_eq!(cfg.context.prune_at, 0.6);
    assert_eq!(cfg.context.spill_share, 0.2);

    // Без раздела — значения по умолчанию.
    let cfg = load_config_with(&home("context_default"), None).unwrap();
    assert_eq!(cfg.context, za_agent::ContextConfig::default());

    // Вне диапазона — ошибка конфигурации с именем поля.
    let h = home("context_range");
    std::fs::write(h.join(CONFIG_FILE), "[context]\ncompact_at = 0.85\n").unwrap();
    let e = load_config_with(&h, None).unwrap_err();
    assert!(matches!(e, AppConfigError::Context(_)), "{e}");
    assert!(e.to_string().contains("context.compact_at"), "{e}");

    // Опечатка ключа — ошибка разбора (`deny_unknown_fields`).
    let h = home("context_typo");
    std::fs::write(h.join(CONFIG_FILE), "[context]\ncompact_after = 0.75\n").unwrap();
    let e = load_config_with(&h, None).unwrap_err();
    assert!(matches!(e, AppConfigError::Parse(_)), "{e}");
}

// ---- Проектная конфигурация, bypass и гранты (R1-C1 задача 13) ----

/// D80, ARC-041: проектная конфигурация — только `[policy]`; правила разбираются тем же
/// разбором, прочие разделы не действуют (без ошибки), ошибка называет файл проекта и
/// путь поля.
#[test]
fn project_config_policy_only() {
    let cfg = zero_agent::config::parse_project_config(
        "[models]\ndefault = \"x\"\n\n[policy]\nallow_bypass = true\n\n[[policy.rules]]\naction = \"deny\"\ntool = \"bash\"\n\n[[policy.rules]]\naction = \"allow\"\ncommand = \"rm *\"\n\n[[policy.grants]]\npath = \"/etc\"\nmode = \"rw\"\n",
    )
    .unwrap();
    assert_eq!(cfg.policy.rules.len(), 2);
    // Снимок run: проектное allow, allow_bypass и гранты не действуют.
    let snap =
        za_agent::PolicySnapshot::build(&za_agent::PolicyConfig::default(), Some(&cfg.policy))
            .unwrap();
    assert_eq!(snap.rules.len(), 1);
    assert_eq!(snap.ignored.len(), 1);
    assert!(!snap.allow_bypass);
    let e = zero_agent::config::parse_project_config(
        "[[policy.rules]]\naction = \"deny\"\ntol = \"bash\"\n",
    )
    .unwrap_err();
    let shown = e.to_string();
    assert!(
        shown.starts_with(".zero-agent/config.toml проекта"),
        "{shown}"
    );
    assert!(shown.contains("policy.rules[0]"), "{shown}");
    let e = zero_agent::config::parse_project_config("[[policy.rules]]\naction = \"deny\"\n")
        .unwrap_err();
    assert!(e.to_string().contains("tool, path или command"), "{e}");
}

/// D80: `run --mode bypass` без `[policy] allow_bypass = true` глобальной конфигурации —
/// ошибка конфигурации, код 1, текст `PolicyError::BypassDisabled`; с флагом режим
/// принимается (дальше — обычная проверка моделей).
#[test]
fn bypass_requires_global_flag() {
    let e = support::process::env_in("config-run", "bypass", "");
    let o = e.run_cmd(&["--mode", "bypass", "Задача"]);
    assert_eq!(o.code(), Some(1), "{o:?}");
    let want = za_agent::PolicyError::BypassDisabled.to_string();
    assert!(o.stderr.contains(&want), "{o:?}");
    let e = support::process::env_in("config-run", "bypass-on", "[policy]\nallow_bypass = true\n");
    let o = e.run_cmd(&["--mode", "bypass", "Задача"]);
    assert_eq!(o.code(), Some(1), "{o:?}");
    assert!(!o.stderr.contains(&want), "{o:?}");
}

/// Ход 1 — `read ../data/info.txt` (вне проекта, по гранту), ход 2 — `ask_user` (пауза),
/// далее — «готово».
fn read_outside(seen: &[serde_json::Value]) -> axum::response::Response {
    match seen.len() {
        1 => support::fake_llama::call_reply("call_1", "read", "{\"path\":\"../data/info.txt\"}"),
        2 => support::fake_llama::call_reply("call_2", "ask_user", "{\"question\":\"дальше?\"}"),
        _ => support::fake_llama::text_reply(&["готово"]),
    }
}

/// События `grant.added` run: (источник, режим).
fn grant_events(e: &support::process::Env, run: za_protocol::RunId) -> Vec<(String, String)> {
    e.events(run)
        .into_iter()
        .filter(|(k, _)| *k == za_protocol::EventKind::GrantAdded)
        .map(|(_, p)| {
            (
                p["source"].as_str().unwrap().to_owned(),
                p["mode"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// D78: `--grant <путь>=rw` даёт грант run с каноническим путём и режимом rw
/// (`RunSpec.grants` → снимок), путь из глобальной конфигурации — тоже грант (`config`);
/// проектный `[[policy.grants]]` не действует (ARC-041). Каждый грант — событие
/// `grant.added` журнала run и запись аудита (I2 ревью задачи 13), в том числе новый
/// грант при resume. Исполненный вызов вне проекта печатается (D96). Сетевой путь —
/// ошибка, код 1.
#[test]
fn grant_flag_reaches_run() {
    let srv = support::fake_llama::Fake::start(32768, true, read_outside);
    let root = unique_dir("config-run", "grant-data");
    let shared = root.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    let cfg = format!(
        "{}\n[[policy.grants]]\npath = {:?}\nreason = \"общий каталог\"\n",
        support::fake_llama::llama_config(&srv.base),
        shared.to_str().unwrap()
    );
    let e = support::process::env_in("config-run", "grant", &cfg);
    let data = e.project.parent().unwrap().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("info.txt"), "outside-data").unwrap();
    std::fs::create_dir_all(e.project.join(".zero-agent")).unwrap();
    std::fs::write(
        e.project.join(".zero-agent/config.toml"),
        format!(
            "[[policy.grants]]\npath = {:?}\nmode = \"rw\"\n",
            root.to_str().unwrap()
        ),
    )
    .unwrap();
    e.trust();
    let arg = format!("{}=rw", data.display());
    let o = e.run_cmd(&[
        "--mode",
        "auto",
        "--tier",
        "standard",
        "--grant",
        &arg,
        "Вопрос",
    ]);
    assert_eq!(o.code(), Some(3), "{o:?}");
    // D96: исполненный вызов вне проекта — строка в stderr; результат дошёл до модели.
    assert!(
        o.stderr.contains("zero-agent: вне проекта: read read ") && o.stderr.contains("info.txt"),
        "{o:?}"
    );
    let second = serde_json::to_string(&srv.bodies()[1]["messages"]).unwrap();
    assert!(second.contains("outside-data"), "{second}");
    let run = support::process::run_id(&o);
    let snap = e.snapshot(run);
    let grants = snap["grants"].as_array().unwrap();
    assert_eq!(grants.len(), 2, "проектный грант действует: {snap}");
    assert_eq!(grants[0]["mode"], "rw", "{snap}");
    assert_eq!(grants[0]["source"], "cli", "{snap}");
    let canon = std::fs::canonicalize(&data).unwrap();
    let canon = canon.to_string_lossy();
    let canon = canon.strip_prefix(r"\\?\").unwrap_or(&canon);
    assert_eq!(grants[0]["path"], canon, "{snap}");
    assert_eq!(grants[1]["mode"], "ro", "{snap}");
    assert_eq!(grants[1]["source"], "config", "{snap}");
    // Первое сообщение top — блок <grants> с обоими путями.
    let first = serde_json::to_string(&srv.bodies()[0]["messages"]).unwrap();
    assert!(first.contains("<grants>"), "{first}");
    assert!(first.contains("(rw)"), "{first}");
    // I2: событие журнала и аудит у каждого источника.
    let pair = |s: &str, m: &str| (s.to_owned(), m.to_owned());
    assert_eq!(
        grant_events(&e, run),
        [pair("cli", "rw"), pair("config", "ro")]
    );
    let audits = |e: &support::process::Env| -> Vec<(String, String)> {
        e.audit("grant.added")
            .iter()
            .map(|a| {
                (
                    a.actor.clone(),
                    a.payload["source"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    assert_eq!(audits(&e), [pair("user", "cli"), pair("runtime", "config")]);
    // Resume с новым `--grant`: событие и аудит только нового гранта.
    let extra = root.join("extra");
    std::fs::create_dir_all(&extra).unwrap();
    let int = support::process::interrupt_id(&o);
    let a = e.answer(&int, "да");
    assert_eq!(a.code(), Some(0), "{a:?}");
    let r = e.run_cmd(&[
        "--resume",
        &run.to_string(),
        "--grant",
        extra.to_str().unwrap(),
    ]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(
        grant_events(&e, run),
        [pair("cli", "rw"), pair("config", "ro"), pair("cli", "ro")]
    );
    assert_eq!(
        audits(&e),
        [
            pair("user", "cli"),
            pair("runtime", "config"),
            pair("user", "cli")
        ]
    );
    assert_eq!(e.snapshot(run)["grants"].as_array().unwrap().len(), 3);
    if cfg!(windows) {
        let o = e.run_cmd(&["--grant", r"\\192.0.2.1\share=ro", "Вопрос"]);
        assert_eq!(o.code(), Some(1), "{o:?}");
        assert!(o.stderr.contains("грант"), "{o:?}");
    }
}
