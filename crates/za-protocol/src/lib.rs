//! Типы протокола zero-agent: идентификаторы, события, статусы run, классы эффектов,
//! канонический JSON.

pub mod canonical;
pub mod effect;
pub mod event;
pub mod ids;
pub mod resource;
pub mod run;

pub use canonical::canonical_json;
pub use effect::{EffectClass, EffectKey};
pub use event::{Actor, Event, EventKind, NewEvent, SCHEMA_V};
pub use ids::{BlobHash, EventId, RunId, Seq};
pub use resource::Priority;
pub use run::{RunRole, RunStatus, pause_reason};
