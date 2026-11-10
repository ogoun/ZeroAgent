//! Сжатие вывода `bash` по типу (R1-C1 D86, CTX-007): логи тестов без потери строк с
//! ошибками, таблица `git diff`, начало списков файлов, скелет JSON; корпус ложных
//! срабатываний — команды, на которых сжатие молчит.

use za_core::shell::{Dialect, Segment, segments};
use za_tools::digest;

fn seg(c: &str) -> Vec<Segment> {
    segments(c, Dialect::Bash)
}

/// Большой вывод без признаков какого-либо типа.
fn big_text() -> String {
    (0..5000)
        .map(|i| format!("line {i} of plain text\n"))
        .collect()
}

/// Лог `cargo test` из 5000 строк с одной упавшей проверкой в середине.
fn cargo_log() -> String {
    let mut s = String::new();
    for i in 0..5000 {
        if i == 2500 {
            s.push_str("test tests::broken ... FAILED\n");
        } else {
            s.push_str(&format!("test tests::case_{i} ... ok\n"));
        }
    }
    s.push_str("\ntest result: FAILED. 4999 passed; 1 failed; 0 ignored\n");
    s
}

#[test]
fn digest_tests_keeps_failures_and_tail() {
    let limit = 4000;
    let d = digest(&seg("cargo test -p za-core"), &cargo_log(), "", limit).unwrap();
    assert!(d.len() <= limit, "{} > {limit}", d.len());
    assert!(d.contains("test tests::broken ... FAILED"), "{d}");
    // Контекст по 2 строки вокруг совпадения.
    assert!(d.contains("test tests::case_2498 ... ok"), "{d}");
    assert!(d.contains("test tests::case_2502 ... ok"), "{d}");
    assert!(!d.contains("test tests::case_2497 ... ok"), "{d}");
    // Хвост: итог и последние строки.
    assert!(d.contains("test result: FAILED. 4999 passed"), "{d}");
    assert!(d.contains("test tests::case_4999 ... ok"), "{d}");
    assert!(!d.contains("test tests::case_10 ... ok"), "{d}");
}

#[test]
fn digest_tests_reads_stderr_too() {
    // Ошибки компиляции cargo пишет в stderr.
    let err = format!("{}error: expected one of `;`\n", big_text());
    let d = digest(&seg("cargo test"), "", &err, 2000).unwrap();
    assert!(d.contains("error: expected one of `;`"), "{d}");
    assert!(d.len() <= 2000);
}

#[test]
fn digest_tests_runners() {
    for c in [
        "cargo nextest run",
        "pytest -x",
        "python -m pytest tests",
        "go test ./...",
        "npm test",
        "pnpm test",
        "yarn test",
        "npm run test",
        "jest",
        "vitest run",
        "dotnet test",
        "mvn test",
        "gradle test",
        "env RUST_BACKTRACE=1 cargo test",
    ] {
        assert!(digest(&seg(c), &cargo_log(), "", 3000).is_some(), "{c}");
    }
}

#[test]
fn digest_allows_2_to_1() {
    assert!(digest(&seg("cargo test 2>&1"), &cargo_log(), "", 3000).is_some());
}

#[test]
fn digest_long_lines_fit_limit() {
    let long = format!("{}\n", "x".repeat(10_000));
    let log = format!("{long}error: boom {}\n{long}", "y".repeat(5000)).repeat(20);
    let d = digest(&seg("cargo test"), &log, "", 3000).unwrap();
    assert!(d.len() <= 3000, "{}", d.len());
    assert!(d.contains("error: boom"), "{d}");
}

fn diff_of(files: &[(&str, usize, usize)]) -> String {
    let mut s = String::new();
    for (name, add, del) in files {
        s.push_str(&format!(
            "diff --git a/{name} b/{name}\nindex 1111111..2222222 100644\n--- a/{name}\n+++ b/{name}\n@@ -1,{del} +1,{add} @@\n"
        ));
        for i in 0..*del {
            s.push_str(&format!("-old {i}\n"));
        }
        for i in 0..*add {
            s.push_str(&format!("+new {i}\n"));
        }
    }
    s
}

#[test]
fn digest_git_diff_stat() {
    let diff = diff_of(&[("a.rs", 2, 1), ("src/b.rs", 0, 3), ("c.md", 5, 0)]);
    let d = digest(&seg("git diff"), &diff, "", 4000).unwrap();
    let lines: Vec<&str> = d.lines().collect();
    for want in ["a.rs +2 -1", "src/b.rs +0 -3", "c.md +5 -0"] {
        assert!(lines.contains(&want), "{want}: {d}");
    }
    // Затем хунки.
    assert!(d.contains("+new 4"), "{d}");
    let d = digest(&seg("git -C sub --no-pager show HEAD"), &diff, "", 4000).unwrap();
    assert!(d.lines().any(|l| l == "a.rs +2 -1"), "{d}");
}

#[test]
fn digest_git_diff_hunks_cut_at_limit() {
    let diff = diff_of(&[("a.rs", 3000, 0), ("b.rs", 1, 1)]);
    let d = digest(&seg("git diff"), &diff, "", 2000).unwrap();
    assert!(d.len() <= 2000, "{}", d.len());
    assert!(d.lines().any(|l| l == "b.rs +1 -1"), "{d}");
    assert!(d.contains("more lines"), "{d}");
}

#[test]
fn digest_listing_head() {
    let list: String = (0..1000).map(|i| format!("file_{i}.rs\n")).collect();
    for c in [
        "ls -la",
        "find . -name x",
        "tree",
        "dir",
        "Get-ChildItem -Recurse",
    ] {
        let d = digest(&seg(c), &list, "", 100_000).unwrap();
        assert!(d.contains("file_199.rs"), "{c}");
        assert!(!d.contains("file_200.rs"), "{c}");
        assert!(d.ends_with("... 800 more entries"), "{c}: {d}");
    }
}

#[test]
fn digest_json_skeleton() {
    assert_eq!(
        digest(&seg("cat x.json"), r#"{"a":[1,2,3],"b":{"c":"x"}}"#, "", 10).unwrap(),
        "{a: array[3], b: {c: string}}"
    );
}

#[test]
fn digest_json_depth_and_arrays() {
    let j = r#"[{"id":1,"tags":["x"],"deep":{"l2":{"l3":{"l4":1}}},"ok":true,"n":null}]"#;
    let d = digest(&seg("curl -s https://example.invalid/api"), j, "", 4000).unwrap();
    assert_eq!(
        d,
        // Ключи — по алфавиту (`serde_json` без `preserve_order`).
        "array[1] of {deep: {l2: {...}}, id: number, n: null, ok: bool, tags: array[1]}"
    );
}

#[test]
fn digest_json_many_keys() {
    let keys: Vec<String> = (0..60).map(|i| format!("\"k{i:02}\": {i}")).collect();
    let j = format!("{{{}}}", keys.join(","));
    let d = digest(&seg("cat big.json"), &j, "", 100_000).unwrap();
    assert!(d.starts_with("{k00: number, k01: number"), "{d}");
    assert!(d.ends_with(", ... 10 more keys}"), "{d}");
}

#[test]
fn digest_false_positive_corpus() {
    for c in [
        "echo cargo test",
        "cat test.log",
        "cargo test | tail",
        "git diff > a.patch",
        "git ls-files",
        "lsof",
        "cargo build",
        "git log --stat",
        "cargo test; ls",
        "cargo test $(echo x)",
    ] {
        assert_eq!(digest(&seg(c), &big_text(), "", 1000), None, "{c}");
    }
    assert_eq!(digest(&seg("cat x"), "[1, 2", "", 3), None);
    // `git diff` без единого файла (`--stat`, пустой diff) — не таблица.
    assert_eq!(digest(&seg("git diff --stat"), &big_text(), "", 1000), None);
}
