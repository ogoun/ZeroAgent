//! Чекпоинт git и восстановление из него — только plumbing через `za_jail::Runner`
//! (D89, Ruling 7.15; ревью задачи 9: C1, I3, M1–M5).
//!
//! Создание: `ls-files -z -s -c` и `-z -o --exclude-standard` (индекс пользователя только
//! читается) → `hash-object --no-filters -w --stdin-paths` для файлов, чей stat отличается
//! от кэша прошлого чекпоинта run (`size`, `mtime`, режим, на Unix ещё `ctime` и `ino`;
//! запись моложе 2 с до сохранения кэша не доверяется) → `update-index -z --index-info` в
//! свой индекс → `write-tree` → `commit-tree` → `update-ref <ref> <commit> ""`. Сообщение
//! коммита несёт `zero-agent-project: <project_id>` и `zero-agent-skipped: <путь>` для
//! файлов сверх [`CHECKPOINT_MAX_FILE`]: такие файлы восстановление не трогает и не
//! удаляет.
//!
//! Восстановление: `ls-tree -r -z -l --full-tree`, мелкие объекты — `cat-file --batch`
//! порциями, крупные (больше [`STREAM_FROM`]) — `cat-file blob` прямо во временный файл
//! (`CommandSpec::stdout_file`), без удержания в памяти.
//!
//! Каждый вызов: `EnvMode::Clear`, `GIT_CONFIG_NOSYSTEM=1`, пустой глобальный конфиг, свой
//! `HOME`, `GIT_CEILING_DIRECTORIES` — родитель корня проекта (поиск не выходит выше),
//! `GIT_NO_LAZY_FETCH=1`, `GIT_PROTOCOL_FROM_USER=0`, `--no-pager` и опции `-c`, которые
//! отключают fsmonitor, хуки (`core.hooksPath` — несуществующий путь), сеть
//! (`protocol.allow=never`), помощников учётных данных, журнал ссылок и разделённый индекс.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use za_core::paths::{Roots, remove_checked};
use za_core::{Checkpoint, CheckpointRequest};
use za_jail::{CommandSpec, EnvMode, Exit, Runner, RunnerError};
use za_protocol::{RunId, checkpoint_kind, checkpoint_reason};

use super::home::{agent_dir, is_missing, make_private_dir, private, read_private, write_private};
use super::write::{Mode, Target, Tmp, fill, finish, target};
use super::{
    CHECKPOINT_MAX_FILE, CHECKPOINT_MAX_TOTAL, CHECKPOINT_REF_PREFIX, RestoreError, blocking,
    is_oid, join_rel, sort_key, valid_id,
};
use crate::AgentHome;

/// Предел вывода git со списками файлов и ссылок.
const LIST_CAPTURE: usize = 64 << 20;
/// Сколько байт содержимого мелких объектов читается одним `cat-file --batch`.
const BATCH_BYTES: u64 = 32 << 20;
/// Объект больше этого восстанавливается потоком в файл.
const STREAM_FROM: u64 = 1 << 20;
/// Запись кэша stat моложе этого (до сохранения кэша) не доверяется («racy git»).
const RACY: Duration = Duration::from_secs(2);
/// Версия кэша stat.
const STAT_V: u32 = 2;
/// Файл кэша stat в каталоге run.
const STAT_FILE: &str = "stat.json";
/// Временные файлы с целями ссылок (содержимое blob ссылки).
const LINKS_DIR: &str = "links";
/// Идентичность коммитов чекпоинтов (Ruling 5.4).
const IDENTITY: &str = "zero-agent";
/// Строки сообщения коммита.
const PROJECT_TRAILER: &str = "zero-agent-project: ";
const SKIPPED_TRAILER: &str = "zero-agent-skipped: ";
/// Файловые режимы git.
const MODE_FILE: &str = "100644";
const MODE_EXEC: &str = "100755";
const MODE_LINK: &str = "120000";
const MODE_GITLINK: &str = "160000";

/// Репозиторий проекта: корень рабочего дерева — корень проекта (статическая проверка).
pub(super) struct Repo {
    pub top: PathBuf,
}

/// Сделанный чекпоинт git и пропущенные (сверх предела) пути.
pub(super) struct Made {
    pub cp: Checkpoint,
    pub skipped: HashSet<String>,
}

/// Пропуск чекпоинта git с причиной (`checkpoint_reason`).
pub(super) struct Skip(pub &'static str);

/// Итог вызова git.
struct GitOut {
    stdout: Vec<u8>,
}

/// Отказ вызова git.
enum GitFail {
    /// Запуск не удался.
    Spawn(String),
    /// Каталоги агента недоступны (M2).
    Io(String),
    /// Предел вызова.
    Timeout,
    /// Ненулевой код или сигнал.
    Failed(String),
}

impl GitFail {
    fn reason(&self) -> &'static str {
        match self {
            GitFail::Spawn(_) => checkpoint_reason::GIT_NOT_FOUND,
            GitFail::Io(_) => checkpoint_reason::IO_ERROR,
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
            GitFail::Spawn(e) | GitFail::Io(e) | GitFail::Failed(e) => e.clone(),
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

/// Безопасные пути вызовов git: пустой глобальный конфиг, несуществующий каталог хуков,
/// `HOME`.
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
    capture: Option<usize>,
    /// stdout — в этот файл.
    stdout_file: Option<Arc<std::fs::File>>,
}

/// Сеанс вызовов git одного чекпоинта или восстановления.
pub(super) struct Session {
    runner: Arc<dyn Runner>,
    program: PathBuf,
    home: AgentHome,
    base_env: Vec<(OsString, OsString)>,
    timeout: Duration,
    safe: Option<SafePaths>,
}

/// Чекпоинт из скрытой ссылки этого проекта.
pub(super) struct RefCp {
    pub cp: Checkpoint,
    pub skipped: HashSet<String>,
}

impl Session {
    pub(super) fn new(
        runner: Arc<dyn Runner>,
        program: PathBuf,
        home: AgentHome,
        base_env: Vec<(OsString, OsString)>,
        timeout: Duration,
    ) -> Self {
        Self {
            runner,
            program,
            home,
            base_env,
            timeout,
            safe: None,
        }
    }

    /// Каталоги безопасного запуска (один раз на сеанс).
    async fn safe(&mut self) -> io::Result<SafePaths> {
        if let Some(s) = &self.safe {
            return Ok(s.clone());
        }
        let home = self.home.clone();
        let s = blocking(move || {
            let root = agent_dir(&home, None)?;
            // Хуки — несуществующий путь внутри своего закрытого каталога (M1).
            let hooks = agent_dir(&home, Some("no-hooks"))?.join("none");
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

    async fn run(
        &mut self,
        repo: &Repo,
        args: &[&str],
        call: Call,
        ok: &[i32],
    ) -> Result<GitOut, GitFail> {
        let safe = self.safe().await.map_err(|e| GitFail::Io(e.to_string()))?;
        let mut spec = CommandSpec::new(&self.program, &repo.top, self.timeout);
        let mut full: Vec<OsString> = vec!["--no-pager".into()];
        let opts: [(&str, OsString); 18] = [
            ("core.fsmonitor", "false".into()),
            ("core.useBuiltinFSMonitor", "false".into()),
            ("core.hooksPath", safe.hooks.clone().into_os_string()),
            ("core.untrackedCache", "false".into()),
            ("core.splitIndex", "false".into()),
            ("core.logAllRefUpdates", "false".into()),
            ("gc.auto", "0".into()),
            ("maintenance.auto", "false".into()),
            ("commit.gpgSign", "false".into()),
            ("core.autocrlf", "false".into()),
            ("core.safecrlf", "false".into()),
            ("core.attributesFile", safe.empty.clone().into_os_string()),
            ("submodule.recurse", "false".into()),
            // Сеть и ленивая догрузка объектов (C1).
            ("protocol.allow", "never".into()),
            ("core.sshCommand", "".into()),
            ("core.askPass", "".into()),
            ("credential.helper", "".into()),
            ("fetch.recurseSubmodules", "false".into()),
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
        let ceiling = repo
            .top
            .parent()
            .map(|p| p.as_os_str().to_owned())
            .unwrap_or_default();
        let vars: Vec<(&str, OsString)> = vec![
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_CONFIG_GLOBAL", safe.empty.clone().into_os_string()),
            ("HOME", home.clone()),
            ("XDG_CONFIG_HOME", home),
            ("GIT_CEILING_DIRECTORIES", ceiling),
            ("GIT_NO_LAZY_FETCH", "1".into()),
            ("GIT_PROTOCOL_FROM_USER", "0".into()),
            ("GIT_ASKPASS", "".into()),
            ("SSH_ASKPASS", "".into()),
            ("GIT_OPTIONAL_LOCKS", "0".into()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
            ("GIT_PAGER", "cat".into()),
            ("PAGER", "cat".into()),
            ("GIT_NO_REPLACE_OBJECTS", "1".into()),
            ("GIT_LITERAL_PATHSPECS", "1".into()),
            ("GIT_ADVICE", "0".into()),
            ("LC_ALL", "C".into()),
            ("LANG", "C".into()),
            ("GIT_AUTHOR_NAME", IDENTITY.into()),
            ("GIT_AUTHOR_EMAIL", IDENTITY.into()),
            ("GIT_COMMITTER_NAME", IDENTITY.into()),
            ("GIT_COMMITTER_EMAIL", IDENTITY.into()),
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
        spec.stdout_file = call.stdout_file;
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
        if code != 0 && !ok.contains(&code) {
            return Err(GitFail::Failed(format!(
                "git {} (код {code}): {}",
                shown(),
                out.stderr.trim()
            )));
        }
        Ok(GitOut {
            stdout: out.stdout_raw.unwrap_or_default(),
        })
    }

    async fn simple(&mut self, repo: &Repo, args: &[&str]) -> Result<GitOut, GitFail> {
        self.run(repo, args, Call::default(), &[]).await
    }

    async fn listing(&mut self, repo: &Repo, args: &[&str]) -> Result<GitOut, GitFail> {
        let call = Call {
            capture: Some(LIST_CAPTURE),
            ..Call::default()
        };
        self.run(repo, args, call, &[]).await
    }

    /// Чекпоинт git (D89).
    pub(super) async fn checkpoint(
        mut self,
        repo: &Repo,
        req: &CheckpointRequest,
        id: &str,
        stem: &str,
    ) -> Result<Made, Skip> {
        let git_ref = format!("{CHECKPOINT_REF_PREFIX}/{id}");
        // Повтор после сбоя: ссылка этого проекта уже есть.
        if let Some(found) = self.existing(repo, id, &git_ref).await? {
            return Ok(found);
        }
        let run_dir = {
            let (home, run) = (self.home.clone(), req.run.to_string());
            blocking(move || agent_dir(&home, Some(&run))).await?
        };

        // 1. Список файлов (индекс пользователя только читается).
        let tracked = self.listing(repo, &["ls-files", "-z", "-s", "-c"]).await?;
        let untracked = self
            .listing(repo, &["ls-files", "-z", "-o", "--exclude-standard"])
            .await?;
        let listed = parse_listing(&tracked.stdout, &untracked.stdout);

        // 2. stat, кэш и пределы.
        let (top, rd) = (repo.top.clone(), run_dir.clone());
        let plan = blocking(move || plan_entries(&top, &rd, listed)).await?;
        let total: u64 = plan.hash.iter().map(|(_, _, st)| st.size).sum();
        if total > CHECKPOINT_MAX_TOTAL {
            tracing::warn!(total, "новых байт больше предела чекпоинта");
            return Err(Skip(checkpoint_reason::TOO_LARGE));
        }
        let mut oids = plan.known;
        if !plan.hash.is_empty() {
            let mut stdin = Vec::new();
            for (_, abs, _) in &plan.hash {
                stdin.extend_from_slice(abs.as_bytes());
                stdin.push(b'\n');
            }
            let call = Call {
                stdin: Some(stdin),
                ..Call::default()
            };
            let out = self
                .run(
                    repo,
                    &["hash-object", "--no-filters", "-w", "--stdin-paths"],
                    call,
                    &[],
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
            let call = Call {
                stdin: Some(info),
                index: Some(index.clone()),
                ..Call::default()
            };
            self.run(repo, &["update-index", "-z", "--index-info"], call, &[])
                .await?;
        }
        let call = Call {
            index: Some(index.clone()),
            ..Call::default()
        };
        let tree = self.run(repo, &["write-tree"], call, &[]).await?;
        let tree = String::from_utf8_lossy(&tree.stdout).trim().to_owned();
        if !is_oid(&tree) {
            return Err(GitFail::Failed("write-tree: неожиданный вывод".into()).into());
        }

        // 4. Коммит и ссылка (только создание).
        let parent = self.last_of_run(repo, req.run).await?;
        let mut msg = format!(
            "zero-agent checkpoint {id}\n\n{PROJECT_TRAILER}{}\n",
            self.home.project_id
        );
        for s in &plan.skipped {
            msg.push_str(SKIPPED_TRAILER);
            msg.push_str(s);
            msg.push('\n');
        }
        let mut args = vec!["commit-tree", tree.as_str()];
        if let Some(p) = &parent {
            args.extend(["-p", p.as_str()]);
        }
        args.extend(["-m", msg.as_str()]);
        let commit = self.simple(repo, &args).await?;
        let commit = String::from_utf8_lossy(&commit.stdout).trim().to_owned();
        if !is_oid(&commit) {
            return Err(GitFail::Failed("commit-tree: неожиданный вывод".into()).into());
        }
        if let Err(e) = self
            .simple(repo, &["update-ref", &git_ref, &commit, ""])
            .await
        {
            if let Some(found) = self.existing(repo, id, &git_ref).await? {
                return Ok(found);
            }
            return Err(e.into());
        }

        // Кэш stat и свой индекс.
        let cache = StatCache {
            v: STAT_V,
            at_ns: now_ns(),
            entries: oids
                .into_iter()
                .filter(|(_, (st, _))| st.cacheable)
                .map(|(rel, (st, oid))| (rel, StatEntry { st, oid }))
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
        Ok(Made {
            cp: git_cp(id, git_ref, commit),
            skipped: plan.skipped.into_iter().collect(),
        })
    }

    /// Существующий чекпоинт `id` этого проекта; ссылка с тем же именем от другого
    /// проекта — ошибка.
    async fn existing(
        &mut self,
        repo: &Repo,
        id: &str,
        git_ref: &str,
    ) -> Result<Option<Made>, Skip> {
        let out = self
            .simple(repo, &["for-each-ref", "--format=%(objectname)", git_ref])
            .await?;
        if !String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| is_oid(l.trim()))
        {
            return Ok(None);
        }
        match self.refs(repo, Some(id)).await {
            Ok(v) => match v.into_iter().next() {
                Some(r) => Ok(Some(Made {
                    cp: r.cp,
                    skipped: r.skipped,
                })),
                None => {
                    tracing::warn!(id, "ссылка чекпоинта занята другим проектом");
                    Err(Skip(checkpoint_reason::GIT_ERROR))
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "ссылка чекпоинта не прочитана");
                Err(Skip(checkpoint_reason::GIT_ERROR))
            }
        }
    }

    /// Последний (по шагу и номеру) чекпоинт run.
    async fn last_of_run(&mut self, repo: &Repo, run: RunId) -> Result<Option<String>, GitFail> {
        let prefix = format!("{CHECKPOINT_REF_PREFIX}/{run}/");
        let out = self
            .simple(
                repo,
                &["for-each-ref", "--format=%(refname) %(objectname)", &prefix],
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

    /// Чекпоинты git этого проекта (по строке проекта в сообщении); `only` — один id.
    /// Объекты читаются без догрузки (`GIT_NO_LAZY_FETCH`): отсутствующий — пропуск.
    pub(super) async fn refs(
        &mut self,
        repo: &Repo,
        only: Option<&str>,
    ) -> Result<Vec<RefCp>, RestoreError> {
        let pattern = match only {
            Some(id) => format!("{CHECKPOINT_REF_PREFIX}/{id}"),
            None => format!("{CHECKPOINT_REF_PREFIX}/"),
        };
        let out = self
            .listing(
                repo,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    &pattern,
                ],
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
        let mut stdin = Vec::new();
        for (_, _, oid) in &refs {
            stdin.extend_from_slice(oid.as_bytes());
            stdin.push(b'\n');
        }
        let call = Call {
            stdin: Some(stdin),
            capture: Some(LIST_CAPTURE),
            ..Call::default()
        };
        let out = self.run(repo, &["cat-file", "--batch"], call, &[]).await?;
        let objects = parse_batch(&out.stdout).map_err(RestoreError::Git)?;
        let mut res = Vec::new();
        for ((id, name, oid), body) in refs.into_iter().zip(objects) {
            let Some(body) = body else { continue };
            let Some(meta) = parse_message(&body) else {
                continue;
            };
            if meta.project.as_deref() == Some(self.home.project_id.as_str()) {
                res.push(RefCp {
                    cp: git_cp(&id, name, oid),
                    skipped: meta.skipped,
                });
            }
        }
        Ok(res)
    }

    /// Пропущенные пути чекпоинта `commit` (из его сообщения).
    async fn skipped_of(
        &mut self,
        repo: &Repo,
        commit: &str,
    ) -> Result<HashSet<String>, RestoreError> {
        let out = self.listing(repo, &["cat-file", "commit", commit]).await?;
        parse_message(&out.stdout)
            .map(|m| m.skipped)
            .ok_or_else(|| RestoreError::Git(format!("коммит {commit} не чекпоинт")))
    }

    /// Записи дерева и пути, которые тронет восстановление (запись и, с `delete_new`,
    /// удаление).
    async fn restore_set(
        &mut self,
        repo: &Repo,
        commit: &str,
        delete_new: bool,
    ) -> Result<(Vec<TreeEntry>, Vec<String>), RestoreError> {
        let tree = self
            .listing(repo, &["ls-tree", "-r", "-z", "-l", "--full-tree", commit])
            .await?;
        let entries: Vec<TreeEntry> = parse_tree(&tree.stdout)?
            .into_iter()
            .filter(|e| {
                let ok = e.mode == MODE_FILE || e.mode == MODE_EXEC;
                if !ok {
                    tracing::warn!(path = %e.rel, mode = %e.mode, "ссылка или подмодуль не восстанавливается");
                }
                ok
            })
            .collect();
        let mut extra = Vec::new();
        if delete_new {
            let skipped = self.skipped_of(repo, commit).await?;
            let all = self
                .listing(
                    repo,
                    &["ls-tree", "-r", "-z", "--name-only", "--full-tree", commit],
                )
                .await?;
            let in_cp: HashSet<String> = all
                .stdout
                .split(|b| *b == 0)
                .filter_map(|r| std::str::from_utf8(r).ok())
                .map(str::to_owned)
                .collect();
            let current = self
                .listing(repo, &["ls-files", "-z", "-c", "-o", "--exclude-standard"])
                .await?;
            let mut seen = HashSet::new();
            for rec in current.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
                // Удаляется только то, что чекпоинт разобрал бы тем же правилом (M5).
                let Ok(rel) = std::str::from_utf8(rec) else {
                    continue;
                };
                if rel.ends_with('/')
                    || rel.contains('\n')
                    || in_cp.contains(rel)
                    || skipped.contains(rel)
                    || !seen.insert(rel)
                {
                    continue;
                }
                extra.push(rel.to_owned());
            }
        }
        Ok((entries, extra))
    }

    /// Пути (от корня проекта), которые тронет восстановление `commit`.
    pub(super) async fn plan_paths(
        &mut self,
        repo: &Repo,
        commit: &str,
        delete_new: bool,
    ) -> Result<Vec<String>, RestoreError> {
        let (entries, extra) = self.restore_set(repo, commit, delete_new).await?;
        let mut v: Vec<String> = entries.into_iter().map(|e| e.rel).collect();
        v.extend(extra);
        Ok(v)
    }

    /// Восстановление рабочей копии из `commit`.
    pub(super) async fn restore(
        &mut self,
        repo: &Repo,
        roots: Roots,
        commit: &str,
        delete_new: bool,
    ) -> Result<Vec<PathBuf>, RestoreError> {
        let (entries, extra) = self.restore_set(repo, commit, delete_new).await?;
        // Проверка всех целей до записи.
        let roots = Arc::new(roots);
        let (r2, rels) = (
            roots.clone(),
            entries.iter().map(|e| e.rel.clone()).collect::<Vec<_>>(),
        );
        let (targets, deletes) = blocking(move || {
            let mut t = BTreeMap::new();
            for rel in rels {
                if let Some(x) = target(&r2, &rel)? {
                    t.insert(rel, x);
                }
            }
            let mut d = Vec::new();
            for rel in extra {
                if let Some(x) = target(&r2, &rel)?
                    && x.exists
                {
                    d.push(x);
                }
            }
            Ok((t, d))
        })
        .await?;
        let wanted: Vec<&TreeEntry> = entries
            .iter()
            .filter(|e| targets.contains_key(&e.rel))
            .collect();
        let mut changed = Vec::new();
        let mut n = 0usize;

        // Крупные — потоком в файл по одному.
        for e in wanted.iter().filter(|e| e.size > STREAM_FROM) {
            let Some(t) = targets.get(&e.rel).cloned() else {
                continue;
            };
            let (root, t2, i) = (roots.project.clone(), t.clone(), n);
            n += 1;
            let tmp = blocking(move || Tmp::new(&root, &t2, i)).await?;
            let handle = match tmp.handle() {
                Ok(h) => h,
                Err(err) => {
                    tmp.discard();
                    return Err(err.into());
                }
            };
            let call = Call {
                stdout_file: Some(Arc::new(handle)),
                capture: Some(usize::try_from(e.size).unwrap_or(usize::MAX)),
                ..Call::default()
            };
            let r = self
                .run(repo, &["cat-file", "blob", e.oid.as_str()], call, &[])
                .await;
            if let Err(err) = r {
                tmp.discard();
                return Err(err.into());
            }
            let (size, exec) = (e.size, e.mode == MODE_EXEC);
            let wrote = blocking(move || {
                let len = tmp.file.metadata()?.len();
                if len != size {
                    tmp.discard();
                    return Err(io::Error::other(format!(
                        "объект: {len} байт вместо {size}"
                    )));
                }
                finish(&t, tmp, Some(Mode::Git(exec)), i)
            })
            .await?;
            if wrote {
                changed.push(targets[&e.rel].r.path.clone());
            }
        }

        // Мелкие — порциями `cat-file --batch`.
        let mut chunks: Vec<Vec<&TreeEntry>> = Vec::new();
        let mut batch: Vec<&TreeEntry> = Vec::new();
        let mut bytes = 0u64;
        for e in wanted.iter().copied().filter(|e| e.size <= STREAM_FROM) {
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
            let call = Call {
                stdin: Some(stdin),
                capture: Some(cap),
                ..Call::default()
            };
            let out = self.run(repo, &["cat-file", "--batch"], call, &[]).await?;
            let objects = parse_batch(&out.stdout).map_err(RestoreError::Git)?;
            if objects.len() != chunk.len() {
                return Err(RestoreError::Git("cat-file: не все объекты".into()));
            }
            let mut jobs: Vec<(Target, Vec<u8>, bool, usize)> = Vec::new();
            for (e, body) in chunk.iter().zip(objects) {
                let body =
                    body.ok_or_else(|| RestoreError::Git(format!("объекта {} нет", e.oid)))?;
                if let Some(t) = targets.get(&e.rel) {
                    jobs.push((t.clone(), body, e.mode == MODE_EXEC, n));
                    n += 1;
                }
            }
            let root = roots.project.clone();
            let written = blocking(move || {
                let mut w = Vec::new();
                for (t, body, exec, i) in jobs {
                    let mut tmp = Tmp::new(&root, &t, i)?;
                    if let Err(e) = fill(&mut tmp, &mut body.as_slice()) {
                        tmp.discard();
                        return Err(e);
                    }
                    if finish(&t, tmp, Some(Mode::Git(exec)), i)? {
                        w.push(t.r.path.clone());
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
}

fn git_cp(id: &str, git_ref: String, commit: String) -> Checkpoint {
    Checkpoint {
        id: id.to_owned(),
        kind: checkpoint_kind::GIT.into(),
        git_ref: Some(git_ref),
        commit: Some(commit),
        reason: None,
    }
}

/// Сведения из сообщения коммита чекпоинта.
struct Message {
    project: Option<String>,
    skipped: HashSet<String>,
}

/// Разбор объекта коммита: заголовки до пустой строки, затем сообщение.
fn parse_message(body: &[u8]) -> Option<Message> {
    let text = std::str::from_utf8(body).ok()?;
    let (_, msg) = text.split_once("\n\n")?;
    let mut m = Message {
        project: None,
        skipped: HashSet::new(),
    };
    for line in msg.lines() {
        if let Some(p) = line.strip_prefix(PROJECT_TRAILER) {
            m.project = Some(p.to_owned());
        } else if let Some(s) = line.strip_prefix(SKIPPED_TRAILER) {
            m.skipped.insert(s.to_owned());
        }
    }
    Some(m)
}

// ---- список файлов и stat ----

/// Запись списка: режим и oid из индекса (у отслеживаемых).
struct Listed {
    rel: String,
    index_mode: Option<String>,
    index_oid: Option<String>,
}

/// Разбор `ls-files -z -s` и `-z -o`: по пути одна запись (у конфликта — режим стадии
/// 2 или любой); каталоги вложенных репозиториев (`x/`) пропускаются.
fn parse_listing(tracked: &[u8], untracked: &[u8]) -> Vec<Listed> {
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
        let e = by_path.entry(path.to_owned()).or_insert_with(|| Listed {
            rel: path.to_owned(),
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
        by_path.entry(path.to_owned()).or_insert_with(|| Listed {
            rel: path.to_owned(),
            index_mode: None,
            index_oid: None,
        });
    }
    by_path
        .into_values()
        .filter(|l| !l.rel.is_empty())
        .collect()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StatCache {
    v: u32,
    /// Когда кэш записан (нс от эпохи).
    at_ns: u64,
    entries: BTreeMap<String, StatEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StatEntry {
    #[serde(flatten)]
    st: Stat,
    oid: String,
}

/// stat записи дерева.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stat {
    mode: String,
    size: u64,
    mtime_ns: u64,
    /// Unix: `ctime` и `ino` (M4); прочие ОС — 0.
    ctime_ns: u64,
    ino: u64,
    /// Входит ли в кэш (обычные файлы).
    #[serde(skip)]
    cacheable: bool,
}

/// План: известные oid (кэш, gitlink), файлы на хэширование `(rel, абсолютный путь,
/// stat)` и пропущенные сверх предела.
struct Plan {
    known: BTreeMap<String, (Stat, String)>,
    hash: Vec<(String, String, Stat)>,
    skipped: Vec<String>,
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

#[cfg(unix)]
fn ctime_ino(m: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let ns = u64::try_from(m.ctime())
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(m.ctime_nsec()).unwrap_or(0));
    (ns, m.ino())
}

#[cfg(not(unix))]
fn ctime_ino(_m: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

fn plan_entries(top: &Path, run_dir: &Path, listed: Vec<Listed>) -> io::Result<Plan> {
    let cache: StatCache = read_private(&run_dir.join(STAT_FILE), 256 << 20)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|c: &StatCache| c.v == STAT_V)
        .unwrap_or_default();
    let trusted_before = cache
        .at_ns
        .saturating_sub(u64::try_from(RACY.as_nanos()).unwrap_or(0));
    let mut plan = Plan {
        known: BTreeMap::new(),
        hash: Vec::new(),
        skipped: Vec::new(),
    };
    let mut links = 0usize;
    for l in listed {
        if l.rel.contains('\n') {
            tracing::warn!(path = %l.rel, "имя с переводом строки: в чекпоинт не входит");
            continue;
        }
        let abs = join_rel(top, &l.rel);
        let Ok(meta) = std::fs::symlink_metadata(&abs) else {
            // Удалён в рабочем дереве: в чекпоинте его нет.
            continue;
        };
        let ft = meta.file_type();
        if ft.is_dir() {
            // Подмодуль: gitlink из индекса.
            if let (Some(m), Some(oid)) = (&l.index_mode, &l.index_oid)
                && m == MODE_GITLINK
            {
                plan.known.insert(l.rel, (gitlink_stat(), oid.clone()));
            }
            continue;
        }
        if ft.is_symlink() {
            // Содержимое blob ссылки — её цель.
            let target = std::fs::read_link(&abs)?;
            let dir = run_dir.join(LINKS_DIR);
            make_private_dir(&dir)?;
            let tmp = dir.join(links.to_string());
            links += 1;
            write_private(&tmp, &link_bytes(&target))?;
            let st = Stat {
                mode: MODE_LINK.into(),
                cacheable: false,
                ..gitlink_stat()
            };
            plan.hash
                .push((l.rel, tmp.to_string_lossy().into_owned(), st));
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        if meta.len() > CHECKPOINT_MAX_FILE {
            tracing::warn!(path = %l.rel, size = meta.len(), "файл больше предела: в чекпоинт не входит");
            plan.skipped.push(l.rel);
            continue;
        }
        let (ctime_ns, ino) = ctime_ino(&meta);
        let st = Stat {
            mode: file_mode(&meta, l.index_mode.as_deref()),
            size: meta.len(),
            mtime_ns: mtime_ns(&meta),
            ctime_ns,
            ino,
            cacheable: true,
        };
        if let Some(c) = cache.entries.get(&l.rel)
            && (Stat {
                cacheable: true,
                ..c.st.clone()
            }) == st
            && st.mtime_ns < trusted_before
            && (st.ctime_ns == 0 || st.ctime_ns < trusted_before)
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

fn gitlink_stat() -> Stat {
    Stat {
        mode: MODE_GITLINK.into(),
        size: 0,
        mtime_ns: 0,
        ctime_ns: 0,
        ino: 0,
        cacheable: false,
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
/// ссылкой; Unix — бит исполнения владельца (`S_IXUSR`, как у git, M6); Windows — режим
/// индекса.
fn file_mode(meta: &std::fs::Metadata, index: Option<&str>) -> String {
    if index == Some(MODE_LINK) {
        return MODE_LINK.into();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o100 != 0 {
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

// ---- разбор вывода ----

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
            .filter(|e| out.get(*e) == Some(&b'\n'))
            .ok_or("cat-file: обрыв содержимого")?;
        res.push(Some(out[pos..end].to_vec()));
        pos = end + 1;
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_parsing() {
        let out = b"aa blob 3\nx\ny\nbb missing\ncc blob 0\n\n";
        let v = parse_batch(out).unwrap();
        assert_eq!(v, vec![Some(b"x\ny".to_vec()), None, Some(Vec::new())]);
        assert!(parse_batch(b"aa blob 5\nxy\n").is_err());
    }

    #[test]
    fn listing_dedups_conflicts() {
        let o1 = "a".repeat(40);
        let o2 = "b".repeat(40);
        let tracked =
            format!("100644 {o1} 1\tf\0100755 {o2} 2\tf\0100644 {o1} 3\tf\0160000 {o2} 0\tm\0");
        let l = parse_listing(tracked.as_bytes(), b"n\0nested/\0");
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

    #[test]
    fn message_trailers() {
        let body = b"tree x\nauthor a\n\nzero-agent checkpoint r/1-1\n\nzero-agent-project: abc\nzero-agent-skipped: big.bin\n";
        let m = parse_message(body).unwrap();
        assert_eq!(m.project.as_deref(), Some("abc"));
        assert!(m.skipped.contains("big.bin"));
    }
}
