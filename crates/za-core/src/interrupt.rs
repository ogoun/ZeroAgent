//! Прерывания run (CORE-007, D16): ожидание решения человека, ввода, внешнего
//! результата или таймера.
//!
//! Id выводится из ключа эффекта, на котором run остановился:
//! `{appr|input|ext|timer|unk}-{effect_key}`. Повтор после сбоя открывает то же
//! прерывание, а не новое. Переход `open → resolved` необратим, поэтому
//! [`state`] при реплее видит то же или более позднее решение.

use serde_json::Value;
use za_protocol::{EffectKey, RunId, pause_reason};
use za_store::{INTERRUPT_OPEN, INTERRUPT_RESOLVED, ProjectStore, StoreError};

/// Вид прерывания; [`InterruptKind::as_str`] — значение поля `kind` в журнале.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptKind {
    /// Вызов инструмента ждёт одобрения человека.
    Approval,
    /// Модель спросила пользователя (`ask_user`).
    Input,
    /// Ждём результата внешнего исполнителя.
    External,
    /// Ждём срабатывания таймера.
    Timer,
    /// Исход `NonIdempotent`-эффекта неизвестен после сбоя (CORE-003).
    UnknownOutcome,
}

impl InterruptKind {
    /// Значение `kind` в таблице и событиях прерываний.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Input => "input",
            Self::External => "external",
            Self::Timer => "timer",
            Self::UnknownOutcome => "unknown_outcome",
        }
    }

    /// Префикс id прерывания (D16).
    const fn id_prefix(self) -> &'static str {
        match self {
            Self::Approval => "appr",
            Self::Input => "input",
            Self::External => "ext",
            Self::Timer => "timer",
            Self::UnknownOutcome => "unk",
        }
    }
}

/// Прерывание, которое узел графа возвращает через `Transition::Suspend`.
#[derive(Debug, Clone, PartialEq)]
pub struct Interrupt {
    /// Детерминированный id (CORE-007).
    pub id: String,
    pub kind: InterruptKind,
    /// Данные для того, кто решает: вопрос, вызов на одобрение, ключ эффекта.
    pub payload: Value,
}

impl Interrupt {
    /// Прерывание на эффекте `key`: id `"{appr|input|ext|timer|unk}-{key}"` (D16).
    pub fn for_effect(kind: InterruptKind, key: &EffectKey, payload: Value) -> Self {
        Self {
            id: format!("{}-{}", kind.id_prefix(), key.0),
            kind,
            payload,
        }
    }
}

/// Состояние прерывания в журнале.
#[derive(Debug, Clone, PartialEq)]
pub enum InterruptState {
    /// Решения ещё нет.
    Open,
    /// Решено; значение — записанный ответ (`interrupts.response`).
    Resolved(Value),
}

/// Открывает прерывание run; повторное открытие того же id ничего не меняет:
/// действует первая запись. Если записанные вид или payload отличаются от `i`
/// (узел построил прерывание недетерминированно), пишется `warn`, ошибки нет.
/// Неизвестный run — [`StoreError::UnknownRun`].
pub async fn open(store: &ProjectStore, run: RunId, i: &Interrupt) -> Result<(), StoreError> {
    store
        .open_interrupt(&i.id, run, i.kind.as_str(), i.payload.clone())
        .await?;
    if let Some(row) = store.interrupt(&i.id).await?
        && (row.kind != i.kind.as_str() || row.payload.as_ref() != Some(&i.payload))
    {
        tracing::warn!(
            id = %i.id,
            recorded_kind = %row.kind,
            kind = i.kind.as_str(),
            "прерывание уже открыто с другим видом или payload, действует первая запись"
        );
    }
    Ok(())
}

/// Состояние прерывания по id; `None`, если его нет. Невозможные сочетания —
/// открытое с ответом, решённое без ответа, неизвестный статус —
/// [`StoreError::Corrupt`].
pub async fn state(store: &ProjectStore, id: &str) -> Result<Option<InterruptState>, StoreError> {
    let Some(row) = store.interrupt(id).await? else {
        return Ok(None);
    };
    match (row.status.as_str(), row.response) {
        (INTERRUPT_OPEN, None) => Ok(Some(InterruptState::Open)),
        (INTERRUPT_RESOLVED, Some(v)) => Ok(Some(InterruptState::Resolved(v))),
        (status, response) => Err(StoreError::Corrupt(format!(
            "прерывание {id}: статус {status}, ответ {}",
            if response.is_some() {
                "есть"
            } else {
                "нет"
            }
        ))),
    }
}

/// Причина паузы run, ждущего прерывания этого вида: `Approval` →
/// `needs_approval`, `Input` → `needs_input`, `UnknownOutcome` →
/// `needs_decision`, `External` и `Timer` → `waiting`.
pub fn pause_reason_for(kind: InterruptKind) -> &'static str {
    match kind {
        InterruptKind::Approval => pause_reason::NEEDS_APPROVAL,
        InterruptKind::Input => pause_reason::NEEDS_INPUT,
        InterruptKind::UnknownOutcome => pause_reason::NEEDS_DECISION,
        InterruptKind::External | InterruptKind::Timer => pause_reason::WAITING,
    }
}
