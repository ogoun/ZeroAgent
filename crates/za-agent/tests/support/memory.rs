//! Память-заглушка [`ScriptedMemory`] (R1-M задача 3): очередь ответов на каждый метод
//! порта и журнал вызовов. Пустая очередь — память ответила, но показать нечего
//! (`Recall::Empty`).

use std::collections::VecDeque;
use std::sync::Mutex;

use za_agent::{MemoryLevel, MemoryPort, NoteOutcome, Recall, RecallReply, SessionNote};
use za_llm::BoxFuture;
use za_protocol::RunId;

/// Вызов порта памяти, как его видит заглушка.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemCall {
    Snapshot {
        budget: u32,
    },
    Before {
        next_step: String,
    },
    After {
        budget: u32,
    },
    Prompt {
        text: String,
        budget: u32,
    },
    ToolError {
        tool: String,
        error: String,
        budget: u32,
    },
    Retry {
        budget: u32,
    },
    Note {
        trigger: &'static str,
    },
    Files {
        paths: Vec<String>,
    },
    End {
        ended: bool,
    },
}

/// Порт памяти по сценарию теста.
pub struct ScriptedMemory {
    level: MemoryLevel,
    snapshot: Mutex<VecDeque<RecallReply>>,
    prompt: Mutex<VecDeque<RecallReply>>,
    tool_error: Mutex<VecDeque<Recall>>,
    retry: Mutex<VecDeque<Recall>>,
    after: Mutex<VecDeque<Recall>>,
    calls: Mutex<Vec<MemCall>>,
}

impl ScriptedMemory {
    pub fn new(level: MemoryLevel) -> Self {
        Self {
            level,
            snapshot: Mutex::default(),
            prompt: Mutex::default(),
            tool_error: Mutex::default(),
            retry: Mutex::default(),
            after: Mutex::default(),
            calls: Mutex::default(),
        }
    }

    pub fn push_snapshot(&self, r: impl Into<RecallReply>) {
        self.snapshot.lock().unwrap().push_back(r.into());
    }

    pub fn push_prompt(&self, r: impl Into<RecallReply>) {
        self.prompt.lock().unwrap().push_back(r.into());
    }

    pub fn push_tool_error(&self, r: Recall) {
        self.tool_error.lock().unwrap().push_back(r);
    }

    pub fn push_retry(&self, r: Recall) {
        self.retry.lock().unwrap().push_back(r);
    }

    pub fn push_after(&self, r: Recall) {
        self.after.lock().unwrap().push_back(r);
    }

    /// Вызовы по порядку.
    pub fn calls(&self) -> Vec<MemCall> {
        self.calls.lock().unwrap().clone()
    }

    fn log(&self, c: MemCall) {
        self.calls.lock().unwrap().push(c);
    }
}

fn next<T: From<Recall>>(q: &Mutex<VecDeque<T>>) -> T {
    q.lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| Recall::Empty.into())
}

impl MemoryPort for ScriptedMemory {
    fn level(&self) -> MemoryLevel {
        self.level
    }

    fn snapshot(&self, _run: RunId, budget_tokens: u32) -> BoxFuture<'_, RecallReply> {
        self.log(MemCall::Snapshot {
            budget: budget_tokens,
        });
        let r = next(&self.snapshot);
        Box::pin(async move { r })
    }

    fn before_compaction<'a>(&'a self, _run: RunId, next_step: &'a str) -> BoxFuture<'a, ()> {
        self.log(MemCall::Before {
            next_step: next_step.into(),
        });
        Box::pin(async {})
    }

    fn after_compaction(&self, _run: RunId, budget_tokens: u32) -> BoxFuture<'_, Recall> {
        self.log(MemCall::After {
            budget: budget_tokens,
        });
        let r = next(&self.after);
        Box::pin(async move { r })
    }

    fn prompt_hint<'a>(
        &'a self,
        _run: RunId,
        text: &'a str,
        budget: u32,
    ) -> BoxFuture<'a, RecallReply> {
        self.log(MemCall::Prompt {
            text: text.into(),
            budget,
        });
        let r = next(&self.prompt);
        Box::pin(async move { r })
    }

    fn tool_error_hint<'a>(
        &'a self,
        _run: RunId,
        tool: &'a str,
        error: &'a str,
        budget: u32,
    ) -> BoxFuture<'a, Recall> {
        self.log(MemCall::ToolError {
            tool: tool.into(),
            error: error.into(),
            budget,
        });
        let r = next(&self.tool_error);
        Box::pin(async move { r })
    }

    fn retry_digest(&self, _run: RunId, budget: u32) -> BoxFuture<'_, Recall> {
        self.log(MemCall::Retry { budget });
        let r = next(&self.retry);
        Box::pin(async move { r })
    }

    fn session_note(&self, _run: RunId, note: SessionNote) -> BoxFuture<'_, NoteOutcome> {
        self.log(MemCall::Note {
            trigger: note.trigger,
        });
        Box::pin(async { NoteOutcome::default() })
    }

    fn files_changed(&self, _run: RunId, paths: Vec<String>) -> BoxFuture<'_, ()> {
        self.log(MemCall::Files { paths });
        Box::pin(async {})
    }

    fn end_session(&self, _run: RunId, ended: bool) -> BoxFuture<'_, ()> {
        self.log(MemCall::End { ended });
        Box::pin(async {})
    }
}
