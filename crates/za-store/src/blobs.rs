//! Blobs, адресуемые содержимым по BLAKE3 (ARC-032).
//!
//! Данные до [`FILE_THRESHOLD`] хранятся прямо в строке таблицы `blobs` (BLOB),
//! больше — файлом `blobs/<hash>` рядом с `journal.db`, а в строке лежит путь
//! (TEXT). Запись идемпотентна: одинаковые данные дают один хеш и одну строку.
//! При чтении хеш пересчитывается, повреждённые данные дают ошибку.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, params};
use za_protocol::BlobHash;

use crate::{ProjectStore, StoreError};

/// Payload события длиннее этого порога (в байтах канонического JSON) уходит в blob.
pub const BLOB_THRESHOLD: usize = 64 * 1024;

/// Blob длиннее этого порога хранится файлом, а не в строке таблицы.
const FILE_THRESHOLD: usize = 1024 * 1024;

/// Каталог файлов blobs внутри каталога проекта.
const BLOB_DIR: &str = "blobs";

impl ProjectStore {
    /// Сохраняет данные и возвращает их хеш; повторная запись тех же данных ничего не меняет.
    pub async fn put_blob(&self, data: &[u8]) -> Result<BlobHash, StoreError> {
        let data = data.to_vec();
        let dir = self.dir();
        self.write(move |c| put(c, &dir, &data, None)).await
    }

    /// Данные blob по хешу; `None`, если такого blob нет.
    pub async fn get_blob(&self, h: &BlobHash) -> Result<Option<Vec<u8>>, StoreError> {
        let h = h.clone();
        let dir = self.dir();
        self.read(move |c| get(c, &dir, &h)).await
    }
}

/// Хеш BLAKE3 в hex.
fn hash(data: &[u8]) -> BlobHash {
    BlobHash(blake3::hash(data).to_hex().to_string())
}

/// Сохраняет blob в открытой транзакции писателя.
///
/// Файл пишется до вставки строки; если транзакция затем откатится, файл
/// останется без строки. Это безопасно: имя файла — хеш содержимого, повторная
/// запись его переиспользует. Если строка уже есть, а файла нет или его длина
/// иная (сбой питания до сброса каталога на диск, оборванная запись), файл
/// пишется заново: данные на руках, хеш тот же.
pub(crate) fn put(
    c: &Connection,
    dir: &Path,
    data: &[u8],
    mime: Option<&str>,
) -> Result<BlobHash, StoreError> {
    let h = hash(data);
    let stored = c
        .query_row(
            "SELECT typeof(path_or_data) = 'text' FROM blobs WHERE hash = ?1",
            [&h.0],
            |r| r.get::<_, bool>(0),
        )
        .optional()?;
    match stored {
        Some(true) => {
            let path = file_path(dir, &h);
            let intact =
                std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() == data.len() as u64);
            if !intact {
                tracing::warn!(hash = %h.0, "файл blob отсутствует или неполон, пишется заново");
                write_file(&path, data)?;
            }
            return Ok(h);
        }
        Some(false) => return Ok(h),
        None => {}
    }
    let size = i64::try_from(data.len()).map_err(|_| corrupt(&h, "размер"))?;
    if data.len() > FILE_THRESHOLD {
        write_file(&file_path(dir, &h), data)?;
        c.execute(
            "INSERT INTO blobs (hash, size, path_or_data, mime) VALUES (?1, ?2, ?3, ?4)",
            params![h.0, size, rel_path(&h), mime],
        )?;
    } else {
        c.execute(
            "INSERT INTO blobs (hash, size, path_or_data, mime) VALUES (?1, ?2, ?3, ?4)",
            params![h.0, size, data, mime],
        )?;
    }
    Ok(h)
}

/// Читает blob и сверяет хеш.
pub(crate) fn get(c: &Connection, dir: &Path, h: &BlobHash) -> Result<Option<Vec<u8>>, StoreError> {
    let mut stmt = c.prepare("SELECT size, path_or_data FROM blobs WHERE hash = ?1")?;
    let mut rows = stmt.query([&h.0])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let size: i64 = row.get(0)?;
    let data = match row.get_ref(1)? {
        ValueRef::Blob(b) => b.to_vec(),
        ValueRef::Text(t) => {
            // Путь всегда выводится из хеша; иное значение в строке — повреждение.
            if t != rel_path(h).as_bytes() {
                return Err(corrupt(h, "путь к файлу"));
            }
            std::fs::read(file_path(dir, h))?
        }
        _ => return Err(corrupt(h, "тип path_or_data")),
    };
    if i64::try_from(data.len()).ok() != Some(size) || hash(&data) != *h {
        return Err(corrupt(h, "содержимое не совпадает с хешем"));
    }
    Ok(Some(data))
}

/// Путь файла в строке таблицы: относительный, с `/` на любой ОС.
fn rel_path(h: &BlobHash) -> String {
    format!("{BLOB_DIR}/{}", h.0)
}

fn file_path(dir: &Path, h: &BlobHash) -> PathBuf {
    dir.join(BLOB_DIR).join(&h.0)
}

/// Пишет файл атомарно: во временный, `fsync`, переименование, `fsync` каталога.
///
/// На Unix запись в каталог (создание `blobs/`, переименование) переживает
/// отключение питания только после `fsync` самого каталога, поэтому после
/// `rename` синхронизируется `blobs/`, а при его создании — и каталог проекта.
/// На Windows каталог так не синхронизируется, а `std::fs::rename` не просит
/// `MOVEFILE_WRITE_THROUGH`: после отключения питания файл может пропасть при
/// зафиксированной строке. Это ограничение закрывает [`put`]: повторная запись
/// тех же данных восстанавливает файл, а чтение без файла даёт ошибку, не
/// подменённые данные.
fn write_file(path: &Path, data: &[u8]) -> Result<(), StoreError> {
    if let Some(parent) = path.parent()
        && !parent.is_dir()
    {
        std::fs::create_dir_all(parent)?;
        if let Some(project) = parent.parent() {
            sync_dir(project)?;
        }
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Сбрасывает на диск запись каталога (Unix); на других ОС ничего не делает.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<(), StoreError> {
    tolerate_dir_sync(File::open(dir).and_then(|f| f.sync_all()))?;
    Ok(())
}

/// Итог `fsync` каталога: `EINVAL` (`InvalidInput`) терпится с `warn`, как в
/// SQLite (`unixSync` игнорирует ошибку `fsync` каталога): часть сетевых и
/// FUSE-ФС не умеет синхронизировать каталог, и падать на этом нельзя.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn tolerate_dir_sync(r: std::io::Result<()>) -> std::io::Result<()> {
    match r {
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            tracing::warn!(error = %e, "fsync каталога не поддерживается файловой системой");
            Ok(())
        }
        r => r,
    }
}

/// Сбрасывает на диск запись каталога (Unix); на других ОС ничего не делает.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<(), StoreError> {
    Ok(())
}

fn corrupt(h: &BlobHash, what: &str) -> StoreError {
    StoreError::Corrupt(format!("blob {}: {what}", h.0))
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn einval_on_dir_sync_is_tolerated() {
        assert!(tolerate_dir_sync(Err(io::Error::from(io::ErrorKind::InvalidInput))).is_ok());
        assert!(tolerate_dir_sync(Err(io::Error::from(io::ErrorKind::PermissionDenied))).is_err());
        assert!(tolerate_dir_sync(Ok(())).is_ok());
    }
}
