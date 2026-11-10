//! Набор доверенных файлов и ссылки (ревью задачи 4 R1-C1, I1, I2; D83).
//!
//! Ссылка на файл внутри проекта хэшируется по содержимому цели; ссылка наружу, на
//! сетевой путь и каталог-ссылка (junction) — метка `external-link`, обход в них не
//! заходит и к цели не обращается. Помощники ссылок и пропуска — как в
//! `za-core/tests/paths.rs`: на Windows ссылка на файл требует прав, без них проверка
//! пропускается с сообщением, а `ZA_REQUIRE_LINK_TESTS=1` превращает пропуск в падение.

use std::path::{Path, PathBuf};

use zero_agent::trust::{EXTERNAL_LINK, LINK_PREFIX, trust_set};

mod support;
use support::process::unique_dir;

/// Каталог проекта и внешний каталог теста.
fn fixture(name: &str) -> (PathBuf, PathBuf) {
    let root = unique_dir("zero-agent-trust", name);
    let project = root.join("project");
    let outside = root.join("outside");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    (project, outside)
}

fn hex(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
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
            skip(&format!(
                "ссылка на файл не создана ({e}); нужен режим разработчика или права администратора"
            ));
            None
        }
    }
}

/// Символическая ссылка на каталог (на Windows нужен режим разработчика или права).
#[cfg(windows)]
fn dir_symlink(link: &Path, target: &Path) -> Option<()> {
    match std::os::windows::fs::symlink_dir(target, link) {
        Ok(()) => Some(()),
        Err(e) => {
            skip(&format!(
                "символическая ссылка не создана ({e}); нужен режим разработчика или права администратора"
            ));
            None
        }
    }
}

/// Пропуск проверки с сообщением; `ZA_REQUIRE_LINK_TESTS=1` (CI) — пропуск — падение.
/// Пропуски бывают только на Windows (на Unix ссылки создаются без прав).
#[cfg(windows)]
fn skip(why: &str) {
    if std::env::var("ZA_REQUIRE_LINK_TESTS").as_deref() == Ok("1") {
        panic!("ZA_REQUIRE_LINK_TESTS=1, а проверка пропущена: {why}");
    }
    eprintln!("ПРОПУСК: {why}");
}

/// I1: `AGENTS.md` — ссылка на файл внутри проекта. В наборе — `link:` и хэш содержимого
/// цели; подмена содержимого цели меняет набор (нужно новое доверие).
#[test]
fn link_inside_is_hashed_by_target_content() {
    let (p, _) = fixture("link-inside");
    std::fs::create_dir_all(p.join("docs")).unwrap();
    std::fs::write(p.join("docs/rules.md"), "версия 1").unwrap();
    if file_link(&p.join("AGENTS.md"), &p.join("docs/rules.md")).is_none() {
        return;
    }
    let first = trust_set(&p).unwrap();
    assert_eq!(
        first.files["AGENTS.md"],
        format!("{LINK_PREFIX}{}", hex("версия 1".as_bytes()))
    );
    std::fs::write(p.join("docs/rules.md"), "версия 2").unwrap();
    let second = trust_set(&p).unwrap();
    assert_ne!(first, second, "подмена цели не изменила набор");
    assert_eq!(
        second.files["AGENTS.md"],
        format!("{LINK_PREFIX}{}", hex("версия 2".as_bytes()))
    );
    // Тот же текст обычным файлом — другое значение (домен хэша).
    std::fs::remove_file(p.join("AGENTS.md")).unwrap();
    std::fs::write(p.join("AGENTS.md"), "версия 2").unwrap();
    assert_ne!(trust_set(&p).unwrap(), second);
}

/// I1: ссылка на файл вне проекта и висячая ссылка — метка, содержимое цели не входит.
#[test]
fn link_outside_or_dangling_is_marked() {
    let (p, out) = fixture("link-outside");
    std::fs::write(out.join("evil.md"), "чужое").unwrap();
    if file_link(&p.join("CLAUDE.md"), &out.join("evil.md")).is_none() {
        return;
    }
    if file_link(&p.join("AGENTS.md"), &p.join("нет-такого.md")).is_none() {
        return;
    }
    let first = trust_set(&p).unwrap();
    assert_eq!(first.files["CLAUDE.md"], EXTERNAL_LINK);
    assert_eq!(first.files["AGENTS.md"], EXTERNAL_LINK);
    std::fs::write(out.join("evil.md"), "другое чужое").unwrap();
    assert_eq!(trust_set(&p).unwrap(), first);
}

/// I2: `.claude` — junction (ссылка) на внешний каталог со скиллами: обход в него не
/// заходит, дерево — метка; изменение внешних файлов набор не меняет. То же для
/// `.zero-agent` с `config.toml` и для каталога-ссылки внутри дерева.
#[test]
fn directory_links_are_not_walked() {
    let (p, out) = fixture("dir-links");
    std::fs::create_dir_all(out.join("skills/x")).unwrap();
    std::fs::write(out.join("skills/x/SKILL.md"), "s").unwrap();
    std::fs::write(out.join("config.toml"), "[policy]").unwrap();
    dir_link(&p.join(".claude"), &out);
    dir_link(&p.join(".zero-agent"), &out);
    std::fs::create_dir_all(p.join(".agents/skills/own")).unwrap();
    std::fs::write(p.join(".agents/skills/own/SKILL.md"), "своё").unwrap();
    dir_link(&p.join(".agents/skills/foreign"), &out.join("skills"));
    let first = trust_set(&p).unwrap();
    assert_eq!(first.files[".claude/skills"], EXTERNAL_LINK);
    assert_eq!(first.files[".zero-agent/config.toml"], EXTERNAL_LINK);
    assert_eq!(first.files[".zero-agent/skills"], EXTERNAL_LINK);
    let own_tree = first.files[".agents/skills"].clone();
    std::fs::write(out.join("skills/x/SKILL.md"), "s2").unwrap();
    std::fs::write(out.join("config.toml"), "[policy] changed").unwrap();
    let second = trust_set(&p).unwrap();
    assert_eq!(second, first, "внешние файлы попали в набор");
    // Своё содержимое дерева по-прежнему учитывается.
    std::fs::write(p.join(".agents/skills/own/SKILL.md"), "своё 2").unwrap();
    assert_ne!(trust_set(&p).unwrap().files[".agents/skills"], own_tree);
}

/// I2, Ruling 7.4: `.claude` и `AGENTS.md` — ссылки на `\\host\share`. Набор строится без
/// обращения к цели: число вызовов `canonicalize` то же, что у проекта без ссылок (только
/// корень проекта), значения — метка.
#[cfg(windows)]
#[test]
fn unc_links_are_not_touched() {
    use za_core::paths::canonicalize_calls;
    let (p, _) = fixture("unc-links");
    let before = canonicalize_calls();
    assert!(trust_set(&p).unwrap().files.is_empty());
    let baseline = canonicalize_calls() - before;
    if dir_symlink(&p.join(".claude"), Path::new(r"\\192.0.2.1\share")).is_none() {
        return;
    }
    if file_link(
        &p.join("AGENTS.md"),
        Path::new(r"\\192.0.2.1\share\AGENTS.md"),
    )
    .is_none()
    {
        return;
    }
    let before = canonicalize_calls();
    let s = trust_set(&p).unwrap();
    assert_eq!(
        canonicalize_calls() - before,
        baseline,
        "цель ссылки канонизирована"
    );
    assert_eq!(s.files[".claude/skills"], EXTERNAL_LINK);
    assert_eq!(s.files["AGENTS.md"], EXTERNAL_LINK);
}
