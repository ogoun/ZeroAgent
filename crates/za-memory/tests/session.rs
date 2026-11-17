//! Сессия run (R1-M D139): id `za-<project_id>-<run>`, регистрация `POST /api/v1/sessions`
//! строгим телом один раз на run — на поддельном ZAM (`test-support/fake_zam.rs`, D154).

#[path = "../../../test-support/fake_zam.rs"]
mod fake_zam;
mod support;

use fake_zam::{FakeZam, WAIT, within};
use serde_json::json;
use za_core::memory::MemoryPort;
use za_memory::{IdeasMode, session_id};
use za_protocol::RunId;

use support::{AREA, PROJECT, SOLUTION, port_at};

#[test]
fn session_id_is_project_and_run() {
    let run = RunId::new();
    let id = session_id(PROJECT, run);
    assert_eq!(id, format!("za-{PROJECT}-{run}"));
    // 3 + 16 + 1 + 36 символов: в пределе id сессии ZAM (128).
    assert_eq!(id.len(), 56);
}

#[tokio::test]
async fn session_registered_once_per_run() {
    let mut fake = FakeZam::start().await;
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    fake.seen();
    let run = RunId::new();
    within(WAIT, "snapshot", mem.snapshot(run, 200)).await;
    within(
        WAIT,
        "prompt",
        mem.prompt_hint(run, "синтетический ввод", 100),
    )
    .await;
    within(
        WAIT,
        "tool_error",
        mem.tool_error_hint(run, "bash", "exit 1", 100),
    )
    .await;
    let seen = fake.seen();
    let routes: Vec<&str> = seen.iter().map(|s| s.route()).collect();
    assert_eq!(
        routes,
        [
            "/api/v1/sessions",
            "/api/v1/recall",
            "/api/v1/recall",
            "/api/v1/recall"
        ],
        "{seen:?}"
    );
    assert_eq!(fake.sessions().len(), 1);
    // Новый run — своя сессия.
    let other = RunId::new();
    within(WAIT, "snapshot 2", mem.snapshot(other, 200)).await;
    let seen = fake.seen();
    assert_eq!(seen[0].route(), "/api/v1/sessions");
    assert_eq!(seen[0].body["id"], json!(session_id(PROJECT, other)));
}

#[tokio::test]
async fn session_body_is_strict_and_attributed() {
    let mut fake = FakeZam::start().await;
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    fake.seen();
    let run = RunId::new();
    within(WAIT, "snapshot", mem.snapshot(run, 200)).await;
    let seen = fake.seen();
    let reg = &seen[0];
    assert_eq!(reg.method, "POST");
    assert_eq!(reg.route(), "/api/v1/sessions");
    let id = session_id(PROJECT, run);
    // Ровно поля регистрации (D139): без `new_epoch`, `holdout`, `tokens`, `parent_id`.
    assert_eq!(
        reg.body,
        json!({
            "id": id,
            "agent": "zero-agent",
            "solution": SOLUTION,
            "area": AREA,
            "agent_type": "top",
        })
    );
    // Сервер принял тело (строгий разбор поддельного ZAM, как `deny_unknown_fields`).
    assert_eq!(fake.sessions().len(), 1);
    assert_eq!(
        reg.header_str("x-zam-session").as_deref(),
        Some(id.as_str())
    );
    assert_eq!(reg.header_str("x-zam-solution").as_deref(), Some(SOLUTION));
    assert_eq!(reg.header("x-zam-area"), Some(AREA.as_bytes()));
}

#[tokio::test]
async fn failed_registration_is_not_repeated_but_recall_goes_on() {
    let mut fake = FakeZam::start().await;
    fake.fail("/api/v1/sessions", 500, "about:blank", None);
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    fake.seen();
    let run = RunId::new();
    within(WAIT, "snapshot", mem.snapshot(run, 200)).await;
    within(WAIT, "prompt", mem.prompt_hint(run, "ввод", 100)).await;
    // Ответ со статусом — сервер жив: сессию создаст первый запрос с `X-Zam-Session`;
    // регистрация не повторяется на каждом вызове.
    let routes: Vec<String> = fake.seen().iter().map(|s| s.route().to_owned()).collect();
    assert_eq!(
        routes,
        ["/api/v1/sessions", "/api/v1/recall", "/api/v1/recall"],
    );
}
