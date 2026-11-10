//! Управление run внутри процесса (D26): пауза и отмена — флаги `watch`, они не
//! теряются и повтор безвреден. Ввод пользователя (D25) и поправка (D36) пишутся в
//! журнал (`turn.input`, `control.requested`) и попадают в цикл только через эффект
//! `boundary` (D9); ввод `interrupt` дополнительно будит цикл сигналом.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use za_protocol::{Actor, EventKind, NewEvent, RunId, Seq};
use za_store::{ProjectStore, StoreError};

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

/// Режим ввода пользователя (D25, CORE-103).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    /// Сообщение берёт ближайшая граница шага.
    Queue,
    /// Сигнал обрывает текущий стрим модели; не начатые вызовы пакета не начинаются.
    Interrupt,
}

/// Длина идентификатора команды в hex-символах (D36).
const COMMAND_HEX: usize = 16;

/// Пишет `turn.input {text, mode}` (actor user, D8). Сигнала не шлёт: его шлёт
/// [`RunControl::input`] при [`InputMode::Interrupt`].
pub async fn submit_input(
    store: &ProjectStore,
    run: RunId,
    text: &str,
    mode: InputMode,
) -> Result<Seq, StoreError> {
    store
        .append(NewEvent {
            run_id: run,
            parent_seq: None,
            actor: Actor::User,
            kind: EventKind::TurnInput,
            payload: json!({ "text": text, "mode": mode }),
            tainted: false,
        })
        .await
}

/// Идентификатор команды (D36): первые 16 hex `blake3(run ‖ Uuid::now_v7())`.
fn command_id(run: RunId) -> String {
    let mut h = blake3::Hasher::new();
    h.update(run.0.as_bytes());
    h.update(Uuid::now_v7().as_bytes());
    let mut hex = h.finalize().to_hex().to_string();
    hex.truncate(COMMAND_HEX);
    hex
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

    /// Ввод пользователя (D25): `turn.input` в журнал; при [`InputMode::Interrupt`] —
    /// ещё и сигнал циклу (после записи: граница, которую он вызовет, уже видит ввод).
    pub async fn input(&self, text: &str, mode: InputMode) -> Result<Seq, StoreError> {
        let seq = submit_input(&self.store, self.run, text, mode).await?;
        if mode == InputMode::Interrupt {
            self.tx.input();
        }
        Ok(seq)
    }

    /// Поправка (D23, D36): `control.requested {action: "correct", note, command}`
    /// (actor user). `command` генерируется до записи и возвращается вызывающему;
    /// цикл применяет поправку на ближайшей границе и пишет `control.applied` с тем же
    /// `command`.
    pub async fn correct(&self, note: &str) -> Result<(Seq, String), StoreError> {
        let command = command_id(self.run);
        let seq = self
            .store
            .append(NewEvent {
                run_id: self.run,
                parent_seq: None,
                actor: Actor::User,
                kind: EventKind::ControlRequested,
                payload: json!({ "action": "correct", "note": note, "command": command }),
                tainted: false,
            })
            .await?;
        Ok((seq, command))
    }

    /// Run, которым управляет.
    pub fn run_id(&self) -> RunId {
        self.run
    }

    /// Журнал проекта run.
    pub fn store(&self) -> &ProjectStore {
        &self.store
    }
}

/// Канал управления run.
pub fn control_channel(store: ProjectStore, run: RunId) -> (RunControl, ControlRx) {
    let (tx, sig) = signal_channel();
    (RunControl { tx, store, run }, ControlRx { sig })
}
