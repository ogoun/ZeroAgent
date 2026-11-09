//! Проверка политики зависимостей и архитектуры: `cargo xtask policy`.
//!
//! Правила: только crates.io в `Cargo.lock`, запрет перечисленных
//! агентных фреймворков, направление зависимостей между `za-*`
//! (`za-res` — только от `za-protocol`, `za-core` и `za-agent` — `za-llm` без функций
//! по умолчанию, `za-agent` — только над нижними слоями, и они от него не зависят),
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
const CORE_FORBIDDEN_DEPS: &[&str] = &[
    "za-server",
    "za-peer",
    "za-browser",
    // `za-agent` — общим правилом нижних слоёв ниже.
    "za-res",
    "reqwest",
];
/// Крейты workspace, от которых может зависеть `za-agent` (ADR 0004).
const AGENT_ALLOWED_ZA_DEPS: &[&str] = &["za-protocol", "za-store", "za-core", "za-llm", "za-res"];
/// Внешние крейты, которые `za-agent` подключать не должен: модель — только через `ModelPort`.
const AGENT_FORBIDDEN_DEPS: &[&str] = &["reqwest"];
/// Единственный крейт `za-*`, от которого может зависеть `za-res` (D1).
const RES_ALLOWED_ZA_DEPS: &[&str] = &["za-protocol"];

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
    /// `default-features` зависимости; в `cargo metadata` поле есть всегда.
    #[serde(default = "default_true")]
    pub uses_default_features: bool,
    /// `None` — обычная зависимость, иначе `"dev"` или `"build"`.
    #[serde(default)]
    pub kind: Option<String>,
}

fn default_true() -> bool {
    true
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
            // Все виды, включая dev: `za-llm` сам зависит от `za-res` (D1).
            if pkg.name == "za-res"
                && dep.name.starts_with("za-")
                && !RES_ALLOWED_ZA_DEPS.contains(&dep.name.as_str())
            {
                out.push(Violation(format!(
                    "za-res не должен зависеть от {} (допустим только za-protocol)",
                    dep.name
                )));
            }
            // Только то, что попадает в сборку ядра: dev-зависимости тестов не в счёт.
            if pkg.name == "za-core"
                && dep.name == "za-llm"
                && dep.kind.as_deref() != Some("dev")
                && dep.uses_default_features
            {
                out.push(Violation(
                    "za-core подключает za-llm с функциями по умолчанию (нужно default-features = false)"
                        .into(),
                ));
            }
            // Все виды: нижние слои не видят цикл даже в тестах (ADR 0004).
            if AGENT_ALLOWED_ZA_DEPS.contains(&pkg.name.as_str()) && dep.name == "za-agent" {
                out.push(Violation(format!(
                    "{} не должен зависеть от za-agent",
                    pkg.name
                )));
            }
            if pkg.name == "za-agent"
                && is_workspace_crate(&dep.name)
                && !AGENT_ALLOWED_ZA_DEPS.contains(&dep.name.as_str())
            {
                out.push(Violation(format!(
                    "za-agent не должен зависеть от {} (допустимы {})",
                    dep.name,
                    AGENT_ALLOWED_ZA_DEPS.join(", ")
                )));
            }
            if pkg.name == "za-agent"
                && dep.kind.as_deref() != Some("dev")
                && AGENT_FORBIDDEN_DEPS.contains(&dep.name.as_str())
            {
                out.push(Violation(format!(
                    "za-agent не должен зависеть от {}",
                    dep.name
                )));
            }
            if pkg.name == "za-agent"
                && dep.name == "za-llm"
                && dep.kind.as_deref() != Some("dev")
                && dep.uses_default_features
            {
                out.push(Violation(
                    "za-agent подключает za-llm с функциями по умолчанию (нужно default-features = false)"
                        .into(),
                ));
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

/// Крейт workspace: `za-*` или бинарник `zero-agent`.
fn is_workspace_crate(name: &str) -> bool {
    name.starts_with("za-") || name == "zero-agent"
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
    fn dep(name: &str) -> Dependency {
        Dependency {
            name: name.into(),
            uses_default_features: true,
            kind: None,
        }
    }
    fn meta_with(name: &str, dep: Dependency) -> Metadata {
        Metadata {
            packages: vec![Package {
                name: name.into(),
                dependencies: vec![dep],
            }],
        }
    }
    fn meta(name: &str, d: &str) -> Metadata {
        meta_with(name, dep(d))
    }
    #[test]
    fn res_must_not_depend_on_llm() {
        assert_eq!(check_direction(&meta("za-res", "za-llm")).len(), 1);
        assert_eq!(check_direction(&meta("za-res", "za-store")).len(), 1);
        let dev = Dependency {
            kind: Some("dev".into()),
            ..dep("za-llm")
        };
        assert_eq!(check_direction(&meta_with("za-res", dev)).len(), 1);
        assert!(check_direction(&meta("za-res", "za-protocol")).is_empty());
        assert!(check_direction(&meta("za-res", "sysinfo")).is_empty());
    }
    #[test]
    fn core_uses_llm_without_defaults() {
        assert_eq!(check_direction(&meta("za-core", "za-llm")).len(), 1);
        let no_defaults = Dependency {
            uses_default_features: false,
            ..dep("za-llm")
        };
        assert!(check_direction(&meta_with("za-core", no_defaults)).is_empty());
        let dev = Dependency {
            kind: Some("dev".into()),
            ..dep("za-llm")
        };
        assert!(check_direction(&meta_with("za-core", dev)).is_empty());
    }
    #[test]
    fn metadata_dependency_fields_are_parsed() {
        let json = r#"{"packages":[{"name":"za-core","dependencies":[
            {"name":"za-llm","uses_default_features":false,"kind":null},
            {"name":"tokio","uses_default_features":true,"kind":"dev"}]}]}"#;
        let m: Metadata = serde_json::from_str(json).unwrap();
        let deps = &m.packages[0].dependencies;
        assert!(!deps[0].uses_default_features);
        assert_eq!(deps[0].kind, None);
        assert_eq!(deps[1].kind.as_deref(), Some("dev"));
        assert!(check_direction(&m).is_empty());
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
    fn policy_rejects_core_on_agent() {
        assert_eq!(check_direction(&meta("za-core", "za-agent")).len(), 1);
        let dev = Dependency {
            kind: Some("dev".into()),
            ..dep("za-agent")
        };
        assert_eq!(check_direction(&meta_with("za-core", dev)).len(), 1);
        for lower in ["za-protocol", "za-store", "za-llm", "za-res"] {
            assert!(
                !check_direction(&meta(lower, "za-agent")).is_empty(),
                "{lower} → za-agent"
            );
        }
        assert!(check_direction(&meta("zero-agent", "za-agent")).is_empty());
    }
    #[test]
    fn agent_uses_llm_without_defaults() {
        assert_eq!(check_direction(&meta("za-agent", "za-llm")).len(), 1);
        let no_defaults = Dependency {
            uses_default_features: false,
            ..dep("za-llm")
        };
        assert!(check_direction(&meta_with("za-agent", no_defaults)).is_empty());
        let dev = Dependency {
            kind: Some("dev".into()),
            ..dep("za-llm")
        };
        assert!(check_direction(&meta_with("za-agent", dev)).is_empty());
    }
    #[test]
    fn agent_depends_only_on_lower_layers() {
        for ok in ["za-protocol", "za-store", "za-core", "za-res", "serde"] {
            assert!(check_direction(&meta("za-agent", ok)).is_empty(), "{ok}");
        }
        assert_eq!(check_direction(&meta("za-agent", "zero-agent")).len(), 1);
        assert_eq!(check_direction(&meta("za-agent", "za-memory")).len(), 1);
        assert_eq!(check_direction(&meta("za-agent", "reqwest")).len(), 1);
    }
    #[test]
    fn core_must_not_depend_on_res_or_http() {
        assert_eq!(check_direction(&meta("za-core", "za-res")).len(), 1);
        assert_eq!(check_direction(&meta("za-core", "reqwest")).len(), 1);
    }
    #[test]
    fn core_over_limit_is_reported() {
        assert!(check_core_size(5001, 5000).is_some());
        assert!(check_core_size(5000, 5000).is_none());
    }
}
