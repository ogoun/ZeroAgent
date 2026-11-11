//! Закрытые каталоги и файлы агента в `ZA_HOME/projects/<project_id>/checkpoints`.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use za_core::paths::{
    ResolvedPath, Roots, open_checked, private_home_dir, remove_checked, replace_checked,
};

use super::{join_rel, valid_id};
use crate::AgentHome;

/// Каталог агента в `ZA_HOME/projects/<project_id>/` и в `work_dir` проекта (временные
/// файлы восстановления).
pub(super) const CHECKPOINTS_DIR: &str = "checkpoints";

/// Закрытый каталог `ZA_HOME/projects/<project_id>/checkpoints[/<sub>]`.
pub(super) fn agent_dir(home: &AgentHome, sub: Option<&str>) -> io::Result<PathBuf> {
    let mut rel = format!("projects/{}/{CHECKPOINTS_DIR}", home.project_id);
    if let Some(s) = sub {
        rel.push('/');
        rel.push_str(s);
    }
    // Канонический `ZA_HOME`: `open_checked` сверяет путь с каноническим.
    let za_home = Roots::canonical(&home.za_home, &home.za_home)?.za_home;
    let dir = private_home_dir(&za_home, &rel)?;
    za_jail::check_registry(&dir)?;
    Ok(dir)
}

/// Каталог копии файлов чекпоинта `id` (`<run>/<step>-<seq>`); создаётся.
pub(super) fn files_dir(home: &AgentHome, id: &str) -> io::Result<PathBuf> {
    check_id(id)?;
    agent_dir(home, Some(id))
}

/// Каталог копии файлов чекпоинта `id` без создания (поиск, восстановление).
pub(super) fn files_dir_existing(home: &AgentHome, id: &str) -> io::Result<PathBuf> {
    check_id(id)?;
    Ok(join_rel(&agent_dir(home, None)?, id))
}

fn check_id(id: &str) -> io::Result<()> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("неверный id чекпоинта {id:?}"),
        ))
    }
}

/// Путь агента вне проекта (уже проверенный каталог агента).
pub(super) fn private(path: PathBuf) -> ResolvedPath {
    ResolvedPath {
        path,
        inside: false,
        protected: false,
    }
}

/// Создаёт и открывает на запись пустой закрытый файл (0600 на Unix, не по ссылке).
pub(super) fn create_private(path: &ResolvedPath) -> io::Result<std::fs::File> {
    #[cfg(unix)]
    za_core::paths::create_private_file(&path.path)?;
    let f = open_checked(path, true)?;
    f.set_len(0)?;
    Ok(f)
}

/// Атомарная запись закрытого файла агента: временный файл рядом и [`replace_checked`].
pub(super) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = private(path.with_file_name(format!(".{name}.{}.tmp", std::process::id())));
    let r = (|| {
        let mut f = create_private(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        replace_checked(&tmp, &private(path.to_path_buf()))
    })();
    if r.is_err() {
        let _ = remove_checked(&tmp);
    }
    r
}

/// Содержимое закрытого файла агента (обычный файл, не ссылка), не больше `max` байт.
pub(super) fn read_private(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let f = open_checked(&private(path.to_path_buf()), false)?;
    let mut bytes = Vec::new();
    (&f).take(max).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Подкаталог закрытого каталога агента (0700 на Unix), не ссылка.
pub(super) fn make_private_dir(dir: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(io::Error::other(format!("{} не каталог", dir.display()))),
        Err(e) if is_missing(&e) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new().mode(0o700).create(dir)
            }
            #[cfg(not(unix))]
            std::fs::create_dir(dir)
        }
        Err(e) => Err(e),
    }
}

pub(super) fn is_missing(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}
