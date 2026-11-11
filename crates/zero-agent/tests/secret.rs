//! `zero-agent secret list` и источник секретов в `doctor` (R1-C1 D90) из отдельного
//! процесса: имена и «задан», значений в выводе нет.

mod support;
use support::process::env_in;

const CONFIG: &str = "[[models.endpoints]]\nid = \"cloud\"\nbackend = \"anthropic\"\nbase_url = \"http://127.0.0.1:9/v1\"\nmodel = \"m\"\nkey_ref = \"anthropic\"\n\n[[models.endpoints]]\nid = \"router\"\nbackend = \"anthropic\"\nbase_url = \"http://127.0.0.1:9/v1\"\nmodel = \"m\"\nkey_ref = \"open-router\"\n";

const VALUE_A: &str = "an-FAKE-value-0123456789";
const VALUE_X: &str = "ex-FAKE-value-0123456789";

#[test]
fn secret_list_prints_names_not_values() {
    let e = env_in("zero-agent-secret", "list", CONFIG);
    let o = e.run_with(
        &["secret", "list"],
        &[
            ("ZA_SECRET_ANTHROPIC", VALUE_A),
            ("ZA_SECRET_EXTRA", VALUE_X),
        ],
    );
    assert_eq!(o.code(), Some(0), "{o:?}");
    let line = |name: &str| {
        o.stdout
            .lines()
            .find(|l| l.split_whitespace().next() == Some(name))
            .unwrap_or_else(|| panic!("нет строки {name}: {}", o.stdout))
            .to_owned()
    };
    let state = |name: &str| line(name).split_whitespace().nth(1).map(str::to_owned);
    assert_eq!(state("anthropic").as_deref(), Some("задан"), "{}", o.stdout);
    assert!(
        line("anthropic").contains("ZA_SECRET_ANTHROPIC"),
        "{}",
        o.stdout
    );
    assert!(line("open-router").contains("не задан"), "{}", o.stdout);
    assert_eq!(state("extra").as_deref(), Some("задан"), "{}", o.stdout);
    assert!(line("extra").contains("нет в config.toml"), "{}", o.stdout);
    assert!(
        o.stdout
            .contains("переменные ZA_SECRET_* (Vault памяти — с клиентом ZeroAIMemory)"),
        "{}",
        o.stdout
    );
    for v in [VALUE_A, VALUE_X] {
        assert!(!o.stdout.contains(v), "{}", o.stdout);
        assert!(!o.stderr.contains(v), "{}", o.stderr);
    }
}

#[test]
fn secret_list_without_config_lists_env() {
    let e = env_in("zero-agent-secret", "noconfig", "");
    let o = e.run_with(&["secret", "list"], &[("ZA_SECRET_OPEN_ROUTER", VALUE_A)]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(o.stdout.contains("open-router"), "{}", o.stdout);
    assert!(!o.stdout.contains(VALUE_A), "{}", o.stdout);
    let o = e.run(&["secret", "list"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(o.stdout.contains("секретов нет"), "{}", o.stdout);
}

#[test]
fn doctor_names_secret_source() {
    let e = env_in("zero-agent-secret", "doctor", "");
    let o = e.run_with(&["doctor"], &[("ZA_SECRET_ANTHROPIC", VALUE_A)]);
    assert!(
        o.stdout.contains(
            "источник секретов: переменные ZA_SECRET_* (Vault памяти — с клиентом ZeroAIMemory)"
        ),
        "{}",
        o.stdout
    );
    assert!(!o.stdout.contains(VALUE_A), "{}", o.stdout);
}
