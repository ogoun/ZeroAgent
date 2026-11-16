//! Разбор `robots.txt` (RFC 9309, D107) — без сети.

use za_web::{ROBOTS_MAX_BYTES, ROBOTS_MAX_RULES, robots_allows};

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

/// M4 ревью задачи 6: разбор ограничен текстом 500 КиБ (как у основных поисковиков,
/// RFC 9309 2.5) — правило дальше предела не действует; обрезка — по последней целой
/// строке.
#[test]
fn robots_text_beyond_500_kib_is_ignored() {
    let mut r = String::from("User-agent: *\nDisallow: /early\n");
    while r.len() < ROBOTS_MAX_BYTES - 100 {
        r.push_str("# filler filler\n");
    }
    // Строка, пересекающая предел, отбрасывается целиком.
    let cross = "Disallow: /cross\n";
    let pad = ROBOTS_MAX_BYTES - r.len() - cross.len() + 3;
    r.push_str(&"#".repeat(pad));
    r.push('\n');
    r.push_str(cross);
    r.push_str("Disallow: /late\n");
    assert!(r.len() > ROBOTS_MAX_BYTES);
    assert!(!robots_allows(&r, "z", "/early"));
    assert!(robots_allows(&r, "z", "/cross"));
    assert!(robots_allows(&r, "z", "/late"));
    // Тот же текст до предела действует целиком.
    let short = "User-agent: *\nDisallow: /early\nDisallow: /late\n";
    assert!(!robots_allows(short, "z", "/late"));
}

/// M4: число правил группы агента ограничено (`ROBOTS_MAX_RULES`): дальше правила не
/// действуют — разбор и сопоставление не растут с текстом.
#[test]
fn robots_rule_count_is_capped() {
    let mut r = String::from("User-agent: *\n");
    for i in 0..ROBOTS_MAX_RULES - 1 {
        r.push_str(&format!("Disallow: /d{i}/\n"));
    }
    r.push_str("Disallow: /last-kept\n");
    r.push_str("Disallow: /dropped\n");
    assert!(!robots_allows(&r, "z", "/d0/x"));
    assert!(!robots_allows(&r, "z", "/last-kept"));
    assert!(robots_allows(&r, "z", "/dropped"));
    // Пустой `Disallow` правилом не считается и место не занимает.
    let mut r = String::from("User-agent: *\n");
    for _ in 0..ROBOTS_MAX_RULES + 10 {
        r.push_str("Disallow:\n");
    }
    r.push_str("Disallow: /kept\n");
    assert!(!robots_allows(&r, "z", "/kept"));
    // Правила чужих групп в предел не входят.
    let mut r = String::from("User-agent: other\n");
    for i in 0..ROBOTS_MAX_RULES + 10 {
        r.push_str(&format!("Disallow: /o{i}\n"));
    }
    r.push_str("\nUser-agent: *\nDisallow: /mine\n");
    assert!(!robots_allows(&r, "z", "/mine"));
}

/// M4: худший случай у предела текста и длины URL — доли секунды даже в отладочной сборке
/// (в release — единицы миллисекунд, замер в отчёте). Прежняя динамика O(шаблон × путь)
/// давала около 0,5 с в release.
#[test]
fn robots_worst_case_is_bounded() {
    let fill = |line: &str| {
        let mut r = String::from("User-agent: *\n");
        while r.len() + line.len() < ROBOTS_MAX_BYTES {
            r.push_str(line);
        }
        r
    };
    let path = format!("/{}", "a".repeat(2000));
    for r in [
        fill(&format!("Disallow: /{}b\n", "*a".repeat(500))),
        fill(&format!("Disallow: /{}b\n", "a".repeat(1990))),
        fill("Disallow: /*b\n"),
    ] {
        let t = std::time::Instant::now();
        assert!(robots_allows(&r, "z", &path));
        let spent = t.elapsed();
        assert!(spent < std::time::Duration::from_secs(1), "{spent:?}");
    }
}

/// Сопоставление `*` и `$` жадным разбором совпадает с определением RFC 9309.
#[test]
fn robots_wildcard_matching_cases() {
    let m = |pat: &str, path: &str| {
        let r = format!("User-agent: *\nDisallow: {pat}\n");
        !robots_allows(&r, "z", path)
    };
    // Буквальный шаблон — префикс пути; с `$` — путь целиком.
    assert!(m("/a/b", "/a/bc"));
    assert!(!m("/a/b$", "/a/bc"));
    assert!(m("/a/b$", "/a/b"));
    // `*` в середине, подряд, в конце; самое левое вхождение не мешает следующему.
    assert!(m("/a*c", "/abbbc"));
    assert!(m("/a**c", "/ac"));
    assert!(m("/a*", "/a"));
    assert!(m("/*a*b*c$", "/xaybzc"));
    assert!(!m("/*a*b*c$", "/xaybzcd"));
    assert!(m("/*ab*ab$", "/abab"));
    assert!(!m("/*aba*aba$", "/ababa"));
    assert!(m("/*aba*aba$", "/abaaba"));
    // Конец `$` после `*`: хвост пути не перекрывает уже совпавшую часть.
    assert!(!m("/*xy*y$", "/xy"));
    assert!(m("/*xy*y$", "/xyy"));
    assert!(m("/*$", "/anything"));
    assert!(m("*", "/"));
    // Не с начала пути — не совпадение.
    assert!(!m("/b", "/ab"));
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
