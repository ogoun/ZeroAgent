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
    pub fn canonical(project: &Path, za_home: &Path) -> io::Result<Roots> {
        let project = strip_verbatim(std::fs::canonicalize(project)?);
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
/// зарезервированные имена (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`,
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
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
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

/// Снимает `\\?\` у дисковых путей (Ruling 7.10); остальное без изменений.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && drive_abs(rest)
        {
            return PathBuf::from(rest);
        }
    }
    p
}

/// Канонический путь: самый длинный существующий префикс канонизирован, хвост дописан.
/// Префикс — сетевой путь (подключённый диск) → `Network`; другой `\\?\` (том без
/// буквы) → `Device`. Висячая ссылка в пути → `Invalid`: её цель не канонизировать,
/// а запись через неё ушла бы туда, куда она указывает.
fn canonical_prefix(path: &Path) -> Result<PathBuf, PathError> {
    let shown = || path.display().to_string();
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let canon = loop {
        match std::fs::canonicalize(&existing) {
            Ok(c) => break c,
            Err(e) if is_missing(&e) => {
                if std::fs::symlink_metadata(&existing).is_ok() {
                    return Err(PathError::Invalid(format!("dangling link: {}", shown())));
                }
                let Some(name) = existing.file_name().map(ToOwned::to_owned) else {
                    return Err(PathError::Invalid(format!("{}: {e}", shown())));
                };
                tail.push(name);
                existing.pop();
            }
            Err(e) => return Err(PathError::Invalid(format!("{}: {e}", shown()))),
        }
    };
    let s = canon.to_string_lossy();
    let network = s.starts_with(r"\\?\UNC\") || (s.starts_with(r"\\") && !s.starts_with(r"\\?\"));
    if network {
        return Err(PathError::Network(shown()));
    }
    let mut out = strip_verbatim(canon);
    if out.to_string_lossy().starts_with(r"\\") {
        return Err(PathError::Device(shown()));
    }
    for name in tail.into_iter().rev() {
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
/// `inside` — путь внутри канонического корня проекта; `protected` — нативная запись
/// (`Access::Write`) в `ZA_HOME`, `<project>/.zero-agent/` кроме `.zero-agent/work/` или
/// `<project>/.git/`. Сверяются канонические пути, поэтому защита действует и через
/// ссылку, junction, короткое имя 8.3 и другой регистр.
pub fn resolve(roots: &Roots, lex: &LexPath, access: Access) -> Result<ResolvedPath, PathError> {
    let path = canonical_prefix(lex.as_path())?;
    let inside = within(&roots.project, &path);
    let za = roots.project.join(ZA_DIR);
    let protected_place = within(&roots.za_home, &path)
        || (within(&za, &path) && !within(&za.join(WORK), &path))
        || within(&roots.project.join(GIT_DIR), &path);
    Ok(ResolvedPath {
        path,
        inside,
        protected: access == Access::Write && protected_place,
    })
}

/// Ключ компонента для сравнения: `\\?\C:` и `C:` — один диск; на Windows без учёта
/// регистра.
fn comp_key(c: Component<'_>) -> String {
    if let Component::Prefix(p) = c
        && let Prefix::Disk(d) | Prefix::VerbatimDisk(d) = p.kind()
    {
        return format!("{}:", char::from(d.to_ascii_lowercase()));
    }
    let s = c.as_os_str().to_string_lossy();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.into_owned()
    }
}

/// `p` лежит в `root` или равен ему — по компонентам (`/data` не содержит `/data2`);
/// на Windows без учёта регистра. Пути сравниваются как есть: оба должны быть
/// каноническими.
pub fn within(root: &Path, p: &Path) -> bool {
    let mut pc = p.components();
    root.components()
        .all(|rc| pc.next().is_some_and(|c| comp_key(c) == comp_key(rc)))
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
    let bad = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("недопустимое имя рабочего каталога: {sub:?}"),
        )
    };
    let parts: Vec<&str> = sub.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains(['\\', ':', '\0']))
    {
        return Err(bad());
    }
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

/// Создаёт каталог, если его нет; путь не ссылка и не junction.
fn make_dir(dir: &Path) -> io::Result<()> {
    not_link(dir)?;
    match std::fs::create_dir(dir) {
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

/// Безопасное открытие файла (D98). Блокирующая.
///
/// `write = false` — только чтение; `write = true` — чтение и запись, файл создаётся,
/// если его нет, и **не** усекается (усечение — дело вызывающего).
///
/// 1. Последний компонент — ссылка или junction → отказ `refusing to follow a link`.
///    Открытие тоже не следует по ней: Unix — `O_NOFOLLOW`, Windows —
///    `FILE_FLAG_OPEN_REPARSE_POINT`, и открытая ссылка — тот же отказ.
/// 2. Затем путь канонизируется ещё раз и должен совпасть с `r.path`, а дескриптор —
///    быть тем же файлом, что и `r.path` (`same_file::Handle`). Иначе (промежуточный
///    каталог или файл подменили после [`resolve`]) — отказ `path changed while opening`.
///
/// Записанные риски (D98): жёсткие ссылки на чтение; подмена между сверкой и
/// использованием процессом вне Job или группы — до SEC-100.
pub fn open_checked(r: &ResolvedPath, write: bool) -> io::Result<File> {
    if std::fs::symlink_metadata(&r.path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(refuse_link(&r.path));
    }
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    if write {
        o.write(true).create(true).truncate(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        o.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let f = match o.open(&r.path) {
        Ok(f) => f,
        // `O_NOFOLLOW` на ссылке — `ELOOP`.
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(refuse_link(&r.path)),
        Err(e) => return Err(e),
    };
    if f.metadata()?.file_type().is_symlink() {
        return Err(refuse_link(&r.path));
    }
    let changed = || {
        tracing::error!(path = %r.path.display(), "путь подменён между разрешением и открытием");
        io::Error::other(format!("{PATH_CHANGED}: {}", r.path.display()))
    };
    let now = strip_verbatim(std::fs::canonicalize(&r.path)?);
    if !same_path(&now, &r.path) {
        return Err(changed());
    }
    let opened = same_file::Handle::from_file(f.try_clone()?)?;
    if opened != same_file::Handle::from_path(&now)? {
        return Err(changed());
    }
    Ok(f)
}

fn refuse_link(p: &Path) -> io::Error {
    io::Error::other(format!("{REFUSE_LINK}: {}", p.display()))
}

/// Один и тот же путь по компонентам (на Windows без учёта регистра).
fn same_path(a: &Path, b: &Path) -> bool {
    a.components().count() == b.components().count() && within(a, b)
}
