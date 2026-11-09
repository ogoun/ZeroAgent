//! История диалога run: только дополняется, ход модели фиксируется атомарно (D10).
//!
//! Инварианты (их проверяет каждая операция добавления и [`Conversation::validate`]):
//! - первое сообщение — от пользователя;
//! - ход модели — это служебные `Role::System` (ноль или больше) и сразу за ними ответ
//!   ассистента, одной операцией [`Conversation::commit_turn`]. Сорвавшийся ход не
//!   оставляет в истории ничего, поэтому последовательности `user, system, user`
//!   (400 у Anthropic, Carry 400 (а)) не бывает;
//! - служебное system-сообщение стоит после `user` или результатов инструментов (для
//!   Anthropic это `user`-сообщение с `tool_result`): Anthropic принимает system посреди
//!   истории только сразу после `user`;
//! - каждый результат отвечает ждущему вызову последнего ответа ассистента, один раз;
//!   новое сообщение пользователя и новый ход не принимаются, пока есть ждущие вызовы.
//!
//! Изменять уже добавленные сообщения API не даёт: preserved thinking Anthropic требует
//! пересылать прежние сообщения, включая погашенные system (`clear_at`), без изменений.
//! Опускание system для моделей без mid-system — дело коннектора при рендере.
//!
//! Каждое сообщение хранится один раз блобом (канонический JSON) по BLAKE3, снимок держит
//! только список хэшей (D4): [`Conversation::persist`] и [`Conversation::load`].

use std::collections::HashSet;

use za_llm::{Message, Part, Role};
use za_protocol::{BlobHash, canonical_json};
use za_store::{ProjectStore, StoreError};

/// Содержимое синтетического результата прерванного вызова (CORE-105).
const INTERRUPTED: &str = "interrupted: call not executed";

/// Нарушение инвариантов истории.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum HistoryError {
    #[error("первое сообщение не от пользователя")]
    FirstNotUser,
    #[error("служебное system-сообщение не перед ответом ассистента")]
    SystemNotBeforeAssistant,
    #[error("служебное system-сообщение не после сообщения пользователя или результатов")]
    SystemNotAfterUser,
    #[error("ход модели не заканчивается ответом ассистента")]
    TurnNotAssistant,
    #[error("в сообщении результатов не результат инструмента")]
    NotToolResult,
    #[error("результат без вызова {0}")]
    UnknownCallId(String),
    #[error("повторный результат вызова {0}")]
    DuplicateResult(String),
    #[error("вызовы без результатов: {0:?}")]
    PendingCalls(Vec<String>),
    #[error("блоб истории {0} не найден")]
    MissingBlob(String),
}

/// Итог [`Conversation::persist`].
#[derive(Debug, Clone, PartialEq)]
pub struct Persisted {
    /// Хэши блобов всех сообщений по порядку.
    pub refs: Vec<BlobHash>,
    /// Сколько сообщений записано этим вызовом.
    pub written: usize,
}

/// Ошибка загрузки истории из блобов.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    History(#[from] HistoryError),
}

/// История диалога run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Conversation {
    msgs: Vec<Message>,
    /// Хэш блоба каждого сообщения; `None` — ещё не сохранено.
    refs: Vec<Option<BlobHash>>,
}

impl Conversation {
    /// Сообщения по порядку.
    pub fn messages(&self) -> &[Message] {
        &self.msgs
    }

    /// Сообщение пользователя; `PendingCalls`, если есть ждущие вызовы.
    pub fn push_user(&mut self, text: String) -> Result<(), HistoryError> {
        self.push_user_message(Message {
            role: Role::User,
            parts: vec![Part::Text(text)],
        })
    }

    /// Ход атомарно: service (только Role::System), затем assistant (D10).
    ///
    /// При ошибке история не меняется.
    pub fn commit_turn(
        &mut self,
        service: Vec<Message>,
        assistant: Message,
    ) -> Result<(), HistoryError> {
        if assistant.role != Role::Assistant {
            return Err(HistoryError::TurnNotAssistant);
        }
        if service.iter().any(|m| m.role != Role::System) {
            return Err(HistoryError::SystemNotBeforeAssistant);
        }
        let Some(last) = self.msgs.last() else {
            return Err(HistoryError::FirstNotUser);
        };
        let pending = self.pending_calls();
        if !pending.is_empty() {
            return Err(HistoryError::PendingCalls(pending));
        }
        if !service.is_empty() && !matches!(last.role, Role::User | Role::Tool) {
            return Err(HistoryError::SystemNotAfterUser);
        }
        for m in service {
            self.append(m);
        }
        self.append(assistant);
        Ok(())
    }

    /// Сообщение Role::Tool с результатами; каждый call_id — ждущий вызов последнего ответа.
    ///
    /// Пустой список ничего не добавляет. При ошибке история не меняется.
    pub fn push_results(&mut self, results: Vec<Part>) -> Result<(), HistoryError> {
        let (calls, mut answered) = self.open_calls();
        for p in &results {
            let Part::ToolResult { call_id, .. } = p else {
                return Err(HistoryError::NotToolResult);
            };
            if !calls.contains(&call_id.as_str()) {
                return Err(HistoryError::UnknownCallId(call_id.clone()));
            }
            if !answered.insert(call_id.clone()) {
                return Err(HistoryError::DuplicateResult(call_id.clone()));
            }
        }
        if !results.is_empty() {
            self.append(Message {
                role: Role::Tool,
                parts: results,
            });
        }
        Ok(())
    }

    /// Вызовы последнего ответа ассистента без результатов, в порядке вызовов.
    pub fn pending_calls(&self) -> Vec<String> {
        let (calls, answered) = self.open_calls();
        calls
            .into_iter()
            .filter(|id| !answered.contains(*id))
            .map(str::to_owned)
            .collect()
    }

    /// CORE-105: результат-ошибка "interrupted: call not executed" каждому ждущему вызову; число.
    pub fn close_interrupted(&mut self) -> usize {
        let pending = self.pending_calls();
        let n = pending.len();
        if n > 0 {
            tracing::debug!(n, "история: ждущие вызовы закрыты как прерванные");
            // Второе подряд сообщение `Role::Tool` после частичных результатов допустимо:
            // Anthropic сливает соседние сообщения одной роли, Chat и Responses шлют
            // результаты по одному.
            self.append(Message {
                role: Role::Tool,
                parts: pending
                    .into_iter()
                    .map(|call_id| Part::ToolResult {
                        call_id,
                        content: INTERRUPTED.to_owned(),
                        is_error: true,
                    })
                    .collect(),
            });
        }
        n
    }

    /// Проверка инвариантов всей истории: повтор операций добавления с начала.
    ///
    /// Ждущие вызовы в конце допустимы (история посреди пакета инструментов).
    pub fn validate(&self) -> Result<(), HistoryError> {
        let mut shadow = Conversation::default();
        let mut i = 0;
        while i < self.msgs.len() {
            let m = &self.msgs[i];
            if i == 0 && m.role != Role::User {
                return Err(HistoryError::FirstNotUser);
            }
            match m.role {
                Role::User => shadow.push_user_message(m.clone())?,
                Role::Tool => shadow.push_results(m.parts.clone())?,
                Role::Assistant | Role::System => {
                    let start = i;
                    while self.msgs.get(i).is_some_and(|m| m.role == Role::System) {
                        i += 1;
                    }
                    let Some(assistant) = self.msgs.get(i).filter(|m| m.role == Role::Assistant)
                    else {
                        return Err(HistoryError::SystemNotBeforeAssistant);
                    };
                    shadow.commit_turn(self.msgs[start..i].to_vec(), assistant.clone())?;
                }
            }
            i += 1;
        }
        Ok(())
    }

    /// put_blob для ещё не сохранённых сообщений (канонический JSON); хэши всех по порядку и число
    /// записанных сейчас (сообщений без ссылки до вызова).
    ///
    /// Сбой посреди записи оставляет уже записанные ссылки: повтор дописывает остальное.
    pub async fn persist(&mut self, store: &ProjectStore) -> Result<Persisted, StoreError> {
        let mut written = 0;
        for (m, r) in self.msgs.iter().zip(self.refs.iter_mut()) {
            if r.is_some() {
                continue;
            }
            let v = serde_json::to_value(m).map_err(|e| StoreError::Serde(e.to_string()))?;
            *r = Some(store.put_blob(canonical_json(&v).as_bytes()).await?);
            written += 1;
        }
        let refs = self.refs.iter().flatten().cloned().collect();
        Ok(Persisted { refs, written })
    }

    /// История из блобов по списку хэшей снимка; инварианты проверяются.
    pub async fn load(store: &ProjectStore, refs: &[BlobHash]) -> Result<Conversation, LoadError> {
        let mut c = Conversation::default();
        for h in refs {
            let raw = store
                .get_blob(h)
                .await?
                .ok_or_else(|| HistoryError::MissingBlob(h.0.clone()))?;
            let m: Message = serde_json::from_slice(&raw)
                .map_err(|e| StoreError::Corrupt(format!("блоб истории {}: {e}", h.0)))?;
            c.msgs.push(m);
            c.refs.push(Some(h.clone()));
        }
        c.validate()?;
        Ok(c)
    }

    /// Сообщение пользователя как есть (общий путь `push_user` и `validate`).
    fn push_user_message(&mut self, m: Message) -> Result<(), HistoryError> {
        let pending = self.pending_calls();
        if !pending.is_empty() {
            return Err(HistoryError::PendingCalls(pending));
        }
        self.append(m);
        Ok(())
    }

    /// Вызовы последнего ответа ассистента и уже полученные на них результаты. После
    /// сообщения пользователя открытых вызовов нет.
    fn open_calls(&self) -> (Vec<&str>, HashSet<String>) {
        let Some(at) = self
            .msgs
            .iter()
            .rposition(|m| matches!(m.role, Role::Assistant | Role::User))
        else {
            return (Vec::new(), HashSet::new());
        };
        if self.msgs[at].role != Role::Assistant {
            return (Vec::new(), HashSet::new());
        }
        let calls = self.msgs[at]
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::ToolCall(c) => Some(c.id.as_str()),
                _ => None,
            })
            .collect();
        let answered = self.msgs[at + 1..]
            .iter()
            .flat_map(|m| &m.parts)
            .filter_map(|p| match p {
                Part::ToolResult { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        (calls, answered)
    }

    fn append(&mut self, m: Message) {
        self.msgs.push(m);
        self.refs.push(None);
    }
}
