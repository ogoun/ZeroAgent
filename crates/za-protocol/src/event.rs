//! События журнала.

use crate::ids::{BlobHash, EventId, RunId, Seq};
use jiff::Timestamp;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Версия схемы событий.
pub const SCHEMA_V: u16 = 1;

/// Кто породил событие.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    User,
    Runtime,
    Model,
    Tool,
    Peer,
}

/// Тип события; неизвестные типы сохраняются как `Unknown`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EventKind {
    RunCreated,
    RunStatus,
    TurnStarted,
    ModelRequest,
    ModelUsage,
    ToolStarted,
    ToolFinished,
    EffectIntent,
    EffectReceipt,
    InterruptOpened,
    InterruptResolved,
    /// Склеенный фрагмент стрима модели (D8 R1-B1).
    ModelDelta,
    /// Состояние ожидания модели: очередь, ресурс, генерация.
    AgentHeartbeat,
    /// Ввод пользователя в run (actor `user`).
    TurnInput,
    /// Итог хода модели.
    TurnFinished,
    /// Запрошена управляющая команда, применяется на границе шага.
    ControlRequested,
    /// Управляющая команда применена.
    ControlApplied,
    /// Превышен бюджет run.
    BudgetExceeded,
    /// Память недоступна (уровень L0): один раз за run, на шаге 0 (N-16).
    MemoryUnavailable,
    /// Предупреждение о контексте модели (R1-B2 D43, D62, D70): `kind` — причина.
    ContextAlert,
    /// Старые результаты инструментов заменены ссылками на spill (R1-B2 D45).
    ContextPruned,
    /// История сжата сводкой (R1-B2 D46); событие помечено `tainted`.
    ContextCompacted,
    /// Сработал детектор зацикливания (R1-B2 D54–D61).
    LoopDetected,
    /// Исполненный вызов обратился к путям вне проекта (R1-C1 D77).
    BoundaryOutside,
    /// Выдан грант доступа вне проекта (R1-C1 D78).
    GrantAdded,
    /// Перед вызовом `Edit` или `Exec` сделан чекпоинт (R1-C1 D89).
    CheckpointCreated,
    /// Инструмент изменил файл проекта.
    FileChanged,
    /// Из ввода удалены невидимые символы (R1-C2 D102): `source`, `removed`.
    InputCleaned,
    /// Обращение сетевого шлюза (R1-C2 D112): `url`, `outcome`.
    WebRequest,
    /// Изменилась экспозиция Rule of Two (R1-C2 D106).
    ExposureChanged,
    /// Каталог скиллов run собран (R1-C2): найденные и пропущенные.
    SkillCatalog,
    /// Скилл загружен в контекст (R1-C2).
    SkillActivated,
    Unknown(String),
}

impl EventKind {
    /// Точечное имя типа.
    pub fn as_str(&self) -> &str {
        match self {
            Self::RunCreated => "run.created",
            Self::RunStatus => "run.status",
            Self::TurnStarted => "turn.started",
            Self::ModelRequest => "model.request",
            Self::ModelUsage => "model.usage",
            Self::ToolStarted => "tool.started",
            Self::ToolFinished => "tool.finished",
            Self::EffectIntent => "effect.intent",
            Self::EffectReceipt => "effect.receipt",
            Self::InterruptOpened => "interrupt.opened",
            Self::InterruptResolved => "interrupt.resolved",
            Self::ModelDelta => "model.delta",
            Self::AgentHeartbeat => "agent.heartbeat",
            Self::TurnInput => "turn.input",
            Self::TurnFinished => "turn.finished",
            Self::ControlRequested => "control.requested",
            Self::ControlApplied => "control.applied",
            Self::BudgetExceeded => "budget.exceeded",
            Self::MemoryUnavailable => "memory.unavailable",
            Self::ContextAlert => "context.alert",
            Self::ContextPruned => "context.pruned",
            Self::ContextCompacted => "context.compacted",
            Self::LoopDetected => "loop.detected",
            Self::BoundaryOutside => "boundary.outside",
            Self::GrantAdded => "grant.added",
            Self::CheckpointCreated => "checkpoint.created",
            Self::FileChanged => "file.changed",
            Self::InputCleaned => "input.cleaned",
            Self::WebRequest => "web.request",
            Self::ExposureChanged => "exposure.changed",
            Self::SkillCatalog => "skill.catalog",
            Self::SkillActivated => "skill.activated",
            Self::Unknown(s) => s,
        }
    }

    /// Разбор имени; незнакомое имя даёт `Unknown`.
    pub fn parse(s: &str) -> Self {
        match s {
            "run.created" => Self::RunCreated,
            "run.status" => Self::RunStatus,
            "turn.started" => Self::TurnStarted,
            "model.request" => Self::ModelRequest,
            "model.usage" => Self::ModelUsage,
            "tool.started" => Self::ToolStarted,
            "tool.finished" => Self::ToolFinished,
            "effect.intent" => Self::EffectIntent,
            "effect.receipt" => Self::EffectReceipt,
            "interrupt.opened" => Self::InterruptOpened,
            "interrupt.resolved" => Self::InterruptResolved,
            "model.delta" => Self::ModelDelta,
            "agent.heartbeat" => Self::AgentHeartbeat,
            "turn.input" => Self::TurnInput,
            "turn.finished" => Self::TurnFinished,
            "control.requested" => Self::ControlRequested,
            "control.applied" => Self::ControlApplied,
            "budget.exceeded" => Self::BudgetExceeded,
            "memory.unavailable" => Self::MemoryUnavailable,
            "context.alert" => Self::ContextAlert,
            "context.pruned" => Self::ContextPruned,
            "context.compacted" => Self::ContextCompacted,
            "loop.detected" => Self::LoopDetected,
            "boundary.outside" => Self::BoundaryOutside,
            "grant.added" => Self::GrantAdded,
            "checkpoint.created" => Self::CheckpointCreated,
            "file.changed" => Self::FileChanged,
            "input.cleaned" => Self::InputCleaned,
            "web.request" => Self::WebRequest,
            "exposure.changed" => Self::ExposureChanged,
            "skill.catalog" => Self::SkillCatalog,
            "skill.activated" => Self::SkillActivated,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

impl Serialize for EventKind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EventKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Self::parse(&s))
    }
}

/// Событие до записи в журнал (seq, id, ts назначает журнал).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewEvent {
    pub run_id: RunId,
    pub parent_seq: Option<Seq>,
    pub actor: Actor,
    pub kind: EventKind,
    pub payload: Value,
    pub tainted: bool,
}

/// Записанное событие журнала.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: Seq,
    pub id: EventId,
    pub run_id: RunId,
    pub parent_seq: Option<Seq>,
    pub ts: Timestamp,
    pub actor: Actor,
    pub kind: EventKind,
    pub schema_v: u16,
    pub payload: Value,
    pub blob_ref: Option<BlobHash>,
    pub tainted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_kind_names_are_dotted_and_unknown_survives() {
        assert_eq!(EventKind::ToolStarted.as_str(), "tool.started");
        assert_eq!(
            EventKind::parse("future.thing"),
            EventKind::Unknown("future.thing".into())
        );
    }

    #[test]
    fn known_kinds_round_trip_through_names() {
        for k in [
            EventKind::RunCreated,
            EventKind::RunStatus,
            EventKind::TurnStarted,
            EventKind::ModelRequest,
            EventKind::ModelUsage,
            EventKind::ToolStarted,
            EventKind::ToolFinished,
            EventKind::EffectIntent,
            EventKind::EffectReceipt,
            EventKind::InterruptOpened,
            EventKind::InterruptResolved,
        ] {
            assert_eq!(EventKind::parse(k.as_str()), k);
        }
    }

    #[test]
    fn new_kinds_round_trip() {
        use EventKind::*;
        for k in [
            ModelDelta,
            AgentHeartbeat,
            TurnInput,
            TurnFinished,
            ControlRequested,
            ControlApplied,
            BudgetExceeded,
            MemoryUnavailable,
            ContextAlert,
            ContextPruned,
            ContextCompacted,
            LoopDetected,
        ] {
            assert_eq!(EventKind::parse(k.as_str()), k);
        }
        assert_eq!(EventKind::AgentHeartbeat.as_str(), "agent.heartbeat");
        assert_eq!(EventKind::ContextAlert.as_str(), "context.alert");
        assert_eq!(EventKind::LoopDetected.as_str(), "loop.detected");
    }

    #[test]
    fn r1c1_kinds_round_trip() {
        use EventKind::*;
        for (k, name) in [
            (BoundaryOutside, "boundary.outside"),
            (GrantAdded, "grant.added"),
            (CheckpointCreated, "checkpoint.created"),
            (FileChanged, "file.changed"),
        ] {
            assert_eq!(k.as_str(), name);
            assert_eq!(EventKind::parse(name), k);
        }
    }

    #[test]
    fn r1c2_kinds_round_trip() {
        use EventKind::*;
        for (k, name) in [
            (InputCleaned, "input.cleaned"),
            (WebRequest, "web.request"),
            (ExposureChanged, "exposure.changed"),
            (SkillCatalog, "skill.catalog"),
            (SkillActivated, "skill.activated"),
        ] {
            assert_eq!(k.as_str(), name);
            assert_eq!(EventKind::parse(name), k);
        }
    }

    #[test]
    fn memory_unavailable_kind() {
        assert_eq!(EventKind::MemoryUnavailable.as_str(), "memory.unavailable");
        assert_eq!(
            EventKind::parse("memory.unavailable"),
            EventKind::MemoryUnavailable
        );
    }

    #[test]
    fn event_serde_round_trip() {
        let e = Event {
            seq: Seq(7),
            id: EventId::new(),
            run_id: RunId::new(),
            parent_seq: Some(Seq(3)),
            ts: "2026-11-08T10:00:00Z".parse().unwrap(),
            actor: Actor::Runtime,
            kind: EventKind::Unknown("future.thing".into()),
            schema_v: SCHEMA_V,
            payload: json!({"a":[1,2],"b":"x"}),
            blob_ref: Some(BlobHash("ab".into())),
            tainted: true,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""kind":"future.thing""#));
        assert!(s.contains(r#""actor":"runtime""#));
        assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), e);
    }
}
