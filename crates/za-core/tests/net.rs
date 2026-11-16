//! Нормализация хоста (R1-C2 D124) и сквозная таблица: сырой хост `za_shell` →
//! `shell_hosts` (сбор с `normalize_host`).

use za_core::net::{normalize_host, shell_hosts};
use za_core::shell::{Dialect, segments};

#[test]
fn normalize_host_table() {
    for (raw, want) in [
        ("Example.COM", Some("example.com")),
        ("evil.test.", Some("evil.test")),
        ("localhost.", Some("localhost")),
        ("LOCALHOST", Some("localhost")),
        ("пример.рф", Some("xn--e1afmkfd.xn--p1ai")),
        ("ПРИМЕР.РФ.", Some("xn--e1afmkfd.xn--p1ai")),
        ("xn--e1afmkfd.xn--p1ai", Some("xn--e1afmkfd.xn--p1ai")),
        ("0x7f.1", Some("127.0.0.1")),
        ("10.0.0.1", Some("10.0.0.1")),
        // Каноническая запись `url::Host` (WHATWG): IPv4 внутри IPv6 — шестнадцатерично.
        ("[::FFFF:127.0.0.1]", Some("::ffff:7f00:1")),
        ("[::1]", Some("::1")),
        ("", None),
        (".", None),
        ("a b", None),
        ("a%2fb", None),
        ("[::1", None),
        // Пустая метка после снятия одной точки (M5 ревью задачи 3).
        ("evil.test..", None),
        ("a..b", None),
        (".a", None),
        ("..", None),
        // Точка, которую даёт отображение UTS 46 (U+3002), — те же проверки (m2 повторного
        // ревью задачи 3).
        ("a\u{3002}.test", None),
        ("a.test\u{3002}", None),
        ("a\u{3002}test", Some("a.test")),
    ] {
        assert_eq!(normalize_host(raw).as_deref(), want, "{raw}");
    }
}

#[test]
fn shell_hosts_normalized() {
    for (cmd, want, unknown) in [
        (
            "curl https://пример.рф/",
            vec!["xn--e1afmkfd.xn--p1ai"],
            false,
        ),
        ("curl https://Evil.TEST./x", vec!["evil.test"], false),
        (
            "wget http://[::FFFF:127.0.0.1]:8080/",
            vec!["::ffff:7f00:1"],
            false,
        ),
        // Повтор после нормализации — один хост.
        (
            "curl https://A.test/ ; curl https://a.test./",
            vec!["a.test"],
            false,
        ),
        // `None` от `normalize_host` — хост неизвестен, а не пропуск (I1 ревью задачи 3).
        ("curl https://1.2.3.4.5/", vec![], true),
        ("curl https://xn--a.test/", vec![], true),
        (
            "curl https://a.test/ https://1.2.3.4.5/",
            vec!["a.test"],
            true,
        ),
        ("curl https://evil.test../", vec![], true),
        ("git status", vec![], false),
    ] {
        let d = Dialect::Bash;
        let got = shell_hosts(&segments(cmd, d), d);
        let want: Vec<String> = want.into_iter().map(String::from).collect();
        assert_eq!(got, (want, unknown), "{cmd}");
    }
}
