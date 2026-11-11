//! Секреты в чекпоинтах (R1-C2 D103): имя секретного файла, фильтр бинарника по
//! содержимому и его кэш (ревью задачи 2, сомнение 2).
//!
//! Проверка по имени ([`secret_name`]) идёт всегда первой и кэша не касается. Проверка
//! по содержимому ([`SecretFilter`]) читает до [`SECRET_SCAN_MAX`] байт; её итог
//! запоминается в памяти процесса по ключу «канонический путь + stat + отпечаток набора
//! известных значений». Запись верна, только если файл не менялся позже, чем за
//! [`RACY`] до проверки (как у кэша stat git). Смена набора значений (другой отпечаток)
//! или stat файла — новая проверка. Отпечаток хранится только в памяти и на диск не
//! пишется.

use std::collections::HashMap;
use std::fs::Metadata;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::git::{RACY, ctime_ino, mtime_ns, now_ns};

/// Файл до этого размера проверяется [`SecretFilter`] (D103); крупнее — только по имени.
pub const SECRET_SCAN_MAX: u64 = 1 << 20;

/// Имя секретного файла (D103) — по последнему компоненту пути (`/` или `\`), без учёта
/// регистра: `.env` и `.env.*` (кроме `.env.example`, `.env.sample`, `.env.template`,
/// `.env.dist`), `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.jks`, `*.keystore`, `*.kdbx`,
/// закрытые ключи SSH без `.pub`, `.netrc`, `_netrc`, `.npmrc`, `.pypirc`,
/// `.git-credentials`.
pub fn secret_name(rel: &str) -> bool {
    let last = rel.rsplit(['/', '\\']).next().unwrap_or(rel);
    let name = last.to_ascii_lowercase();
    if name == ".env" {
        return true;
    }
    if name.starts_with(".env.") {
        return !matches!(
            name.as_str(),
            ".env.example" | ".env.sample" | ".env.template" | ".env.dist"
        );
    }
    const EXT: [&str; 7] = [".pem", ".key", ".p12", ".pfx", ".jks", ".keystore", ".kdbx"];
    const EXACT: [&str; 9] = [
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        ".netrc",
        "_netrc",
        ".npmrc",
        ".pypirc",
        ".git-credentials",
    ];
    EXT.iter().any(|e| name.ends_with(e)) || EXACT.contains(&name.as_str())
}

/// Признак секрета в содержимом файла (D103): `true` — файл в чекпоинт не входит и
/// восстановление его не трогает. Даёт бинарник (известные значения хранилища и блок
/// PEM); `za-tools` от `za-secrets` не зависит. Вызывается только для файлов до
/// [`SECRET_SCAN_MAX`] байт.
///
/// `fingerprint` — отпечаток набора известных значений (например, BLAKE3): при его смене
/// запомненные итоги проверки не используются. Отпечаток хранится только в памяти.
#[derive(Clone)]
pub struct SecretFilter {
    check: Arc<CheckFn>,
    fingerprint: [u8; 32],
}

impl SecretFilter {
    /// Фильтр `check` с отпечатком набора значений `fingerprint`.
    pub fn new(
        fingerprint: [u8; 32],
        check: impl Fn(&[u8]) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            check: Arc::new(check),
            fingerprint,
        }
    }

    /// Есть ли секрет в `bytes`.
    pub fn matches(&self, bytes: &[u8]) -> bool {
        (self.check)(bytes)
    }

    /// Отпечаток набора известных значений.
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
}

impl std::fmt::Debug for SecretFilter {
    /// Отпечаток не печатается.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretFilter").finish_non_exhaustive()
    }
}

/// Проверка байт фильтра.
type CheckFn = dyn Fn(&[u8]) -> bool + Send + Sync;

/// stat обычного файла — ключ запомненной проверки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatKey {
    size: u64,
    mtime_ns: u64,
    /// Unix: `ctime` и `ino`; прочие ОС — 0.
    ctime_ns: u64,
    ino: u64,
}

impl StatKey {
    fn of(m: &Metadata) -> Self {
        let (ctime_ns, ino) = ctime_ino(m);
        Self {
            size: m.len(),
            mtime_ns: mtime_ns(m),
            ctime_ns,
            ino,
        }
    }
}

/// Итог проверки содержимого.
#[derive(Debug, Clone, Copy)]
struct Scanned {
    key: StatKey,
    fingerprint: [u8; 32],
    /// Когда проверено (нс от эпохи).
    at_ns: u64,
    secret: bool,
}

impl Scanned {
    /// Итог годен для файла со stat `key` и фильтра `fp`: stat и отпечаток те же, файл
    /// не менялся позже, чем за [`RACY`] до проверки (время изменения известно).
    fn fits(&self, key: StatKey, fp: [u8; 32]) -> bool {
        let racy = u64::try_from(RACY.as_nanos()).unwrap_or(u64::MAX);
        let before = self.at_ns.saturating_sub(racy);
        self.key == key
            && self.fingerprint == fp
            && key.mtime_ns != 0
            && key.mtime_ns < before
            && (key.ctime_ns == 0 || key.ctime_ns < before)
    }
}

/// Проверка «секрет ли файл» для чекпоинтов и восстановления: имя, затем фильтр с
/// кэшем итогов в памяти. Клоны делят один кэш.
#[derive(Clone, Default)]
pub(crate) struct SecretScan {
    filter: Option<SecretFilter>,
    cache: Arc<Mutex<HashMap<PathBuf, Scanned>>>,
}

impl SecretScan {
    pub(crate) fn new(filter: Option<SecretFilter>) -> Self {
        Self {
            filter,
            cache: Arc::default(),
        }
    }

    /// Секрет ли обычный файл `abs` (имя `rel`, stat `meta`): по имени, затем фильтром
    /// по содержимому (до [`SECRET_SCAN_MAX`] байт, с кэшем). Файл не прочитан — не
    /// секрет: его содержимое не попадёт и в чекпоинт.
    pub(crate) fn is_secret(&self, rel: &str, abs: &Path, meta: &Metadata) -> bool {
        if secret_name(rel) {
            return true;
        }
        let Some(f) = &self.filter else {
            return false;
        };
        if meta.len() > SECRET_SCAN_MAX {
            return false;
        }
        let (key, fp) = (StatKey::of(meta), f.fingerprint());
        if let Some(s) = self.cached(abs, key, fp) {
            return s;
        }
        let at_ns = now_ns();
        let mut buf = Vec::new();
        let read = std::fs::File::open(abs)
            .and_then(|file| file.take(SECRET_SCAN_MAX).read_to_end(&mut buf));
        if let Err(e) = read {
            tracing::debug!(path = %rel, error = %e, "файл не прочитан фильтром секретов");
            return false;
        }
        let secret = f.matches(&buf);
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).insert(
            abs.to_path_buf(),
            Scanned {
                key,
                fingerprint: fp,
                at_ns,
                secret,
            },
        );
        secret
    }

    /// Секрет ли сейчас файл `abs` (имя `rel`) — для восстановления (ревью задачи 2, I2):
    /// по имени — всегда, по содержимому — если это обычный файл. Нет файла — только имя.
    pub(crate) fn now(&self, rel: &str, abs: &Path) -> bool {
        if secret_name(rel) {
            return true;
        }
        match std::fs::symlink_metadata(abs) {
            Ok(m) if m.is_file() => self.is_secret(rel, abs, &m),
            _ => false,
        }
    }

    /// Есть ли секрет в уже прочитанных байтах (копия файлов читает файл один раз:
    /// проверяется ровно то, что будет скопировано). Без фильтра — нет.
    pub(crate) fn matches(&self, bytes: &[u8]) -> bool {
        self.filter.as_ref().is_some_and(|f| f.matches(bytes))
    }

    /// Задан ли фильтр по содержимому.
    pub(crate) fn has_filter(&self) -> bool {
        self.filter.is_some()
    }

    fn cached(&self, abs: &Path, key: StatKey, fp: [u8; 32]) -> Option<bool> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.get(abs).filter(|s| s.fits(key, fp)).map(|s| s.secret)
    }
}
