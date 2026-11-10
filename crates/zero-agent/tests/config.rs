//! Чтение `ZA_HOME/config.toml` и запасной эндпоинт из `LLAMA_URL`.

use std::path::PathBuf;

use za_llm::{BackendKind, EndpointId};
use zero_agent::config::{AgentConfig, CONFIG_FILE, load_config_with};

fn home(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("config")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
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
