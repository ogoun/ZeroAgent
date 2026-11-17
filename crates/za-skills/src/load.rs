//! Загрузка скилла по каталогу с проверкой хэша (SKL-005, D118).
//!
//! Файл перечитывается и сверяется с хэшем каталога: изменился после обнаружения —
//! [`LoadError::Changed`]. Ресурс — только из списка каталога; `..`, абсолютные пути,
//! `\` и `:` — [`LoadError::UnknownFile`] до любого обращения к ФС. Путь разрешается от
//! канонического каталога скилла, файл открывается `open_checked`. Очистка невидимых
//! символов и экранирование текста — у вызывающего (обёртка `<skill_content>`, задача 13).

use crate::discover::{SKILL_FILE, SkillMeta};
use crate::parse::parse_skill_md;
use crate::tree::{self, LINK_PREFIX, ReadFail, TRUST_LIMITS};

/// Отказ загрузки.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    /// Файл изменился (или исчез) после построения каталога.
    #[error("skill changed since the catalog was built")]
    Changed,
    /// Файла нет в списке ресурсов скилла.
    #[error("unknown file {0:?}")]
    UnknownFile(String),
    #[error("read error: {0}")]
    Io(String),
}

/// Текст скилла: `file = None` — тело `SKILL.md` после frontmatter, иначе — содержимое
/// ресурса из [`SkillMeta::files`]. Блокирующая: из асинхронного кода — `spawn_blocking`.
///
/// Скилл поставки (`dir = None`) берётся из [`crate::bundled`] по хэшу каталога: текст
/// заглушки `DiscoverInput::bundled` отсюда не виден, и такой скилл даёт
/// [`LoadError::Changed`]. Заглушка `Catalog::bundled_for_tests` (признак `test-hooks`)
/// несёт свой текст и сверяется с хэшем так же.
pub fn load(meta: &SkillMeta, file: Option<&str>) -> Result<String, LoadError> {
    let Some(file) = file else {
        return load_body(meta);
    };
    if !safe_rel(file) {
        return Err(LoadError::UnknownFile(file.to_owned()));
    }
    let (Some(dir), Some(value)) = (&meta.dir, meta.files.get(file)) else {
        return Err(LoadError::UnknownFile(file.to_owned()));
    };
    let expected = value.strip_prefix(LINK_PREFIX).unwrap_or(value);
    let bytes = read(dir, file)?;
    if blake3::hash(&bytes).to_hex().as_str() != expected {
        return Err(LoadError::Changed);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Тело `SKILL.md` скилла.
fn load_body(meta: &SkillMeta) -> Result<String, LoadError> {
    let (text, dir_name) = match &meta.dir {
        None => {
            #[cfg(feature = "test-hooks")]
            let stub = meta.stub.as_deref();
            #[cfg(not(feature = "test-hooks"))]
            let stub: Option<&str> = None;
            let text = stub
                .into_iter()
                .chain(crate::bundled().iter().map(|(_, t)| *t))
                .find(|t| blake3::hash(t.as_bytes()).to_hex().as_str() == meta.hash)
                .ok_or(LoadError::Changed)?;
            (text.to_owned(), meta.name.clone())
        }
        Some(dir) => {
            let bytes = read(dir, SKILL_FILE)?;
            if blake3::hash(&bytes).to_hex().as_str() != meta.hash {
                return Err(LoadError::Changed);
            }
            let dir_name = dir
                .file_name()
                .map_or_else(|| meta.name.clone(), |n| n.to_string_lossy().into_owned());
            (String::from_utf8_lossy(&bytes).into_owned(), dir_name)
        }
    };
    parse_skill_md(&text, &dir_name)
        .map(|p| p.body)
        .map_err(|e| LoadError::Io(e.to_string()))
}

fn read(dir: &std::path::Path, rel: &str) -> Result<Vec<u8>, LoadError> {
    tree::read_checked(dir, rel, TRUST_LIMITS.max_file).map_err(|e| match e {
        ReadFail::Missing | ReadFail::TooLarge => LoadError::Changed,
        ReadFail::Other(e) => LoadError::Io(e),
    })
}

/// Относительный путь ресурса без `..`, `.`, пустых компонентов, `\`, `:` и NUL.
fn safe_rel(file: &str) -> bool {
    !file.is_empty()
        && !file.starts_with('/')
        && !file.contains(['\\', ':', '\0'])
        && file
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}
