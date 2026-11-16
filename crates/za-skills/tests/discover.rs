//! Обход доверенных деревьев, обнаружение и загрузка скиллов (D114, D116, D118).
//!
//! Каталоги тестов — `CARGO_TARGET_TMPDIR/za-skills/<name>-<pid>-<n>` (внутри проекта).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use za_core::paths::Roots;
use za_protocol::skill_source;
use za_skills::{
    Catalog, DiscoverInput, EXTERNAL_LINK, LOCAL_TREE_LIMITS, Limits, LoadError, PROJECT_TREES,
    Place, ProjectSkills, TRUST_LIMITS, TreeLimits, TreeListing, discover, file_value, load,
    tree_hash, tree_listing,
};

/// Свой каталог на процесс и вызов.
fn dir(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("za-skills")
        .join(format!("{name}-{}-{n}", std::process::id()));
    if d.exists() {
        std::fs::remove_dir_all(&d).unwrap();
    }
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn hex(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn skill_text(name: &str, desc: &str) -> String {
    format!("---\nname: {name}\ndescription: {desc}\n---\nBody of {name}.\n")
}

/// `<root>/<rel>/SKILL.md` с именем `name`.
fn skill(root: &Path, rel: &str, name: &str, desc: &str) {
    write(&root.join(rel).join("SKILL.md"), &skill_text(name, desc));
}

fn roots(p: &Path) -> Roots {
    Roots::canonical(p, p).unwrap()
}

/// Списки деревьев проекта — так, как их даст бинарник после сверки доверия.
fn project_skills(project: &Path) -> ProjectSkills {
    let roots = roots(project);
    let trees = PROJECT_TREES
        .iter()
        .filter_map(|rel| tree_listing(&roots, rel, &TRUST_LIMITS).unwrap().1)
        .collect();
    ProjectSkills { roots, trees }
}

fn reasons(cat: &Catalog) -> Vec<(String, String)> {
    cat.skipped
        .iter()
        .map(|s| (s.path.clone(), s.reason.clone()))
        .collect()
}

static NO_BUNDLED: &[(&str, &str)] = &[];

/// Ссылка на каталог: junction на Windows (`mklink /J` прав не требует), символическая
/// ссылка на Unix (как в `zero-agent/tests/trust.rs`).
fn dir_link(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
        // `mklink` понимает только `\`: путь собирается заново по компонентам.
        let native = |p: &Path| p.components().collect::<PathBuf>();
        let out = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(native(link))
            .arg(native(target))
            .output()
            .expect("cmd не запустился");
        assert!(
            out.status.success(),
            "mklink /J: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// Ссылка на файл. На Windows требует режима разработчика или прав администратора: без
/// них — `None`, и тест явно пропускает проверку с сообщением.
#[cfg(unix)]
fn file_link(link: &Path, target: &Path) -> Option<()> {
    std::os::unix::fs::symlink(target, link).unwrap();
    Some(())
}

/// См. Unix-вариант выше.
#[cfg(windows)]
fn file_link(link: &Path, target: &Path) -> Option<()> {
    match std::os::windows::fs::symlink_file(target, link) {
        Ok(()) => Some(()),
        Err(e) => {
            // `ZA_REQUIRE_LINK_TESTS=1` (CI) — пропуск — падение.
            let why = format!(
                "ссылка на файл не создана ({e}); нужен режим разработчика или права администратора"
            );
            if std::env::var("ZA_REQUIRE_LINK_TESTS").as_deref() == Ok("1") {
                panic!("ZA_REQUIRE_LINK_TESTS=1, а проверка пропущена: {why}");
            }
            eprintln!("ПРОПУСК: {why}");
            None
        }
    }
}

#[test]
fn tree_hash_matches_r1c1_golden() {
    // Хэш дерева посчитан кодом R1-C1 (`zero_agent::trust::trust_set` на 3f66315) на этой
    // фикстуре до выноса обхода: существующие доверия действуют.
    const GOLDEN: &str = "eb13e6150daaa3418b63797e969d3fb2ca0825757add6bca139e26e946fc8bfc";
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tree");
    let (place, listing) =
        tree_listing(&roots(&root), ".zero-agent/skills", &TRUST_LIMITS).unwrap();
    assert_eq!(place, Place::Found);
    let listing = listing.unwrap();
    assert_eq!(listing.rel, ".zero-agent/skills");
    let keys: Vec<&str> = listing.files.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "spec/SKILL.md",
            "spec/scripts/run.py",
            "tdd-x/SKILL.md",
            "tdd-x/ref/notes.md"
        ]
    );
    let run = std::fs::read(root.join(".zero-agent/skills/spec/scripts/run.py")).unwrap();
    assert_eq!(listing.files["spec/scripts/run.py"], hex(&run));
    assert_eq!(tree_hash(&listing), GOLDEN);
    // Нет дерева — `Missing`; путь — файл, а не каталог — `Found` без списка.
    let r = roots(&root);
    assert_eq!(
        tree_listing(&r, ".claude/skills", &TRUST_LIMITS).unwrap(),
        (Place::Missing, None)
    );
    let f = tree_listing(&r, ".zero-agent/skills/spec/SKILL.md", &TRUST_LIMITS).unwrap();
    assert_eq!(f, (Place::Found, None));
}

#[test]
fn trust_walk_limits() {
    // Файл больше 1 МиБ — значение `too-large:<size>` без хэша содержимого; ровно 1 МиБ —
    // обычный хэш.
    let p = dir("walk-limits");
    write(&p.join(".claude/skills/x/SKILL.md"), "s");
    std::fs::write(p.join(".claude/skills/x/big.bin"), vec![1u8; 2 << 20]).unwrap();
    let edge = vec![2u8; 1 << 20];
    std::fs::write(p.join(".claude/skills/x/edge.bin"), &edge).unwrap();
    let (place, l) = tree_listing(&roots(&p), ".claude/skills", &TRUST_LIMITS).unwrap();
    assert_eq!(place, Place::Found);
    let l = l.unwrap();
    assert_eq!(l.files["x/big.bin"], "too-large:2097152");
    assert_eq!(l.files["x/edge.bin"], hex(&edge));
    assert_eq!(l.files["x/SKILL.md"], hex(b"s"));
    // Unix: файл без прав на чтение — тоже `too-large`, его не открывают (кроме root, у
    // которого права не проверяются: тогда проверка пропускается).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let big = p.join(".claude/skills/x/big.bin");
        std::fs::set_permissions(&big, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&big).is_err() {
            let (_, l) = tree_listing(&roots(&p), ".claude/skills", &TRUST_LIMITS).unwrap();
            assert_eq!(l.unwrap().files["x/big.bin"], "too-large:2097152");
        }
        std::fs::set_permissions(&big, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    // Граница числа записей на малом пределе: 3 записи — список, 4 — `Link`.
    let small = TreeLimits {
        max_file: 1 << 20,
        max_entries: 3,
    };
    let q = dir("walk-entries-small");
    write(&q.join("t/a/f1"), "1");
    write(&q.join("t/a/f2"), "2");
    let (place, l) = tree_listing(&roots(&q), "t", &small).unwrap();
    assert_eq!(place, Place::Found);
    assert_eq!(l.unwrap().files.len(), 2);
    write(&q.join("t/a/f3"), "3");
    assert_eq!(
        tree_listing(&roots(&q), "t", &small).unwrap(),
        (Place::Link, None)
    );

    // 10 001 запись при пределах доверия — дерево не читается (значение `external-link`);
    // граница «ровно предел — список» проверена выше на малом пределе.
    assert_eq!(
        TRUST_LIMITS,
        TreeLimits {
            max_file: 1 << 20,
            max_entries: 10_000
        }
    );
    let w = dir("walk-entries");
    let t = w.join(".claude/skills");
    std::fs::create_dir_all(&t).unwrap();
    for i in 0..10_001 {
        std::fs::write(t.join(format!("f{i:05}")), "").unwrap();
    }
    assert_eq!(
        tree_listing(&roots(&w), ".claude/skills", &TRUST_LIMITS).unwrap(),
        (Place::Link, None)
    );
    std::fs::remove_dir_all(&w).unwrap();
}

#[test]
fn discover_precedence() {
    static BUNDLED: &[(&str, &str)] = &[
        ("tdd", "---\nname: tdd\ndescription: bundled tdd\n---\nb\n"),
        (
            "spec",
            "---\nname: spec\ndescription: bundled spec\n---\nb\n",
        ),
    ];
    let root = dir("precedence");
    let user = root.join("user");
    let extra = root.join("extra");
    let project = root.join("project");
    skill(&user, "tdd", "tdd", "user tdd");
    skill(&extra, "tdd", "tdd", "extra tdd");
    skill(&project, ".claude/skills/tdd", "tdd", "project tdd");
    skill(&project, ".claude/skills/spec", "spec", "project spec");
    let cat = discover(&DiscoverInput {
        project: Some(project_skills(&project)),
        user_dir: Some(user.clone()),
        extra_dirs: vec![extra],
        bundled: BUNDLED,
        limits: Limits::default(),
    });
    let tdd = cat.get("tdd").unwrap();
    assert_eq!(tdd.source, skill_source::USER);
    assert_eq!(tdd.description, "user tdd");
    assert!(tdd.dir.as_deref().unwrap().ends_with("tdd"));
    let spec = cat.get("spec").unwrap();
    assert_eq!(spec.source, skill_source::BUNDLED);
    assert_eq!(spec.dir, None);
    assert_eq!(spec.hash, hex(BUNDLED[1].1.as_bytes()));
    let r = reasons(&cat);
    let shadows_user = r
        .iter()
        .filter(|(_, why)| why == "shadows user skill tdd")
        .count();
    assert_eq!(shadows_user, 3, "{r:?}");
    assert!(
        r.iter()
            .any(|(path, why)| path == ".claude/skills/spec" && why == "shadows bundled skill spec"),
        "{r:?}"
    );
    assert_eq!(cat.skills.len(), 2);
    // Порядок каталога — по старшинству источника.
    assert_eq!(cat.skills[0].source, skill_source::USER);
    assert_eq!(cat.skills[1].source, skill_source::BUNDLED);
}

#[test]
fn order_within_source() {
    let root = dir("order");
    // Проект: `.zero-agent/skills` старше `.claude/skills`, в каком бы порядке ни пришли
    // списки деревьев.
    let project = root.join("project");
    skill(&project, ".zero-agent/skills/x", "x", "from zero-agent");
    skill(&project, ".claude/skills/x", "x", "from claude");
    let mut ps = project_skills(&project);
    ps.trees.reverse();
    assert_eq!(ps.trees[0].rel, ".claude/skills");
    // extra: по порядку списка.
    let a = root.join("a");
    let b = root.join("b");
    skill(&a, "y", "y", "from a");
    skill(&b, "y", "y", "from b");
    // Два каталога с одним `name` в одном корне: первый по пути остаётся.
    let user = root.join("user");
    skill(&user, "d1", "dup", "first");
    skill(&user, "d2", "dup", "second");
    let cat = discover(&DiscoverInput {
        project: Some(ps),
        user_dir: Some(user),
        extra_dirs: vec![b, a],
        bundled: NO_BUNDLED,
        limits: Limits::default(),
    });
    let x = cat.get("x").unwrap_or_else(|| panic!("{cat:?}"));
    assert_eq!(x.description, "from zero-agent");
    assert_eq!(x.display, ".zero-agent/skills/x");
    assert_eq!(cat.get("y").unwrap().description, "from b");
    assert_eq!(cat.get("dup").unwrap().description, "first");
    let r = reasons(&cat);
    assert!(
        r.iter()
            .any(|(path, why)| path.ends_with("d2") && why == "duplicate skill dup"),
        "{r:?}"
    );
    assert!(
        r.iter()
            .any(|(path, why)| path == ".claude/skills/x" && why == "duplicate skill x"),
        "{r:?}"
    );
    assert!(
        r.iter().any(|(path, why)| path.ends_with("y")
            && Path::new(path).starts_with(root.join("a"))
            && why == "duplicate skill y"),
        "{r:?}"
    );
}

#[test]
fn discover_limits() {
    let root = dir("limits");
    // Глубина: каталог скилла в 6 компонентах найден, в 7 — нет.
    let deep = root.join("deep");
    skill(&deep, "d1/d2/d3/d4/d5/d6", "d6", "six");
    skill(&deep, "e1/e2/e3/e4/e5/e6/e7", "e7", "seven");
    let cat = discover(&DiscoverInput {
        user_dir: Some(deep),
        ..DiscoverInput::default()
    });
    assert!(cat.get("d6").is_some());
    assert!(cat.get("e7").is_none());

    // Каталогов больше `Limits.dirs` у user (считаются на обходе) — `limit reached`;
    // найденное до предела остаётся. Предел каталогов — малый: при `LOCAL_TREE_LIMITS`
    // (1000 записей) 2000 каталогов у `user` и `extra` недостижимы.
    let few_dirs = Limits {
        dirs: 20,
        ..Limits::default()
    };
    let many = root.join("many");
    skill(&many, "a", "a", "first");
    for i in 0..21 {
        write(&many.join(format!("s{i:04}/f.txt")), "f");
    }
    let cat = discover(&DiscoverInput {
        user_dir: Some(many.clone()),
        limits: few_dirs,
        ..DiscoverInput::default()
    });
    assert!(cat.get("a").is_some());
    assert!(
        cat.skipped
            .iter()
            .any(|s| s.reason == "limit reached: more than 20 directories"),
        "{:?}",
        reasons(&cat)
    );
    // Пустые каталоги в счёт не идут.
    let empty = root.join("empty");
    skill(&empty, "a", "a", "first");
    for i in 0..21 {
        std::fs::create_dir_all(empty.join(format!("s{i:04}"))).unwrap();
    }
    let cat = discover(&DiscoverInput {
        user_dir: Some(empty),
        limits: few_dirs,
        ..DiscoverInput::default()
    });
    assert!(cat.get("a").is_some());
    assert!(cat.skipped.is_empty(), "{:?}", reasons(&cat));

    // Предел записей обхода `user` и `extra` (ревью задачи 11, m3): ровно 1000 записей —
    // скиллы найдены; 1001 — корень не читается, `limit reached: more than 1000
    // entries`, а следующий корень `extra` обходится (m2 ревью задачи 12).
    assert_eq!(
        LOCAL_TREE_LIMITS,
        TreeLimits {
            max_file: 1 << 20,
            max_entries: 1000
        }
    );
    let edge = root.join("entries");
    skill(&edge, "a", "a", "first");
    // `a`, `a/SKILL.md` и 998 файлов в корне — 1000 записей.
    for i in 0..998 {
        write(&edge.join(format!("f{i:04}")), "");
    }
    let cat = discover(&DiscoverInput {
        user_dir: Some(edge.clone()),
        ..DiscoverInput::default()
    });
    assert!(cat.get("a").is_some(), "{:?}", reasons(&cat));
    write(&edge.join("f0998"), "");
    let other = root.join("other");
    skill(&other, "b", "b", "second extra");
    let cat = discover(&DiscoverInput {
        extra_dirs: vec![edge.clone(), other.clone()],
        ..DiscoverInput::default()
    });
    let names: Vec<&str> = cat.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["b"], "{:?}", reasons(&cat));
    assert_eq!(cat.skills[0].source, "extra");
    assert_eq!(
        reasons(&cat),
        [(
            edge.display().to_string(),
            "limit reached: more than 1000 entries".to_owned()
        )]
    );
    // Предел записей задаёт `Limits::entries` (`[skills] max_entries`, задача 14): с
    // пределом 1001 тот же корень читается; с пределом 2 корень `user` пропущен, а у
    // `other` ровно 2 записи (`b`, `b/SKILL.md`) — он читается.
    let cat = discover(&DiscoverInput {
        extra_dirs: vec![edge.clone(), other.clone()],
        limits: Limits {
            entries: 1001,
            ..Limits::default()
        },
        ..DiscoverInput::default()
    });
    assert!(
        cat.get("a").is_some() && cat.get("b").is_some(),
        "{:?}",
        reasons(&cat)
    );
    assert!(cat.skipped.is_empty(), "{:?}", reasons(&cat));
    let cat = discover(&DiscoverInput {
        user_dir: Some(edge.clone()),
        extra_dirs: vec![other],
        limits: Limits {
            entries: 2,
            ..Limits::default()
        },
        ..DiscoverInput::default()
    });
    assert!(cat.get("b").is_some(), "{:?}", reasons(&cat));
    assert_eq!(
        reasons(&cat),
        [(
            edge.display().to_string(),
            "limit reached: more than 2 entries".to_owned()
        )]
    );

    // У project каталоги считаются по путям файлов списка доверия.
    let project = root.join("project");
    skill(&project, ".claude/skills/a", "a", "first");
    let mut ps = project_skills(&project);
    for i in 0..2001 {
        ps.trees[0]
            .files
            .insert(format!("s{i:04}/f.txt"), hex(b"f"));
    }
    let cat = discover(&DiscoverInput {
        project: Some(ps),
        ..DiscoverInput::default()
    });
    assert!(cat.get("a").is_some());
    assert!(
        cat.skipped
            .iter()
            .any(|s| s.reason.starts_with("limit reached")),
        "{:?}",
        reasons(&cat)
    );

    // `.git`, `node_modules`, `target`, `__pycache__` не обходятся и в счёт не идут.
    let skip = root.join("skip");
    skill(&skip, "ok", "ok", "found");
    for d in [".git", "node_modules", "target", "__pycache__"] {
        skill(&skip, &format!("{d}/hidden"), "hidden", "not found");
    }
    for i in 0..2001 {
        write(&skip.join(format!("node_modules/m{i:04}/f.txt")), "f");
    }
    let cat = discover(&DiscoverInput {
        user_dir: Some(skip.clone()),
        ..DiscoverInput::default()
    });
    assert!(cat.get("ok").is_some());
    assert!(cat.get("hidden").is_none());
    assert!(cat.skipped.is_empty(), "{:?}", reasons(&cat));
    // В проекте — так же: список доверия их содержит, обнаружение их пропускает.
    let project = root.join("project-skip");
    skill(&project, ".claude/skills/ok", "ok", "found");
    skill(
        &project,
        ".claude/skills/.git/hidden",
        "hidden",
        "not found",
    );
    let cat = discover(&DiscoverInput {
        project: Some(project_skills(&project)),
        ..DiscoverInput::default()
    });
    assert!(cat.get("ok").is_some());
    assert!(cat.get("hidden").is_none());

    // Файлов скилла больше `files_per_skill` — список обрезан, предупреждение.
    let lots = root.join("lots");
    skill(&lots, "big", "big", "many files");
    for i in 0..5 {
        write(&lots.join(format!("big/r{i}.md")), "r");
    }
    let cat = discover(&DiscoverInput {
        user_dir: Some(lots),
        limits: Limits {
            files_per_skill: 3,
            ..Limits::default()
        },
        ..DiscoverInput::default()
    });
    let big = cat.get("big").unwrap();
    assert_eq!(big.files.len(), 3);
    assert!(
        big.warnings.iter().any(|w| w.starts_with("limit reached")),
        "{:?}",
        big.warnings
    );
}

#[test]
fn nested_skill_not_discovered() {
    let user = dir("nested");
    skill(&user, "a", "a", "outer");
    skill(&user, "a/b", "b", "inner");
    let cat = discover(&DiscoverInput {
        user_dir: Some(user),
        ..DiscoverInput::default()
    });
    assert_eq!(cat.skills.len(), 1);
    let a = cat.get("a").unwrap();
    assert!(cat.get("b").is_none());
    // Вложенный `SKILL.md` — ресурс внешнего скилла.
    assert!(a.files.contains_key("b/SKILL.md"), "{:?}", a.files);
}

#[test]
fn project_file_hash_mismatch_skipped() {
    let project = dir("mismatch");
    skill(&project, ".claude/skills/x", "x", "trusted");
    skill(&project, ".claude/skills/y", "y", "trusted");
    let ps = project_skills(&project);
    // После доверия `SKILL.md` изменён: список несёт старый хэш.
    skill(&project, ".claude/skills/x", "x", "changed");
    let cat = discover(&DiscoverInput {
        project: Some(ps),
        ..DiscoverInput::default()
    });
    assert!(cat.get("x").is_none());
    assert!(cat.get("y").is_some());
    assert!(
        reasons(&cat)
            .iter()
            .any(|(path, why)| path == ".claude/skills/x" && why == "changed since trust"),
        "{:?}",
        reasons(&cat)
    );
}

#[test]
fn external_link_not_read() {
    // Значение `external-link` — файл не открывается, даже если по пути лежит годный
    // `SKILL.md`; `too-large` — тоже.
    let project = dir("external");
    skill(
        &project,
        ".claude/skills/x",
        "x",
        "valid file under a link value",
    );
    skill(
        &project,
        ".claude/skills/z",
        "z",
        "valid file under a too-large value",
    );
    let listing = TreeListing {
        rel: ".claude/skills".to_owned(),
        files: BTreeMap::from([
            ("x/SKILL.md".to_owned(), EXTERNAL_LINK.to_owned()),
            ("z/SKILL.md".to_owned(), "too-large:2097152".to_owned()),
        ]),
    };
    let cat = discover(&DiscoverInput {
        project: Some(ProjectSkills {
            roots: roots(&project),
            trees: vec![listing],
        }),
        ..DiscoverInput::default()
    });
    assert!(cat.skills.is_empty());
    let r = reasons(&cat);
    assert!(
        r.iter()
            .any(|(path, why)| path == ".claude/skills/x" && why == "external link"),
        "{r:?}"
    );
    assert!(
        r.iter()
            .any(|(path, why)| path == ".claude/skills/z" && why.starts_with("too large")),
        "{r:?}"
    );
}

/// Поля метаданных: хэш, ресурсы с хэшами, предупреждения, `allowed-tools`, очищенное
/// описание.
#[test]
fn skill_meta_fields() {
    let user = dir("meta");
    let text = "---\nname: m\ndescription: \"use\\u200B it\"\nallowed-tools: Read, Grep Bash\nhooks:\n  Stop: x\n---\nBody.\n";
    write(&user.join("m/SKILL.md"), text);
    write(&user.join("m/ref/a.md"), "resource");
    let cat = discover(&DiscoverInput {
        user_dir: Some(user.clone()),
        ..DiscoverInput::default()
    });
    let m = cat.get("m").unwrap();
    assert_eq!(m.description, "use it");
    assert_eq!(m.hash, hex(text.as_bytes()));
    assert_eq!(m.allowed_tools, ["Read", "Grep", "Bash"]);
    assert_eq!(
        m.files,
        BTreeMap::from([("ref/a.md".to_owned(), hex(b"resource"))])
    );
    assert!(
        m.warnings
            .iter()
            .any(|w| w == "executable extension ignored: hooks"),
        "{:?}",
        m.warnings
    );
    assert_eq!(m.source, skill_source::USER);
    // Нет каталога — нет источника, без ошибок.
    let cat = discover(&DiscoverInput {
        user_dir: Some(user.join("absent")),
        extra_dirs: vec![user.join("absent-too")],
        ..DiscoverInput::default()
    });
    assert!(cat.skills.is_empty() && cat.skipped.is_empty());
    // Неразбираемый frontmatter — пропуск с причиной разбора.
    write(
        &user.join("bad/SKILL.md"),
        "---\nname: bad\ndescription: *x\n---\n",
    );
    let cat = discover(&DiscoverInput {
        user_dir: Some(user),
        ..DiscoverInput::default()
    });
    assert!(cat.get("bad").is_none());
    assert!(
        reasons(&cat).iter().any(|(path, why)| path.ends_with("bad")
            && why.starts_with("unsupported YAML feature: alias")),
        "{:?}",
        reasons(&cat)
    );
}

#[test]
fn load_detects_change() {
    let user = dir("load");
    skill(&user, "x", "x", "loaded");
    write(&user.join("x/ref/a.md"), "resource");
    let cat = discover(&DiscoverInput {
        user_dir: Some(user.clone()),
        ..DiscoverInput::default()
    });
    let meta = cat.get("x").unwrap();
    assert_eq!(load(meta, None).unwrap(), "Body of x.\n");
    assert_eq!(load(meta, Some("ref/a.md")).unwrap(), "resource");
    // Ресурс изменён — `Changed`.
    write(&user.join("x/ref/a.md"), "other");
    assert_eq!(load(meta, Some("ref/a.md")), Err(LoadError::Changed));
    // `SKILL.md` изменён после обнаружения — `Changed`.
    skill(&user, "x", "x", "edited");
    assert_eq!(load(meta, None), Err(LoadError::Changed));
    // Удалён — тоже `Changed`.
    std::fs::remove_file(user.join("x/SKILL.md")).unwrap();
    assert_eq!(load(meta, None), Err(LoadError::Changed));
}

/// Ревью задачи 11, m2: каталог-ссылка (junction) внутри каталога скилла не обходится;
/// у источника `base` (корень `user` или дерево проекта) — каталог-ссылка `x/linked` на
/// соседний каталог `shared`.
fn junction_fixture(base: &Path) {
    skill(base, "x", "x", "skill with links");
    write(&base.join("x/ref/a.md"), "resource");
    write(&base.join("shared/ref.md"), "shared resource");
    dir_link(&base.join("x/linked"), &base.join("shared"));
}

/// Ссылки на файлы у источника `base`: `x/alias.md` → `x/ref/a.md` (внутри каталога
/// скилла), `x/shared.md` → `shared/ref.md` (в корне источника, но вне каталога скилла),
/// `SKILL.md` скилла `y` → `store/y.md` (наружу), `SKILL.md` скилла `z` →
/// `z/docs/skill.md` (внутрь). `None` — ОС не дала создать ссылку на файл.
fn file_links_fixture(base: &Path) -> Option<()> {
    file_link(&base.join("x/alias.md"), &base.join("x/ref/a.md"))?;
    file_link(&base.join("x/shared.md"), &base.join("shared/ref.md"))?;
    write(&base.join("store/y.md"), &skill_text("y", "outside target"));
    std::fs::create_dir_all(base.join("y")).unwrap();
    file_link(&base.join("y/SKILL.md"), &base.join("store/y.md"))?;
    write(
        &base.join("z/docs/skill.md"),
        &skill_text("z", "inside target"),
    );
    file_link(&base.join("z/SKILL.md"), &base.join("z/docs/skill.md"))?;
    Some(())
}

/// Проверки каталога и загрузки по фикстурам выше; `links` — есть ли ссылки на файлы.
fn check_links(cat: &Catalog, shown: impl Fn(&str) -> String, links: bool) {
    let x = cat.get("x").unwrap_or_else(|| panic!("{cat:?}"));
    // Каталог-ссылка внутри скилла — запись `external-link`: не ресурс, предупреждение,
    // загрузка по пути внутри неё — `UnknownFile`.
    assert!(
        !x.files.keys().any(|k| k.starts_with("linked")),
        "{:?}",
        x.files
    );
    assert!(
        x.warnings
            .iter()
            .any(|w| w == "file linked is not listed: external link"),
        "{:?}",
        x.warnings
    );
    assert_eq!(
        load(x, Some("linked/ref.md")),
        Err(LoadError::UnknownFile("linked/ref.md".into()))
    );
    assert_eq!(load(x, Some("ref/a.md")).unwrap(), "resource");
    if !links {
        return;
    }
    // Ссылка на файл внутри каталога скилла — ресурс `link:<hex>`, загружается.
    assert_eq!(x.files["alias.md"], format!("link:{}", hex(b"resource")));
    assert_eq!(load(x, Some("alias.md")).unwrap(), "resource");
    // Ссылка за пределы каталога скилла (в том же корне источника) — не ресурс (m1):
    // модель не увидит файл, который не загрузится.
    assert!(!x.files.contains_key("shared.md"), "{:?}", x.files);
    assert!(
        x.warnings
            .iter()
            .any(|w| w == "file shared.md is not listed: link outside the skill directory"),
        "{:?}",
        x.warnings
    );
    assert_eq!(
        load(x, Some("shared.md")),
        Err(LoadError::UnknownFile("shared.md".into()))
    );
    // `SKILL.md`-ссылка наружу — пропуск; внутрь — скилл с хэшем цели.
    assert!(cat.get("y").is_none());
    assert!(
        reasons(cat)
            .iter()
            .any(|(path, why)| *path == shown("y")
                && why == "unreadable: outside the skill directory"),
        "{:?}",
        reasons(cat)
    );
    let z = cat.get("z").unwrap();
    assert_eq!(z.hash, hex(skill_text("z", "inside target").as_bytes()));
    assert_eq!(load(z, None).unwrap(), "Body of z.\n");
    // Цель внутри каталога скилла — его ресурс, как обычный файл.
    assert!(z.files.contains_key("docs/skill.md"), "{:?}", z.files);
}

#[test]
fn links_inside_skill_dir() {
    // Источник `user`.
    let user = dir("links-user");
    junction_fixture(&user);
    let links = file_links_fixture(&user).is_some();
    let cat = discover(&DiscoverInput {
        user_dir: Some(user.clone()),
        ..DiscoverInput::default()
    });
    check_links(&cat, |d| user.join(d).display().to_string(), links);

    // Проект: те же ссылки внутри доверенного дерева (общие файлы монорепозитория —
    // `ref.md -> ../shared/ref.md`); значения дал обход доверия.
    let project = dir("links-project");
    let tree = project.join(".claude/skills");
    junction_fixture(&tree);
    let links = file_links_fixture(&tree).is_some();
    // Junction на каталог внутри дерева — метка в списке доверия, а не ошибка обхода
    // (на Windows открытие каталога — `PermissionDenied`).
    let (_, listing) = tree_listing(&roots(&project), ".claude/skills", &TRUST_LIMITS).unwrap();
    assert_eq!(listing.unwrap().files["x/linked"], EXTERNAL_LINK);
    let cat = discover(&DiscoverInput {
        project: Some(project_skills(&project)),
        ..DiscoverInput::default()
    });
    check_links(&cat, |d| format!(".claude/skills/{d}"), links);
}

/// I1 финального ревью скиллов: каталог скилла, подключённый ссылкой (junction на Windows,
/// символическая ссылка на Unix), не обходится (D116), но и не пропадает молча — он среди
/// пропущенных с причиной `linked skill directory` у `user`, `extra` и в дереве проекта,
/// на любой глубине в пределах обнаружения. Каталог-ссылка внутри каталога скилла —
/// по-прежнему предупреждение этого скилла, а не пропуск.
#[test]
fn linked_skill_dir_is_skipped_with_reason() {
    use za_skills::LINKED_SKILL_DIR;
    let base = dir("linked-dir");
    let target = base.join("outside").join("ext");
    skill(&base, "outside/ext", "ext", "skill outside the source");
    let has = |cat: &Catalog, path: String| {
        let r = reasons(cat);
        assert!(r.contains(&(path, LINKED_SKILL_DIR.to_owned())), "{r:?}");
    };
    let (user, extra) = (base.join("user"), base.join("extra"));
    skill(&user, "real", "real", "a regular skill");
    dir_link(&user.join("linked"), &target);
    std::fs::create_dir_all(user.join("group")).unwrap();
    dir_link(&user.join("group").join("deep"), &target);
    std::fs::create_dir_all(&extra).unwrap();
    dir_link(&extra.join("linked"), &target);
    // Каталог-ссылка внутри скилла — предупреждение скилла `x`, не пропуск.
    junction_fixture(&user);
    let cat = discover(&DiscoverInput {
        user_dir: Some(user.clone()),
        extra_dirs: vec![extra.clone()],
        ..DiscoverInput::default()
    });
    assert!(cat.get("real").is_some() && cat.get("x").is_some());
    assert!(cat.get("ext").is_none(), "{cat:?}");
    has(&cat, user.join("linked").display().to_string());
    has(&cat, user.join("group").join("deep").display().to_string());
    has(&cat, extra.join("linked").display().to_string());
    assert_eq!(
        cat.skipped
            .iter()
            .filter(|s| s.reason == LINKED_SKILL_DIR)
            .count(),
        3,
        "{:?}",
        reasons(&cat)
    );

    // Проект: каталог-ссылка в доверенном дереве (ссылка наружу — значение
    // `external-link` обхода доверия).
    let project = base.join("project");
    skill(&project, ".claude/skills/real", "real", "a regular skill");
    dir_link(&project.join(".claude/skills/linked"), &target);
    let cat = discover(&DiscoverInput {
        project: Some(project_skills(&project)),
        ..DiscoverInput::default()
    });
    assert!(cat.get("real").is_some() && cat.get("ext").is_none());
    has(&cat, ".claude/skills/linked".to_owned());
}

/// Ревью задачи 12, m8: доверенный файл корня проекта (`AGENTS.md`, `CLAUDE.md`,
/// `.zero-agent/config.toml` R1-C1) в виде ссылки (junction) на каталог внутри проекта —
/// значение `external-link`, а не ошибка. Это та же ветка `entry_value`, что у обхода
/// дерева: на Windows открытие каталога давало `PermissionDenied`, и `trust_set` падал.
#[test]
fn trusted_file_link_to_dir_is_external() {
    let project = dir("file-value-link");
    write(&project.join("docs/notes.md"), "notes");
    write(&project.join("CLAUDE.md"), "rules");
    dir_link(&project.join("AGENTS.md"), &project.join("docs"));
    let roots = roots(&project);
    assert_eq!(
        file_value(&roots, "AGENTS.md").unwrap().as_deref(),
        Some(EXTERNAL_LINK)
    );
    assert_eq!(
        file_value(&roots, "CLAUDE.md").unwrap(),
        Some(hex(b"rules"))
    );
    assert_eq!(file_value(&roots, "missing.md").unwrap(), None);
}

#[test]
fn load_file_rejects_dotdot() {
    let user = dir("load-dotdot");
    skill(&user, "x", "x", "loaded");
    write(&user.join("x/ref/a.md"), "resource");
    write(&user.join("secret"), "outside the skill");
    let cat = discover(&DiscoverInput {
        user_dir: Some(user),
        ..DiscoverInput::default()
    });
    let meta = cat.get("x").unwrap();
    for bad in [
        "../x",
        "../secret",
        "/etc/x",
        "ref/../ref/a.md",
        "./ref/a.md",
        "ref\\a.md",
        "C:/x",
        "",
        "SKILL.md",
        "ref/absent.md",
    ] {
        assert_eq!(
            load(meta, Some(bad)),
            Err(LoadError::UnknownFile(bad.to_owned())),
            "{bad:?}"
        );
    }
}
