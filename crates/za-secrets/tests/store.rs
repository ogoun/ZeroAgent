//! `EnvStore`: только `ZA_SECRET_*`, имена `key_ref`, значения не в `Debug` (R1-C1 D90).

use std::ffi::OsString;

use za_secrets::{EnvStore, SecretStore, env_var_name, known_values};

fn vars(list: &[(&str, &str)]) -> impl Iterator<Item = (OsString, OsString)> {
    list.iter()
        .map(|(k, v)| (OsString::from(*k), OsString::from(*v)))
        .collect::<Vec<_>>()
        .into_iter()
}

#[test]
fn env_store_reads_injected_vars_only() {
    let s = EnvStore::from_vars(
        vec![
            ("ZA_SECRET_OPEN_ROUTER".into(), "v123456789".into()),
            ("PATH".into(), "p".into()),
        ]
        .into_iter(),
    );
    assert_eq!(s.get("open-router").as_deref(), Some("v123456789"));
    assert_eq!(s.names(), vec!["open-router".to_string()]);
    assert_eq!(s.kind(), "env");
}

#[test]
fn get_normalizes_like_secret_env_name() {
    let s = EnvStore::from_vars(vars(&[("ZA_SECRET_OPEN_ROUTER", "v123456789")]));
    for name in ["open-router", "open_router", "OPEN.ROUTER", "Open-Router"] {
        assert_eq!(s.get(name).as_deref(), Some("v123456789"), "{name}");
    }
    assert_eq!(s.get("openrouter"), None);
    assert_eq!(env_var_name("open-router"), "ZA_SECRET_OPEN_ROUTER");
    assert_eq!(env_var_name("my.key-2"), "ZA_SECRET_MY_KEY_2");
}

#[test]
fn skips_empty_unprefixed_and_foreign_names() {
    let s = EnvStore::from_vars_with(
        vars(&[
            ("ZA_SECRET_EMPTY", ""),
            ("ZA_SECRET_", "no-name-value"),
            ("ZA_SECRETS_FILE", "/path/to/file"),
            ("XZA_SECRET_A", "value-12345"),
            ("ZA_SECRET_A-B", "dash-not-from-env-name"),
            ("ZA_SECRET_B", "value-b-123"),
        ]),
        false,
    );
    assert_eq!(s.names(), vec!["b".to_string()]);
}

#[test]
fn case_of_names_follows_platform() {
    let list = [
        ("za_secret_Open_Router", "v123456789"),
        ("ZA_SECRET_lower", "x1234567"),
    ];
    // Windows: имена переменных без учёта регистра, как у `std::env::var`.
    let w = EnvStore::from_vars_with(vars(&list), true);
    assert_eq!(
        w.names(),
        vec!["lower".to_string(), "open-router".to_string()]
    );
    assert_eq!(w.get("open-router").as_deref(), Some("v123456789"));
    // Unix: `std::env::var("ZA_SECRET_OPEN_ROUTER")` таких имён не видел.
    let u = EnvStore::from_vars_with(vars(&list), false);
    assert!(u.names().is_empty(), "{:?}", u.names());
}

#[cfg(unix)]
#[test]
fn non_utf8_is_skipped() {
    use std::os::unix::ffi::OsStringExt;
    let s = EnvStore::from_vars_with(
        vec![
            (
                OsString::from("ZA_SECRET_BAD"),
                OsString::from_vec(vec![0xff, 0xfe, b'a']),
            ),
            (
                OsString::from_vec(b"ZA_SECRET_\xff".to_vec()),
                OsString::from("v-12345678"),
            ),
        ]
        .into_iter(),
        false,
    );
    assert!(s.names().is_empty());
}

#[cfg(windows)]
#[test]
fn non_utf16_is_skipped() {
    use std::os::windows::ffi::OsStringExt;
    let s = EnvStore::from_vars_with(
        vec![(
            OsString::from("ZA_SECRET_BAD"),
            OsString::from_wide(&[0xD800, 0x61]),
        )]
        .into_iter(),
        true,
    );
    assert!(s.names().is_empty());
}

#[test]
fn debug_shows_names_not_values() {
    let s = EnvStore::from_vars(vars(&[("ZA_SECRET_ANTHROPIC", "very-secret-value-42")]));
    let d = format!("{s:?}");
    assert!(d.contains("anthropic"), "{d}");
    assert!(!d.contains("very-secret-value-42"), "{d}");
}

#[test]
fn known_values_keep_long_ones_only() {
    let s = EnvStore::from_vars(vars(&[
        ("ZA_SECRET_LONG", "abcdefgh"),
        ("ZA_SECRET_SHORT", "1234567"),
        // Длина — в символах, не в байтах: 7 кириллических букв — 14 байт.
        ("ZA_SECRET_CYR", "ключзна"),
    ]));
    assert_eq!(
        known_values(&s),
        vec![("long".to_string(), "abcdefgh".to_string())]
    );
}
