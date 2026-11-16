//! Действуют ли права доступа ОС к файлам для процесса теста — один помощник на workspace
//! (R1-C2 fix-8, подозрение 2 отчёта исправления CI Linux; причина исправлена в fix-9, I1
//! ревью fix-8).
//!
//! Под root (или с `CAP_DAC_OVERRIDE`, например в контейнере) ОС не запрещает запись в файл
//! без права записи. Это важно различать:
//! - тест, который полагается на отказ самой ОС, в таком окружении проверять нечего;
//! - тест политики инструмента (например, `za-tools` `edit` отказывает по битам режима и
//!   владельцу) пропускать нельзя: под root ОС не защищает файл, и проверка инструмента —
//!   единственная преграда. Такой тест берёт [`denial_note`] только в сообщение при падении.
//!
//! Проверка прямая: файл без права записи открывается на запись — запрет ОС не действует.
//! `geteuid` (std без `unsafe` его нет, в `nix` есть) не видит `CAP_DAC_OVERRIDE` без root,
//! поэтому проба надёжнее. На Windows атрибут «только чтение» действует и для администратора.
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

/// `true`, если запрет записи ОС действует: файл «только чтение» в `dir` на запись не
/// открывается. Пробный файл удаляется.
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

/// Строка для сообщения теста политики: действует ли запрет записи ОС в этом окружении.
pub fn denial_note(dir: &Path) -> &'static str {
    if write_denial_enforced(dir) {
        "запрет записи ОС действует"
    } else {
        "запрет записи ОС не действует (root или CAP_DAC_OVERRIDE): отказ обязан дать сам инструмент"
    }
}
