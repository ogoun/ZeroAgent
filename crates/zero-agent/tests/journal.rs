//! Журнал проекта из бинарника: открытие с ограниченным повтором на `Locked`, выгрузка
//! событий run (`journal export`), аудит ответа на прерывание (`answer`, `audit`, R1-C1 D91)
//! и сообщение о журнале, занятом запущенным run.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use za_protocol::{Actor, EventKind, NewEvent, RunId, RunRole, Seq};
use za_store::{AuditEntry, INTERRUPT_RESOLVED, NewRun, ProjectStore, StoreError};
use zero_agent::journal::open_retrying;

mod support;
use support::process::{Env, env_in, unique_dir};

fn dir(name: &str) -> PathBuf {
    unique_dir("zero-agent-journal", name)
}

/// Чистые `ZA_HOME` и проект теста.
fn env(name: &str) -> Env {
    env_in("zero-agent-journal", name, "")
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

/// Журнал проекта `e` с одним run; `f` дописывает в него, пока хранилище открыто.
fn with_run<T>(e: &Env, f: impl AsyncFnOnce(&ProjectStore, RunId) -> T) -> (RunId, T) {
    let rt = rt();
    let store = ProjectStore::open(&e.journal_dir()).unwrap();
    let run = RunId::new();
    rt.block_on(async {
        store
            .create_run(NewRun {
                id: run,
                project_id: "p".into(),
                parent_id: None,
                role: RunRole::Top,
                model_profile: None,
            })
            .await
            .unwrap();
        let out = f(&store, run).await;
        (run, out)
    })
}

fn event(run: RunId, payload: Value) -> NewEvent {
    NewEvent {
        run_id: run,
        parent_seq: None,
        actor: Actor::Runtime,
        kind: EventKind::TurnStarted,
        payload,
        tainted: false,
    }
}

fn answer(e: &Env, interrupt: &str, flag: &str) -> support::process::Out {
    e.run(&[
        "answer",
        "--project",
        e.project_arg(),
        "--interrupt",
        interrupt,
        flag,
    ])
}

#[test]
fn open_waits_for_lock_release() {
    let d = dir("released");
    let held = ProjectStore::open(&d).unwrap();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        drop(held);
    });
    let s = open_retrying(&d);
    t.join().unwrap();
    assert!(s.is_ok(), "{s:?}");
}

#[test]
fn open_gives_up_after_bounded_retries() {
    let d = dir("held");
    let _held = ProjectStore::open(&d).unwrap();
    let start = Instant::now();
    assert!(matches!(open_retrying(&d), Err(StoreError::Locked)));
    let spent = start.elapsed();
    assert!(spent >= Duration::from_millis(2900), "{spent:?}");
    assert!(spent < Duration::from_secs(20), "{spent:?}");
}

/// Значение секрета, переданное процессу через `ZA_SECRET_T`, не попадает в выгрузку.
const SECRET: &str = "s3cr3t-Value-0123456789";

#[test]
fn journal_export_prints_masked_jsonl() {
    let e = env("export");
    let (run, n_events) = with_run(&e, async |s, run| {
        s.append(event(run, json!({"text": format!("token {SECRET} end")})))
            .await
            .unwrap();
        s.append(event(run, json!({"args": {"key": SECRET}})))
            .await
            .unwrap();
        // Событие другого run в выгрузку не попадает.
        let other = RunId::new();
        s.append(event(other, json!({"x": 1}))).await.unwrap();
        s.read_run(run, Seq(0), usize::MAX).await.unwrap().len()
    });
    let o = e.run_with(
        &[
            "journal",
            "export",
            "--project",
            e.project_arg(),
            "--run",
            &run.to_string(),
        ],
        &[("ZA_SECRET_T", SECRET)],
    );
    assert_eq!(o.code(), Some(0), "{o:?}");
    let lines: Vec<&str> = o.stdout.lines().collect();
    assert_eq!(lines.len(), n_events, "{}", o.stdout);
    for l in &lines {
        let v: Value = serde_json::from_str(l).unwrap_or_else(|err| panic!("{err}: {l}"));
        assert_eq!(v["run_id"], json!(run.to_string()), "{l}");
    }
    assert!(!o.stdout.contains(SECRET), "{}", o.stdout);
    assert!(!o.stderr.contains(SECRET), "{}", o.stderr);
    assert_eq!(
        o.stdout.matches("[secret masked: t]").count(),
        2,
        "{}",
        o.stdout
    );
    // Порядок — по `seq`.
    let seqs: Vec<u64> = lines
        .iter()
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["seq"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
}

#[test]
fn journal_export_unknown_run_fails() {
    let e = env("export-unknown");
    with_run(&e, async |_, _| ());
    let other = RunId::new().to_string();
    let o = e.run(&[
        "journal",
        "export",
        "--project",
        e.project_arg(),
        "--run",
        &other,
    ]);
    assert_eq!(o.code(), Some(1), "{o:?}");
    assert!(o.stderr.contains("не найден"), "{o:?}");
}

#[test]
fn answer_writes_approval_audit() {
    let e = env("answer-audit");
    let (run, ()) = with_run(&e, async |s, run| {
        s.open_interrupt("appr-test", run, "approval", json!({"tool": "bash"}))
            .await
            .unwrap();
    });
    let o = answer(&e, "appr-test", "--approve");
    assert_eq!(o.code(), Some(0), "{o:?}");
    let s = e.store();
    let rt = rt();
    let a = rt
        .block_on(s.last_audit("approval."))
        .unwrap()
        .expect("нет записи approval.resolved");
    assert_eq!(a.target, "appr-test");
    assert_eq!(
        (a.actor.as_str(), a.action.as_str()),
        ("user", "approval.resolved")
    );
    assert_eq!(a.payload["run"], json!(run.to_string()));
    assert_eq!(a.payload["approved"], json!(true));
    let row = rt.block_on(s.interrupt("appr-test")).unwrap().unwrap();
    assert_eq!(row.status, INTERRUPT_RESOLVED);
    drop(s);
    // Повторный ответ не меняет решения и не пишет аудит.
    let again = answer(&e, "appr-test", "--deny");
    assert_eq!(again.code(), Some(1), "{again:?}");
    let s = e.store();
    assert_eq!(rt.block_on(s.audit_entries(0, 10)).unwrap().len(), 1);
    drop(s);
    // `audit`: проверка цепочки и выгрузка записей.
    let v = e.run(&["audit", "--project", e.project_arg(), "--verify"]);
    assert_eq!(v.code(), Some(0), "{v:?}");
    assert!(v.stdout.contains("цепочка аудита цела"), "{v:?}");
    let j = e.run(&["audit", "--project", e.project_arg(), "--json"]);
    assert_eq!(j.code(), Some(0), "{j:?}");
    let entries: Vec<AuditEntry> = j
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].target, "appr-test");
    let t = e.run(&["audit", "--project", e.project_arg()]);
    assert_eq!(t.code(), Some(0), "{t:?}");
    assert!(t.stdout.contains("approval.resolved appr-test"), "{t:?}");
}

/// Решение заказчика (б), D83: ответ на прерывание доверия из CLI. `--approve` пишет
/// `approval.resolved` и `trust.granted` с набором доверенных файлов на момент ответа;
/// `--deny` — только `approval.resolved`, доверия нет.
#[test]
fn answer_trust_records_trust_in_audit() {
    let e = env("answer-trust");
    std::fs::write(e.project.join("AGENTS.md"), "правила проекта").unwrap();
    with_run(&e, async |s, run| {
        for id in ["trust-a", "trust-b"] {
            s.open_interrupt(id, run, "trust", json!({"files": {}}))
                .await
                .unwrap();
        }
    });
    let wrong = e.run(&[
        "answer",
        "--project",
        e.project_arg(),
        "--interrupt",
        "trust-a",
        "--text",
        "да",
    ]);
    assert_eq!(wrong.code(), Some(1), "{wrong:?}");
    assert!(wrong.stderr.contains("--approve или --deny"), "{wrong:?}");

    let o = answer(&e, "trust-a", "--approve");
    assert_eq!(o.code(), Some(0), "{o:?}");
    let rt = rt();
    let s = e.store();
    let row = rt.block_on(s.interrupt("trust-a")).unwrap().unwrap();
    assert_eq!(row.response, Some(json!({"trusted": true})));
    let all = rt.block_on(s.audit_entries(0, 10)).unwrap();
    let actions: Vec<&str> = all.iter().map(|a| a.action.as_str()).collect();
    assert_eq!(actions, ["approval.resolved", "trust.granted"]);
    assert_eq!(all[0].target, "trust-a");
    let granted = &all[1];
    let project = std::fs::canonicalize(&e.project).unwrap();
    assert_eq!(
        granted.target,
        zero_agent::run::display_path(&project),
        "{granted:?}"
    );
    let hash = blake3::hash("правила проекта".as_bytes())
        .to_hex()
        .to_string();
    assert_eq!(granted.payload, json!({"files": {"AGENTS.md": hash}}));
    assert_eq!(
        rt.block_on(s.last_audit("trust.")).unwrap().as_ref(),
        Some(granted)
    );
    drop(s);

    let d = answer(&e, "trust-b", "--deny");
    assert_eq!(d.code(), Some(0), "{d:?}");
    let s = e.store();
    let row = rt.block_on(s.interrupt("trust-b")).unwrap().unwrap();
    assert_eq!(row.response, Some(json!({"trusted": false})));
    let all = rt.block_on(s.audit_entries(0, 10)).unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(
        (all[2].action.as_str(), all[2].target.as_str()),
        ("approval.resolved", "trust-b")
    );
    assert_eq!(all[2].payload["approved"], json!(false));
    assert_eq!(rt.block_on(s.verify_audit()).unwrap(), Ok(()));
}

#[test]
fn locked_journal_message() {
    let e = env("locked");
    let (run, ()) = with_run(&e, async |_, _| ());
    // Журнал держит «запущенный run» — открытое хранилище этого процесса.
    let _held = e.store();
    let run = run.to_string();
    for args in [
        vec!["audit", "--project", e.project_arg()],
        vec![
            "journal",
            "export",
            "--project",
            e.project_arg(),
            "--run",
            &run,
        ],
    ] {
        let o = e.run(&args);
        assert_eq!(o.code(), Some(1), "{args:?}: {o:?}");
        assert!(
            o.stderr
                .contains("журнал проекта занят запущенным run; повторите после его остановки"),
            "{args:?}: {o:?}"
        );
    }
}
