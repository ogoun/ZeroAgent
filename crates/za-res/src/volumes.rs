//! Тома и занятое агентом место: сопоставление путей с точками монтирования,
//! размеры каталогов, пути категорий данных (RES-016, D42).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Размер каталога в байтах.
///
/// Корень берётся с переходом по ссылке или junction (`models_dir` на другом диске),
/// внутри обхода ссылки и junction не обходятся. Каталоги из `exclude` не обходятся.
/// Второй элемент `true` — обход неполный, размер — нижняя оценка: просмотрено больше
/// `max_entries` записей или вложенный каталог не читается. Корня нет или он не читается —
/// `(None, false)`.
pub fn dir_size(path: &Path, max_entries: usize, exclude: &[PathBuf]) -> (Option<u64>, bool) {
    walk(path, max_entries, exclude, &AtomicBool::new(false)).unwrap_or((None, false))
}

/// [`dir_size`] с отменой: `cancel` проверяется на каждой записи, отменённый обход — `None`.
pub(crate) fn walk(
    path: &Path,
    max_entries: usize,
    exclude: &[PathBuf],
    cancel: &AtomicBool,
) -> Option<(Option<u64>, bool)> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Some((None, false)),
    };
    if !meta.is_dir() {
        return Some((Some(if meta.is_file() { meta.len() } else { 0 }), false));
    }
    let exclude: Vec<String> = exclude.iter().map(|p| norm(p)).collect();
    let mut total = 0u64;
    let mut seen = 0usize;
    let mut incomplete = false;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if dir == path => {
                tracing::debug!(dir = %dir.display(), error = %e, "корень категории не читается");
                return Some((None, false));
            }
            Err(e) => {
                tracing::debug!(dir = %dir.display(), error = %e, "каталог не читается");
                incomplete = true;
                continue;
            }
        };
        for entry in entries {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let Ok(entry) = entry else {
                incomplete = true;
                continue;
            };
            seen += 1;
            if seen > max_entries {
                return Some((Some(total), true));
            }
            let Ok(ft) = entry.file_type() else {
                incomplete = true;
                continue;
            };
            let p = entry.path();
            if ft.is_dir() {
                if !exclude.contains(&norm(&p)) {
                    stack.push(p);
                }
            } else if ft.is_file() {
                match entry.metadata() {
                    Ok(m) => total = total.saturating_add(m.len()),
                    Err(_) => incomplete = true,
                }
            }
        }
    }
    Some((Some(total), incomplete))
}

/// Убирает категории с путём, уже встречавшимся раньше (один проект дважды,
/// `CARGO_HOME` = `UV_CACHE_DIR`): иначе байты посчитались бы дважды.
pub(crate) fn dedup_paths(paths: &mut Vec<(DataCategory, PathBuf)>) {
    let mut seen = Vec::new();
    paths.retain(|(_, p)| {
        let n = norm(p);
        if seen.contains(&n) {
            false
        } else {
            seen.push(n);
            true
        }
    });
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
/// Кэш без известного пути (нет переменных окружения) пропускается, повторный путь — тоже.
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
    dedup_paths(&mut out);
    out
}

/// Размеры категорий. Обход каждой категории пропускает пути других категорий, вложенные
/// в неё (D42: `ProjectWork` без `Spill`, `Worktrees`, `BrowserProfiles`; так же `Models`
/// внутри `ZA_HOME`), поэтому байты не считаются дважды.
pub fn measure_categories(
    paths: &[(DataCategory, PathBuf)],
    max_entries: usize,
) -> Vec<CategoryUsage> {
    measure_categories_cancellable(paths, max_entries, &AtomicBool::new(false)).unwrap_or_default()
}

/// [`measure_categories`] с отменой (drop монитора): отменённый обход — `None`.
pub(crate) fn measure_categories_cancellable(
    paths: &[(DataCategory, PathBuf)],
    max_entries: usize,
    cancel: &AtomicBool,
) -> Option<Vec<CategoryUsage>> {
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
            let (bytes, truncated) = walk(path, max_entries, &exclude, cancel)?;
            Some(CategoryUsage {
                category: *category,
                path: path.clone(),
                bytes,
                truncated,
            })
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

    #[test]
    fn cancelled_walk_returns_none() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        assert_eq!(walk(&dir, 100, &[], &AtomicBool::new(true)), None);
        assert!(walk(&dir, 100, &[], &AtomicBool::new(false)).is_some());
        let paths = vec![(DataCategory::ZaHome, dir)];
        assert_eq!(
            measure_categories_cancellable(&paths, 100, &AtomicBool::new(true)),
            None
        );
    }

    #[test]
    fn duplicate_paths_are_dropped() {
        let mut v = vec![
            (DataCategory::UvCache, PathBuf::from("/c/x")),
            (DataCategory::CargoCache, PathBuf::from("/c/x/")),
            (DataCategory::ZaHome, PathBuf::from("/c")),
        ];
        dedup_paths(&mut v);
        assert_eq!(
            v,
            vec![
                (DataCategory::UvCache, PathBuf::from("/c/x")),
                (DataCategory::ZaHome, PathBuf::from("/c")),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_are_normalized() {
        assert_eq!(norm(Path::new(r"\\?\D:\Proj\")), r"d:\proj");
        assert_eq!(norm(Path::new(r"C:\")), r"c:\");
        assert_eq!(norm(Path::new(r"\\?\UNC\srv\share\x")), r"\\srv\share\x");
    }
}
