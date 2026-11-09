//! Пул читателей: отдельные соединения `query_only=ON` в своих потоках (ARC-030).
//!
//! В режиме WAL читатели не блокируют писателя и видят только зафиксированные данные.

use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use rusqlite::Connection;

use crate::{BUSY_TIMEOUT, StoreError};

/// Число соединений-читателей.
pub(crate) const READERS: usize = 4;

/// Запрос к читателю.
pub(crate) type ReadJob = Box<dyn FnOnce(&Connection) + Send>;

/// Открывает соединения и запускает потоки читателей.
pub(crate) fn spawn(
    path: &Path,
) -> Result<(mpsc::Sender<ReadJob>, Vec<JoinHandle<()>>), StoreError> {
    let mut conns = Vec::with_capacity(READERS);
    for _ in 0..READERS {
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "query_only", "ON")?;
        conns.push(conn);
    }
    let (tx, rx) = mpsc::channel::<ReadJob>();
    let rx = Arc::new(Mutex::new(rx));
    let mut handles = Vec::with_capacity(READERS);
    for (i, conn) in conns.into_iter().enumerate() {
        let rx = Arc::clone(&rx);
        let h = std::thread::Builder::new()
            .name(format!("za-store-reader-{i}"))
            .spawn(move || run(&conn, &rx))?;
        handles.push(h);
    }
    Ok((tx, handles))
}

fn run(conn: &Connection, rx: &Mutex<mpsc::Receiver<ReadJob>>) {
    loop {
        // Блокировка держится только на время получения запроса.
        let job = {
            let guard = match rx.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.recv()
        };
        match job {
            Ok(job) => job(conn),
            Err(_) => break,
        }
    }
}
