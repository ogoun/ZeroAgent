//! Проверка политики зависимостей и архитектуры: `cargo xtask policy`.
//!
//! Правила: только crates.io в `Cargo.lock`, запрет перечисленных
//! агентных фреймворков, направление зависимостей между `za-*`,
//! лимит размера `za-core` (ARC-011).

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde::Deserialize;

/// Допустимый префикс источника пакета в `Cargo.lock`.
const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";
/// Запрещённые пакеты (чужие агентные фреймворки).
const FORBIDDEN_PACKAGES: &[&str] = &["rig-core", "adk-rust", "autoagents"];
/// Крейты, от которых `za-core` зависеть не должен.
const CORE_FORBIDDEN_DEPS: &[&str] = &["za-server", "za-peer", "za-browser"];

/// Нарушение политики.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation(pub String);

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Метаданные workspace (подмножество `cargo metadata --no-deps`).
#[derive(Debug, Deserialize)]
pub struct Metadata {
    pub packages: Vec<Package>,
}

#[derive(Debug, Deserialize)]
pub struct Package {
    pub name: String,
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug, Deserialize)]
pub struct Dependency {
    pub name: String,
}

/// Проверяет источники и имена пакетов в тексте `Cargo.lock`.
pub fn check_lock_sources(lock: &str) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut name = String::new();
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            name.clear();
        } else if let Some(v) = value_of(line, "name") {
            name = v.to_string();
            if FORBIDDEN_PACKAGES.contains(&v) {
                out.push(Violation(format!("запрещённый пакет `{v}`")));
            }
        } else if let Some(src) = value_of(line, "source")
            && !src.starts_with(CRATES_IO)
        {
            out.push(Violation(format!(
                "пакет `{name}`: недопустимый источник `{src}`"
            )));
        }
    }
    out
}

/// Значение строки вида `key = "value"`.
fn value_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.strip_prefix(key)?
        .trim_start()
        .strip_prefix('=')?
        .trim()
        .strip_prefix('"')?
        .strip_suffix('"')
}

/// Проверяет направление зависимостей между крейтами workspace.
pub fn check_direction(metadata: &Metadata) -> Vec<Violation> {
    let mut out = Vec::new();
    for pkg in &metadata.packages {
        for dep in &pkg.dependencies {
            if pkg.name == "za-core" && CORE_FORBIDDEN_DEPS.contains(&dep.name.as_str()) {
                out.push(Violation(format!(
                    "za-core не должен зависеть от {}",
                    dep.name
                )));
            }
            if pkg.name == "za-protocol" && dep.name.starts_with("za-") {
                out.push(Violation(format!(
                    "za-protocol не должен зависеть от {}",
                    dep.name
                )));
            }
        }
    }
    out
}

/// Проверяет, что размер `za-core` в строках не превышает лимит.
pub fn check_core_size(lines: usize, limit: usize) -> Option<Violation> {
    (lines > limit).then(|| Violation(format!("za-core: {lines} строк при лимите {limit}")))
}

fn count_lines(dir: &Path) -> std::io::Result<usize> {
    let mut total = 0;
    if !dir.exists() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            total += count_lines(&path)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            total += std::fs::read_to_string(&path)?.lines().count();
        }
    }
    Ok(total)
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("корень workspace")
        .to_path_buf()
}

fn collect(root: &Path) -> Result<Vec<Violation>, String> {
    let mut out = Vec::new();
    let lock =
        std::fs::read_to_string(root.join("Cargo.lock")).map_err(|e| format!("Cargo.lock: {e}"))?;
    out.extend(check_lock_sources(&lock));

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let res = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !res.status.success() {
        return Err(format!(
            "cargo metadata: {}",
            String::from_utf8_lossy(&res.stderr)
        ));
    }
    let metadata: Metadata =
        serde_json::from_slice(&res.stdout).map_err(|e| format!("metadata: {e}"))?;
    out.extend(check_direction(&metadata));

    let limit_text = std::fs::read_to_string(root.join("xtask/core-size-limit.txt"))
        .map_err(|e| format!("core-size-limit.txt: {e}"))?;
    let limit: usize = limit_text
        .trim()
        .parse()
        .map_err(|e| format!("core-size-limit.txt: {e}"))?;
    let lines =
        count_lines(&root.join("crates/za-core/src")).map_err(|e| format!("za-core: {e}"))?;
    out.extend(check_core_size(lines, limit));
    Ok(out)
}

/// Точка входа команды `policy`.
pub fn run() -> ExitCode {
    match collect(&root()) {
        Ok(v) if v.is_empty() => {
            println!("policy: нарушений нет");
            ExitCode::SUCCESS
        }
        Ok(v) => {
            for x in &v {
                eprintln!("policy: {x}");
            }
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("policy: ошибка: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_dependency_is_rejected() {
        let lock = "[[package]]\nname = \"x\"\nversion = \"1.0.0\"\nsource = \"git+https://example.com/x\"\n";
        assert_eq!(check_lock_sources(lock).len(), 1);
    }
    #[test]
    fn crates_io_source_is_allowed() {
        let lock = "[[package]]\nname = \"x\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
        assert!(check_lock_sources(lock).is_empty());
    }
    #[test]
    fn forbidden_package_is_rejected() {
        let lock = "[[package]]
name = \"rig-core\"
version = \"1.0.0\"
";
        assert_eq!(check_lock_sources(lock).len(), 1);
    }
    #[test]
    fn path_package_without_source_is_allowed() {
        assert!(
            check_lock_sources(
                "[[package]]
name = \"za-core\"
version = \"0.0.1\"
"
            )
            .is_empty()
        );
    }
    fn meta(name: &str, dep: &str) -> Metadata {
        Metadata {
            packages: vec![Package {
                name: name.into(),
                dependencies: vec![Dependency { name: dep.into() }],
            }],
        }
    }
    #[test]
    fn core_must_not_depend_on_server() {
        assert_eq!(check_direction(&meta("za-core", "za-server")).len(), 1);
        assert!(check_direction(&meta("za-core", "za-protocol")).is_empty());
    }
    #[test]
    fn protocol_must_not_depend_on_za_crates() {
        assert_eq!(check_direction(&meta("za-protocol", "za-store")).len(), 1);
        assert!(check_direction(&meta("za-protocol", "serde")).is_empty());
    }
    #[test]
    fn core_over_limit_is_reported() {
        assert!(check_core_size(5001, 5000).is_some());
        assert!(check_core_size(5000, 5000).is_none());
    }
}
