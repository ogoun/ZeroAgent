//! Хранилище проекта zero-agent: журнал событий в SQLite (`journal.db`).
//!
//! Один поток-писатель с group commit и пул из четырёх читателей (ARC-030),
//! WAL с настраиваемой надёжностью фиксации [`Durability`] (CORE-005), блокировка `journal.lock` от второго
//! процесса, миграции по `PRAGMA user_version`.

mod events;
mod migrate;
mod reader;
mod writer;

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::{Connection, ErrorCode};
use tokio::sync::oneshot;

use crate::reader::ReadJob;
use crate::writer::JobSender;

/// Имя файла журнала в каталоге проекта.
const DB_FILE: &str = "journal.db";
/// Имя файла межпроцессной блокировки.
const LOCK_FILE: &str = "journal.lock";
/// Минимальная версия SQLite (ARC-031: исправление WAL-reset bug).
const MIN_SQLITE_VERSION: i32 = 3_051_003;
/// Ожидание занятой БД, например при чекпоинте WAL.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Ошибки хранилища.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// Журнал уже открыт другим процессом или экземпляром.
    #[error("журнал проекта уже открыт другим процессом")]
    Locked,
    /// Нет места: диск заполнен или достигнут лимит размера БД.
    #[error("нет места для записи в журнал")]
    Full,
    /// Встроенная SQLite старше требуемой.
    #[error("SQLite {0} старше требуемой 3.51.3")]
    SqliteTooOld(String),
    /// Прочая ошибка SQLite или неверные данные в БД.
    #[error("ошибка SQLite: {0}")]
    Sqlite(String),
    /// Ошибка файловой системы.
    #[error("ошибка ввода-вывода: {0}")]
    Io(String),
    /// Потоки хранилища остановлены.
    #[error("хранилище закрыто")]
    Closed,
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        match e.sqlite_error_code() {
            Some(ErrorCode::DiskFull) => Self::Full,
            _ => Self::Sqlite(e.to_string()),
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// Надёжность фиксации транзакций писателя (`PRAGMA synchronous` в режиме WAL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Durability {
    /// `NORMAL`: зафиксированное переживает падение и `kill -9` процесса (NFR-08),
    /// но последние транзакции могут пропасть при отключении питания или сбое ОС.
    /// Одиночная запись ~0,2 мс против ~32 мс у `Full` (замер 2026-11-08, NFR-02).
    #[default]
    Normal,
    /// `FULL`: каждая транзакция переживает и отключение питания.
    Full,
}

impl Durability {
    /// Значение для `PRAGMA synchronous`.
    pub(crate) fn pragma(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Full => "FULL",
        }
    }
}

/// Параметры открытия хранилища. Создаются через `Default` и правку полей:
/// новые поля не ломают вызывающих.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct StoreOptions {
    /// Лимит `PRAGMA max_page_count` для писателя; в тестах имитирует заполненный диск.
    pub max_page_count: Option<u64>,
    /// Надёжность фиксации по умолчанию; [`ProjectStore::append_durable`] всегда `Full`.
    pub durability: Durability,
}

/// Хранилище проекта. Клонирование дешёвое: все клоны делят потоки и соединения.
#[derive(Clone)]
pub struct ProjectStore {
    inner: Arc<Inner>,
}

struct Inner {
    writer: Option<JobSender>,
    readers: Option<mpsc::Sender<ReadJob>>,
    threads: Vec<JoinHandle<()>>,
    sqlite_version: String,
    /// Держит блокировку, пока живы потоки; снимается последним.
    _lock: File,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Закрытие очередей завершает потоки; ждём их, чтобы соединения
        // закрылись раньше, чем снимется блокировка.
        self.writer.take();
        self.readers.take();
        for h in self.threads.drain(..) {
            if h.join().is_err() {
                tracing::error!("поток хранилища завершился паникой");
            }
        }
    }
}

impl std::fmt::Debug for ProjectStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectStore")
            .field("sqlite_version", &self.inner.sqlite_version)
            .finish_non_exhaustive()
    }
}

impl ProjectStore {
    /// Открывает журнал в каталоге `dir` (создаёт каталог при необходимости).
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        Self::open_with(dir, StoreOptions::default())
    }

    /// Открывает журнал с параметрами.
    pub fn open_with(dir: &Path, opts: StoreOptions) -> Result<Self, StoreError> {
        if rusqlite::version_number() < MIN_SQLITE_VERSION {
            return Err(StoreError::SqliteTooOld(rusqlite::version().to_owned()));
        }
        std::fs::create_dir_all(dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(LOCK_FILE))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(StoreError::Locked),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }

        let path = dir.join(DB_FILE);
        let mut conn = Connection::open(&path)?;
        writer::configure(&conn, opts.durability)?;
        migrate::migrate(&mut conn)?;
        if let Some(n) = opts.max_page_count {
            let n = i64::try_from(n).unwrap_or(i64::MAX);
            conn.pragma_update(None, "max_page_count", n)?;
        }

        let mut threads = Vec::new();
        let (readers, reader_threads) = reader::spawn(&path)?;
        threads.extend(reader_threads);
        let (writer, writer_thread) = writer::spawn(conn, opts.durability)?;
        threads.push(writer_thread);

        tracing::info!(path = %path.display(), sqlite = rusqlite::version(), "журнал открыт");
        Ok(Self {
            inner: Arc::new(Inner {
                writer: Some(writer),
                readers: Some(readers),
                threads,
                sqlite_version: rusqlite::version().to_owned(),
                _lock: lock,
            }),
        })
    }

    /// Версия встроенной SQLite, например `3.54.1`.
    pub fn sqlite_version(&self) -> String {
        self.inner.sqlite_version.clone()
    }

    /// Выполняет `f` в транзакции писателя; ответ приходит после COMMIT
    /// с надёжностью, заданной [`StoreOptions::durability`].
    pub(crate) async fn write<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    {
        self.submit(f, false).await
    }

    /// Как [`Self::write`], но транзакция фиксируется с `synchronous=FULL`
    /// независимо от настройки: результат переживает отключение питания.
    pub(crate) async fn write_durable<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    {
        self.submit(f, true).await
    }

    async fn submit<T, F>(&self, f: F, durable: bool) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job = writer::job(f, tx, durable);
        self.inner
            .writer
            .as_ref()
            .ok_or(StoreError::Closed)?
            .send(job)
            .map_err(|_| StoreError::Closed)?;
        rx.await.map_err(|_| StoreError::Closed)?
    }

    /// Выполняет `f` на одном из соединений-читателей.
    pub(crate) async fn read<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job: ReadJob = Box::new(move |conn| {
            let _ = tx.send(f(conn));
        });
        self.inner
            .readers
            .as_ref()
            .ok_or(StoreError::Closed)?
            .send(job)
            .map_err(|_| StoreError::Closed)?;
        rx.await.map_err(|_| StoreError::Closed)?
    }
}
