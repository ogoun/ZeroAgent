//! Состояние цикла в памяти и его снимок в журнале (D4, D5, D19).
//!
//! [`LoopState`] — то, над чем работают узлы графа цикла. [`Snapshot`] — его
//! сохраняемая часть: история хранится блобами по BLAKE3 и в снимке остаётся
//! списком хэшей (D4), поэтому снимок на каждой границе не растёт как O(n²).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use za_core::RunCtx;
use za_llm::ToolSchema;
use za_protocol::{BlobHash, Budget, BudgetKind, EffectClass, RunId, RunMode, RunStatus, Seq};

use crate::context::{ContextMeter, UserInput};
use crate::conversation::Conversation;
use crate::exit::LoopExit;
use crate::request::{FrozenPrefix, RunSetup};
use crate::tools::FinishArgs;
use crate::turn::{Reviewed, Signals};

/// Версия формата снимка: 2 — R1-B2 (D66). Снимок версии 1 (R1-B1) читается: новые
/// поля — по умолчанию, `resume` восстанавливает их разбором истории.
pub const SNAPSHOT_V: u32 = 2;

/// С чего resume продолжает run (D4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Next {
    /// Снимок посреди шага: resume реплеит шаг с узла `model`.
    Model,
    /// Шаг завершён: resume начинает новую границу шагом `step + 1`.
    Boundary,
}

/// Отложенный выход чистой паузы (D5): resume статуса running дописывает его, если шаг step + 1 не начат.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingExit {
    Paused { reason: String },
    BlockedOnBudget { budget: BudgetKind },
}

/// Снимок run (D4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub v: u32,
    pub step: u64,
    pub effect_seq: u64,
    pub next: Next,
    #[serde(default)]
    pub exit: Option<PendingExit>,
    pub turn: u32,
    pub setup: RunSetup,
    pub frozen: FrozenPrefix,
    pub frozen_ref: BlobHash,
    pub messages: Vec<BlobHash>,
    pub inbox_after: u64,
    pub budget: Budget,
    pub counters: Counters,
    /// Счётчик контекста (D41, D43).
    #[serde(default)]
    pub meter: ContextMeter,
    /// Задача run, как в первом сообщении (D47).
    #[serde(default)]
    pub task: String,
    /// Блок `<env>` первого сообщения (D46).
    #[serde(default)]
    pub env_text: String,
    /// Вводы пользователя по порядку (D47, D48).
    #[serde(default)]
    pub user_inputs: Vec<UserInput>,
    /// Эпоха истории: растёт при каждом переписывании — удалении старых результатов
    /// (D45) и компакции (D46).
    #[serde(default)]
    pub epoch: u32,
}

/// Счётчики цикла, переживающие resume.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Counters {
    /// Пустых ходов подряд (D18).
    pub empty_turns: u32,
    /// Начатых ходов модели (D19, бюджет `steps`).
    pub steps: u32,
    /// Следующий запрос — с `tool_choice = Required` (D18, после второго пустого хода).
    pub forced_required: bool,
    /// D19: активное время run, мс; last_boundary_ms — now_ms прошлой границы в этом процессе (resume обнуляет).
    pub active_ms: u64,
    pub last_boundary_ms: Option<u64>,
}

impl Counters {
    /// D19: active_ms += now_ms − last_boundary_ms (если есть); last_boundary_ms = Some(now_ms).
    ///
    /// Часы назад (now_ms меньше прошлой границы) дают прибавку 0.
    pub fn tick(&mut self, now_ms: u64) {
        if let Some(last) = self.last_boundary_ms {
            self.active_ms = self.active_ms.saturating_add(now_ms.saturating_sub(last));
        }
        self.last_boundary_ms = Some(now_ms);
    }
}

/// Отложенное resume до ближайшей границы шага (задача 15). Не входит в снимок: после
/// сбоя до этой границы resume с прежнего снимка получает изменения заново.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AtBoundary {
    /// Бюджет resume со снимка `Model`: до конца шага не применяется (D6).
    pub budget: Option<Budget>,
    /// Режим resume со снимка `Model`: до конца шага не применяется (D6).
    pub run_mode: Option<RunMode>,
    /// После ввода — управляющее сообщение `resume` с блоком `<state>` (эффект `state`).
    pub resume_note: bool,
}

/// Состояние цикла в памяти; узлы графа работают над ним (S в Graph<S, LoopError>).
pub struct LoopState {
    pub run: RunId,
    pub ctx: RunCtx,
    pub conv: Conversation,
    /// Хэши блобов истории после последнего `persist`.
    pub refs: Vec<BlobHash>,
    pub frozen: FrozenPrefix,
    pub frozen_ref: BlobHash,
    pub setup: RunSetup,
    pub budget: Budget,
    /// turn — номер последнего начатого хода модели, первый ход — 1 (события D8, --crash-at after-delta:N).
    pub counters: Counters,
    pub turn: u32,
    /// `seq`, до которого граница уже прочитала ввод (D9).
    pub inbox_after: u64,
    /// seq последнего события, записанного драйвером: seq снимка чистой паузы (D5).
    pub last_seq: Seq,
    /// Поколение статуса для set_status (expected_generation).
    pub generation: u64,
    /// Описания инструментов run (как в префиксе).
    pub tools: Vec<ToolSchema>,
    /// Класс эффекта по имени инструмента (для [`crate::review`]).
    pub classes: BTreeMap<String, EffectClass>,
    /// Итог узла model для узла tools.
    pub reviewed: Option<Reviewed>,
    pub sig: Signals,
    pub answer: Option<String>,
    pub finish: Option<FinishArgs>,
    /// Выход, положенный узлом (D31).
    pub exit: Option<LoopExit>,
    /// `Pausing` или `Cancelling`, уже записанный драйвером в момент наблюдения флага
    /// (D26); `None` — run в `running`. Не входит в снимок: resume читает статус run.
    pub stopping: Option<RunStatus>,
    /// Изменения resume, ждущие ближайшей границы шага.
    pub at_boundary: AtBoundary,
    /// Счётчик контекста (D41, D43).
    pub meter: ContextMeter,
    /// Идёт шаг R1-B1, восстановленный из снимка версии 1 посреди шага: ход реплеится с
    /// параметрами R1-B1 (D66). Снимается на ближайшей границе; в снимок не входит.
    pub legacy_step: bool,
    /// Задача run, как в первом сообщении (D47).
    pub task: String,
    /// Блок `<env>` первого сообщения (D46).
    pub env_text: String,
    /// Вводы пользователя по порядку; дописываются на шаге (6) границы (D66).
    pub user_inputs: Vec<UserInput>,
    /// Эпоха истории (D45, D46).
    pub epoch: u32,
}

impl LoopState {
    /// Снимок текущего состояния; v = [`SNAPSHOT_V`]. История — `refs` последнего `persist`.
    pub fn snapshot(&self, next: Next, exit: Option<PendingExit>) -> Snapshot {
        Snapshot {
            v: SNAPSHOT_V,
            step: self.ctx.step(),
            effect_seq: self.ctx.effect_seq(),
            next,
            exit,
            turn: self.turn,
            setup: self.setup.clone(),
            frozen: self.frozen.clone(),
            frozen_ref: self.frozen_ref.clone(),
            messages: self.refs.clone(),
            inbox_after: self.inbox_after,
            budget: self.budget.clone(),
            counters: self.counters.clone(),
            meter: self.meter.clone(),
            task: self.task.clone(),
            env_text: self.env_text.clone(),
            user_inputs: self.user_inputs.clone(),
            epoch: self.epoch,
        }
    }
}
