//! Core pack (D120): состав, разбор без предупреждений, формат текстов, обнаружение и
//! загрузка поставки.

use std::path::Path;

use za_protocol::skill_source;
use za_skills::{DiscoverInput, PACK, PACK_TOKEN_LIMIT, bundled, discover, load, parse_skill_md};

#[test]
fn pack_is_complete_and_valid() {
    let names: Vec<_> = bundled().iter().map(|(n, _)| *n).collect();
    assert_eq!(
        names,
        [
            "diagnose",
            "domain-model",
            "doubt",
            "grill",
            "handoff",
            "research-with-citations",
            "retro",
            "review",
            "router",
            "source-driven",
            "spec",
            "tdd",
            "tickets",
            "wayfinder"
        ]
    );
    for (n, t) in bundled() {
        let p = parse_skill_md(t, n).unwrap();
        assert!(p.warnings.is_empty(), "{n}");
        assert!(t.is_ascii(), "{n}");
    }
    assert_eq!(names, PACK);
    assert_eq!(PACK_TOKEN_LIMIT, 700);
}

/// Формат D120: frontmatter только `name` (= имя каталога) и `description` («Use when …»,
/// целиком в 160 символах каталога D117), тело — шаги, критерий остановки и запреты.
/// Угловых скобок нет: `escape_untrusted` при выдаче превратил бы их в `&lt;`/`&gt;`.
#[test]
fn pack_texts_follow_format() {
    for (n, t) in bundled() {
        let p = parse_skill_md(t, n).unwrap();
        let keys: Vec<&str> = p.fields.keys().map(String::as_str).collect();
        assert_eq!(keys, ["description", "name"], "{n}");
        assert_eq!(p.name.as_deref(), Some(*n));
        assert!(
            p.description.starts_with("Use when "),
            "{n}: {}",
            p.description
        );
        assert!(p.description.len() <= 160, "{n}: {}", p.description.len());
        for part in ["## Stop when\n", "## Do not\n"] {
            assert!(p.body.contains(part), "{n}: {part}");
        }
        assert!(!t.contains(['<', '>', '\r', '\t']), "{n}");
        assert!(t.ends_with(".\n") && !t.ends_with("\n\n"), "{n}");
    }
}

/// Ruling m3 ревью задачи 12: набор инструментов run задают тир, режим и конфигурация, и
/// нужного может не быть (`plan_update` — до R1-D, `web_search` — без `[web.search] url`,
/// `activate_skill` — на small). Поэтому текст поставки называет инструмент только условно:
/// строка с его именем содержит `if `. Без условия текст должен обходиться без имени.
#[test]
fn pack_texts_name_tools_conditionally() {
    const OPTIONAL: [&str; 6] = [
        "activate_skill",
        "ask_user",
        "glob",
        "plan_update",
        "web_fetch",
        "web_search",
    ];
    for (n, t) in bundled() {
        for line in t.lines() {
            if let Some(tool) = OPTIONAL.iter().find(|tool| line.contains(*tool)) {
                assert!(line.contains("if "), "{n}: {tool} без условия: {line}");
            }
        }
    }
}

/// `router` сам перечисляет скиллы поставки строками `- <шаг>: <имя>`: так он работает и без
/// каталога (small, `/router`). Новый скилл поставки должен попасть и в этот список.
#[test]
fn router_lists_every_pack_skill() {
    let (_, router) = bundled().iter().find(|(n, _)| *n == "router").unwrap();
    for n in PACK.iter().filter(|n| **n != "router") {
        assert!(router.contains(&format!(": {n}\n")), "{n}");
    }
}

/// Каталоги `pack/` на диске — ровно [`PACK`]: новый скилл не теряется молча, а текст в
/// бинарнике — байты файла.
#[test]
fn pack_dirs_match_list() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("pack");
    let mut on_disk: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    on_disk.sort();
    assert_eq!(on_disk, PACK);
    for (n, t) in bundled() {
        let file = std::fs::read_to_string(root.join(n).join("SKILL.md")).unwrap();
        assert_eq!(file, *t, "{n}");
    }
}

/// Поставка в каталоге — источник `bundled` без пропусков; `load` находит текст по хэшу.
#[test]
fn pack_is_discovered_and_loaded() {
    let cat = discover(&DiscoverInput {
        bundled: bundled(),
        ..DiscoverInput::default()
    });
    assert!(cat.skipped.is_empty(), "{:?}", cat.skipped);
    let names: Vec<&str> = cat.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, PACK);
    for s in &cat.skills {
        assert_eq!(s.source, skill_source::BUNDLED);
        assert_eq!(s.display, format!("bundled:{}", s.name));
        assert!(s.files.is_empty() && s.warnings.is_empty(), "{}", s.name);
        let body = load(s, None).unwrap();
        assert!(body.starts_with("# "), "{}", s.name);
        assert!(body.contains("## Stop when\n"), "{}", s.name);
    }
}
