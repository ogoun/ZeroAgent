//! Тестовый писатель для проверки устойчивости журнала к `kill -9`
//! (`tests/crash.rs`). Не для использования вне тестов.
//!
//! `crash_writer <dir> [--durable]`: открывает журнал в `<dir>` и бесконечно
//! пишет события из нескольких потоков (чтобы убийство попадало и в середину
//! group commit). После подтверждения каждого события печатает его `seq`
//! отдельной строкой и сразу сбрасывает stdout. С `--durable` пишет через
//! `append_durable`.

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
use za_store::{ProjectStore, StoreError};

/// Число параллельных потоков-клиентов.
const CLIENTS: usize = 4;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(dir) = args.next().map(PathBuf::from) else {
        eprintln!("использование: crash_writer <dir> [--durable]");
        return ExitCode::from(2);
    };
    let durable = args.next().is_some_and(|a| a == "--durable");
    let store = match ProjectStore::open(&dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("crash_writer: {e}");
            return ExitCode::FAILURE;
        }
    };
    let clients: Vec<_> = (0..CLIENTS)
        .map(|_| {
            let s = store.clone();
            thread::spawn(move || client(&s, durable))
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

/// Пишет события по одному и печатает `seq` каждого подтверждённого.
fn client(s: &ProjectStore, durable: bool) -> Result<(), StoreError> {
    let run = RunId::new();
    for i in 0u64.. {
        let e = NewEvent {
            run_id: run,
            parent_seq: None,
            actor: Actor::Runtime,
            kind: EventKind::TurnStarted,
            payload: json!({ "i": i }),
            tainted: false,
        };
        let seq = if durable {
            block_on(s.append_durable(e))?
        } else {
            block_on(s.append(e))?
        };
        // Одна запись строки целиком и flush: тест читает построчно.
        let mut out = std::io::stdout().lock();
        writeln!(out, "{}", seq.0)
            .and_then(|()| out.flush())
            .map_err(StoreError::from)?;
    }
    Ok(())
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
