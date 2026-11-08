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
/// запись его переиспользует.
pub(crate) fn put(
    c: &Connection,
    dir: &Path,
    data: &[u8],
    mime: Option<&str>,
) -> Result<BlobHash, StoreError> {
    let h = hash(data);
    let exists = c
        .query_row("SELECT 1 FROM blobs WHERE hash = ?1", [&h.0], |_| Ok(()))
        .optional()?
        .is_some();
    if exists {
        return Ok(h);
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

/// Пишет файл атомарно: во временный, `fsync`, переименование.
fn write_file(path: &Path, data: &[u8]) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn corrupt(h: &BlobHash, what: &str) -> StoreError {
    StoreError::Sqlite(format!("повреждённый blob {}: {what}", h.0))
}
