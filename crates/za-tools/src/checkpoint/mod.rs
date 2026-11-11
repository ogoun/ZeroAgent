//! Чекпоинты рабочей копии и восстановление (R1-C1 D89, TOOL-009; Ruling 7.15 «безопасный
//! запуск git», 1.13, 5.3–5.7; ревью задачи 9: C1, I1–I3).
//!
//! [`GitCheckpointer`] делает чекпоинт перед каждым исполняемым вызовом вида `Edit` или
//! `Exec`. Имя — от ключа эффекта: `id = "<run>/<step>-<seq>"` (без `:`); если такой
//! чекпоинт уже есть (повтор после сбоя), возвращается он.
//!
//! **git** — только в доверенном проекте, у которого `.git` лежит в корне проекта и
//! прошёл статическую проверку (модуль `repo`, C1): без следования ссылкам, `.git` или
//! цель `gitdir:` внутри проекта, альтернативные хранилища объектов только внутри
//! проекта, в конфиге нет `include`/`includeIf`, частичного клона и промисоров, ключей,
//! исполняющих команды, и сетевых путей; сетевой путь отвергается лексически, до
//! обращения. Не прошёл — git не вызывается, `Edit` получает копию файлов, `Exec` —
//! `skipped` с причиной `git_repo_unsafe`. Поиск репозитория не выходит выше корня
//! проекта (`GIT_CEILING_DIRECTORIES` — родитель корня). Только plumbing (модуль `git`),
//! каждый вызов — через [`za_jail::Runner`] без сети и ленивой догрузки объектов
//! (`GIT_NO_LAZY_FETCH=1`, `-c protocol.allow=never`). HEAD, индекс, ветки, stash и
//! рабочее дерево пользователя не меняются; в репозиторий добавляются только объекты и
//! скрытая ссылка [`CHECKPOINT_REF_PREFIX`]`/<run>/<step>-<seq>`.
//!
//! **Копия файлов** — для `Edit` в недоверенном проекте (git там не вызывается вовсе, D83)
//! и там, где git недоступен или небезопасен: прежнее содержимое затронутых файлов внутри
//! проекта копируется в `ZA_HOME/projects/<project_id>/checkpoints/<run>/<step>-<seq>/` с
//! `manifest.json` (модуль `files`).
//!
//! **Пределы** (I3): файл больше [`CHECKPOINT_MAX_FILE`] в чекпоинт не входит (пометка в
//! сообщении коммита или манифесте; такой файл восстановление не трогает и не удаляет),
//! новых байт на чекпоинт — не больше [`CHECKPOINT_MAX_TOTAL`], иначе `too_large`.
//! Время — [`CHECKPOINT_TIMEOUT`] на чекпоинт git; превышение — `slow`, и до конца run git
//! не вызывается.
//!
//! **Восстановление** ([`restore`]) — только по чекпоинту, найденному для этого проекта, и
//! только после успешного чекпоинта «до» ([`backup_before_restore`], I1): git — если
//! получилось, иначе копии всех затрагиваемых файлов. Цели проверяются все до первой
//! записи (модуль `write`); содержимое пишется потоком во временный файл и сравнивается с
//! текущим потоком, без удержания файла в памяти.
//!
//! **Доверие** (I2): `list`, `find`, `restore` и `backup_before_restore` принимают
//! `trusted`; без доверия git не вызывается, видны и восстанавливаются только копии файлов.

mod files;
mod git;
mod home;
mod repo;
mod write;

use std::collections::HashSet;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use za_core::paths::Roots;
use za_core::{BoxFuture, Checkpoint, CheckpointRequest, Checkpointer, ToolKind};
use za_jail::Runner;
use za_protocol::{EffectKey, RunId, checkpoint_kind, checkpoint_reason};

use crate::AgentHome;

/// Предел чекпоинта git целиком при вызове инструмента (D89).
pub const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(20);
/// Пространство скрытых ссылок чекпоинтов.
pub const CHECKPOINT_REF_PREFIX: &str = "refs/zero-agent/checkpoints";
/// Файл больше этого в чекпоинт не входит (I3).
pub const CHECKPOINT_MAX_FILE: u64 = 64 << 20;
/// Сколько новых байт (хэшируемых git или копируемых) самое большее на один чекпоинт (I3).
pub const CHECKPOINT_MAX_TOTAL: u64 = 1 << 30;

/// Предел чекпоинта «до» и вызовов git при восстановлении (команда пользователя).
const RESTORE_TIMEOUT: Duration = Duration::from_secs(300);

/// Ошибка восстановления и поиска чекпоинта.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("чекпоинт {0} не найден")]
    NotFound(String),
    #[error("git: {0}")]
    Git(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Чекпоинт «до» не сделан: восстановление не выполняется (I1).
    #[error("чекпоинт перед восстановлением не сделан ({0}); восстановление не выполнено")]
    NoBackup(String),
    /// Чекпоинт git в недоверенном проекте (I2, D83).
    #[error(
        "проект не доверен: чекпоинты git недоступны (zero-agent trust); доступны копии файлов"
    )]
    Untrusted,
}

/// Чекпоинты рабочей копии: git plumbing или копия файлов (D89).
#[derive(Clone)]
pub struct GitCheckpointer {
    runner: Arc<dyn Runner>,
    git: Option<PathBuf>,
    home: AgentHome,
    /// Дополнительные переменные каждого вызова git (тесты). Переменные безопасности их
    /// перекрывают.
    base_env: Vec<(OsString, OsString)>,
    /// Статическая проверка репозитория (только тесты второго рубежа отключают её).
    repo_check: bool,
    /// Предел чекпоинта «до» (тесты сокращают).
    backup_timeout: Duration,
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
            .field("repo_check", &self.repo_check)
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
            repo_check: true,
            backup_timeout: RESTORE_TIMEOUT,
            slow: Arc::new(Mutex::new(HashSet::new())),
            lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Для тестов: переменные, добавляемые к окружению каждого вызова git.
    pub fn with_base_env(mut self, env: Vec<(OsString, OsString)>) -> Self {
        self.base_env = env;
        self
    }

    /// Только для тестов второго рубежа (опции и окружение git): без статической проверки
    /// репозитория.
    #[doc(hidden)]
    pub fn without_repo_check_for_tests(mut self) -> Self {
        self.repo_check = false;
        self
    }

    /// Только для тестов: предел чекпоинта «до» восстановления.
    #[doc(hidden)]
    pub fn with_backup_timeout_for_tests(mut self, d: Duration) -> Self {
        self.backup_timeout = d;
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

    /// Репозиторий проекта для git или причина отказа (статическая проверка, C1).
    async fn repo(&self, project: &Path) -> Result<git::Repo, &'static str> {
        let (p, h, check) = (
            project.to_path_buf(),
            self.home.za_home.clone(),
            self.repo_check,
        );
        let r = blocking(move || {
            let roots = Roots::canonical(&p, &h)?;
            if !check {
                return Ok(Ok(roots.project));
            }
            Ok(repo::check(&roots).map(|()| roots.project))
        })
        .await;
        match r {
            Ok(Ok(top)) => Ok(git::Repo { top }),
            Ok(Err(reason)) => Err(reason),
            Err(e) => {
                tracing::warn!(error = %e, "корень проекта не канонизирован");
                Err(checkpoint_reason::IO_ERROR)
            }
        }
    }

    /// Чекпоинт git: `Ok` — сделан (или найден), `Err` — причина пропуска.
    async fn git_checkpoint(
        &self,
        req: &CheckpointRequest,
        id: &str,
        stem: &str,
        timeout: Duration,
    ) -> Result<git::Made, &'static str> {
        if self.is_slow(req.run) {
            return Err(checkpoint_reason::SLOW);
        }
        let Some(program) = self.git.clone() else {
            return Err(checkpoint_reason::GIT_NOT_FOUND);
        };
        let repo = self.repo(&req.project_dir).await?;
        let session = self.session(program, timeout);
        match tokio::time::timeout(timeout, session.checkpoint(&repo, req, id, stem)).await {
            Ok(Ok(made)) => Ok(made),
            Ok(Err(git::Skip(r))) => {
                if r == checkpoint_reason::SLOW {
                    self.mark_slow(req.run);
                }
                Err(r)
            }
            Err(_) => {
                tracing::warn!(id = %id, "чекпоинт git не уложился в предел, до конца run — без git");
                self.mark_slow(req.run);
                Err(checkpoint_reason::SLOW)
            }
        }
    }

    async fn create_inner(self, req: CheckpointRequest) -> Checkpoint {
        let _one = self.lock.lock().await;
        let (id, stem) = names(&req);
        let edit = req.kind == ToolKind::Edit;
        // Недоверенный проект: git не вызывается (D83).
        let reason = if req.trusted {
            match self
                .git_checkpoint(&req, &id, &stem, CHECKPOINT_TIMEOUT)
                .await
            {
                Ok(made) => return made.cp,
                Err(r) => r,
            }
        } else {
            checkpoint_reason::NO_GIT
        };
        if edit {
            self.files(&req, &id, &stem, false).await
        } else {
            skipped(&id, reason)
        }
    }

    /// Копия затронутых файлов (D89); `strict` — файл сверх предела тоже ошибка (чекпоинт
    /// «до» восстановления).
    async fn files(
        &self,
        req: &CheckpointRequest,
        id: &str,
        stem: &str,
        strict: bool,
    ) -> Checkpoint {
        let home = self.home.clone();
        let req = req.clone();
        let stem2 = stem.to_owned();
        match blocking(move || files::copy(&home, &req, &stem2, strict)).await {
            Ok(()) => Checkpoint {
                id: id.to_owned(),
                kind: checkpoint_kind::FILES.into(),
                git_ref: None,
                commit: None,
                reason: None,
            },
            Err(e) => {
                tracing::warn!(id = %id, error = %e, "копия файлов чекпоинта не сделана");
                let reason = if e.kind() == io::ErrorKind::FileTooLarge {
                    checkpoint_reason::TOO_LARGE
                } else {
                    checkpoint_reason::IO_ERROR
                };
                skipped(id, reason)
            }
        }
    }

    fn session(&self, program: PathBuf, timeout: Duration) -> git::Session {
        git::Session::new(
            self.runner.clone(),
            program,
            self.home.clone(),
            self.base_env.clone(),
            timeout,
        )
    }

    /// Сеанс git для чтения чекпоинтов (`list`, `find`, `restore`): только в доверенном
    /// проекте с безопасным репозиторием; иначе `None`.
    async fn read_session(
        &self,
        project: &Path,
        trusted: bool,
    ) -> Option<(git::Session, git::Repo)> {
        if !trusted {
            return None;
        }
        let program = self.git.clone()?;
        match self.repo(project).await {
            Ok(repo) => Some((self.session(program, RESTORE_TIMEOUT), repo)),
            Err(r) => {
                tracing::debug!(reason = r, "git для чекпоинтов проекта не используется");
                None
            }
        }
    }

    /// Чекпоинты проекта `project` (канонический корень): скрытые ссылки этого проекта
    /// (только при `trusted`, I2) и копии файлов в `ZA_HOME`; по возрастанию id.
    pub async fn list(
        &self,
        project: &Path,
        trusted: bool,
    ) -> Result<Vec<Checkpoint>, RestoreError> {
        let mut out = Vec::new();
        if let Some((mut s, repo)) = self.read_session(project, trusted).await {
            out.extend(s.refs(&repo, None).await?.into_iter().map(|r| r.cp));
        }
        let home = self.home.clone();
        out.extend(blocking(move || files::list(&home)).await?);
        out.sort_by_key(|c| sort_key(&c.id));
        Ok(out)
    }

    /// Чекпоинт по id `<run>/<step>-<seq>`; неверный, чужой или (без доверия) git —
    /// [`RestoreError::NotFound`]; один id у двух видов — ошибка (M8).
    pub async fn find(
        &self,
        project: &Path,
        id: &str,
        trusted: bool,
    ) -> Result<Checkpoint, RestoreError> {
        if !valid_id(id) {
            return Err(RestoreError::NotFound(id.to_owned()));
        }
        let mut git_cp = None;
        if let Some((mut s, repo)) = self.read_session(project, trusted).await {
            git_cp = s
                .refs(&repo, Some(id))
                .await?
                .into_iter()
                .next()
                .map(|r| r.cp);
        }
        let home = self.home.clone();
        let id2 = id.to_owned();
        let has_files = blocking(move || files::exists(&home, &id2)).await?;
        match (git_cp, has_files) {
            (Some(_), true) => Err(RestoreError::Git(format!(
                "id {id} есть и у чекпоинта git, и у копии файлов"
            ))),
            (Some(cp), false) => Ok(cp),
            (None, true) => Ok(files_cp(id)),
            (None, false) => Err(RestoreError::NotFound(id.to_owned())),
        }
    }
}

impl Checkpointer for GitCheckpointer {
    fn create(&self, req: CheckpointRequest) -> BoxFuture<'static, Checkpoint> {
        Box::pin(self.clone().create_inner(req))
    }
}

/// Чекпоинт перед восстановлением `cp` (D89, I1). Для чекпоинта git (только `trusted`) —
/// git с пределом команды пользователя; если не вышло или в нём не хватает файлов,
/// которые тронет восстановление, — копии всех этих файлов. Для копии файлов — копии тех
/// же файлов. Файл сверх предела или сбой — [`RestoreError::NoBackup`]: восстанавливать
/// без чекпоинта «до» нельзя.
pub async fn backup_before_restore(
    ck: &GitCheckpointer,
    project: &Path,
    cp: &Checkpoint,
    delete_new: bool,
    trusted: bool,
) -> Result<Checkpoint, RestoreError> {
    let _one = ck.lock.lock().await;
    let run = RunId::new();
    let req = |kind, paths, trusted| CheckpointRequest {
        project_dir: project.to_path_buf(),
        run,
        effect_key: EffectKey::derive(run, 0, 0),
        kind,
        paths,
        trusted,
    };
    let (id, stem) = names(&req(ToolKind::Edit, Vec::new(), false));
    let paths: Vec<PathBuf> = match cp.kind.as_str() {
        checkpoint_kind::GIT => {
            let (mut s, repo, commit) = git_parts(ck, project, cp, trusted).await?;
            let plan = s.plan_paths(&repo, &commit, delete_new).await?;
            let made = ck
                .git_checkpoint(
                    &req(ToolKind::Exec, Vec::new(), true),
                    &id,
                    &stem,
                    ck.backup_timeout,
                )
                .await;
            match made {
                Ok(m) if plan.iter().all(|p| !m.skipped.contains(p)) => return Ok(m.cp),
                Ok(_) => tracing::warn!("в чекпоинте git «до» нет больших файлов: копии файлов"),
                Err(r) => tracing::warn!(reason = r, "чекпоинт git «до» не сделан: копии файлов"),
            }
            plan.iter().map(|rel| join_rel(project, rel)).collect()
        }
        checkpoint_kind::FILES => {
            let (home, cid) = (ck.home.clone(), cp.id.clone());
            let rels = blocking(move || files::paths(&home, &cid)).await?;
            rels.iter().map(|rel| join_rel(project, rel)).collect()
        }
        _ => return Err(RestoreError::NotFound(cp.id.clone())),
    };
    let b = ck
        .files(&req(ToolKind::Edit, paths, false), &id, &stem, true)
        .await;
    if b.kind == checkpoint_kind::FILES {
        Ok(b)
    } else {
        Err(RestoreError::NoBackup(
            b.reason.unwrap_or_else(|| "unknown".into()),
        ))
    }
}

/// Восстанавливает рабочую копию проекта `project` (канонический корень) по чекпоинту
/// `cp`. Возвращает изменённые (записанные и удалённые) пути. Чекпоинт git — только
/// `trusted` (I2). Чекпоинт «до» делает вызывающий ([`backup_before_restore`]).
pub async fn restore(
    ck: &GitCheckpointer,
    project: &Path,
    cp: &Checkpoint,
    delete_new: bool,
    trusted: bool,
) -> Result<Vec<PathBuf>, RestoreError> {
    let _one = ck.lock.lock().await;
    let roots = {
        let (p, h) = (project.to_path_buf(), ck.home.za_home.clone());
        blocking(move || Roots::canonical(&p, &h)).await?
    };
    match cp.kind.as_str() {
        checkpoint_kind::GIT => {
            let (mut s, repo, commit) = git_parts(ck, project, cp, trusted).await?;
            s.restore(&repo, roots, &commit, delete_new).await
        }
        checkpoint_kind::FILES => {
            let home = ck.home.clone();
            let id = cp.id.clone();
            blocking(move || Ok(files::restore(&home, &roots, &id, delete_new))).await?
        }
        _ => Err(RestoreError::NotFound(cp.id.clone())),
    }
}

/// Сеанс, репозиторий и коммит чекпоинта git: только доверенный проект с безопасным
/// репозиторием и чекпоинт этого проекта.
async fn git_parts(
    ck: &GitCheckpointer,
    project: &Path,
    cp: &Checkpoint,
    trusted: bool,
) -> Result<(git::Session, git::Repo, String), RestoreError> {
    if !trusted {
        return Err(RestoreError::Untrusted);
    }
    let commit = cp
        .commit
        .clone()
        .filter(|c| is_oid(c))
        .ok_or_else(|| RestoreError::NotFound(cp.id.clone()))?;
    let program = ck
        .git
        .clone()
        .ok_or_else(|| RestoreError::Git("git не найден в PATH".into()))?;
    let repo = ck
        .repo(project)
        .await
        .map_err(|r| RestoreError::Git(format!("репозиторий проекта не используется: {r}")))?;
    let mut s = ck.session(program, RESTORE_TIMEOUT);
    let ours = s.refs(&repo, Some(&cp.id)).await?;
    if !ours
        .iter()
        .any(|r| r.cp.commit.as_deref() == Some(commit.as_str()))
    {
        return Err(RestoreError::NotFound(cp.id.clone()));
    }
    Ok((s, repo, commit))
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

fn files_cp(id: &str) -> Checkpoint {
    Checkpoint {
        id: id.to_owned(),
        kind: checkpoint_kind::FILES.into(),
        git_ref: None,
        commit: None,
        reason: None,
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
}
