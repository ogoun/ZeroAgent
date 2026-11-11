//! Чекпоинты рабочей копии и восстановление (R1-C1 D89, TOOL-009; Ruling 7.15 «безопасный
//! запуск git», 1.13, 5.3–5.7).
//!
//! [`GitCheckpointer`] делает чекпоинт перед каждым исполняемым вызовом вида `Edit` или
//! `Exec`. Имя — от ключа эффекта: `id = "<run>/<step>-<seq>"` (без `:`); если такой
//! чекпоинт уже есть (повтор после сбоя), возвращается он.
//!
//! **git** — только в доверенном проекте, который лежит в рабочем дереве репозитория и не
//! игнорируется им. Только plumbing, без `add`, `checkout` и фильтров:
//! 1. `ls-files -z -s -c` (индекс пользователя только читается) и
//!    `ls-files -z -o --exclude-standard` — список файлов без чтения содержимого;
//! 2. `hash-object --no-filters -w --stdin-paths` — для файлов, чей `(size, mtime)`
//!    отличается от кэша прошлого чекпоинта run (`stat.json`; запись моложе 2 с до
//!    сохранения кэша не доверяется — «racy git»);
//! 3. `update-index -z --index-info` в **свой** индекс (`GIT_INDEX_FILE` в каталоге агента)
//!    и `write-tree`;
//! 4. `commit-tree` (родитель — прошлый чекпоинт run) и `update-ref` скрытой ссылки
//!    [`CHECKPOINT_REF_PREFIX`]`/<run>/<step>-<seq>` только на создание.
//!
//! HEAD, индекс, ветки, stash и рабочее дерево пользователя не меняются; в репозиторий
//! добавляются только объекты и скрытая ссылка. Каждый вызов git — через
//! [`za_jail::Runner`], с окружением `EnvMode::Clear` (наследуются только `PATH`,
//! `SystemRoot`, `WINDIR`), `GIT_CONFIG_NOSYSTEM=1`, пустым глобальным конфигом, своим
//! `HOME`, `--no-pager` и опциями `-c`, отключающими fsmonitor, хуки (пустой каталог
//! `core.hooksPath`), кэш неотслеживаемых, автоматическую уборку, подпись, `autocrlf` и
//! файл атрибутов. Фильтры не запускаются: `hash-object --no-filters`, восстановление —
//! `cat-file --batch`; байты в объектах сырые, поэтому восстановление точно при любом
//! `autocrlf`. Встроенные команды git алиасами не переопределяются. `safe.directory` не
//! трогается: отказ по владельцу — пропуск `git_unsafe_directory`.
//!
//! **Копия файлов** — для `Edit` в недоверенном проекте (git там не вызывается вовсе, D83)
//! и там, где git недоступен: прежнее содержимое затронутых файлов внутри проекта
//! копируется в `ZA_HOME/projects/<project_id>/checkpoints/<run>/<step>-<seq>/` с
//! `manifest.json` (отсутствовавший файл — `absent`). Каталоги агента — через
//! [`private_home_dir`] и [`za_jail::check_registry`]: вне проекта, свои и закрытые,
//! поэтому `bash` модели не подменит ни копии, ни пустой каталог хуков. `Exec` без git —
//! пропуск `no_git` (`git_not_found` без git в `PATH`).
//!
//! **Предел** — [`CHECKPOINT_TIMEOUT`] на весь чекпоинт git. Превышение — `slow`, и все
//! следующие чекпоинты git этого run пропускаются без ожидания (`Edit` при этом получает
//! копию файлов: она не зависит от git).
//!
//! **Восстановление** ([`restore`]): git — `ls-tree -r -z -l` и `cat-file --batch`, копия —
//! по манифесту. Сначала проверяются все цели: путь проходит
//! [`za_core::paths::classify`] и [`za_core::paths::resolve`], должен быть внутри проекта,
//! совпадать со своим лексическим путём (ни один компонент не ссылка и не junction) и быть
//! обычным файлом или отсутствовать; иначе — отказ до первой записи. Защищённые места
//! (D79: `.git/`, `.zero-agent/`, `ZA_HOME`) пропускаются с предупреждением. Запись —
//! временный файл и [`replace_checked`], каталоги — [`create_parents_checked`], удаление —
//! [`remove_checked`]; неизменившиеся файлы не переписываются. Файлы, которых нет в
//! чекпоинте (у копии — `absent`), удаляются только с `delete_new`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use za_core::paths::{
    ResolvedPath, Roots, classify, create_parents_checked, display, open_checked, private_home_dir,
    read_dir_checked, remove_checked, replace_checked, resolve, stat_checked, within, work_dir,
};
use za_core::{Access, BoxFuture, Checkpoint, CheckpointRequest, Checkpointer, ToolKind};
use za_jail::{CommandSpec, EnvMode, Exit, Runner, RunnerError};
use za_protocol::{EffectKey, RunId, checkpoint_kind, checkpoint_reason};

use crate::AgentHome;

/// Предел чекпоинта git целиком (D89).
pub const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(20);
/// Пространство скрытых ссылок чекпоинтов.
pub const CHECKPOINT_REF_PREFIX: &str = "refs/zero-agent/checkpoints";

/// Предел одного вызова git при восстановлении (команда пользователя, не вызов модели).
const RESTORE_GIT_TIMEOUT: Duration = Duration::from_secs(300);
/// Предел вывода git со списками файлов и ссылок.
const LIST_CAPTURE: usize = 64 << 20;
/// Сколько байт содержимого объектов читается одним `cat-file --batch`.
const BATCH_BYTES: u64 = 32 << 20;
/// Запись кэша stat моложе этого (до сохранения кэша) не доверяется: файл мог
/// измениться в тот же квант `mtime` («racy git»).
const RACY: Duration = Duration::from_secs(2);
/// Каталог агента в `ZA_HOME/projects/<project_id>/` и в `work_dir` проекта (временные
/// файлы восстановления).
const CHECKPOINTS_DIR: &str = "checkpoints";
/// Манифест копии файлов.
const MANIFEST: &str = "manifest.json";
/// Версия манифеста и кэша stat.
const FORMAT_V: u32 = 1;
/// Служебный каталог проекта: целиком не восстанавливается (D79, Ruling 2 задачи 7).
const ZA_DIR: &str = ".zero-agent";
/// Идентичность коммитов чекпоинтов (Ruling 5.4).
const IDENTITY: &str = "zero-agent";
/// Строка сообщения коммита с подкаталогом проекта в репозитории.
const PREFIX_TRAILER: &str = "zero-agent-prefix: ";
/// Файловые режимы git.
const MODE_FILE: &str = "100644";
const MODE_EXEC: &str = "100755";
const MODE_LINK: &str = "120000";
const MODE_GITLINK: &str = "160000";

/// Ошибка восстановления и поиска чекпоинта.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("чекпоинт {0} не найден")]
    NotFound(String),
    #[error("git: {0}")]
    Git(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Чекпоинты рабочей копии: git plumbing или копия файлов (D89).
#[derive(Clone)]
pub struct GitCheckpointer {
    runner: Arc<dyn Runner>,
    git: Option<PathBuf>,
    home: AgentHome,
    /// Дополнительные переменные каждого вызова git (тесты: `GIT_CEILING_DIRECTORIES`).
    base_env: Vec<(OsString, OsString)>,
    /// Run, у которых git не уложился в предел.
    slow: Arc<Mutex<HashSet<RunId>>>,
    /// Чекпоинты одного процесса идут по одному: общий кэш stat и каталоги run.
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for GitCheckpointer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitCheckpointer")
            .field("git", &self.git)
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl GitCheckpointer {
    /// git ищется в `PATH` процесса (Windows — `git.exe`); нет — чекпоинты git
    /// пропускаются (`git_not_found`).
    pub fn new(runner: Arc<dyn Runner>, home: AgentHome) -> Self {
        Self::with_git(runner, home, find_git())
    }

    /// Явный путь git; `None` — git нет.
    pub fn with_git(runner: Arc<dyn Runner>, home: AgentHome, git: Option<PathBuf>) -> Self {
        Self {
            runner,
            git,
            home,
            base_env: Vec::new(),
            slow: Arc::new(Mutex::new(HashSet::new())),
            lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Для тестов: переменные, добавляемые к окружению каждого вызова git
    /// (`GIT_CEILING_DIRECTORIES`). Переменные безопасности их перекрывают.
    pub fn with_base_env(mut self, env: Vec<(OsString, OsString)>) -> Self {
        self.base_env = env;
        self
    }

    fn is_slow(&self, run: RunId) -> bool {
        self.slow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&run)
    }

    fn mark_slow(&self, run: RunId) {
        self.slow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(run);
    }

    async fn create_inner(self, req: CheckpointRequest) -> Checkpoint {
        let _one = self.lock.lock().await;
        let (id, stem) = names(&req);
        let edit = req.kind == ToolKind::Edit;
        // Недоверенный проект: git не вызывается (D83).
        if !req.trusted {
            return if edit {
                self.files(&req, &id, &stem).await
            } else {
                skipped(&id, checkpoint_reason::NO_GIT)
            };
        }
        let reason = if self.is_slow(req.run) {
            checkpoint_reason::SLOW
        } else if let Some(program) = self.git.clone() {
            let git = self.session(program, RunMode::Create);
            let work = git.checkpoint(&req, &id, &stem);
            match tokio::time::timeout(CHECKPOINT_TIMEOUT, work).await {
                Ok(Ok(cp)) => return cp,
                Ok(Err(Skip(r))) => {
                    if r == checkpoint_reason::SLOW {
                        self.mark_slow(req.run);
                    }
                    r
                }
                Err(_) => {
                    tracing::warn!(id = %id, "чекпоинт git не уложился в предел, до конца run — без git");
                    self.mark_slow(req.run);
                    checkpoint_reason::SLOW
                }
            }
        } else {
            checkpoint_reason::GIT_NOT_FOUND
        };
        if edit {
            self.files(&req, &id, &stem).await
        } else {
            skipped(&id, reason)
        }
    }

    /// Копия затронутых файлов (D89).
    async fn files(&self, req: &CheckpointRequest, id: &str, stem: &str) -> Checkpoint {
        let home = self.home.clone();
        let req = req.clone();
        let (id2, stem2) = (id.to_owned(), stem.to_owned());
        match blocking(move || copy_files(&home, &req, &stem2)).await {
            Ok(()) => Checkpoint {
                id: id2,
                kind: checkpoint_kind::FILES.into(),
                git_ref: None,
                commit: None,
                reason: None,
            },
            Err(e) => {
                tracing::warn!(id = %id, error = %e, "копия файлов чекпоинта не сделана");
                skipped(id, checkpoint_reason::IO_ERROR)
            }
        }
    }

    fn session(&self, program: PathBuf, mode: RunMode) -> GitSession {
        GitSession {
            runner: self.runner.clone(),
            program,
            home: self.home.clone(),
            base_env: self.base_env.clone(),
            timeout: match mode {
                RunMode::Create => CHECKPOINT_TIMEOUT,
                RunMode::Restore => RESTORE_GIT_TIMEOUT,
            },
            safe: None,
        }
    }

    /// Чекпоинты проекта `project` (канонический корень): скрытые ссылки репозитория,
    /// сделанные для этого каталога, и копии файлов в `ZA_HOME`; по возрастанию id.
    pub async fn list(&self, project: &Path) -> Result<Vec<Checkpoint>, RestoreError> {
        let mut out = Vec::new();
        if let Some(program) = self.git.clone() {
            let mut git = self.session(program, RunMode::Restore);
            if let Some(repo) = git.repo(project).await? {
                out.extend(git.refs(&repo, None).await?);
            }
        }
        let home = self.home.clone();
        out.extend(blocking(move || list_files(&home)).await?);
        out.sort_by_key(|c| sort_key(&c.id));
        Ok(out)
    }

    /// Чекпоинт по id `<run>/<step>-<seq>`; неверный id или чужой чекпоинт —
    /// [`RestoreError::NotFound`].
    pub async fn find(&self, project: &Path, id: &str) -> Result<Checkpoint, RestoreError> {
        if !valid_id(id) {
            return Err(RestoreError::NotFound(id.to_owned()));
        }
        if let Some(program) = self.git.clone() {
            let mut git = self.session(program, RunMode::Restore);
            if let Some(repo) = git.repo(project).await?
                && let Some(cp) = git.refs(&repo, Some(id)).await?.into_iter().next()
            {
                return Ok(cp);
            }
        }
        let home = self.home.clone();
        let id2 = id.to_owned();
        let has = blocking(move || {
            let dir = files_dir_existing(&home, &id2)?;
            Ok(load_manifest(&dir.join(MANIFEST)).is_ok())
        })
        .await?;
        if has {
            return Ok(Checkpoint {
                id: id.to_owned(),
                kind: checkpoint_kind::FILES.into(),
                git_ref: None,
                commit: None,
                reason: None,
            });
        }
        Err(RestoreError::NotFound(id.to_owned()))
    }
}

impl Checkpointer for GitCheckpointer {
    fn create(&self, req: CheckpointRequest) -> BoxFuture<'static, Checkpoint> {
        Box::pin(self.clone().create_inner(req))
    }
}

/// Чекпоинт перед восстановлением `cp` (D89): тем же способом, что и `cp` — git для
/// чекпоинта git, копия тех же файлов для копии; новый run-идентификатор, шаг 0.
pub async fn backup_before_restore(
    ck: &GitCheckpointer,
    project: &Path,
    cp: &Checkpoint,
) -> Checkpoint {
    let run = RunId::new();
    let git = cp.kind == checkpoint_kind::GIT;
    let paths = if git {
        Vec::new()
    } else {
        let home = ck.home.clone();
        let (id, root) = (cp.id.clone(), project.to_path_buf());
        blocking(move || {
            let m = load_manifest(&files_dir_existing(&home, &id)?.join(MANIFEST))?;
            Ok(m.files.iter().map(|f| join_rel(&root, &f.path)).collect())
        })
        .await
        .unwrap_or_else(|e: io::Error| {
            tracing::warn!(error = %e, "манифест не прочитан");
            Vec::new()
        })
    };
    let req = CheckpointRequest {
        project_dir: project.to_path_buf(),
        run,
        effect_key: EffectKey::derive(run, 0, 0),
        kind: if git { ToolKind::Exec } else { ToolKind::Edit },
        paths,
        trusted: git,
    };
    ck.create(req).await
}

/// Восстанавливает рабочую копию проекта `project` (канонический корень) по чекпоинту
/// `cp`. Возвращает изменённые (записанные и удалённые) пути. Цели проверяются все до
/// первой записи (см. модуль).
pub async fn restore(
    ck: &GitCheckpointer,
    project: &Path,
    cp: &Checkpoint,
    delete_new: bool,
) -> Result<Vec<PathBuf>, RestoreError> {
    let roots = {
        let (p, h) = (project.to_path_buf(), ck.home.za_home.clone());
        blocking(move || Roots::canonical(&p, &h)).await?
    };
    match cp.kind.as_str() {
        checkpoint_kind::GIT => {
            let commit = cp
                .commit
                .clone()
                .filter(|c| is_oid(c))
                .ok_or_else(|| RestoreError::NotFound(cp.id.clone()))?;
            let program = ck
                .git
                .clone()
                .ok_or_else(|| RestoreError::Git("git не найден в PATH".into()))?;
            let mut git = ck.session(program, RunMode::Restore);
            let repo = git
                .repo(&roots.project)
                .await?
                .ok_or_else(|| RestoreError::Git("проект не в репозитории git".into()))?;
            restore_git(&mut git, &repo, roots, &commit, delete_new).await
        }
        checkpoint_kind::FILES => {
            let home = ck.home.clone();
            let id = cp.id.clone();
            blocking(move || Ok(restore_files(&home, &roots, &id, delete_new))).await?
        }
        _ => Err(RestoreError::NotFound(cp.id.clone())),
    }
}

// ---- имена ----

/// `("<run>/<step>-<seq>", "<step>-<seq>")` по ключу эффекта (`run:step:seq`).
fn names(req: &CheckpointRequest) -> (String, String) {
    let mut it = req.effect_key.0.rsplitn(3, ':');
    let stem = match (it.next(), it.next()) {
        (Some(seq), Some(step))
            if !seq.is_empty()
                && !step.is_empty()
                && seq.bytes().all(|b| b.is_ascii_digit())
                && step.bytes().all(|b| b.is_ascii_digit()) =>
        {
            format!("{step}-{seq}")
        }
        _ => req.effect_key.file_stem(),
    };
    (format!("{}/{stem}", req.run), stem)
}

/// id вида `<run>/<step>-<seq>`: run — hex и `-`, шаг и номер — цифры.
fn valid_id(id: &str) -> bool {
    let Some((run, stem)) = id.split_once('/') else {
        return false;
    };
    let Some((step, seq)) = stem.split_once('-') else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit());
    !run.is_empty()
        && run.len() <= 64
        && run.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        && digits(step)
        && digits(seq)
}

/// Порядок id: run, затем шаг и номер числами.
fn sort_key(id: &str) -> (String, u64, u64) {
    let (run, stem) = id.split_once('/').unwrap_or((id, ""));
    let (step, seq) = stem.split_once('-').unwrap_or(("", ""));
    (
        run.to_owned(),
        step.parse().unwrap_or(u64::MAX),
        seq.parse().unwrap_or(u64::MAX),
    )
}

fn is_oid(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn skipped(id: &str, reason: &str) -> Checkpoint {
    Checkpoint {
        id: id.to_owned(),
        kind: checkpoint_kind::SKIPPED.into(),
        git_ref: None,
        commit: None,
        reason: Some(reason.to_owned()),
    }
}

/// Путь от корня по относительному пути git (`/`).
fn join_rel(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in rel.split('/').filter(|s| !s.is_empty()) {
        p.push(part);
    }
    p
}

/// git в `PATH` процесса.
fn find_git() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let name = if cfg!(windows) { "git.exe" } else { "git" };
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

async fn blocking<T, F>(f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    let scoped = move || {
        let _drives = za_core::paths::DriveScope::enter();
        f()
    };
    tokio::task::spawn_blocking(scoped)
        .await
        .map_err(|e| io::Error::other(format!("задача чекпоинта упала: {e}")))?
}

// ---- каталоги агента в ZA_HOME ----

/// Закрытый каталог `ZA_HOME/projects/<project_id>/checkpoints[/<sub>]`.
fn agent_dir(home: &AgentHome, sub: Option<&str>) -> io::Result<PathBuf> {
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
fn files_dir(home: &AgentHome, id: &str) -> io::Result<PathBuf> {
    check_id(id)?;
    agent_dir(home, Some(id))
}

/// Каталог копии файлов чекпоинта `id` без создания (поиск, восстановление).
fn files_dir_existing(home: &AgentHome, id: &str) -> io::Result<PathBuf> {
    check_id(id)?;
    Ok(agent_dir(home, None)?.join(join_rel(Path::new(""), id)))
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

fn private(path: PathBuf) -> ResolvedPath {
    ResolvedPath {
        path,
        inside: false,
        protected: false,
    }
}

/// Атомарная запись закрытого файла агента: временный файл рядом (0600 на Unix) и
/// [`replace_checked`].
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = private(path.with_file_name(format!(".{name}.{}.tmp", std::process::id())));
    let r = (|| {
        #[cfg(unix)]
        za_core::paths::create_private_file(&tmp.path)?;
        let mut f = open_checked(&tmp, true)?;
        f.set_len(0)?;
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

/// Содержимое закрытого файла агента (обычный файл, не ссылка).
fn read_private(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let f = open_checked(&private(path.to_path_buf()), false)?;
    let mut bytes = Vec::new();
    (&f).take(max).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn is_missing(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

// ---- копия файлов ----

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
    /// `file` или `absent`.
    state: String,
    /// Имя копии в каталоге чекпоинта (у `file`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    copy: Option<String>,
    /// Права Unix (у `file`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<u32>,
}

const STATE_FILE: &str = "file";
const STATE_ABSENT: &str = "absent";

fn copy_files(home: &AgentHome, req: &CheckpointRequest, stem: &str) -> io::Result<()> {
    let id = format!("{}/{stem}", req.run);
    let dir = files_dir(home, &id)?;
    let manifest_path = dir.join(MANIFEST);
    // Повтор после сбоя: копия уже сделана (манифест пишется последним).
    if load_manifest(&manifest_path).is_ok() {
        return Ok(());
    }
    let roots = Roots::canonical(&req.project_dir, &home.za_home)?;
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
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
        match open_checked(&r, false) {
            Ok(mut src) => {
                let copy = files.len().to_string();
                let dst = private(dir.join(&copy));
                #[cfg(unix)]
                za_core::paths::create_private_file(&dst.path)?;
                let mut out = open_checked(&dst, true)?;
                out.set_len(0)?;
                io::copy(&mut src, &mut out)?;
                out.sync_all()?;
                files.push(ManifestFile {
                    path: rel,
                    state: STATE_FILE.into(),
                    copy: Some(copy),
                    mode: unix_mode(&src.metadata()?),
                });
            }
            Err(e) if is_missing(&e) => files.push(ManifestFile {
                path: rel,
                state: STATE_ABSENT.into(),
                copy: None,
                mode: None,
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
    write_private(&manifest_path, &json)
}

fn load_manifest(path: &Path) -> io::Result<Manifest> {
    let bytes = read_private(path, 64 << 20)?;
    let m: Manifest = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if m.v != FORMAT_V {
        return Err(io::Error::other(format!("манифест версии {}", m.v)));
    }
    Ok(m)
}

fn list_files(home: &AgentHome) -> io::Result<Vec<Checkpoint>> {
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
            if valid_id(&id) && load_manifest(&run_dir.path.join(&stem).join(MANIFEST)).is_ok() {
                out.push(Checkpoint {
                    id,
                    kind: checkpoint_kind::FILES.into(),
                    git_ref: None,
                    commit: None,
                    reason: None,
                });
            }
        }
    }
    Ok(out)
}

fn restore_files(
    home: &AgentHome,
    roots: &Roots,
    id: &str,
    delete_new: bool,
) -> Result<Vec<PathBuf>, RestoreError> {
    let dir = files_dir_existing(home, id)?;
    let m = load_manifest(&dir.join(MANIFEST)).map_err(|e| {
        if is_missing(&e) {
            RestoreError::NotFound(id.to_owned())
        } else {
            RestoreError::Io(e)
        }
    })?;
    // Сначала проверка всех целей.
    let mut plan = Vec::new();
    for f in &m.files {
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
            let bytes = read_private(&dir.join(copy), u64::MAX)?;
            if write_target(&roots.project, &t, &bytes, f.mode.map(Mode::Unix), n)? {
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

// ---- цели восстановления ----

/// Проверенная цель восстановления.
struct Target {
    r: ResolvedPath,
    exists: bool,
}

/// Цель восстановления `rel`: внутри проекта, без ссылок на пути, обычный файл или нет
/// файла. Защищённое место — `None` и предупреждение; прочее — ошибка.
fn target(roots: &Roots, rel: &str) -> io::Result<Option<Target>> {
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
        Ok(_) => {
            return Err(io::Error::other(format!("not a regular file: {rel}")));
        }
        Err(e) if is_missing(&e) || e.kind() == io::ErrorKind::NotADirectory => false,
        Err(e) => return Err(e),
    };
    Ok(Some(Target { r, exists }))
}

/// Режим файла при записи (на Windows права Unix не применяются).
#[derive(Clone, Copy)]
#[cfg_attr(not(unix), allow(dead_code))]
enum Mode {
    /// Режим git: исполняемый или нет.
    Git(bool),
    /// Права Unix из манифеста копии.
    Unix(u32),
}

/// Пишет `bytes` в цель, если содержимое отличается; true — файл записан.
fn write_target(
    root: &Path,
    t: &Target,
    bytes: &[u8],
    mode: Option<Mode>,
    n: usize,
) -> io::Result<bool> {
    let old_meta = if t.exists {
        let f = open_checked(&t.r, false)?;
        let meta = f.metadata()?;
        if meta.len() == bytes.len() as u64 {
            let mut cur = Vec::with_capacity(bytes.len());
            (&f).read_to_end(&mut cur)?;
            if cur == bytes && !mode_differs(&meta, mode) {
                return Ok(false);
            }
        }
        Some(meta)
    } else {
        create_parents_checked(&t.r)?;
        None
    };
    let mut candidates = Vec::new();
    match work_dir(root, CHECKPOINTS_DIR) {
        Ok(dir) => candidates.push(ResolvedPath {
            path: dir.join(format!("restore-{}-{n}.tmp", std::process::id())),
            inside: true,
            protected: false,
        }),
        Err(e) => tracing::warn!(error = %e, "каталог временных файлов восстановления недоступен"),
    }
    if let Some(parent) = t.r.path.parent() {
        candidates.push(ResolvedPath {
            path: parent.join(format!(".za-restore-{}-{n}.tmp", std::process::id())),
            inside: true,
            protected: false,
        });
    }
    let mut last = io::Error::other("нет места для временного файла");
    for tmp in &candidates {
        let r = write_tmp(tmp, bytes, old_meta.as_ref(), mode)
            .and_then(|()| replace_checked(tmp, &t.r));
        match r {
            Ok(()) => return Ok(true),
            Err(e) => {
                let _ = remove_checked(tmp);
                if e.kind() != io::ErrorKind::CrossesDevices {
                    return Err(e);
                }
                last = e;
            }
        }
    }
    Err(last)
}

fn write_tmp(
    tmp: &ResolvedPath,
    bytes: &[u8],
    old: Option<&std::fs::Metadata>,
    mode: Option<Mode>,
) -> io::Result<()> {
    #[cfg(unix)]
    za_core::paths::create_private_file(&tmp.path)?;
    let mut f = open_checked(tmp, true)?;
    f.set_len(0)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    if let Some(p) = target_perms(old, mode) {
        f.set_permissions(p)?;
    }
    Ok(())
}

#[cfg(unix)]
fn target_perms(
    old: Option<&std::fs::Metadata>,
    mode: Option<Mode>,
) -> Option<std::fs::Permissions> {
    use std::os::unix::fs::PermissionsExt;
    let base = old.map_or(0o644, |m| m.permissions().mode() & 0o7777);
    let m = match mode {
        Some(Mode::Unix(m)) => m,
        Some(Mode::Git(true)) => base | ((base & 0o444) >> 2) | 0o100,
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

// ---- git ----

/// Пропуск чекпоинта git с причиной (`checkpoint_reason`).
struct Skip(&'static str);

#[derive(Clone, Copy)]
enum RunMode {
    Create,
    Restore,
}

/// Репозиторий проекта: корень рабочего дерева и подкаталог проекта в нём (`""` или
/// `a/b/`).
struct Repo {
    top: PathBuf,
    prefix: String,
}

/// Итог вызова git.
struct GitOut {
    code: i32,
    stdout: Vec<u8>,
}

/// Отказ вызова git.
enum GitFail {
    /// Запуск не удался.
    Spawn(String),
    /// Предел вызова.
    Timeout,
    /// Ненулевой код или сигнал.
    Failed(String),
}

impl GitFail {
    fn reason(&self) -> &'static str {
        match self {
            GitFail::Spawn(_) => checkpoint_reason::GIT_NOT_FOUND,
            GitFail::Timeout => checkpoint_reason::SLOW,
            GitFail::Failed(e) if e.contains("dubious ownership") => {
                checkpoint_reason::GIT_UNSAFE_DIRECTORY
            }
            GitFail::Failed(e) if e.contains("not a git repository") => checkpoint_reason::NO_GIT,
            GitFail::Failed(_) => checkpoint_reason::GIT_ERROR,
        }
    }

    fn text(&self) -> String {
        match self {
            GitFail::Spawn(e) | GitFail::Failed(e) => e.clone(),
            GitFail::Timeout => "превышен предел времени".into(),
        }
    }
}

impl From<GitFail> for Skip {
    fn from(f: GitFail) -> Self {
        tracing::warn!(error = %f.text(), "чекпоинт git не сделан");
        Skip(f.reason())
    }
}

impl From<GitFail> for RestoreError {
    fn from(f: GitFail) -> Self {
        RestoreError::Git(f.text())
    }
}

impl From<io::Error> for Skip {
    fn from(e: io::Error) -> Self {
        tracing::warn!(error = %e, "каталог чекпоинтов недоступен");
        Skip(checkpoint_reason::IO_ERROR)
    }
}

/// Безопасные пути вызовов git: пустой глобальный конфиг, пустой каталог хуков, `HOME`.
#[derive(Clone)]
struct SafePaths {
    empty: PathBuf,
    hooks: PathBuf,
    home: PathBuf,
}

/// Опции вызова git.
#[derive(Default)]
struct Call {
    stdin: Option<Vec<u8>>,
    /// Свой индекс (`GIT_INDEX_FILE`); без него — индекс пользователя (только чтение).
    index: Option<PathBuf>,
    /// Пути — буквально (`GIT_LITERAL_PATHSPECS=1`); `check-ignore` его не принимает.
    pathspec_magic: bool,
    capture: Option<usize>,
    /// Коды, которые не ошибка (кроме 0).
    ok: &'static [i32],
}

struct GitSession {
    runner: Arc<dyn Runner>,
    program: PathBuf,
    home: AgentHome,
    base_env: Vec<(OsString, OsString)>,
    timeout: Duration,
    safe: Option<SafePaths>,
}

impl GitSession {
    /// Каталоги безопасного запуска (один раз на сеанс).
    async fn safe(&mut self) -> io::Result<SafePaths> {
        if let Some(s) = &self.safe {
            return Ok(s.clone());
        }
        let home = self.home.clone();
        let s = blocking(move || {
            let root = agent_dir(&home, None)?;
            let hooks = agent_dir(&home, Some("no-hooks"))?;
            // Каталог хуков обязан быть пуст: иначе git исполнил бы его содержимое.
            if !read_dir_checked(&private(hooks.clone()))?.is_empty() {
                return Err(io::Error::other(format!(
                    "каталог {} не пуст",
                    hooks.display()
                )));
            }
            let git_home = agent_dir(&home, Some("home"))?;
            let empty = root.join("empty.gitconfig");
            if read_private(&empty, 1).map_or(true, |b| !b.is_empty()) {
                write_private(&empty, b"")?;
            }
            Ok(SafePaths {
                empty,
                hooks,
                home: git_home,
            })
        })
        .await?;
        self.safe = Some(s.clone());
        Ok(s)
    }

    async fn run(&mut self, cwd: &Path, args: &[&str], call: Call) -> Result<GitOut, GitFail> {
        let safe = self
            .safe()
            .await
            .map_err(|e| GitFail::Spawn(e.to_string()))?;
        let mut spec = CommandSpec::new(&self.program, cwd, self.timeout);
        let mut full: Vec<OsString> = vec!["--no-pager".into()];
        let opts: [(&str, OsString); 11] = [
            ("core.fsmonitor", "false".into()),
            ("core.useBuiltinFSMonitor", "false".into()),
            ("core.hooksPath", safe.hooks.clone().into_os_string()),
            ("core.untrackedCache", "false".into()),
            ("gc.auto", "0".into()),
            ("maintenance.auto", "false".into()),
            ("commit.gpgSign", "false".into()),
            ("core.autocrlf", "false".into()),
            ("core.safecrlf", "false".into()),
            ("core.attributesFile", safe.empty.clone().into_os_string()),
            ("submodule.recurse", "false".into()),
        ];
        for (k, v) in opts {
            let mut kv = OsString::from(k);
            kv.push("=");
            kv.push(v);
            full.push("-c".into());
            full.push(kv);
        }
        full.extend(args.iter().map(OsString::from));
        spec.args = full;
        spec.env_mode = EnvMode::Clear;
        let mut env = self.base_env.clone();
        let home = safe.home.clone().into_os_string();
        let vars: Vec<(&str, OsString)> = vec![
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_CONFIG_GLOBAL", safe.empty.clone().into_os_string()),
            ("HOME", home.clone()),
            ("XDG_CONFIG_HOME", home),
            ("GIT_OPTIONAL_LOCKS", "0".into()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
            ("GIT_PAGER", "cat".into()),
            ("PAGER", "cat".into()),
            ("GIT_NO_REPLACE_OBJECTS", "1".into()),
            ("GIT_ADVICE", "0".into()),
            ("LC_ALL", "C".into()),
            ("LANG", "C".into()),
            ("GIT_AUTHOR_NAME", IDENTITY.into()),
            ("GIT_AUTHOR_EMAIL", IDENTITY.into()),
            ("GIT_COMMITTER_NAME", IDENTITY.into()),
            ("GIT_COMMITTER_EMAIL", IDENTITY.into()),
            (
                "GIT_LITERAL_PATHSPECS",
                if call.pathspec_magic { "0" } else { "1" }.into(),
            ),
        ];
        for (k, v) in vars {
            env.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
            env.push((k.into(), v));
        }
        if let Some(i) = &call.index {
            env.push(("GIT_INDEX_FILE".into(), i.clone().into_os_string()));
        }
        spec.env_set = env;
        spec.stdin = call.stdin;
        spec.raw_stdout = true;
        if let Some(c) = call.capture {
            spec.capture_limit = c;
        }
        let out = self.runner.run(spec).await.map_err(|e| match e {
            RunnerError::Spawn { program, source } => {
                GitFail::Spawn(format!("{program}: {source}"))
            }
            RunnerError::Io(e) => GitFail::Failed(e.to_string()),
        })?;
        let shown = || args.first().copied().unwrap_or_default();
        let code = match out.exit {
            Exit::Code(c) => c,
            Exit::Timeout => return Err(GitFail::Timeout),
            Exit::Signal(s) => {
                return Err(GitFail::Failed(format!(
                    "git {} убит сигналом {s}",
                    shown()
                )));
            }
        };
        if out.stdout_truncated {
            return Err(GitFail::Failed(format!(
                "git {}: вывод больше предела",
                shown()
            )));
        }
        if code != 0 && !call.ok.contains(&code) {
            return Err(GitFail::Failed(format!(
                "git {} (код {code}): {}",
                shown(),
                out.stderr.trim()
            )));
        }
        Ok(GitOut {
            code,
            stdout: out.stdout_raw.unwrap_or_default(),
        })
    }

    /// Репозиторий проекта; `None` — проект не в рабочем дереве git.
    async fn repo(&mut self, project: &Path) -> Result<Option<Repo>, RestoreError> {
        match self.repo_or_skip(project).await {
            Ok(r) => Ok(Some(r)),
            Err(Skip(r))
                if r == checkpoint_reason::NO_GIT || r == checkpoint_reason::GIT_NOT_FOUND =>
            {
                Ok(None)
            }
            Err(Skip(r)) => Err(RestoreError::Git(format!(
                "репозиторий проекта недоступен: {r}"
            ))),
        }
    }

    async fn repo_or_skip(&mut self, project: &Path) -> Result<Repo, Skip> {
        let out = self
            .run(
                project,
                &["rev-parse", "--show-toplevel", "--show-prefix"],
                Call::default(),
            )
            .await?;
        let text = String::from_utf8(out.stdout)
            .map_err(|_| GitFail::Failed("rev-parse: путь не UTF-8".into()))?;
        let mut lines = text.lines();
        let top = lines.next().unwrap_or_default().trim_end_matches('\r');
        let prefix = lines.next().unwrap_or_default().trim_end_matches('\r');
        if top.is_empty() {
            return Err(Skip(checkpoint_reason::NO_GIT));
        }
        let repo = Repo {
            top: PathBuf::from(top),
            prefix: prefix.to_owned(),
        };
        // Проект в подкаталоге не должен игнорироваться репозиторием.
        if !repo.prefix.is_empty() {
            let dir = format!("./{}", repo.prefix.trim_end_matches('/'));
            let c = self
                .run(
                    &repo.top,
                    &["check-ignore", "-q", "--", &dir],
                    Call {
                        pathspec_magic: true,
                        ok: &[1],
                        ..Call::default()
                    },
                )
                .await?;
            if c.code == 0 {
                tracing::debug!(prefix = %repo.prefix, "проект игнорируется репозиторием");
                return Err(Skip(checkpoint_reason::NO_GIT));
            }
        }
        Ok(repo)
    }

    /// Аргумент pathspec подкаталога проекта (пусто — весь репозиторий).
    fn scope(repo: &Repo) -> Vec<String> {
        if repo.prefix.is_empty() {
            Vec::new()
        } else {
            vec!["--".into(), repo.prefix.trim_end_matches('/').to_owned()]
        }
    }

    /// Чекпоинт git (D89).
    async fn checkpoint(
        mut self,
        req: &CheckpointRequest,
        id: &str,
        stem: &str,
    ) -> Result<Checkpoint, Skip> {
        let repo = self.repo_or_skip(&req.project_dir).await?;
        let git_ref = format!("{CHECKPOINT_REF_PREFIX}/{id}");
        let found = |commit: String| Checkpoint {
            id: id.to_owned(),
            kind: checkpoint_kind::GIT.into(),
            git_ref: Some(git_ref.clone()),
            commit: Some(commit),
            reason: None,
        };
        // Повтор после сбоя: ссылка уже есть.
        if let Some(c) = self.ref_commit(&repo, &git_ref).await? {
            return Ok(found(c));
        }
        let run_dir = {
            let (home, run) = (self.home.clone(), req.run.to_string());
            blocking(move || agent_dir(&home, Some(&run))).await?
        };

        // 1. Список файлов.
        let scope = Self::scope(&repo);
        let mut args = vec!["ls-files", "-z", "-s", "-c"];
        args.extend(scope.iter().map(String::as_str));
        let tracked = self
            .run(
                &repo.top,
                &args,
                Call {
                    capture: Some(LIST_CAPTURE),
                    ..Call::default()
                },
            )
            .await?;
        let mut args = vec!["ls-files", "-z", "-o", "--exclude-standard"];
        args.extend(scope.iter().map(String::as_str));
        let untracked = self
            .run(
                &repo.top,
                &args,
                Call {
                    capture: Some(LIST_CAPTURE),
                    ..Call::default()
                },
            )
            .await?;
        let listed = parse_listing(&tracked.stdout, &untracked.stdout, &repo.prefix);

        // 2. stat и кэш.
        let (top, rd) = (repo.top.clone(), run_dir.clone());
        let plan = blocking(move || plan_entries(&top, &rd, listed)).await?;
        let mut oids = plan.known;
        if !plan.hash.is_empty() {
            let mut stdin = Vec::new();
            for (_, abs, _) in &plan.hash {
                stdin.extend_from_slice(abs.as_bytes());
                stdin.push(b'\n');
            }
            let out = self
                .run(
                    &repo.top,
                    &["hash-object", "--no-filters", "-w", "--stdin-paths"],
                    Call {
                        stdin: Some(stdin),
                        ..Call::default()
                    },
                )
                .await?;
            let text = String::from_utf8_lossy(&out.stdout);
            let got: Vec<&str> = text.split_whitespace().collect();
            if got.len() != plan.hash.len() || !got.iter().all(|o| is_oid(o)) {
                return Err(GitFail::Failed("hash-object: неожиданный вывод".into()).into());
            }
            for ((rel, _, st), oid) in plan.hash.into_iter().zip(got) {
                oids.insert(rel, (st, oid.to_owned()));
            }
        }

        // 3. Свой индекс и дерево.
        let index = run_dir.join(format!("{stem}.index"));
        {
            let i = index.clone();
            blocking(move || {
                for p in [i.clone(), i.with_extension("index.lock")] {
                    match remove_checked(&private(p)) {
                        Ok(()) => {}
                        Err(e) if is_missing(&e) => {}
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            })
            .await?;
        }
        let mut info = Vec::new();
        for (rel, (st, oid)) in &oids {
            info.extend_from_slice(format!("{} {oid}\t{rel}", st.mode).as_bytes());
            info.push(0);
        }
        if !info.is_empty() {
            self.run(
                &repo.top,
                &["update-index", "-z", "--index-info"],
                Call {
                    stdin: Some(info),
                    index: Some(index.clone()),
                    ..Call::default()
                },
            )
            .await?;
        }
        let tree = self
            .run(
                &repo.top,
                &["write-tree"],
                Call {
                    index: Some(index.clone()),
                    ..Call::default()
                },
            )
            .await?;
        let tree = String::from_utf8_lossy(&tree.stdout).trim().to_owned();
        if !is_oid(&tree) {
            return Err(GitFail::Failed("write-tree: неожиданный вывод".into()).into());
        }

        // 4. Коммит и ссылка.
        let parent = self.last_of_run(&repo, req.run).await?;
        let msg = format!(
            "zero-agent checkpoint {id}\n\n{PREFIX_TRAILER}{}\n",
            repo.prefix
        );
        let mut args = vec!["commit-tree", tree.as_str()];
        if let Some(p) = &parent {
            args.extend(["-p", p.as_str()]);
        }
        args.extend(["-m", msg.as_str()]);
        let commit = self.run(&repo.top, &args, Call::default()).await?;
        let commit = String::from_utf8_lossy(&commit.stdout).trim().to_owned();
        if !is_oid(&commit) {
            return Err(GitFail::Failed("commit-tree: неожиданный вывод".into()).into());
        }
        // Только создание: существующая ссылка не переписывается.
        if let Err(e) = self
            .run(
                &repo.top,
                &["update-ref", &git_ref, &commit, ""],
                Call::default(),
            )
            .await
        {
            if let Some(c) = self.ref_commit(&repo, &git_ref).await? {
                return Ok(found(c));
            }
            return Err(e.into());
        }

        // Кэш stat и свой индекс.
        let cache = StatCache {
            v: FORMAT_V,
            at_ns: now_ns(),
            entries: oids
                .into_iter()
                .filter(|(_, (st, _))| st.cacheable)
                .map(|(rel, (st, oid))| {
                    (
                        rel,
                        StatEntry {
                            size: st.size,
                            mtime_ns: st.mtime_ns,
                            mode: st.mode,
                            oid,
                        },
                    )
                })
                .collect(),
        };
        let rd = run_dir.clone();
        if let Err(e) = blocking(move || {
            let json = serde_json::to_vec(&cache).map_err(io::Error::other)?;
            write_private(&rd.join(STAT_FILE), &json)?;
            let _ = remove_checked(&private(index));
            let _ = std::fs::remove_dir_all(rd.join(LINKS_DIR));
            Ok(())
        })
        .await
        {
            tracing::warn!(error = %e, "кэш stat чекпоинтов не сохранён");
        }
        Ok(found(commit))
    }

    /// Коммит ссылки `git_ref`, если она есть.
    async fn ref_commit(&mut self, repo: &Repo, git_ref: &str) -> Result<Option<String>, GitFail> {
        let out = self
            .run(
                &repo.top,
                &["for-each-ref", "--format=%(objectname)", git_ref],
                Call::default(),
            )
            .await?;
        let s = String::from_utf8_lossy(&out.stdout);
        Ok(s.lines()
            .map(str::trim)
            .find(|l| is_oid(l))
            .map(str::to_owned))
    }

    /// Последний (по шагу и номеру) чекпоинт run.
    async fn last_of_run(&mut self, repo: &Repo, run: RunId) -> Result<Option<String>, GitFail> {
        let prefix = format!("{CHECKPOINT_REF_PREFIX}/{run}/");
        let out = self
            .run(
                &repo.top,
                &["for-each-ref", "--format=%(refname) %(objectname)", &prefix],
                Call::default(),
            )
            .await?;
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .filter_map(|l| {
                let (name, oid) = l.trim().split_once(' ')?;
                let id = name.strip_prefix(&format!("{CHECKPOINT_REF_PREFIX}/"))?;
                is_oid(oid).then(|| (sort_key(id), oid.to_owned()))
            })
            .max()
            .map(|(_, oid)| oid))
    }

    /// Чекпоинты git этого каталога проекта (по строке префикса в сообщении); `only` —
    /// один id.
    async fn refs(
        &mut self,
        repo: &Repo,
        only: Option<&str>,
    ) -> Result<Vec<Checkpoint>, RestoreError> {
        let pattern = match only {
            Some(id) => format!("{CHECKPOINT_REF_PREFIX}/{id}"),
            None => format!("{CHECKPOINT_REF_PREFIX}/"),
        };
        let out = self
            .run(
                &repo.top,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    &pattern,
                ],
                Call {
                    capture: Some(LIST_CAPTURE),
                    ..Call::default()
                },
            )
            .await?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut refs = Vec::new();
        for l in text.lines() {
            let Some((name, oid)) = l.trim().split_once(' ') else {
                continue;
            };
            let Some(id) = name.strip_prefix(&format!("{CHECKPOINT_REF_PREFIX}/")) else {
                continue;
            };
            if valid_id(id) && is_oid(oid) && only.is_none_or(|o| o == id) {
                refs.push((id.to_owned(), name.to_owned(), oid.to_owned()));
            }
        }
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        // Сообщения коммитов — одним `cat-file --batch`.
        let mut stdin = Vec::new();
        for (_, _, oid) in &refs {
            stdin.extend_from_slice(oid.as_bytes());
            stdin.push(b'\n');
        }
        let out = self
            .run(
                &repo.top,
                &["cat-file", "--batch"],
                Call {
                    stdin: Some(stdin),
                    capture: Some(LIST_CAPTURE),
                    ..Call::default()
                },
            )
            .await?;
        let objects = parse_batch(&out.stdout).map_err(RestoreError::Git)?;
        let want = format!("\n{PREFIX_TRAILER}{}\n", repo.prefix);
        let mut res = Vec::new();
        for ((id, name, oid), body) in refs.into_iter().zip(objects) {
            let ours = body
                .as_deref()
                .is_some_and(|b| String::from_utf8_lossy(b).contains(&want));
            if ours {
                res.push(Checkpoint {
                    id,
                    kind: checkpoint_kind::GIT.into(),
                    git_ref: Some(name),
                    commit: Some(oid),
                    reason: None,
                });
            }
        }
        Ok(res)
    }
}

// ---- список файлов и stat ----

/// Запись списка: режим и oid из индекса (у отслеживаемых), стадия конфликта.
struct Listed {
    /// Путь от корня репозитория.
    top_rel: String,
    /// Путь от корня проекта.
    rel: String,
    index_mode: Option<String>,
    index_oid: Option<String>,
}

/// Разбор `ls-files -z -s` и `-z -o`: по пути одна запись (у конфликта — режим стадии
/// 2 или любой), только под подкаталогом проекта; каталоги вложенных репозиториев
/// (`x/`) пропускаются.
fn parse_listing(tracked: &[u8], untracked: &[u8], prefix: &str) -> Vec<Listed> {
    let mut by_path: BTreeMap<String, Listed> = BTreeMap::new();
    for rec in tracked.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let Ok(rec) = std::str::from_utf8(rec) else {
            tracing::warn!("путь не UTF-8: в чекпоинт не входит");
            continue;
        };
        let Some((meta, path)) = rec.split_once('\t') else {
            continue;
        };
        let mut it = meta.split(' ');
        let (Some(mode), Some(oid), Some(stage)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let Some(rel) = path.strip_prefix(prefix) else {
            continue;
        };
        let e = by_path.entry(path.to_owned()).or_insert_with(|| Listed {
            top_rel: path.to_owned(),
            rel: rel.to_owned(),
            index_mode: None,
            index_oid: None,
        });
        if e.index_mode.is_none() || stage == "0" || stage == "2" {
            e.index_mode = Some(mode.to_owned());
            e.index_oid = (stage == "0").then(|| oid.to_owned());
        }
    }
    for rec in untracked.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let Ok(path) = std::str::from_utf8(rec) else {
            tracing::warn!("путь не UTF-8: в чекпоинт не входит");
            continue;
        };
        if path.ends_with('/') {
            continue;
        }
        let Some(rel) = path.strip_prefix(prefix) else {
            continue;
        };
        by_path.entry(path.to_owned()).or_insert_with(|| Listed {
            top_rel: path.to_owned(),
            rel: rel.to_owned(),
            index_mode: None,
            index_oid: None,
        });
    }
    by_path
        .into_values()
        .filter(|l| !l.rel.is_empty())
        .collect()
}

/// Файл кэша stat в каталоге run.
const STAT_FILE: &str = "stat.json";
/// Временные файлы с целями ссылок (содержимое blob ссылки).
const LINKS_DIR: &str = "links";

#[derive(Debug, Default, Serialize, Deserialize)]
struct StatCache {
    v: u32,
    /// Когда кэш записан (нс от эпохи).
    at_ns: u64,
    entries: BTreeMap<String, StatEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StatEntry {
    size: u64,
    mtime_ns: u64,
    mode: String,
    oid: String,
}

/// stat записи дерева.
#[derive(Debug, Clone)]
struct Stat {
    mode: String,
    size: u64,
    mtime_ns: u64,
    /// Входит ли в кэш (обычные файлы).
    cacheable: bool,
}

/// План: известные oid (кэш, gitlink) и файлы на хэширование `(rel, абсолютный путь,
/// stat)`.
struct Plan {
    known: BTreeMap<String, (Stat, String)>,
    hash: Vec<(String, String, Stat)>,
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

fn mtime_ns(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

fn plan_entries(top: &Path, run_dir: &Path, listed: Vec<Listed>) -> io::Result<Plan> {
    let cache: StatCache = read_private(&run_dir.join(STAT_FILE), 256 << 20)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|c: &StatCache| c.v == FORMAT_V)
        .unwrap_or_default();
    let trusted_before = cache
        .at_ns
        .saturating_sub(u64::try_from(RACY.as_nanos()).unwrap_or(0));
    let mut plan = Plan {
        known: BTreeMap::new(),
        hash: Vec::new(),
    };
    let mut links = 0usize;
    for l in listed {
        if l.top_rel.contains('\n') {
            tracing::warn!(path = %l.top_rel, "имя с переводом строки: в чекпоинт не входит");
            continue;
        }
        let abs = join_rel(top, &l.top_rel);
        let meta = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            // Удалён в рабочем дереве (или «не каталог» на пути): в чекпоинте его нет.
            Err(_) => continue,
        };
        let ft = meta.file_type();
        if ft.is_dir() {
            // Подмодуль: gitlink из индекса.
            if let (Some(m), Some(oid)) = (&l.index_mode, &l.index_oid)
                && m == MODE_GITLINK
            {
                plan.known.insert(
                    l.rel,
                    (
                        Stat {
                            mode: MODE_GITLINK.into(),
                            size: 0,
                            mtime_ns: 0,
                            cacheable: false,
                        },
                        oid.clone(),
                    ),
                );
            }
            continue;
        }
        if ft.is_symlink() {
            // Содержимое blob ссылки — её цель.
            let target = std::fs::read_link(&abs)?;
            let bytes = link_bytes(&target);
            let dir = run_dir.join(LINKS_DIR);
            make_private_dir(&dir)?;
            let tmp = dir.join(links.to_string());
            links += 1;
            write_private(&tmp, &bytes)?;
            plan.hash.push((
                l.rel,
                tmp.to_string_lossy().into_owned(),
                Stat {
                    mode: MODE_LINK.into(),
                    size: 0,
                    mtime_ns: 0,
                    cacheable: false,
                },
            ));
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let mode = file_mode(&meta, l.index_mode.as_deref());
        let st = Stat {
            mode,
            size: meta.len(),
            mtime_ns: mtime_ns(&meta),
            cacheable: true,
        };
        if let Some(c) = cache.entries.get(&l.rel)
            && c.size == st.size
            && c.mtime_ns == st.mtime_ns
            && c.mode == st.mode
            && st.mtime_ns < trusted_before
            && is_oid(&c.oid)
        {
            let oid = c.oid.clone();
            plan.known.insert(l.rel, (st, oid));
            continue;
        }
        plan.hash
            .push((l.rel, abs.to_string_lossy().into_owned(), st));
    }
    Ok(plan)
}

fn make_private_dir(dir: &Path) -> io::Result<()> {
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

#[cfg(unix)]
fn link_bytes(target: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    target.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn link_bytes(target: &Path) -> Vec<u8> {
    target.to_string_lossy().replace('\\', "/").into_bytes()
}

/// Режим обычного файла: ссылка, записанная файлом (`core.symlinks=false`), остаётся
/// ссылкой; Unix — бит исполнения; Windows — режим индекса.
fn file_mode(meta: &std::fs::Metadata, index: Option<&str>) -> String {
    if index == Some(MODE_LINK) {
        return MODE_LINK.into();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = index;
        if meta.permissions().mode() & 0o111 != 0 {
            MODE_EXEC.into()
        } else {
            MODE_FILE.into()
        }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        if index == Some(MODE_EXEC) {
            MODE_EXEC.into()
        } else {
            MODE_FILE.into()
        }
    }
}

// ---- восстановление git ----

/// Запись `ls-tree -r -z -l`.
struct TreeEntry {
    mode: String,
    oid: String,
    size: u64,
    rel: String,
}

fn parse_tree(out: &[u8]) -> Result<Vec<TreeEntry>, RestoreError> {
    let mut v = Vec::new();
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let rec = std::str::from_utf8(rec)
            .map_err(|_| RestoreError::Git("ls-tree: путь не UTF-8".into()))?;
        let (meta, path) = rec
            .split_once('\t')
            .ok_or_else(|| RestoreError::Git("ls-tree: неожиданный вывод".into()))?;
        let mut it = meta.split_whitespace();
        let (Some(mode), Some(_kind), Some(oid), Some(size)) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            return Err(RestoreError::Git("ls-tree: неожиданный вывод".into()));
        };
        v.push(TreeEntry {
            mode: mode.to_owned(),
            oid: oid.to_owned(),
            size: size.parse().unwrap_or(0),
            rel: path.to_owned(),
        });
    }
    Ok(v)
}

/// Разбор вывода `cat-file --batch`: содержимое по порядку запросов (`None` —
/// `missing`).
fn parse_batch(out: &[u8]) -> Result<Vec<Option<Vec<u8>>>, String> {
    let mut res = Vec::new();
    let mut pos = 0;
    while pos < out.len() {
        let nl = out[pos..]
            .iter()
            .position(|b| *b == b'\n')
            .ok_or("cat-file: обрыв заголовка")?;
        let header = String::from_utf8_lossy(&out[pos..pos + nl]).into_owned();
        pos += nl + 1;
        let parts: Vec<&str> = header.split(' ').collect();
        if parts.len() == 2 && parts[1] == "missing" {
            res.push(None);
            continue;
        }
        let size: usize = parts
            .get(2)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("cat-file: заголовок {header:?}"))?;
        let end = pos
            .checked_add(size)
            .filter(|e| *e < out.len() + 1 && out.get(*e) == Some(&b'\n'))
            .ok_or("cat-file: обрыв содержимого")?;
        res.push(Some(out[pos..end].to_vec()));
        pos = end + 1;
    }
    Ok(res)
}

async fn restore_git(
    git: &mut GitSession,
    repo: &Repo,
    roots: Roots,
    commit: &str,
    delete_new: bool,
) -> Result<Vec<PathBuf>, RestoreError> {
    let tree = git
        .run(
            &repo.top,
            &["ls-tree", "-r", "-z", "-l", "--full-tree", commit],
            Call {
                capture: Some(LIST_CAPTURE),
                ..Call::default()
            },
        )
        .await?;
    let entries = parse_tree(&tree.stdout)?;
    let scope = GitSession::scope(repo);
    let mut args = vec!["ls-files", "-z", "-c", "-o", "--exclude-standard"];
    args.extend(scope.iter().map(String::as_str));
    let current = git
        .run(
            &repo.top,
            &args,
            Call {
                capture: Some(LIST_CAPTURE),
                ..Call::default()
            },
        )
        .await?;
    let in_cp: HashSet<String> = entries.iter().map(|e| e.rel.clone()).collect();
    let mut extra: Vec<String> = Vec::new();
    if delete_new {
        let mut seen = HashSet::new();
        for rec in current.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let Ok(p) = std::str::from_utf8(rec) else {
                continue;
            };
            let Some(rel) = p.strip_prefix(repo.prefix.as_str()) else {
                continue;
            };
            if !rel.is_empty() && !rel.ends_with('/') && !in_cp.contains(rel) && seen.insert(rel) {
                extra.push(rel.to_owned());
            }
        }
    }

    // Проверка всех целей до записи.
    let blobs: Vec<TreeEntry> = entries
        .into_iter()
        .filter(|e| {
            let ok = e.mode == MODE_FILE || e.mode == MODE_EXEC;
            if !ok {
                tracing::warn!(path = %e.rel, mode = %e.mode, "ссылка или подмодуль не восстанавливается");
            }
            ok
        })
        .collect();
    let roots = Arc::new(roots);
    let (r2, rels, extra2) = (
        roots.clone(),
        blobs.iter().map(|e| e.rel.clone()).collect::<Vec<_>>(),
        extra.clone(),
    );
    let (targets, deletes) = blocking(move || {
        let mut t = HashMap::new();
        for rel in rels {
            if let Some(x) = target(&r2, &rel)? {
                t.insert(rel, x);
            }
        }
        let mut d = Vec::new();
        for rel in extra2 {
            if let Some(x) = target(&r2, &rel)?
                && x.exists
            {
                d.push(x);
            }
        }
        Ok((t, d))
    })
    .await?;
    let targets = Arc::new(Mutex::new(targets));

    // Содержимое порциями и запись отличающихся.
    let mut changed = Vec::new();
    let mut n = 0usize;
    let mut batch: Vec<&TreeEntry> = Vec::new();
    let mut bytes = 0u64;
    let wanted: Vec<&TreeEntry> = {
        let t = targets.lock().unwrap_or_else(|e| e.into_inner());
        blobs.iter().filter(|e| t.contains_key(&e.rel)).collect()
    };
    let mut chunks: Vec<Vec<&TreeEntry>> = Vec::new();
    for e in wanted {
        if !batch.is_empty() && bytes + e.size > BATCH_BYTES {
            chunks.push(std::mem::take(&mut batch));
            bytes = 0;
        }
        bytes += e.size;
        batch.push(e);
    }
    if !batch.is_empty() {
        chunks.push(batch);
    }
    for chunk in chunks {
        let mut stdin = Vec::new();
        let mut cap = 4096usize;
        for e in &chunk {
            stdin.extend_from_slice(e.oid.as_bytes());
            stdin.push(b'\n');
            cap = cap
                .saturating_add(usize::try_from(e.size).unwrap_or(usize::MAX))
                .saturating_add(128);
        }
        let out = git
            .run(
                &repo.top,
                &["cat-file", "--batch"],
                Call {
                    stdin: Some(stdin),
                    capture: Some(cap),
                    ..Call::default()
                },
            )
            .await?;
        let objects = parse_batch(&out.stdout).map_err(RestoreError::Git)?;
        if objects.len() != chunk.len() {
            return Err(RestoreError::Git("cat-file: не все объекты".into()));
        }
        let mut jobs = Vec::new();
        for (e, body) in chunk.iter().zip(objects) {
            let body = body.ok_or_else(|| RestoreError::Git(format!("объекта {} нет", e.oid)))?;
            jobs.push((e.rel.clone(), body, e.mode == MODE_EXEC, n));
            n += 1;
        }
        let (t, root) = (targets.clone(), roots.project.clone());
        let written = blocking(move || {
            let t = t.lock().unwrap_or_else(|e| e.into_inner());
            let mut w = Vec::new();
            for (rel, body, exec, i) in jobs {
                let Some(x) = t.get(&rel) else { continue };
                if write_target(&root, x, &body, Some(Mode::Git(exec)), i)? {
                    w.push(x.r.path.clone());
                }
            }
            Ok(w)
        })
        .await?;
        changed.extend(written);
    }
    if !deletes.is_empty() {
        let removed = blocking(move || {
            let mut v = Vec::new();
            for d in deletes {
                remove_checked(&d.r)?;
                v.push(d.r.path);
            }
            Ok(v)
        })
        .await?;
        changed.extend(removed);
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_names() {
        let run = RunId::new();
        let req = CheckpointRequest {
            project_dir: PathBuf::from("."),
            run,
            effect_key: EffectKey::derive(run, 12, 3),
            kind: ToolKind::Exec,
            paths: Vec::new(),
            trusted: true,
        };
        let (id, stem) = names(&req);
        assert_eq!(id, format!("{run}/12-3"));
        assert_eq!(stem, "12-3");
        assert!(valid_id(&id));
        for bad in [
            "", "x", "a/b", "../1-1", "a:b/1-1", "ab/1-", "ab/-1", "ab/1-1/x",
        ] {
            assert!(!valid_id(bad), "{bad}");
        }
        assert!(sort_key("a/2-1") < sort_key("a/10-1"));
    }

    #[test]
    fn batch_parsing() {
        let out = b"aa blob 3\nx\ny\nbb missing\ncc blob 0\n\n";
        let v = parse_batch(out).unwrap();
        assert_eq!(v, vec![Some(b"x\ny".to_vec()), None, Some(Vec::new())]);
        assert!(parse_batch(b"aa blob 5\nxy\n").is_err());
    }

    #[test]
    fn listing_dedups_conflicts_and_scopes_prefix() {
        let o1 = "a".repeat(40);
        let o2 = "b".repeat(40);
        let tracked = format!(
            "100644 {o1} 1\tsub/f\0100755 {o2} 2\tsub/f\0100644 {o1} 3\tsub/f\0160000 {o2} 0\tsub/m\0100644 {o1} 0\tother\0"
        );
        let l = parse_listing(tracked.as_bytes(), b"sub/n\0sub/nested/\0", "sub/");
        let names: Vec<_> = l
            .iter()
            .map(|x| (x.rel.as_str(), x.index_mode.as_deref()))
            .collect();
        assert_eq!(
            names,
            vec![("f", Some("100755")), ("m", Some("160000")), ("n", None)]
        );
        assert_eq!(l[0].index_oid, None, "у конфликта нет oid стадии 0");
    }
}
