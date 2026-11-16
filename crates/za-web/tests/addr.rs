//! Фильтр адресов (D108) и разбор URL шлюза (D107, D124) — без сети.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use za_web::{FetchError, IANA_SNAPSHOT, IPV4_SPECIAL, IPV6_SPECIAL, check_ip, parse_url};

/// Реестр IANA IPv4 Special-Purpose Address Registry, переписанный из источника
/// <https://www.iana.org/assignments/iana-ipv4-special-registry/> (Last Updated 2025-10-09)
/// независимо от таблицы `addr.rs`: тест сверяет их построчно.
const REGISTRY_V4: &[&str] = &[
    "0.0.0.0/8",
    "0.0.0.0/32",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.0.0.0/29",
    "192.0.0.8/32",
    "192.0.0.9/32",
    "192.0.0.10/32",
    "192.0.0.170/32",
    "192.0.0.171/32",
    "192.0.2.0/24",
    "192.31.196.0/24",
    "192.52.193.0/24",
    "192.88.99.0/24",
    "192.88.99.2/32",
    "192.168.0.0/16",
    "192.175.48.0/24",
    "198.18.0.0/15",
    "198.51.100.0/24",
    "203.0.113.0/24",
    "240.0.0.0/4",
    "255.255.255.255/32",
];

/// Реестр IANA IPv6 Special-Purpose Address Registry
/// <https://www.iana.org/assignments/iana-ipv6-special-registry/> (Last Updated 2025-10-09).
const REGISTRY_V6: &[&str] = &[
    "::1/128",
    "::/128",
    "::ffff:0:0/96",
    "64:ff9b::/96",
    "64:ff9b:1::/48",
    "100::/64",
    "100:0:0:1::/64",
    "2001::/23",
    "2001::/32",
    "2001:1::1/128",
    "2001:1::2/128",
    "2001:1::3/128",
    "2001:2::/48",
    "2001:3::/32",
    "2001:4:112::/48",
    "2001:10::/28",
    "2001:20::/28",
    "2001:30::/28",
    "2001:db8::/32",
    "2002::/16",
    "2620:4f:8000::/48",
    "3fff::/20",
    "5f00::/16",
    "fc00::/7",
    "fe80::/10",
];

fn v4_range(a: Ipv4Addr, len: u8) -> (Ipv4Addr, Ipv4Addr) {
    let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
    let first = a.to_bits() & mask;
    (
        Ipv4Addr::from_bits(first),
        Ipv4Addr::from_bits(first | !mask),
    )
}

fn v6_range(a: Ipv6Addr, len: u8) -> (Ipv6Addr, Ipv6Addr) {
    let mask = if len == 0 {
        0
    } else {
        u128::MAX << (128 - len)
    };
    let first = a.to_bits() & mask;
    (
        Ipv6Addr::from_bits(first),
        Ipv6Addr::from_bits(first | !mask),
    )
}

#[test]
fn tables_match_iana_registries() {
    assert_eq!(IANA_SNAPSHOT, "2025-10-09");
    let v4: BTreeSet<String> = IPV4_SPECIAL.iter().map(ToString::to_string).collect();
    let want4: BTreeSet<String> = REGISTRY_V4.iter().map(ToString::to_string).collect();
    assert_eq!(v4, want4);
    assert_eq!(IPV4_SPECIAL.len(), REGISTRY_V4.len(), "повтор строки IPv4");
    // IPv6 сравнивается значениями: запись `std` (`::ffff:0.0.0.0`) не та, что в реестре.
    let v6: BTreeSet<(Ipv6Addr, u8)> = IPV6_SPECIAL.iter().map(|b| (b.addr, b.len)).collect();
    let want6: BTreeSet<(Ipv6Addr, u8)> = REGISTRY_V6
        .iter()
        .map(|s| {
            let (a, l) = s.split_once('/').unwrap();
            (a.parse().unwrap(), l.parse().unwrap())
        })
        .collect();
    assert_eq!(v6, want6);
    assert_eq!(IPV6_SPECIAL.len(), REGISTRY_V6.len(), "повтор строки IPv6");
    // Адрес блока записан началом сети: иначе строка таблицы не та, что в реестре.
    for b in IPV4_SPECIAL {
        assert_eq!(v4_range(b.addr, b.len).0, b.addr, "{b}");
    }
    for b in IPV6_SPECIAL {
        assert_eq!(v6_range(b.addr, b.len).0, b.addr, "{b}");
    }
}

#[test]
fn every_registry_block_is_blocked_at_both_ends() {
    for b in IPV4_SPECIAL {
        let (first, last) = v4_range(b.addr, b.len);
        for a in [first, last] {
            assert_eq!(
                check_ip(IpAddr::V4(a)),
                Err("local or private address"),
                "{b}: {a}"
            );
        }
    }
    for b in IPV6_SPECIAL {
        let (first, last) = v6_range(b.addr, b.len);
        for a in [first, last] {
            assert_eq!(
                check_ip(IpAddr::V6(a)),
                Err("local or private address"),
                "{b}: {a}"
            );
        }
    }
    // Вне реестра IPv4, но не одноадресное: multicast `224.0.0.0/4`.
    for a in ["224.0.0.0", "239.255.255.255"] {
        assert!(check_ip(a.parse().unwrap()).is_err(), "{a}");
    }
    // IPv6 вне `2000::/3` — отказ целиком: первый и последний адрес соседних областей.
    for a in [
        "1fff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        "4000::",
        "fec0::1",
        "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
    ] {
        assert!(check_ip(a.parse().unwrap()).is_err(), "{a}");
    }
}

#[test]
fn check_ip_table() {
    // Запрещено: каждый класс реестров IANA IPv4/IPv6 special-purpose (выборка из брифа;
    // полный обход блоков — `every_registry_block_is_blocked_at_both_ends`).
    for a in [
        "127.0.0.1",
        "10.1.2.3",
        "172.31.0.1",
        "192.168.0.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "224.0.0.1",
        "192.88.99.1",
        "::1",
        "::",
        "fd00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "::7f00:1",
        "64:ff9b::7f00:1",
        "64:ff9b:1::7f00:1",
        "2002:7f00:1::1",
        "2001::1",
        "2001:db8::1",
        "3fff::1",
        "5f00::1",
        "100::1",
        "ff02::1",
    ] {
        assert!(check_ip(a.parse().unwrap()).is_err(), "{a}");
    }
    for a in [
        "1.1.1.1",
        "8.8.8.8",
        "2606:4700:4700::1111",
        "::ffff:8.8.8.8",
        "64:ff9b::808:808",
        "2002:808:808::1",
    ] {
        assert!(check_ip(a.parse().unwrap()).is_ok(), "{a}");
    }
}

/// Вложенный IPv4: вердикт по нему (D108) только там, где положение IPv4 однозначно и
/// трансляция стандартна: `::ffff:0:0/96`, `64:ff9b::/96`, `2002::/16`.
///
/// - Локальный NAT64 `64:ff9b:1::/48` (RFC 8215) отвергается целиком, в любой форме, в том
///   числе `/96` (Ruling I1 ревью задачи 6): длину префикса и саму трансляцию выбирает
///   оператор сети, вердикт по вложенному адресу был бы догадкой.
/// - IPv4-compatible `::/96` (устарел, RFC 4291 2.5.5.1; глобально не маршрутизируется)
///   тоже отвергается целиком (M6 ревью задачи 6, строже буквы D108).
#[test]
fn embedded_ipv4_verdicts() {
    for a in ["::ffff:8.8.8.8", "64:ff9b::808:808", "2002:101:101:5::1"] {
        assert!(check_ip(a.parse().unwrap()).is_ok(), "{a}");
    }
    for a in [
        "::ffff:10.0.0.1",
        "::ffff:192.168.1.1",
        "::a00:1",
        "::808:808",
        "::101:101",
        "64:ff9b::a9fe:a9fe",
        "64:ff9b:1::808:808",
        "64:ff9b:1::101:101",
        "64:ff9b:1::7f00:1",
        "64:ff9b:1:7f00:0:100:808:808",
        "64:ff9b:1:0:1::808:808",
        "64:ff9b:1:ffff:ffff:ffff:ffff:ffff",
        "2002:a9fe:a9fe::1",
        "2002:e000:1::",
    ] {
        assert!(check_ip(a.parse().unwrap()).is_err(), "{a}");
    }
}

#[test]
fn parse_url_table() {
    assert!(matches!(
        parse_url("http://2130706433/"),
        Err(FetchError::Blocked(_))
    ));
    assert!(matches!(
        parse_url("http://0x7f.1/"),
        Err(FetchError::Blocked(_))
    ));
    assert!(matches!(
        parse_url("http://[::ffff:127.0.0.1]/"),
        Err(FetchError::Blocked(_))
    ));
    assert!(matches!(
        parse_url("http://a.localhost/"),
        Err(FetchError::Blocked(_))
    ));
    // Последняя точка (D124).
    assert!(matches!(
        parse_url("http://localhost./"),
        Err(FetchError::Blocked(_))
    ));
    assert!(matches!(
        parse_url("http://u:p@example.com/"),
        Err(FetchError::Credentials)
    ));
    assert!(matches!(
        parse_url("file:///etc/passwd"),
        Err(FetchError::Scheme)
    ));
    assert!(parse_url("https://example.com/a?b=c").is_ok());
}

#[test]
fn parse_url_more_cases() {
    for s in [
        "http://[::1]/",
        "http://LOCALHOST/",
        "http://localhost:8080/",
        "http://169.254.169.254/latest/meta-data/",
        "http://0/",
        "http://[fd00::1]:8080/",
        "http://127.1/",
    ] {
        assert_eq!(
            parse_url(s).err(),
            Some(FetchError::Blocked("local or private address".into())),
            "{s}"
        );
    }
    assert_eq!(
        parse_url("http://user@example.com/").err(),
        Some(FetchError::Credentials)
    );
    for s in [
        "ftp://example.com/",
        "javascript:alert(1)",
        "data:text/plain,x",
    ] {
        assert_eq!(parse_url(s).err(), Some(FetchError::Scheme), "{s}");
    }
    for s in [
        "not a url",
        "http://",
        "http://a..b/",
        "http://[fe80::1%25eth0]/",
    ] {
        assert!(
            matches!(parse_url(s), Err(FetchError::InvalidUrl(_))),
            "{s}"
        );
    }
    let long = format!("https://example.com/{}", "a".repeat(2048));
    assert!(matches!(parse_url(&long), Err(FetchError::InvalidUrl(_))));
    // Хост нормализуется `normalize_host`: последняя точка снята, регистр, punycode;
    // фрагмент не уходит на сервер.
    let u = parse_url("HTTP://Example.COM./a#frag").unwrap();
    assert_eq!(u.as_str(), "http://example.com/a");
    let u = parse_url("https://пример.рф/").unwrap();
    assert_eq!(u.host_str(), Some("xn--e1afmkfd.xn--p1ai"));
    // Публичный IP-литерал допустим.
    assert!(parse_url("http://8.8.8.8/").is_ok());
    assert!(parse_url("http://[2606:4700:4700::1111]/").is_ok());
}

#[test]
fn fetch_error_texts() {
    let t = |e: FetchError| e.text();
    assert_eq!(
        t(FetchError::Encoding("gzip".into())),
        "unsupported content encoding gzip"
    );
    assert_eq!(
        t(FetchError::Credentials),
        "credentials in URL are not allowed"
    );
    assert_eq!(
        t(FetchError::Blocked("local or private address".into())),
        "blocked: local or private address"
    );
    assert_eq!(
        t(FetchError::Blocked("port 25 is not allowed".into())),
        "blocked: port 25 is not allowed"
    );
    assert_eq!(
        t(FetchError::Redirect("http://other.test/".into())),
        "redirected to http://other.test/; call web_fetch with it if it is the page you need"
    );
    assert_eq!(
        t(FetchError::Robots("unavailable")),
        "blocked by robots.txt: unavailable"
    );
    // Тексты для модели — ASCII (D23, Global Constraints).
    for e in [
        FetchError::InvalidUrl("x".into()),
        FetchError::Scheme,
        FetchError::Timeout,
        FetchError::Network("too many redirects".into()),
    ] {
        assert!(e.text().is_ascii(), "{e:?}");
    }
}
