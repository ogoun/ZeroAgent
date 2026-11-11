//! Статическая проверка репозитория до любого вызова git (ревью задачи 9, C1).
//!
//! Всё читается через `za_core::paths` без следования ссылкам; сетевой и device-путь
//! отвергается лексически (`classify`), до обращения к нему. Репозиторий годится, если:
//! - `.git` в корне проекта — каталог или файл `gitdir: <путь>` с целью-каталогом внутри
//!   проекта; ни `.git`, ни служебные записи в нём (`objects`, `refs`, `config`, …) не
//!   ссылки; нет `commondir` (связанный worktree делит ссылки с другим каталогом);
//! - альтернативные хранилища объектов (`objects/info/alternates`) — каталоги внутри
//!   проекта без своих `alternates`; `http-alternates` нет;
//! - в `config` (и `config.worktree`) нет `include` и `includeIf`, частичного клона и
//!   промисоров, ключей, исполняющих команды (`core.sshCommand`, `core.fsmonitor`,
//!   `core.hooksPath`, `credential.*`, `filter.*`, `diff.*.textconv`, `!`-алиасов и др.),
//!   `core.worktree`, `core.bare`, сетевых путей в значениях, продолжений строк и прочего,
//!   что разбор не понял.
//!
//! Не прошёл — причина `git_repo_unsafe` (`.git` нет — `no_git`): git не вызывается.

use std::io::Read;
use std::path::{Path, PathBuf};

use za_core::Access;
use za_core::paths::{ResolvedPath, Roots, classify, open_checked, resolve, within};
use za_protocol::checkpoint_reason;

/// Предел размера конфига и файлов-указателей.
const MAX_CONFIG: u64 = 1 << 20;

/// Отказ проверки.
type Reason = &'static str;

const UNSAFE: Reason = checkpoint_reason::GIT_REPO_UNSAFE;

fn unsafe_because(why: &str) -> Reason {
    tracing::warn!(why, "репозиторий проекта не используется для чекпоинтов");
    UNSAFE
}

/// Проверка репозитория проекта `roots.project`.
pub(super) fn check(roots: &Roots) -> Result<(), Reason> {
    let root = &roots.project;
    let dot = root.join(".git");
    let meta = match std::fs::symlink_metadata(&dot) {
        Ok(m) => m,
        Err(_) => return Err(checkpoint_reason::NO_GIT),
    };
    let ft = meta.file_type();
    let gitdir = if ft.is_symlink() {
        return Err(unsafe_because(".git — ссылка"));
    } else if ft.is_dir() {
        dot
    } else if ft.is_file() {
        let text = read_small(&dot).ok_or_else(|| unsafe_because(".git не читается"))?;
        let line = text.lines().next().unwrap_or_default().trim();
        let target = line
            .strip_prefix("gitdir:")
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| unsafe_because("файл .git без gitdir"))?;
        inside_dir(roots, root, target).ok_or_else(|| unsafe_because("gitdir вне проекта"))?
    } else {
        return Err(unsafe_because(".git — не файл и не каталог"));
    };

    for entry in [
        "commondir",
        "objects",
        "refs",
        "config",
        "config.worktree",
        "HEAD",
        "index",
        "packed-refs",
        "info",
        "objects/info",
        "objects/info/alternates",
        "objects/info/http-alternates",
        "objects/pack",
        "shallow",
    ] {
        match std::fs::symlink_metadata(gitdir.join(entry)) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(unsafe_because("служебная запись .git — ссылка"));
            }
            Ok(_) if entry == "commondir" => {
                return Err(unsafe_because("связанный worktree (commondir)"));
            }
            Ok(_) if entry == "objects/info/http-alternates" => {
                return Err(unsafe_because("http-alternates"));
            }
            _ => {}
        }
    }
    check_alternates(roots, &gitdir.join("objects"))?;
    for cfg in ["config", "config.worktree"] {
        let p = gitdir.join(cfg);
        if std::fs::symlink_metadata(&p).is_err() {
            continue;
        }
        let text = read_small(&p).ok_or_else(|| unsafe_because("конфиг не читается"))?;
        check_config(&text).map_err(unsafe_because)?;
    }
    Ok(())
}

/// Каталог `arg` (относительно `base`) внутри проекта, без ссылок на пути; сетевой путь
/// отвергается лексически.
fn inside_dir(roots: &Roots, base: &Path, arg: &str) -> Option<PathBuf> {
    if looks_network(arg) {
        return None;
    }
    let joined = if Path::new(arg).is_absolute() {
        PathBuf::from(arg)
    } else {
        base.join(arg)
    };
    let lex = classify(&roots.project, &joined.to_string_lossy()).ok()?;
    let r = resolve(roots, &lex, Access::Read).ok()?;
    let lexical = lex.as_path();
    let same =
        lexical.components().count() == r.path.components().count() && within(lexical, &r.path);
    let meta = std::fs::symlink_metadata(&r.path).ok()?;
    (r.inside && same && meta.is_dir()).then_some(r.path)
}

/// Альтернативные хранилища: только каталоги внутри проекта без своих `alternates`.
fn check_alternates(roots: &Roots, objects: &Path) -> Result<(), Reason> {
    let alt = objects.join("info").join("alternates");
    if std::fs::symlink_metadata(&alt).is_err() {
        return Ok(());
    }
    let text = read_small(&alt).ok_or_else(|| unsafe_because("alternates не читается"))?;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('"') {
            return Err(unsafe_because("alternates в кавычках"));
        }
        let dir = inside_dir(roots, objects, line)
            .ok_or_else(|| unsafe_because("alternates вне проекта"))?;
        for nested in ["info/alternates", "info/http-alternates"] {
            if std::fs::symlink_metadata(dir.join(nested)).is_ok() {
                return Err(unsafe_because("вложенные alternates"));
            }
        }
    }
    Ok(())
}

/// Текст небольшого файла `.git` через `open_checked` (не ссылка, обычный файл).
fn read_small(p: &Path) -> Option<String> {
    let f = open_checked(
        &ResolvedPath {
            path: p.to_path_buf(),
            inside: true,
            protected: true,
        },
        false,
    )
    .ok()?;
    let mut bytes = Vec::new();
    (&f).take(MAX_CONFIG + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_CONFIG {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Сетевой путь по виду: `\\host`, `//host`, `\\?\UNC`, а также URL-подобные `file://`.
fn looks_network(v: &str) -> bool {
    let v = v.trim().trim_matches('"');
    v.starts_with("\\\\")
        || v.starts_with("//")
        || v.starts_with("\\/")
        || v.starts_with("/\\")
        || v.to_ascii_lowercase().starts_with("file:")
}

/// Ключи, исполняющие команды или меняющие место данных: `(раздел, ключ)`; ключ `*` —
/// любой. Раздел — без подраздела, в нижнем регистре.
const DENY: &[(&str, &str)] = &[
    ("core", "sshcommand"),
    ("core", "askpass"),
    ("core", "pager"),
    ("core", "editor"),
    ("core", "hookspath"),
    ("core", "gitproxy"),
    ("core", "alternaterefscommand"),
    ("core", "worktree"),
    ("core", "fsmonitorhookversion"),
    ("credential", "*"),
    ("filter", "*"),
    ("merge", "driver"),
    ("gpg", "program"),
    ("sequence", "editor"),
    ("uploadpack", "packobjectshook"),
    ("pager", "*"),
    ("difftool", "*"),
    ("mergetool", "*"),
    ("sendemail", "*"),
    ("web", "browser"),
    ("browser", "*"),
    ("man", "*"),
    ("instaweb", "*"),
    ("trace2", "*"),
    ("remote", "promisor"),
    ("remote", "partialclonefilter"),
    ("remote", "uploadpack"),
    ("remote", "receivepack"),
    ("remote", "vcs"),
    ("extensions", "partialclone"),
    ("diff", "external"),
    ("diff", "textconv"),
    ("diff", "command"),
];

/// Проверка текста конфига; `Err` — что не так.
fn check_config(text: &str) -> Result<(), &'static str> {
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.ends_with('\\') {
            return Err("продолжение строки в конфиге");
        }
        if let Some(rest) = line.strip_prefix('[') {
            let Some(end) = rest.find(']') else {
                return Err("неразобранный заголовок раздела");
            };
            let head = rest[..end].trim();
            // `[a "b"]` или `[a.b]`: имя раздела — до пробела или точки.
            let name = head
                .split(|c: char| c.is_whitespace() || c == '.')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if name.is_empty() {
                return Err("пустое имя раздела");
            }
            if name == "include" || name == "includeif" {
                return Err("include в конфиге");
            }
            section = name;
            let tail = rest[end + 1..].trim();
            if tail.is_empty() || tail.starts_with('#') || tail.starts_with(';') {
                continue;
            }
            // Ключ на строке заголовка: `[core] sshCommand = x`.
            check_kv(&section, tail)?;
            continue;
        }
        if section.is_empty() {
            return Err("ключ вне раздела");
        }
        check_kv(&section, line)?;
    }
    Ok(())
}

fn check_kv(section: &str, line: &str) -> Result<(), &'static str> {
    let (key, value) = match line.split_once('=') {
        Some((k, v)) => (k.trim(), v.trim()),
        None => (line, "true"),
    };
    let key = key.to_ascii_lowercase();
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err("неразобранный ключ");
    }
    if DENY
        .iter()
        .any(|(s, k)| *s == section && (*k == "*" || *k == key))
    {
        return Err("ключ, исполняющий команды или меняющий место данных");
    }
    let v = value.trim_matches('"');
    let falsy = matches!(
        v.to_ascii_lowercase().as_str(),
        "" | "false" | "no" | "off" | "0"
    );
    match (section, key.as_str()) {
        ("core", "fsmonitor") if !falsy => return Err("core.fsmonitor"),
        ("core", "bare") if !falsy => return Err("core.bare"),
        ("alias", _) if v.trim_start().starts_with('!') => return Err("алиас-команда"),
        ("submodule", "update") if v.trim_start().starts_with('!') => {
            return Err("submodule.update-команда");
        }
        _ => {}
    }
    if looks_network(v) {
        return Err("сетевой путь в конфиге");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_rules() {
        let ok = "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tbare = false\n\tfsmonitor = false\n[remote \"origin\"]\n\turl = https://example.invalid/x\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n[branch \"main\"]\n\tremote = origin\n[alias]\n\tco = checkout\n";
        assert_eq!(check_config(ok), Ok(()));
        for bad in [
            "[include]\n path = x\n",
            "[includeIf \"gitdir:/x/\"]\n path = x\n",
            "[core]\n sshCommand = x\n",
            "[core] sshCommand = x\n",
            "[core]\n fsmonitor = /x.sh\n",
            "[filter \"x\"]\n clean = cat\n",
            "[credential]\n helper = store\n",
            "[credential \"https://h\"]\n helper = x\n",
            "[extensions]\n partialClone = origin\n",
            "[remote \"origin\"]\n promisor = true\n",
            "[core]\n worktree = ..\n",
            "[core]\n bare = true\n",
            "[core]\n excludesFile = //host/share/x\n",
            "[core]\n excludesFile = \\\\\\\\host\\\\share\\\\x\n",
            "[alias]\n st = !sh -c x\n",
            "[diff \"x\"]\n textconv = cat\n",
            "[core]\n pager = \\\nless\n",
            "sshCommand = x\n",
            "[core\n",
        ] {
            assert!(check_config(bad).is_err(), "{bad:?}");
        }
    }
}
