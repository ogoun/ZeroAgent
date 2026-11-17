//! Общее для тестов порта памяти (`tests/{session,recall}.rs`): привязка run и сборка
//! `ZamMemory` к поддельному ZAM на `127.0.0.1`.

#![allow(dead_code)]

use std::sync::Arc;

use za_memory::discovery::from_health;
use za_memory::{
    Clock, Discovery, MemoryConfig, RunBinding, TokioClock, ZamClient, ZamMemory, ZamParts,
    discover,
};
use za_secrets::SecretMasker;

/// `project_id` проекта: 16 hex (`zero-agent/src/home.rs`).
pub const PROJECT: &str = "0123456789abcdef";
/// GUID решения проекта.
pub const SOLUTION: &str = "6f1c2b3a-0d4e-4f5a-8b6c-7d8e9f0a1b2c";
/// Область проекта (не ASCII — байтами UTF-8 в заголовке).
pub const AREA: &str = "модуль-ядро";

pub fn binding() -> RunBinding {
    RunBinding {
        project_id: PROJECT.to_owned(),
        solution: Some(SOLUTION.to_owned()),
        area: Some(AREA.to_owned()),
        writable: true,
    }
}

/// Обнаружение L2 формы `0.0.11-m6` без запроса (для недоступного сервера).
pub fn discovery_m6() -> Discovery {
    from_health(&serde_json::json!({
        "status": "ok",
        "version": "0.0.11-m6",
        "features": ["m6", "recall_timeout", "epochs"],
        "token_counter": "model",
    }))
}

/// Порт памяти с заданными частями; токена нет (поддельный сервер его не требует).
pub fn port(
    url: &str,
    discovery: Discovery,
    clock: Arc<dyn Clock>,
    masker: SecretMasker,
) -> ZamMemory {
    let client = ZamClient::new(&MemoryConfig::new(url), None).unwrap();
    ZamMemory::new(ZamParts {
        client: Arc::new(client),
        discovery,
        binding: binding(),
        masker: Arc::new(masker),
        store: None,
        clock,
    })
}

/// Порт к серверу `url` с настоящим обнаружением (`/api/v1/health`), часы tokio.
pub async fn port_at(url: &str) -> ZamMemory {
    let client = ZamClient::new(&MemoryConfig::new(url), None).unwrap();
    let d = discover(&client).await;
    port(url, d, Arc::new(TokioClock), SecretMasker::default())
}
