//! Открытие журнала проекта с ограниченным повтором на `Locked`.
//!
//! На Windows ОС снимает блокировку `journal.lock` убитого процесса не
//! мгновенно, поэтому сразу после аварийного завершения открытие может
//! получить [`StoreError::Locked`]. Повтор ограничен: живой владелец
//! блокировки (второй процесс) всё равно даёт `Locked` примерно через секунду.

use std::path::Path;
use std::time::Duration;

use za_store::{ProjectStore, StoreError};

/// Сколько раз повторить открытие после `Locked`.
/// Окно около 3 с: на Windows закрытие журнала (потоки, контрольная точка WAL)
/// и снятие блокировки убитого процесса под нагрузкой занимают больше секунды.
pub const OPEN_RETRIES: u32 = 30;
/// Пауза между попытками открытия.
pub const OPEN_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Открывает журнал в `dir`; при `Locked` повторяет до [`OPEN_RETRIES`] раз
/// с паузой [`OPEN_RETRY_DELAY`]. Прочие ошибки возвращаются сразу.
pub fn open_retrying(dir: &Path) -> Result<ProjectStore, StoreError> {
    let mut left = OPEN_RETRIES;
    loop {
        match ProjectStore::open(dir) {
            Err(StoreError::Locked) if left > 0 => {
                left -= 1;
                tracing::debug!(dir = %dir.display(), left, "журнал занят, повтор открытия");
                std::thread::sleep(OPEN_RETRY_DELAY);
            }
            r => return r,
        }
    }
}
