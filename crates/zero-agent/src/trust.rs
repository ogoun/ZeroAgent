//! Доверие к проекту (SEC-030, R1-C1 D83; решение заказчика (б)): набор доверенных файлов
//! проекта с их хэшами BLAKE3.
//!
//! Набор пишется в запись аудита `trust.granted` в момент ответа на прерывание доверия
//! (`answer --interrupt trust-… --approve`, D91). Сравнение с набором на старте `run`,
//! команда `trust` и чтение `AGENTS.md` у доверенного проекта добавляет задача 13 плана.
//!
//! Ссылки не разыменовываются: у ссылки хэшируется её цель как текст, поэтому подмена
//! файла ссылкой (или цели ссылки) меняет набор.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use za_protocol::canonical_json;

/// Доверенные файлы в корне проекта.
pub const TRUSTED_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".zero-agent/config.toml"];
/// Доверенные деревья (скиллы).
pub const TRUSTED_TREES: &[&str] = &[".zero-agent/skills", ".agents/skills", ".claude/skills"];

/// Набор доверенных файлов: путь относительно проекта (через `/`) → BLAKE3 в hex. У
/// дерева один элемент — хэш отсортированного списка пар (путь, хэш) его файлов.
/// Отсутствующие файлы и пустые деревья в набор не входят.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustSet {
    pub files: BTreeMap<String, String>,
}

/// Набор доверенных файлов проекта `project` сейчас. Блокирующий ввод-вывод: из
/// асинхронного кода — через `spawn_blocking`.
pub fn trust_set(project: &Path) -> std::io::Result<TrustSet> {
    let mut files = BTreeMap::new();
    for rel in TRUSTED_FILES {
        if let Some(h) = entry_hash(&project.join(rel))? {
            files.insert((*rel).to_owned(), h);
        }
    }
    for rel in TRUSTED_TREES {
        let mut list = Vec::new();
        walk(&project.join(rel), "", &mut list)?;
        if !list.is_empty() {
            list.sort();
            let text = canonical_json(&serde_json::json!(list));
            files.insert(
                (*rel).to_owned(),
                blake3::hash(text.as_bytes()).to_hex().to_string(),
            );
        }
    }
    Ok(TrustSet { files })
}

/// Хэш файла или ссылки; `None`, если пути нет или это каталог.
fn entry_hash(p: &Path) -> std::io::Result<Option<String>> {
    let meta = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(p)?;
        let text = format!("symlink {}", target.to_string_lossy());
        return Ok(Some(blake3::hash(text.as_bytes()).to_hex().to_string()));
    }
    if meta.is_file() {
        return Ok(Some(blake3::hash(&std::fs::read(p)?).to_hex().to_string()));
    }
    Ok(None)
}

/// Пары (путь относительно дерева, хэш) файлов дерева `dir`; ссылки на каталоги не
/// обходятся.
fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, String)>) -> std::io::Result<()> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        return Ok(());
    }
    for item in std::fs::read_dir(dir)? {
        let item = item?;
        let name = item.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let path = item.path();
        if item.file_type()?.is_dir() {
            walk(&path, &rel, out)?;
        } else if let Some(h) = entry_hash(&path)? {
            out.push((rel, h));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Свой каталог на процесс и вызов `.tmp/zero-agent-unit/<name>-<pid>-<n>`.
    fn dir(name: &str) -> std::path::PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.tmp/zero-agent-unit");
        crate::test_unique_dir::unique_dir_in(&root, name)
    }

    #[test]
    fn trust_set_hashes_trusted_files_and_trees() {
        let p = dir("trust-set");
        assert_eq!(trust_set(&p).unwrap(), TrustSet::default());
        std::fs::write(p.join("AGENTS.md"), "a").unwrap();
        std::fs::write(p.join("README.md"), "не входит").unwrap();
        std::fs::create_dir_all(p.join(".claude/skills/x")).unwrap();
        std::fs::write(p.join(".claude/skills/x/SKILL.md"), "s").unwrap();
        let first = trust_set(&p).unwrap();
        let keys: Vec<&str> = first.files.keys().map(String::as_str).collect();
        assert_eq!(keys, [".claude/skills", "AGENTS.md"]);
        assert_eq!(
            first.files["AGENTS.md"],
            blake3::hash(b"a").to_hex().to_string()
        );
        // Изменение файла дерева меняет хэш дерева.
        std::fs::write(p.join(".claude/skills/x/SKILL.md"), "s2").unwrap();
        let second = trust_set(&p).unwrap();
        assert_ne!(
            first.files[".claude/skills"],
            second.files[".claude/skills"]
        );
        assert_eq!(first.files["AGENTS.md"], second.files["AGENTS.md"]);
    }
}
