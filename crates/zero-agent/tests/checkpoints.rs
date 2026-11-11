//! Команда `checkpoints` (R1-C1 D89, D91, D96): список, восстановление с чекпоинтом «до» и
//! записью аудита `checkpoint.restored`, `--json`, неизвестный id, нет журнала, журнал
//! занят запущенным run.
//!
//! Репозиторий теста — `<ZeroAgent>/.tmp/r1c1-git/zero-agent-checkpoints/<name>-<pid>-<n>`
//! со своим `git init` (Global Constraints); `ZA_HOME` — в `CARGO_TARGET_TMPDIR`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use za_core::{CheckpointRequest, Checkpointer, ToolKind};
use za_jail::DirectRunner;
use za_protocol::{EffectKey, RunId, audit_action, checkpoint_kind};
use za_store::{AuditNew, ProjectStore};
use za_tools::{AgentHome, GitCheckpointer};
use zero_agent::home::project_id;
use zero_agent::trust::trust_set;

mod support;
use support::process::{Env, unique_dir, unique_dir_in};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `ZA_HOME` в каталоге тестов, проект — git-репозиторий в `.tmp/r1c1-git`.
fn env(name: &str) -> Env {
    let home = unique_dir("zero-agent-checkpoints", name).join("home");
    std::fs::create_dir_all(&home).unwrap();
    let project = unique_dir_in(
        &repo_root().join(".tmp/r1c1-git/zero-agent-checkpoints"),
        name,
    );
    Env {
        home,
        project: std::fs::canonicalize(&project)
            .map(|p| PathBuf::from(zero_agent::run::display_path(&p)))
            .unwrap(),
    }
}

fn git(e: &Env, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.autocrlf=false",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(&e.project)
        .env("GIT_CEILING_DIRECTORIES", repo_root().join(".tmp"))
        .output()
        .expect("git не запустился: нужен git в PATH");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Чекпоинт git проекта, как его сделал бы run.
fn checkpoint(e: &Env) -> za_core::Checkpoint {
    let ck = GitCheckpointer::new(
        Arc::new(DirectRunner),
        AgentHome {
            za_home: e.home.clone(),
            project_id: project_id(&e.project).unwrap(),
        },
    );
    let run = RunId::new();
    let cp = rt().block_on(ck.create(CheckpointRequest {
        project_dir: e.project.clone(),
        run,
        effect_key: EffectKey::derive(run, 1, 1),
        kind: ToolKind::Exec,
        paths: Vec::new(),
        trusted: true,
    }));
    assert_eq!(cp.kind, checkpoint_kind::GIT, "{cp:?}");
    cp
}

#[test]
fn list_and_restore_with_audit() {
    let e = env("restore");
    git(&e, &["init", "-q"]);
    std::fs::write(e.project.join("a.txt"), "before\n").unwrap();
    drop(ProjectStore::open(&e.journal_dir()).unwrap());
    let cp = checkpoint(&e);

    // Недоверенный проект: чекпоинты git не читаются (I2), восстановить их нельзя.
    let l = e.run(&["checkpoints", "list", "--project", e.project_arg()]);
    assert_eq!(l.code(), Some(0), "{l:?}");
    assert!(!l.stdout.contains(&cp.id), "{l:?}");
    assert!(l.stderr.contains("проект не доверен"), "{l:?}");
    let r = e.run(&[
        "checkpoints",
        "restore",
        "--project",
        e.project_arg(),
        &cp.id,
    ]);
    assert_eq!(r.code(), Some(1), "{r:?}");
    trust(&e);

    let l = e.run(&["checkpoints", "list", "--project", e.project_arg()]);
    assert_eq!(l.code(), Some(0), "{l:?}");
    assert!(
        l.stdout.contains(&cp.id) && l.stdout.contains("git"),
        "{l:?}"
    );
    let j = e.run(&[
        "checkpoints",
        "list",
        "--project",
        e.project_arg(),
        "--json",
    ]);
    let rows: Vec<Value> = j
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 1, "{j:?}");
    assert_eq!(rows[0]["id"], cp.id.as_str());

    std::fs::write(e.project.join("a.txt"), "after\n").unwrap();
    std::fs::write(e.project.join("new.txt"), "new\n").unwrap();
    let r = e.run(&[
        "checkpoints",
        "restore",
        "--project",
        e.project_arg(),
        &cp.id,
        "--delete-new",
        "--json",
    ]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(std::fs::read(e.project.join("a.txt")).unwrap(), b"before\n");
    assert!(!e.project.join("new.txt").exists());
    let v: Value = serde_json::from_str(r.stdout.trim()).unwrap();
    let mut changed: Vec<&str> = v["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    changed.sort_unstable();
    assert_eq!(changed, ["a.txt", "new.txt"]);
    let before = v["before"]["id"].as_str().unwrap().to_owned();
    assert_eq!(v["before"]["kind"], checkpoint_kind::GIT);

    // Аудит восстановления.
    let s = e.store();
    let a = rt()
        .block_on(s.last_audit(audit_action::CHECKPOINT_RESTORED))
        .unwrap()
        .expect("запись аудита");
    assert_eq!(a.target, cp.id);
    assert_eq!(a.actor, "user");
    assert_eq!(a.payload["before"], before.as_str());
    assert_eq!(a.payload["delete_new"], true);
    drop(s);

    // Чекпоинт «до» держит состояние перед восстановлением.
    let r = e.run(&[
        "checkpoints",
        "restore",
        "--project",
        e.project_arg(),
        &before,
        "--delete-new",
    ]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(std::fs::read(e.project.join("a.txt")).unwrap(), b"after\n");
    assert_eq!(std::fs::read(e.project.join("new.txt")).unwrap(), b"new\n");

    // Неизвестный id — код 1 и понятное сообщение.
    let bad = format!("{}/9-9", RunId::new());
    let n = e.run(&["checkpoints", "restore", "--project", e.project_arg(), &bad]);
    assert_eq!(n.code(), Some(1), "{n:?}");
    assert!(
        n.stderr.contains(&format!("чекпоинт {bad} не найден")),
        "{n:?}"
    );
}

/// Доверие к проекту, как его пишет `answer --approve` на прерывание доверия.
fn trust(e: &Env) {
    let set = trust_set(&e.project).unwrap();
    let s = e.store();
    rt().block_on(s.append_audit(AuditNew {
        actor: "user".into(),
        action: audit_action::TRUST_GRANTED.into(),
        target: e.project_arg().into(),
        payload: serde_json::to_value(&set).unwrap(),
    }))
    .unwrap();
}

#[test]
fn no_journal_and_busy_journal() {
    let e = env("journal");
    let l = e.run(&["checkpoints", "list", "--project", e.project_arg()]);
    assert_eq!(l.code(), Some(1), "{l:?}");
    assert!(l.stderr.contains("не найден"), "{l:?}");
    assert!(
        !e.journal_dir().join("journal.db").exists(),
        "журнал не создаётся"
    );

    let held = ProjectStore::open(&e.journal_dir()).unwrap();
    let b = e.run(&["checkpoints", "list", "--project", e.project_arg()]);
    drop(held);
    assert_eq!(b.code(), Some(1), "{b:?}");
    assert!(
        b.stderr
            .contains("журнал проекта занят запущенным run; повторите после его остановки"),
        "{b:?}"
    );
}
