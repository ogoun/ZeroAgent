//! Демонстрационный run: каждый шаг пишет строку ровно один раз, повтор после
//! завершения ничего не меняет, `replay_hash` не зависит от времени и id run.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use za_core::{DemoConfig, DemoOutcome, EffectError, replay_hash, run_demo};
use za_protocol::{EffectClass, EffectKey, EventKind, RunId, RunRole, RunStatus, Seq};
use za_store::{NewRun, ProjectStore};

/// Имя каталога теста `<name>-<pid>-<n>`: свой у каждого вызова и процесса, одновременные
/// прогоны одного бинарника не делят каталог (R1-C1, гигиена тестов).
fn unique(name: &str) -> String {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{name}-{}-{n}", std::process::id())
}

/// Чистый каталог теста внутри `CARGO_TARGET_TMPDIR`.
fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("za-core-demo")
        .join(unique(name));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cfg(steps: u32, workdir: &Path) -> DemoConfig {
    DemoConfig {
        steps,
        workdir: workdir.to_path_buf(),
        crash_at: None,
    }
}

fn log_lines(workdir: &Path) -> Vec<String> {
    std::fs::read_to_string(workdir.join("log.txt"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

async fn event_count(s: &ProjectStore, run: RunId) -> usize {
    s.read_run(run, Seq(0), usize::MAX).await.unwrap().len()
}

/// Значения `to` всех событий `run.status` run по порядку.
async fn statuses(s: &ProjectStore, run: RunId) -> Vec<String> {
    s.read_run(run, Seq(0), usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::RunStatus)
        .map(|e| e.payload["to"].as_str().unwrap().to_owned())
        .collect()
}

/// Поле `reason` последнего события `run.status` run.
async fn last_status_reason(s: &ProjectStore, run: RunId) -> Option<String> {
    s.read_run(run, Seq(0), usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.kind == EventKind::RunStatus)
        .and_then(|e| e.payload["reason"].as_str().map(str::to_owned))
}

/// Run в `running`, у которого `model` шага 1 записан с другим `prefix_hash`;
/// возвращает ключ этого эффекта.
async fn drifted_run(s: &ProjectStore, run: RunId) -> EffectKey {
    s.create_run(NewRun {
        id: run,
        project_id: "demo".into(),
        parent_id: None,
        role: RunRole::Top,
        model_profile: None,
    })
    .await
    .unwrap();
    s.set_status(run, RunStatus::Running, 0).await.unwrap();
    let key = EffectKey::derive(run, 1, 0);
    s.record_intent(
        run,
        &key,
        EffectClass::Idempotent,
        "model",
        json!({"step": 1, "prefix_hash": "0".repeat(64)}),
    )
    .await
    .unwrap();
    s.record_receipt(
        run,
        &key,
        json!({"outcome": "ok", "value": {"text": "ответ модели на шаг 1"}}),
    )
    .await
    .unwrap();
    key
}

#[tokio::test]
async fn demo_writes_each_step_once() {
    let d = dir("each_step");
    let s = ProjectStore::open(&d.join("project")).unwrap();
    let run = RunId::new();
    let work = d.join("work");

    let out = run_demo(&s, run, &cfg(5, &work)).await.unwrap();

    assert_eq!(out, DemoOutcome::Done);
    let want: Vec<String> = (1..=5).map(|n| format!("step {n}")).collect();
    assert_eq!(log_lines(&work), want);
    let r = s.get_run(run).await.unwrap().unwrap();
    assert_eq!(r.status, RunStatus::Done);
    assert_eq!(r.role, RunRole::Top);
    // Шаг n в журнале — эффекты `n:0` (model) и `n:1` (tool).
    let tool = s
        .effect(run, &EffectKey::derive(run, 3, 1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tool.class, EffectClass::NonIdempotent);
    assert_eq!(tool.name, "tool");
    let model = s
        .effect(run, &EffectKey::derive(run, 3, 0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(model.class, EffectClass::Idempotent);
    assert_eq!(model.name, "model");
}

#[tokio::test]
async fn rerun_after_finish_changes_nothing() {
    let d = dir("rerun");
    let s = ProjectStore::open(&d.join("project")).unwrap();
    let run = RunId::new();
    let work = d.join("work");

    run_demo(&s, run, &cfg(3, &work)).await.unwrap();
    let lines = log_lines(&work);
    let hash = replay_hash(&s, run).await.unwrap();
    let events = event_count(&s, run).await;

    let out = run_demo(&s, run, &cfg(3, &work)).await.unwrap();

    assert_eq!(out, DemoOutcome::Done);
    assert_eq!(log_lines(&work), lines);
    assert_eq!(replay_hash(&s, run).await.unwrap(), hash);
    assert_eq!(event_count(&s, run).await, events, "новых событий нет");
}

#[tokio::test]
async fn replay_hash_ignores_time() {
    let d = dir("hash_time");
    let s = ProjectStore::open(&d.join("project")).unwrap();
    let (a, b, c) = (RunId::new(), RunId::new(), RunId::new());

    run_demo(&s, a, &cfg(3, &d.join("a"))).await.unwrap();
    // Другие время, id run, каталог и seq событий.
    tokio::time::sleep(Duration::from_millis(20)).await;
    run_demo(&s, b, &cfg(3, &d.join("b"))).await.unwrap();
    run_demo(&s, c, &cfg(2, &d.join("c"))).await.unwrap();

    let ea = s.read_run(a, Seq(0), usize::MAX).await.unwrap();
    let eb = s.read_run(b, Seq(0), usize::MAX).await.unwrap();
    assert_eq!(ea.len(), eb.len());
    assert!(
        ea.iter().zip(&eb).all(|(x, y)| x.ts != y.ts),
        "время событий различается"
    );

    let ha = replay_hash(&s, a).await.unwrap();
    assert_eq!(ha.len(), 64);
    assert_eq!(ha, replay_hash(&s, b).await.unwrap());
    assert_ne!(ha, replay_hash(&s, c).await.unwrap(), "другой сценарий");
}

#[tokio::test]
async fn pending_tool_intent_needs_decision() {
    let d = dir("pending");
    let s = ProjectStore::open(&d.join("project")).unwrap();
    let run = RunId::new();
    let work = d.join("work");
    // Состояние после падения между намерением tool шага 1 и записью файла.
    s.create_run(NewRun {
        id: run,
        project_id: "demo".into(),
        parent_id: None,
        role: RunRole::Top,
        model_profile: None,
    })
    .await
    .unwrap();
    s.set_status(run, RunStatus::Running, 0).await.unwrap();
    let key = EffectKey::derive(run, 1, 1);
    s.record_intent(
        run,
        &key,
        EffectClass::NonIdempotent,
        "tool",
        json!({"file": "log.txt", "line": "step 1"}),
    )
    .await
    .unwrap();

    let want = DemoOutcome::NeedsDecision(vec![format!("unk-{}", key.0)]);
    assert_eq!(run_demo(&s, run, &cfg(3, &work)).await.unwrap(), want);
    let r = s.get_run(run).await.unwrap().unwrap();
    assert_eq!(r.status, RunStatus::Paused);
    let events = s.read_run(run, Seq(0), usize::MAX).await.unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.kind, EventKind::RunStatus);
    assert_eq!(
        last.payload,
        json!({"from": "running", "to": "paused", "generation": 2, "reason": "needs_decision"})
    );

    // Повтор даёт те же id, шагов не выполняет и событий не пишет.
    assert_eq!(run_demo(&s, run, &cfg(3, &work)).await.unwrap(), want);
    assert!(!work.join("log.txt").exists());
    assert_eq!(event_count(&s, run).await, events.len());
    assert_eq!(s.get_run(run).await.unwrap().unwrap(), r);
}

#[tokio::test]
async fn paused_run_without_open_decisions_resumes() {
    let d = dir("paused_resume");
    let s = ProjectStore::open(&d.join("project")).unwrap();
    let run = RunId::new();
    let work = d.join("work");
    s.create_run(NewRun {
        id: run,
        project_id: "demo".into(),
        parent_id: None,
        role: RunRole::Top,
        model_profile: None,
    })
    .await
    .unwrap();
    s.set_status(run, RunStatus::Running, 0).await.unwrap();
    s.set_status_with_reason(run, RunStatus::Paused, 1, Some("needs_decision"))
        .await
        .unwrap();

    assert_eq!(
        run_demo(&s, run, &cfg(2, &work)).await.unwrap(),
        DemoOutcome::Done
    );
    assert_eq!(log_lines(&work), ["step 1", "step 2"]);
    let statuses: Vec<_> = s
        .read_run(run, Seq(0), usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::RunStatus)
        .map(|e| e.payload["to"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(statuses, ["running", "paused", "running", "done"]);
}

/// Дрейф префикса: журнал записан с другим `prefix_hash` в аргументах `model`.
/// Продолжение — ошибка, шаги не выполняются.
#[tokio::test]
async fn prefix_drift_on_resume_is_an_error() {
    let d = dir("prefix_drift");
    let s = ProjectStore::open(&d.join("project")).unwrap();
    let run = RunId::new();
    let work = d.join("work");
    let key = drifted_run(&s, run).await;

    let err = run_demo(&s, run, &cfg(2, &work)).await.unwrap_err();
    assert_eq!(
        err,
        EffectError::ReplayMismatch {
            key: key.0,
            field: "args"
        }
    );
    assert!(!work.join("log.txt").exists(), "шаги не выполнены");
    let r = s.get_run(run).await.unwrap().unwrap();
    assert_eq!(r.status, RunStatus::Paused);
    assert_eq!(
        last_status_reason(&s, run).await,
        Some("replay_mismatch".into())
    );
}

/// Каждое продолжение с расхождением снова даёт `ReplayMismatch` и снова
/// ставит run на паузу с причиной `replay_mismatch`.
#[tokio::test]
async fn replay_mismatch_pauses_run_every_time() {
    let d = dir("replay_mismatch_twice");
    let s = ProjectStore::open(&d.join("project")).unwrap();
    let run = RunId::new();
    let work = d.join("work");
    let key = drifted_run(&s, run).await;
    let want = EffectError::ReplayMismatch {
        key: key.0,
        field: "args",
    };

    for _ in 0..2 {
        assert_eq!(run_demo(&s, run, &cfg(2, &work)).await.unwrap_err(), want);
        let r = s.get_run(run).await.unwrap().unwrap();
        assert_eq!(r.status, RunStatus::Paused);
        assert_eq!(
            last_status_reason(&s, run).await,
            Some("replay_mismatch".into())
        );
    }
    assert!(!work.join("log.txt").exists(), "шаги не выполнены");
    assert_eq!(
        statuses(&s, run).await,
        ["running", "paused", "running", "paused"]
    );
}
