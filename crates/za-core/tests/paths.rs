//! Пути (R1-C1 D76, D79, D98): лексический разбор без обращения к ФС, разрешение
//! канонических путей, граница проекта, защищённые пути, рабочие каталоги и безопасное
//! открытие. На каждый вектор обхода — тест, который показывает отказ: `..`, ссылки и
//! junction, `\\?\`, UNC, device-пути, 8.3, регистр, ADS, хвостовая точка, гонка подмены.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use za_core::Access;
use za_core::paths::{
    LexPath, PathError, Platform, ResolvedPath, Roots, classify, classify_with, display,
    open_checked, resolve, within, work_dir,
};

/// Чистый каталог теста `CARGO_TARGET_TMPDIR/za-core-paths/<name>-<pid>-<n>`.
fn unique_dir(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("za-core-paths")
        .join(format!("{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Проект, `ZA_HOME` и чужой каталог рядом (вне проекта).
struct Fixture {
    project: PathBuf,
    home: PathBuf,
    outside: PathBuf,
    roots: Roots,
}

fn fixture(name: &str) -> Fixture {
    let base = unique_dir(name);
    let (project, home, outside) = (
        base.join("project"),
        base.join("home"),
        base.join("outside"),
    );
    for d in [&project, &home, &outside] {
        std::fs::create_dir_all(d).unwrap();
    }
    let roots = Roots::canonical(&project, &home).unwrap();
    Fixture {
        project,
        home,
        outside,
        roots,
    }
}

fn res(f: &Fixture, arg: &str, access: Access) -> ResolvedPath {
    let lex = classify(&f.project, arg).unwrap();
    resolve(&f.roots, &lex, access).unwrap()
}

/// Ссылка на каталог: junction на Windows (`mklink /J` прав не требует), символическая
/// ссылка на Unix.
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

/// Ссылка на файл. На Windows требует прав администратора или режима разработчика:
/// без них — `None`, и тест явно пропускает проверку с сообщением.
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
            eprintln!(
                "ПРОПУСК: ссылка на файл не создана ({e}); нужен режим разработчика или права администратора"
            );
            None
        }
    }
}

// ---- Лексический этап: обе ОС через classify_with ----

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("lexical-root")
}

#[test]
fn windows_lexical_rejections() {
    let root = root();
    let w = |a: &str| classify_with(&root, a, Platform::Windows);
    assert!(matches!(w(r"\\192.0.2.1\x"), Err(PathError::Network(_))));
    assert!(matches!(w("//server/share/a"), Err(PathError::Network(_))));
    assert!(matches!(w(r"\/server\share"), Err(PathError::Network(_))));
    assert!(matches!(w(r"\\?\UNC\s\x"), Err(PathError::Network(_))));
    assert!(matches!(w(r"//?/unc/s/x"), Err(PathError::Network(_))));
    assert!(matches!(
        w(r"\\.\PhysicalDrive0"),
        Err(PathError::Device(_))
    ));
    assert!(matches!(w(r"\\.\C:\x"), Err(PathError::Device(_))));
    assert!(matches!(
        w(r"\\?\GLOBALROOT\Device\X"),
        Err(PathError::Device(_))
    ));
    assert!(matches!(
        w(r"\\?\Volume{0-1-2}\x"),
        Err(PathError::Device(_))
    ));
    assert!(matches!(w(r"\??\C:\x"), Err(PathError::Device(_))));
    assert!(matches!(w("sub/con.txt"), Err(PathError::Device(_))));
    for name in [
        "NUL", "prn", "Aux.log", "com1", "LPT9.x", "COM¹", "conin$", "con .txt",
    ] {
        assert!(matches!(w(name), Err(PathError::Device(_))), "{name}");
    }
    assert!(matches!(w("a.txt:stream"), Err(PathError::Stream(_))));
    assert!(matches!(
        w(r".zero-agent::$INDEX_ALLOCATION\config.toml"),
        Err(PathError::Stream(_))
    ));
    assert!(matches!(w(r"C:\a\b.txt::$DATA"), Err(PathError::Stream(_))));
    assert!(matches!(w(".git./HEAD"), Err(PathError::TrailingDot(_))));
    assert!(matches!(w("a /b"), Err(PathError::TrailingDot(_))));
    assert!(matches!(w("C:foo"), Err(PathError::DriveRelative(_))));
    assert!(matches!(w("C:"), Err(PathError::DriveRelative(_))));
    assert!(matches!(w(r"\Windows\x"), Err(PathError::DriveRelative(_))));
    assert!(matches!(w("a*b"), Err(PathError::Invalid(_))));
    assert!(matches!(w("a\0b"), Err(PathError::Invalid(_))));
    assert!(matches!(w(""), Err(PathError::Invalid(_))));
    assert!(w(r"C:\a\b").is_ok());
    assert!(w(r"\\?\C:\a\b").is_ok());
    assert!(w("sub/console.txt").is_ok());
    assert!(w("src/lib.rs").is_ok());
}

/// Отказ — строкой ошибки, без ФС: UNC на несуществующий узел возвращается сразу, а
/// [`LexPath`] без [`classify`] не построить, поэтому `resolve` (и `canonicalize`) до
/// UNC не доходит (Ruling 7.4: нет SMB-запроса и утечки NTLM).
#[test]
fn unc_path_rejected_without_io() {
    let root = root();
    for arg in [
        r"\\192.0.2.1\share\x",
        "//192.0.2.1/share",
        r"\\?\UNC\192.0.2.1\share",
        r"\\.\pipe\x",
    ] {
        let e = classify_with(&root, arg, Platform::Windows).unwrap_err();
        assert!(
            matches!(e, PathError::Network(_) | PathError::Device(_)),
            "{arg}: {e}"
        );
        #[cfg(windows)]
        assert!(classify(&root, arg).is_err(), "{arg}");
    }
}

#[test]
fn unix_colon_is_a_name() {
    let root = PathBuf::from("/p");
    assert!(classify_with(&root, "a:b", Platform::Unix).is_ok());
    assert!(classify_with(&root, "con.txt", Platform::Unix).is_ok());
    assert!(classify_with(&root, r"a\b", Platform::Unix).is_ok());
    assert!(matches!(
        classify_with(&root, "a\0b", Platform::Unix),
        Err(PathError::Invalid(_))
    ));
}

#[test]
fn lexical_normalization() {
    let root = root();
    let lex = classify(&root, "a/./b/../c").unwrap();
    assert_eq!(lex.as_path(), root.join("a").join("c"));
    // `~` не раскрывается: это имя каталога внутри проекта.
    assert_eq!(
        classify(&root, "~/x").unwrap().as_path(),
        root.join("~").join("x")
    );
    // `..` за корень ФС не уходит.
    let deep = classify(&root, &"../".repeat(64)).unwrap();
    assert!(
        deep.as_path().parent().is_none(),
        "{}",
        deep.as_path().display()
    );
}

// ---- Файловый этап ----

#[test]
fn dotdot_beyond_root_is_outside() {
    let f = fixture("dotdot");
    std::fs::write(f.outside.join("x"), "x").unwrap();
    let r = res(&f, "../outside/x", Access::Read);
    assert!(!r.inside, "{}", r.path.display());
    let r = res(&f, "sub/../../project/a", Access::Read);
    assert!(r.inside, "{}", r.path.display());
    let r = res(&f, "../project2/a", Access::Read);
    assert!(!r.inside, "{}", r.path.display());
}

#[test]
fn absolute_outside_is_outside() {
    let f = fixture("absolute");
    let arg = f.outside.join("file.txt");
    let r = res(&f, &arg.to_string_lossy(), Access::Write);
    assert!(!r.inside && !r.protected);
}

#[test]
fn link_inside_pointing_out_is_outside() {
    let f = fixture("link_out");
    std::fs::write(f.outside.join("secret.txt"), "s").unwrap();
    dir_link(&f.project.join("l"), &f.outside);
    // Существующий файл через ссылку и новый файл под ней — оба снаружи.
    for arg in ["l/secret.txt", "l/new.txt", "l/deep/new.txt", "l"] {
        let r = res(&f, arg, Access::Read);
        assert!(!r.inside, "{arg}: {}", r.path.display());
    }
}

/// Висячая ссылка: цели нет, и `canonicalize` её не раскрывает. Запись через неё ушла
/// бы туда, куда она указывает, поэтому разрешение — отказ.
#[test]
fn dangling_link_is_refused() {
    let f = fixture("dangling");
    dir_link(&f.project.join("l"), &f.outside.join("missing"));
    let lex = classify(&f.project, "l/new.txt").unwrap();
    let e = resolve(&f.roots, &lex, Access::Write).unwrap_err();
    assert!(e.to_string().contains("dangling link"), "{e}");
    let lex = classify(&f.project, "l").unwrap();
    assert!(resolve(&f.roots, &lex, Access::Write).is_err());
}

#[test]
fn protected_paths() {
    let f = fixture("protected");
    std::fs::create_dir_all(f.project.join(".zero-agent/work")).unwrap();
    std::fs::create_dir_all(f.project.join(".git")).unwrap();
    for arg in [
        ".zero-agent/config.toml",
        ".zero-agent",
        ".git/HEAD",
        ".git/hooks/pre-commit",
        ".zero-agent/work/../config.toml",
    ] {
        assert!(res(&f, arg, Access::Write).protected, "{arg}");
        // Защита — только от нативной записи (D79): чтение разрешено.
        assert!(!res(&f, arg, Access::Read).protected, "{arg}");
    }
    for arg in [
        ".zero-agent/work/tmp/x",
        ".gitignore",
        ".github/x",
        ".zero-agent-x/y",
    ] {
        assert!(!res(&f, arg, Access::Write).protected, "{arg}");
    }
    let home = f.home.join("config.toml");
    assert!(res(&f, &home.to_string_lossy(), Access::Write).protected);
}

#[test]
fn link_to_protected_is_protected() {
    let f = fixture("link_protected");
    std::fs::create_dir_all(f.project.join(".zero-agent")).unwrap();
    dir_link(&f.project.join("l"), &f.project.join(".zero-agent"));
    let r = res(&f, "l/config.toml", Access::Write);
    assert!(r.protected && r.inside, "{}", r.path.display());
    // То же для ZA_HOME.
    dir_link(&f.project.join("h"), &f.home);
    let r = res(&f, "h/config.toml", Access::Write);
    assert!(r.protected && !r.inside, "{}", r.path.display());
    // И для `.git` через ссылку на сам корень проекта.
    std::fs::create_dir_all(f.project.join(".git")).unwrap();
    dir_link(&f.project.join("self"), &f.project);
    assert!(res(&f, "self/.git/config", Access::Write).protected);
}

#[cfg(windows)]
#[test]
fn case_and_verbatim_are_inside() {
    // Корень — НЕ канонический путь из CARGO_TARGET_TMPDIR: Roots::canonical
    // канонизирует сам.
    let raw_root = unique_dir("case");
    let roots = Roots::canonical(&raw_root, &raw_root.join("no-home")).unwrap();
    let upper = raw_root.to_string_lossy().to_uppercase();
    let r = resolve(&roots, &classify(&raw_root, &upper).unwrap(), Access::Read).unwrap();
    assert!(r.inside, "{}", r.path.display());
    let verbatim = format!(r"\\?\{}", raw_root.display());
    let r = resolve(
        &roots,
        &classify(&raw_root, &verbatim).unwrap(),
        Access::Read,
    )
    .unwrap();
    assert!(r.inside, "{}", r.path.display());
    assert!(
        !r.path.to_string_lossy().starts_with(r"\\?\"),
        "{}",
        r.path.display()
    );
    let lower = format!(r"{}\SUB\file.TXT", raw_root.display()).to_lowercase();
    let r = resolve(&roots, &classify(&raw_root, &lower).unwrap(), Access::Read).unwrap();
    assert!(r.inside, "{}", r.path.display());
}

/// Регистр NTFS: `.ZERO-AGENT` и `.Git` — те же защищённые каталоги, и когда они есть,
/// и когда их ещё нет (хвост сравнивается без учёта регистра).
#[cfg(windows)]
#[test]
fn case_does_not_bypass_protection() {
    let f = fixture("case_protected");
    assert!(res(&f, ".ZERO-AGENT/config.toml", Access::Write).protected);
    assert!(res(&f, ".Git/HEAD", Access::Write).protected);
    std::fs::create_dir_all(f.project.join(".zero-agent")).unwrap();
    std::fs::create_dir_all(f.project.join(".git")).unwrap();
    assert!(res(&f, ".ZERO-AGENT/config.toml", Access::Write).protected);
    assert!(res(&f, ".GIT/config", Access::Write).protected);
    assert!(!res(&f, ".ZERO-AGENT/WORK/x", Access::Write).protected);
    let home = f.home.to_string_lossy().to_uppercase();
    assert!(res(&f, &format!(r"{home}\config.toml"), Access::Write).protected);
}

/// Короткое имя 8.3 каталога (`dir /x`), если том их создаёт; иначе `None`.
#[cfg(windows)]
fn short_name(dir: &Path, long: &str) -> Option<String> {
    let out = std::process::Command::new("cmd")
        .arg("/C")
        .arg("dir")
        .arg("/x")
        .arg("/a")
        .arg(dir.components().collect::<PathBuf>())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.lines().find_map(|l| {
        let mut words = l.split_whitespace().rev();
        let name = words.next()?;
        let short = words.next()?;
        (name == long && short.contains('~')).then(|| short.to_owned())
    })
}

/// Короткие имена 8.3: `ZERO-A~1\config.toml` — тот же защищённый `.zero-agent`, а
/// короткое имя ссылки наружу — снаружи. Если том не создаёт имена 8.3 (`fsutil
/// 8dot3name`), тест явно пропускает проверку с сообщением.
#[cfg(windows)]
#[test]
fn short_names_are_resolved() {
    let f = fixture("short_names");
    std::fs::create_dir_all(f.project.join(".zero-agent")).unwrap();
    std::fs::create_dir_all(f.project.join("averyverylongdirectoryname")).unwrap();
    dir_link(&f.project.join("averyverylonglinkname"), &f.outside);
    let Some(za) = short_name(&f.project, ".zero-agent") else {
        eprintln!("ПРОПУСК: том не создаёт короткие имена 8.3 (fsutil 8dot3name)");
        return;
    };
    let r = res(&f, &format!(r"{za}\config.toml"), Access::Write);
    assert!(r.protected, "{za}: {}", r.path.display());
    let long = short_name(&f.project, "averyverylongdirectoryname").unwrap();
    let r = res(&f, &format!(r"{long}\x"), Access::Read);
    assert!(r.inside);
    assert!(
        r.path.ends_with(r"averyverylongdirectoryname\x"),
        "{}",
        r.path.display()
    );
    if let Some(link) = short_name(&f.project, "averyverylonglinkname") {
        assert!(!res(&f, &format!(r"{link}\x"), Access::Read).inside);
    }
}

#[test]
fn within_by_components() {
    assert!(!within(Path::new("/data"), Path::new("/data2/x")));
    assert!(within(Path::new("/data"), Path::new("/data/x")));
    assert!(within(Path::new("/data"), Path::new("/data")));
    assert!(!within(Path::new("/data/x"), Path::new("/data")));
    #[cfg(windows)]
    {
        assert!(within(Path::new(r"C:\Data"), Path::new(r"c:\data\X")));
        assert!(within(Path::new(r"\\?\C:\Data"), Path::new(r"C:\data\x")));
        assert!(!within(Path::new(r"C:\data"), Path::new(r"D:\data\x")));
    }
}

#[test]
fn display_is_relative_inside() {
    let root = root();
    assert_eq!(display(&root, &root.join("a").join("b.rs")), "a/b.rs");
    assert_eq!(display(&root, &root), ".");
    let out = root.parent().unwrap().join("x");
    assert_eq!(display(&root, &out), out.display().to_string());
}

#[test]
fn work_dir_refuses_link_and_writes_gitignore() {
    let f = fixture("work_link");
    std::fs::create_dir_all(f.project.join(".zero-agent")).unwrap();
    dir_link(&f.project.join(".zero-agent/work"), &f.outside);
    let e = work_dir(&f.project, "spill").unwrap_err();
    assert!(e.to_string().contains("ссылка"), "{e}");
    assert!(std::fs::read_dir(&f.outside).unwrap().next().is_none());

    let f = fixture("work_ok");
    let d = work_dir(&f.project, "checkpoints/run-1").unwrap();
    assert!(d.is_dir());
    assert!(d.ends_with(Path::new(".zero-agent/work/checkpoints/run-1")));
    let ignore = f.project.join(".zero-agent/work/.gitignore");
    assert_eq!(std::fs::read_to_string(&ignore).unwrap(), "*\n");
    // Повтор идемпотентен.
    assert_eq!(work_dir(&f.project, "checkpoints/run-1").unwrap(), d);
    for bad in ["", "..", "a/../b", "/abs", "a:b", r"a\b"] {
        assert!(work_dir(&f.project, bad).is_err(), "{bad}");
    }
}

#[test]
fn open_checked_reads_and_writes_inside() {
    use std::io::{Read, Write};
    let f = fixture("open_ok");
    std::fs::write(f.project.join("a.txt"), "hello").unwrap();
    let r = res(&f, "a.txt", Access::Read);
    let mut s = String::new();
    open_checked(&r, false)
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    assert_eq!(s, "hello");
    let r = res(&f, "new.txt", Access::Write);
    open_checked(&r, true).unwrap().write_all(b"x").unwrap();
    assert_eq!(
        std::fs::read_to_string(f.project.join("new.txt")).unwrap(),
        "x"
    );
}

#[test]
fn open_checked_refuses_final_link() {
    let f = fixture("open_link");
    let victim = f.outside.join("victim.txt");
    std::fs::write(&victim, "чужой файл").unwrap();
    // Разрешение до подмены: обычный файл внутри; затем на его месте — ссылка наружу.
    std::fs::write(f.project.join("a.txt"), "own").unwrap();
    let r = res(&f, "a.txt", Access::Write);
    std::fs::remove_file(f.project.join("a.txt")).unwrap();
    if file_link(&f.project.join("a.txt"), &victim).is_some() {
        let e = open_checked(&r, true).unwrap_err();
        assert!(e.to_string().contains("refusing to follow a link"), "{e}");
        let e = open_checked(&r, false).unwrap_err();
        assert!(e.to_string().contains("refusing to follow a link"), "{e}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "чужой файл");
    }
    // Junction (каталог) на последнем компоненте — тот же отказ, прав не нужно.
    let r = res(&f, "d", Access::Read);
    dir_link(&f.project.join("d"), &f.outside);
    let e = open_checked(&r, false).unwrap_err();
    assert!(e.to_string().contains("refusing to follow a link"), "{e}");
}

/// Гонка подмены (TOCTOU): после разрешения промежуточный каталог заменён ссылкой
/// наружу, где лежит файл с тем же именем. Открытие сверяет канонический путь и
/// дескриптор — отказ, чужой файл не тронут.
#[test]
fn open_checked_refuses_swapped_parent() {
    let f = fixture("open_swap");
    std::fs::create_dir_all(f.project.join("sub")).unwrap();
    std::fs::write(f.project.join("sub/a.txt"), "own").unwrap();
    std::fs::write(f.outside.join("a.txt"), "чужой файл").unwrap();
    let r = res(&f, "sub/a.txt", Access::Write);
    assert!(r.inside);
    std::fs::rename(f.project.join("sub"), f.project.join("sub-old")).unwrap();
    dir_link(&f.project.join("sub"), &f.outside);
    for write in [false, true] {
        let e = open_checked(&r, write).unwrap_err();
        assert!(e.to_string().contains("path changed while opening"), "{e}");
    }
    assert_eq!(
        std::fs::read_to_string(f.outside.join("a.txt")).unwrap(),
        "чужой файл"
    );
}

/// Отказ classify — до ФС, поэтому и для пути, которого заведомо нет.
#[test]
fn lex_path_only_from_classify() {
    let root = root();
    let lex: LexPath = classify(&root, "x").unwrap();
    assert_eq!(lex.as_path(), root.join("x"));
}

/// Роли: корни канонизируются, `ZA_HOME` может ещё не существовать.
#[test]
fn roots_are_canonical() {
    let f = fixture("roots");
    let dotted = f.project.join("..").join("project");
    let roots = Roots::canonical(&dotted, &f.home.join("missing")).unwrap();
    assert!(within(
        &roots.project,
        &std::fs::canonicalize(&f.project).unwrap()
    ));
    assert!(roots.za_home.ends_with("missing"));
    assert!(Roots::canonical(&f.project.join("absent"), &f.home).is_err());
}
