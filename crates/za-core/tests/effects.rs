//! Протокол эффектов намерение → выполнение → квитанция и восстановление
//! (CORE-002, CORE-003, CORE-004, CORE-007).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use za_core::{EffectError, EffectSpec, Recovery, RunCtx, recover};
use za_protocol::{EffectClass, EffectKey, EventKind, RunId, RunRole, Seq};
use za_store::{BLOB_THRESHOLD, NewRun, ProjectStore};

/// Чистый каталог теста внутри `CARGO_TARGET_TMPDIR`.
fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("za-core")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Хранилище и созданный в нём run.
async fn setup(name: &str) -> (ProjectStore, RunId) {
    let s = ProjectStore::open(&dir(name)).unwrap();
    let run = RunId::new();
    s.create_run(NewRun {
        id: run,
        project_id: "p".into(),
        parent_id: None,
        role: RunRole::Top,
        model_profile: None,
    })
    .await
    .unwrap();
    (s, run)
}

fn spec(class: EffectClass) -> EffectSpec {
    EffectSpec {
        class,
        name: "write_file".into(),
        args: json!({"path": "a.txt"}),
    }
}

/// Замыкание эффекта, считающее вызовы и возвращающее `v`.
fn counted(
    calls: &Arc<AtomicUsize>,
    v: Value,
) -> impl FnOnce() -> std::future::Ready<Result<Value, String>> {
    let calls = Arc::clone(calls);
    move || {
        calls.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(v))
    }
}

/// Конверт квитанции успешного эффекта.
fn ok(v: Value) -> Value {
    json!({"outcome": "ok", "value": v})
}

/// Ключ первого эффекта шага 0.
fn first_key(run: RunId) -> EffectKey {
    EffectKey::derive(run, 0, 0)
}

#[tokio::test]
async fn completed_effect_is_not_executed_twice() {
    let (s, run) = setup("completed").await;
    let calls = Arc::new(AtomicUsize::new(0));

    let mut a = RunCtx::new(s.clone(), run);
    let r1 = a
        .effect(
            spec(EffectClass::NonIdempotent),
            counted(&calls, json!({"ok": 1})),
        )
        .await
        .unwrap();

    // Повтор после перезапуска: новый контекст с теми же run и шагом.
    let mut b = RunCtx::new(s.clone(), run);
    let r2 = b
        .effect(
            spec(EffectClass::NonIdempotent),
            counted(&calls, json!({"ok": 2})),
        )
        .await
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(r1, json!({"ok": 1}));
    assert_eq!(r2, json!({"ok": 1}), "результат берётся из журнала");
}

#[tokio::test]
async fn effect_keys_follow_step_and_effect_seq() {
    let (s, run) = setup("keys").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut ctx = RunCtx::new(s.clone(), run);
    ctx.effect(spec(EffectClass::Pure), counted(&calls, json!(1)))
        .await
        .unwrap();
    ctx.effect(spec(EffectClass::Pure), counted(&calls, json!(2)))
        .await
        .unwrap();
    ctx.next_step();
    ctx.effect(spec(EffectClass::Pure), counted(&calls, json!(3)))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    for (step, n, v) in [(0, 0, 1), (0, 1, 2), (1, 0, 3)] {
        let row = s
            .effect(run, &EffectKey::derive(run, step, n))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.result, Some(ok(json!(v))), "ключ {step}:{n}");
        assert!(row.receipt_seq.unwrap() > row.intent_seq);
    }
}

#[tokio::test]
async fn idempotent_intent_without_receipt_is_retried() {
    let (s, run) = setup("idem-retry").await;
    let key = first_key(run);
    s.record_intent(run, &key, EffectClass::Idempotent, "write_file", json!({}))
        .await
        .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let mut ctx = RunCtx::new(s.clone(), run);
    let r = ctx
        .effect(
            spec(EffectClass::Idempotent),
            counted(&calls, json!("done")),
        )
        .await
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(r, json!("done"));
    let row = s.effect(run, &key).await.unwrap().unwrap();
    assert_eq!(row.result, Some(ok(json!("done"))));
    assert!(s.pending_effects(run).await.unwrap().is_empty());
    assert!(s.interrupts(run).await.unwrap().is_empty());
}

#[tokio::test]
async fn non_idempotent_intent_without_receipt_interrupts() {
    let (s, run) = setup("nonidem-interrupt").await;
    let key = first_key(run);
    s.record_intent(
        run,
        &key,
        EffectClass::NonIdempotent,
        "send_mail",
        json!({}),
    )
    .await
    .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let mut ctx = RunCtx::new(s.clone(), run);
    let err = ctx
        .effect(spec(EffectClass::NonIdempotent), counted(&calls, json!(1)))
        .await
        .unwrap_err();

    let expected = format!("unk-{}", key.0);
    assert_eq!(
        err,
        EffectError::Unknown {
            interrupt_id: expected.clone()
        }
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "замыкание не вызвано");
    let ints = s.interrupts(run).await.unwrap();
    assert_eq!(ints.len(), 1);
    assert_eq!(ints[0].id, expected);
    assert_eq!(ints[0].kind, "unknown_outcome");
    assert_eq!(ints[0].status, "open");
    assert_eq!(
        ints[0].payload.as_ref().unwrap()["effect_key"],
        json!(key.0)
    );
    // Эффект остаётся незавершённым: решение за человеком.
    assert_eq!(s.pending_effects(run).await.unwrap().len(), 1);
}

#[tokio::test]
async fn interrupt_id_is_deterministic() {
    let (s, run) = setup("interrupt-id").await;
    let key = first_key(run);
    s.record_intent(run, &key, EffectClass::NonIdempotent, "pay", json!({}))
        .await
        .unwrap();

    let mut ids = Vec::new();
    for _ in 0..2 {
        let mut ctx = RunCtx::new(s.clone(), run);
        match ctx
            .effect(spec(EffectClass::NonIdempotent), || async { Ok(json!(1)) })
            .await
        {
            Err(EffectError::Unknown { interrupt_id }) => ids.push(interrupt_id),
            other => panic!("ожидалось Unknown, получено {other:?}"),
        }
    }
    let Recovery::NeedsDecision { interrupt_ids } = recover(&s, run).await.unwrap() else {
        panic!("ожидалось NeedsDecision");
    };
    ids.extend(interrupt_ids);

    assert_eq!(ids, vec![format!("unk-{}", key.0); 3]);
    // Повторное открытие идемпотентно: одна строка и одно событие.
    assert_eq!(s.interrupts(run).await.unwrap().len(), 1);
    let opened = s
        .read_run(run, Seq(0), 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::InterruptOpened)
        .count();
    assert_eq!(opened, 1);
}

#[tokio::test]
async fn intent_is_written_before_execution() {
    let (s, run) = setup("intent-first").await;
    let key = first_key(run);
    let mut ctx = RunCtx::new(s.clone(), run);
    let probe = s.clone();
    let k = key.clone();
    let seen = ctx
        .effect(spec(EffectClass::NonIdempotent), move || async move {
            let pending = probe
                .pending_effects(run)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!(pending.iter().any(|r| r.key == k)))
        })
        .await
        .unwrap();
    assert_eq!(seen, json!(true), "намерение видно внутри замыкания");
    assert!(s.pending_effects(run).await.unwrap().is_empty());
}

#[tokio::test]
async fn failed_effect_is_recorded_and_not_retried() {
    let (s, run) = setup("failed").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    let mut a = RunCtx::new(s.clone(), run);
    let err = a
        .effect(spec(EffectClass::Idempotent), move || async move {
            c.fetch_add(1, Ordering::SeqCst);
            Err::<Value, _>("нет доступа".to_owned())
        })
        .await
        .unwrap_err();
    assert_eq!(err, EffectError::Failed("нет доступа".into()));

    let row = s.effect(run, &first_key(run)).await.unwrap().unwrap();
    assert_eq!(
        row.result,
        Some(json!({"outcome": "error", "message": "нет доступа"}))
    );

    // Завершённый с ошибкой эффект не выполняется заново.
    let mut b = RunCtx::new(s.clone(), run);
    let err = b
        .effect(spec(EffectClass::Idempotent), counted(&calls, json!(1)))
        .await
        .unwrap_err();
    assert_eq!(err, EffectError::Failed("нет доступа".into()));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn success_that_looks_like_error_replays_as_success() {
    let (s, run) = setup("ok-like-error").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let v = json!({"error": "это данные, а не отказ"});
    let mut a = RunCtx::new(s.clone(), run);
    let r1 = a
        .effect(spec(EffectClass::Idempotent), counted(&calls, v.clone()))
        .await
        .unwrap();
    let mut b = RunCtx::new(s.clone(), run);
    let r2 = b
        .effect(spec(EffectClass::Idempotent), counted(&calls, json!(0)))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(r1, v);
    assert_eq!(r2, v, "успех при повторе остаётся успехом");
}

#[tokio::test]
async fn receipt_without_envelope_is_corruption() {
    let (s, run) = setup("no-envelope").await;
    let key = first_key(run);
    s.record_intent(run, &key, EffectClass::Idempotent, "f", json!({}))
        .await
        .unwrap();
    s.record_receipt(run, &key, json!({"error": "голое значение"}))
        .await
        .unwrap();
    let mut ctx = RunCtx::new(s.clone(), run);
    let err = ctx
        .effect(spec(EffectClass::Idempotent), || async { Ok(json!(1)) })
        .await
        .unwrap_err();
    assert!(matches!(err, EffectError::Store(_)), "получено {err:?}");
}

#[tokio::test]
async fn large_result_round_trips() {
    let (s, run) = setup("large-result").await;
    let big = json!({"text": "x".repeat(BLOB_THRESHOLD + 10)});
    let mut a = RunCtx::new(s.clone(), run);
    let r = a
        .effect(spec(EffectClass::ReadOnly), {
            let big = big.clone();
            move || async move { Ok(big) }
        })
        .await
        .unwrap();
    assert_eq!(r, big);
    let row = s.effect(run, &first_key(run)).await.unwrap().unwrap();
    assert_eq!(row.result, Some(ok(big)));
}

#[tokio::test]
async fn recover_reports_unknown_outcomes() {
    let (s, run) = setup("recover").await;
    assert_eq!(recover(&s, run).await.unwrap(), Recovery::Clean);

    // Завершённый эффект и незавершённый Idempotent не требуют решения.
    let mut ctx = RunCtx::new(s.clone(), run);
    ctx.effect(spec(EffectClass::NonIdempotent), || async { Ok(json!(1)) })
        .await
        .unwrap();
    let idem = EffectKey::derive(run, 0, 1);
    s.record_intent(run, &idem, EffectClass::Idempotent, "fetch", json!({}))
        .await
        .unwrap();
    assert_eq!(recover(&s, run).await.unwrap(), Recovery::Clean);

    let a = EffectKey::derive(run, 1, 0);
    let b = EffectKey::derive(run, 1, 1);
    for k in [&a, &b] {
        s.record_intent(run, k, EffectClass::NonIdempotent, "pay", json!({}))
            .await
            .unwrap();
    }
    let expected = vec![format!("unk-{}", a.0), format!("unk-{}", b.0)];
    assert_eq!(
        recover(&s, run).await.unwrap(),
        Recovery::NeedsDecision {
            interrupt_ids: expected.clone()
        }
    );
    let ids: Vec<String> = s
        .interrupts(run)
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(ids, expected);
}

#[tokio::test]
async fn recover_unknown_run_is_an_error() {
    let s = ProjectStore::open(&dir("recover-unknown")).unwrap();
    let run = RunId::new();
    assert_eq!(
        recover(&s, run).await.unwrap_err(),
        za_store::StoreError::UnknownRun(run)
    );
}
