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
