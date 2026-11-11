//! Чекпоинты рабочей копии (R1-C1 D89, TOOL-009; Ruling 7.15 «безопасный запуск git»):
//! plumbing без хуков, fsmonitor и фильтров, состояние репозитория пользователя не
//! меняется, восстановление байт в байт, повтор по ключу эффекта, слияние и подмодуль,
//! копия файлов без git и в недоверенном проекте, пропуски `git_not_found` и `slow`,
//! восстановление не идёт по ссылке, подменившей файл или каталог.
//!
//! Репозитории тестов — `<ZeroAgent>/.tmp/r1c1-git/checkpoint/<name>-<pid>-<n>/p` со своим
//! `git init` и `GIT_CEILING_DIRECTORIES=<ZeroAgent>/.tmp` (Global Constraints): git теста
//! не видит репозиторий ZeroAgent. `ZA_HOME` теста — соседний каталог `h`. Окружение
//! передаётся инъекцией (`with_base_env`, `Command::env`), не через `set_var`.

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use za_core::paths::Roots;
use za_core::{BoxFuture, Checkpoint, CheckpointRequest, Checkpointer, ToolKind};
use za_jail::{CommandSpec, DirectRunner, Output, Runner, RunnerError};
use za_protocol::{EffectKey, RunId, checkpoint_kind, checkpoint_reason};
use za_tools::{
    AgentHome, CHECKPOINT_REF_PREFIX, CHECKPOINT_TIMEOUT, GitCheckpointer, RestoreError,
    backup_before_restore, restore,
};

#[path = "../../../test-support/unique_dir.rs"]
mod shared_dir;

// ---- помощники ----

/// Множитель тайм-аутов `ZA_TEST_TIME_SCALE` (целое 1–6).
fn scale() -> u32 {
    std::env::var("ZA_TEST_TIME_SCALE")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map_or(1, |n| n.clamp(1, 6))
}

/// Ждёт `f` не дольше `base` × [`scale`]; иначе — паника.
async fn within<T>(base: Duration, what: &str, f: impl Future<Output = T>) -> T {
    let limit = base * scale();
    tokio::time::timeout(limit, f)
        .await
        .unwrap_or_else(|_| panic!("не дождались за {limit:?}: {what}"))
}

/// Корень репозитория ZeroAgent.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Потолок поиска репозитория для git тестов и чекпоинтера.
fn ceiling() -> OsString {
    repo_root().join(".tmp").into_os_string()
}

/// Каталог случая: `p` — проект, `h` — `ZA_HOME`; оба созданы.
struct Case {
    base: PathBuf,
    project: PathBuf,
    home: AgentHome,
}

fn case(name: &str) -> Case {
    let base = shared_dir::unique_dir_in(&repo_root().join(".tmp/r1c1-git/checkpoint"), name);
    let project = base.join("p");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(base.join("h")).unwrap();
    Case {
        home: AgentHome {
            za_home: base.join("h"),
            project_id: "0123456789abcdef".into(),
        },
        project,
        base,
    }
}

impl Case {
    /// Канонический корень проекта (как его подаёт пакет вызовов).
    fn root(&self) -> PathBuf {
        Roots::canonical(&self.project, &self.home.za_home)
            .unwrap()
            .project
    }

    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) {
        let p = self.project.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.project.join(rel)).unwrap()
    }

    /// Чекпоинтер на настоящем git с потолком поиска репозитория.
    fn ck(&self) -> GitCheckpointer {
        GitCheckpointer::new(Arc::new(DirectRunner), self.home.clone())
            .with_base_env(vec![("GIT_CEILING_DIRECTORIES".into(), ceiling())])
    }

    fn req(&self, kind: ToolKind, trusted: bool, run: RunId, step: u64) -> CheckpointRequest {
        CheckpointRequest {
            project_dir: self.root(),
            run,
            effect_key: EffectKey::derive(run, step, 1),
            kind,
            paths: Vec::new(),
            trusted,
        }
    }

    /// Запрос `Edit` с затронутыми файлами `rels` (канонические пути, как у пакета).
    fn req_edit(&self, rels: &[&str], trusted: bool, run: RunId) -> CheckpointRequest {
        let mut r = self.req(ToolKind::Edit, trusted, run, 1);
        r.paths = rels.iter().map(|rel| self.root().join(rel)).collect();
        r
    }

    /// git теста: явная идентичность, без autocrlf и подписи, без необязательных
    /// блокировок (`status` не переписывает индекс), с потолком поиска.
    fn git_out(&self, args: &[&str]) -> std::process::Output {
        let mut full = vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.autocrlf=false",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "protocol.file.allow=always",
        ];
        full.extend_from_slice(args);
        std::process::Command::new("git")
            .args(&full)
            .current_dir(&self.project)
            .env("GIT_CEILING_DIRECTORIES", ceiling())
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .expect("git не запустился: нужен git в PATH")
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self.git_out(args);
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn init(&self) {
        self.git(&["-c", "init.defaultBranch=main", "init", "-q"]);
        assert!(self.project.join(".git").is_dir());
    }

    fn index_bytes(&self) -> Vec<u8> {
        std::fs::read(self.project.join(".git/index")).unwrap_or_default()
    }

    /// Состояние пользователя: HEAD, индекс, stash, рабочее дерево, байты индекса.
    fn user_state(&self) -> (String, String, String, String, Vec<u8>) {
        (
            self.git(&["rev-parse", "HEAD"]),
            self.git(&["diff", "--cached"]),
            self.git(&["stash", "list"]),
            self.git(&["status", "--porcelain"]),
            self.index_bytes(),
        )
    }
}

async fn create(ck: &GitCheckpointer, req: CheckpointRequest) -> Checkpoint {
    within(Duration::from_secs(60), "чекпоинт", ck.create(req)).await
}

/// Заглушка `Runner`: считает вызовы; `hang` — не отвечает никогда.
struct StubRunner {
    calls: AtomicUsize,
    hang: bool,
}

impl StubRunner {
    fn new(hang: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            hang,
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Runner for StubRunner {
    fn run(&self, spec: CommandSpec) -> BoxFuture<'static, Result<Output, RunnerError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let hang = self.hang;
        Box::pin(async move {
            if hang {
                std::future::pending::<()>().await;
            }
            Err(RunnerError::Spawn {
                program: spec.program.display().to_string(),
                source: std::io::Error::other("stub"),
            })
        })
    }
}

/// Ссылка на каталог: junction на Windows (`mklink /J` прав не требует), символическая
/// ссылка на Unix. Не создалась — `None` и пропуск с сообщением.
fn dir_link(link: &Path, target: &Path) -> Option<()> {
    #[cfg(windows)]
    {
        let native = |p: &Path| p.components().collect::<PathBuf>();
        let out = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(native(link))
            .arg(native(target))
            .output()
            .expect("cmd не запустился");
        if !out.status.success() {
            if std::env::var("ZA_REQUIRE_LINK_TESTS").as_deref() == Ok("1") {
                panic!(
                    "ZA_REQUIRE_LINK_TESTS=1, а junction не создан: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            eprintln!(
                "ПРОПУСК: junction не создан: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        Some(())
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).unwrap();
        Some(())
    }
}

// ---- git: состояние пользователя ----

#[tokio::test]
async fn git_checkpoint_leaves_user_state_untouched() {
    let c = case("untouched");
    c.init();
    c.write(".gitignore", "target/\n");
    c.write("a.txt", "one\n");
    c.write("b.txt", "bee\n");
    c.git(&["add", "-A"]);
    c.git(&["commit", "-q", "-m", "init"]);
    // stash, затем staged, unstaged, неотслеживаемый и игнорируемый файлы.
    c.write("a.txt", "stashed\n");
    c.git(&["stash", "-q"]);
    c.write("a.txt", "staged\n");
    c.git(&["add", "a.txt"]);
    c.write("b.txt", "unstaged\n");
    c.write("new.txt", "untracked\n");
    c.write("target/big.bin", "ignored\n");
    let before = c.user_state();

    let ck = c.ck();
    let run = RunId::new();
    let cp = create(&ck, c.req(ToolKind::Edit, true, run, 3)).await;
    assert_eq!(cp.kind, checkpoint_kind::GIT, "{cp:?}");
    let git_ref = cp.git_ref.clone().unwrap();
    assert!(
        git_ref.starts_with("refs/zero-agent/checkpoints/"),
        "{git_ref}"
    );
    assert!(git_ref.starts_with(CHECKPOINT_REF_PREFIX));
    assert!(!cp.id.contains(':'), "{}", cp.id);
    assert_eq!(cp.id, format!("{run}/3-1"));
    assert_eq!(git_ref, format!("{CHECKPOINT_REF_PREFIX}/{run}/3-1"));
    assert_eq!(before, c.user_state(), "состояние пользователя изменилось");

    let commit = cp.commit.clone().unwrap();
    let files = c.git(&["ls-tree", "-r", "--name-only", &commit]);
    assert!(!files.contains("target/"), "{files}");
    for f in [".gitignore", "a.txt", "b.txt", "new.txt"] {
        assert!(
            files.lines().any(|l| l == f),
            "{f} нет в чекпоинте: {files}"
        );
    }
    // Содержимое — рабочее дерево, а не индекс и не HEAD.
    assert_eq!(c.git(&["show", &format!("{commit}:a.txt")]), "staged\n");
    assert_eq!(c.git(&["show", &format!("{commit}:b.txt")]), "unstaged\n");
    assert_eq!(
        c.git(&["for-each-ref", "--format=%(objectname)", &git_ref])
            .trim(),
        commit
    );
    // HEAD пользователя не стал родителем: чекпоинт не связан с веткой.
    assert!(!c.git(&["branch", "--contains", &commit]).contains("main"));

    // Второй чекпоинт run: родитель — первый, состояние пользователя то же.
    c.write("b.txt", "second\n");
    let before2 = c.user_state();
    let cp2 = create(&ck, c.req(ToolKind::Exec, true, run, 4)).await;
    assert_eq!(cp2.kind, checkpoint_kind::GIT, "{cp2:?}");
    let commit2 = cp2.commit.clone().unwrap();
    assert_eq!(c.git(&["rev-parse", &format!("{commit2}^")]).trim(), commit);
    assert_eq!(before2, c.user_state());
}

// ---- git: конфиг недоверенного содержимого не исполняет код ----

#[tokio::test]
async fn repo_hooks_fsmonitor_filters_not_executed() {
    let c = case("hooks");
    c.init();
    let markers = c.base.join("markers");
    std::fs::create_dir_all(&markers).unwrap();
    let scripts = c.base.join("scripts");
    std::fs::create_dir_all(scripts.join("hooks")).unwrap();
    let fwd = |p: &Path| p.display().to_string().replace('\\', "/");
    let script = |path: &Path, marker: &str| {
        std::fs::write(
            path,
            format!("#!/bin/sh\necho x > '{}'\n", fwd(&markers.join(marker))),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    };
    script(&scripts.join("fsmonitor.sh"), "fsmonitor");
    for hook in ["post-index-change", "reference-transaction", "pre-auto-gc"] {
        script(&scripts.join("hooks").join(hook), hook);
    }
    c.git(&[
        "config",
        "core.fsmonitor",
        &fwd(&scripts.join("fsmonitor.sh")),
    ]);
    c.git(&["config", "core.hooksPath", &fwd(&scripts.join("hooks"))]);
    c.git(&[
        "config",
        "filter.x.clean",
        &format!("sh -c 'echo x > \"{}\"; cat'", fwd(&markers.join("clean"))),
    ]);
    c.git(&[
        "config",
        "filter.x.smudge",
        &format!("sh -c 'echo x > \"{}\"; cat'", fwd(&markers.join("smudge"))),
    ]);
    c.write(".gitattributes", "* filter=x\n");
    c.write("a.txt", "alpha\n");

    // Контроль: обычный git исполняет все три.
    c.git(&["add", "-A"]);
    let fired: Vec<String> = std::fs::read_dir(&markers)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    for m in ["fsmonitor", "post-index-change", "clean"] {
        assert!(
            fired.iter().any(|f| f == m),
            "контроль: {m} не сработал: {fired:?}"
        );
    }
    c.git(&["commit", "-q", "--no-verify", "-m", "init"]);
    for e in std::fs::read_dir(&markers).unwrap() {
        std::fs::remove_file(e.unwrap().path()).unwrap();
    }

    let ck = c.ck();
    c.write("a.txt", "beta\n");
    let cp = create(&ck, c.req(ToolKind::Exec, true, RunId::new(), 1)).await;
    assert_eq!(cp.kind, checkpoint_kind::GIT, "{cp:?}");
    c.write("a.txt", "gamma\n");
    c.write("new.txt", "new\n");
    let changed = within(
        Duration::from_secs(60),
        "восстановление",
        restore(&ck, &c.root(), &cp, true),
    )
    .await
    .expect("восстановление");
    assert!(!changed.is_empty());
    assert_eq!(c.read("a.txt"), b"beta\n");
    let left: Vec<_> = std::fs::read_dir(&markers).unwrap().collect();
    assert!(left.is_empty(), "сработал код репозитория: {left:?}");
}

// ---- git: восстановление байт в байт ----

#[tokio::test]
async fn restore_round_trip_exact_bytes() {
    let c = case("roundtrip");
    c.init();
    let crlf: &[u8] = b"one\r\ntwo\r\n";
    let lf: &[u8] = b"one\ntwo\n";
    let bin: &[u8] = b"\x00\xff\r\n\x01";
    c.write("crlf.txt", crlf);
    c.write("lf.txt", lf);
    c.write("d/bin.dat", bin);
    c.git(&["add", "-A"]);
    c.git(&["commit", "-q", "-m", "init"]);
    // Фильтры перевода строк репозитория не должны ни на что влиять.
    c.git(&["config", "core.autocrlf", "true"]);
    c.write("untracked.txt", "u\r\n");

    let ck = c.ck();
    let cp = create(&ck, c.req(ToolKind::Edit, true, RunId::new(), 1)).await;
    assert_eq!(cp.kind, checkpoint_kind::GIT, "{cp:?}");

    c.write("crlf.txt", "changed\n");
    c.write("lf.txt", "changed\r\n");
    std::fs::remove_file(c.project.join("d/bin.dat")).unwrap();
    c.write("untracked.txt", "x");
    c.write("brand-new.txt", "new");
    c.write("d/also-new.txt", "new");

    // Без --delete-new новые файлы остаются.
    let changed = within(
        Duration::from_secs(60),
        "восстановление",
        restore(&ck, &c.root(), &cp, false),
    )
    .await
    .expect("восстановление");
    assert_eq!(c.read("crlf.txt"), crlf);
    assert_eq!(c.read("lf.txt"), lf);
    assert_eq!(c.read("d/bin.dat"), bin);
    assert_eq!(c.read("untracked.txt"), b"u\r\n");
    assert!(c.project.join("brand-new.txt").exists());
    assert_eq!(changed.len(), 4, "{changed:?}");

    // С --delete-new — удаляются; неизменившиеся файлы не переписываются.
    let changed = within(
        Duration::from_secs(60),
        "восстановление",
        restore(&ck, &c.root(), &cp, true),
    )
    .await
    .expect("восстановление");
    assert!(!c.project.join("brand-new.txt").exists());
    assert!(!c.project.join("d/also-new.txt").exists());
    assert_eq!(changed.len(), 2, "{changed:?}");
    assert_eq!(c.read("crlf.txt"), crlf);
}

// ---- повтор по ключу эффекта ----

#[tokio::test]
async fn same_effect_key_reuses_checkpoint() {
    let c = case("reuse");
    c.init();
    c.write("a.txt", "one\n");
    let ck = c.ck();
    let run = RunId::new();
    let first = create(&ck, c.req(ToolKind::Exec, true, run, 2)).await;
    assert_eq!(first.kind, checkpoint_kind::GIT, "{first:?}");
    // Повтор после сбоя: файлы уже другие, но чекпоинт тот же, ссылка не переписана.
    c.write("a.txt", "two\n");
    let again = create(
        &GitCheckpointer::new(Arc::new(DirectRunner), c.home.clone())
            .with_base_env(vec![("GIT_CEILING_DIRECTORIES".into(), ceiling())]),
        c.req(ToolKind::Exec, true, run, 2),
    )
    .await;
    assert_eq!(again, first);
    let git_ref = first.git_ref.unwrap();
    assert_eq!(
        c.git(&["for-each-ref", "--format=%(objectname)", &git_ref])
            .trim(),
        first.commit.unwrap()
    );
    // То же для копии файлов: каталог чекпоинта уже есть.
    let files_run = RunId::new();
    let f1 = create(&ck, c.req_edit(&["a.txt"], false, files_run)).await;
    c.write("a.txt", "three\n");
    let f2 = create(&ck, c.req_edit(&["a.txt"], false, files_run)).await;
    assert_eq!(f1.kind, checkpoint_kind::FILES);
    assert_eq!(f1, f2);
    c.write("a.txt", "four\n");
    within(
        Duration::from_secs(60),
        "восстановление",
        restore(&ck, &c.root(), &f2, false),
    )
    .await
    .unwrap();
    assert_eq!(c.read("a.txt"), b"two\n", "копия — первая, не перезаписана");
}

// ---- слияние и подмодуль ----

#[tokio::test]
async fn mid_merge_and_submodule() {
    let c = case("merge");
    // Репозиторий подмодуля — рядом с проектом.
    let sub = Case {
        base: c.base.clone(),
        project: c.base.join("sub"),
        home: c.home.clone(),
    };
    std::fs::create_dir_all(&sub.project).unwrap();
    sub.init();
    sub.write("s.txt", "sub\n");
    sub.git(&["add", "-A"]);
    sub.git(&["commit", "-q", "-m", "sub"]);

    c.init();
    c.write("f.txt", "base\n");
    c.git(&["add", "-A"]);
    c.git(&["commit", "-q", "-m", "base"]);
    c.git(&["submodule", "add", "-q", "../sub", "sub"]);
    c.git(&["commit", "-q", "-m", "submodule"]);
    c.git(&["checkout", "-q", "-b", "other"]);
    c.write("f.txt", "other\n");
    c.git(&["commit", "-q", "-am", "other"]);
    c.git(&["checkout", "-q", "main"]);
    c.write("f.txt", "main\n");
    c.git(&["commit", "-q", "-am", "main"]);
    let merge = c.git_out(&["merge", "other"]);
    assert!(!merge.status.success(), "ожидался конфликт слияния");
    let merge_head = std::fs::read(c.project.join(".git/MERGE_HEAD")).unwrap();
    let before = (c.git(&["status", "--porcelain"]), c.index_bytes());

    let ck = c.ck();
    let cp = create(&ck, c.req(ToolKind::Exec, true, RunId::new(), 1)).await;
    match cp.kind.as_str() {
        checkpoint_kind::GIT => {
            let commit = cp.commit.clone().unwrap();
            let tree = c.git(&["ls-tree", &commit]);
            assert!(
                tree.contains("160000 commit"),
                "подмодуль — gitlink: {tree}"
            );
            let f = c.git(&["show", &format!("{commit}:f.txt")]);
            assert!(
                f.contains("<<<<<<<"),
                "файл с конфликтом — как в рабочем дереве: {f}"
            );
        }
        checkpoint_kind::SKIPPED => assert!(cp.reason.is_some(), "{cp:?}"),
        other => panic!("вид {other}: {cp:?}"),
    }
    assert_eq!(
        std::fs::read(c.project.join(".git/MERGE_HEAD")).unwrap(),
        merge_head
    );
    assert_eq!(before, (c.git(&["status", "--porcelain"]), c.index_bytes()));
}

// ---- копия файлов: недоверенный проект и не git ----

#[tokio::test]
async fn untrusted_or_non_git_uses_files() {
    // Недоверенный репозиторий: git не вызывается совсем.
    let c = case("untrusted");
    c.init();
    c.write("a.txt", "before\r\n");
    let stub = StubRunner::new(false);
    let ck = GitCheckpointer::with_git(stub.clone(), c.home.clone(), Some("git".into()));
    let cp = create(&ck, c.req_edit(&["a.txt", "new.txt"], false, RunId::new())).await;
    assert_eq!(cp.kind, checkpoint_kind::FILES, "{cp:?}");
    assert!(cp.git_ref.is_none() && cp.commit.is_none());
    assert!(!cp.id.contains(':'));
    assert_eq!(stub.calls(), 0, "git в недоверенном проекте не вызывается");
    // Копия лежит в ZA_HOME, а не в проекте.
    assert!(!c.project.join(".zero-agent/work/checkpoints").exists());
    let manifest = c
        .home
        .za_home
        .join("projects/0123456789abcdef/checkpoints")
        .join(&cp.id)
        .join("manifest.json");
    assert!(manifest.is_file(), "{}", manifest.display());

    // Восстановление по манифесту: прежние байты, созданный файл удаляется с delete_new.
    c.write("a.txt", "after\n");
    c.write("new.txt", "created\n");
    let backup = within(
        Duration::from_secs(60),
        "чекпоинт перед восстановлением",
        backup_before_restore(&ck, &c.root(), &cp),
    )
    .await;
    assert_eq!(backup.kind, checkpoint_kind::FILES, "{backup:?}");
    let changed = within(
        Duration::from_secs(60),
        "восстановление",
        restore(&ck, &c.root(), &cp, true),
    )
    .await
    .expect("восстановление");
    assert_eq!(c.read("a.txt"), b"before\r\n");
    assert!(!c.project.join("new.txt").exists());
    assert_eq!(changed.len(), 2, "{changed:?}");
    assert_eq!(stub.calls(), 0);
    // Чекпоинт перед восстановлением держит состояние «после».
    within(
        Duration::from_secs(60),
        "восстановление",
        restore(&ck, &c.root(), &backup, true),
    )
    .await
    .unwrap();
    assert_eq!(c.read("a.txt"), b"after\n");
    assert_eq!(c.read("new.txt"), b"created\n");

    // Не git: `Exec` — пропуск `no_git`, `Edit` — копия файлов.
    let n = case("non-git");
    n.write("a.txt", "x");
    let ck = n.ck();
    let s = create(&ck, n.req(ToolKind::Exec, true, RunId::new(), 1)).await;
    assert_eq!(
        (s.kind.as_str(), s.reason.as_deref()),
        (checkpoint_kind::SKIPPED, Some(checkpoint_reason::NO_GIT))
    );
    assert_eq!(s.reason.as_deref(), Some("no_git"));
    let f = create(&ck, n.req_edit(&["a.txt"], true, RunId::new())).await;
    assert_eq!(f.kind, checkpoint_kind::FILES, "{f:?}");
    // Недоверенный `Exec` — тоже `no_git`, без git.
    let stub = StubRunner::new(false);
    let ck = GitCheckpointer::with_git(stub.clone(), n.home.clone(), Some("git".into()));
    let s = create(&ck, n.req(ToolKind::Exec, false, RunId::new(), 1)).await;
    assert_eq!(s.reason.as_deref(), Some(checkpoint_reason::NO_GIT));
    assert_eq!(stub.calls(), 0);
}

#[tokio::test]
async fn no_git_binary_skips() {
    let c = case("no-git-binary");
    c.init();
    c.write("a.txt", "x");
    let stub = StubRunner::new(false);
    let ck = GitCheckpointer::with_git(stub.clone(), c.home.clone(), None);
    let s = create(&ck, c.req(ToolKind::Exec, true, RunId::new(), 1)).await;
    assert_eq!(
        (s.kind.as_str(), s.reason.as_deref()),
        ("skipped", Some("git_not_found"))
    );
    // `Edit` без git — копия файлов (D89: «не git»).
    let f = create(&ck, c.req_edit(&["a.txt"], true, RunId::new())).await;
    assert_eq!(f.kind, checkpoint_kind::FILES, "{f:?}");
    assert_eq!(stub.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn slow_git_skips_then_stays_skipped() {
    let c = case("slow");
    c.init();
    let stub = StubRunner::new(true);
    let ck = GitCheckpointer::with_git(stub.clone(), c.home.clone(), Some("git".into()));
    let run = RunId::new();
    let started = tokio::time::Instant::now();
    let first = ck.create(c.req(ToolKind::Exec, true, run, 1)).await;
    assert_eq!(
        (first.kind.as_str(), first.reason.as_deref()),
        ("skipped", Some("slow"))
    );
    assert!(
        started.elapsed() >= CHECKPOINT_TIMEOUT,
        "{:?}",
        started.elapsed()
    );
    let calls = stub.calls();
    assert!(calls >= 1);
    let at = tokio::time::Instant::now();
    let second = ck.create(c.req(ToolKind::Exec, true, run, 2)).await;
    assert_eq!(second.reason.as_deref(), Some(checkpoint_reason::SLOW));
    assert_eq!(stub.calls(), calls, "второй чекпоинт run — без git");
    assert!(at.elapsed() < CHECKPOINT_TIMEOUT, "без ожидания");
    // Другой run — снова пробует git.
    let other = ck
        .create(c.req(ToolKind::Exec, true, RunId::new(), 1))
        .await;
    assert_eq!(other.reason.as_deref(), Some(checkpoint_reason::SLOW));
    assert!(stub.calls() > calls);
}

// ---- восстановление не идёт по ссылкам ----

#[tokio::test]
async fn restore_refuses_link_swapped_after_checkpoint() {
    for git in [true, false] {
        let c = case(if git { "swap-git" } else { "swap-files" });
        if git {
            c.init();
        }
        c.write("d/a.txt", "inside\n");
        let outside = c.base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("a.txt"), "outside\n").unwrap();

        let ck = c.ck();
        let cp = create(&ck, c.req_edit(&["d/a.txt"], git, RunId::new())).await;
        let want = if git {
            checkpoint_kind::GIT
        } else {
            checkpoint_kind::FILES
        };
        assert_eq!(cp.kind, want, "{cp:?}");
        // Каталог подменён ссылкой наружу с тем же именем файла.
        std::fs::remove_dir_all(c.project.join("d")).unwrap();
        if dir_link(&c.project.join("d"), &outside).is_none() {
            return;
        }
        let r = within(
            Duration::from_secs(60),
            "восстановление",
            restore(&ck, &c.root(), &cp, true),
        )
        .await;
        assert!(matches!(r, Err(RestoreError::Io(_))), "{r:?}");
        assert_eq!(
            std::fs::read(outside.join("a.txt")).unwrap(),
            b"outside\n",
            "запись прошла по ссылке наружу"
        );
    }
}

#[tokio::test]
async fn find_and_list_by_id() {
    let c = case("list");
    c.init();
    c.write("a.txt", "x");
    let ck = c.ck();
    let run = RunId::new();
    let g = create(&ck, c.req(ToolKind::Exec, true, run, 1)).await;
    let f = create(&ck, c.req_edit(&["a.txt"], false, run)).await;
    assert_eq!(g.kind, checkpoint_kind::GIT);
    assert_eq!(f.kind, checkpoint_kind::FILES);
    let root = c.root();
    let all = within(Duration::from_secs(60), "список", ck.list(&root))
        .await
        .unwrap();
    assert!(all.contains(&g) && all.contains(&f), "{all:?}");
    let found = within(Duration::from_secs(60), "поиск", ck.find(&root, &g.id))
        .await
        .unwrap();
    assert_eq!(found, g);
    for bad in ["nope", "../x/1-1", &format!("{run}/9-9"), "a:b/1-1"] {
        let r = within(Duration::from_secs(60), "поиск", ck.find(&root, bad)).await;
        assert!(matches!(r, Err(RestoreError::NotFound(_))), "{bad}: {r:?}");
    }
}
