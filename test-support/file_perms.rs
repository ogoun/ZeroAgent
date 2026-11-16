//! Действуют ли права доступа к файлам для процесса теста — один помощник на workspace
//! (R1-C2 fix-8, подозрение 2 отчёта исправления CI Linux).
//!
//! Под root (или с `CAP_DAC_OVERRIDE`, например в контейнере) запрет записи и чтения по
//! правам обходится, и тест отказа по правам проверять нечего: он упал бы, хотя код прав.
//! `geteuid` в std без `unsafe` недоступен (`unsafe_code = "forbid"`), а `id -u` проверял
//! бы не то: в контейнере без root, но с `CAP_DAC_OVERRIDE`, права тоже не действуют.
//! Поэтому проверка прямая: файл без права записи открывается на запись — права не
//! действуют. На Windows атрибут «только чтение» действует и для администратора.
//!
//! Подключается в тестовый крейт как модуль:
//!
//! ```ignore
//! #[path = "../../../test-support/file_perms.rs"]
//! mod file_perms;
//! ```
#![allow(dead_code)]

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

/// Причина пропуска для вывода теста.
pub const NOT_ENFORCED: &str =
    "права доступа не действуют для процесса теста (root или CAP_DAC_OVERRIDE): тест пропущен";

/// `true`, если запрет записи по правам действует: файл «только чтение» в `dir` на запись
/// не открывается. Пробный файл удаляется.
pub fn write_denial_enforced(dir: &Path) -> bool {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let probe = dir.join(format!(".perm-probe-{}-{n}", std::process::id()));
    std::fs::create_dir_all(dir).expect("каталог пробы прав");
    std::fs::write(&probe, b"x").expect("пробный файл прав");
    let mut perms = std::fs::metadata(&probe).expect("stat пробы").permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&probe, perms.clone()).expect("только чтение для пробы");
    let opened = std::fs::OpenOptions::new().write(true).open(&probe).is_ok();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    let _ = std::fs::set_permissions(&probe, perms);
    let _ = std::fs::remove_file(&probe);
    !opened
}
