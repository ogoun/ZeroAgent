//! Протокол эффектов намерение → выполнение → квитанция и восстановление
//! (CORE-002, CORE-003, CORE-004, CORE-007).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use za_core::{
    EffectError, EffectFailure, EffectSource, EffectSpec, Recovery, RunCtx, UnknownDecision,
    recover, resolve_unknown,
};
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

/// Намерение с именем и аргументами [`spec`], как его записал бы `RunCtx`.
async fn record_spec_intent(s: &ProjectStore, run: RunId, key: &EffectKey, class: EffectClass) {
    let sp = spec(class);
    s.record_intent(run, key, class, &sp.name, sp.args)
        .await
        .unwrap();
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
    record_spec_intent(&s, run, &key, EffectClass::Idempotent).await;

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
    record_spec_intent(&s, run, &key, EffectClass::NonIdempotent).await;

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
    record_spec_intent(&s, run, &key, EffectClass::NonIdempotent).await;

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
    record_spec_intent(&s, run, &key, EffectClass::Idempotent).await;
    s.record_receipt(run, &key, json!({"error": "голое значение"}))
        .await
        .unwrap();
    let mut ctx = RunCtx::new(s.clone(), run);
    let err = ctx
        .effect(spec(EffectClass::Idempotent), || async { Ok(json!(1)) })
        .await
        .unwrap_err();
    assert!(
        matches!(err, EffectError::Store(za_store::StoreError::Corrupt(_))),
        "получено {err:?}"
    );
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

#[tokio::test]
async fn store_error_poisons_the_context() {
    let s = ProjectStore::open(&dir("poison")).unwrap();
    // Run ещё не создан: намерение не запишется, это ошибка журнала.
    let run = RunId::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut ctx = RunCtx::new(s.clone(), run);
    let err = ctx
        .effect(spec(EffectClass::NonIdempotent), counted(&calls, json!(1)))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        EffectError::Store(za_store::StoreError::UnknownRun(run))
    );

    s.create_run(NewRun {
        id: run,
        project_id: "p".into(),
        parent_id: None,
        role: RunRole::Top,
        model_profile: None,
    })
    .await
    .unwrap();
    // Даже когда журнал снова исправен, отравленный контекст ничего не делает,
    // в том числе после смены шага.
    for _ in 0..2 {
        let err = ctx
            .effect(spec(EffectClass::NonIdempotent), counted(&calls, json!(1)))
            .await
            .unwrap_err();
        assert_eq!(err, EffectError::Poisoned);
        ctx.next_step();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(s.pending_effects(run).await.unwrap().is_empty());

    // Новый контекст начинает с того же ключа (шаг 0, эффект 0).
    let mut fresh = RunCtx::new(s.clone(), run);
    fresh
        .effect(spec(EffectClass::NonIdempotent), counted(&calls, json!(7)))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let row = s.effect(run, &first_key(run)).await.unwrap().unwrap();
    assert_eq!(row.result, Some(ok(json!(7))));
}

#[tokio::test]
async fn recreated_context_resumes_at_same_key_without_rerunning() {
    let (s, run) = setup("poison-resume").await;
    let key = first_key(run);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut ctx = RunCtx::new(s.clone(), run);

    // Действие выполнено, но квитанция контекста не записывается: внутри
    // замыкания квитанцию занимает другой писатель, запись даёт ошибку журнала.
    let probe = s.clone();
    let k = key.clone();
    let c = Arc::clone(&calls);
    let err = ctx
        .effect(spec(EffectClass::NonIdempotent), move || async move {
            c.fetch_add(1, Ordering::SeqCst);
            probe
                .record_receipt(run, &k, ok(json!("sent")))
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!("sent"))
        })
        .await
        .unwrap_err();
    assert!(matches!(err, EffectError::Store(_)), "получено {err:?}");

    // Повтор на том же контексте не занимает ключ (0, 1) и ничего не выполняет.
    let err = ctx
        .effect(spec(EffectClass::NonIdempotent), counted(&calls, json!(0)))
        .await
        .unwrap_err();
    assert_eq!(err, EffectError::Poisoned);
    assert!(
        s.effect(run, &EffectKey::derive(run, 0, 1))
            .await
            .unwrap()
            .is_none()
    );

    // Новый контекст после восстановления видит тот же ключ (0, 0) завершённым.
    assert_eq!(recover(&s, run).await.unwrap(), Recovery::Clean);
    let mut fresh = RunCtx::new(s.clone(), run);
    let r = fresh
        .effect(spec(EffectClass::NonIdempotent), counted(&calls, json!(0)))
        .await
        .unwrap();
    assert_eq!(r, json!("sent"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "необратимое действие не повторено"
    );
}

#[tokio::test]
async fn effect_row_carries_name_and_args() {
    let (s, run) = setup("name-args").await;
    let mut ctx = RunCtx::new(s.clone(), run);
    ctx.effect(spec(EffectClass::Pure), || async { Ok(json!(1)) })
        .await
        .unwrap();
    let row = s.effect(run, &first_key(run)).await.unwrap().unwrap();
    assert_eq!(row.name, "write_file");
    assert_eq!(row.args, json!({"path": "a.txt"}));
}

/// Подменённые при продолжении аргументы — ошибка, а не результат из журнала:
/// эффект не выполняется, контекст отравлен.
#[tokio::test]
async fn changed_args_on_replay_is_an_error() {
    let (s, run) = setup("mismatch-args").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut a = RunCtx::new(s.clone(), run);
    a.effect(spec(EffectClass::Idempotent), counted(&calls, json!(1)))
        .await
        .unwrap();

    let mut b = RunCtx::new(s.clone(), run);
    let mut changed = spec(EffectClass::Idempotent);
    changed.args = json!({"path": "b.txt"});
    let err = b
        .effect(changed, counted(&calls, json!(2)))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        EffectError::ReplayMismatch {
            key: first_key(run).0,
            field: "args"
        }
    );
    assert!(err.to_string().contains("расходится с журналом"), "{err}");
    let err = b
        .effect(spec(EffectClass::Idempotent), counted(&calls, json!(3)))
        .await
        .unwrap_err();
    assert_eq!(err, EffectError::Poisoned);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// Другое имя действия под тем же ключом — тоже расхождение, даже если
/// намерение ещё без квитанции: эффект не повторяется и прерывание не открывается.
#[tokio::test]
async fn changed_name_on_pending_intent_is_an_error() {
    let (s, run) = setup("mismatch-name").await;
    let key = first_key(run);
    record_spec_intent(&s, run, &key, EffectClass::NonIdempotent).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut ctx = RunCtx::new(s.clone(), run);
    let mut changed = spec(EffectClass::NonIdempotent);
    changed.name = "delete_file".into();
    let err = ctx
        .effect(changed, counted(&calls, json!(1)))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        EffectError::ReplayMismatch {
            key: key.0.clone(),
            field: "name"
        }
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(s.interrupts(run).await.unwrap().is_empty());
}

/// После `Unknown` контекст отравлен: следующий эффект не займёт ключ `seq+1`.
#[tokio::test]
async fn unknown_outcome_poisons_the_context() {
    let (s, run) = setup("unknown-poison").await;
    let key = first_key(run);
    record_spec_intent(&s, run, &key, EffectClass::NonIdempotent).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut ctx = RunCtx::new(s.clone(), run);
    let err = ctx
        .effect(spec(EffectClass::NonIdempotent), counted(&calls, json!(1)))
        .await
        .unwrap_err();
    assert!(
        matches!(err, EffectError::Unknown { .. }),
        "получено {err:?}"
    );
    for _ in 0..2 {
        let err = ctx
            .effect(spec(EffectClass::Pure), counted(&calls, json!(2)))
            .await
            .unwrap_err();
        assert_eq!(err, EffectError::Poisoned);
        ctx.next_step();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        s.effect(run, &EffectKey::derive(run, 0, 1))
            .await
            .unwrap()
            .is_none()
    );
}

/// Дробные числа в аргументах переживают запись в журнал без потери точности:
/// повтор не даёт ложного [`EffectError::ReplayMismatch`].
#[tokio::test]
async fn float_args_replay_without_mismatch() {
    let (s, run) = setup("float-args").await;
    // Детерминированный набор «трудных» f64: случайные биты и крайние значения.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut vals: Vec<f64> = (0..2000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            f64::from_bits(x)
        })
        .filter(|v| v.is_finite())
        .collect();
    vals.extend([
        0.1 + 0.2,
        5e-324,
        2.225_073_858_507_201_4e-308,
        f64::MAX,
        std::f64::consts::PI,
    ]);
    let sp = EffectSpec {
        class: EffectClass::Idempotent,
        name: "model".into(),
        args: json!({ "temperature": vals }),
    };
    let calls = Arc::new(AtomicUsize::new(0));
    RunCtx::new(s.clone(), run)
        .effect(sp.clone(), counted(&calls, json!(1)))
        .await
        .unwrap();
    let r = RunCtx::new(s.clone(), run)
        .effect(sp, counted(&calls, json!(2)))
        .await;
    assert_eq!(r, Ok(json!(1)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

// --- R1-B1, задача 2: слоты эффектов, продолжение с шага, решение неизвестного исхода ---

/// Спецификация чтения с номером `i`: у каждого слота своё намерение.
fn read_spec(i: u64) -> EffectSpec {
    EffectSpec {
        class: EffectClass::ReadOnly,
        name: "read".into(),
        args: json!({ "i": i }),
    }
}

/// Резервирует три слота текущего шага и выполняет их в `JoinSet` в обратном
/// порядке без часов: замыкание слота `i` ждёт, пока задача слота `i + 1`
/// завершит свой эффект (квитанция записана), и только после этого работает;
/// слот 2 начинает сразу. На `current_thread` задача, отправившая сигнал,
/// завершается в том же опросе, поэтому порядок завершения — 2, 1, 0.
/// При реплее замыкания не вызываются и никого не ждут.
/// Возвращает ключи слотов, значения по номеру слота и порядок завершения.
async fn run_three_reserved(
    ctx: &mut RunCtx,
    calls: &Arc<AtomicUsize>,
) -> (Vec<String>, Vec<Value>, Vec<u64>) {
    use tokio::sync::oneshot;
    let slots: Vec<_> = (0..3).map(|_| ctx.reserve().unwrap()).collect();
    let keys: Vec<String> = slots.iter().map(|s| s.key().0.clone()).collect();
    // Канал `j` ведёт от задачи слота `j + 1` к замыканию слота `j`.
    let (mut txs, mut rxs): (Vec<_>, Vec<_>) = (0..2).map(|_| oneshot::channel::<()>()).unzip();
    let mut waits: Vec<Option<oneshot::Receiver<()>>> = rxs.drain(..).map(Some).collect();
    waits.push(None);
    let mut notifies: Vec<Option<oneshot::Sender<()>>> = vec![None];
    notifies.extend(txs.drain(..).map(Some));
    let mut set = tokio::task::JoinSet::new();
    for (((i, slot), wait), notify) in slots.into_iter().enumerate().zip(waits).zip(notifies) {
        let i = i as u64;
        let c = Arc::clone(calls);
        set.spawn(async move {
            let r = slot
                .run(read_spec(i), move || async move {
                    if let Some(w) = wait {
                        w.await.unwrap();
                    }
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(json!(i))
                })
                .await;
            if let Some(n) = notify {
                // При реплее получатель уже брошен вместе с невызванным замыканием.
                let _ = n.send(());
            }
            (i, r)
        });
    }
    let mut values = vec![Value::Null; 3];
    let mut finished = Vec::new();
    while let Some(done) = set.join_next().await {
        let (i, r) = done.unwrap();
        ctx.observe(&r);
        finished.push(i);
        values[i as usize] = r.unwrap();
    }
    (keys, values, finished)
}

#[tokio::test]
async fn reserved_slots_keep_order_when_run_concurrently() {
    let (s, run) = setup("slots-order").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut ctx = RunCtx::new(s.clone(), run);
    ctx.next_step();
    let (keys, values, finished) = run_three_reserved(&mut ctx, &calls).await;
    assert_eq!(
        keys,
        vec![
            format!("{run}:1:0"),
            format!("{run}:1:1"),
            format!("{run}:1:2")
        ]
    );
    assert_eq!(values, vec![json!(0), json!(1), json!(2)]);
    assert_eq!(
        finished,
        vec![2, 1, 0],
        "слоты завершились в обратном порядке"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    for i in 0..3u64 {
        let row = s
            .effect(run, &EffectKey::derive(run, 1, i))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.args, json!({ "i": i }), "ключ 1:{i}");
        assert_eq!(row.result, Some(ok(json!(i))));
    }

    // Повтор из снимка шага 1: те же ключи и значения, замыкания не вызываются.
    let mut again = RunCtx::resume_at(s.clone(), run, 1, 0);
    let (keys2, values2, _) = run_three_reserved(&mut again, &calls).await;
    assert_eq!(keys2, keys);
    assert_eq!(values2, vec![json!(0), json!(1), json!(2)]);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "замыкания при повторе не вызваны"
    );
    assert!(!again.is_poisoned());
}

#[tokio::test]
async fn observe_poisons_on_unknown() {
    let (s, run) = setup("observe-unknown").await;
    let key = first_key(run);
    record_spec_intent(&s, run, &key, EffectClass::NonIdempotent).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut ctx = RunCtx::new(s.clone(), run);
    let slot = ctx.reserve().unwrap();
    assert_eq!(slot.key(), &key);
    let r = slot
        .run(spec(EffectClass::NonIdempotent), counted(&calls, json!(1)))
        .await;
    assert_eq!(
        r,
        Err(EffectError::Unknown {
            interrupt_id: format!("unk-{}", key.0)
        })
    );
    assert!(!ctx.is_poisoned(), "слот сам контекст не отравляет");
    ctx.observe(&r);
    assert!(ctx.is_poisoned());
    assert_eq!(ctx.reserve().unwrap_err(), EffectError::Poisoned);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn observe_ignores_success_and_failure() {
    let (s, run) = setup("observe-ok").await;
    let mut ctx = RunCtx::new(s.clone(), run);
    ctx.observe(&Ok::<_, EffectError>(json!(1)));
    ctx.observe(&Err::<Value, _>(EffectError::Failed("нет".into())));
    assert!(!ctx.is_poisoned());
    ctx.observe(&Err::<Value, _>(EffectError::ReplayMismatch {
        key: "k".into(),
        field: "args",
    }));
    assert!(ctx.is_poisoned());
}

#[tokio::test]
async fn run_traced_reports_source() {
    let (s, run) = setup("traced").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let model = |n: u64| EffectSpec {
        class: EffectClass::Idempotent,
        name: "model".into(),
        args: json!({ "n": n }),
    };

    let mut ctx = RunCtx::resume_at(s.clone(), run, 1, 0);
    let slot = ctx.reserve().unwrap();
    assert_eq!(slot.key().0, format!("{run}:1:0"));
    let c = Arc::clone(&calls);
    let live = slot
        .run_traced(model(0), move || async move {
            c.fetch_add(1, Ordering::SeqCst);
            Err::<Value, _>("interrupted:pause".to_owned())
        })
        .await;
    assert_eq!(
        live,
        (
            Err(EffectError::Failed("interrupted:pause".into())),
            EffectSource::Live
        )
    );
    let ok_live = ctx
        .reserve()
        .unwrap()
        .run_traced(model(1), counted(&calls, json!({"v": 1})))
        .await;
    assert_eq!(ok_live, (Ok(json!({"v": 1})), EffectSource::Live));
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let mut again = RunCtx::resume_at(s.clone(), run, 1, 0);
    let replay = again
        .reserve()
        .unwrap()
        .run_traced(model(0), counted(&calls, json!(0)))
        .await;
    assert_eq!(
        replay,
        (
            Err(EffectError::Failed("interrupted:pause".into())),
            EffectSource::Journal
        )
    );
    let ok_replay = again
        .reserve()
        .unwrap()
        .run_traced(model(1), counted(&calls, json!(0)))
        .await;
    assert_eq!(ok_replay, (Ok(json!({"v": 1})), EffectSource::Journal));
    assert_eq!(calls.load(Ordering::SeqCst), 2, "при повторе f не вызван");
}

#[tokio::test]
async fn run_traced_errors_without_receipt_are_live() {
    let (s, run) = setup("traced-live-errors").await;
    let key = first_key(run);
    record_spec_intent(&s, run, &key, EffectClass::NonIdempotent).await;
    let mut ctx = RunCtx::new(s.clone(), run);
    let (r, src) = ctx
        .reserve()
        .unwrap()
        .run_traced(spec(EffectClass::NonIdempotent), || async { Ok(json!(1)) })
        .await;
    assert!(matches!(r, Err(EffectError::Unknown { .. })), "{r:?}");
    assert_eq!(src, EffectSource::Live);
}

#[tokio::test]
async fn resume_at_continues_numbering() {
    let (s, run) = setup("resume-at").await;
    let mut c = RunCtx::resume_at(s.clone(), run, 4, 2);
    assert_eq!((c.step(), c.effect_seq(), c.run_id()), (4, 2, run));
    assert_eq!(c.reserve().unwrap().key().0, format!("{run}:4:2"));
    assert_eq!(c.effect_seq(), 3);
    c.next_step();
    assert_eq!(c.reserve().unwrap().key().0, format!("{run}:5:0"));
    assert!(!c.is_poisoned());
    assert!(c.store().get_run(run).await.unwrap().is_some());
}

/// `NonIdempotent`-намерение шага 1 без квитанции и id его прерывания после `recover`.
async fn unknown_at_step_1(s: &ProjectStore, run: RunId) -> String {
    let key = EffectKey::derive(run, 1, 0);
    record_spec_intent(s, run, &key, EffectClass::NonIdempotent).await;
    let id = format!("unk-{}", key.0);
    assert_eq!(
        recover(s, run).await.unwrap(),
        Recovery::NeedsDecision {
            interrupt_ids: vec![id.clone()]
        }
    );
    id
}

#[tokio::test]
async fn resolved_unknown_closes_effect() {
    let (s, run) = setup("resolve-done").await;
    let id = unknown_at_step_1(&s, run).await;
    assert_eq!(
        resolve_unknown(&s, &id, UnknownDecision::Done).await,
        Ok(UnknownDecision::Done)
    );
    assert_eq!(recover(&s, run).await.unwrap(), Recovery::Clean);

    let calls = Arc::new(AtomicUsize::new(0));
    let r = RunCtx::resume_at(s.clone(), run, 1, 0)
        .effect(spec(EffectClass::NonIdempotent), counted(&calls, json!(1)))
        .await;
    assert_eq!(r, Ok(json!({"resolved": "done"})));
    assert_eq!(calls.load(Ordering::SeqCst), 0, "действие не выполнено");
    let int = s.interrupt(&id).await.unwrap().unwrap();
    assert_eq!(int.status, "resolved");
    assert_eq!(int.response, Some(json!({"decision": "done"})));

    // Повтор — без изменений: другое решение не перезаписывает первое,
    // вызывающий получает действующее решение.
    let before = s.read_run(run, Seq(0), 1000).await.unwrap().len();
    assert_eq!(
        resolve_unknown(&s, &id, UnknownDecision::NotDone).await,
        Ok(UnknownDecision::Done)
    );
    assert_eq!(s.read_run(run, Seq(0), 1000).await.unwrap().len(), before);
    assert_eq!(
        s.interrupt(&id).await.unwrap().unwrap().response,
        Some(json!({"decision": "done"}))
    );
}

#[tokio::test]
async fn resolved_unknown_not_done_fails_effect() {
    let (s, run) = setup("resolve-not-done").await;
    let id = unknown_at_step_1(&s, run).await;
    assert_eq!(
        resolve_unknown(&s, &id, UnknownDecision::NotDone).await,
        Ok(UnknownDecision::NotDone)
    );
    assert_eq!(recover(&s, run).await.unwrap(), Recovery::Clean);
    let calls = Arc::new(AtomicUsize::new(0));
    let r = RunCtx::resume_at(s.clone(), run, 1, 0)
        .effect(spec(EffectClass::NonIdempotent), counted(&calls, json!(1)))
        .await;
    assert_eq!(
        r,
        Err(EffectError::Failed(
            "not executed: the user decided the action did not happen".into()
        ))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        s.interrupt(&id).await.unwrap().unwrap().response,
        Some(json!({"decision": "not_done"}))
    );
}

/// Число квитанций в журнале run.
async fn receipts(s: &ProjectStore, run: RunId) -> usize {
    s.read_run(run, Seq(0), 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::EffectReceipt)
        .count()
}

/// Сбой между квитанцией и решением: повтор только записывает решение.
#[tokio::test]
async fn resolve_unknown_after_receipt_only_resolves() {
    let (s, run) = setup("resolve-after-receipt").await;
    let id = unknown_at_step_1(&s, run).await;
    let key = EffectKey::derive(run, 1, 0);
    s.record_receipt(run, &key, ok(json!({"resolved": "done"})))
        .await
        .unwrap();
    assert_eq!(
        resolve_unknown(&s, &id, UnknownDecision::Done).await,
        Ok(UnknownDecision::Done)
    );
    let int = s.interrupt(&id).await.unwrap().unwrap();
    assert_eq!(int.status, "resolved");
    assert_eq!(int.response, Some(json!({"decision": "done"})));
    assert_eq!(receipts(&s, run).await, 1);
}

/// Квитанция `done` записана, решение — нет (сбой), человек снова отвечает
/// иначе: решение выводится из квитанции, аудит совпадает с тем, что увидит run.
#[tokio::test]
async fn resolve_unknown_follows_existing_receipt() {
    let (s, run) = setup("resolve-follows-receipt").await;
    let id = unknown_at_step_1(&s, run).await;
    let key = EffectKey::derive(run, 1, 0);
    s.record_receipt(run, &key, ok(json!({"resolved": "done"})))
        .await
        .unwrap();
    assert_eq!(
        resolve_unknown(&s, &id, UnknownDecision::NotDone).await,
        Ok(UnknownDecision::Done)
    );
    assert_eq!(
        s.interrupt(&id).await.unwrap().unwrap().response,
        Some(json!({"decision": "done"}))
    );
    assert_eq!(receipts(&s, run).await, 1);
    let r = RunCtx::resume_at(s.clone(), run, 1, 0)
        .effect(spec(EffectClass::NonIdempotent), || async { Ok(json!(0)) })
        .await;
    assert_eq!(r, Ok(json!({"resolved": "done"})));
}

/// Квитанция-отказ `not_done` и повтор с `Done` — действует `NotDone`.
#[tokio::test]
async fn resolve_unknown_follows_existing_not_done_receipt() {
    let (s, run) = setup("resolve-follows-not-done").await;
    let id = unknown_at_step_1(&s, run).await;
    let key = EffectKey::derive(run, 1, 0);
    s.record_receipt(
        run,
        &key,
        json!({"outcome": "error", "message": za_core::NOT_DONE_MESSAGE}),
    )
    .await
    .unwrap();
    assert_eq!(
        resolve_unknown(&s, &id, UnknownDecision::Done).await,
        Ok(UnknownDecision::NotDone)
    );
    assert_eq!(
        s.interrupt(&id).await.unwrap().unwrap().response,
        Some(json!({"decision": "not_done"}))
    );
}

/// Два одновременных решения: действует одно, оба вызова получают его,
/// квитанция и ответ прерывания совпадают.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_resolve_unknown_agrees() {
    for round in 0..8 {
        let (s, run) = setup(&format!("resolve-race-{round}")).await;
        let id = unknown_at_step_1(&s, run).await;
        let (a, b) = tokio::join!(
            tokio::spawn({
                let (s, id) = (s.clone(), id.clone());
                async move { resolve_unknown(&s, &id, UnknownDecision::Done).await }
            }),
            tokio::spawn({
                let (s, id) = (s.clone(), id.clone());
                async move { resolve_unknown(&s, &id, UnknownDecision::NotDone).await }
            }),
        );
        let (a, b) = (a.unwrap().unwrap(), b.unwrap().unwrap());
        assert_eq!(a, b, "раунд {round}: оба вызова видят одно решение");
        let want = match a {
            UnknownDecision::Done => (json!({"decision": "done"}), Ok(json!({"resolved": "done"}))),
            UnknownDecision::NotDone => (
                json!({"decision": "not_done"}),
                Err(EffectError::Failed(za_core::NOT_DONE_MESSAGE.into())),
            ),
        };
        assert_eq!(
            s.interrupt(&id).await.unwrap().unwrap().response,
            Some(want.0)
        );
        assert_eq!(receipts(&s, run).await, 1);
        let r = RunCtx::resume_at(s.clone(), run, 1, 0)
            .effect(spec(EffectClass::NonIdempotent), || async { Ok(json!(0)) })
            .await;
        assert_eq!(r, want.1, "раунд {round}");
    }
}

/// Прерывание, чей ключ принадлежит другому run, или эффект не `NonIdempotent` —
/// повреждение данных, а не решение.
#[tokio::test]
async fn resolve_unknown_rejects_inconsistent_payload() {
    let (s, run) = setup("resolve-inconsistent").await;
    let key = EffectKey::derive(run, 1, 0);
    record_spec_intent(&s, run, &key, EffectClass::Idempotent).await;
    let id = format!("unk-{}", key.0);
    s.open_interrupt(
        &id,
        run,
        za_core::UNKNOWN_OUTCOME,
        json!({"effect_key": key.0}),
    )
    .await
    .unwrap();
    assert!(matches!(
        resolve_unknown(&s, &id, UnknownDecision::Done).await,
        Err(za_store::StoreError::Corrupt(_))
    ));
    let other = EffectKey::derive(RunId::new(), 1, 0);
    let id2 = format!("unk-{}", other.0);
    s.open_interrupt(
        &id2,
        run,
        za_core::UNKNOWN_OUTCOME,
        json!({"effect_key": other.0}),
    )
    .await
    .unwrap();
    assert!(matches!(
        resolve_unknown(&s, &id2, UnknownDecision::Done).await,
        Err(za_store::StoreError::Corrupt(_))
    ));
    assert_eq!(receipts(&s, run).await, 0);
}

#[tokio::test]
async fn resolve_unknown_rejects_foreign_ids() {
    let (s, run) = setup("resolve-foreign").await;
    s.open_interrupt("appr-x", run, "approval", json!({}))
        .await
        .unwrap();
    for id in ["appr-x", "unk-nothing"] {
        assert_eq!(
            resolve_unknown(&s, id, UnknownDecision::Done)
                .await
                .unwrap_err(),
            za_store::StoreError::UnknownInterrupt(id.into())
        );
    }
    // Прерывание `unk-` без намерения под ключом из payload.
    let ghost = EffectKey::derive(run, 9, 0);
    let id = format!("unk-{}", ghost.0);
    s.open_interrupt(
        &id,
        run,
        za_core::UNKNOWN_OUTCOME,
        json!({"effect_key": ghost.0}),
    )
    .await
    .unwrap();
    assert_eq!(
        resolve_unknown(&s, &id, UnknownDecision::Done)
            .await
            .unwrap_err(),
        za_store::StoreError::UnknownInterrupt(id.clone())
    );
    assert_eq!(s.interrupt(&id).await.unwrap().unwrap().status, "open");
}

// ---- Разовый сбой без квитанции (R1-C1 D72) ----

#[tokio::test]
async fn transient_failure_writes_no_receipt_and_retries() {
    let (s, run) = setup("transient-retry").await;
    let key = first_key(run);
    let slot = RunCtx::new(s.clone(), run).reserve().unwrap();
    let (r, src) = slot
        .run_fallible(spec(EffectClass::ReadOnly), || async {
            Err(EffectFailure::Transient("busy".into()))
        })
        .await;
    assert_eq!(r, Err(EffectError::Transient("busy".into())));
    assert_eq!(src, EffectSource::Live);
    let row = s.effect(run, &key).await.unwrap().unwrap();
    assert!(row.receipt_seq.is_none(), "квитанции разового сбоя нет");
    assert!(row.result.is_none());

    // Новый контекст с того же шага повторяет безопасный эффект вживую.
    let slot2 = RunCtx::new(s.clone(), run).reserve().unwrap();
    let (r2, src) = slot2
        .run_fallible(spec(EffectClass::ReadOnly), || async { Ok(json!(1)) })
        .await;
    assert_eq!((r2.unwrap(), src), (json!(1), EffectSource::Live));
    assert!(
        s.effect(run, &key)
            .await
            .unwrap()
            .unwrap()
            .receipt_seq
            .is_some()
    );
}

#[tokio::test]
async fn recorded_failure_through_fallible_is_a_receipt() {
    let (s, run) = setup("transient-recorded").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut a = RunCtx::new(s.clone(), run);
    let err = a
        .effect_fallible(spec(EffectClass::Idempotent), || async {
            Err(EffectFailure::Recorded("нет доступа".into()))
        })
        .await
        .unwrap_err();
    assert_eq!(err, EffectError::Failed("нет доступа".into()));
    assert!(!a.is_poisoned(), "записанный отказ контекст не отравляет");
    let mut b = RunCtx::new(s.clone(), run);
    let err = b
        .effect(spec(EffectClass::Idempotent), counted(&calls, json!(1)))
        .await
        .unwrap_err();
    assert_eq!(err, EffectError::Failed("нет доступа".into()));
    assert_eq!(calls.load(Ordering::SeqCst), 0, "отказ не повторяется");
}

#[tokio::test]
async fn transient_non_idempotent_opens_unknown_on_retry() {
    let (s, run) = setup("transient-unknown").await;
    let mut a = RunCtx::new(s.clone(), run);
    let err = a
        .effect_fallible(spec(EffectClass::NonIdempotent), || async {
            Err(EffectFailure::Transient("busy".into()))
        })
        .await
        .unwrap_err();
    assert_eq!(err, EffectError::Transient("busy".into()));
    // Повтор с того же шага: намерение без квитанции, класс `NonIdempotent`.
    let calls = Arc::new(AtomicUsize::new(0));
    let mut b = RunCtx::new(s.clone(), run);
    let err = b
        .effect(spec(EffectClass::NonIdempotent), counted(&calls, json!(1)))
        .await
        .unwrap_err();
    assert!(matches!(err, EffectError::Unknown { .. }), "{err:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn transient_poisons_context() {
    let (s, run) = setup("transient-poison").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut ctx = RunCtx::new(s.clone(), run);
    let err = ctx
        .effect_fallible(spec(EffectClass::ReadOnly), || async {
            Err(EffectFailure::Transient("busy".into()))
        })
        .await
        .unwrap_err();
    assert!(matches!(err, EffectError::Transient(_)));
    assert!(ctx.is_poisoned());
    let err = ctx
        .effect(spec(EffectClass::ReadOnly), counted(&calls, json!(1)))
        .await
        .unwrap_err();
    assert_eq!(err, EffectError::Poisoned);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // То же через слот и `observe`.
    let mut ctx2 = RunCtx::new(s.clone(), run);
    ctx2.next_step();
    ctx2.observe(&Err::<Value, _>(EffectError::Transient("busy".into())));
    assert_eq!(ctx2.reserve().unwrap_err(), EffectError::Poisoned);
}
