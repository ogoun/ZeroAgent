//! Нормализация хоста (R1-C2 D124) и сквозная таблица: сырой хост `za_shell` →
//! `normalize_host`.

use za_core::net::normalize_host;
use za_core::shell::{Dialect, network_hosts, segments};

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
    ] {
        assert_eq!(normalize_host(raw).as_deref(), want, "{raw}");
    }
}

#[test]
fn shell_hosts_normalized() {
    for (cmd, want) in [
        ("curl https://пример.рф/", vec!["xn--e1afmkfd.xn--p1ai"]),
        ("curl https://Evil.TEST./x", vec!["evil.test"]),
        (
            "wget http://[::FFFF:127.0.0.1]:8080/",
            vec!["::ffff:7f00:1"],
        ),
    ] {
        let d = Dialect::Bash;
        let got: Vec<String> = segments(cmd, d)
            .iter()
            .flat_map(|s| network_hosts(s, d).hosts)
            .filter_map(|h| normalize_host(&h))
            .collect();
        assert_eq!(got, want, "{cmd}");
    }
}
