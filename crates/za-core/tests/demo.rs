//! Демонстрационный run: каждый шаг пишет строку ровно один раз, повтор после
//! завершения ничего не меняет, `replay_hash` не зависит от времени и id run.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use za_core::{DemoConfig, DemoOutcome, replay_hash, run_demo};
use za_protocol::{EffectClass, EffectKey, EventKind, RunId, RunRole, RunStatus, Seq};
use za_store::{NewRun, ProjectStore};

/// Чистый каталог теста внутри `CARGO_TARGET_TMPDIR`.
fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("za-core-demo")
        .join(name);
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
