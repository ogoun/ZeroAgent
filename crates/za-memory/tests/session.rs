//! Сессия run (R1-M D139): id `za-<project_id>-<run>`, регистрация `POST /api/v1/sessions`
//! строгим телом до успеха или отказа без повтора — на поддельном ZAM
//! (`test-support/fake_zam.rs`, D154).

#[path = "../../../test-support/fake_zam.rs"]
mod fake_zam;
mod support;

use fake_zam::{FakeZam, WAIT, within};
use serde_json::json;
use za_core::memory::MemoryPort;
use za_memory::session::register;
use za_memory::{Ctx, ErrorKind, MemoryConfig, ZamClient, session_id};
use za_protocol::RunId;

use support::{AREA, PROJECT, SOLUTION, binding, port_at};

#[test]
fn session_id_is_project_and_run() {
    let run = RunId::new();
    let id = session_id(PROJECT, run);
    assert_eq!(id, format!("za-{PROJECT}-{run}"));
    // 3 + 16 + 1 + 36 символов: в пределе id сессии ZAM (128).
    assert_eq!(id.len(), 56);
}

/// Minor 8 ревью задачи 4: `register` берёт id явно — он идёт и в тело, и в `X-Zam-Session`;
/// пустой id — ошибка до сети.
#[tokio::test]
async fn register_takes_explicit_id() {
    let mut fake = FakeZam::start().await;
    let client = ZamClient::new(&MemoryConfig::new(&fake.url()), None).unwrap();
    let cx = Ctx {
        session: None,
        solution: Some(SOLUTION),
        area: None,
    };
    let e = within(WAIT, "empty", register(&client, "", &binding(), &cx))
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::BadRequest);
    assert!(fake.seen().is_empty(), "запрос с пустым id ушёл в сеть");
    within(
        WAIT,
        "register",
        register(&client, "za-synthetic-1", &binding(), &cx),
    )
    .await
    .unwrap();
    let seen = fake.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].body["id"], "za-synthetic-1");
    assert_eq!(
        seen[0].header_str("x-zam-session").as_deref(),
        Some("za-synthetic-1")
    );
    assert_eq!(
        seen[0].header_str("x-zam-solution").as_deref(),
        Some(SOLUTION)
    );
}

#[tokio::test]
async fn session_registered_once_per_run() {
    let mut fake = FakeZam::start().await;
    let mem = port_at(&fake.url()).await;
    fake.seen();
    let run = RunId::new();
    within(WAIT, "snapshot", mem.snapshot(run, 200, None)).await;
    within(
        WAIT,
        "prompt",
        mem.prompt_hint(run, "синтетический ввод", 100, None),
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
    within(WAIT, "snapshot 2", mem.snapshot(other, 200, None)).await;
    let seen = fake.seen();
    assert_eq!(seen[0].route(), "/api/v1/sessions");
    assert_eq!(seen[0].body["id"], json!(session_id(PROJECT, other)));
}

#[tokio::test]
async fn session_body_is_strict_and_attributed() {
    let mut fake = FakeZam::start().await;
    let mem = port_at(&fake.url()).await;
    fake.seen();
    let run = RunId::new();
    within(WAIT, "snapshot", mem.snapshot(run, 200, None)).await;
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

fn routes(fake: &mut FakeZam) -> Vec<String> {
    fake.seen().iter().map(|s| s.route().to_owned()).collect()
}

/// I2 ревью задачи 4: временная ошибка регистрации (503) не блокирует `/recall` — сервер
/// создаёт сессию неявно по `X-Zam-Session` (`api/sessions.rs:197-209@v0.0.12-m6.5`), без
/// регистрации теряются только метаданные; регистрация повторяется при следующем вызове порта.
#[tokio::test]
async fn temporary_registration_error_is_retried() {
    let mut fake = FakeZam::start().await;
    let mem = port_at(&fake.url()).await;
    fake.fail("/api/v1/sessions", 503, "about:blank", None);
    fake.seen();
    let run = RunId::new();
    within(WAIT, "snapshot", mem.snapshot(run, 200, None)).await;
    assert_eq!(routes(&mut fake), ["/api/v1/sessions", "/api/v1/recall"]);
    assert!(fake.sessions().is_empty());
    fake.clear_fail("/api/v1/sessions");
    within(WAIT, "prompt", mem.prompt_hint(run, "ввод", 100, None)).await;
    assert_eq!(routes(&mut fake), ["/api/v1/sessions", "/api/v1/recall"]);
    assert_eq!(fake.sessions().len(), 1);
    // Зарегистрирована — больше не повторяется.
    within(WAIT, "prompt 2", mem.prompt_hint(run, "ввод", 100, None)).await;
    assert_eq!(routes(&mut fake), ["/api/v1/recall"]);
}

/// I2: постоянный отказ (4xx, здесь 422) — без повтора; предупреждение — в отчёте порта
/// (данные события `memory.recall`), `/recall` идёт.
#[tokio::test]
async fn refused_registration_is_not_repeated_and_reported() {
    let mut fake = FakeZam::start().await;
    let mem = port_at(&fake.url()).await;
    fake.fail("/api/v1/sessions", 422, "about:blank", None);
    fake.seen();
    let run = RunId::new();
    within(WAIT, "snapshot", mem.snapshot(run, 200, None)).await;
    within(WAIT, "prompt", mem.prompt_hint(run, "ввод", 100, None)).await;
    assert_eq!(
        routes(&mut fake),
        ["/api/v1/sessions", "/api/v1/recall", "/api/v1/recall"]
    );
    let reports = mem.drain_reports();
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert!(
        reports[0]
            .warnings
            .iter()
            .any(|w| w == "session not registered: 422 unprocessable"),
        "{reports:?}"
    );
    assert!(reports[1].warnings.is_empty(), "{reports:?}");
}

/// I2: 429 с `Retry-After` — регистрация повторяется не раньше срока (часы `Clock`).
#[cfg(feature = "test-hooks")]
#[tokio::test]
async fn rate_limited_registration_waits_retry_after() {
    use std::sync::Arc;
    use std::time::Duration;

    use za_memory::ManualClock;
    use za_secrets::SecretMasker;

    let mut fake = FakeZam::start().await;
    let client =
        za_memory::ZamClient::new(&za_memory::MemoryConfig::new(&fake.url()), None).unwrap();
    let d = za_memory::discover(&client).await;
    let clock = Arc::new(ManualClock::new());
    let mem = support::port(&fake.url(), d, clock.clone(), SecretMasker::default());
    fake.fail("/api/v1/sessions", 429, "/problems/rate-limited", Some(5));
    fake.seen();
    let run = RunId::new();
    within(WAIT, "snapshot", mem.snapshot(run, 200, None)).await;
    assert_eq!(routes(&mut fake), ["/api/v1/sessions", "/api/v1/recall"]);
    fake.clear_fail("/api/v1/sessions");
    // Срок `Retry-After` не прошёл — регистрации нет, `/recall` идёт.
    clock.advance(Duration::from_secs(4));
    within(WAIT, "prompt 4 s", mem.prompt_hint(run, "ввод", 100, None)).await;
    assert_eq!(routes(&mut fake), ["/api/v1/recall"]);
    clock.advance(Duration::from_secs(1));
    within(WAIT, "prompt 5 s", mem.prompt_hint(run, "ввод", 100, None)).await;
    assert_eq!(routes(&mut fake), ["/api/v1/sessions", "/api/v1/recall"]);
    assert_eq!(fake.sessions().len(), 1);
}
