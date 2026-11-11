//! Тестовый писатель для проверки устойчивости журнала к `kill -9`
//! (`tests/crash.rs`). Не для использования вне тестов.
//!
//! `crash_writer <dir> [--durable | --audit | --abort-in-migration N]`: открывает журнал в `<dir>` и бесконечно
//! пишет события из нескольких потоков (чтобы убийство попадало и в середину
//! group commit). После подтверждения каждого события печатает его `seq`
//! отдельной строкой и сразу сбрасывает stdout. С `--durable` пишет через
//! `append_durable`, с `--audit` — записи аудита (`append_audit`, R1-C1 D91) и печатает
//! их `seq`. С `--abort-in-migration N` процесс аварийно завершается посреди миграции
//! схемы версии N (после её SQL, до COMMIT) и ничего не пишет.

use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

use serde_json::json;
use za_protocol::{Actor, EventKind, NewEvent, RunId};
use za_store::{AuditNew, ProjectStore, StoreError, StoreOptions};

/// Число параллельных потоков-клиентов.
const CLIENTS: usize = 4;

/// Что пишет клиент.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Events,
    Durable,
    Audit,
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(dir) = args.next().map(PathBuf::from) else {
        eprintln!(
            "использование: crash_writer <dir> [--durable | --audit | --abort-in-migration N]"
        );
        return ExitCode::from(2);
    };
    let mut opts = StoreOptions::default();
    let mode = match args.next() {
        None => Mode::Events,
        Some(a) if a == "--abort-in-migration" => {
            let Some(v) = args.next().and_then(|v| v.to_str()?.parse().ok()) else {
                eprintln!("crash_writer: --abort-in-migration ждёт номер версии");
                return ExitCode::from(2);
            };
            opts.abort_in_migration = Some(v);
            Mode::Events
        }
        Some(a) if a == "--durable" => Mode::Durable,
        Some(a) if a == "--audit" => Mode::Audit,
        Some(a) => {
            eprintln!("crash_writer: неизвестный режим {a:?}");
            return ExitCode::from(2);
        }
    };
    // Прежний писатель убит, а тест мог ещё не отпустить журнал (своё открытие для
    // проверки): блокировка снимается не мгновенно — повтор на `Locked` до 5 с.
    let mut tries = 0;
    let opened = loop {
        match ProjectStore::open_with(&dir, opts.clone()) {
            Err(StoreError::Locked) if tries < 50 => {
                tries += 1;
                thread::sleep(std::time::Duration::from_millis(100));
            }
            r => break r,
        }
    };
    let store = match opened {
        Ok(s) => s,
        Err(e) => {
            eprintln!("crash_writer: {e}");
            return ExitCode::FAILURE;
        }
    };
    let clients: Vec<_> = (0..CLIENTS)
        .map(|_| {
            let s = store.clone();
            thread::spawn(move || client(&s, mode))
        })
        .collect();
    // Клиенты работают до убийства процесса; сюда попадаем только при ошибке.
    for c in clients {
        match c.join() {
            Ok(Err(e)) => eprintln!("crash_writer: {e}"),
            Ok(Ok(())) => {}
            Err(_) => eprintln!("crash_writer: поток-клиент завершился паникой"),
        }
    }
    ExitCode::FAILURE
}

/// Пишет события (или записи аудита) по одному и печатает `seq` каждого подтверждённого.
fn client(s: &ProjectStore, mode: Mode) -> Result<(), StoreError> {
    let run = RunId::new();
    for i in 0u64.. {
        if mode == Mode::Audit {
            let a = AuditNew {
                actor: "user".into(),
                action: "grant.added".into(),
                target: format!("/x/{i}"),
                payload: json!({ "i": i }),
            };
            let seq = block_on(s.append_audit(a))?.seq;
            print_seq(seq)?;
            continue;
        }
        let e = NewEvent {
            run_id: run,
            parent_seq: None,
            actor: Actor::Runtime,
            kind: EventKind::TurnStarted,
            payload: json!({ "i": i }),
            tainted: false,
        };
        let seq = if mode == Mode::Durable {
            block_on(s.append_durable(e))?
        } else {
            block_on(s.append(e))?
        };
        print_seq(seq.0)?;
    }
    Ok(())
}

/// Печатает подтверждённый `seq`: одна запись строки целиком и flush, тест читает
/// построчно.
fn print_seq(seq: u64) -> Result<(), StoreError> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{seq}")
        .and_then(|()| out.flush())
        .map_err(StoreError::from)
}

/// Будит поток, ждущий в [`block_on`].
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Минимальный исполнитель: ответы хранилища приходят через oneshot-каналы,
/// которым не нужен рантайм tokio.
fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        thread::park();
    }
}
