//! Поток-писатель: единственное соединение на запись, очередь команд, group commit.
//!
//! Писатель берёт первую команду с ожиданием, затем без ожидания всё накопленное
//! (не больше [`MAX_BATCH`]) и выполняет пачку в одной транзакции `BEGIN IMMEDIATE`.
//! Каждая команда работает в своей точке сохранения: ошибка одной откатывает только
//! её. Ответы уходят после COMMIT, поэтому `Ok` означает, что данные на диске.

use std::sync::mpsc;
use std::thread::JoinHandle;

use rusqlite::Connection;
use tokio::sync::oneshot;

use crate::{BUSY_TIMEOUT, StoreError};

/// Очередь команд писателю.
pub(crate) type JobSender = mpsc::Sender<Box<dyn Job>>;

/// Наибольшее число команд в одной транзакции.
pub(crate) const MAX_BATCH: usize = 256;

/// Команда писателю.
pub(crate) trait Job: Send {
    /// Выполняет команду в открытой транзакции; возвращает её ошибку, если была.
    fn run(&mut self, conn: &Connection) -> Option<StoreError>;
    /// Отправляет ответ по итогам транзакции.
    fn finish(self: Box<Self>, commit: Result<(), StoreError>);
}

struct TypedJob<T, F> {
    f: Option<F>,
    result: Option<Result<T, StoreError>>,
    reply: oneshot::Sender<Result<T, StoreError>>,
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

    fn finish(self: Box<Self>, commit: Result<(), StoreError>) {
        let reply = match commit {
            Err(e) => Err(e),
            Ok(()) => self.result.unwrap_or(Err(StoreError::Closed)),
        };
        // Получатель мог перестать ждать; это не ошибка писателя.
        let _ = self.reply.send(reply);
    }
}

/// Упаковывает замыкание в команду писателю.
pub(crate) fn job<T, F>(f: F, reply: oneshot::Sender<Result<T, StoreError>>) -> Box<dyn Job>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
{
    Box::new(TypedJob {
        f: Some(f),
        result: None,
        reply,
    })
}

/// Настройки соединения писателя (CORE-005).
pub(crate) fn configure(conn: &Connection) -> Result<(), StoreError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Sqlite(format!(
            "не удалось включить WAL, режим {mode}"
        )));
    }
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Запускает поток-писатель на готовом соединении.
pub(crate) fn spawn(conn: Connection) -> Result<(JobSender, JoinHandle<()>), StoreError> {
    let (tx, rx) = mpsc::channel::<Box<dyn Job>>();
    let handle = std::thread::Builder::new()
        .name("za-store-writer".into())
        .spawn(move || run(&conn, &rx))?;
    Ok((tx, handle))
}

fn run(conn: &Connection, rx: &mpsc::Receiver<Box<dyn Job>>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(j) => batch.push(j),
                Err(_) => break,
            }
        }
        let commit = commit_batch(conn, &mut batch);
        if let Err(e) = &commit {
            tracing::warn!(error = %e, jobs = batch.len(), "транзакция писателя не зафиксирована");
        }
        for j in batch {
            j.finish(commit.clone());
        }
    }
    tracing::debug!("поток-писатель завершён");
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
}
