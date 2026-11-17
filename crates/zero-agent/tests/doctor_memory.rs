//! Строка `memory` команды `doctor` (R1-M D136, D152) на поддельном ZeroAIMemory
//! (`test-support/fake_zam.rs`, только `127.0.0.1`). Функция строки берёт токен параметром:
//! окружение процесса тест не трогает (`set_var` не нужен).

#[path = "../../../test-support/fake_zam.rs"]
mod fake_zam;

use fake_zam::{FakeZam, Health, WAIT, within};
use za_memory::{MemoryConfig, Token, ZamClient};
use zero_agent::doctor::{Check, check_memory, check_memory_live};

/// Хвост фиктивного токена (в тексте строки его нет).
const TAIL: &str = "D0ct0rTa1lQw8Er5Ty2Ui9Op3As6Df1Gh7Jk4";

/// Фиктивный токен — из частей во время выполнения (GitHub push protection).
fn token_raw() -> String {
    ["zam", "_", "doc", TAIL].concat()
}

fn token() -> Option<Token> {
    Some(Token::new(token_raw()))
}

#[tokio::test]
async fn doctor_memory_line() {
    let fake = FakeZam::start().await;
    fake.require_token(&token_raw());
    let cfg = MemoryConfig::new(&fake.url());

    // L2 (m6) и принятый токен — `Ok` с версией, коммитом, уровнем и числом флагов.
    let c = within(WAIT, "live", check_memory_live(Some(&cfg), token())).await;
    let Check::Ok(text) = &c else {
        panic!("{c:?}");
    };
    let want = format!(
        "ZeroAIMemory {} ({}), L2, флаги: {}; токен принят",
        fake_zam::VERSION_M6,
        fake_zam::GIT_M6,
        fake_zam::FEATURES_M6.len()
    );
    assert!(text.starts_with(&want), "{text}");
    assert!(text.contains("Vault открыт"), "{text}");
    assert!(!text.contains(TAIL), "{text}");

    // Форма m6.5 (тестовый стенд): тоже L2, версия только печатается.
    fake.set_health(Health::M65);
    let c = within(WAIT, "m65", check_memory_live(Some(&cfg), token())).await;
    assert!(
        matches!(&c, Check::Ok(t) if t.contains(fake_zam::VERSION_M65) && t.contains("L2")),
        "{c:?}"
    );
    fake.set_health(Health::M6);

    // Без `[memory]` — «память не настроена»; выключена — `Ok`.
    let c = check_memory(None, token(), None);
    assert!(
        matches!(&c, Check::Warn(t) if t.contains("память не настроена")),
        "{c:?}"
    );
    let off = MemoryConfig {
        enabled: false,
        ..MemoryConfig::new("http://example.org:7700")
    };
    let c = within(WAIT, "off", check_memory_live(Some(&off), token())).await;
    assert!(
        matches!(&c, Check::Ok(t) if t.contains("выключена")),
        "{c:?}"
    );

    // Статус с поддельного сервера, адрес конфигурации — другой: частный адрес по http —
    // `Ok`, внешний хост по http — `Warn` «токен идёт открытым текстом».
    let client = ZamClient::new(&cfg, token()).unwrap();
    let st = within(WAIT, "status", za_memory::status(&client, None)).await;
    let private = MemoryConfig::new("http://10.0.0.5:7700");
    assert!(
        matches!(
            check_memory(Some(&private), token(), Some(&st)),
            Check::Ok(_)
        ),
        "{st:?}"
    );
    let public = MemoryConfig::new("http://example.org:7700");
    let c = check_memory(Some(&public), token(), Some(&st));
    assert!(
        matches!(&c, Check::Warn(t) if t.contains("токен идёт открытым текстом")),
        "{c:?}"
    );
    let tls = MemoryConfig::new("https://example.org:7700");
    assert!(matches!(
        check_memory(Some(&tls), token(), Some(&st)),
        Check::Ok(_)
    ));

    // Без токена — `Warn`; Vault заблокирован, подсказки выключены — `Warn` с причинами.
    let c = within(WAIT, "no token", check_memory_live(Some(&cfg), None)).await;
    assert!(
        matches!(&c, Check::Warn(t) if t.contains("ZA_MEMORY_TOKEN")),
        "{c:?}"
    );
    fake.lock_vault("no master key");
    fake.hook_off("post_tool");
    let c = within(WAIT, "locked", check_memory_live(Some(&cfg), token())).await;
    assert!(
        matches!(&c, Check::Warn(t) if t.contains("Vault заблокирован: no master key") && t.contains("post_tool")),
        "{c:?}"
    );
}

/// Сервер недоступен или отвечает не ZAM — `Warn` «работа без памяти (L0)», не `Fail`.
#[tokio::test]
async fn doctor_memory_unreachable_is_warn() {
    let cfg = MemoryConfig::new(&fake_zam::closed_port_url());
    let c = within(WAIT, "offline", check_memory_live(Some(&cfg), token())).await;
    assert!(
        matches!(&c, Check::Warn(t) if t.contains("память недоступна") && t.contains("L0")),
        "{c:?}"
    );
    let fake = FakeZam::start().await;
    fake.set_health(Health::Html);
    let c = within(
        WAIT,
        "html",
        check_memory_live(Some(&MemoryConfig::new(&fake.url())), token()),
    )
    .await;
    assert!(
        matches!(&c, Check::Warn(t) if t.contains("not a ZeroAIMemory API")),
        "{c:?}"
    );
}
