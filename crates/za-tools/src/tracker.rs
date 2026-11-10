//! Трекер чтений (раздел 4.1 ТЗ 02, D87): `edit` правит только файл, прочитанный в этом
//! run и не изменившийся с тех пор.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use za_protocol::RunId;

/// Почему правка без нового чтения недопустима.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackError {
    /// Файл не читался в этом run.
    NotRead,
    /// Содержимое отличается от прочитанного (или от своей последней правки).
    Changed,
}

/// Общий на процесс трекер: `(run, канонический путь)` → BLAKE3 (hex) содержимого на
/// момент чтения или своей правки. После перезапуска процесса он пуст — записанное
/// ограничение D87: правка после resume требует нового чтения.
#[derive(Debug, Default)]
pub struct ReadTracker {
    seen: Mutex<HashMap<(RunId, PathBuf), String>>,
}

impl ReadTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Запоминает хэш файла: после чтения или своей правки.
    pub fn mark(&self, run: RunId, path: &Path, hash: &str) {
        self.lock()
            .insert((run, path.to_path_buf()), hash.to_owned());
    }

    /// Совпадает ли текущий хэш файла с запомненным.
    pub fn check(&self, run: RunId, path: &Path, current: &str) -> Result<(), TrackError> {
        match self.lock().get(&(run, path.to_path_buf())) {
            None => Err(TrackError::NotRead),
            Some(h) if h == current => Ok(()),
            Some(_) => Err(TrackError::Changed),
        }
    }

    /// Отравленный мьютекс не теряет данные: таблица — только вставки и чтения.
    fn lock(&self) -> MutexGuard<'_, HashMap<(RunId, PathBuf), String>> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
