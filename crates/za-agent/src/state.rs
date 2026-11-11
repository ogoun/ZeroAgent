//! Состояние цикла в памяти и его снимок в журнале (D4, D5, D19).
//!
//! [`LoopState`] — то, над чем работают узлы графа цикла. [`Snapshot`] — его
//! сохраняемая часть: история хранится блобами по BLAKE3 и в снимке остаётся
//! списком хэшей (D4), поэтому снимок на каждой границе не растёт как O(n²).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use za_core::RunCtx;
use za_llm::ToolSchema;
use za_protocol::{BlobHash, Budget, BudgetKind, EffectClass, RunId, RunMode, RunStatus, Seq};

use crate::context::{ContextMeter, UserInput};
use crate::conversation::Conversation;
use crate::detect::DetectorState;
use crate::exit::LoopExit;
use crate::policy::{Grant, PolicySnapshot};
use crate::request::{FrozenPrefix, RunSetup};
use crate::tools::FinishArgs;
use crate::turn::{Reviewed, Signals};

/// Версия формата снимка: 3 — R1-C1 (D92: политика, доверие, гранты, метки). Снимки
/// версий 2 (R1-B2) и 1 (R1-B1) читаются: новые поля — по умолчанию (проект не доверен,
/// меток и грантов нет, политика пустая), у версии 1 `resume` восстанавливает поля R1-B2
/// разбором истории.
pub const SNAPSHOT_V: u32 = 3;

/// Голова цепочки аудита журнала проекта (R1-C1 D91, m1 ревью задачи 4): последняя
/// запись на момент старта или продолжения run. Хранится в снимке; бинарник при resume
/// сверяет, что запись `seq` с этим `hash` в цепочке есть, — удаление хвоста аудита
/// (например, `trust.revoked`) не проходит молча.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditHead {
    pub seq: u64,
    pub hash: String,
}

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
    /// Раздел `## Next step` последней сводки компакции (D50): `next_step` хука памяти
    /// перед следующей компакцией.
    #[serde(default)]
    pub last_next_step: String,
    /// Состояние детекторов повтора, цикла и застоя (D54–D56).
    #[serde(default)]
    pub detectors: DetectorState,
    /// Снимок политики run (R1-C1 D80, D92).
    #[serde(default)]
    pub policy: PolicySnapshot,
    /// Проект доверен (D83).
    #[serde(default)]
    pub trusted: bool,
    /// Пользователь отказал проекту в доверии (D83).
    #[serde(default)]
    pub trust_refused: bool,
    /// Гранты run (D78).
    #[serde(default)]
    pub grants: Vec<Grant>,
    /// Происхождения недоверенного содержимого (D82).
    #[serde(default)]
    pub taint: BTreeSet<String>,
    /// Голова цепочки аудита на момент старта или последнего продолжения run (D91);
    /// `None` — не задана (тесты без бинарника, снимки до задачи 13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_head: Option<AuditHead>,
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
    /// Скорость вывода последнего принятого хода, ток/с (`model.usage.output_tps`): дедлайн
    /// рассуждения D59 (Ruling I-2 ревью задачи 7). Снимок без поля — `None`.
    #[serde(default)]
    pub output_tps: Option<f64>,
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
    /// Шаг, начатый до resume без блока `<state>` (у него уже есть эффекты после
    /// `boundary`): блок — в конце границы этого шага, ключ после записанных эффектов
    /// (R1-C1, Ruling I1).
    pub state_late: Option<u64>,
    /// Политика resume со снимка `Model` (R1-C1 D92): с границы.
    pub policy: Option<PolicySnapshot>,
    /// Доверие resume со снимка `Model`: с границы.
    pub trusted: Option<bool>,
    /// Гранты resume со снимка `Model`: с границы.
    pub grants_add: Vec<Grant>,
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
    /// Попытка текущего хода (`turn_attempt` в `model.request` и `turn.finished`, D67,
    /// D69): 0 — первая. Ставит узел `model`; в снимок не входит — реплей шага со
    /// снимка `Model` проходит попытки заново по квитанциям.
    pub turn_attempt: u32,
    /// Задача run, как в первом сообщении (D47).
    pub task: String,
    /// Блок `<env>` первого сообщения (D46).
    pub env_text: String,
    /// Вводы пользователя по порядку; дописываются на шаге (6) границы (D66).
    pub user_inputs: Vec<UserInput>,
    /// Эпоха истории (D45, D46).
    pub epoch: u32,
    /// Раздел `## Next step` последней сводки компакции (D50).
    pub last_next_step: String,
    /// Состояние детекторов (D54–D56).
    pub detectors: DetectorState,
    /// Снимок политики run (R1-C1 D80, D92).
    pub policy: PolicySnapshot,
    /// Проект доверен (D83).
    pub trusted: bool,
    /// Пользователь отказал проекту в доверии (D83).
    pub trust_refused: bool,
    /// Гранты run (D78).
    pub grants: Vec<Grant>,
    /// Происхождения недоверенного содержимого run (D82).
    pub taint: BTreeSet<String>,
    /// Голова цепочки аудита (D91): из `LoopDeps::audit_head`, иначе из снимка.
    pub audit_head: Option<AuditHead>,
}

impl LoopState {
    /// Изменения политики, доверия и грантов (resume, D92). Доверие снимает метку
    /// `untrusted_project` (D82) и прежний отказ; отказ в доверии — `trust_refused`.
    /// Возвращает действительно добавленные гранты (для `grant.added`, D78).
    pub fn apply_policy_change(
        &mut self,
        policy: Option<PolicySnapshot>,
        trusted: Option<bool>,
        grants_add: Vec<Grant>,
    ) -> Vec<Grant> {
        if let Some(p) = policy {
            self.policy = p;
        }
        match trusted {
            Some(true) => self.trust_granted(),
            Some(false) => {
                self.trusted = false;
                self.trust_refused = true;
            }
            None => {}
        }
        let mut added = Vec::new();
        for g in grants_add {
            if !self.grants.contains(&g) {
                self.grants.push(g.clone());
                added.push(g);
            }
        }
        added
    }

    /// Доверие дано (D83): метка недоверенного проекта снимается.
    pub fn trust_granted(&mut self) {
        self.trusted = true;
        self.trust_refused = false;
        self.taint
            .remove(za_protocol::taint_origin::UNTRUSTED_PROJECT);
    }

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
            last_next_step: self.last_next_step.clone(),
            detectors: self.detectors.clone(),
            policy: self.policy.clone(),
            trusted: self.trusted,
            trust_refused: self.trust_refused,
            grants: self.grants.clone(),
            taint: self.taint.clone(),
            audit_head: self.audit_head.clone(),
        }
    }
}
