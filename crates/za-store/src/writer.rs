//! Поток-писатель: единственное соединение на запись, очередь команд, group commit.
//!
//! Писатель берёт первую команду с ожиданием, затем без ожидания всё накопленное
//! (не больше [`MAX_BATCH`]) и выполняет пачку в одной транзакции `BEGIN IMMEDIATE`.
//! Каждая команда работает в своей точке сохранения: ошибка одной откатывает только
//! её. Ответы уходят после COMMIT. Гарантия `Ok` зависит от [`Durability`]: при
//! `Normal` запись переживает падение процесса, но последние транзакции могут
//! пропасть при отключении питания; при `Full` переживает и отключение питания.
//! Пачка, где есть хотя бы одна команда «durable», целиком фиксируется с `FULL`.
//!
//! Команды аудита (`append_audit`, `resolve_with_audit`, R1-C1 D91) — тоже замыкания
//! этой очереди: последний `hash` цепочки читается в транзакции пачки, поэтому команды
//! одной пачки продолжают цепочку друг за другом, а откат команды (точка сохранения)
//! убирает и её звено.
//!
//! Рост WAL (ARC-035): авточекпоинт SQLite переносит страницы в БД, но файл WAL
//! не обрезает, а пока читатель держит снимок, WAL не может начаться заново и
//! растёт. Поэтому писатель каждые [`WAL_CHECK_EVERY`] пачек смотрит размер WAL
//! и, если он больше [`WAL_TRUNCATE_THRESHOLD`], делает
//! `wal_checkpoint(TRUNCATE)` без ожидания читателей. Занятый читателями
//! чекпоинт пропускается до следующей проверки. `journal_size_limit`
//! ([`JOURNAL_SIZE_LIMIT`]) ограничивает файл и при обычном перезапуске WAL.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::Connection;
use tokio::sync::oneshot;
use tracing::Level;

use crate::{BUSY_TIMEOUT, Durability, StoreError};

/// Очередь команд писателю.
pub(crate) type JobSender = mpsc::Sender<Box<dyn Job>>;

/// Наибольшее число команд в одной транзакции.
pub(crate) const MAX_BATCH: usize = 256;

/// `PRAGMA journal_size_limit`: до скольких байт SQLite обрезает WAL при его
/// перезапуске (64 МиБ).
pub(crate) const JOURNAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

/// Размер WAL в байтах, после которого писатель делает
/// `wal_checkpoint(TRUNCATE)` (8 МиБ, вдвое больше авточекпоинта SQLite по
/// умолчанию — 1000 страниц по 4 КиБ).
pub(crate) const WAL_TRUNCATE_THRESHOLD: u64 = 8 * 1024 * 1024;

/// Через сколько зафиксированных пачек писатель проверяет размер WAL; проверка —
/// один `stat` файла.
pub(crate) const WAL_CHECK_EVERY: u32 = 16;

/// Команда писателю.
pub(crate) trait Job: Send {
    /// Выполняет команду в открытой транзакции; возвращает её ошибку, если была.
    fn run(&mut self, conn: &Connection) -> Option<StoreError>;
    /// Требует ли команда фиксации с `synchronous=FULL`.
    fn durable(&self) -> bool;
    /// Отправляет ответ по итогам транзакции.
    fn finish(self: Box<Self>, commit: Result<(), StoreError>);
}

struct TypedJob<T, F> {
    f: Option<F>,
    result: Option<Result<T, StoreError>>,
    reply: oneshot::Sender<Result<T, StoreError>>,
    durable: bool,
}

impl<T, F> Job for TypedJob<T, F>
where
    T: Send,
    F: FnOnce(&Connection) -> Result<T, StoreError> + Send,
{
    fn run(&mut self, conn: &Connection) -> Option<StoreError> {
        let f = self.f.take()?;
        let r = in_savepoint(conn, f);
        let err = r.as_ref().err().cloned();
        self.result = Some(r);
        err
    }

    fn durable(&self) -> bool {
        self.durable
    }

    fn finish(self: Box<Self>, commit: Result<(), StoreError>) {
        let reply = match commit {
            Err(e) => Err(e),
            Ok(()) => self.result.unwrap_or(Err(StoreError::Closed)),
        };
        // Получатель мог перестать ждать; это не ошибка писателя.
        let _ = self.reply.send(reply);
    }
}

/// Упаковывает замыкание в команду писателю; `durable` требует фиксации с `FULL`.
pub(crate) fn job<T, F>(
    f: F,
    reply: oneshot::Sender<Result<T, StoreError>>,
    durable: bool,
) -> Box<dyn Job>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
{
    Box::new(TypedJob {
        f: Some(f),
        result: None,
        reply,
        durable,
    })
}

/// Настройки соединения писателя (CORE-005).
pub(crate) fn configure(conn: &Connection, durability: Durability) -> Result<(), StoreError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Sqlite(format!(
            "не удалось включить WAL, режим {mode}"
        )));
    }
    conn.pragma_update(None, "synchronous", durability.pragma())?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_size_limit", JOURNAL_SIZE_LIMIT)?;
    Ok(())
}

/// Запускает поток-писатель на готовом соединении; `wal` — путь файла WAL.
pub(crate) fn spawn(
    conn: Connection,
    durability: Durability,
    wal: PathBuf,
) -> Result<(JobSender, JoinHandle<()>), StoreError> {
    let (tx, rx) = mpsc::channel::<Box<dyn Job>>();
    let handle = std::thread::Builder::new()
        .name("za-store-writer".into())
        .spawn(move || run(&conn, &rx, durability, &wal))?;
    Ok((tx, handle))
}

fn run(conn: &Connection, rx: &mpsc::Receiver<Box<dyn Job>>, durability: Durability, wal: &Path) {
    let mut since_check: u32 = 0;
    let mut busy = BusyCheckpoint::default();
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(j) => batch.push(j),
                Err(_) => break,
            }
        }
        let commit = commit_with(conn, &mut batch, durability);
        if let Err(e) = &commit {
            tracing::warn!(error = %e, jobs = batch.len(), "транзакция писателя не зафиксирована");
        }
        for j in batch {
            j.finish(commit.clone());
        }
        since_check += 1;
        if since_check >= WAL_CHECK_EVERY {
            since_check = 0;
            limit_wal(conn, wal, &mut busy);
        }
    }
    tracing::debug!("поток-писатель завершён");
}

/// Серия подряд занятых чекпоинтов WAL; успешный чекпоинт её обнуляет.
#[derive(Debug, Default)]
struct BusyCheckpoint {
    /// Сколько проверок подряд чекпоинт был занят читателем.
    streak: u32,
    /// Было ли в этой серии предупреждение о WAL больше [`JOURNAL_SIZE_LIMIT`].
    warned_over_limit: bool,
}

/// Уровень лога для занятого чекпоинта: `warn` в начале серии и ещё раз, когда
/// WAL впервые в серии перерос [`JOURNAL_SIZE_LIMIT`]; остальное — `debug`.
/// Так постоянный читатель (SSE-клиент) не засоряет stderr каждые
/// [`WAL_CHECK_EVERY`] пачек.
pub(crate) fn busy_log_level(streak: u32, wal_size: u64, warned_over_limit: bool) -> Level {
    let over_limit = wal_size > JOURNAL_SIZE_LIMIT.unsigned_abs();
    if streak <= 1 || (over_limit && !warned_over_limit) {
        Level::WARN
    } else {
        Level::DEBUG
    }
}

/// Обрезает WAL чекпоинтом `TRUNCATE`, если файл больше
/// [`WAL_TRUNCATE_THRESHOLD`]. Читателей не ждёт: на время чекпоинта
/// `busy_timeout` снят, занятый чекпоинт откладывается до следующей проверки,
/// а уровень его лога выбирает [`busy_log_level`].
fn limit_wal(conn: &Connection, wal: &Path, busy: &mut BusyCheckpoint) {
    let size = std::fs::metadata(wal).map_or(0, |m| m.len());
    if size <= WAL_TRUNCATE_THRESHOLD {
        return;
    }
    if let Err(e) = conn.busy_timeout(Duration::ZERO) {
        tracing::warn!(error = %e, "чекпоинт WAL пропущен: не снят busy_timeout");
        return;
    }
    let r = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        r.get::<_, i64>(0)
    });
    match r {
        Ok(0) => {
            tracing::debug!(size, "WAL обрезан чекпоинтом");
            *busy = BusyCheckpoint::default();
        }
        Ok(_) => {
            busy.streak = busy.streak.saturating_add(1);
            let streak = busy.streak;
            if busy_log_level(streak, size, busy.warned_over_limit) == Level::WARN {
                tracing::warn!(
                    size,
                    streak,
                    "чекпоинт WAL занят долгим читателем, WAL продолжает расти"
                );
            } else {
                tracing::debug!(
                    size,
                    streak,
                    "чекпоинт WAL занят долгим читателем, WAL продолжает расти"
                );
            }
            if size > JOURNAL_SIZE_LIMIT.unsigned_abs() {
                busy.warned_over_limit = true;
            }
        }
        Err(e) => tracing::warn!(size, error = %e, "чекпоинт WAL не выполнен"),
    }
    if let Err(e) = conn.busy_timeout(BUSY_TIMEOUT) {
        tracing::error!(error = %e, "не удалось вернуть busy_timeout писателя");
    }
}

/// Фиксирует пачку; если в ней есть команда «durable», а по умолчанию `Normal`,
/// на эту транзакцию включается `synchronous=FULL`, затем значение восстанавливается.
fn commit_with(
    conn: &Connection,
    batch: &mut [Box<dyn Job>],
    durability: Durability,
) -> Result<(), StoreError> {
    let raise = durability == Durability::Normal && batch.iter().any(|j| j.durable());
    if !raise {
        return commit_batch(conn, batch);
    }
    conn.pragma_update(None, "synchronous", Durability::Full.pragma())?;
    let r = commit_batch(conn, batch);
    if let Err(e) = conn.pragma_update(None, "synchronous", durability.pragma()) {
        // Остаться на FULL безопасно: это только медленнее.
        tracing::error!(error = %e, "не удалось вернуть synchronous после durable-пачки");
    }
    r
}

/// Выполняет пачку команд в одной транзакции.
fn commit_batch(conn: &Connection, batch: &mut [Box<dyn Job>]) -> Result<(), StoreError> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    for j in batch.iter_mut() {
        let err = j.run(conn);
        // Ошибки вроде SQLITE_FULL могут откатить всю транзакцию. Тогда остальные
        // команды выполнились бы в автокоммите мимо общей пачки: прерываем пачку.
        if conn.is_autocommit() {
            return Err(
                err.unwrap_or_else(|| StoreError::Sqlite("транзакция откачена SQLite".to_owned()))
            );
        }
    }
    let r = conn.execute_batch("COMMIT").map_err(StoreError::from);
    if r.is_err()
        && !conn.is_autocommit()
        && let Err(e) = conn.execute_batch("ROLLBACK")
    {
        tracing::error!(error = %e, "ROLLBACK после неудачного COMMIT");
    }
    r
}

/// Выполняет `f` в точке сохранения: при ошибке откатываются только её изменения.
fn in_savepoint<T>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    conn.execute_batch("SAVEPOINT za_job")?;
    let r = f(conn).and_then(|v| {
        conn.execute_batch("RELEASE za_job")?;
        Ok(v)
    });
    if r.is_err() && !conn.is_autocommit() {
        // Точка сохранения могла исчезнуть вместе с транзакцией; тогда откат не нужен.
        let _ = conn.execute_batch("ROLLBACK TO za_job; RELEASE za_job");
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_job(
        v: i64,
        fail: bool,
    ) -> (Box<dyn Job>, oneshot::Receiver<Result<i64, StoreError>>) {
        let (tx, rx) = oneshot::channel();
        let j = job(
            move |c: &Connection| {
                c.execute("INSERT INTO t (v) VALUES (?1)", [v])?;
                if fail {
                    return Err(StoreError::Sqlite("отказ команды".into()));
                }
                Ok(v)
            },
            tx,
            false,
        );
        (j, rx)
    }

    #[test]
    fn failed_job_rolls_back_only_itself() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE t (v INTEGER)").unwrap();
        let (a, ra) = insert_job(1, false);
        let (b, rb) = insert_job(2, true);
        let (d, rd) = insert_job(3, false);
        let mut batch = vec![a, b, d];
        let commit = commit_batch(&c, &mut batch);
        assert!(commit.is_ok());
        for j in batch {
            j.finish(commit.clone());
        }
        assert_eq!(ra.blocking_recv().unwrap(), Ok(1));
        assert!(rb.blocking_recv().unwrap().is_err());
        assert_eq!(rd.blocking_recv().unwrap(), Ok(3));
        let mut s = c.prepare("SELECT v FROM t ORDER BY v").unwrap();
        let got: Vec<i64> = s
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(got, vec![1, 3]);
        assert!(c.is_autocommit());
    }

    #[test]
    fn failed_begin_fails_every_job() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("BEGIN").unwrap();
        let (a, ra) = insert_job(1, false);
        let mut batch = vec![a];
        let commit = commit_batch(&c, &mut batch);
        assert!(commit.is_err());
        for j in batch {
            j.finish(commit.clone());
        }
        assert!(ra.blocking_recv().unwrap().is_err());
    }

    /// Команда, возвращающая `PRAGMA synchronous` внутри транзакции.
    fn sync_job(durable: bool) -> (Box<dyn Job>, oneshot::Receiver<Result<i64, StoreError>>) {
        let (tx, rx) = oneshot::channel();
        let j = job(
            |c: &Connection| Ok(c.pragma_query_value(None, "synchronous", |r| r.get(0))?),
            tx,
            durable,
        );
        (j, rx)
    }

    fn run_batch(c: &Connection, mut batch: Vec<Box<dyn Job>>, d: Durability) {
        let commit = commit_with(c, &mut batch, d);
        assert!(commit.is_ok());
        for j in batch {
            j.finish(commit.clone());
        }
    }

    fn sync_of(c: &Connection) -> i64 {
        c.pragma_query_value(None, "synchronous", |r| r.get(0))
            .unwrap()
    }

    // Значения PRAGMA synchronous: 1 = NORMAL, 2 = FULL.
    #[test]
    fn configure_applies_durability() {
        // WAL требует файла; каталог внутри target проекта.
        let d = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/za-store-unit/configure");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let c = Connection::open(d.join("db")).unwrap();
        configure(&c, Durability::Normal).unwrap();
        assert_eq!(sync_of(&c), 1);
        configure(&c, Durability::Full).unwrap();
        assert_eq!(sync_of(&c), 2);
        let limit: i64 = c
            .pragma_query_value(None, "journal_size_limit", |r| r.get(0))
            .unwrap();
        assert_eq!(limit, JOURNAL_SIZE_LIMIT);
    }

    #[test]
    fn durable_job_raises_the_whole_batch_to_full_and_restores() {
        let c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "synchronous", "NORMAL").unwrap();
        let (n, rn) = sync_job(false);
        run_batch(&c, vec![n], Durability::Normal);
        assert_eq!(rn.blocking_recv().unwrap(), Ok(1));

        let (n, rn) = sync_job(false);
        let (d, rd) = sync_job(true);
        run_batch(&c, vec![n, d], Durability::Normal);
        // Обычная команда в одной пачке с durable тоже фиксируется с FULL.
        assert_eq!(rn.blocking_recv().unwrap(), Ok(2));
        assert_eq!(rd.blocking_recv().unwrap(), Ok(2));
        assert_eq!(sync_of(&c), 1, "synchronous не восстановлен");
    }

    #[test]
    fn full_default_stays_full() {
        let c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "synchronous", "FULL").unwrap();
        let (d, rd) = sync_job(true);
        run_batch(&c, vec![d], Durability::Full);
        assert_eq!(rd.blocking_recv().unwrap(), Ok(2));
        assert_eq!(sync_of(&c), 2);
    }

    #[test]
    fn busy_checkpoint_warns_once_per_streak() {
        assert_eq!(busy_log_level(1, 9 << 20, false), Level::WARN);
        assert_eq!(busy_log_level(2, 9 << 20, false), Level::DEBUG);
        // Впервые больше JOURNAL_SIZE_LIMIT.
        assert_eq!(busy_log_level(5, 65 << 20, false), Level::WARN);
        assert_eq!(busy_log_level(6, 65 << 20, true), Level::DEBUG);
    }
}
