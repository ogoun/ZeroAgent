//! Управление run внутри процесса (D26): пауза и отмена — флаги `watch`, они не
//! теряются и повтор безвреден. Ввод и поправка (`input`, `correct`, `submit_input`)
//! добавляются к [`RunControl`] в задаче 14.

use std::fmt;

use za_protocol::RunId;
use za_store::ProjectStore;

use crate::turn::{SignalTx, Signals, signal_channel};

/// Принимающая сторона управления: её забирает цикл (`start_run`).
#[derive(Debug)]
pub struct ControlRx {
    pub(crate) sig: Signals,
}

impl ControlRx {
    pub fn signals(&mut self) -> &mut Signals {
        &mut self.sig
    }

    /// Снимает применённую паузу: следующая `RunControl::pause` в том же процессе
    /// снова останавливает run (resume, задача 15).
    pub fn clear_pause(&mut self) {
        self.sig.clear_pause();
    }
}

/// Управляющая сторона run; клонируется дёшево.
#[derive(Clone)]
pub struct RunControl {
    tx: SignalTx,
    store: ProjectStore,
    run: RunId,
}

impl fmt::Debug for RunControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunControl")
            .field("run", &self.run)
            .field("tx", &self.tx)
            .finish_non_exhaustive()
    }
}

impl RunControl {
    /// Пауза (D26): цикл применяет её на ближайшей границе, во время стрима или пакета.
    pub fn pause(&self) {
        self.tx.pause();
    }

    /// Отмена (D26): стрим и идущие инструменты бросаются, run — `cancelled`.
    pub fn cancel(&self) {
        self.tx.cancel();
    }

    /// Run, которым управляет.
    pub fn run_id(&self) -> RunId {
        self.run
    }

    /// Журнал проекта run (для ввода и поправок, задача 14).
    pub fn store(&self) -> &ProjectStore {
        &self.store
    }
}

/// Канал управления run.
pub fn control_channel(store: ProjectStore, run: RunId) -> (RunControl, ControlRx) {
    let (tx, sig) = signal_channel();
    (RunControl { tx, store, run }, ControlRx { sig })
}
