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
