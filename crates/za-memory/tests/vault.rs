//! Vault памяти (R1-M D143, D144, D160): поиск записи по `key_ref` — Vault решения, затем
//! личный; раскрытие только нужных ключей с сессией run или без неё; кэш на процесс; маска
//! процесса до возврата значения — на поддельном ZAM (`test-support/fake_zam.rs`, только
//! `127.0.0.1`). Значения ключей — склейка из частей во время выполнения.

#[path = "../../../test-support/fake_zam.rs"]
mod fake_zam;

use std::sync::Arc;

use fake_zam::{FakeZam, WAIT, within};
use za_memory::vault::{SKIP_NO_SOLUTION, SKIP_UNTRUSTED};
use za_memory::{
    ErrorKind, KeyState, MemoryConfig, SolutionVault, VaultClient, VaultError, VaultKeys,
    VaultScope, ZamClient,
};
use za_secrets::{MaskCell, SecretStore};

/// GUID решения проекта (синтетический).
const SOLUTION: &str = "6f1c2b3a-0d4e-4f5a-8b6c-7d8e9f0a1b2c";

/// Фиктивное значение ключа — из частей (GitHub push protection).
fn value(tag: &str) -> String {
    ["vault", "-synthetic-", tag, "-0123456789"].concat()
}

fn client(fake: &FakeZam) -> Arc<ZamClient> {
    Arc::new(ZamClient::new(&MemoryConfig::new(&fake.url()), None).unwrap())
}

fn bound() -> SolutionVault {
    SolutionVault::Bound(SOLUTION.to_owned())
}

#[tokio::test]
async fn solution_before_knowledge_exact_title() {
    let fake = FakeZam::start().await;
    let in_solution = fake.add_vault(SOLUTION, "open-router", "token", Some(&value("s")));
    fake.add_vault("knowledge", "open-router", "token", Some(&value("k")));
    let v = VaultClient::new(client(&fake));

    // Запись решения закрывает личную.
    let item = within(WAIT, "find", v.find("open-router", &bound()))
        .await
        .unwrap();
    assert_eq!(item.id, in_solution);
    assert_eq!(item.scope, VaultScope::Solution(SOLUTION.to_owned()));
    assert_eq!(item.item_type, "token");

    // «OPEN-ROUTER» точно нет нигде; без учёта регистра в решении — одна запись: она.
    let item = within(WAIT, "case", v.find("OPEN-ROUTER", &bound()))
        .await
        .unwrap();
    assert_eq!(item.id, in_solution);

    // Только в личном: одна «Open-Router» без учёта регистра — она.
    let fake = FakeZam::start().await;
    let one = fake.add_vault("knowledge", "Open-Router", "token", Some(&value("k1")));
    let v = VaultClient::new(client(&fake));
    let item = within(WAIT, "knowledge", v.find("OPEN-ROUTER", &bound()))
        .await
        .unwrap();
    assert_eq!(item.id, one);
    assert_eq!(item.scope, VaultScope::Knowledge);
    // Точное совпадение важнее регистра.
    let exact = fake.add_vault("knowledge", "OPEN-ROUTER", "token", Some(&value("k2")));
    let v = VaultClient::new(client(&fake));
    let item = within(WAIT, "exact", v.find("OPEN-ROUTER", &bound()))
        .await
        .unwrap();
    assert_eq!(item.id, exact);
    // Две без учёта регистра и ни одной точной — без догадки.
    let e = within(WAIT, "ambiguous", v.find("open-router", &bound()))
        .await
        .unwrap_err();
    let VaultError::Ambiguous(titles) = &e else {
        panic!("{e:?}");
    };
    assert_eq!(titles.len(), 2, "{titles:?}");
    assert!(!e.is_waitable());
}

#[tokio::test]
async fn untrusted_not_found_names_reason() {
    let mut fake = FakeZam::start().await;
    fake.add_vault(SOLUTION, "demo-key", "token", Some(&value("u")));
    let v = VaultClient::new(client(&fake));
    let e = within(
        WAIT,
        "untrusted",
        v.find("demo-key", &SolutionVault::Untrusted),
    )
    .await
    .unwrap_err();
    assert_eq!(
        e,
        VaultError::NotFound {
            solution_skipped: Some(SKIP_UNTRUSTED)
        }
    );
    assert!(
        e.to_string()
            .contains("Vault решения не читается: проект не доверен (zero-agent trust)"),
        "{e}"
    );
    // Vault решения недоверенного проекта не запрашивается вовсе.
    let routes: Vec<String> = fake.seen().iter().map(|s| s.route().to_owned()).collect();
    assert_eq!(routes, ["/api/v1/spaces/knowledge/vault"]);
    // Доверенный без решения — своя причина.
    let e = within(WAIT, "unbound", v.find("demo-key", &SolutionVault::Unbound))
        .await
        .unwrap_err();
    assert_eq!(
        e,
        VaultError::NotFound {
            solution_skipped: Some(SKIP_NO_SOLUTION)
        }
    );
    assert!(e.to_string().contains("zero-agent memory init"), "{e}");
    assert!(!e.is_waitable());
}

#[tokio::test]
async fn environment_records_are_not_keys() {
    let fake = FakeZam::start().await;
    fake.add_vault("knowledge", "searxng", "host", None);
    fake.add_vault(SOLUTION, "stand", "environment", None);
    let v = VaultClient::new(client(&fake));
    let e = within(WAIT, "host", v.find("searxng", &bound()))
        .await
        .unwrap_err();
    assert_eq!(e, VaultError::NotAKey);
    let keys = VaultKeys::new(
        VaultClient::new(client(&fake)),
        bound(),
        Arc::new(MaskCell::default()),
    );
    let (revealed, states) = within(
        WAIT,
        "keys",
        keys.reveal(&["searxng".to_owned(), "stand".to_owned()], None),
    )
    .await;
    assert!(revealed.is_empty());
    assert!(
        states.values().all(|s| matches!(s, KeyState::Missing(_))),
        "{states:?}"
    );
    assert!(fake.reveals().is_empty(), "запись окружения раскрывалась");
    // Окружение для `secret list` — без раскрытия.
    let env = within(WAIT, "env", v.environment(&bound())).await.unwrap();
    let mut titles: Vec<&str> = env.iter().map(|i| i.title.as_str()).collect();
    titles.sort_unstable();
    assert_eq!(titles, ["searxng", "stand"]);
    assert!(fake.reveals().is_empty());
}

#[tokio::test]
async fn locked_list_and_reveal() {
    let mut fake = FakeZam::start().await;
    fake.add_vault("knowledge", "demo-key", "token", Some(&value("l")));
    fake.lock_vault("master key file is missing");
    let v = VaultClient::new(client(&fake));
    // Метаданные открыты и при блокировке: запись находится.
    let (item, locked) = within(WAIT, "locate", v.locate("demo-key", &bound()))
        .await
        .unwrap();
    assert_eq!(locked.as_deref(), Some("master key file is missing"));
    // Раскрытие — 503 `vault-locked`.
    let e = within(WAIT, "reveal", v.reveal(&item, None))
        .await
        .unwrap_err();
    assert_eq!(
        e,
        VaultError::Locked("master key file is missing".to_owned())
    );
    assert!(e.is_waitable());
    assert!(e.to_string().starts_with("Vault заблокирован"), "{e}");
    // `VaultKeys` у заблокированного Vault ждёт без запроса раскрытия; список не
    // кэшируется — разблокировку видит следующая попытка.
    let keys = VaultKeys::new(
        VaultClient::new(client(&fake)),
        bound(),
        Arc::new(MaskCell::default()),
    );
    let names = ["demo-key".to_owned()];
    fake.seen();
    let (_, states) = within(WAIT, "keys locked", keys.reveal(&names, None)).await;
    assert!(
        matches!(&states["demo-key"], KeyState::Waiting(r) if r.starts_with("Vault заблокирован")),
        "{states:?}"
    );
    let asked: Vec<String> = fake.seen().into_iter().map(|s| s.path).collect();
    assert!(
        asked.iter().all(|p| !p.contains("reveal=")),
        "раскрытие при заблокированном Vault: {asked:?}"
    );
    fake.unlock_vault();
    let (revealed, states) = within(WAIT, "keys open", keys.reveal(&names, None)).await;
    assert_eq!(states["demo-key"], KeyState::Ready);
    assert_eq!(revealed.len(), 1);
    assert_eq!(fake.reveals().len(), 1);
}

#[tokio::test]
async fn name_query_param_never_sent() {
    let mut fake = FakeZam::start().await;
    fake.add_vault(SOLUTION, "demo-key", "token", Some(&value("n")));
    fake.add_vault("knowledge", "other", "token", Some(&value("o")));
    let keys = VaultKeys::new(
        VaultClient::new(client(&fake)),
        bound(),
        Arc::new(MaskCell::default()),
    );
    within(
        WAIT,
        "keys",
        keys.reveal(&["demo-key".to_owned(), "other".to_owned()], None),
    )
    .await;
    let seen = fake.seen();
    assert!(!seen.is_empty());
    for s in &seen {
        assert!(!s.path.contains("name="), "{}", s.path);
        assert!(!s.path.contains("q="), "{}", s.path);
    }
    // Открытый список — один раз на процесс.
    let lists = seen
        .iter()
        .filter(|s| s.route().ends_with("/vault"))
        .count();
    assert_eq!(lists, 2, "{seen:?}");
}

#[tokio::test]
async fn reveal_carries_session_header() {
    let fake = FakeZam::start().await;
    let id = fake.add_vault("knowledge", "demo-key", "token", Some(&value("h")));
    let v = VaultClient::new(client(&fake));
    let item = within(WAIT, "find", v.find("demo-key", &bound()))
        .await
        .unwrap();
    let session = "za-0123456789abcdef-0192f1c4-7a1e-7c3b-9a55-2b1f6f0d9e77";
    // `run` — с сессией этого run.
    let got = within(WAIT, "run", v.reveal(&item, Some(session)))
        .await
        .unwrap();
    assert_eq!(got, value("h"));
    // `ask` и `models probe` — без сессии.
    within(WAIT, "ask", v.reveal(&item, None)).await.unwrap();
    assert_eq!(
        fake.reveals(),
        [(id.clone(), Some(session.to_owned())), (id, None)]
    );
}

#[tokio::test]
async fn only_needed_keys_revealed_and_cached() {
    let fake = FakeZam::start().await;
    for (name, tag) in [("key-a", "a"), ("key-b", "b"), ("key-c", "c")] {
        fake.add_vault("knowledge", name, "token", Some(&value(tag)));
    }
    let keys = VaultKeys::new(
        VaultClient::new(client(&fake)),
        bound(),
        Arc::new(MaskCell::default()),
    );
    // Команде нужен один ключ из трёх — раскрывается один.
    let plan = ["key-b".to_owned()];
    let (revealed, states) = within(WAIT, "first", keys.reveal(&plan, Some("za-s"))).await;
    assert_eq!(fake.reveals().len(), 1);
    assert_eq!(states["key-b"], KeyState::Ready);
    assert_eq!(revealed.len(), 1);
    assert_eq!(revealed[0].name, "key-b");
    assert_eq!(revealed[0].scope, "knowledge");
    assert!(!revealed[0].cached);
    assert_eq!(keys.get("key-b"), Some(value("b")));
    assert_eq!(keys.get("key-a"), None);
    assert_eq!(keys.names(), ["key-b"]);
    assert_eq!(keys.kind(), "vault");
    // Второй раз — из кэша, без запроса.
    let (revealed, _) = within(WAIT, "second", keys.reveal(&plan, Some("za-s"))).await;
    assert_eq!(fake.reveals().len(), 1);
    assert!(revealed[0].cached);
    assert_eq!(revealed[0].item, fake.reveals()[0].0);
}

#[tokio::test]
async fn reveal_extends_mask_before_return() {
    let fake = FakeZam::start().await;
    fake.add_vault("knowledge", "demo-key", "token", Some(&value("m")));
    let cell = Arc::new(MaskCell::default());
    let keys = VaultKeys::new(VaultClient::new(client(&fake)), bound(), cell.clone());
    let text = format!("echo {}", value("m"));
    let before = cell.current();
    assert_eq!(
        before.mask(&text),
        text.as_str(),
        "до раскрытия маскировать нечего"
    );
    within(WAIT, "reveal", keys.reveal(&["demo-key".to_owned()], None)).await;
    let masked = cell.current().mask(&text).into_owned();
    assert_eq!(masked, "echo [secret masked: demo-key]");
    // Прежний снимок не меняется: потребители берут `current()` после фазы раскрытия.
    assert_eq!(before.mask(&text), text.as_str());
}

#[tokio::test]
async fn unavailable_waits_forbidden_does_not() {
    // Сервер недоступен: соединение закрывается до чтения запроса.
    let fake = FakeZam::start().await;
    fake.refuse_connections(true);
    let keys = VaultKeys::new(
        VaultClient::new(client(&fake)),
        bound(),
        Arc::new(MaskCell::default()),
    );
    let (_, states) = within(WAIT, "offline", keys.reveal(&["k".to_owned()], None)).await;
    assert!(
        matches!(&states["k"], KeyState::Waiting(r) if r.contains("сервер памяти недоступен")),
        "{states:?}"
    );
    // Vault решения закрыт (403 `solution-access`) — ошибка без ожидания.
    let fake = FakeZam::start().await;
    fake.forbid_solution(SOLUTION, "pending");
    let v = VaultClient::new(client(&fake));
    let e = within(WAIT, "forbidden", v.find("k", &bound()))
        .await
        .unwrap_err();
    assert_eq!(e, VaultError::Forbidden);
    assert!(!e.is_waitable());
    // 503 `about:blank` списка — временная ошибка, ждать.
    let fake = FakeZam::start().await;
    fake.fail("/api/v1/spaces/knowledge/vault", 503, "about:blank", None);
    let v = VaultClient::new(client(&fake));
    let e = within(WAIT, "503", v.find("k", &SolutionVault::Untrusted))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, VaultError::Unavailable(z) if z.kind == ErrorKind::Server),
        "{e:?}"
    );
    assert!(e.is_waitable());
}

#[test]
fn vault_keys_debug_has_no_values() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fake = FakeZam::start().await;
        fake.add_vault("knowledge", "demo-key", "token", Some(&value("d")));
        let cell = Arc::new(MaskCell::default());
        let keys = VaultKeys::new(VaultClient::new(client(&fake)), bound(), cell.clone());
        let (revealed, states) =
            within(WAIT, "reveal", keys.reveal(&["demo-key".to_owned()], None)).await;
        let v = value("d");
        for d in [
            format!("{keys:?}"),
            format!("{cell:?}"),
            format!("{revealed:?}"),
            format!("{states:?}"),
            format!("{:?}", keys.vault()),
        ] {
            assert!(!d.contains(&v), "{d}");
        }
        assert!(format!("{keys:?}").contains("demo-key"));
    });
}
