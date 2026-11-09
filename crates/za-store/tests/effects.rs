//! Хранилище эффектов и прерываний (CORE-002, CORE-007).

use std::path::PathBuf;

use serde_json::json;
use za_protocol::{Actor, EffectClass, EffectKey, EventKind, RunId, RunRole, Seq};
use za_store::{BLOB_THRESHOLD, NewRun, ProjectStore, StoreError};

fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("za-store-effects")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

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

#[tokio::test]
async fn intent_and_receipt_write_rows_and_events() {
    let (s, run) = setup("roundtrip").await;
    let key = EffectKey::derive(run, 0, 0);
    let i = s
        .record_intent(run, &key, EffectClass::Idempotent, "fetch", json!({"u": 1}))
        .await
        .unwrap();
    let row = s.effect(run, &key).await.unwrap().unwrap();
    assert_eq!(row.class, EffectClass::Idempotent);
    assert_eq!(row.intent_seq, i);
    assert_eq!(row.receipt_seq, None);
    assert_eq!(row.result, None);

    let r = s.record_receipt(run, &key, json!([1, 2])).await.unwrap();
    let row = s.effect(run, &key).await.unwrap().unwrap();
    assert_eq!(row.receipt_seq, Some(r));
    assert_eq!(row.result, Some(json!([1, 2])));

    let evs = s.read_run(run, Seq(0), 100).await.unwrap();
    let intent = evs.iter().find(|e| e.seq == i).unwrap();
    assert_eq!(intent.kind, EventKind::EffectIntent);
    assert_eq!(
        intent.payload,
        json!({"key": key.0, "class": "idempotent", "name": "fetch", "args": {"u": 1}})
    );
    let receipt = evs.iter().find(|e| e.seq == r).unwrap();
    assert_eq!(receipt.kind, EventKind::EffectReceipt);
    assert_eq!(receipt.payload, json!({"key": key.0, "result": [1, 2]}));
}

#[tokio::test]
async fn missing_effect_is_none_and_receipt_without_intent_fails() {
    let (s, run) = setup("missing").await;
    let key = EffectKey::derive(run, 0, 0);
    assert_eq!(s.effect(run, &key).await.unwrap(), None);
    assert_eq!(
        s.record_receipt(run, &key, json!(1)).await.unwrap_err(),
        StoreError::UnknownEffect(key.clone())
    );
    // Отказ ничего не пишет в журнал.
    let kinds: Vec<_> = s
        .read_run(run, Seq(0), 100)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(kinds, vec![EventKind::RunCreated]);
}

#[tokio::test]
async fn duplicate_intent_and_receipt_are_conflicts() {
    let (s, run) = setup("conflict").await;
    let key = EffectKey::derive(run, 0, 0);
    s.record_intent(run, &key, EffectClass::Pure, "f", json!(null))
        .await
        .unwrap();
    assert_eq!(
        s.record_intent(run, &key, EffectClass::Pure, "f", json!(null))
            .await
            .unwrap_err(),
        StoreError::EffectConflict(key.clone())
    );
    s.record_receipt(run, &key, json!(1)).await.unwrap();
    assert_eq!(
        s.record_receipt(run, &key, json!(2)).await.unwrap_err(),
        StoreError::EffectConflict(key.clone())
    );
    let row = s.effect(run, &key).await.unwrap().unwrap();
    assert_eq!(
        row.result,
        Some(json!(1)),
        "первая квитанция не перезаписана"
    );
}

#[tokio::test]
async fn intent_for_unknown_run_fails() {
    let s = ProjectStore::open(&dir("unknown-run")).unwrap();
    let run = RunId::new();
    let key = EffectKey::derive(run, 0, 0);
    assert_eq!(
        s.record_intent(run, &key, EffectClass::NonIdempotent, "f", json!({}))
            .await
            .unwrap_err(),
        StoreError::UnknownRun(run)
    );
}

#[tokio::test]
async fn pending_effects_lists_only_intents_without_receipt_in_order() {
    let (s, run) = setup("pending").await;
    let keys: Vec<_> = (0..3).map(|n| EffectKey::derive(run, 0, n)).collect();
    for k in &keys {
        s.record_intent(run, k, EffectClass::NonIdempotent, "f", json!({}))
            .await
            .unwrap();
    }
    s.record_receipt(run, &keys[1], json!(1)).await.unwrap();
    let pending: Vec<_> = s
        .pending_effects(run)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(pending, vec![keys[0].clone(), keys[2].clone()]);
    // Другие run не видны.
    let other = RunId::new();
    assert!(s.pending_effects(other).await.unwrap().is_empty());
}

#[tokio::test]
async fn large_result_goes_to_blob() {
    let (s, run) = setup("large").await;
    let key = EffectKey::derive(run, 0, 0);
    s.record_intent(run, &key, EffectClass::ReadOnly, "read", json!({}))
        .await
        .unwrap();
    let big = json!("y".repeat(BLOB_THRESHOLD + 1));
    let r = s.record_receipt(run, &key, big.clone()).await.unwrap();
    let row = s.effect(run, &key).await.unwrap().unwrap();
    assert_eq!(row.result, Some(big));
    // В событии квитанции только ссылка на blob, а не сам результат.
    let ev = s
        .read_run(run, Seq(0), 100)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.seq == r)
        .unwrap();
    let h = ev.payload["result_blob"].as_str().unwrap().to_owned();
    assert!(ev.payload.get("result").is_none());
    assert!(ev.blob_ref.is_none());
    assert!(
        s.get_blob(&za_protocol::BlobHash(h))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn open_interrupt_is_idempotent_and_logged_once() {
    let (s, run) = setup("interrupt").await;
    for _ in 0..2 {
        s.open_interrupt("unk-x", run, "unknown_outcome", json!({"a": 1}))
            .await
            .unwrap();
    }
    let ints = s.interrupts(run).await.unwrap();
    assert_eq!(ints.len(), 1);
    assert_eq!(ints[0].id, "unk-x");
    assert_eq!(ints[0].run, run);
    assert_eq!(ints[0].kind, "unknown_outcome");
    assert_eq!(ints[0].status, "open");
    assert_eq!(ints[0].payload, Some(json!({"a": 1})));
    let opened: Vec<_> = s
        .read_run(run, Seq(0), 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::InterruptOpened)
        .collect();
    assert_eq!(opened.len(), 1);
    assert_eq!(
        opened[0].payload,
        json!({"id": "unk-x", "kind": "unknown_outcome", "payload": {"a": 1}})
    );
}

#[tokio::test]
async fn resolve_interrupt_once() {
    let (s, run) = setup("resolve").await;
    s.open_interrupt("appr-k", run, "approval", json!({}))
        .await
        .unwrap();
    assert_eq!(
        s.resolve_interrupt("appr-k", json!({"approved": true}))
            .await,
        Ok(true)
    );
    assert_eq!(
        s.resolve_interrupt("appr-k", json!({"approved": false}))
            .await,
        Ok(false)
    );
    let row = s.interrupt("appr-k").await.unwrap().unwrap();
    assert_eq!(row.status, "resolved");
    assert_eq!(row.response, Some(json!({"approved": true})));
    assert_eq!(row.run, run);
    assert_eq!(s.interrupts(run).await.unwrap()[0].response, row.response);

    let resolved: Vec<_> = s
        .read_run(run, Seq(0), 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::InterruptResolved)
        .collect();
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].payload,
        json!({"id": "appr-k", "kind": "approval", "response": {"approved": true}})
    );
    assert_eq!(resolved[0].actor, Actor::User);

    assert_eq!(
        s.resolve_interrupt("nope", json!({})).await,
        Err(StoreError::UnknownInterrupt("nope".into()))
    );
    assert_eq!(s.interrupt("nope").await, Ok(None));
}

#[tokio::test]
async fn open_interrupt_has_no_response() {
    let (s, run) = setup("open-no-response").await;
    s.open_interrupt("input-k", run, "input", json!({"q": 1}))
        .await
        .unwrap();
    let row = s.interrupt("input-k").await.unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.response), ("open", None));
}

#[tokio::test]
async fn step_has_effects_by_prefix() {
    let (s, run) = setup("step-prefix").await;
    s.record_intent(
        run,
        &EffectKey::derive(run, 3, 0),
        EffectClass::ReadOnly,
        "read",
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(s.step_has_effects(run, 3).await, Ok(true));
    assert_eq!(s.step_has_effects(run, 4).await, Ok(false));
    assert_eq!(s.step_has_effects(run, 30).await, Ok(false));
    assert_eq!(s.step_has_effects(RunId::new(), 3).await, Ok(false));

    // Двоеточие после шага: эффект шага 30 не делает шаг 3 «с эффектами».
    let other = RunId::new();
    s.create_run(NewRun {
        id: other,
        project_id: "p".into(),
        parent_id: None,
        role: RunRole::Top,
        model_profile: None,
    })
    .await
    .unwrap();
    s.record_intent(
        other,
        &EffectKey::derive(other, 30, 0),
        EffectClass::ReadOnly,
        "read",
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(s.step_has_effects(other, 3).await, Ok(false));
    assert_eq!(s.step_has_effects(other, 30).await, Ok(true));
}

#[tokio::test]
async fn concurrent_resolves_have_one_winner() {
    let (s, run) = setup("resolve-race").await;
    s.open_interrupt("input-k", run, "input", json!({}))
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        s.resolve_interrupt("input-k", json!({"answer": "a"})),
        s.resolve_interrupt("input-k", json!({"answer": "b"})),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert!(a ^ b, "ровно одно решение побеждает: {a} {b}");
    let resolved: Vec<_> = s
        .read_run(run, Seq(0), 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::InterruptResolved)
        .collect();
    assert_eq!(resolved.len(), 1);
    let row = s.interrupt("input-k").await.unwrap().unwrap();
    assert_eq!(
        Some(&resolved[0].payload["response"]),
        row.response.as_ref()
    );
    let winner = if a { "a" } else { "b" };
    assert_eq!(row.response, Some(json!({"answer": winner})));
}

#[tokio::test]
async fn resolve_as_records_the_deciding_actor() {
    let (s, run) = setup("resolve-actor").await;
    s.open_interrupt("timer-k", run, "timer", json!({}))
        .await
        .unwrap();
    assert_eq!(
        s.resolve_interrupt_as("timer-k", json!({"fired": true}), Actor::Runtime)
            .await,
        Ok(true)
    );
    let ev = s
        .read_run(run, Seq(0), 100)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == EventKind::InterruptResolved)
        .unwrap();
    assert_eq!(ev.actor, Actor::Runtime);
}
