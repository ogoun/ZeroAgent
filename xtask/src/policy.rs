//! Проверка политики зависимостей и архитектуры: `cargo xtask policy`.
//!
//! Правила: только crates.io в `Cargo.lock`, запрет перечисленных
//! агентных фреймворков, направление зависимостей между `za-*`
//! (`za-res` — только от `za-protocol` и `za-jail`, `za-core` и `za-agent` — `za-llm` без функций
//! по умолчанию, `za-agent` — только над нижними слоями, и они от него не зависят),
//! лимит размера `za-core` (ARC-011), инструменты сборки (`tiktoken-rs`) — только у
//! `xtask`. R1-C1 (D73): направления `za-jail`, `za-tools`, `za-secrets`; запуск процессов
//! (`std::process::Command`, `tokio::process::Command`) в `crates/*/src` — только в
//! `za-jail` (SEC-006); в `Cargo.lock` нет `openssl-sys`, `native-tls`, `libdbus-sys`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde::Deserialize;

/// Допустимый префикс источника пакета в `Cargo.lock`.
const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";
/// Запрещённые пакеты (чужие агентные фреймворки).
const FORBIDDEN_PACKAGES: &[&str] = &["rig-core", "adk-rust", "autoagents"];
/// Запрещённые пакеты: TLS только встроенный (rustls), без системного D-Bus (R1-C1).
const FORBIDDEN_SYSTEM_PACKAGES: &[&str] = &["openssl-sys", "native-tls", "libdbus-sys"];
/// Крейты, от которых `za-core` зависеть не должен.
const CORE_FORBIDDEN_DEPS: &[&str] = &[
    "za-server",
    "za-peer",
    "za-browser",
    // `za-agent` — общим правилом нижних слоёв ниже.
    "za-res",
    "reqwest",
    // Крейты R1-C1 стоят над `za-core` (D73).
    "za-jail",
    "za-tools",
    "za-secrets",
];
/// Крейты workspace, от которых может зависеть `za-agent` (ADR 0004).
/// R1-C1 D73: `za-secrets` — да; `za-tools` и `za-jail` — нет ни в каком виде.
const AGENT_ALLOWED_ZA_DEPS: &[&str] = &[
    "za-protocol",
    "za-store",
    "za-core",
    "za-llm",
    "za-res",
    "za-secrets",
];
/// Крейты, подключающие `za-llm` только без функций: без HTTP и `Gateway`.
const PURE_LLM_USERS: &[&str] = &["za-core", "za-agent"];
/// Внешние крейты, которые `za-agent` подключать не должен: модель — только через `ModelPort`.
const AGENT_FORBIDDEN_DEPS: &[&str] = &["reqwest"];
/// Крейты `za-*`, от которых может зависеть `za-res`: протокол (D1) и запуск
/// `nvidia-smi` через `za-jail` (R1-C1 D73).
const RES_ALLOWED_ZA_DEPS: &[&str] = &["za-protocol", "za-jail"];
/// Крейты `za-*`, от которых может зависеть `za-tools` (обычные зависимости, R1-C1 D73).
const TOOLS_ALLOWED_ZA_DEPS: &[&str] = &["za-protocol", "za-core", "za-jail"];
/// Где можно запускать процессы, и почему (SEC-006). Путь — от корня, через `/`.
const PROCESS_ALLOWED: &[(&str, &str)] = &[(
    "crates/za-jail/",
    "единственная точка запуска процессов: дерево, тайм-аут, окружение (R1-C1 D84)",
)];
/// Крейты только для `xtask` (R1-B2 D64): счёт токенов промптов в CI, не в продукте.
const XTASK_ONLY_DEPS: &[&str] = &["tiktoken-rs"];

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
    /// Явно включённые функции зависимости (`features = [...]`).
    #[serde(default)]
    pub features: Vec<String>,
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
            if FORBIDDEN_SYSTEM_PACKAGES.contains(&v) {
                out.push(Violation(format!(
                    "запрещённый пакет `{v}`: TLS только rustls, без системных OpenSSL и D-Bus"
                )));
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
                    "za-res не должен зависеть от {} (допустимы {})",
                    dep.name,
                    RES_ALLOWED_ZA_DEPS.join(", ")
                )));
            }
            // Все виды: `za-jail` — нижний слой без крейтов workspace (D73).
            if pkg.name == "za-jail" && is_workspace_crate(&dep.name) {
                out.push(Violation(format!(
                    "za-jail не должен зависеть от {}",
                    dep.name
                )));
            }
            // Обычные зависимости: `za-secrets` — без крейтов workspace и внешних (D73).
            if pkg.name == "za-secrets" && dep.kind.as_deref() != Some("dev") {
                out.push(Violation(format!(
                    "za-secrets не должен иметь зависимостей ({})",
                    dep.name
                )));
            }
            if pkg.name == "za-tools"
                && dep.kind.as_deref() != Some("dev")
                && is_workspace_crate(&dep.name)
                && !TOOLS_ALLOWED_ZA_DEPS.contains(&dep.name.as_str())
            {
                out.push(Violation(format!(
                    "za-tools не должен зависеть от {} (допустимы {})",
                    dep.name,
                    TOOLS_ALLOWED_ZA_DEPS.join(", ")
                )));
            }
            // Только то, что попадает в сборку: dev-зависимости тестов не в счёт.
            // `za-llm` — без функций по умолчанию и без явных (`http`, `gateway`, `probe`).
            if PURE_LLM_USERS.contains(&pkg.name.as_str())
                && dep.name == "za-llm"
                && dep.kind.as_deref() != Some("dev")
            {
                if dep.uses_default_features {
                    out.push(Violation(format!(
                        "{} подключает za-llm с функциями по умолчанию (нужно default-features = false)",
                        pkg.name
                    )));
                } else if !dep.features.is_empty() {
                    out.push(Violation(format!(
                        "{} подключает za-llm с функциями {:?} (допустим только пустой набор)",
                        pkg.name, dep.features
                    )));
                }
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
            // Все виды: и dev-зависимость тянет крейт в сборку тестов продукта.
            if pkg.name != "xtask" && XTASK_ONLY_DEPS.contains(&dep.name.as_str()) {
                out.push(Violation(format!(
                    "{} не должен зависеть от {} (только xtask)",
                    pkg.name, dep.name
                )));
            }
            // `za-shell` — чистый разбор без зависимостей; продукт видит его только
            // через `za_core::shell` (R1-C1 D99).
            if pkg.name == "za-shell" && dep.kind.as_deref() != Some("dev") {
                out.push(Violation(format!(
                    "za-shell не должен иметь зависимостей ({})",
                    dep.name
                )));
            }
            if dep.name == "za-shell" && pkg.name != "za-core" {
                out.push(Violation(format!(
                    "{} не должен зависеть от za-shell (только za-core, D99)",
                    pkg.name
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

/// Итоговые функции крейта `krate` из вывода `cargo tree --prefix none -f "{p}|{f}"`;
/// `None` — крейта в дереве нет. Повторы (`(*)`) объединяются.
pub fn parse_tree_features(tree: &str, krate: &str) -> Option<Vec<String>> {
    let mut found: Option<Vec<String>> = None;
    for line in tree.lines() {
        let Some((pkg, feats)) = line.split_once('|') else {
            continue;
        };
        if pkg.split(' ').next() != Some(krate) {
            continue;
        }
        let list = found.get_or_insert_with(Vec::new);
        let feats = feats.trim_end().trim_end_matches("(*)").trim();
        for f in feats.split(',').map(str::trim).filter(|f| !f.is_empty()) {
            if !list.iter().any(|x| x == f) {
                list.push(f.to_string());
            }
        }
    }
    found
}

/// Нарушение, если в итоговой сборке `pkg` у `za-llm` включена хоть одна функция.
pub fn check_resolved_llm_features(pkg: &str, features: &[String]) -> Option<Violation> {
    (!features.is_empty()).then(|| {
        Violation(format!(
            "{pkg}: в сборке `-p {pkg}` у za-llm включены функции {features:?} (допустим только пустой набор)"
        ))
    })
}

/// Итоговые функции `za-llm` в сборке `-p pkg` (обычные зависимости, все цели).
///
/// `resolve` из `cargo metadata` для этого не годится: он объединяет функции по всему
/// workspace (бинарник `zero-agent` включает `gateway`), а `cargo tree -p` разрешает их
/// так же, как `cargo build -p` — с собственным `[features]` и транзитивными включениями.
fn resolved_llm_features(root: &Path, pkg: &str) -> Result<Vec<String>, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let res = Command::new(cargo)
        .args([
            "tree", "--locked", "--color", "never", "-p", pkg, "-e", "normal", "--target", "all",
            "--prefix", "none", "-f", "{p}|{f}",
        ])
        // CI задаёт CARGO_TERM_COLOR=always: без явного отказа от цвета маркер `(*)` приходит
        // с ANSI-кодами и разбирается как имя функции.
        .env("CARGO_TERM_COLOR", "never")
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo tree -p {pkg}: {e}"))?;
    if !res.status.success() {
        return Err(format!(
            "cargo tree -p {pkg}: {}",
            String::from_utf8_lossy(&res.stderr)
        ));
    }
    Ok(parse_tree_features(&String::from_utf8_lossy(&res.stdout), "za-llm").unwrap_or_default())
}

/// Запуск процессов в исходнике `rel` (путь от корня через `/`) вне [`PROCESS_ALLOWED`]:
/// `std::process::Command`, `tokio::process::Command`, `Command::new`, импорт `Command`
/// из `process::{…}`. Строки-комментарии не считаются.
pub fn check_process_spawn(rel: &str, text: &str) -> Vec<Violation> {
    if PROCESS_ALLOWED
        .iter()
        .any(|(prefix, _)| rel.starts_with(prefix))
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let code = line.trim_start();
        if code.starts_with("//") {
            continue;
        }
        let hit = code.contains("process::Command")
            || code.contains("Command::new")
            || (code.contains("process::{") && code.contains("Command"));
        if hit {
            out.push(Violation(format!(
                "{rel}:{}: запуск процесса вне za-jail (только через za_jail::Runner, SEC-006)",
                i + 1
            )));
        }
    }
    out
}

/// Исходники `crates/*/src/**/*.rs`: путь от корня через `/` и текст.
fn crate_sources(root: &Path) -> std::io::Result<Vec<(String, String)>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(root, &path, out)?;
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel, std::fs::read_to_string(&path)?));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    for krate in std::fs::read_dir(root.join("crates"))? {
        let src = krate?.path().join("src");
        if src.is_dir() {
            walk(root, &src, &mut out)?;
        }
    }
    Ok(out)
}

/// Крейт workspace: `za-*` или бинарник `zero-agent`.
fn is_workspace_crate(name: &str) -> bool {
    name.starts_with("za-") || name == "zero-agent"
}

/// Проверяет, что размер `za-core` в строках не превышает лимит.
pub fn check_core_size(lines: usize, limit: usize) -> Option<Violation> {
    check_size("za-core", lines, limit)
}

/// Проверяет, что размер крейта в строках не превышает лимит.
pub fn check_size(krate: &str, lines: usize, limit: usize) -> Option<Violation> {
    (lines > limit).then(|| Violation(format!("{krate}: {lines} строк при лимите {limit}")))
}

/// Лимит строк из файла `xtask/<file>`.
fn read_limit(root: &Path, file: &str) -> Result<usize, String> {
    std::fs::read_to_string(root.join("xtask").join(file))
        .map_err(|e| format!("{file}: {e}"))?
        .trim()
        .parse()
        .map_err(|e| format!("{file}: {e}"))
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
    for pkg in PURE_LLM_USERS {
        let features = resolved_llm_features(root, pkg)?;
        out.extend(check_resolved_llm_features(pkg, &features));
    }

    let limit = read_limit(root, "core-size-limit.txt")?;
    let lines =
        count_lines(&root.join("crates/za-core/src")).map_err(|e| format!("za-core: {e}"))?;
    out.extend(check_core_size(lines, limit));
    let limit = read_limit(root, "shell-size-limit.txt")?;
    let lines =
        count_lines(&root.join("crates/za-shell/src")).map_err(|e| format!("za-shell: {e}"))?;
    out.extend(check_size("za-shell", lines, limit));
    for (rel, text) in crate_sources(root).map_err(|e| format!("crates/*/src: {e}"))? {
        out.extend(check_process_spawn(&rel, &text));
    }
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
            features: Vec::new(),
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
    fn llm_features_must_be_empty_for_core_and_agent() {
        for pkg in ["za-agent", "za-core"] {
            for f in ["gateway", "http", "probe"] {
                let with = Dependency {
                    uses_default_features: false,
                    features: vec![f.into()],
                    ..dep("za-llm")
                };
                assert_eq!(
                    check_direction(&meta_with(pkg, with)).len(),
                    1,
                    "{pkg}: {f}"
                );
            }
            let dev = Dependency {
                kind: Some("dev".into()),
                features: vec!["gateway".into()],
                ..dep("za-llm")
            };
            assert!(check_direction(&meta_with(pkg, dev)).is_empty(), "{pkg}");
        }
        let json = r#"{"packages":[{"name":"za-agent","dependencies":[
            {"name":"za-llm","uses_default_features":false,"kind":null,"features":["gateway"]}]}]}"#;
        let m: Metadata = serde_json::from_str(json).unwrap();
        assert_eq!(m.packages[0].dependencies[0].features, vec!["gateway"]);
        assert_eq!(check_direction(&m).len(), 1);
    }
    #[test]
    fn resolved_llm_features_are_parsed_from_tree() {
        let clean = "za-agent v0.0.1 (H:\\p\\crates\\za-agent)|\n\
            za-core v0.0.1 (H:\\p\\crates\\za-core)|\n\
            za-llm v0.0.1 (H:\\p\\crates\\za-llm)|\n\
            za-llm v0.0.1 (H:\\p\\crates\\za-llm)| (*)\n\
            tokio v1.54.1|default,sync\n";
        assert_eq!(
            parse_tree_features(clean, "za-llm"),
            Some(Vec::<String>::new())
        );
        assert_eq!(check_resolved_llm_features("za-agent", &[]), None);
        // Включение через собственный `[features]` или транзитивно через другой крейт
        // видно только в итоговом наборе сборки `-p za-agent`.
        let dirty = "za-agent v0.0.1 (/p/za-agent)|x\n\
            za-llm v0.0.1 (/p/za-llm)|gateway,http\n\
            za-llm v0.0.1 (/p/za-llm)|gateway,http (*)\n";
        let f = parse_tree_features(dirty, "za-llm").unwrap();
        assert_eq!(f, vec!["gateway", "http"]);
        assert!(check_resolved_llm_features("za-agent", &f).is_some());
        assert_eq!(parse_tree_features("tokio v1|sync\n", "za-llm"), None);
        // Префикс имени не путается: `za-llm-x` — другой крейт.
        assert_eq!(
            parse_tree_features("za-llm-x v1 (/p)|http\n", "za-llm"),
            None
        );
    }
    #[test]
    fn core_must_not_depend_on_res_or_http() {
        assert_eq!(check_direction(&meta("za-shell", "serde")).len(), 1);
        assert_eq!(check_direction(&meta("za-agent", "za-shell")).len(), 2);
        assert!(check_direction(&meta("za-core", "za-shell")).is_empty());
        assert!(check_size("za-shell", 10, 10).is_none());
        assert!(check_size("za-shell", 11, 10).is_some());
        assert_eq!(check_direction(&meta("za-core", "za-res")).len(), 1);
        assert_eq!(check_direction(&meta("za-core", "reqwest")).len(), 1);
    }
    #[test]
    fn tiktoken_only_in_xtask() {
        assert!(check_direction(&meta("xtask", "tiktoken-rs")).is_empty());
        for pkg in ["za-agent", "za-llm", "zero-agent"] {
            assert_eq!(check_direction(&meta(pkg, "tiktoken-rs")).len(), 1, "{pkg}");
        }
        let dev = Dependency {
            kind: Some("dev".into()),
            ..dep("tiktoken-rs")
        };
        assert_eq!(check_direction(&meta_with("za-core", dev)).len(), 1);
    }
    #[test]
    fn system_tls_and_dbus_packages_are_rejected() {
        for p in ["openssl-sys", "native-tls", "libdbus-sys"] {
            let lock = format!(
                "[[package]]\nname = \"{p}\"\nversion = \"1.0.0\"\nsource = \"{CRATES_IO}\"\n"
            );
            assert_eq!(check_lock_sources(&lock).len(), 1, "{p}");
        }
    }
    #[test]
    fn r1c1_crate_directions() {
        // za-jail — без крейтов workspace, в том числе dev.
        assert_eq!(check_direction(&meta("za-jail", "za-core")).len(), 1);
        let dev = Dependency {
            kind: Some("dev".into()),
            ..dep("za-protocol")
        };
        assert_eq!(check_direction(&meta_with("za-jail", dev)).len(), 1);
        assert!(check_direction(&meta("za-jail", "process-wrap")).is_empty());
        // za-res → za-jail разрешено.
        assert!(check_direction(&meta("za-res", "za-jail")).is_empty());
        // za-agent → za-secrets да, za-tools и za-jail нет ни в каком виде.
        assert!(check_direction(&meta("za-agent", "za-secrets")).is_empty());
        for d in ["za-tools", "za-jail"] {
            assert_eq!(check_direction(&meta("za-agent", d)).len(), 1, "{d}");
            let dev = Dependency {
                kind: Some("dev".into()),
                ..dep(d)
            };
            assert_eq!(
                check_direction(&meta_with("za-agent", dev)).len(),
                1,
                "dev {d}"
            );
        }
        // za-core — не от новых крейтов.
        for d in ["za-jail", "za-tools", "za-secrets"] {
            assert_eq!(check_direction(&meta("za-core", d)).len(), 1, "{d}");
        }
        // za-tools — только от za-protocol, za-core, za-jail.
        for ok in ["za-protocol", "za-core", "za-jail", "similar"] {
            assert!(check_direction(&meta("za-tools", ok)).is_empty(), "{ok}");
        }
        assert_eq!(check_direction(&meta("za-tools", "za-store")).len(), 1);
        // xtask → za-tools (случаи бюджета промптов, D93).
        assert!(check_direction(&meta("xtask", "za-tools")).is_empty());
        // za-secrets — без зависимостей.
        assert_eq!(check_direction(&meta("za-secrets", "serde")).len(), 1);
        assert_eq!(check_direction(&meta("za-secrets", "za-protocol")).len(), 1);
    }
    #[test]
    fn process_spawn_outside_jail_is_rejected() {
        let v = check_process_spawn(
            "crates/za-tools/src/x.rs",
            "let c = Command::new(\"git\");\n",
        );
        assert_eq!(v.len(), 1);
        assert!(v[0].0.contains("crates/za-tools/src/x.rs:1"), "{v:?}");
        for line in [
            "use std::process::Command;",
            "use tokio::process::Command;",
            "use std::process::{Command, Stdio};",
            "let c = tokio::process::Command::new(exe);",
        ] {
            assert_eq!(
                check_process_spawn("crates/za-res/src/gpu.rs", line).len(),
                1,
                "{line}"
            );
        }
        assert!(check_process_spawn("crates/za-jail/src/direct.rs", "Command::new(x)").is_empty());
        assert!(
            check_process_spawn("crates/za-core/src/a.rs", "// Command::new — пример").is_empty()
        );
        assert!(
            check_process_spawn("crates/za-core/src/a.rs", "use std::process::Stdio;").is_empty()
        );
    }
    #[test]
    fn core_over_limit_is_reported() {
        assert!(check_core_size(5001, 5000).is_some());
        assert!(check_core_size(5000, 5000).is_none());
    }
}
