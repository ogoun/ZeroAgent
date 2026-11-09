//! Каталог данных агента `ZA_HOME` и идентификатор проекта (ARC-020).
//!
//! Журнал проекта лежит вне каталога проекта:
//! `ZA_HOME/projects/<project_id>/journal.db`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Переменная окружения, переопределяющая каталог данных.
pub const ZA_HOME_ENV: &str = "ZA_HOME";
/// Имя каталога агента внутри платформенного каталога данных.
const APP_DIR: &str = "zero-agent";
/// Длина `project_id` в hex-символах.
const PROJECT_ID_LEN: usize = 16;

/// Каталог данных не определён.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "каталог данных не определён: задайте ZA_HOME (нет LOCALAPPDATA на Windows или HOME на Linux)"
)]
pub struct NoHome;

/// Каталог данных агента: `ZA_HOME`, иначе `%LOCALAPPDATA%\zero-agent` на
/// Windows и `$XDG_DATA_HOME/zero-agent` или `~/.local/share/zero-agent` на
/// прочих ОС. Пустые значения переменных считаются незаданными.
pub fn za_home() -> Result<PathBuf, NoHome> {
    za_home_from(|k| std::env::var_os(k), cfg!(windows))
}

/// [`za_home`] с явным окружением и платформой (для тестов).
fn za_home_from(var: impl Fn(&str) -> Option<OsString>, windows: bool) -> Result<PathBuf, NoHome> {
    let var = |k: &str| var(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(home) = var(ZA_HOME_ENV) {
        return Ok(home);
    }
    let base = if windows {
        var("LOCALAPPDATA")
    } else {
        var("XDG_DATA_HOME")
            .filter(|p| p.is_absolute())
            .or_else(|| var("HOME").map(|h| h.join(".local").join("share")))
    };
    base.map(|b| b.join(APP_DIR)).ok_or(NoHome)
}

/// Идентификатор проекта: первые 16 hex-символов BLAKE3 от канонического пути
/// каталога (байты пути в кодировке ОС). Каталог должен существовать.
pub fn project_id(dir: &Path) -> std::io::Result<String> {
    let canonical = std::fs::canonicalize(dir)?;
    let hash = blake3::hash(canonical.as_os_str().as_encoded_bytes());
    let mut hex = hash.to_hex().to_string();
    hex.truncate(PROJECT_ID_LEN);
    Ok(hex)
}

/// Каталог журнала проекта: `home/projects/<project_id>`.
pub fn journal_dir(home: &Path, project: &Path) -> std::io::Result<PathBuf> {
    Ok(home.join("projects").join(project_id(project)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let m: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn za_home_env_wins() {
        let e = env(&[
            ("ZA_HOME", "/data/za"),
            ("LOCALAPPDATA", "C:/x"),
            ("HOME", "/h"),
        ]);
        assert_eq!(za_home_from(&e, true), Ok(PathBuf::from("/data/za")));
        assert_eq!(za_home_from(&e, false), Ok(PathBuf::from("/data/za")));
    }

    #[test]
    fn platform_defaults() {
        let e = env(&[
            ("ZA_HOME", ""),
            ("LOCALAPPDATA", "C:/Users/u/AppData/Local"),
            ("HOME", "/home/u"),
        ]);
        assert_eq!(
            za_home_from(&e, true),
            Ok(PathBuf::from("C:/Users/u/AppData/Local").join("zero-agent"))
        );
        assert_eq!(
            za_home_from(&e, false),
            Ok(PathBuf::from("/home/u/.local/share").join("zero-agent"))
        );
        // Абсолютный путь на любой ОС, где идёт тест.
        let abs = env!("CARGO_MANIFEST_DIR");
        let xdg = env(&[("XDG_DATA_HOME", abs), ("HOME", "/home/u")]);
        assert_eq!(
            za_home_from(&xdg, false),
            Ok(PathBuf::from(abs).join("zero-agent"))
        );
        let relative = env(&[("XDG_DATA_HOME", "rel"), ("HOME", "/home/u")]);
        assert_eq!(
            za_home_from(&relative, false),
            Ok(PathBuf::from("/home/u/.local/share/zero-agent"))
        );
    }

    #[test]
    fn no_home_is_an_error() {
        assert_eq!(za_home_from(env(&[]), true), Err(NoHome));
        assert_eq!(za_home_from(env(&[]), false), Err(NoHome));
    }
}
