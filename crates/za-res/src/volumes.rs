//! Тома и занятое агентом место: сопоставление путей с точками монтирования,
//! размеры каталогов, пути категорий данных (RES-016, D42).

use std::path::{Path, PathBuf};

use crate::snapshot::{CategoryUsage, DataCategory};

/// Предел записей обхода одной категории (D11).
pub const CATEGORY_MAX_ENTRIES: usize = 200_000;

/// Путь в виде для сравнения. На Windows: снят `\\?\` (`\\?\UNC\` → `\\`), `/` → `\`,
/// нижний регистр, без завершающего разделителя (кроме корня `C:\`). На Unix — как есть,
/// без завершающего `/` (кроме корня).
fn norm(path: &Path) -> String {
    let s = path.to_string_lossy();
    #[cfg(windows)]
    let s = {
        let s = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{rest}")
        } else if let Some(rest) = s.strip_prefix(r"\\?\") {
            rest.to_string()
        } else {
            s.into_owned()
        };
        s.replace('/', "\\").to_lowercase()
    };
    #[cfg(not(windows))]
    let s = s.into_owned();
    trim_sep(s)
}

fn is_sep(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

fn trim_sep(mut s: String) -> String {
    while s.len() > 1 && s.ends_with(is_sep) && !s.ends_with(":\\") {
        s.pop();
    }
    s
}

/// `path` лежит внутри `base` или совпадает с ним (по компонентам, не по строке).
fn within(path: &str, base: &str) -> bool {
    match path.strip_prefix(base) {
        Some("") => true,
        Some(rest) => base.ends_with(is_sep) || rest.starts_with(is_sep),
        None => false,
    }
}

/// Точка монтирования, на которой лежит `path`: самая длинная из содержащих его.
/// Сопоставление лексическое, файловую систему не трогает.
pub fn volume_for<'a>(path: &Path, mounts: &'a [PathBuf]) -> Option<&'a PathBuf> {
    let p = norm(path);
    mounts
        .iter()
        .map(|m| (m, norm(m)))
        .filter(|(_, m)| within(&p, m))
        .max_by_key(|(_, m)| m.len())
        .map(|(m, _)| m)
}

/// Размер каталога в байтах: файлы без перехода по символическим ссылкам и junction.
///
/// Каталоги из `exclude` не обходятся. Обход останавливается, когда просмотрено больше
/// `max_entries` записей: тогда второй элемент `true`, а размер — нижняя оценка.
/// Нет `path` — `(None, false)`. Нечитаемые вложенные каталоги пропускаются.
pub fn dir_size(path: &Path, max_entries: usize, exclude: &[PathBuf]) -> (Option<u64>, bool) {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return (None, false),
    };
    if !meta.is_dir() {
        return (Some(if meta.is_file() { meta.len() } else { 0 }), false);
    }
    let exclude: Vec<String> = exclude.iter().map(|p| norm(p)).collect();
    let mut total = 0u64;
    let mut seen = 0usize;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(dir = %dir.display(), error = %e, "каталог не читается");
                continue;
            }
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > max_entries {
                return (Some(total), true);
            }
            let Ok(ft) = entry.file_type() else { continue };
            let p = entry.path();
            if ft.is_dir() {
                if !exclude.contains(&norm(&p)) {
                    stack.push(p);
                }
            } else if ft.is_file()
                && let Ok(m) = entry.metadata()
            {
                total = total.saturating_add(m.len());
            }
        }
    }
    (Some(total), false)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        env_path("USERPROFILE")
    } else {
        env_path("HOME")
    }
}

/// Кэш uv: `UV_CACHE_DIR`, иначе `%LOCALAPPDATA%\uv\cache` (Windows) или
/// `$XDG_CACHE_HOME/uv`, `~/.cache/uv` (Unix) — как у самого uv.
fn uv_cache() -> Option<PathBuf> {
    if let Some(p) = env_path("UV_CACHE_DIR") {
        return Some(p);
    }
    if cfg!(windows) {
        env_path("LOCALAPPDATA").map(|p| p.join("uv").join("cache"))
    } else {
        env_path("XDG_CACHE_HOME")
            .or_else(|| home_dir().map(|h| h.join(".cache")))
            .map(|p| p.join("uv"))
    }
}

/// Кэш cargo: `CARGO_HOME`, иначе `~/.cargo`.
fn cargo_cache() -> Option<PathBuf> {
    env_path("CARGO_HOME").or_else(|| home_dir().map(|h| h.join(".cargo")))
}

/// Пути категорий данных без `Models` (его даёт `MonitorConfig::models_dir`).
///
/// `ZaHome`, `UvCache`, `CargoCache`, затем для каждого проекта `ProjectWork`
/// (`.zero-agent/work`), `Spill`, `Worktrees`, `BrowserProfiles` внутри него.
/// Кэш без известного пути (нет переменных окружения) пропускается.
pub fn default_category_paths(
    za_home: &Path,
    projects: &[PathBuf],
) -> Vec<(DataCategory, PathBuf)> {
    let mut out = vec![(DataCategory::ZaHome, za_home.to_path_buf())];
    out.extend(uv_cache().map(|p| (DataCategory::UvCache, p)));
    out.extend(cargo_cache().map(|p| (DataCategory::CargoCache, p)));
    for project in projects {
        let work = project.join(".zero-agent").join("work");
        out.push((DataCategory::Spill, work.join("spill")));
        out.push((DataCategory::Worktrees, work.join("worktrees")));
        out.push((DataCategory::BrowserProfiles, work.join("browser")));
        out.push((DataCategory::ProjectWork, work));
    }
    out
}

/// Размеры категорий. Обход каждой категории пропускает пути других категорий, вложенные
/// в неё (D42: `ProjectWork` без `Spill`, `Worktrees`, `BrowserProfiles`; так же `Models`
/// внутри `ZA_HOME`), поэтому байты не считаются дважды.
pub fn measure_categories(
    paths: &[(DataCategory, PathBuf)],
    max_entries: usize,
) -> Vec<CategoryUsage> {
    let normed: Vec<String> = paths.iter().map(|(_, p)| norm(p)).collect();
    paths
        .iter()
        .enumerate()
        .map(|(i, (category, path))| {
            let exclude: Vec<PathBuf> = paths
                .iter()
                .enumerate()
                .filter(|&(j, _)| {
                    j != i && normed[j] != normed[i] && within(&normed[j], &normed[i])
                })
                .map(|(_, (_, p))| p.clone())
                .collect();
            let (bytes, truncated) = dir_size(path, max_entries, &exclude);
            CategoryUsage {
                category: *category,
                path: path.clone(),
                bytes,
                truncated,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_is_component_wise() {
        assert!(within("/a/b", "/a"));
        assert!(within("/a", "/a"));
        assert!(within("/a", "/"));
        assert!(!within("/ab", "/a"));
    }

    #[test]
    fn trailing_separator_is_ignored() {
        assert_eq!(norm(Path::new("/home/u/")), norm(Path::new("/home/u")));
        assert_eq!(norm(Path::new("/")).len(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_are_normalized() {
        assert_eq!(norm(Path::new(r"\\?\D:\Proj\")), r"d:\proj");
        assert_eq!(norm(Path::new(r"C:\")), r"c:\");
        assert_eq!(norm(Path::new(r"\\?\UNC\srv\share\x")), r"\\srv\share\x");
    }
}
