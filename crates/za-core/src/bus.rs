//! Шина событий run (D35) и журнал, публикующий в неё записанные события (D34).
//!
//! Шина — `tokio::sync::broadcast`: отстающий подписчик получает
//! `RecvError::Lagged` и теряет старые события; источник истины — журнал, по
//! которому подписчик дочитывает пропущенное (`read_run` после последнего `seq`).

use serde_json::{Value, json};
use tokio::sync::broadcast;
use za_protocol::{Actor, EventKind, NewEvent, RunId, RunStatus, Seq};
use za_store::{ProjectStore, StoreError, UsageRow};

/// Событие на шине; `seq` — номер в журнале, `None` у `run.status` (D34).
#[derive(Debug, Clone)]
pub struct AgentEvent {
    pub seq: Option<Seq>,
    pub event: NewEvent,
}

/// Шина событий run.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<AgentEvent>,
}

impl EventBus {
    /// Ёмкость шины цикла и бинарника (D35).
    pub const DEFAULT_CAPACITY: usize = 1024;

    /// Шина ёмкостью `capacity` событий; `0` считается `1` (`broadcast` не
    /// допускает пустой буфер).
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    /// Публикует событие; отсутствие подписчиков не ошибка.
    fn publish(&self, seq: Option<Seq>, event: NewEvent) {
        let _ = self.tx.send(AgentEvent { seq, event });
    }
}

/// Хранилище + шина: каждое записанное циклом событие публикуется после COMMIT.
#[derive(Clone)]
pub struct Journal {
    store: ProjectStore,
    bus: EventBus,
}

impl Journal {
    pub fn new(store: ProjectStore, bus: EventBus) -> Self {
        Self { store, bus }
    }

    pub fn store(&self) -> &ProjectStore {
        &self.store
    }

    /// Пишет событие и публикует его с назначенным `seq`. Ошибка записи — без публикации.
    pub async fn append(&self, e: NewEvent) -> Result<Seq, StoreError> {
        let seq = self.store.append(e.clone()).await?;
        self.bus.publish(Some(seq), e);
        Ok(seq)
    }

    /// set_status_with_reason + публикация run.status с seq None (D34: курсор — read_run по последнему seq).
    ///
    /// Payload опубликованного события — `{to, generation[, reason]}`: прежний
    /// статус (`from`) есть только в событии журнала.
    pub async fn set_status(
        &self,
        run: RunId,
        to: RunStatus,
        generation: u64,
        reason: Option<&str>,
    ) -> Result<u64, StoreError> {
        let next = self
            .store
            .set_status_with_reason(run, to, generation, reason)
            .await?;
        let mut payload = json!({ "to": to, "generation": next });
        if let Some(reason) = reason {
            payload["reason"] = Value::String(reason.to_owned());
        }
        self.bus.publish(
            None,
            NewEvent {
                run_id: run,
                parent_seq: None,
                actor: Actor::Runtime,
                kind: EventKind::RunStatus,
                payload,
                tainted: false,
            },
        );
        Ok(next)
    }

    /// [`ProjectStore::record_usage`] + публикация `model.usage` с `seq`.
    pub async fn record_usage(
        &self,
        run: RunId,
        row: UsageRow,
        payload: Value,
    ) -> Result<Seq, StoreError> {
        let seq = self.store.record_usage(run, row, payload.clone()).await?;
        self.bus.publish(
            Some(seq),
            NewEvent {
                run_id: run,
                parent_seq: None,
                actor: Actor::Model,
                kind: EventKind::ModelUsage,
                payload,
                tainted: false,
            },
        );
        Ok(seq)
    }
}
