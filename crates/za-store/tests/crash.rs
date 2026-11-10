//! Подтверждённые события переживают `kill -9` писателя (NFR-08, CORE-005).
//!
//! Тестовый бинарник `crash_writer` пишет события и печатает `seq` каждого
//! подтверждённого. Тест убивает его в случайный момент (`Child::kill`:
//! SIGKILL на Linux, TerminateProcess на Windows), открывает журнал заново
//! и проверяет, что все напечатанные `seq` на месте. В режиме `--audit` писатель
//! добавляет записи аудита (R1-C1 D91): после убийства цепочка цела, подтверждённые
//! записи на месте; первый раунд идёт по копии журнала R1-B1 (схема 2), поэтому
//! убийство может попасть и в миграцию 3.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use za_protocol::Seq;
use za_store::{ProjectStore, StoreError};

/// Сколько ждать первого подтверждения: запуск процесса и миграции.
const FIRST_ACK_TIMEOUT: Duration = Duration::from_secs(20);

#[path = "../../../test-support/unique_dir.rs"]
mod unique_dir;
use unique_dir::unique_dir;

/// Чистый каталог теста `CARGO_TARGET_TMPDIR/za-store/<name>-<pid>-<n>`.
fn dir(name: &str) -> PathBuf {
    unique_dir("za-store", name)
}

/// Запущенный писатель и поток, читающий его stdout.
struct Writer {
    child: Child,
    seqs: mpsc::Receiver<Seq>,
    reader: Option<JoinHandle<()>>,
}

/// Режим писателя: события, события через `append_durable`, записи аудита.
#[derive(Clone, Copy)]
enum Mode {
    Events,
    Durable,
    Audit,
}

fn spawn_writer(d: &Path, mode: Mode) -> Writer {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_crash_writer"));
    cmd.arg(d);
    match mode {
        Mode::Events => {}
        Mode::Durable => {
            cmd.arg("--durable");
        }
        Mode::Audit => {
            cmd.arg("--audit");
        }
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, seqs) = mpsc::channel();
    // Отдельный поток: чтение блокирующее, а таймаут ждёт в recv_timeout.
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            let n: u64 = line
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("неожиданная строка писателя: {line:?}"));
            if tx.send(Seq(n)).is_err() {
                break;
            }
        }
    });
    Writer {
        child,
        seqs,
        reader: Some(reader),
    }
}

/// Ждёт первое подтверждение, затем собирает подтверждения в течение `window`.
fn read_seqs_for(w: &mut Writer, window: Duration) -> Vec<Seq> {
    let first = w
        .seqs
        .recv_timeout(FIRST_ACK_TIMEOUT)
        .expect("писатель не подтвердил ни одного события");
    let mut acked = vec![first];
    let deadline = Instant::now() + window;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match w.seqs.recv_timeout(left) {
            Ok(s) => acked.push(s),
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("писатель завершился сам"),
        }
    }
    acked
}

/// Убивает писателя и добирает подтверждения, напечатанные до смерти.
fn kill(w: &mut Writer, acked: &mut Vec<Seq>) {
    w.child.kill().unwrap(); // TerminateProcess на Windows, SIGKILL на Linux
    w.child.wait().unwrap();
    if let Some(r) = w.reader.take() {
        r.join().unwrap();
    }
    acked.extend(w.seqs.try_iter());
}

fn read_all_seqs(s: &ProjectStore) -> HashSet<Seq> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut got = HashSet::new();
        let mut after = Seq(0);
        loop {
            let page = s.read_after(after, 1000).await.unwrap();
            let Some(last) = page.last() else { break };
            after = last.seq;
            got.extend(page.iter().map(|e| e.seq));
        }
        got
    })
}

/// Открывает журнал после убийства писателя. Блокировку `journal.lock` ОС
/// снимает вместе с процессом, но на Windows не всегда мгновенно: `Locked`
/// повторяется ограниченно (10 × 100 мс), как в бинарнике zero-agent.
fn open_after_kill(d: &Path) -> ProjectStore {
    for _ in 0..10 {
        match ProjectStore::open(d) {
            Err(StoreError::Locked) => std::thread::sleep(Duration::from_millis(100)),
            r => return r.unwrap(),
        }
    }
    ProjectStore::open(d).unwrap()
}

fn crash_rounds(name: &str, rounds: u64, durable: bool) {
    for round in 0..rounds {
        let d = dir(&format!("{name}{round}"));
        let mode = if durable { Mode::Durable } else { Mode::Events };
        let mut w = spawn_writer(&d, mode);
        let mut acked = read_seqs_for(&mut w, Duration::from_millis(50 + round * 37));
        kill(&mut w, &mut acked);
        let s = open_after_kill(&d);
        let got = read_all_seqs(&s);
        let lost: Vec<_> = acked.iter().filter(|q| !got.contains(q)).collect();
        assert!(
            lost.is_empty(),
            "раунд {round}: потеряно {} подтверждённых событий из {}, первые: {:?}",
            lost.len(),
            acked.len(),
            &lost[..lost.len().min(10)]
        );
    }
}

#[test]
fn acknowledged_events_survive_kill() {
    crash_rounds("crash", 20, false);
}

#[test]
fn acknowledged_durable_events_survive_kill() {
    crash_rounds("crash-durable", 5, true);
}

/// Записи аудита переживают `kill -9`: после каждого убийства цепочка цела (без
/// разрыва), подтверждённые записи на месте, `seq` идут подряд с 1. Журнал общий для
/// всех раундов, цепочка продолжается через сбои. Первый раунд начинается с журнала
/// R1-B1 схемы 2 и убивается без ожидания подтверждений: сбой может прийтись на
/// миграцию 3, которая идёт одной транзакцией.
#[test]
fn audit_chain_survives_kill() {
    let d = dir("crash-audit");
    let golden = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../za-agent/tests/golden/r1b1_v1/approval/store/journal.db");
    std::fs::copy(golden, d.join("journal.db")).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut acked_all: Vec<u64> = Vec::new();
    for round in 0..12u64 {
        let mut w = spawn_writer(&d, Mode::Audit);
        let mut acked = if round < 2 {
            // Сразу после запуска: открытие и миграция ещё идут.
            std::thread::sleep(Duration::from_millis(5 + round * 20));
            Vec::new()
        } else {
            read_seqs_for(&mut w, Duration::from_millis(30 + round * 23))
        };
        kill(&mut w, &mut acked);
        acked_all.extend(acked.iter().map(|q| q.0));
        let s = open_after_kill(&d);
        assert_eq!(
            rt.block_on(s.verify_audit()).unwrap(),
            Ok(()),
            "раунд {round}: цепочка аудита разорвана"
        );
        let got: Vec<u64> = rt
            .block_on(s.audit_entries(0, usize::MAX))
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(
            got,
            (1..=got.len() as u64).collect::<Vec<_>>(),
            "раунд {round}: seq не подряд"
        );
        let lost: Vec<_> = acked_all.iter().filter(|q| !got.contains(q)).collect();
        assert!(
            lost.is_empty(),
            "раунд {round}: потеряны подтверждённые записи аудита {lost:?}"
        );
    }
    assert!(
        !acked_all.is_empty(),
        "писатель не подтвердил ни одной записи"
    );
}
