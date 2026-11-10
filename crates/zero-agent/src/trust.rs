//! Доверие к проекту (SEC-030, R1-C1 D83; решение заказчика (б)): набор доверенных файлов
//! проекта с хэшами их содержимого.
//!
//! Набор пишется в запись аудита `trust.granted` в момент ответа на прерывание доверия
//! (`answer --interrupt trust-… --approve`, D91) и сверяется с набором из payload
//! прерывания. Сравнение с набором на старте `run`, команда `trust` и чтение `AGENTS.md`
//! у доверенного проекта добавляет задача 13 плана.
//!
//! Хэш описывает ровно то, что потом прочтёт рантайм (ревью задачи 4, I1, I2):
//! - обычный файл — BLAKE3 содержимого (потоково, блоками), значение — hex;
//! - файл-ссылка (symlink, junction), цель которой — обычный файл внутри проекта, —
//!   `link:` и BLAKE3 содержимого цели: подмена содержимого цели меняет набор, а замена
//!   файла ссылкой на такое же содержимое — тоже (другой домен хэша);
//! - ссылка наружу, на сетевой или device-путь, висячая ссылка, ссылка в промежуточном
//!   каталоге (`.claude` — junction и т. п.), имя, которое не проходит
//!   `za_core::paths::classify`, — значение [`EXTERNAL_LINK`]. Загрузчик такой файл не
//!   читает, как отсутствующий; само появление метки меняет набор.
//!
//! Обход не заходит в каталоги-ссылки и junction ни на каком уровне: у промежуточных
//! компонентов читаются только локальные метаданные самой записи (`symlink_metadata`),
//! без перехода по ссылке. Цель файла-ссылки разрешается `za_core::paths::resolve`:
//! цепочка ссылок проверяется лексически до любой канонизации, сетевая цель — отказ без
//! SMB-запроса (Ruling 7.4, C1 задачи 2). Файл открывается `open_checked` (без следования
//! ссылке и со сверкой дескриптора). Компонента нет или он не каталог (`ENOTDIR`) —
//! файла нет, а не ошибка.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use za_core::Access;
use za_core::paths::{self, Roots};
use za_protocol::canonical_json;

/// Доверенные файлы в корне проекта.
pub const TRUSTED_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".zero-agent/config.toml"];
/// Доверенные деревья (скиллы).
pub const TRUSTED_TREES: &[&str] = &[".zero-agent/skills", ".agents/skills", ".claude/skills"];
/// Значение набора у файла, который рантайм не читает: ссылка наружу, на сетевой или
/// device-путь, висячая ссылка, ссылка в пути, неразбираемое имя.
pub const EXTERNAL_LINK: &str = "external-link";
/// Префикс значения у ссылки на файл внутри проекта.
pub const LINK_PREFIX: &str = "link:";

/// Набор доверенных файлов: путь относительно проекта (через `/`) → значение (hex BLAKE3
/// содержимого, `link:<hex>` или [`EXTERNAL_LINK`]). У дерева один элемент — BLAKE3
/// канонического JSON отсортированного списка пар (путь в дереве, значение). Отсутствующие
/// файлы и пустые деревья в набор не входят.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustSet {
    pub files: BTreeMap<String, String>,
}

impl TrustSet {
    /// Хэш набора — BLAKE3 канонического JSON (поле `set_hash` payload прерывания `Trust`).
    pub fn hash(&self) -> String {
        let v = serde_json::json!({ "files": self.files });
        blake3::hash(canonical_json(&v).as_bytes())
            .to_hex()
            .to_string()
    }
}

/// Набор доверенных файлов проекта `project` сейчас. Блокирующий ввод-вывод: из
/// асинхронного кода — через `spawn_blocking`. Проект за сетевой ссылкой или на сетевом
/// диске — ошибка (`Roots::canonical`).
pub fn trust_set(project: &Path) -> io::Result<TrustSet> {
    // `ZA_HOME` нужен `Roots` только для защиты записи; набор только читает.
    let roots = Roots::canonical(project, project)?;
    let mut files = BTreeMap::new();
    for rel in TRUSTED_FILES {
        if let Some(v) = file_value(&roots, rel)? {
            files.insert((*rel).to_owned(), v);
        }
    }
    for rel in TRUSTED_TREES {
        match locate(&roots.project, rel)? {
            Place::Missing => {}
            Place::Link => {
                files.insert((*rel).to_owned(), EXTERNAL_LINK.to_owned());
            }
            Place::Found(dir, meta) => {
                if !meta.is_dir() {
                    continue;
                }
                let mut list = Vec::new();
                walk(&roots, &dir, rel, "", &mut list)?;
                if !list.is_empty() {
                    list.sort();
                    let text = canonical_json(&serde_json::json!(list));
                    files.insert(
                        (*rel).to_owned(),
                        blake3::hash(text.as_bytes()).to_hex().to_string(),
                    );
                }
            }
        }
    }
    Ok(TrustSet { files })
}

/// Где лежит доверенный путь `rel`.
enum Place {
    /// Компонента нет или промежуточный компонент не каталог.
    Missing,
    /// Промежуточный компонент — ссылка или junction: дальше не идём.
    Link,
    /// Путь и локальные метаданные последнего компонента (без перехода по ссылке).
    Found(PathBuf, std::fs::Metadata),
}

/// Проходит `rel` по компонентам от корня проекта, читая только метаданные самих записей.
fn locate(project: &Path, rel: &str) -> io::Result<Place> {
    let parts: Vec<&str> = rel.split('/').collect();
    let mut p = project.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        p.push(part);
        let meta = match std::fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(e) if is_missing(&e) => return Ok(Place::Missing),
            Err(e) => return Err(e),
        };
        if i + 1 == parts.len() {
            return Ok(Place::Found(p, meta));
        }
        if meta.file_type().is_symlink() {
            return Ok(Place::Link);
        }
        if !meta.is_dir() {
            return Ok(Place::Missing);
        }
    }
    Ok(Place::Missing)
}

/// Значение доверенного файла `rel`; `None` — файла нет (или это каталог).
fn file_value(roots: &Roots, rel: &str) -> io::Result<Option<String>> {
    match locate(&roots.project, rel)? {
        Place::Missing => Ok(None),
        Place::Link => Ok(Some(EXTERNAL_LINK.to_owned())),
        Place::Found(_, meta) => entry_value(roots, rel, &meta),
    }
}

/// Значение записи с относительным путём `rel` и локальными метаданными `meta`.
fn entry_value(roots: &Roots, rel: &str, meta: &std::fs::Metadata) -> io::Result<Option<String>> {
    let link = meta.file_type().is_symlink();
    if !link && !meta.is_file() {
        return Ok(None);
    }
    let Ok(lex) = paths::classify(&roots.project, rel) else {
        return Ok(Some(EXTERNAL_LINK.to_owned()));
    };
    // Цепочка ссылок проверяется лексически до канонизации: сетевая цель — отказ без SMB.
    let Ok(resolved) = paths::resolve(roots, &lex, Access::Read) else {
        return Ok(Some(EXTERNAL_LINK.to_owned()));
    };
    if !resolved.inside {
        return Ok(Some(EXTERNAL_LINK.to_owned()));
    }
    let file = match paths::open_checked(&resolved, false) {
        Ok(f) => f,
        // Висячая ссылка — метка; исчезнувший обычный файл — нет файла.
        Err(e) if is_missing(&e) => {
            return Ok(link.then(|| EXTERNAL_LINK.to_owned()));
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Err(e),
        // Не обычный файл, ссылка, подмена между разрешением и открытием: не читается.
        Err(_) => {
            return Ok(link.then(|| EXTERNAL_LINK.to_owned()));
        }
    };
    let mut h = blake3::Hasher::new();
    h.update_reader(file)?;
    let hex = h.finalize().to_hex().to_string();
    Ok(Some(if link {
        format!("{LINK_PREFIX}{hex}")
    } else {
        hex
    }))
}

/// Пары (путь в дереве, значение) записей каталога `dir` (путь `rel` от проекта).
/// Каталоги-ссылки и junction не обходятся: у них значение [`EXTERNAL_LINK`].
fn walk(
    roots: &Roots,
    dir: &Path,
    rel: &str,
    prefix: &str,
    out: &mut Vec<(String, String)>,
) -> io::Result<()> {
    let items = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) if is_missing(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    for item in items {
        let item = item?;
        let name = item.file_name();
        let Some(name) = name.to_str().map(str::to_owned) else {
            // Имя не в UTF-8: рантайм его не прочтёт.
            out.push((
                join(prefix, &item.file_name().to_string_lossy()),
                EXTERNAL_LINK.to_owned(),
            ));
            continue;
        };
        let in_tree = join(prefix, &name);
        let path = item.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if is_missing(&e) => continue,
            Err(e) => return Err(e),
        };
        if meta.file_type().is_symlink() {
            // Ссылка на файл внутри проекта хэшируется по цели; на каталог — не обходится.
            let v = entry_value(roots, &format!("{rel}/{in_tree}"), &meta)?
                .unwrap_or_else(|| EXTERNAL_LINK.to_owned());
            out.push((in_tree, v));
        } else if meta.is_dir() {
            walk(roots, &path, rel, &in_tree, out)?;
        } else if let Some(v) = entry_value(roots, &format!("{rel}/{in_tree}"), &meta)? {
            out.push((in_tree, v));
        }
    }
    Ok(())
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

/// Компонента нет: `NotFound` или «не каталог» (`ENOTDIR`).
fn is_missing(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Свой каталог на процесс и вызов `.tmp/zero-agent-unit/<name>-<pid>-<n>`.
    fn dir(name: &str) -> PathBuf {
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
        assert_ne!(first.hash(), second.hash());
    }

    /// `ENOTDIR`: `.zero-agent` — файл, а не каталог; это «нет файла», не ошибка.
    #[test]
    fn file_in_place_of_directory_is_missing() {
        let p = dir("trust-enotdir");
        std::fs::write(p.join(".zero-agent"), "файл").unwrap();
        std::fs::write(p.join(".claude"), "файл").unwrap();
        assert_eq!(trust_set(&p).unwrap(), TrustSet::default());
    }

    /// Большой файл хэшируется потоково и совпадает с хэшем содержимого.
    #[test]
    fn large_file_is_hashed_by_content() {
        let p = dir("trust-large");
        let data = vec![7u8; 3 * 1024 * 1024 + 11];
        std::fs::write(p.join("CLAUDE.md"), &data).unwrap();
        let s = trust_set(&p).unwrap();
        assert_eq!(
            s.files["CLAUDE.md"],
            blake3::hash(&data).to_hex().to_string()
        );
    }
}
