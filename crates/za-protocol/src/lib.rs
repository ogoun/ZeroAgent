//! Типы протокола zero-agent: идентификаторы, события, статусы run, классы эффектов,
//! режимы и тиры, бюджеты, канонический JSON.

pub mod budget;
pub mod canonical;
pub mod effect;
pub mod event;
pub mod ids;
pub mod mode;
pub mod resource;
pub mod run;

pub use budget::{Budget, BudgetKind};
pub use canonical::canonical_json;
pub use effect::{EffectClass, EffectKey};
pub use event::{Actor, Event, EventKind, NewEvent, SCHEMA_V};
pub use ids::{BlobHash, EventId, RunId, Seq};
pub use mode::{RunMode, Tier};
pub use resource::Priority;
pub use run::{
    RunRole, RunStatus, alert_kind, approval_reason, audit_action, checkpoint_kind, deny_reason,
    detector, detector_action, fail_reason, irreversible, pause_reason, taint_origin,
};
