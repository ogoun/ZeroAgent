//! Падение подпроцесса извлекателя не роняет запустивший процесс (R1-C2 D127, D128).
//!
//! Глубокая вложенность больше не роняет `html_to_markdown` (её ловит страж, см.
//! `deep_nesting_is_simplified` в `tests/html.rs`). Чтобы проверить само свойство «падение
//! убивает только подпроцесс», здесь — намеренный сбой под признаком `test-hooks`:
//! подпроцессом запускается сам тестовый бинарник, его скрытый тест [`crash_child`]
//! завершает процесс `abort()`. Сквозной путь через `za_jail::Runner` — задача 10.
//!
//! Весь файл компилируется только с `test-hooks` (`required-features`), поэтому `abort()`
//! в продукт не попадает. Подпроцесс — сам бинарник, а не `zero-agent`: `CARGO_BIN_EXE_*`
//! есть только у тестов пакета-владельца бинарника.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Множитель времени тестов: `ZA_TEST_TIME_SCALE` (целое 1–6, по умолчанию 1).
fn time_scale() -> u32 {
    std::env::var("ZA_TEST_TIME_SCALE")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map_or(1, |n| n.clamp(1, 6))
}

/// Подпроцесс [`deep_nesting_kills_only_child`]: при `ZA_EXTRACT_CRASH` завершает процесс
/// `abort()`, иначе ничего не делает.
#[test]
#[ignore = "подпроцесс deep_nesting_kills_only_child"]
fn crash_child() {
    if std::env::var_os("ZA_EXTRACT_CRASH").is_some() {
        std::process::abort();
    }
}

/// Падение подпроцесса наблюдается родителем кодом завершения, родитель продолжает работу
/// (D127). Подпроцесс — скрытый тест [`crash_child`] этого же бинарника.
#[test]
fn deep_nesting_kills_only_child() {
    let exe = std::env::current_exe().expect("путь тестового бинарника");
    let mut child = Command::new(exe)
        .args(["crash_child", "--exact", "--ignored", "--test-threads", "1"])
        .env("ZA_EXTRACT_CRASH", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("запуск подпроцесса");
    let deadline = Instant::now() + Duration::from_secs(30) * time_scale();
    let status = loop {
        if let Some(s) = child.try_wait().expect("ожидание подпроцесса") {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("подпроцесс не завершился до предела");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Родитель дошёл до сюда — он пережил падение подпроцесса.
    assert!(
        !status.success(),
        "подпроцесс должен был упасть, а завершился успешно ({status:?})"
    );
}
