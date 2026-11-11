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

/// Выгрузка маскирует шаблоны ключей D90 и без переменных `ZA_SECRET_*` в окружении
/// (ревью задачи 4, m4): токен в тексте, аргументы вызова, ключ PEM, пароль в URL.
#[test]
fn journal_export_masks_patterns_without_env() {
    // Фиктивные значения собираются из частей: цельная строка формата токена или ключа
    // в исходнике блокируется защитой push от секретов.
    let token = ["gh", "p_0123456789abcdefghijklmnopqrstuvwxyz"].concat();
    let pem = [
        "-----BEGIN OPENSSH PRIVATE",
        " KEY-----\nb3BlbnNzaFAKEexample0000\n",
        "-----END OPENSSH PRIVATE",
        " KEY-----",
    ]
    .concat();
    let (token, pem) = (token.as_str(), pem.as_str());
    let e = env("export-patterns");
    let (run, _) = with_run(&e, async |s, run| {
        s.append(event(run, json!({"text": format!("token {token}")})))
            .await
            .unwrap();
        // `raw_arguments` — текст JSON: токен после экранирования `\n` (ревью задачи 10, I2).
        s.append(event(
            run,
            json!({"arguments": {"cmd": format!("cat key\n{pem}")},
                   "raw_arguments": format!("{{\"url\":\"postgres://u:FakePass123@db/x\",\"cmd\":\"cd x\\n{token}\"}}")}),
        ))
        .await
        .unwrap();
    });
    let o = e.run(&[
        "journal",
        "export",
        "--project",
        e.project_arg(),
        "--run",
        &run.to_string(),
    ]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    for leak in [token, "b3BlbnNzaFAKEexample0000", "FakePass123"] {
        assert!(!o.stdout.contains(leak), "{leak}: {}", o.stdout);
    }
    for label in [
        "[secret masked: github_token]",
        "[secret masked: private_key]",
        "[secret masked: url_password]",
    ] {
        assert!(o.stdout.contains(label), "{label}: {}", o.stdout);
    }
    // Каждая строка — по-прежнему JSON.
    for l in o.stdout.lines() {
        serde_json::from_str::<Value>(l).unwrap_or_else(|err| panic!("{err}: {l}"));
    }
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
    // Показанный набор — текущий: хэш набора в payload прерывания (m2).
    let shown = zero_agent::trust::trust_set(&e.project).unwrap();
    with_run(&e, async |s, run| {
        for id in ["trust-a", "trust-b"] {
            let p = json!({"files": shown.files, "set_hash": shown.hash()});
            s.open_interrupt(id, run, "trust", p).await.unwrap();
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

    // Отказ с заметкой: заметка и в ответе, и в аудите (m7).
    let d = e.run(&[
        "answer",
        "--project",
        e.project_arg(),
        "--interrupt",
        "trust-b",
        "--deny",
        "--note",
        "чужой репозиторий",
    ]);
    assert_eq!(d.code(), Some(0), "{d:?}");
    let s = e.store();
    let row = rt.block_on(s.interrupt("trust-b")).unwrap().unwrap();
    assert_eq!(
        row.response,
        Some(json!({"trusted": false, "note": "чужой репозиторий"}))
    );
    let all = rt.block_on(s.audit_entries(0, 10)).unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(
        (all[2].action.as_str(), all[2].target.as_str()),
        ("approval.resolved", "trust-b")
    );
    assert_eq!(all[2].payload["approved"], json!(false));
    assert_eq!(all[2].payload["note"], json!("чужой репозиторий"));
    assert_eq!(rt.block_on(s.verify_audit()).unwrap(), Ok(()));
}

/// m2: набор доверенных файлов изменился между вопросом и ответом — доверие не
/// записывается, открывается новое прерывание с текущим набором (код 3), старое закрыто
/// ссылкой на новое. Ответ на новое записывает доверие.
#[test]
fn trust_set_changed_after_question_asks_again() {
    let e = env("trust-changed");
    std::fs::write(e.project.join("AGENTS.md"), "до вопроса").unwrap();
    let shown = zero_agent::trust::trust_set(&e.project).unwrap();
    with_run(&e, async |s, run| {
        let p = json!({"files": shown.files, "set_hash": shown.hash()});
        s.open_interrupt("trust-c", run, "trust", p).await.unwrap();
    });
    std::fs::write(e.project.join("AGENTS.md"), "после вопроса").unwrap();
    let o = answer(&e, "trust-c", "--approve");
    assert_eq!(o.code(), Some(3), "{o:?}");
    assert!(o.stderr.contains("изменились"), "{o:?}");
    let now = zero_agent::trust::trust_set(&e.project).unwrap();
    let new_id = format!("trust-c-set{}", &now.hash()[..16]);
    assert!(o.stderr.contains(&new_id), "{o:?}");
    let rt = rt();
    let s = e.store();
    assert_eq!(rt.block_on(s.last_audit("trust.")).unwrap(), None);
    let old = rt.block_on(s.interrupt("trust-c")).unwrap().unwrap();
    assert_eq!(old.response, Some(json!({"superseded_by": new_id})));
    let fresh = rt.block_on(s.interrupt(&new_id)).unwrap().unwrap();
    assert_eq!(fresh.kind, "trust");
    assert_eq!(
        fresh.payload.as_ref().unwrap()["set_hash"],
        json!(now.hash())
    );
    drop(s);
    let ok = answer(&e, &new_id, "--approve");
    assert_eq!(ok.code(), Some(0), "{ok:?}");
    let s = e.store();
    let g = rt.block_on(s.last_audit("trust.")).unwrap().unwrap();
    assert_eq!(g.payload, json!({"files": now.files}));
}

/// m3: `audit` на журнале с `NULL` или не-JSON в записи — текст разрыва D91 и код 1;
/// список печатает записи до разрыва.
#[test]
fn audit_verify_reports_break_on_null_and_garbage() {
    for (name, sql, at) in [
        (
            "audit-null",
            "UPDATE audit SET actor = NULL WHERE seq = 2",
            2,
        ),
        (
            "audit-garbage",
            "UPDATE audit SET payload = 'not json' WHERE seq = 2",
            2,
        ),
    ] {
        let e = env(name);
        with_run(&e, async |s, _| {
            for i in 0..3 {
                s.append_audit(za_store::AuditNew {
                    actor: "user".into(),
                    action: "grant.added".into(),
                    target: format!("/x/{i}"),
                    payload: json!({"i": i}),
                })
                .await
                .unwrap();
            }
        });
        rusqlite_exec(&e, sql);
        let text = format!("разрыв цепочки аудита на записи {at}");
        let v = e.run(&["audit", "--project", e.project_arg(), "--verify"]);
        assert_eq!(v.code(), Some(1), "{v:?}");
        assert!(v.stderr.contains(&text), "{v:?}");
        let l = e.run(&["audit", "--project", e.project_arg()]);
        assert_eq!(l.code(), Some(1), "{l:?}");
        assert!(l.stderr.contains(&text), "{l:?}");
        assert_eq!(l.stdout.lines().count(), 1, "{l:?}");
        assert!(l.stdout.contains("/x/0"), "{l:?}");
    }
}

/// SQL в журнал проекта мимо хранилища (подделка записи); хранилище закрыто.
fn rusqlite_exec(e: &Env, sql: &str) {
    rusqlite::Connection::open(e.journal_dir().join("journal.db"))
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}
