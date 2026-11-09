//! Conformance-тесты журнала `ProjectStore` (ARC-034).

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::json;
use za_protocol::{Actor, BlobHash, EventKind, NewEvent, RunId, RunRole, RunStatus, Seq};
use za_store::{BLOB_THRESHOLD, Durability, NewRun, ProjectStore, StoreError, StoreOptions};

/// Путь каталога теста внутри `CARGO_TARGET_TMPDIR` без очистки.
fn dir_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("za-store")
        .join(name)
}

/// Чистый каталог теста внутри `CARGO_TARGET_TMPDIR`.
fn dir(name: &str) -> PathBuf {
    let d = dir_path(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn open(name: &str) -> ProjectStore {
    ProjectStore::open(&dir(name)).unwrap()
}

fn ev_for(run: RunId) -> NewEvent {
    NewEvent {
        run_id: run,
        parent_seq: None,
        actor: Actor::Runtime,
        kind: EventKind::TurnStarted,
        payload: json!({"b": 1, "a": [1, 2]}),
        tainted: false,
    }
}

fn ev() -> NewEvent {
    ev_for(RunId::new())
}

#[tokio::test]
async fn seq_is_monotonic_and_read_after_resumes() {
    let s = open("seq");
    let a = s.append(ev()).await.unwrap();
    let b = s.append(ev()).await.unwrap();
    assert!(b > a);
    let tail = s.read_after(a, 10).await.unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].seq, b);
}

#[tokio::test]
async fn appended_event_reads_back_intact() {
    let s = open("roundtrip");
    let run = RunId::new();
    let mut e = ev_for(run);
    e.parent_seq = Some(Seq(1));
    e.actor = Actor::Tool;
    e.kind = EventKind::Unknown("future.thing".into());
    e.tainted = true;
    let seq = s.append(e.clone()).await.unwrap();
    let got = s.read_run(run, Seq(0), 10).await.unwrap();
    assert_eq!(got.len(), 1);
    let g = &got[0];
    assert_eq!(g.seq, seq);
    assert_eq!(g.run_id, run);
    assert_eq!(g.parent_seq, Some(Seq(1)));
    assert_eq!(g.actor, Actor::Tool);
    assert_eq!(g.kind, e.kind);
    assert_eq!(g.payload, e.payload);
    assert_eq!(g.schema_v, za_protocol::SCHEMA_V);
    assert_eq!(g.blob_ref, None);
    assert!(g.tainted);
}

#[tokio::test]
async fn concurrent_appends_get_unique_seq() {
    let s = open("concurrent");
    let mut tasks = Vec::new();
    for _ in 0..100 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move { s.append(ev()).await.unwrap() }));
    }
    let mut seqs = HashSet::new();
    for t in tasks {
        assert!(seqs.insert(t.await.unwrap()), "повтор seq");
    }
    assert_eq!(seqs.len(), 100);
    assert_eq!(s.read_after(Seq(0), 1000).await.unwrap().len(), 100);
}

#[tokio::test]
async fn append_batch_is_ordered_and_read_run_filters() {
    let s = open("batch");
    let run = RunId::new();
    let other = s.append(ev()).await.unwrap();
    let seqs = s
        .append_batch(vec![ev_for(run), ev_for(run), ev_for(run)])
        .await
        .unwrap();
    assert_eq!(seqs.len(), 3);
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));
    assert!(seqs[0] > other);
    let got: Vec<Seq> = s
        .read_run(run, Seq(0), 10)
        .await
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(got, seqs);
    let after_first: Vec<Seq> = s
        .read_run(run, seqs[0], 1)
        .await
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(after_first, vec![seqs[1]]);
    assert!(s.append_batch(Vec::new()).await.unwrap().is_empty());
}

#[tokio::test]
async fn order_comes_from_seq_not_clock() {
    // Review Focus 3: часы пошли назад — порядок задаёт seq, а не ts.
    let d = dir("clock");
    let run = RunId::new();
    let mut appended = Vec::new();
    {
        let s = ProjectStore::open(&d).unwrap();
        for _ in 0..5 {
            appended.push(s.append(ev_for(run)).await.unwrap());
        }
    }
    // Переписываем ts в обратном порядке: чем больше seq, тем раньше время.
    {
        let c = rusqlite::Connection::open(d.join("journal.db")).unwrap();
        let n = c
            .execute(
                "UPDATE events SET ts = strftime('%Y-%m-%dT%H:%M:%SZ', 1000000000 - seq * 3600, 'unixepoch')",
                [],
            )
            .unwrap();
        assert_eq!(n, 5);
    }
    let s = ProjectStore::open(&d).unwrap();
    for got in [
        s.read_after(Seq(0), 10).await.unwrap(),
        s.read_run(run, Seq(0), 10).await.unwrap(),
    ] {
        let seqs: Vec<Seq> = got.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, appended);
        // Время действительно убывает, значит порядок не от ts.
        assert!(got.windows(2).all(|w| w[0].ts > w[1].ts));
    }
}

#[tokio::test]
async fn append_durable_interleaves_with_normal_appends() {
    // Durable- и обычные записи вперемешку: все подтверждены, seq уникальны и читаются.
    let s = open("durable");
    let mut tasks = Vec::new();
    for i in 0..40 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            if i % 4 == 0 {
                s.append_durable(ev()).await.unwrap()
            } else {
                s.append(ev()).await.unwrap()
            }
        }));
    }
    let mut seqs = HashSet::new();
    for t in tasks {
        assert!(seqs.insert(t.await.unwrap()));
    }
    let got: HashSet<Seq> = s
        .read_after(Seq(0), 100)
        .await
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(got, seqs);
}

#[tokio::test]
async fn full_durability_option_works() {
    let mut opts = StoreOptions::default();
    assert_eq!(opts.durability, Durability::Normal);
    opts.durability = Durability::Full;
    let s = ProjectStore::open_with(&dir("full-durability"), opts).unwrap();
    let a = s.append(ev()).await.unwrap();
    let b = s.append_durable(ev()).await.unwrap();
    assert!(b > a);
    assert_eq!(s.read_after(Seq(0), 10).await.unwrap().len(), 2);
}

#[test]
fn second_open_is_locked() {
    let d = dir("lock");
    let _a = ProjectStore::open(&d).unwrap();
    assert!(matches!(ProjectStore::open(&d), Err(StoreError::Locked)));
}

#[test]
fn lock_is_released_on_drop() {
    let d = dir("relock");
    drop(ProjectStore::open(&d).unwrap());
    ProjectStore::open(&d).unwrap();
}

#[test]
fn open_creates_missing_directory() {
    let d = dir("mkdir").join("вложенный каталог");
    ProjectStore::open(&d).unwrap();
    assert!(d.join("journal.db").exists());
    assert!(d.join("journal.lock").exists());
}

/// ARC-035: пока читатель держит снимок, WAL растёт; после закрытия читателя
/// следующие записи писателя обрезают файл WAL.
#[tokio::test]
async fn wal_shrinks_after_long_reader_closes() {
    const MIB: u64 = 1024 * 1024;
    let d = dir("wal-growth");
    let s = ProjectStore::open(&d).unwrap();
    let wal = d.join("journal.db-wal");
    s.append(ev()).await.unwrap();

    // Отдельное соединение держит транзакцию чтения: WAL нельзя начать заново.
    let reader = rusqlite::Connection::open(d.join("journal.db")).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let n: i64 = reader
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    let fill = "x".repeat(60 * 1024);
    for i in 0..200 {
        let mut e = ev();
        e.payload = json!({"i": i, "fill": fill});
        s.append(e).await.unwrap();
    }
    let grown = std::fs::metadata(&wal).unwrap().len();
    assert!(grown > 10 * MIB, "WAL вырос до {grown} байт");

    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    for _ in 0..40 {
        s.append(ev()).await.unwrap();
    }
    let after = std::fs::metadata(&wal).map_or(0, |m| m.len());
    assert!(
        after < grown / 4,
        "WAL не уменьшился: было {grown}, стало {after} байт"
    );
    assert_eq!(
        s.read_after(Seq(0), usize::MAX).await.unwrap().len(),
        241,
        "данные на месте"
    );
}

#[tokio::test]
async fn disk_full_is_an_error_not_a_panic() {
    // Review Focus 1: маленький max_page_count имитирует заполненный диск.
    let d = dir("full");
    let mut opts = StoreOptions::default();
    opts.max_page_count = Some(64);
    let s = ProjectStore::open_with(&d, opts).unwrap();
    let before = s.append(ev()).await.unwrap();
    // Payload уходит в blob, но до 1 МиБ blob лежит в строке `blobs` и в БД не влезает.
    let mut big = ev();
    big.payload = json!({ "data": "x".repeat(512 * 1024) });
    assert!(matches!(s.append(big).await, Err(StoreError::Full)));
    // Подтверждённое событие на месте, писатель жив и принимает новые записи.
    let got = s.read_after(Seq(0), 10).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].seq, before);
    let after = s.append(ev()).await.unwrap();
    assert!(after > before);
    drop(s);
    // После переоткрытия без ограничения данные целы.
    let s = ProjectStore::open(&d).unwrap();
    let seqs: Vec<Seq> = s
        .read_after(Seq(0), 10)
        .await
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(seqs, vec![before, after]);
}

#[test]
fn sqlite_version_is_at_least_3_51_3() {
    let s = open("version");
    let v: Vec<u32> = s
        .sqlite_version()
        .split('.')
        .map(|p| p.parse().unwrap())
        .collect();
    assert!(
        (v[0], v[1], v[2]) >= (3, 51, 3),
        "SQLite {} старше 3.51.3",
        s.sqlite_version()
    );
}

fn new_run(parent: Option<RunId>) -> NewRun {
    NewRun {
        id: RunId::new(),
        project_id: "p".into(),
        parent_id: parent,
        role: if parent.is_some() {
            RunRole::Worker
        } else {
            RunRole::Top
        },
        model_profile: None,
    }
}

#[tokio::test]
async fn large_payload_goes_to_blob_and_reads_back() {
    let s = open("blob-payload");
    let run = RunId::new();
    let mut e = ev_for(run);
    e.payload = json!({ "data": "y".repeat(100 * 1024) });
    let seq = s.append(e.clone()).await.unwrap();
    let small = s.append(ev_for(run)).await.unwrap();
    let got = s.read_run(run, Seq(0), 10).await.unwrap();
    assert_eq!(got[0].seq, seq);
    let h = got[0].blob_ref.clone().expect("blob_ref не заполнен");
    assert_eq!(got[0].payload, e.payload);
    assert_eq!(got[1].seq, small);
    assert_eq!(got[1].blob_ref, None);
    let all = s.read_after(Seq(0), 10).await.unwrap();
    assert_eq!(all[0].payload, e.payload);
    assert_eq!(all[0].blob_ref, Some(h.clone()));
    assert!(s.get_blob(&h).await.unwrap().unwrap().len() > BLOB_THRESHOLD);
    drop(s);
    // В строке события вместо payload лежит `null`.
    let c = rusqlite::Connection::open(dir_path("blob-payload").join("journal.db")).unwrap();
    let raw: String = c
        .query_row(
            "SELECT payload FROM events WHERE seq = ?1",
            [i64::try_from(seq.0).unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(raw, "null");
}

#[tokio::test]
async fn payload_at_threshold_stays_inline() {
    let s = open("blob-edge");
    let run = RunId::new();
    let mut e = ev_for(run);
    // Канонический JSON `"…"` ровно BLOB_THRESHOLD байт: порог не превышен.
    e.payload = json!("z".repeat(BLOB_THRESHOLD - 2));
    s.append(e.clone()).await.unwrap();
    let got = s.read_run(run, Seq(0), 1).await.unwrap();
    assert_eq!(got[0].blob_ref, None);
    assert_eq!(got[0].payload, e.payload);
}

#[tokio::test]
async fn blob_is_content_addressed_and_idempotent() {
    let s = open("blob-small");
    let a = s.put_blob(b"hello").await.unwrap();
    let b = s.put_blob(b"hello").await.unwrap();
    assert_eq!(a, b);
    assert_eq!(a.0, blake3::hash(b"hello").to_hex().to_string());
    assert_eq!(
        s.get_blob(&a).await.unwrap().as_deref(),
        Some(&b"hello"[..])
    );
    let missing = BlobHash("0".repeat(64));
    assert_eq!(s.get_blob(&missing).await.unwrap(), None);
}

#[tokio::test]
async fn blob_over_one_mib_is_a_file() {
    let d = dir("blob-file");
    let s = ProjectStore::open(&d).unwrap();
    let data = vec![7u8; 1024 * 1024 + 1];
    let h = s.put_blob(&data).await.unwrap();
    assert_eq!(s.put_blob(&data).await.unwrap(), h);
    assert!(d.join("blobs").join(&h.0).is_file());
    assert_eq!(s.get_blob(&h).await.unwrap(), Some(data.clone()));
    drop(s);
    let s = ProjectStore::open(&d).unwrap();
    assert_eq!(s.get_blob(&h).await.unwrap(), Some(data));
}

/// Строка blob есть, а файл пропал (например, после отключения питания):
/// повторный `put_blob` тех же данных восстанавливает файл.
#[tokio::test]
async fn put_blob_restores_missing_file() {
    let d = dir("blob-restore");
    let s = ProjectStore::open(&d).unwrap();
    let data = vec![9u8; 1024 * 1024 + 7];
    let h = s.put_blob(&data).await.unwrap();
    let file = d.join("blobs").join(&h.0);
    std::fs::remove_file(&file).unwrap();
    assert!(
        s.get_blob(&h).await.is_err(),
        "файла нет — чтение не удаётся"
    );

    assert_eq!(s.put_blob(&data).await.unwrap(), h);
    assert!(file.is_file());
    assert_eq!(s.get_blob(&h).await.unwrap(), Some(data.clone()));

    // Файл другой длины (оборванная запись) тоже переписывается.
    std::fs::write(&file, &data[..10]).unwrap();
    assert_eq!(s.put_blob(&data).await.unwrap(), h);
    assert_eq!(s.get_blob(&h).await.unwrap(), Some(data));
}

/// Подменённое содержимое файла blob — повреждение журнала, а не ошибка SQLite.
#[tokio::test]
async fn tampered_blob_file_is_corruption() {
    let d = dir("blob-tampered");
    let s = ProjectStore::open(&d).unwrap();
    let data = vec![3u8; 1024 * 1024 + 1];
    let h = s.put_blob(&data).await.unwrap();
    let mut other = data.clone();
    other[0] = 4;
    std::fs::write(d.join("blobs").join(&h.0), &other).unwrap();
    assert!(matches!(s.get_blob(&h).await, Err(StoreError::Corrupt(_))));
}

#[tokio::test]
async fn create_run_writes_row_and_event() {
    let s = open("run-create");
    let nr = new_run(None);
    let r = s.create_run(nr.clone()).await.unwrap();
    assert_eq!(r.id, nr.id);
    assert_eq!(r.root_id, nr.id);
    assert_eq!(r.parent_id, None);
    assert_eq!(r.role, RunRole::Top);
    assert_eq!(r.status, RunStatus::Queued);
    assert_eq!(r.generation, 0);
    assert_eq!(s.get_run(nr.id).await.unwrap(), Some(r));
    assert_eq!(s.get_run(RunId::new()).await.unwrap(), None);
    let evs = s.read_run(nr.id, Seq(0), 10).await.unwrap();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].kind, EventKind::RunCreated);
    assert_eq!(evs[0].actor, Actor::Runtime);
    // Повторное создание с тем же id отклоняется и не пишет второе событие.
    assert!(s.create_run(nr.clone()).await.is_err());
    assert_eq!(s.read_run(nr.id, Seq(0), 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn child_inherits_root() {
    let s = open("run-root");
    let top = s.create_run(new_run(None)).await.unwrap();
    let mid = s.create_run(new_run(Some(top.id))).await.unwrap();
    let leaf = s.create_run(new_run(Some(mid.id))).await.unwrap();
    assert_eq!(mid.root_id, top.root_id);
    assert_eq!(leaf.root_id, top.root_id);
    assert_eq!(leaf.parent_id, Some(mid.id));
    let orphan = new_run(Some(RunId::new()));
    let parent = orphan.parent_id.unwrap();
    assert_eq!(
        s.create_run(orphan).await,
        Err(StoreError::UnknownRun(parent))
    );
}

#[tokio::test]
async fn set_status_bumps_generation_and_logs_event() {
    let s = open("run-status");
    let r = s.create_run(new_run(None)).await.unwrap();
    assert_eq!(s.set_status(r.id, RunStatus::Running, 0).await, Ok(1));
    let got = s.get_run(r.id).await.unwrap().unwrap();
    assert_eq!((got.status, got.generation), (RunStatus::Running, 1));
    let evs = s.read_run(r.id, Seq(0), 10).await.unwrap();
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[1].kind, EventKind::RunStatus);
    assert_eq!(
        evs[1].payload,
        json!({"from": "queued", "to": "running", "generation": 1})
    );
    let unknown = RunId::new();
    assert_eq!(
        s.set_status(unknown, RunStatus::Running, 0).await,
        Err(StoreError::UnknownRun(unknown))
    );
}

#[tokio::test]
async fn set_status_with_reason_logs_reason() {
    let s = open("run-status-reason");
    let r = s.create_run(new_run(None)).await.unwrap();
    s.set_status(r.id, RunStatus::Running, 0).await.unwrap();
    assert_eq!(
        s.set_status_with_reason(r.id, RunStatus::Paused, 1, Some("needs_decision"))
            .await,
        Ok(2)
    );
    let got = s.get_run(r.id).await.unwrap().unwrap();
    assert_eq!((got.status, got.generation), (RunStatus::Paused, 2));
    let evs = s.read_run(r.id, Seq(0), 10).await.unwrap();
    assert_eq!(
        evs[2].payload,
        json!({"from": "running", "to": "paused", "generation": 2, "reason": "needs_decision"})
    );
    // Без причины payload прежний, поля `reason` нет.
    s.set_status_with_reason(r.id, RunStatus::Running, 2, None)
        .await
        .unwrap();
    let evs = s.read_run(r.id, Seq(0), 10).await.unwrap();
    assert_eq!(
        evs[3].payload,
        json!({"from": "paused", "to": "running", "generation": 3})
    );
}

#[tokio::test]
async fn invalid_transition_is_rejected() {
    let s = open("run-invalid");
    let r = s.create_run(new_run(None)).await.unwrap();
    let g = s.set_status(r.id, RunStatus::Cancelled, 0).await.unwrap();
    assert_eq!(
        s.set_status(r.id, RunStatus::Running, g).await,
        Err(StoreError::InvalidTransition {
            from: RunStatus::Cancelled,
            to: RunStatus::Running
        })
    );
    // Отказ ничего не меняет: generation та же, нового события нет.
    let got = s.get_run(r.id).await.unwrap().unwrap();
    assert_eq!((got.status, got.generation), (RunStatus::Cancelled, g));
    assert_eq!(s.read_run(r.id, Seq(0), 10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn stale_generation_is_rejected() {
    // SWM-064: команда со старой generation отклоняется.
    let s = open("run-stale");
    let r = s.create_run(new_run(None)).await.unwrap();
    s.set_status(r.id, RunStatus::Running, 0).await.unwrap();
    assert_eq!(
        s.set_status(r.id, RunStatus::Pausing, 0).await,
        Err(StoreError::StaleGeneration { current: 1 })
    );
    let got = s.get_run(r.id).await.unwrap().unwrap();
    assert_eq!((got.status, got.generation), (RunStatus::Running, 1));
    assert_eq!(s.read_run(r.id, Seq(0), 10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn latest_snapshot_is_the_highest_seq() {
    let s = open("snapshots");
    let r = s.create_run(new_run(None)).await.unwrap();
    assert_eq!(s.latest_snapshot(r.id).await.unwrap(), None);
    for n in [5u64, 9, 7] {
        s.save_snapshot(r.id, Seq(n), json!({ "step": n }))
            .await
            .unwrap();
    }
    assert_eq!(
        s.latest_snapshot(r.id).await.unwrap(),
        Some((Seq(9), json!({"step": 9})))
    );
    // Повторное сохранение того же seq заменяет снимок.
    s.save_snapshot(r.id, Seq(9), json!({"step": "again"}))
        .await
        .unwrap();
    assert_eq!(
        s.latest_snapshot(r.id).await.unwrap(),
        Some((Seq(9), json!({"step": "again"})))
    );
    assert_eq!(s.latest_snapshot(RunId::new()).await.unwrap(), None);
}
