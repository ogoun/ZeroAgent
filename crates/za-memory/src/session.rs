//! Сессия run в памяти (R1-M D139; часть 04, §5; MEM-063).
//!
//! - **Id** — `za-<project_id>-<run>`: 16 hex `project_id` (`zero-agent/src/home.rs`) и UUIDv7
//!   run, 56 символов из 128 допустимых. Id нового run известен до создания run.
//! - **Регистрация** — `POST /api/v1/sessions {id, agent: "zero-agent", solution, area,
//!   agent_type: "top"}` перед первым запросом run; повтор безопасен, а resume той же сессии
//!   снимает отметку конца (`ended_at`). Тело строгое (`NewSession` ZAM —
//!   `deny_unknown_fields`): других полей нет, `new_epoch`, `holdout` и `tokens` не шлются —
//!   с ними запрос был бы не регистрацией, а учётом.
//! - **Конец** (`PATCH /sessions/{id} {ended: true}`) — задача 9.

use std::time::Duration;

use serde_json::{Map, Value, json};
use za_protocol::RunId;

use crate::error::ZamError;
use crate::transport::{Ctx, Method, ZamClient};

/// Путь регистрации сессии.
pub const SESSIONS_PATH: &str = "/api/v1/sessions";
/// Имя агента в сессии.
pub const AGENT: &str = "zero-agent";
/// Тип агента run: верхний (рой — R2).
pub const AGENT_TYPE_TOP: &str = "top";
/// Срок регистрации: как у `/api/v1/health` (D137) — короткий служебный запрос перед
/// `/recall`.
pub const SESSION_LIMIT: Duration = Duration::from_secs(3);

/// Привязка run к памяти: проект, решение и область (D135, D139); `writable` — может ли run
/// писать в память (доверенный проект; шлюз записи — задача 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunBinding {
    /// 16 hex проекта (`zero-agent/src/home.rs`).
    pub project_id: String,
    /// GUID решения проекта; `None` — проект не привязан (или не доверен).
    pub solution: Option<String>,
    /// Область проекта (`X-Zam-Area`).
    pub area: Option<String>,
    pub writable: bool,
}

/// Id сессии run: `za-<project_id>-<run>` (D139).
pub fn session_id(project_id: &str, run: RunId) -> String {
    format!("za-{project_id}-{run}")
}

/// Тело регистрации сессии `id` (D139): ровно поля `NewSession`, которые нужны агенту.
pub fn register_body(id: &str, binding: &RunBinding) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!(id));
    m.insert("agent".into(), json!(AGENT));
    if let Some(s) = &binding.solution {
        m.insert("solution".into(), json!(s));
    }
    if let Some(a) = &binding.area {
        m.insert("area".into(), json!(a));
    }
    m.insert("agent_type".into(), json!(AGENT_TYPE_TOP));
    Value::Object(m)
}

/// Регистрирует сессию `cx.session` (D139) с атрибуцией `cx`.
pub async fn register(
    client: &ZamClient,
    binding: &RunBinding,
    cx: &Ctx<'_>,
) -> Result<(), ZamError> {
    let id = cx.session.unwrap_or_default();
    client
        .send_json(
            Method::POST,
            SESSIONS_PATH,
            &register_body(id, binding),
            cx,
            None,
            None,
            SESSION_LIMIT,
        )
        .await
        .map(|_| ())
}
