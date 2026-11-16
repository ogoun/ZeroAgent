//! Обход доверенных деревьев (D116): один код для набора доверия проекта
//! (`zero-agent/src/trust.rs`) и для чтения скиллов — «доверили одно, прочитали другое»
//! невозможно. Перенесён из `trust.rs` R1-C1 без изменения значений обычных деревьев:
//! хэш дерева прежний (golden-тест `tree_hash_matches_r1c1_golden`).
//!
//! Значение записи описывает ровно то, что потом прочтёт рантайм (ревью задачи 4 R1-C1,
//! I1, I2):
//! - обычный файл — BLAKE3 содержимого (hex);
//! - файл-ссылка (symlink, junction), цель которой — обычный файл внутри корня, — `link:` и
//!   BLAKE3 содержимого цели;
//! - ссылка наружу, на сетевой или device-путь, висячая ссылка, ссылка в промежуточном
//!   каталоге, имя не в UTF-8 или не проходящее `za_core::paths::classify` —
//!   [`EXTERNAL_LINK`]: такой файл не читается;
//! - файл больше [`TreeLimits::max_file`] — `too-large:<size>` без чтения содержимого
//!   (пределы обхода, preflight 8.1; R1-C2).
//!
//! Обход не заходит в каталоги-ссылки и junction ни на каком уровне: у промежуточных
//! компонентов читаются только локальные метаданные самой записи (`symlink_metadata`).
//! Цель файла-ссылки разрешается `za_core::paths::resolve` (цепочка ссылок проверяется
//! лексически до канонизации, сетевая цель — отказ без SMB-запроса), файл открывается
//! `open_checked`. Обход итеративный: глубина дерева не расходует стек. Больше
//! [`TreeLimits::max_entries`] записей — обход останавливается, дерево не читается.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use za_core::Access;
use za_core::paths::{self, Roots};
use za_protocol::canonical_json;

/// Значение записи, которую рантайм не читает: ссылка наружу, на сетевой или device-путь,
/// висячая ссылка, ссылка в пути, неразбираемое имя; у дерева — ещё и превышение
/// [`TreeLimits::max_entries`].
pub const EXTERNAL_LINK: &str = "external-link";
/// Префикс значения у ссылки на файл внутри корня.
pub const LINK_PREFIX: &str = "link:";
/// Префикс значения у файла больше [`TreeLimits::max_file`]: `too-large:<size>`.
pub const TOO_LARGE_PREFIX: &str = "too-large:";
/// Деревья скиллов проекта по старшинству (D114): они же — доверенные деревья набора
/// доверия (`zero_agent::trust::TRUSTED_TREES`).
pub const PROJECT_TREES: &[&str] = &[".zero-agent/skills", ".agents/skills", ".claude/skills"];

/// Где лежит дерево.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// Компонента нет или промежуточный компонент не каталог.
    Missing,
    /// Промежуточный компонент — ссылка или junction, или в дереве больше
    /// [`TreeLimits::max_entries`] записей: дерево не читается, его значение в наборе
    /// доверия — [`EXTERNAL_LINK`].
    Link,
    /// Путь есть (каталог — со списком, иначе без).
    Found,
}

/// Список файлов дерева: путь в дереве (через `/`) → значение (hex, `link:<hex>`,
/// [`EXTERNAL_LINK`], `too-large:<size>`). Каталоги в список не входят.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeListing {
    /// Путь дерева от корня (`.claude/skills`; пусто — сам корень).
    pub rel: String,
    pub files: BTreeMap<String, String>,
}

/// Пределы обхода.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeLimits {
    /// Файл больше — значение `too-large:<size>` без чтения.
    pub max_file: u64,
    /// Записей (файлов, каталогов, ссылок) больше — дерево не читается ([`Place::Link`]).
    pub max_entries: usize,
}

/// Пределы обхода доверия проекта (D116): 1 МиБ на файл, 10 000 записей на дерево.
/// Хэш меняется только у деревьев сверх них (миграция, CHANGELOG).
pub const TRUST_LIMITS: TreeLimits = TreeLimits {
    max_file: 1 << 20,
    max_entries: 10_000,
};

/// Список дерева `rel` от корня `roots.project` (D116). Блокирующая. Пустой `rel` — сам
/// корень.
pub fn tree_listing(
    roots: &Roots,
    rel: &str,
    limits: &TreeLimits,
) -> io::Result<(Place, Option<TreeListing>)> {
    listing_with(roots, rel, limits, &[])
}

/// [`tree_listing`], не заходящий в каталоги с именами из `skip` (обнаружение у `user` и
/// `extra`: `.git`, `node_modules` …). У доверия `skip` пуст: значения прежние.
pub(crate) fn listing_with(
    roots: &Roots,
    rel: &str,
    limits: &TreeLimits,
    skip: &[&str],
) -> io::Result<(Place, Option<TreeListing>)> {
    let (dir, meta) = if rel.is_empty() {
        let meta = std::fs::symlink_metadata(&roots.project)?;
        (roots.project.clone(), meta)
    } else {
        match locate(&roots.project, rel)? {
            Located::Missing => return Ok((Place::Missing, None)),
            Located::Link => return Ok((Place::Link, None)),
            Located::Found(p, m) => (p, m),
        }
    };
    if !meta.is_dir() {
        return Ok((Place::Found, None));
    }
    let mut files = BTreeMap::new();
    if !walk(roots, &dir, rel, limits, skip, &mut files)? {
        return Ok((Place::Link, None));
    }
    Ok((
        Place::Found,
        Some(TreeListing {
            rel: rel.to_owned(),
            files,
        }),
    ))
}

/// Хэш дерева (алгоритм `TrustSet` R1-C1): BLAKE3 канонического JSON отсортированного
/// списка пар `[путь в дереве, значение]`.
pub fn tree_hash(l: &TreeListing) -> String {
    let list: Vec<(&str, &str)> = l
        .files
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let text = canonical_json(&serde_json::json!(list));
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

/// Значение файла `rel` от корня проекта без пределов размера (доверенные файлы
/// `AGENTS.md`, `CLAUDE.md`, `.zero-agent/config.toml` R1-C1); `None` — файла нет или это
/// каталог. Блокирующая.
pub fn file_value(roots: &Roots, rel: &str) -> io::Result<Option<String>> {
    match locate(&roots.project, rel)? {
        Located::Missing => Ok(None),
        Located::Link => Ok(Some(EXTERNAL_LINK.to_owned())),
        Located::Found(_, meta) => entry_value(roots, rel, &meta, None),
    }
}

/// Где лежит путь: с локальными метаданными последнего компонента (внутри обхода).
enum Located {
    Missing,
    Link,
    Found(PathBuf, std::fs::Metadata),
}

/// Проходит `rel` по компонентам от корня, читая только метаданные самих записей.
fn locate(project: &Path, rel: &str) -> io::Result<Located> {
    let parts: Vec<&str> = rel.split('/').collect();
    let mut p = project.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        p.push(part);
        let meta = match std::fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(e) if is_missing(&e) => return Ok(Located::Missing),
            Err(e) => return Err(e),
        };
        if i + 1 == parts.len() {
            return Ok(Located::Found(p, meta));
        }
        if meta.file_type().is_symlink() {
            return Ok(Located::Link);
        }
        if !meta.is_dir() {
            return Ok(Located::Missing);
        }
    }
    Ok(Located::Missing)
}

fn too_large(size: u64) -> String {
    format!("{TOO_LARGE_PREFIX}{size}")
}

/// Значение записи с путём `rel` от корня и локальными метаданными `meta`; `max_file` —
/// предел размера (`None` — без предела).
fn entry_value(
    roots: &Roots,
    rel: &str,
    meta: &std::fs::Metadata,
    max_file: Option<u64>,
) -> io::Result<Option<String>> {
    let link = meta.file_type().is_symlink();
    if !link && !meta.is_file() {
        return Ok(None);
    }
    // Обычный файл сверх предела не открывается вовсе.
    if let Some(max) = max_file
        && !link
        && meta.len() > max
    {
        return Ok(Some(too_large(meta.len())));
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
    match max_file {
        None => {
            h.update_reader(file)?;
        }
        Some(max) => {
            let size = file.metadata()?.len();
            if size > max {
                return Ok(Some(too_large(size)));
            }
            // Файл мог вырасти после проверки: читается не больше предела и ещё байта.
            let read = io::copy(&mut (&file).take(max + 1), &mut h)?;
            if read > max {
                return Ok(Some(too_large(file.metadata()?.len().max(read))));
            }
        }
    }
    let hex = h.finalize().to_hex().to_string();
    Ok(Some(if link {
        format!("{LINK_PREFIX}{hex}")
    } else {
        hex
    }))
}

/// Записи дерева `root` (путь `rel` от корня) в `out`. `false` — записей больше
/// `limits.max_entries` (обход остановлен). Каталоги-ссылки и junction не обходятся: у
/// них значение [`EXTERNAL_LINK`]. Каталоги с именами из `skip` не обходятся.
///
/// Два прохода: сначала перечень записей (только `read_dir` и `symlink_metadata`), и лишь
/// если предел не превышен — значения файлов. Дерево сверх предела не хэшируется вовсе.
fn walk(
    roots: &Roots,
    root: &Path,
    rel: &str,
    limits: &TreeLimits,
    skip: &[&str],
    out: &mut BTreeMap<String, String>,
) -> io::Result<bool> {
    let mut found = Vec::new();
    if !list_entries(root, limits, skip, out, &mut found)? {
        return Ok(false);
    }
    for (in_tree, meta) in found {
        let from_root = join(rel, &in_tree);
        let v = entry_value(roots, &from_root, &meta, Some(limits.max_file))?;
        if meta.file_type().is_symlink() {
            // Ссылка на файл внутри корня хэшируется по цели; на каталог — не обходится.
            out.insert(in_tree, v.unwrap_or_else(|| EXTERNAL_LINK.to_owned()));
        } else if let Some(v) = v {
            out.insert(in_tree, v);
        }
    }
    Ok(true)
}

/// Перечень записей дерева: не-каталоги с метаданными — в `found`, имена не в UTF-8 — сразу
/// в `out` со значением [`EXTERNAL_LINK`]. `false` — записей больше предела.
fn list_entries(
    root: &Path,
    limits: &TreeLimits,
    skip: &[&str],
    out: &mut BTreeMap<String, String>,
    found: &mut Vec<(String, std::fs::Metadata)>,
) -> io::Result<bool> {
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    let mut entries = 0usize;
    while let Some((dir, prefix)) = stack.pop() {
        let items = match std::fs::read_dir(&dir) {
            Ok(it) => it,
            Err(e) if is_missing(&e) => continue,
            Err(e) => return Err(e),
        };
        for item in items {
            let item = item?;
            entries += 1;
            if entries > limits.max_entries {
                return Ok(false);
            }
            let name = item.file_name();
            let Some(name) = name.to_str().map(str::to_owned) else {
                // Имя не в UTF-8: рантайм его не прочтёт.
                out.insert(
                    join(&prefix, &item.file_name().to_string_lossy()),
                    EXTERNAL_LINK.to_owned(),
                );
                continue;
            };
            let in_tree = join(&prefix, &name);
            let path = item.path();
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) if is_missing(&e) => continue,
                Err(e) => return Err(e),
            };
            if meta.is_dir() && !meta.file_type().is_symlink() {
                if !skip.contains(&name.as_str()) {
                    stack.push((path, in_tree));
                }
            } else {
                found.push((in_tree, meta));
            }
        }
    }
    Ok(true)
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

/// Компонента нет: `NotFound` или «не каталог» (`ENOTDIR`).
pub(crate) fn is_missing(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Почему файл скилла не прочитан.
#[derive(Debug)]
pub(crate) enum ReadFail {
    /// Файла (или каталога скилла) нет.
    Missing,
    /// Больше предела.
    TooLarge,
    /// Вне каталога скилла, ссылка, ошибка ввода-вывода.
    Other(String),
}

/// Байты файла `rel` каталога скилла `dir` (не больше `max`). Путь разрешается от
/// канонического каталога скилла: ссылка за его пределы — отказ; файл открывается
/// `open_checked` (без следования ссылке на последнем компоненте, со сверкой
/// дескриптора). Блокирующая.
pub(crate) fn read_checked(dir: &Path, rel: &str, max: u64) -> Result<Vec<u8>, ReadFail> {
    let roots = Roots::canonical(dir, dir).map_err(|e| {
        if is_missing(&e) {
            ReadFail::Missing
        } else {
            ReadFail::Other(e.to_string())
        }
    })?;
    let lex = paths::classify(&roots.project, rel).map_err(|e| ReadFail::Other(e.to_string()))?;
    let resolved =
        paths::resolve(&roots, &lex, Access::Read).map_err(|e| ReadFail::Other(e.to_string()))?;
    if !resolved.inside {
        return Err(ReadFail::Other("outside the skill directory".into()));
    }
    let file = paths::open_checked(&resolved, false).map_err(|e| {
        if is_missing(&e) {
            ReadFail::Missing
        } else {
            ReadFail::Other(e.to_string())
        }
    })?;
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ReadFail::Other(e.to_string()))?;
    if bytes.len() as u64 > max {
        return Err(ReadFail::TooLarge);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.tmp/za-skills-unit")
            .join(format!("{name}-{}", std::process::id()));
        if d.exists() {
            std::fs::remove_dir_all(&d).unwrap();
        }
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Каталоги из `skip` не обходятся вовсе: их записи не идут в счёт предела.
    #[test]
    fn skipped_dirs_are_not_walked() {
        let d = dir("skip-walk");
        for i in 0..5 {
            std::fs::create_dir_all(d.join(format!("node_modules/m{i}"))).unwrap();
        }
        std::fs::write(d.join("ok.md"), "x").unwrap();
        let roots = Roots::canonical(&d, &d).unwrap();
        let limits = TreeLimits {
            max_file: 1 << 20,
            max_entries: 2,
        };
        let (place, l) = listing_with(&roots, "", &limits, &["node_modules"]).unwrap();
        assert_eq!(place, Place::Found);
        assert_eq!(l.unwrap().files.len(), 1);
        // Без пропуска те же каталоги превышают предел.
        assert_eq!(
            listing_with(&roots, "", &limits, &[]).unwrap(),
            (Place::Link, None)
        );
    }

    /// Глубокая вложенность не расходует стек: обход итеративный.
    #[test]
    fn deep_tree_is_walked_iteratively() {
        let d = dir("deep-walk");
        let mut p = d.clone();
        // Короткие имена: путь остаётся в пределах MAX_PATH Windows.
        for _ in 0..60 {
            p.push("d");
        }
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("f"), "x").unwrap();
        let roots = Roots::canonical(&d, &d).unwrap();
        let (_, l) = tree_listing(&roots, "", &TRUST_LIMITS).unwrap();
        let l = l.unwrap();
        assert_eq!(l.files.len(), 1);
        assert!(l.files.keys().next().unwrap().ends_with("d/f"));
    }
}
