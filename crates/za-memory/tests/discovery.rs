//! Обнаружение сервера памяти и проверки статуса (R1-M D136) на поддельном ZAM
//! (`test-support/fake_zam.rs`, D154): только `127.0.0.1`.

#[path = "../../../test-support/fake_zam.rs"]
mod fake_zam;

use std::time::Duration;

use fake_zam::{FakeZam, Health};
use za_memory::{
    ErrorKind, Level, MemoryConfig, SolutionState, Token, TokenState, VaultState, ZamClient,
    discover, status,
};

/// Ожидание под пределом с понятной паникой (сетевой вызов на 127.0.0.1 не виснет дольше
/// своих сроков, предел — страховка).
async fn within<F: std::future::Future>(what: &str, f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(30), f)
        .await
        .unwrap_or_else(|_| panic!("{what}: не завершилось за 30 с"))
}

/// Фиктивный токен — из частей во время выполнения (GitHub push protection).
fn token_raw() -> String {
    [
        "zam",
        "_",
        "d1sc0veryT0ken",
        "Xy7Qw2Rt9Lp4Mn8Zk3Vb6Hc1Jf5Gs0Dq",
    ]
    .concat()
}

fn token() -> Token {
    Token::new(token_raw())
}

fn client(url: &str, token: Option<Token>) -> ZamClient {
    ZamClient::new(&MemoryConfig::new(url), token).unwrap()
}

#[tokio::test]
async fn root_health_html_is_not_zam() {
    let mut fake = FakeZam::start().await;
    fake.set_health(Health::Html);
    let d = within("discover", discover(&client(&fake.url(), None))).await;
    assert_eq!(d.level, Level::L0);
    assert_eq!(d.reason.as_deref(), Some("not a ZeroAIMemory API"));
    assert_eq!(d.error, Some(ErrorKind::NotZam));
    assert!(d.info.is_none());
    // Путь — `/api/v1/health`, без токена и заголовков атрибуции.
    let seen = fake.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].path, "/api/v1/health");
    assert!(!seen[0].has_authorization);
}

#[tokio::test]
async fn health_without_features_is_l1() {
    let fake = FakeZam::start().await;
    fake.set_health(Health::NoFeatures);
    let d = within("discover", discover(&client(&fake.url(), Some(token())))).await;
    assert_eq!(d.level, Level::L1);
    let info = d.info.unwrap();
    assert_eq!(info.version, "0.0.10-m5.7");
    assert_eq!(info.git.as_deref(), Some("13b0bac"));
    assert!(info.features.is_empty());
    assert_eq!(info.tokenizer_id, None);
    assert!(d.reason.is_none());
}

#[tokio::test]
async fn health_with_m6_is_l2_and_unknown_flags_kept() {
    let fake = FakeZam::start().await;
    fake.set_health(Health::Json(serde_json::json!({
        "status": "ok",
        "version": "0.0.11-m6",
        "git": "e391440c0677",
        "features": ["m6", "snapshot", "zz_future", 7],
        "token_counter": "model",
        "tokenizer_id": "qwen3",
        "uptime_s": 1,
        "zz_new_field": {"a": [1, 2]},
    })));
    let mut fake = fake;
    let d = within("discover", discover(&client(&fake.url(), Some(token())))).await;
    assert_eq!(d.level, Level::L2);
    let info = d.info.unwrap();
    assert!(info.features.contains("zz_future"), "{:?}", info.features);
    assert!(info.features.contains("snapshot"));
    // Не строка в списке флагов пропускается.
    assert_eq!(info.features.len(), 3);
    assert_eq!(info.token_counter.as_deref(), Some("model"));
    assert_eq!(info.tokenizer_id.as_deref(), Some("qwen3"));
    // Обнаружение без токена: `GET /api/v1/health` анонимно (D136).
    let seen = fake.seen();
    assert!(!seen[0].has_authorization);
    assert!(seen[0].header("x-zam-machine").is_none());
}

/// Форма `0.0.12-m6.5` (тестовый стенд): уровень — по флагу `m6`, версия не сравнивается.
#[tokio::test]
async fn health_m65_is_l2_with_idea_review() {
    let fake = FakeZam::start().await;
    fake.set_health(Health::M65);
    let d = within("discover", discover(&client(&fake.url(), None))).await;
    assert_eq!(d.level, Level::L2);
    let info = d.info.unwrap();
    assert_eq!(info.version, fake_zam::VERSION_M65);
    assert!(info.features.contains("idea_review"));
    assert_eq!(info.features.len(), fake_zam::features_m65().len());
}

/// JSON без `version` — не ZAM; 5xx `/health` — L0 с кодом.
#[tokio::test]
async fn health_without_version_or_5xx_is_l0() {
    let fake = FakeZam::start().await;
    fake.set_health(Health::Json(serde_json::json!({"status": "ok"})));
    let c = client(&fake.url(), None);
    let d = within("discover", discover(&c)).await;
    assert_eq!(d.level, Level::L0);
    assert_eq!(d.error, Some(ErrorKind::NotZam));
    fake.set_health(Health::Status(503));
    let d = within("discover", discover(&c)).await;
    assert_eq!(d.level, Level::L0);
    assert_eq!(d.error, Some(ErrorKind::Server));
    assert!(d.reason.unwrap().contains("503"));
}

#[tokio::test]
async fn connection_refused_is_l0_offline() {
    let c = client(&fake_zam::closed_port_url(), Some(token()));
    let d = within("discover", discover(&c)).await;
    assert_eq!(d.level, Level::L0);
    assert_eq!(d.error, Some(ErrorKind::Offline));
    assert!(d.reason.is_some());
}

#[tokio::test]
async fn status_reports_token_solution_vault_hooks() {
    const GUID: &str = "01a1e164-0ff6-77dc-b4cb-84f08cc1f69e";
    let mut fake = FakeZam::start().await;
    // Токен не тот — 401 на `/spaces`.
    fake.require_token("other");
    let st = within(
        "status",
        status(&client(&fake.url(), Some(token())), Some(GUID)),
    )
    .await;
    assert_eq!(st.discovery.level, Level::L2);
    assert_eq!(st.token, TokenState::Rejected);
    assert!(matches!(st.solution, SolutionState::Unchecked(_)), "{st:?}");
    fake.seen();

    // Токен принят; решение — 403 `/problems/solution-access`; Vault заблокирован;
    // подсказки `prompt` выключены.
    fake.require_token(&token_raw());
    fake.forbid_solution(GUID, "pending");
    fake.lock_vault("no master key");
    fake.hook_off("prompt");
    let st = within(
        "status",
        status(&client(&fake.url(), Some(token())), Some(GUID)),
    )
    .await;
    assert_eq!(st.token, TokenState::Accepted);
    assert!(
        matches!(&st.solution, SolutionState::Forbidden(d) if d.contains("ждёт")),
        "{st:?}"
    );
    assert_eq!(st.vault, VaultState::Locked("no master key".into()));
    assert_eq!(st.hooks_off, vec!["prompt".to_owned()]);
    let seen = fake.seen();
    let recall = seen
        .iter()
        .find(|s| s.route() == "/api/v1/recall")
        .expect("запрос /recall");
    assert_eq!(recall.body["event"], "session_start");
    assert_eq!(recall.body["record"], false);
    assert!(recall.header("x-zam-session").is_none(), "{recall:?}");
    assert!(recall.authorized);
    assert!(
        seen.iter()
            .any(|s| s.route() == "/api/v1/spaces/knowledge/vault"),
        "{seen:?}"
    );

    // Решение доступно; без решения — `NotBound`.
    let fake = FakeZam::start().await;
    fake.add_solution(GUID, "ZeroAgent", "owner", "rw");
    let c = client(&fake.url(), Some(token()));
    let st = within("status", status(&c, Some(GUID))).await;
    assert_eq!(
        st.solution,
        SolutionState::Bound {
            name: "ZeroAgent".into(),
            role: "owner".into(),
            access: "rw".into()
        }
    );
    assert_eq!(st.vault, VaultState::Unlocked);
    assert!(st.hooks_off.is_empty());
    let st = within("status", status(&c, None)).await;
    assert_eq!(st.solution, SolutionState::NotBound);
    // Решения нет на сервере — 404.
    let st = within(
        "status",
        status(&c, Some("01a1e164-0000-7000-8000-000000000000")),
    )
    .await;
    assert_eq!(st.solution, SolutionState::NotFound);
}

/// Без токена проверяются только обнаружение; без сервера — ничего.
#[tokio::test]
async fn status_without_token_or_server() {
    let mut fake = FakeZam::start().await;
    let st = within("status", status(&client(&fake.url(), None), None)).await;
    assert_eq!(st.discovery.level, Level::L2);
    assert_eq!(st.token, TokenState::NotSet);
    assert!(matches!(st.vault, VaultState::Unchecked(_)));
    let seen = fake.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");

    let st = within(
        "status",
        status(&client(&fake_zam::closed_port_url(), Some(token())), None),
    )
    .await;
    assert_eq!(st.discovery.level, Level::L0);
    assert!(matches!(st.token, TokenState::Unchecked(_)), "{st:?}");
}

/// 429 на `/spaces` — токен не проверен, с `Retry-After`.
#[tokio::test]
async fn status_rate_limited_token() {
    let fake = FakeZam::start().await;
    fake.fail("/api/v1/spaces", 429, "/problems/rate-limited", Some(9));
    let st = within("status", status(&client(&fake.url(), Some(token())), None)).await;
    assert_eq!(
        st.token,
        TokenState::RateLimited(Some(Duration::from_secs(9)))
    );
}

/// Живая проверка на тестовом стенде (решение владельца 4, D151): только
/// `http://192.168.0.210:7700`, адрес и токен — из окружения теста (`ZAM_URL`, `ZAM_TOKEN`),
/// `NO_PROXY=192.168.0.210`. Только чтение: `/api/v1/health`, `/spaces`, личный Vault
/// (метаданные) и `/recall` с `record: false` без сессии. Уровень — по флагу `m6`, версия
/// не сравнивается (стенд — `0.0.12-m6.5`). Токен не печатается. В CI не идёт.
///
/// Запуск: `cargo test -p za-memory --test discovery -- --ignored live_stand`.
#[tokio::test]
#[ignore = "живой стенд 192.168.0.210:7700; ZAM_URL, ZAM_TOKEN, NO_PROXY из окружения"]
async fn live_stand_discovery_and_status() {
    const STAND: &str = "192.168.0.210";
    let url = std::env::var("ZAM_URL").expect("ZAM_URL тестового стенда");
    // Охранник до первого запроса: только хост стенда (рабочий 192.168.0.240 — никогда).
    let host = url::Url::parse(&url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned));
    assert_eq!(host.as_deref(), Some(STAND), "ZAM_URL — не тестовый стенд");
    let no_proxy = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    assert!(no_proxy.contains(STAND), "нужен NO_PROXY={STAND}");
    let token = Token::from_env_value(std::env::var("ZAM_TOKEN").ok()).expect("ZAM_TOKEN");
    let c = client(&url, Some(token));
    let st = within("live status", status(&c, None)).await;
    assert_eq!(st.discovery.level, Level::L2, "{:?}", st.discovery);
    let info = st.discovery.info.as_ref().unwrap();
    assert!(info.has("m6"), "{:?}", info.features);
    assert_eq!(st.token, TokenState::Accepted);
    assert!(
        !matches!(st.vault, VaultState::Unchecked(_)),
        "{:?}",
        st.vault
    );
    eprintln!(
        "стенд: {} ({}), {}, флагов {}, Vault {:?}, hooks_off {:?}",
        info.version,
        info.git.as_deref().unwrap_or("-"),
        st.discovery.level.as_str(),
        info.features.len(),
        st.vault,
        st.hooks_off
    );
}
