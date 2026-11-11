//! Копия затронутых файлов (D89): `ZA_HOME/projects/<id>/checkpoints/<run>/<step>-<seq>/`
//! с `manifest.json`; восстановление по манифесту (ревью задачи 9: I3).
//!
//! Отсутствовавший файл — `absent` (удаляется при восстановлении только с `delete_new`).
//! Файл больше [`CHECKPOINT_MAX_FILE`] — `skipped`: не копируется и при восстановлении не
//! трогается (`strict` — ошибка); новых байт больше [`CHECKPOINT_MAX_TOTAL`] — ошибка
//! `FileTooLarge`. Секретный файл (D103: [`super::secret_name`] или фильтр по содержимому)
//! — `secret`: не копируется и при восстановлении не трогается. Манифест пишется
//! последним: его наличие — признак готовой копии.

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use za_core::paths::{
    ResolvedPath, Roots, display, open_checked, read_dir_checked, remove_checked, within,
};
use za_core::{Checkpoint, CheckpointRequest};

use super::home::{
    agent_dir, create_private, files_dir, files_dir_existing, is_missing, private, read_private,
    write_private,
};
use super::write::{Mode, Tmp, fill, finish, target};
use super::{
    CHECKPOINT_MAX_FILE, CHECKPOINT_MAX_TOTAL, RestoreError, SecretFilter, files_cp, is_secret,
    valid_id,
};
use crate::AgentHome;

/// Манифест копии файлов.
const MANIFEST: &str = "manifest.json";
/// Версия манифеста.
const FORMAT_V: u32 = 2;
const STATE_FILE: &str = "file";
const STATE_ABSENT: &str = "absent";
const STATE_SKIPPED: &str = "skipped";
/// Секретный файл (D103): копии нет, восстановление не трогает.
const STATE_SECRET: &str = "secret";

/// Состояние, которое восстановление не трогает и чекпоинт «до» не копирует.
fn untouched(state: &str) -> bool {
    state == STATE_SKIPPED || state == STATE_SECRET
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    v: u32,
    id: String,
    files: Vec<ManifestFile>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestFile {
    /// Путь от корня проекта через `/`.
    path: String,
    /// `file`, `absent`, `skipped` (сверх предела) или `secret` (D103).
    state: String,
    /// Имя копии в каталоге чекпоинта (у `file`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    copy: Option<String>,
    /// Права Unix (у `file`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<u32>,
    /// Размер (у `skipped`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
}

fn too_large(what: String) -> io::Error {
    io::Error::new(io::ErrorKind::FileTooLarge, what)
}

/// Копия файлов запроса `req` под id `<run>/<stem>`. Повтор — готовая копия остаётся.
/// Возвращает пропущенные как секреты пути (от корня проекта, D103).
pub(super) fn copy(
    home: &AgentHome,
    req: &CheckpointRequest,
    stem: &str,
    strict: bool,
    filter: Option<&SecretFilter>,
) -> io::Result<Vec<String>> {
    let id = format!("{}/{stem}", req.run);
    let dir = files_dir(home, &id)?;
    let manifest_path = dir.join(MANIFEST);
    if let Ok(m) = load(&manifest_path) {
        return Ok(secrets_of(&m));
    }
    let roots = Roots::canonical(&req.project_dir, &home.za_home)?;
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for p in &req.paths {
        let abs = if p.is_absolute() {
            p.clone()
        } else {
            roots.project.join(p)
        };
        if !within(&roots.project, &abs) || abs == roots.project {
            tracing::debug!(path = %abs.display(), "путь вне проекта: в копию не входит");
            continue;
        }
        let rel = display(&roots.project, &abs);
        if !seen.insert(rel.clone()) {
            continue;
        }
        let r = ResolvedPath {
            path: abs,
            inside: true,
            protected: false,
        };
        // По имени — и отсутствующий файл: восстановление не удалит созданный секрет.
        if super::secret_name(&rel) {
            tracing::info!(path = %rel, "имя секретного файла: в копию не входит");
            files.push(ManifestFile {
                path: rel,
                state: STATE_SECRET.into(),
                copy: None,
                mode: None,
                size: None,
            });
            continue;
        }
        match open_checked(&r, false) {
            Ok(src) => {
                let meta = src.metadata()?;
                let size = meta.len();
                if is_secret(&rel, &r.path, size, filter) {
                    tracing::info!(path = %rel, "похоже на секрет: в копию не входит");
                    files.push(ManifestFile {
                        path: rel,
                        state: STATE_SECRET.into(),
                        copy: None,
                        mode: None,
                        size: None,
                    });
                    continue;
                }
                if size > CHECKPOINT_MAX_FILE {
                    if strict {
                        return Err(too_large(format!("{rel}: {size} байт")));
                    }
                    tracing::warn!(path = %rel, size, "файл больше предела: в копию не входит");
                    files.push(ManifestFile {
                        path: rel,
                        state: STATE_SKIPPED.into(),
                        copy: None,
                        mode: None,
                        size: Some(size),
                    });
                    continue;
                }
                total += size;
                if total > CHECKPOINT_MAX_TOTAL {
                    return Err(too_large(format!(
                        "всего больше {CHECKPOINT_MAX_TOTAL} байт"
                    )));
                }
                let copy = files.len().to_string();
                let mut out = create_private(&private(dir.join(&copy)))?;
                // Не больше заявленного размера: файл мог расти во время копии.
                io::copy(&mut (&src).take(CHECKPOINT_MAX_FILE), &mut out)?;
                out.sync_all()?;
                files.push(ManifestFile {
                    path: rel,
                    state: STATE_FILE.into(),
                    copy: Some(copy),
                    mode: unix_mode(&meta),
                    size: None,
                });
            }
            Err(e) if is_missing(&e) => files.push(ManifestFile {
                path: rel,
                state: STATE_ABSENT.into(),
                copy: None,
                mode: None,
                size: None,
            }),
            Err(e) => return Err(e),
        }
    }
    let m = Manifest {
        v: FORMAT_V,
        id,
        files,
    };
    let json = serde_json::to_vec(&m).map_err(io::Error::other)?;
    write_private(&manifest_path, &json)?;
    Ok(secrets_of(&m))
}

/// Пути манифеста в состоянии `secret`, по возрастанию.
fn secrets_of(m: &Manifest) -> Vec<String> {
    let mut v: Vec<String> = m
        .files
        .iter()
        .filter(|f| f.state == STATE_SECRET)
        .map(|f| f.path.clone())
        .collect();
    v.sort();
    v
}

fn load(path: &std::path::Path) -> io::Result<Manifest> {
    let bytes = read_private(path, 64 << 20)?;
    let m: Manifest = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if m.v != FORMAT_V {
        return Err(io::Error::other(format!("манифест версии {}", m.v)));
    }
    Ok(m)
}

/// Есть ли готовая копия `id`.
pub(super) fn exists(home: &AgentHome, id: &str) -> io::Result<bool> {
    Ok(load(&files_dir_existing(home, id)?.join(MANIFEST)).is_ok())
}

/// Пути (от корня проекта) копии `id` — для чекпоинта «до» восстановления.
pub(super) fn paths(home: &AgentHome, id: &str) -> io::Result<Vec<String>> {
    let m = load(&files_dir_existing(home, id)?.join(MANIFEST))?;
    Ok(m.files
        .into_iter()
        .filter(|f| !untouched(&f.state))
        .map(|f| f.path)
        .collect())
}

/// Готовые копии файлов в `ZA_HOME`.
pub(super) fn list(home: &AgentHome) -> io::Result<Vec<Checkpoint>> {
    let root = private(agent_dir(home, None)?);
    let mut out = Vec::new();
    for run in read_dir_checked(&root)? {
        let run = run.to_string_lossy().into_owned();
        let run_dir = private(root.path.join(&run));
        let Ok(stems) = read_dir_checked(&run_dir) else {
            continue;
        };
        for stem in stems {
            let id = format!("{run}/{}", stem.to_string_lossy());
            if valid_id(&id) && load(&run_dir.path.join(&stem).join(MANIFEST)).is_ok() {
                out.push(files_cp(&id));
            }
        }
    }
    Ok(out)
}

/// Восстановление по манифесту: сначала проверка всех целей, затем запись потоком.
pub(super) fn restore(
    home: &AgentHome,
    roots: &Roots,
    id: &str,
    delete_new: bool,
) -> Result<Vec<PathBuf>, RestoreError> {
    let dir = files_dir_existing(home, id)?;
    let m = load(&dir.join(MANIFEST)).map_err(|e| {
        if is_missing(&e) {
            RestoreError::NotFound(id.to_owned())
        } else {
            RestoreError::Io(e)
        }
    })?;
    let mut plan = Vec::new();
    for f in &m.files {
        if untouched(&f.state) {
            continue;
        }
        if let Some(t) = target(roots, &f.path)? {
            plan.push((f, t));
        }
    }
    let mut changed = Vec::new();
    for (n, (f, t)) in plan.into_iter().enumerate() {
        if f.state == STATE_FILE {
            let copy = f
                .copy
                .as_deref()
                .filter(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(|| io::Error::other(format!("манифест {id}: нет копии {}", f.path)))?;
            let mut src = open_checked(&private(dir.join(copy)), false)?;
            let mut tmp = Tmp::new(&roots.project, &t, n)?;
            if let Err(e) = fill(&mut tmp, &mut src) {
                tmp.discard();
                return Err(e.into());
            }
            if finish(&t, tmp, f.mode.map(Mode::Unix), n)? {
                changed.push(t.r.path.clone());
            }
        } else if delete_new && t.exists {
            remove_checked(&t.r)?;
            changed.push(t.r.path.clone());
        }
    }
    Ok(changed)
}

#[cfg(unix)]
fn unix_mode(m: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(m.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn unix_mode(_m: &std::fs::Metadata) -> Option<u32> {
    None
}
