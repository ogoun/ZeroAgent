//! Секреты в чекпоинтах (R1-C2 D103): имя секретного файла, фильтр бинарника по
//! содержимому и его кэш (ревью задачи 2, сомнение 2).
//!
//! Проверка по имени ([`secret_name`]) идёт всегда первой и кэша не касается. Проверка
//! по содержимому ([`SecretFilter`]) читает до [`SECRET_SCAN_MAX`] байт; её итог
//! запоминается в памяти процесса по ключу «канонический путь + stat + отпечаток набора
//! известных значений». Запись верна, только если файл не менялся позже, чем за
//! [`RACY`] до проверки (как у кэша stat git). Смена набора значений (другой отпечаток)
//! или stat файла — новая проверка. Отпечаток хранится только в памяти и на диск не
//! пишется. Восстановление ([`SecretScan::now`]) запомненным итогом не пользуется и
//! читает файл заново.
//!
//! Момент проверки берётся из часов [`SecretScan`] — [`now_ns`]. Подменить их может только
//! сборка с признаком `test-hooks` (D128, `GitCheckpointer::with_secret_clock_for_tests`):
//! на Unix `ctime` в прошлое не сдвинуть (`set_modified` ставит его на «сейчас»), и без
//! подмены тест ждал бы [`RACY`] по таймеру. Обычная и release-сборка подмены не содержат.

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
/// `*.ppk` (ключ PuTTY, финальное ревью безопасности, Minor 2), закрытые ключи SSH без
/// `.pub`, `.netrc`, `_netrc`, `.npmrc`, `.pypirc`, `.git-credentials`. `*.gpg` в списке нет
/// осознанно: так называют и открытые связки ключей, и просто зашифрованные данные, а
/// ложное срабатывание лишает файл чекпоинта; закрытый ключ PGP ловит фильтр содержимого.
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
    const EXT: [&str; 8] = [
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        ".jks",
        ".keystore",
        ".kdbx",
        ".ppk",
    ];
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

/// Часы проверки: наносекунды от эпохи. От их показания отсчитывается [`RACY`] записи.
pub(crate) type Clock = fn() -> u64;

/// Проверка «секрет ли файл» для чекпоинтов и восстановления: имя, затем фильтр с
/// кэшем итогов в памяти. Клоны делят один кэш.
#[derive(Clone)]
pub(crate) struct SecretScan {
    filter: Option<SecretFilter>,
    cache: Arc<Mutex<HashMap<PathBuf, Scanned>>>,
    /// Момент проверки ([`Scanned::at_ns`]); вне `test-hooks` — всегда [`now_ns`].
    clock: Clock,
}

impl Default for SecretScan {
    fn default() -> Self {
        Self::new(None)
    }
}

impl SecretScan {
    pub(crate) fn new(filter: Option<SecretFilter>) -> Self {
        Self {
            filter,
            cache: Arc::default(),
            clock: now_ns,
        }
    }

    /// Фильтр `f` с новым (пустым) кэшем; часы прежние.
    pub(crate) fn with_filter(self, f: SecretFilter) -> Self {
        Self {
            clock: self.clock,
            ..Self::new(Some(f))
        }
    }

    /// Часы проверки `clock` вместо [`now_ns`] — только модульные тесты и признак
    /// `test-hooks` (D128).
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
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
        let key = StatKey::of(meta);
        if let Some(s) = self.cached(abs, key, f.fingerprint()) {
            return s;
        }
        self.scan(f, rel, abs, key)
    }

    /// Проверка содержимого фильтром `f` без кэша; итог записывается в кэш.
    fn scan(&self, f: &SecretFilter, rel: &str, abs: &Path, key: StatKey) -> bool {
        let fp = f.fingerprint();
        let at_ns = (self.clock)();
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
    /// по имени — всегда, по содержимому — если это обычный файл до
    /// [`SECRET_SCAN_MAX`] байт. Нет файла — только имя.
    ///
    /// Содержимое читается всегда, запомненный итог не используется (повторное ревью
    /// задач 1–2, Minor 1): на Windows в ключе нет `ctime` и `ino`, и файл, изменённый с
    /// прежними размером и `mtime`, прошёл бы по устаревшей записи «не секрет», а
    /// восстановление перезаписало бы его без копии «до». Новый итог попадает в кэш.
    pub(crate) fn now(&self, rel: &str, abs: &Path) -> bool {
        if secret_name(rel) {
            return true;
        }
        let Some(f) = &self.filter else {
            return false;
        };
        match std::fs::symlink_metadata(abs) {
            Ok(m) if m.is_file() && m.len() <= SECRET_SCAN_MAX => {
                self.scan(f, rel, abs, StatKey::of(&m))
            }
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Секунда в наносекундах.
    const S: u64 = 1_000_000_000;
    /// Показание часов проверки в тестах.
    const AT: u64 = 1_000 * S;

    fn at() -> u64 {
        AT
    }

    /// stat с метками `mtime_ns` и `ctime_ns`; `ctime_ns` = 0 — как на Windows (без
    /// `ctime` и `ino`).
    fn key(mtime_ns: u64, ctime_ns: u64) -> StatKey {
        StatKey {
            size: 6,
            mtime_ns,
            ctime_ns,
            ino: if ctime_ns == 0 { 0 } else { 42 },
        }
    }

    fn entry(key: StatKey) -> Scanned {
        Scanned {
            key,
            fingerprint: [1; 32],
            at_ns: AT,
            secret: false,
        }
    }

    /// Запись годна, только если `mtime` и (Unix) `ctime` раньше проверки больше чем на
    /// RACY. Ветку `ctime` CI Windows иначе не проверяет: там `ctime` в ключе — 0.
    #[test]
    fn fits_only_files_older_than_racy() {
        let racy = u64::try_from(RACY.as_nanos()).unwrap();
        let old = AT - 600 * S;
        let fits = |k: StatKey| entry(k).fits(k, [1; 32]);
        assert!(fits(key(old, old)));
        // Windows: `ctime` нет — решает `mtime`.
        assert!(fits(key(old, 0)));
        // Unix после `set_modified`: `mtime` в прошлом, `ctime` — «сейчас».
        assert!(!fits(key(old, AT - S / 10)));
        // Ровно RACY до проверки — ещё не годится: «раньше» строгое.
        assert!(!fits(key(AT - racy, old)));
        assert!(!fits(key(old, AT - racy)));
        assert!(fits(key(AT - racy - 1, AT - racy - 1)));
        // `mtime` неизвестен.
        assert!(!fits(key(0, 0)));
    }

    /// Запись годна только для того же stat и того же отпечатка набора значений.
    #[test]
    fn fits_only_same_stat_and_fingerprint() {
        let old = AT - 600 * S;
        let k = key(old, old);
        let e = entry(k);
        assert!(e.fits(k, [1; 32]));
        assert!(!e.fits(k, [2; 32]));
        assert!(!e.fits(StatKey { size: 7, ..k }, [1; 32]));
        assert!(!e.fits(
            StatKey {
                mtime_ns: old + 1,
                ..k
            },
            [1; 32]
        ));
        assert!(!e.fits(
            StatKey {
                ctime_ns: old + 1,
                ..k
            },
            [1; 32]
        ));
        assert!(!e.fits(StatKey { ino: 43, ..k }, [1; 32]));
    }

    /// Момент записи — показание часов проверки; `with_filter` часы сохраняет.
    #[test]
    fn scan_stamps_entry_with_its_clock() {
        let calls = Arc::new(AtomicUsize::new(0));
        let n = calls.clone();
        let f = SecretFilter::new([1; 32], move |_: &[u8]| {
            n.fetch_add(1, Ordering::SeqCst);
            false
        });
        let scan = SecretScan::default().with_clock(at).with_filter(f.clone());
        // Любой читаемый файл: содержимое фильтру безразлично, stat — из ключа.
        let abs = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let old = key(AT - 600 * S, AT - 600 * S);
        assert!(!scan.scan(&f, "Cargo.toml", &abs, old));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(scan.cached(&abs, old, [1; 32]), Some(false));
        assert_eq!(scan.cached(&abs, old, [2; 32]), None);
        // Файл моложе RACY по часам проверки — запись не годится.
        let fresh = key(AT - S, AT - S);
        assert!(!scan.scan(&f, "Cargo.toml", &abs, fresh));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(scan.cached(&abs, fresh, [1; 32]), None);
    }
}
