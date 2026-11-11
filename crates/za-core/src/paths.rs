//! Пути вызовов (R1-C1 D76, D79, D98): разбор аргумента модели, граница проекта,
//! защищённые пути, рабочие каталоги агента и безопасное открытие файла.
//!
//! Два этапа:
//! 1. [`classify`] — чистая функция без ввода-вывода. Сетевые пути (UNC, `\\?\UNC\`),
//!    пространства устройств (`\\.\`, `\\?\GLOBALROOT`, `CON`…), альтернативные потоки
//!    (`:` в компоненте, `::$`), хвостовые точки и пробелы, `C:foo` отклоняются **до**
//!    любого обращения к ФС: канонизация UNC из аргумента модели — это SMB-запрос и
//!    утечка NTLM-хэша ещё до решения (Ruling 7.4). Результат — [`LexPath`]:
//!    абсолютный, лексически нормализованный путь; построить его можно только здесь.
//! 2. [`resolve`] — блокирующая (только в `spawn_blocking`): канонизирует самый длинный
//!    существующий префикс и дописывает хвост. Ссылки и junction раскрываются, короткие
//!    имена 8.3 и регистр NTFS приводятся к настоящим именам. Префикс, ставший
//!    `\\?\UNC\…` (подключённый сетевой диск), — [`PathError::Network`]: это одно
//!    обращение к диску, который пользователь подключил сам (записанный остаточный риск).
//!
//! «Внутри» — [`within`] по компонентам, на Windows без учёта регистра. Корни
//! ([`Roots`]) канонизируются один раз при старте run.
//!
//! Открытие ([`open_checked`]) не следует по ссылке на последнем компоненте и сверяет
//! открытый дескриптор с каноническим путём: подмена между решением и открытием (TOCTOU)
//! даёт отказ, а не доступ к чужому файлу.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::tool::Access;

/// Ошибка разбора или разрешения пути. Тексты на английском: они уходят модели.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("network paths are not supported: {0}")]
    Network(String),
    #[error("device paths are not supported: {0}")]
    Device(String),
    #[error("alternate data streams are not supported: {0}")]
    Stream(String),
    #[error("path components must not end with a dot or space: {0}")]
    TrailingDot(String),
    #[error("drive-relative paths are not supported: {0}")]
    DriveRelative(String),
    #[error("invalid path: {0}")]
    Invalid(String),
}

/// Правила разбора: Windows или Unix. Тесты проверяют правила Windows на обеих ОС.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Unix,
}

impl Platform {
    /// Правила текущей ОС.
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// Абсолютный, лексически нормализованный путь (`.` и `..` сняты, разделители
/// приведены), прошедший [`classify`]. Других способов построить его нет: поэтому
/// [`resolve`] никогда не канонизирует UNC или device-путь.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexPath(PathBuf);

impl LexPath {
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// Разрешённый путь вызова: канонический путь, внутри ли он проекта и защищён ли он
/// от нативной записи (D79).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    pub path: PathBuf,
    pub inside: bool,
    pub protected: bool,
}

/// Канонические корни run (D76, Ruling 7.9): проект и `ZA_HOME`.
#[derive(Debug, Clone)]
pub struct Roots {
    pub project: PathBuf,
    pub za_home: PathBuf,
}

impl Roots {
    /// Канонизирует корни (блокирующая). Проект должен существовать; `ZA_HOME` — нет:
    /// канонизируется самый длинный существующий префикс.
    ///
    /// Корни разрешаются так же, как пути вызовов ([`resolve`]): проект на сетевом диске
    /// или за ссылкой на UNC — ошибка, а не корень `\\?\UNC\…` (M5 ревью).
    pub fn canonical(project: &Path, za_home: &Path) -> io::Result<Roots> {
        let _drives = DriveScope::enter();
        let p = std::path::absolute(project)?;
        let (_, missing) = walk(&p, &mut 0).map_err(io::Error::other)?;
        if !missing.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("каталог проекта не найден: {}", p.display()),
            ));
        }
        let project = canonical_prefix(&p).map_err(io::Error::other)?;
        let za_home = canonical_prefix(&std::path::absolute(za_home)?).map_err(io::Error::other)?;
        Ok(Roots { project, za_home })
    }
}

/// Каталог служебных файлов проекта.
const ZA_DIR: &str = ".zero-agent";
/// Рабочие файлы агента внутри [`ZA_DIR`]: запись туда не защищена.
const WORK: &str = "work";
/// Каталог репозитория git.
const GIT_DIR: &str = ".git";
/// Содержимое `.zero-agent/work/.gitignore` (SEC-004, ARC-021).
const WORK_GITIGNORE: &str = "*\n";
/// `FILE_FLAG_OPEN_REPARSE_POINT`: открыть саму ссылку, а не её цель (D98).
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
/// Текст отказа открыть ссылку.
const REFUSE_LINK: &str = "refusing to follow a link";
/// Текст отказа при подмене пути между разрешением и открытием.
const PATH_CHANGED: &str = "path changed while opening";

/// Разбор аргумента модели по правилам текущей ОС; см. [`classify_with`].
pub fn classify(root: &Path, arg: &str) -> Result<LexPath, PathError> {
    classify_with(root, arg, Platform::current())
}

/// Лексический разбор `arg` (D76) без обращения к ФС. Относительный путь берётся от
/// `root` (абсолютного), `~` не раскрывается, `.`, `..` и разделители нормализуются
/// лексически; `..` за корень ФС не уходит.
///
/// Windows (`/` и `\` — разделители): `\\host\share`, `//host/share`, `\\?\UNC\…` →
/// [`PathError::Network`]; `\\.\…`, `\??\…`, `\\?\` не перед буквой диска,
/// зарезервированные имена (`CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9`,
/// `CONIN$`, `CONOUT$`; с расширением, в любом регистре, в любом компоненте) →
/// [`PathError::Device`]; `:` в компоненте → [`PathError::Stream`]; компонент с
/// хвостовой точкой или пробелом → [`PathError::TrailingDot`]; `C:foo`, `C:` и `\foo`
/// (от корня текущего диска) → [`PathError::DriveRelative`]; `<>"|?*` и управляющие
/// символы → [`PathError::Invalid`]. Unix: `:` и `\` — обычные символы имени.
pub fn classify_with(root: &Path, arg: &str, p: Platform) -> Result<LexPath, PathError> {
    if arg.is_empty() {
        return Err(PathError::Invalid("empty path".into()));
    }
    if arg.contains('\0') {
        return Err(PathError::Invalid(arg.escape_debug().to_string()));
    }
    match p {
        Platform::Windows => classify_windows(root, arg),
        Platform::Unix => {
            let parts: Vec<&str> = arg.split('/').collect();
            if arg.starts_with('/') {
                Ok(LexPath(build(PathBuf::from("/"), &parts)))
            } else {
                Ok(LexPath(build(base(root), &parts)))
            }
        }
    }
}

fn is_sep(c: char) -> bool {
    c == '/' || c == '\\'
}

fn classify_windows(root: &Path, arg: &str) -> Result<LexPath, PathError> {
    let err = || arg.to_owned();
    let mut chars = arg.chars();
    let (c0, c1) = (chars.next(), chars.next());
    // Префиксы из двух разделителей: UNC, `\\?\`, `\\.\`.
    let rest = if c0.is_some_and(is_sep) && c1.is_some_and(is_sep) {
        let tail = &arg[2..];
        if let Some(after) = tail.strip_prefix('?').filter(|t| t.starts_with(is_sep)) {
            // `\\?\…`: снимается только перед буквой диска (Ruling 7.10).
            let after = &after[1..];
            let head: String = after
                .chars()
                .take(4)
                .collect::<String>()
                .to_ascii_uppercase();
            if head.starts_with("UNC") && after[3..].starts_with(is_sep) {
                return Err(PathError::Network(err()));
            }
            if !drive_abs(after) {
                return Err(PathError::Device(err()));
            }
            after
        } else if tail.starts_with('.') && tail[1..].starts_with(is_sep) {
            return Err(PathError::Device(err()));
        } else {
            return Err(PathError::Network(err()));
        }
    } else if arg.starts_with(r"\??\") || arg.starts_with("/??/") {
        return Err(PathError::Device(err()));
    } else if c0.is_some_and(is_sep) {
        // `\foo` — от корня текущего диска процесса, а не проекта.
        return Err(PathError::DriveRelative(err()));
    } else {
        arg
    };
    let drive = if drive_abs(rest) {
        Some(&rest[..2])
    } else if rest.len() >= 2
        && rest.as_bytes()[0].is_ascii_alphabetic()
        && rest.as_bytes()[1] == b':'
    {
        return Err(PathError::DriveRelative(err()));
    } else {
        None
    };
    let body = if drive.is_some() { &rest[3..] } else { rest };
    let parts: Vec<&str> = body.split(is_sep).collect();
    for part in &parts {
        check_windows_component(part, arg)?;
    }
    let start = match drive {
        Some(d) => PathBuf::from(format!("{}:\\", d[..1].to_ascii_uppercase())),
        None => base(root),
    };
    Ok(LexPath(build(start, &parts)))
}

/// `X:` и разделитель за ним.
fn drive_abs(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
}

/// Правила компонента Windows (D76).
fn check_windows_component(part: &str, arg: &str) -> Result<(), PathError> {
    if part.is_empty() || part == "." || part == ".." {
        return Ok(());
    }
    if part.contains(':') {
        return Err(PathError::Stream(arg.to_owned()));
    }
    if part.chars().any(|c| c < ' ' || "<>\"|?*".contains(c)) {
        return Err(PathError::Invalid(arg.escape_debug().to_string()));
    }
    let stem = part.split('.').next().unwrap_or(part).trim_end_matches(' ');
    if is_reserved(stem) {
        return Err(PathError::Device(arg.to_owned()));
    }
    if part.ends_with('.') || part.ends_with(' ') {
        return Err(PathError::TrailingDot(arg.to_owned()));
    }
    Ok(())
}

/// Зарезервированное имя устройства Windows (без расширения), в любом регистре.
fn is_reserved(stem: &str) -> bool {
    let up = stem.to_uppercase();
    if matches!(
        up.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let (head, num) = match (up.get(..3), up.get(3..)) {
        (Some(h), Some(n)) => (h, n),
        _ => return false,
    };
    matches!(head, "COM" | "LPT")
        && matches!(
            num,
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        )
}

/// Корень, лексически нормализованный (`.` и `..` сняты).
fn base(root: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in root.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Дописывает компоненты к `start` с лексическим `.` и `..`; `..` у корня — на месте.
fn build(mut out: PathBuf, parts: &[&str]) -> PathBuf {
    for part in parts {
        match *part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            name => out.push(name),
        }
    }
    out
}

/// Предел переходов по ссылкам при разрешении одного пути (как `MAXSYMLINKS` Linux).
const MAX_LINKS: u32 = 40;

thread_local! {
    /// Счётчик вызовов `canonicalize` в этом потоке: тесты проверяют, что до проверки
    /// цепочки ссылок к ФС за целью ссылки не обращались.
    static CANONICALIZE_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Сколько раз этот поток вызвал `canonicalize` внутри модуля (для тестов).
#[doc(hidden)]
pub fn canonicalize_calls() -> u64 {
    CANONICALIZE_CALLS.with(std::cell::Cell::get)
}

/// `canonicalize` только для путей, вся цепочка ссылок которых проверена: локальный
/// диск, цели ссылок не сетевые и не device.
fn canonicalize_local(p: &Path) -> io::Result<PathBuf> {
    CANONICALIZE_CALLS.with(|c| c.set(c.get() + 1));
    std::fs::canonicalize(p)
}

/// Канонический путь без `\\?\` (Ruling 7.10, I2). Префикс — только диск с буквой:
/// `\\?\UNC\…` → `Network`, другой `\\?\…` (том без буквы) → `Device`. На Windows
/// каждый компонент проходит правила [`classify`] **до** снятия `\\?\`: имя вида
/// `.git.` или `nul.txt`, созданное через `\\?\` и найденное по короткому имени 8.3, в
/// Win32 означало бы другой объект (`.git`, устройство `NUL`). Снятие идёт по
/// компонентам, без перевода в строку (имена с непарными суррогатами не портятся).
fn strip_canonical(canon: PathBuf, shown: &dyn Fn() -> String) -> Result<PathBuf, PathError> {
    #[cfg(windows)]
    {
        let mut out = PathBuf::new();
        for c in canon.components() {
            match c {
                Component::Prefix(p) => match p.kind() {
                    Prefix::VerbatimDisk(d) | Prefix::Disk(d) => {
                        out.push(format!("{}:", char::from(d.to_ascii_uppercase())));
                    }
                    Prefix::VerbatimUNC(..) | Prefix::UNC(..) => {
                        return Err(PathError::Network(shown()));
                    }
                    _ => return Err(PathError::Device(shown())),
                },
                Component::RootDir => out.push(r"\"),
                Component::Normal(n) => {
                    let Some(name) = n.to_str() else {
                        return Err(PathError::Invalid(format!("non-Unicode name: {}", shown())));
                    };
                    check_windows_component(name, &shown())?;
                    out.push(n);
                }
                Component::CurDir | Component::ParentDir => {
                    return Err(PathError::Invalid(shown()));
                }
            }
        }
        check_drive(&out, shown)?;
        Ok(out)
    }
    #[cfg(not(windows))]
    {
        let _ = shown;
        Ok(canon)
    }
}

#[cfg(windows)]
thread_local! {
    /// Буквы локальных томов на время одного разрешения ([`DriveScope`], N1): тип диска
    /// проверяется заново на каждый вызов `resolve`, `Roots::canonical`, `open_checked`.
    /// Буква, бывшая локальной (флешка), а затем подключённая как сетевой диск, не
    /// проходит по устаревшему списку.
    static LOCAL_DRIVES: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
    /// Подменённый перечень томов (тесты; [`set_local_drives_for_tests`]).
    static DRIVES_OVERRIDE: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
}

/// Подменяет перечень локальных томов в этом потоке (`Some(буквы)`), `None` — настоящий
/// перечень. Только для тестов: так моделируется буква, переставшая быть локальной.
#[cfg(windows)]
#[doc(hidden)]
pub fn set_local_drives_for_tests(letters: Option<Vec<u8>>) {
    DRIVES_OVERRIDE.with(|o| *o.borrow_mut() = letters);
}

thread_local! {
    /// Глубина вложенных [`DriveScope`] этого потока.
    static DRIVE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Область кэша перечня локальных томов (N1, Ruling I5 задачи 6). Каждый публичный вызов
/// модуля входит в свою область. Внешняя область сбрасывает кэш при входе и выходе, а
/// вложенные его не трогают. Поэтому перечень живёт не дольше самой внешней области: одного
/// вызова, а у обхода `search` и `glob` — одного обхода, который держит область сам
/// ([`DriveScope::enter`]). Область привязана к потоку и не передаётся между потоками.
pub struct DriveScope {
    _not_send: std::marker::PhantomData<*const ()>,
}

impl DriveScope {
    /// Вход в область; перечень томов строится при первой проверке диска внутри неё.
    pub fn enter() -> Self {
        DRIVE_DEPTH.with(|d| {
            if d.get() == 0 {
                #[cfg(windows)]
                LOCAL_DRIVES.with(|c| *c.borrow_mut() = None);
            }
            d.set(d.get() + 1);
        });
        Self {
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for DriveScope {
    fn drop(&mut self) {
        DRIVE_DEPTH.with(|d| {
            d.set(d.get().saturating_sub(1));
            if d.get() == 0 {
                #[cfg(windows)]
                LOCAL_DRIVES.with(|c| *c.borrow_mut() = None);
            }
        });
    }
}

/// Перечисляет локальные тома (`FindFirstVolumeW` и `GetDriveTypeW` внутри `sysinfo`,
/// без `unsafe` у нас). Сетевые диски и `subst` томами не являются и в список не входят.
#[cfg(windows)]
fn list_local_drives() -> Vec<u8> {
    let disks = match sysinfo::Disks::new_with_refreshed_list_specifics(
        sysinfo::DiskRefreshKind::nothing(),
    ) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "локальные тома не перечислены");
            return Vec::new();
        }
    };
    (&disks)
        .into_iter()
        .filter_map(|d| {
            let mut c = d.mount_point().components();
            match (c.next(), c.next(), c.next()) {
                (Some(Component::Prefix(p)), Some(Component::RootDir), None) => match p.kind() {
                    Prefix::Disk(l) | Prefix::VerbatimDisk(l) => Some(l.to_ascii_uppercase()),
                    _ => None,
                },
                _ => None,
            }
        })
        .collect()
}

/// Диск пути — локальный том (Ruling C1): подключённый сетевой диск и `subst` — отказ
/// по типу диска, без обращения к нему (ни `canonicalize`, ни метаданных).
#[cfg(windows)]
fn check_drive(p: &Path, shown: &dyn Fn() -> String) -> Result<(), PathError> {
    let letter = match p.components().next() {
        Some(Component::Prefix(px)) => match px.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => d.to_ascii_uppercase(),
            Prefix::UNC(..) | Prefix::VerbatimUNC(..) => return Err(PathError::Network(shown())),
            _ => return Err(PathError::Device(shown())),
        },
        _ => return Err(PathError::Invalid(shown())),
    };
    let local = LOCAL_DRIVES.with(|c| {
        let mut c = c.borrow_mut();
        let list = c.get_or_insert_with(|| {
            DRIVES_OVERRIDE
                .with(|o| o.borrow().clone())
                .unwrap_or_else(list_local_drives)
        });
        list.contains(&letter)
    });
    if local {
        Ok(())
    } else {
        Err(PathError::Network(format!(
            "drive {}: is not a local volume: {}",
            char::from(letter),
            shown()
        )))
    }
}

/// Цель ссылки (`read_link`) по тем же правилам, что и аргумент модели ([`classify`]):
/// UNC, `\\?\UNC\`, `\??\UNC\`, device-пути, ADS и прочее — отказ без обращения к цели.
/// Относительная цель берётся от каталога ссылки `dir`.
fn link_target(
    dir: &Path,
    target: &Path,
    shown: &dyn Fn() -> String,
) -> Result<PathBuf, PathError> {
    let Some(t) = target.to_str() else {
        return Err(PathError::Invalid(format!(
            "non-Unicode link target: {}",
            shown()
        )));
    };
    let lex = classify(dir, t).map_err(|e| match e {
        PathError::Network(_) => PathError::Network(format!("link target {t}: {}", shown())),
        PathError::Device(_) => PathError::Device(format!("link target {t}: {}", shown())),
        other => PathError::Invalid(format!("link target {t} ({other}): {}", shown())),
    })?;
    Ok(lex.0)
}

/// Разрешение по компонентам (Ruling C1). Каждый существующий компонент читается
/// `symlink_metadata` — он не следует по ссылке; у ссылки и junction цель читается
/// `read_link` (локальные метаданные самой ссылки) и проверяется [`link_target`] до любого
/// обращения к ней; затем цель разрешается так же. Итог — `(существующий префикс без
/// ссылок, несуществующий хвост)`. Ссылка на несуществующую цель — `dangling link`.
fn walk(path: &Path, hops: &mut u32) -> Result<(PathBuf, Vec<std::ffi::OsString>), PathError> {
    let shown = || path.display().to_string();
    let mut cur = PathBuf::new();
    let mut names = Vec::new();
    for c in path.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => cur.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => names.push(std::ffi::OsString::from("..")),
            Component::Normal(n) => names.push(n.to_owned()),
        }
    }
    #[cfg(windows)]
    check_drive(&cur, &shown)?;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    for name in names {
        if !tail.is_empty() {
            // `..` после несуществующего компонента снимает его лексически (N3): на Unix
            // `std::path::absolute` не нормализует `..`, и `ZA_HOME=/u/missing/../zah`
            // иначе остался бы с `..` и не защищал бы настоящий `/u/zah`.
            if name == ".." {
                tail.pop();
            } else {
                tail.push(name);
            }
            continue;
        }
        if name == ".." {
            cur.pop();
            continue;
        }
        let next = cur.join(&name);
        match std::fs::symlink_metadata(&next) {
            Ok(m) if m.file_type().is_symlink() => {
                *hops += 1;
                if *hops > MAX_LINKS {
                    return Err(PathError::Invalid(format!("too many links: {}", shown())));
                }
                let target = std::fs::read_link(&next)
                    .map_err(|e| PathError::Invalid(format!("{}: {e}", shown())))?;
                let abs = link_target(&cur, &target, &shown)?;
                let (real, rest) = walk(&abs, hops)?;
                if !rest.is_empty() {
                    return Err(PathError::Invalid(format!("dangling link: {}", shown())));
                }
                cur = real;
            }
            Ok(_) => cur = next,
            Err(e) if is_missing(&e) => tail.push(name),
            Err(e) => return Err(PathError::Invalid(format!("{}: {e}", shown()))),
        }
    }
    Ok((cur, tail))
}

/// Канонический путь: цепочка ссылок проверена [`walk`], существующий префикс (уже без
/// ссылок, на локальном диске) канонизирован — он приводит короткие имена 8.3 и регистр
/// к настоящим именам; хвост дописан литерально.
fn canonical_prefix(path: &Path) -> Result<PathBuf, PathError> {
    let shown = || path.display().to_string();
    let (existing, tail) = walk(path, &mut 0)?;
    let canon = canonicalize_local(&existing)
        .map_err(|e| PathError::Invalid(format!("{}: {e}", shown())))?;
    let mut out = strip_canonical(canon, &shown)?;
    for name in tail {
        out.push(name);
    }
    Ok(out)
}

/// Компонента пути нет: `NotFound`, а на Unix ещё «не каталог» (`ENOTDIR`).
fn is_missing(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::NotADirectory
}

/// Разрешение пути (D76, D79). Блокирующая: вызывать только в `spawn_blocking`.
///
/// `inside` — путь внутри канонического корня проекта, сравнение ординальное (I1);
/// `protected` — нативная запись (`Access::Write`) в `ZA_HOME`, `<project>/.zero-agent/`
/// кроме `.zero-agent/work/` или `<project>/.git/`. Защищённые места сравниваются на
/// Windows без учёта регистра (ошибка такого сравнения — в безопасную сторону: ещё не
/// созданный `.ZERO-AGENT` — тот же каталог NTFS). Сверяются канонические пути, поэтому
/// защита действует и через ссылку, junction, короткое имя 8.3 и другой регистр.
pub fn resolve(roots: &Roots, lex: &LexPath, access: Access) -> Result<ResolvedPath, PathError> {
    let _drives = DriveScope::enter();
    let path = canonical_prefix(lex.as_path())?;
    let inside = within(&roots.project, &path);
    let protected = access == Access::Write && protected_place(roots, &path);
    Ok(ResolvedPath {
        path,
        inside,
        protected,
    })
}

/// Канонический путь `p` лежит в защищённом месте D79: `ZA_HOME`, `<project>/.zero-agent/`
/// кроме `.zero-agent/work/`, `<project>/.git/`. Чистая функция; на Windows без учёта
/// регистра (ошибка — в безопасную сторону). Её же повторно применяет политика (D80).
pub fn protected_place(roots: &Roots, p: &Path) -> bool {
    let za = roots.project.join(ZA_DIR);
    in_za_home(roots, p)
        || (within_with(&za, p, cfg!(windows)) && !within(&za.join(WORK), p))
        || within_with(&roots.project.join(GIT_DIR), p, cfg!(windows))
}

/// Канонический путь `p` лежит в `ZA_HOME` агента; на Windows без учёта регистра.
pub fn in_za_home(roots: &Roots, p: &Path) -> bool {
    within_with(&roots.za_home, p, cfg!(windows))
}

/// Компоненты равны: буква диска (`\\?\C:` ≡ `C:`) — без учёта ASCII-регистра, прочее —
/// ординально, а при `fold` — после `to_lowercase` (шире равенства имён NTFS).
fn comp_eq(a: Component<'_>, b: Component<'_>, fold: bool) -> bool {
    let disk = |c: Component<'_>| match c {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => Some(d.to_ascii_lowercase()),
            _ => None,
        },
        _ => None,
    };
    match (disk(a), disk(b)) {
        (Some(x), Some(y)) => return x == y,
        (Some(_), None) | (None, Some(_)) => return false,
        (None, None) => {}
    }
    if fold {
        a.as_os_str().to_string_lossy().to_lowercase()
            == b.as_os_str().to_string_lossy().to_lowercase()
    } else {
        a.as_os_str() == b.as_os_str()
    }
}

fn within_with(root: &Path, p: &Path, fold: bool) -> bool {
    let mut pc = p.components();
    root.components()
        .all(|rc| pc.next().is_some_and(|c| comp_eq(c, rc, fold)))
}

/// `p` лежит в `root` или равен ему — по компонентам (`/data` не содержит `/data2`).
/// Сравнение ординальное, без Unicode-свёртки регистра (Ruling I1: `K` (U+212A) и `k`
/// — разные имена NTFS); без учёта регистра — только буква диска. Оба пути должны быть
/// каноническими: регистр существующих компонентов берётся с диска.
pub fn within(root: &Path, p: &Path) -> bool {
    within_with(root, p, false)
}

/// Путь для человека и модели: внутри `root` — относительный через `/` (`.` для самого
/// корня), снаружи — абсолютный.
pub fn display(root: &Path, p: &Path) -> String {
    if !within(root, p) {
        return p.display().to_string();
    }
    let rel: Vec<String> = p
        .components()
        .skip(root.components().count())
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if rel.is_empty() {
        ".".to_owned()
    } else {
        rel.join("/")
    }
}

/// Рабочий каталог агента `<root>/.zero-agent/work/<sub>` (D76; SEC-004, ARC-021).
/// Блокирующая. `sub` — имена через `/` (`spill`, `tmp/<run>`), без `.`, `..`, `\` и `:`.
///
/// Каталоги создаются по компонентам; ни один не может быть символической ссылкой или
/// junction, канонический путь итога — внутри канонического корня; в
/// `.zero-agent/work/` пишется `.gitignore` со строкой `*` (не через ссылку). Возвращает
/// путь от `root`.
pub fn work_dir(root: &Path, sub: &str) -> io::Result<PathBuf> {
    let parts = sub_parts(sub)?;
    let mut dir = root.to_path_buf();
    for (i, part) in [ZA_DIR, WORK]
        .into_iter()
        .chain(parts.iter().copied())
        .enumerate()
    {
        dir.push(part);
        make_dir(&dir)?;
        if i == 1 {
            write_gitignore(&dir.join(".gitignore"))?;
        }
    }
    let canon_root = std::fs::canonicalize(root)?;
    let real = std::fs::canonicalize(&dir)?;
    if !within(&canon_root, &real) {
        return Err(io::Error::other(format!(
            "рабочий каталог {} вне проекта {}",
            real.display(),
            canon_root.display()
        )));
    }
    Ok(dir)
}

/// Каталог агента внутри `ZA_HOME`: `<home>/<sub>` (например, реестр групп процессов
/// `projects/<project_id>/procs`, R1-C1 D84). Блокирующая. `sub` — как у [`work_dir`].
///
/// `home` должен существовать; компоненты под ним создаются по одному, ни один не может
/// быть символической ссылкой или junction, канонический путь итога — внутри
/// канонического `home`. Возвращает путь от `home`.
pub fn home_dir(home: &Path, sub: &str) -> io::Result<PathBuf> {
    home_dir_with(home, sub, false)
}

/// [`home_dir`] для закрытых каталогов агента (реестр групп процессов, маркеры правок;
/// ревью задачи 8, I1): на Unix новые компоненты создаются с правами 0700 при любом
/// umask, а у последнего уже существующего снимаются права группы и прочих. Чужой
/// каталог так не исправить: его отвергнет проверка вызывающего
/// (`za_jail::check_registry`). На прочих ОС — как [`home_dir`].
pub fn private_home_dir(home: &Path, sub: &str) -> io::Result<PathBuf> {
    let dir = home_dir_with(home, sub, true)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = std::fs::symlink_metadata(&dir)?;
        if m.is_dir() && m.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(dir)
}

/// Создаёт пустой файл с правами 0600 (Unix) без перехода по ссылке на последнем
/// компоненте (`O_NOFOLLOW`, `create_new`); уже есть — `Ok` (его проверит
/// [`open_checked`]). Временные файлы правки (m4 задачи 7). Блокирующая.
#[cfg(unix)]
pub fn create_private_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

fn home_dir_with(home: &Path, sub: &str, private: bool) -> io::Result<PathBuf> {
    let parts = sub_parts(sub)?;
    let mut dir = home.to_path_buf();
    for part in parts {
        dir.push(part);
        make_dir_with(&dir, private)?;
    }
    let canon_home = std::fs::canonicalize(home)?;
    let real = std::fs::canonicalize(&dir)?;
    if !within(&canon_home, &real) {
        return Err(io::Error::other(format!(
            "каталог {} вне ZA_HOME {}",
            real.display(),
            canon_home.display()
        )));
    }
    Ok(dir)
}

/// Компоненты имени рабочего каталога: через `/`, без пустых, `.`, `..`, `\`, `:`, NUL.
fn sub_parts(sub: &str) -> io::Result<Vec<&str>> {
    let parts: Vec<&str> = sub.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains(['\\', ':', '\0']))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("недопустимое имя рабочего каталога: {sub:?}"),
        ));
    }
    Ok(parts)
}

/// Создаёт каталог, если его нет; путь не ссылка и не junction.
fn make_dir(dir: &Path) -> io::Result<()> {
    make_dir_with(dir, false)
}

/// [`make_dir`]; `private` — новый каталог 0700 на Unix (umask права не расширит).
fn make_dir_with(dir: &Path, private: bool) -> io::Result<()> {
    not_link(dir)?;
    #[cfg(unix)]
    let b = {
        use std::os::unix::fs::DirBuilderExt;
        let mut b = std::fs::DirBuilder::new();
        if private {
            b.mode(0o700);
        }
        b
    };
    #[cfg(not(unix))]
    let b = {
        let _ = private;
        std::fs::DirBuilder::new()
    };
    match b.create(dir) {
        Ok(()) => Ok(()),
        // Уже есть (или создан параллельно) — проверить ещё раз.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            not_link(dir)?;
            if dir.is_dir() {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("путь рабочего каталога — не каталог: {}", dir.display()),
                ))
            }
        }
        Err(e) => Err(e),
    }
}

/// Путь — не символическая ссылка и не junction (на Windows junction — тоже ссылка у
/// `symlink_metadata`); отсутствующий путь допустим.
fn not_link(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(io::Error::other(format!(
            "путь рабочего каталога — символическая ссылка или junction: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// `.gitignore` = `*\n`: переписывается, только если отличается; не через ссылку.
fn write_gitignore(path: &Path) -> io::Result<()> {
    not_link(path)?;
    if std::fs::read(path).ok().as_deref() == Some(WORK_GITIGNORE.as_bytes()) {
        return Ok(());
    }
    atomic_write(path, WORK_GITIGNORE.as_bytes())
}

/// Порядковый номер временного файла в процессе: одновременные записи не делят `.tmp`.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Запись через временный файл рядом (`create_new`: чужая ссылка на его месте — ошибка)
/// и переименование; сбой удаляет временный файл.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}-{n}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let mut created = false;
    let r = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| {
            created = true;
            f.write_all(bytes)
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if r.is_err() && created {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Метаданные разрешённого пути без перехода по ссылке на последнем компоненте
/// (`symlink_metadata`). Блокирующая. Ссылка или junction — отказ `refusing to follow a
/// link`. На Windows диск пути должен быть локальным томом (Ruling C1). Это нужно для
/// проверок «каталог или файл» у инструментов вместо `std::fs::metadata`, который идёт по
/// ссылке (I3 задачи 6). Записанный риск — тот же, что у [`open_checked`]: подмена
/// промежуточного каталога после [`resolve`].
pub fn stat_checked(r: &ResolvedPath) -> io::Result<std::fs::Metadata> {
    let _drives = DriveScope::enter();
    #[cfg(windows)]
    check_drive(&r.path, &|| r.path.display().to_string()).map_err(io::Error::other)?;
    let m = std::fs::symlink_metadata(&r.path)?;
    if m.file_type().is_symlink() {
        return Err(refuse_link(&r.path));
    }
    Ok(m)
}

/// Имена записей каталога `r` (блокирующая). Сам каталог проходит [`stat_checked`]:
/// ссылка на каталог — отказ, файл — `NotADirectory`.
pub fn read_dir_checked(r: &ResolvedPath) -> io::Result<Vec<std::ffi::OsString>> {
    let _drives = DriveScope::enter();
    if !stat_checked(r)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("not a directory: {}", r.path.display()),
        ));
    }
    Ok(std::fs::read_dir(&r.path)?
        .flatten()
        .map(|e| e.file_name())
        .collect())
}

/// Текст отказа открыть не обычный файл (FIFO, устройство, сокет, каталог).
const NOT_REGULAR: &str = "not a regular file";
/// Текст отказа писать в файл с несколькими жёсткими ссылками.
const HARD_LINKS: &str = "refusing to write a file with several hard links";

/// Безопасное открытие файла (D98). Блокирующая.
///
/// `write = false` — только чтение; `write = true` — чтение и запись, файл **не**
/// усекается (усечение — дело вызывающего), а если его нет — создаётся (`create_new`).
///
/// 1. Последний компонент — ссылка или junction → отказ `refusing to follow a link`.
///    Открытие тоже не следует по ней: Unix — `O_NOFOLLOW`, Windows —
///    `FILE_FLAG_OPEN_REPARSE_POINT`, и открытая ссылка — тот же отказ.
/// 2. Открыт должен быть обычный файл (по дескриптору): FIFO, устройство, сокет, каталог —
///    отказ `not a regular file`. На Unix открытие идёт с `O_NONBLOCK`, поэтому FIFO без
///    писателя не блокирует поток (I4).
/// 3. Запись в файл с `nlink > 1` — отказ (I3): жёсткая ссылка внутри проекта может вести
///    к файлу снаружи или к `ZA_HOME/config.toml`, а путь и дескриптор при этом честные.
///    Чтение жёсткой ссылки разрешено (D98, записанный риск 7.12).
/// 4. Путь канонизируется ещё раз и должен совпасть с `r.path`, а дескриптор — быть тем
///    же файлом (`same_file::Handle`). Иначе (промежуточный каталог или файл подменили
///    после [`resolve`]) — отказ `path changed while opening`; файл, созданный этим
///    вызовом по подменённому пути, удаляется (M1 ревью).
///
/// Записанные риски: подмена промежуточного каталога ссылкой на UNC между [`resolve`] и
/// открытием (открытие пойдёт по ней, Windows не даёт `openat`), подмена между сверкой и
/// использованием процессом вне Job или группы — до SEC-100.
pub fn open_checked(r: &ResolvedPath, write: bool) -> io::Result<File> {
    let _drives = DriveScope::enter();
    if std::fs::symlink_metadata(&r.path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(refuse_link(&r.path));
    }
    let open = |create: bool| {
        let mut o = std::fs::OpenOptions::new();
        o.read(true);
        if write {
            o.write(true);
            if create {
                o.create_new(true);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            o.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        match o.open(&r.path) {
            // `O_NOFOLLOW` на ссылке — `ELOOP`.
            #[cfg(unix)]
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => Err(refuse_link(&r.path)),
            other => other,
        }
    };
    let (f, created) = match open(false) {
        Ok(f) => (f, false),
        Err(e) if write && e.kind() == io::ErrorKind::NotFound => (open(true)?, true),
        Err(e) => return Err(e),
    };
    match verify_open(r, &f, write) {
        Ok(()) => Ok(f),
        Err((e, actual)) => {
            if created && let Some(actual) = actual {
                // Удаляется только тот файл, что создан этим вызовом.
                let ours = same_file::Handle::from_file(f.try_clone()?)?;
                if same_file::Handle::from_path(&actual).is_ok_and(|h| h == ours) {
                    drop(f);
                    if let Err(rm) = std::fs::remove_file(&actual) {
                        tracing::warn!(path = %actual.display(), error = %rm, "созданный файл не удалён");
                    }
                }
            }
            Err(e)
        }
    }
}

/// Проверки открытого дескриптора (шаги 1–4 [`open_checked`]). При ошибке сверки пути
/// возвращает и фактический канонический путь открытого файла.
fn verify_open(
    r: &ResolvedPath,
    f: &File,
    write: bool,
) -> Result<(), (io::Error, Option<PathBuf>)> {
    let plain = |e: io::Error| (e, None);
    let meta = f.metadata().map_err(plain)?;
    if meta.file_type().is_symlink() {
        return Err(plain(refuse_link(&r.path)));
    }
    if !meta.is_file() {
        return Err(plain(io::Error::other(format!(
            "{NOT_REGULAR}: {}",
            r.path.display()
        ))));
    }
    if write && hard_links(f, &meta).map_err(plain)? > 1 {
        return Err(plain(io::Error::other(format!(
            "{HARD_LINKS}: {}",
            r.path.display()
        ))));
    }
    let changed = |actual: Option<PathBuf>| {
        tracing::error!(path = %r.path.display(), "путь подменён между разрешением и открытием");
        (
            io::Error::other(format!("{PATH_CHANGED}: {}", r.path.display())),
            actual,
        )
    };
    let shown = || r.path.display().to_string();
    let canon = canonicalize_local(&r.path).map_err(plain)?;
    let now = match strip_canonical(canon, &shown) {
        Ok(p) => p,
        Err(_) => return Err(changed(None)),
    };
    if !same_path(&now, &r.path) {
        return Err(changed(Some(now)));
    }
    let opened = same_file::Handle::from_file(f.try_clone().map_err(plain)?).map_err(plain)?;
    if opened != same_file::Handle::from_path(&now).map_err(plain)? {
        return Err(changed(None));
    }
    Ok(())
}

/// Число жёстких ссылок открытого файла.
#[cfg(unix)]
fn hard_links(_f: &File, meta: &std::fs::Metadata) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(meta.nlink())
}

/// Число жёстких ссылок открытого файла (`GetFileInformationByHandle` через
/// `winapi-util`, без `unsafe` у нас).
#[cfg(windows)]
fn hard_links(f: &File, _meta: &std::fs::Metadata) -> io::Result<u64> {
    Ok(winapi_util::file::information(f)?.number_of_links())
}

fn refuse_link(p: &Path) -> io::Error {
    io::Error::other(format!("{REFUSE_LINK}: {}", p.display()))
}

/// Один и тот же путь по компонентам (ординально, кроме буквы диска).
fn same_path(a: &Path, b: &Path) -> bool {
    a.components().count() == b.components().count() && within(a, b)
}

/// Текст отказа при подмене цели между проверкой и заменой.
const PATH_CHANGED_WRITE: &str = "path changed while writing";
/// Сколько раз повторяется `rename` занятого файла на Windows (D88, Ruling 5.29).
#[cfg(windows)]
const RENAME_RETRIES: u32 = 3;
/// Пауза между повторами `rename` на Windows.
#[cfg(windows)]
const RENAME_PAUSE: std::time::Duration = std::time::Duration::from_millis(50);

/// Атомарная замена файла `target` готовым файлом `tmp` (D88, D98). Блокирующая.
///
/// `tmp` и `target` должны лежать на одном томе: иначе `rename` не атомарен и даёт
/// `ErrorKind::CrossesDevices` — тогда вызывающий кладёт временный файл рядом с целью.
/// 1. `tmp` — обычный файл, не ссылка, одна жёсткая ссылка.
/// 2. `target`, если есть, — не ссылка и не junction (ссылка не подменяется файлом,
///    Ruling 7.14), обычный файл с одной жёсткой ссылкой (I3).
/// 3. `rename`; на Windows занятый файл (отказ в доступе, нарушение совместного доступа) —
///    до трёх повторов с паузой 50 мс.
/// 4. После `rename` цель канонизируется ещё раз и должна совпасть с `target.path`, а файл
///    по ней — быть тем самым `tmp`. Расхождение — `error!` и отказ `path changed while
///    writing`.
///
/// Записанный риск — тот же, что у [`open_checked`]: подмена между проверкой и `rename`
/// процессом вне Job или группы (до SEC-100).
pub fn replace_checked(tmp: &ResolvedPath, target: &ResolvedPath) -> io::Result<()> {
    let _drives = DriveScope::enter();
    #[cfg(windows)]
    check_drive(&target.path, &|| target.path.display().to_string()).map_err(io::Error::other)?;
    let t = open_checked(tmp, false)?;
    if hard_links(&t, &t.metadata()?)? > 1 {
        return Err(io::Error::other(format!(
            "{HARD_LINKS}: {}",
            tmp.path.display()
        )));
    }
    let ours = same_file::Handle::from_file(t)?;
    match open_checked(target, false) {
        Ok(f) => {
            if hard_links(&f, &f.metadata()?)? > 1 {
                return Err(io::Error::other(format!(
                    "{HARD_LINKS}: {}",
                    target.path.display()
                )));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    rename_retry(&tmp.path, &target.path)?;
    let shown = || target.path.display().to_string();
    let now = canonicalize_local(&target.path)
        .and_then(|c| strip_canonical(c, &shown).map_err(io::Error::other));
    let same = match &now {
        Ok(p) => {
            same_path(p, &target.path) && same_file::Handle::from_path(p).is_ok_and(|h| h == ours)
        }
        Err(_) => false,
    };
    if !same {
        tracing::error!(path = %target.path.display(), "цель подменена во время замены файла");
        return Err(io::Error::other(format!(
            "{PATH_CHANGED_WRITE}: {}",
            target.path.display()
        )));
    }
    Ok(())
}

fn rename_retry(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let mut tries = 0;
        loop {
            match std::fs::rename(from, to) {
                Ok(()) => return Ok(()),
                // Отказ в доступе (5), нарушение совместного доступа (32) или блокировки
                // (33): файл держит другой процесс (антивирус, индексатор, редактор).
                Err(e)
                    if tries < RENAME_RETRIES
                        && (e.kind() == io::ErrorKind::PermissionDenied
                            || matches!(e.raw_os_error(), Some(5 | 32 | 33))) =>
                {
                    tries += 1;
                    std::thread::sleep(RENAME_PAUSE);
                }
                Err(e) => return Err(e),
            }
        }
    }
    #[cfg(not(windows))]
    std::fs::rename(from, to)
}

/// Удаление файла `r` (блокирующая): только обычный файл и не ссылка (ссылка — отказ,
/// её цель не трогается).
pub fn remove_checked(r: &ResolvedPath) -> io::Result<()> {
    let _drives = DriveScope::enter();
    let m = stat_checked(r)?;
    if !m.is_file() {
        return Err(io::Error::other(format!(
            "{NOT_REGULAR}: {}",
            r.path.display()
        )));
    }
    std::fs::remove_file(&r.path)
}

/// Создаёт недостающие каталоги над файлом `r` (блокирующая) по одному, как
/// [`work_dir`]: ни один не может оказаться ссылкой или junction. Итоговый каталог
/// канонизируется и должен совпасть с родителем `r.path`, иначе — отказ `path changed
/// while writing`.
pub fn create_parents_checked(r: &ResolvedPath) -> io::Result<()> {
    let _drives = DriveScope::enter();
    let Some(parent) = r.path.parent() else {
        return Ok(());
    };
    #[cfg(windows)]
    check_drive(parent, &|| parent.display().to_string()).map_err(io::Error::other)?;
    let mut missing = Vec::new();
    let mut cur = parent;
    loop {
        match std::fs::symlink_metadata(cur) {
            Ok(m) if m.file_type().is_symlink() => return Err(refuse_link(cur)),
            Ok(m) if m.is_dir() => break,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("not a directory: {}", cur.display()),
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                missing.push(cur);
                match cur.parent() {
                    Some(p) => cur = p,
                    None => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
    for dir in missing.into_iter().rev() {
        make_dir(dir)?;
    }
    let shown = || parent.display().to_string();
    let now = strip_canonical(canonicalize_local(parent)?, &shown).map_err(io::Error::other)?;
    if !same_path(&now, parent) {
        tracing::error!(path = %parent.display(), "каталог подменён при создании");
        return Err(io::Error::other(format!(
            "{PATH_CHANGED_WRITE}: {}",
            parent.display()
        )));
    }
    Ok(())
}
