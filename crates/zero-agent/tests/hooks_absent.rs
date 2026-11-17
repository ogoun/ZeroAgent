//! Страж D128 (m2 ревью задачи 15 R1-C2): бинарник обычной сборки — без признака
//! `test-hooks` — не знает тестовых крюков `run --web-resolve` (подмена имён мимо фильтра
//! адресов шлюза), `extract --hang` и `extract --crash`. Регрессию вида «крюк перенесли под
//! `debug_assertions`» поймает шаг CI `cargo test --workspace`: цель собирается только без
//! признака (с ним весь файл пуст).
//!
//! Разбор аргументов отвергает флаги до любой работы: ни конфигурации, ни сети, ни
//! журнала; `ZA_HOME` — пустой каталог теста.

#![cfg(not(feature = "test-hooks"))]

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

/// Пустой `ZA_HOME` теста в `CARGO_TARGET_TMPDIR`.
fn home(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("zero-agent-hooks-absent")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(name: &str, args: &[&str]) -> Output {
    let home = home(name);
    let out = Command::new(env!("CARGO_BIN_EXE_zero-agent"))
        .args(args)
        .env("ZA_HOME", &home)
        .env_remove("LLAMA_URL")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    // Отказ разбора аргументов ничего не создаёт.
    assert_eq!(
        std::fs::read_dir(&home).unwrap().count(),
        0,
        "{args:?}: ZA_HOME не пуст"
    );
    out
}

/// Флаг отвергнут разбором аргументов: код не 0 и `unexpected argument '<флаг>'`.
fn assert_rejected(out: &Output, flag: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{flag}: {out:?}");
    assert!(
        stderr.contains(&format!("unexpected argument '{flag}'")),
        "{flag}: {stderr}"
    );
}

#[test]
fn run_web_resolve_is_unknown() {
    let project = home("project");
    let out = run(
        "web-resolve",
        &[
            "run",
            "--project",
            project.to_str().unwrap(),
            "--web-resolve",
            "docs.test=127.0.0.1:80",
            "x",
        ],
    );
    assert_rejected(&out, "--web-resolve");
}

#[test]
fn extract_hang_and_crash_are_unknown() {
    assert_rejected(&run("hang", &["extract", "--hang", "html"]), "--hang");
    assert_rejected(&run("crash", &["extract", "--crash", "html"]), "--crash");
}
