//! Цели восстановления и запись (D79, D98; ревью задачи 9: I3, M6).
//!
//! Цель проверяется до первой записи ([`target`]): путь проходит `classify` и `resolve`,
//! лежит внутри проекта, совпадает со своим лексическим путём (ни один компонент не ссылка
//! и не junction), это обычный файл или его нет. Защищённые места пропускаются.
//! Содержимое пишется потоком во временный файл ([`Tmp`]) в `work_dir("checkpoints")`,
//! затем сравнивается с текущим файлом потоком ([`finish`]): совпадает — временный файл
//! удаляется; иначе права, группа (Unix) и [`replace_checked`].

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use za_core::Access;
use za_core::paths::{
    ResolvedPath, Roots, classify, create_parents_checked, open_checked, remove_checked,
    replace_checked, resolve, stat_checked, within, work_dir,
};

use super::home::{CHECKPOINTS_DIR, create_private, is_missing};

/// Служебный каталог проекта: целиком не восстанавливается (D79, Ruling 2 задачи 7).
const ZA_DIR: &str = ".zero-agent";
/// Кусок потокового копирования и сравнения.
const CHUNK: usize = 256 * 1024;

/// Проверенная цель восстановления.
#[derive(Clone)]
pub(super) struct Target {
    pub r: ResolvedPath,
    pub exists: bool,
}

/// Цель восстановления `rel`: внутри проекта, без ссылок на пути, обычный файл или нет
/// файла. Защищённое место — `None` и предупреждение; прочее — ошибка.
pub(super) fn target(roots: &Roots, rel: &str) -> io::Result<Option<Target>> {
    let lex = classify(&roots.project, rel).map_err(|e| io::Error::other(format!("{rel}: {e}")))?;
    let r =
        resolve(roots, &lex, Access::Write).map_err(|e| io::Error::other(format!("{rel}: {e}")))?;
    if r.protected || within(&roots.project.join(ZA_DIR), &r.path) {
        tracing::warn!(path = rel, "защищённое место не восстанавливается");
        return Ok(None);
    }
    if !r.inside {
        return Err(io::Error::other(format!(
            "refusing to restore outside the project: {rel}"
        )));
    }
    // Ни один компонент не ссылка: канонический путь равен лексическому.
    let lexical = lex.as_path();
    if !(lexical.components().count() == r.path.components().count() && within(lexical, &r.path)) {
        return Err(io::Error::other(format!(
            "refusing to follow a link: {rel}"
        )));
    }
    let exists = match stat_checked(&r) {
        Ok(m) if m.is_file() => true,
        Ok(_) => return Err(io::Error::other(format!("not a regular file: {rel}"))),
        Err(e) if is_missing(&e) || e.kind() == io::ErrorKind::NotADirectory => false,
        Err(e) => return Err(e),
    };
    Ok(Some(Target { r, exists }))
}

/// Режим файла при записи (на Windows права Unix не применяются).
#[derive(Clone, Copy)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) enum Mode {
    /// Режим git: исполняемый или нет.
    Git(bool),
    /// Права Unix из манифеста копии.
    Unix(u32),
}

/// Временный файл восстановления внутри проекта (тот же том, что и цель).
pub(super) struct Tmp {
    pub r: ResolvedPath,
    pub file: File,
}

impl Tmp {
    /// `work_dir("checkpoints")/restore-<pid>-<n>.tmp`; каталог недоступен — рядом с целью.
    pub(super) fn new(root: &Path, t: &Target, n: usize) -> io::Result<Tmp> {
        let name = format!("restore-{}-{n}.tmp", std::process::id());
        let path = match work_dir(root, CHECKPOINTS_DIR) {
            Ok(dir) => dir.join(name),
            Err(e) => {
                tracing::warn!(error = %e, "каталог временных файлов восстановления недоступен");
                sibling(t, n)?
            }
        };
        let r = ResolvedPath {
            path,
            inside: true,
            protected: false,
        };
        let file = create_private(&r)?;
        Ok(Tmp { r, file })
    }

    /// Удаляет временный файл; ошибка — `warn`.
    pub(super) fn discard(self) {
        drop(self.file);
        discard(&self.r);
    }

    /// Открытый файл для записи потоком извне (stdout `git cat-file blob`).
    pub(super) fn handle(&self) -> io::Result<File> {
        self.file.try_clone()
    }
}

fn sibling(t: &Target, n: usize) -> io::Result<std::path::PathBuf> {
    let parent =
        t.r.path
            .parent()
            .ok_or_else(|| io::Error::other("цель без каталога"))?;
    Ok(parent.join(format!(".za-restore-{}-{n}.tmp", std::process::id())))
}

fn discard(r: &ResolvedPath) {
    match remove_checked(r) {
        Ok(()) => {}
        Err(e) if is_missing(&e) => {}
        Err(e) => tracing::warn!(path = %r.path.display(), error = %e, "временный файл не удалён"),
    }
}

/// Копирует `src` во временный файл потоком.
pub(super) fn fill(tmp: &mut Tmp, src: &mut impl Read) -> io::Result<()> {
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        tmp.file.write_all(&buf[..n])?;
    }
    Ok(())
}

/// Завершает запись цели содержимым `tmp`: совпадает с текущим (байты и бит исполнения) —
/// `false` и временный файл удаляется; иначе каталоги, права, `replace_checked` — `true`.
pub(super) fn finish(t: &Target, tmp: Tmp, mode: Option<Mode>, n: usize) -> io::Result<bool> {
    let Tmp { r, file } = tmp;
    let res = finish_inner(t, &r, file, mode, n);
    if !matches!(res, Ok(true)) {
        discard(&r);
    }
    res
}

fn finish_inner(
    t: &Target,
    tmp: &ResolvedPath,
    file: File,
    mode: Option<Mode>,
    n: usize,
) -> io::Result<bool> {
    file.sync_all()?;
    let old = if t.exists {
        let cur = open_checked(&t.r, false)?;
        let meta = cur.metadata()?;
        if same_content(tmp, cur)? && !mode_differs(&meta, mode) {
            return Ok(false);
        }
        Some(meta)
    } else {
        create_parents_checked(&t.r)?;
        None
    };
    // Сначала группа, потом права: `chown` не-root на Linux снимает setgid и setuid
    // (N9), поэтому права ставятся последними — как у `edit`.
    #[cfg(unix)]
    if let Some(m) = &old {
        use std::os::unix::fs::MetadataExt;
        if let Err(e) = std::os::unix::fs::fchown(&file, None, Some(m.gid())) {
            tracing::warn!(path = %t.r.path.display(), error = %e, "группа файла не сохранена");
        }
    }
    if let Some(p) = target_perms(old.as_ref(), mode) {
        file.set_permissions(p)?;
    }
    let perms = file.metadata()?.permissions();
    drop(file);
    match replace_checked(tmp, &t.r) {
        Ok(()) => Ok(true),
        // Другой том: копия рядом с целью и замена оттуда.
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            let side = ResolvedPath {
                path: sibling(t, n)?,
                inside: true,
                protected: false,
            };
            let r = (|| {
                let mut f = create_private(&side)?;
                let mut src = open_checked(tmp, false)?;
                io::copy(&mut src, &mut f)?;
                f.sync_all()?;
                f.set_permissions(perms)?;
                drop(f);
                replace_checked(&side, &t.r)
            })();
            if r.is_err() {
                discard(&side);
            }
            discard(tmp);
            r.map(|()| true)
        }
        Err(e) => Err(e),
    }
}

/// Потоковое сравнение временного файла с открытым текущим.
fn same_content(tmp: &ResolvedPath, mut cur: File) -> io::Result<bool> {
    let mut a = open_checked(tmp, false)?;
    if a.metadata()?.len() != cur.metadata()?.len() {
        return Ok(false);
    }
    let (mut ba, mut bb) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    loop {
        let n = read_full(&mut a, &mut ba)?;
        let m = read_full(&mut cur, &mut bb)?;
        if n != m || ba[..n] != bb[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        let n = f.read(&mut buf[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    Ok(got)
}

/// Права после записи: Unix — права цели с битом исполнения по git (`S_IXUSR`, M6), у
/// нового файла `0666 & !umask` (исполняемый — `0777 & !umask`); прочие ОС — права цели.
#[cfg(unix)]
fn target_perms(
    old: Option<&std::fs::Metadata>,
    mode: Option<Mode>,
) -> Option<std::fs::Permissions> {
    use std::os::unix::fs::PermissionsExt;
    let mask = crate::edit::umask();
    let exec_new = matches!(mode, Some(Mode::Git(true)));
    let base = old.map_or_else(
        || {
            if exec_new {
                0o777 & !mask
            } else {
                0o666 & !mask
            }
        },
        |m| m.permissions().mode() & 0o7777,
    );
    let m = match mode {
        Some(Mode::Unix(m)) => m,
        Some(Mode::Git(true)) => base | 0o100 | ((base & 0o044) >> 2),
        Some(Mode::Git(false)) => base & !0o111,
        None => base,
    };
    Some(std::fs::Permissions::from_mode(m))
}

#[cfg(not(unix))]
fn target_perms(
    old: Option<&std::fs::Metadata>,
    _mode: Option<Mode>,
) -> Option<std::fs::Permissions> {
    old.map(std::fs::Metadata::permissions)
}

#[cfg(unix)]
fn mode_differs(meta: &std::fs::Metadata, mode: Option<Mode>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let cur = meta.permissions().mode() & 0o7777;
    match mode {
        Some(Mode::Unix(m)) => cur != m,
        Some(Mode::Git(exec)) => (cur & 0o100 != 0) != exec,
        None => false,
    }
}

#[cfg(not(unix))]
fn mode_differs(_meta: &std::fs::Metadata, _mode: Option<Mode>) -> bool {
    false
}
