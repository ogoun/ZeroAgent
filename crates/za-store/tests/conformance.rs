//! Conformance-тесты журнала `ProjectStore` (ARC-034).

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::json;
use za_protocol::{Actor, EventKind, NewEvent, RunId, Seq};
use za_store::{Durability, ProjectStore, StoreError, StoreOptions};

/// Чистый каталог теста внутри `CARGO_TARGET_TMPDIR`.
fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("za-store")
        .join(name);
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

#[tokio::test]
async fn disk_full_is_an_error_not_a_panic() {
    // Review Focus 1: маленький max_page_count имитирует заполненный диск.
    let d = dir("full");
    let mut opts = StoreOptions::default();
    opts.max_page_count = Some(64);
    let s = ProjectStore::open_with(&d, opts).unwrap();
    let before = s.append(ev()).await.unwrap();
    let mut big = ev();
    big.payload = json!({ "data": "x".repeat(1024 * 1024) });
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
