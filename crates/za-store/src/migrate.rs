//! Миграции схемы: SQL встроен в бинарник, версия хранится в `PRAGMA user_version`,
//! каждая миграция применяется в своей транзакции.

use rusqlite::{Connection, TransactionBehavior};

use crate::StoreError;

/// Миграции по возрастанию версии.
const MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../migrations/0001_init.sql")),
    (2, include_str!("../migrations/0002_usage.sql")),
    (3, include_str!("../migrations/0003_audit.sql")),
];

/// Последняя известная версия схемы.
pub(crate) fn latest() -> u32 {
    MIGRATIONS.last().map_or(0, |m| m.0)
}

/// Доводит схему до последней версии.
#[cfg(test)]
pub(crate) fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    migrate_with(conn, None)
}

/// Доводит схему до последней версии; `abort_at` — тестовая точка сбоя: процесс
/// аварийно завершается после SQL миграции этой версии, до COMMIT.
pub(crate) fn migrate_with(conn: &mut Connection, abort_at: Option<u32>) -> Result<(), StoreError> {
    let current: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if current > latest() {
        return Err(StoreError::SchemaTooNew {
            found: current,
            supported: latest(),
        });
    }
    for (version, sql) in MIGRATIONS.iter().filter(|m| m.0 > current) {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        if abort_at == Some(*version) {
            tracing::error!(version, "тестовый сбой посреди миграции");
            std::process::abort();
        }
        tx.commit()?;
        tracing::info!(version, "миграция журнала применена");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(c: &Connection) -> u32 {
        c.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn migrate_is_idempotent() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        assert_eq!(version(&c), latest());
        migrate(&mut c).unwrap();
        assert_eq!(version(&c), latest());
        for t in [
            "runs",
            "events",
            "effects",
            "interrupts",
            "snapshots",
            "blobs",
            "usage",
            "audit",
        ] {
            let n: i64 = c
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "нет таблицы {t}");
        }
    }

    /// Миграция 3 повторно применима (`IF NOT EXISTS`): журнал, где таблица уже есть,
    /// а `user_version` не поднят, доводится без ошибки и без потери записей.
    #[test]
    fn migration_3_is_reapplicable() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        c.execute(
            "INSERT INTO audit (seq, prev_hash, hash) VALUES (1, 'a', 'b')",
            [],
        )
        .unwrap();
        c.pragma_update(None, "user_version", 2).unwrap();
        migrate(&mut c).unwrap();
        assert_eq!(version(&c), 3);
        let n: i64 = c
            .query_row("SELECT count(*) FROM audit", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn newer_schema_is_rejected() {
        let mut c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "user_version", latest() + 1).unwrap();
        assert_eq!(
            migrate(&mut c),
            Err(StoreError::SchemaTooNew {
                found: latest() + 1,
                supported: latest()
            })
        );
    }

    #[test]
    fn failed_migration_leaves_no_trace() {
        let mut c = Connection::open_in_memory().unwrap();
        // Занятое имя таблицы ломает миграцию посередине.
        c.execute_batch("CREATE TABLE events (x)").unwrap();
        assert!(migrate(&mut c).is_err());
        assert_eq!(version(&c), 0);
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name = 'runs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }
}
