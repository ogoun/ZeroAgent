//! Разбор `robots.txt` (RFC 9309, D107) — без сети.

use za_web::robots_allows;

#[test]
fn robots_table() {
    let r = "User-agent: *\nDisallow: /private\nAllow: /private/ok\n\nUser-agent: zero-agent\nDisallow: /z$\n";
    // Своя группа заменяет `*`.
    assert!(robots_allows(r, "zero-agent", "/private"));
    assert!(!robots_allows(r, "zero-agent", "/z"));
    assert!(robots_allows(r, "zero-agent", "/zz"));
    assert!(!robots_allows(r, "other", "/private/x"));
    assert!(robots_allows(r, "other", "/private/ok/1"));
}

#[test]
fn robots_rules_and_groups() {
    // Нет групп — разрешено всё; пустой Disallow — правила нет.
    assert!(robots_allows("", "zero-agent", "/a"));
    assert!(robots_allows(
        "User-agent: *\nDisallow:\n",
        "zero-agent",
        "/a"
    ));
    // Регистр имени поля и агента не важен; комментарии и CRLF; BOM в начале.
    let r = "\u{feff}USER-AGENT: Zero-Agent # мы\r\nDISALLOW: /a # тоже\r\n";
    assert!(!robots_allows(r, "zero-agent", "/a/b"));
    assert!(robots_allows(r, "zero-agent", "/b"));
    // Несколько User-agent подряд — одна группа; группы одного агента объединяются.
    let r = "User-agent: x\nUser-agent: zero-agent\nDisallow: /one\n\nUser-agent: zero-agent\nDisallow: /two\n";
    assert!(!robots_allows(r, "zero-agent", "/one"));
    assert!(!robots_allows(r, "zero-agent", "/two"));
    assert!(!robots_allows(r, "x", "/one"));
    assert!(robots_allows(r, "x", "/two"));
    // Самое длинное совпадение; при равной длине — Allow.
    let r = "User-agent: *\nDisallow: /a\nAllow: /a/b\nDisallow: /a/b/c\nAllow: /p\nDisallow: /p\n";
    assert!(!robots_allows(r, "z", "/a/x"));
    assert!(robots_allows(r, "z", "/a/b/x"));
    assert!(!robots_allows(r, "z", "/a/b/c"));
    assert!(robots_allows(r, "z", "/p"));
    // `*` — любая последовательность, `$` — конец пути; запрос входит в путь.
    let r = "User-agent: *\nDisallow: /*.pdf$\nDisallow: /s*q=\nDisallow: /x$y\n";
    assert!(!robots_allows(r, "z", "/docs/a.pdf"));
    assert!(robots_allows(r, "z", "/docs/a.pdf?x=1"));
    assert!(!robots_allows(r, "z", "/search?q=1"));
    assert!(robots_allows(r, "z", "/search?p=1"));
    // `$` не в конце шаблона — обычный символ.
    assert!(!robots_allows(r, "z", "/x$y/1"));
    // `/robots.txt` разрешён всегда (RFC 9309, 2.2.2).
    assert!(robots_allows(
        "User-agent: *\nDisallow: /\n",
        "z",
        "/robots.txt"
    ));
    assert!(!robots_allows("User-agent: *\nDisallow: /\n", "z", "/"));
    // Правила до первой группы и незнакомые поля не действуют.
    let r = "Disallow: /a\nSitemap: https://x/s.xml\nUser-agent: *\nCrawl-delay: 5\nDisallow: /b\n";
    assert!(robots_allows(r, "z", "/a"));
    assert!(!robots_allows(r, "z", "/b"));
    // Агент своей группы задаётся токеном: `zero-agent/1.0` в строке — тот же агент,
    // `zero-agentx` — другой.
    let r = "User-agent: zero-agent/1.0\nDisallow: /a\n\nUser-agent: zero-agentx\nDisallow: /b\n";
    assert!(!robots_allows(r, "zero-agent", "/a"));
    assert!(robots_allows(r, "zero-agent", "/b"));
}

#[test]
fn robots_percent_encoding_is_normalized() {
    // Незарезервированные символы в `%XX` раскрываются, не-ASCII кодируется (RFC 9309, 2.2.2).
    let r = "User-agent: *\nDisallow: /%7Euser\nDisallow: /путь\nDisallow: /a%2fb\n";
    assert!(!robots_allows(r, "z", "/~user/x"));
    assert!(!robots_allows(r, "z", "/%D0%BF%D1%83%D1%82%D1%8C/1"));
    // `%2F` — зарезервированный `/`: не равен буквальному `/`.
    assert!(!robots_allows(r, "z", "/a%2Fb"));
    assert!(robots_allows(r, "z", "/a/b"));
}

#[test]
fn robots_many_wildcards_are_fast() {
    // Шаблон из сотен `*` против длинного пути: разбор не экспоненциальный.
    let pat = "/a".to_string() + &"*a".repeat(400) + "b";
    let r = format!("User-agent: *\nDisallow: {pat}\n");
    let path = "/".to_string() + &"a".repeat(4000);
    let t = std::time::Instant::now();
    assert!(robots_allows(&r, "z", &path));
    assert!(t.elapsed() < std::time::Duration::from_secs(5));
}
