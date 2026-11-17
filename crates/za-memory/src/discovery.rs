//! Обнаружение сервера памяти и проверки статуса (R1-M D136; MEM-007, MEM-010, MEM-011).
//!
//! - **Путь** — `GET /api/v1/health` без токена: корневой `/health` отдаёт HTML WebUI, и
//!   ответ не JSON — L0 с причиной `not a ZeroAIMemory API`.
//! - **Уровни** — по флагам, номер версии ни с чем не сравнивается (только печатается):
//!   L0 — нет ответа или не ZAM; L1 — нет `features` (`0.0.10-m5.7`); L2 — в `features` есть
//!   `m6` (`0.0.11-m6`, `0.0.12-m6.5` и новее). Неизвестные флаги и поля пропускаются
//!   разбором и сохраняются в [`ServerInfo::features`] (`api-compat.md` ZAM).
//! - **Статус** ([`status`]) — для `doctor` и `memory status`: токен (`GET /api/v1/spaces`),
//!   решение (`GET /api/v1/solutions/{guid}`), личный Vault (`GET /api/v1/spaces/knowledge/
//!   vault`, поле `locked`) и выключенные типы подсказок (`hooks` ответа `POST /api/v1/recall
//!   {event: "session_start", record: false, budget_tokens: 1}` без сессии — ни отметок
//!   показа, ни учёта). Несовместимость или недоступность сервера агента не останавливает
//!   (MEM-011): статус только описывает. Не проверенное — не «всё хорошо»: Vault без
//!   булева `locked` — [`VaultState::Unchecked`], подсказки без `hooks` или с ошибкой
//!   `/recall` — [`MemoryStatus::hooks_unchecked`] (M2 ревью задачи 2).

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::{Value, json};

use crate::error::{ErrorKind, ZamError};
use crate::transport::{Ctx, Method, NOT_ZAM_TITLE, STATUS_LIMIT, VAULT_LIMIT, ZamClient};

/// Флаг выпуска M6 в `features` — признак уровня L2.
pub const FEATURE_M6: &str = "m6";

/// Типы подсказок, которые выключают REST `/recall` агента (D136): дайджест на старте,
/// подсказка к вводу и к ошибке инструмента.
pub const RECALL_HOOKS: [&str; 3] = ["session_start", "prompt", "post_tool"];

/// Уровень памяти (D136): L2 и L3 части 04 на `0.0.11-m6` слились.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Нет ответа или не ZAM: агент работает без памяти.
    L0,
    /// Сервер без `features` (`0.0.10-m5.7`).
    L1,
    /// В `features` есть `m6`.
    L2,
}

impl Level {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::L0 => "L0",
            Self::L1 => "L1",
            Self::L2 => "L2",
        }
    }
}

/// Что сервер сообщил о себе в `/api/v1/health`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub version: String,
    pub git: Option<String>,
    /// Все строковые флаги `features`, в том числе неизвестные клиенту.
    pub features: BTreeSet<String>,
    pub tokenizer_id: Option<String>,
    pub token_counter: Option<String>,
}

impl ServerInfo {
    /// Есть ли флаг `name`.
    pub fn has(&self, name: &str) -> bool {
        self.features.contains(name)
    }
}

/// Итог обнаружения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub level: Level,
    /// Ответ сервера; у L0 — `None`.
    pub info: Option<ServerInfo>,
    /// Почему L0: текст ошибки (`not a ZeroAIMemory API`, нет соединения, код 5xx).
    pub reason: Option<String>,
    /// Вид ошибки L0 (`Offline`, `Timeout`, `NotZam`, `Server` …): по нему порт решает о
    /// повторной пробе (D137).
    pub error: Option<ErrorKind>,
}

impl Discovery {
    fn l0(e: &ZamError) -> Self {
        Self {
            level: Level::L0,
            info: None,
            reason: Some(e.to_string()),
            error: Some(e.kind),
        }
    }
}

/// `GET /api/v1/health` без токена (срок `HEALTH_LIMIT`, 3 с) и уровень по флагам.
pub async fn discover(c: &ZamClient) -> Discovery {
    match c.health().await {
        Ok(v) => from_health(&v),
        Err(e) => {
            tracing::debug!(error = %e, "память: /api/v1/health без ответа ZAM");
            Discovery::l0(&e)
        }
    }
}

/// Уровень по телу `/api/v1/health`: нет строки `version` — не ZAM (L0); нет `features` —
/// L1; есть `m6` — L2; список флагов без `m6` — L1 (консервативно). Не строки в `features`
/// и незнакомые поля пропускаются.
pub fn from_health(v: &Value) -> Discovery {
    let str_of = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let Some(version) = str_of("version").filter(|s| !s.trim().is_empty()) else {
        return Discovery::l0(&ZamError::new(ErrorKind::NotZam, NOT_ZAM_TITLE));
    };
    let features: Option<BTreeSet<String>> = v.get("features").and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    });
    let level = match &features {
        Some(f) if f.contains(FEATURE_M6) => Level::L2,
        _ => Level::L1,
    };
    Discovery {
        level,
        info: Some(ServerInfo {
            version,
            git: str_of("git"),
            features: features.unwrap_or_default(),
            tokenizer_id: str_of("tokenizer_id"),
            token_counter: str_of("token_counter"),
        }),
        reason: None,
        error: None,
    }
}

/// Токен памяти по `GET /api/v1/spaces`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenState {
    /// `ZA_MEMORY_TOKEN` не задан.
    NotSet,
    Accepted,
    /// 401: неверен или отозван.
    Rejected,
    /// 429: проверка отложена (`Retry-After`).
    RateLimited(Option<Duration>),
    /// Не проверен: причина.
    Unchecked(String),
}

/// Привязка к решению проекта по `GET /api/v1/solutions/{guid}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolutionState {
    /// Проект не привязан к решению.
    NotBound,
    /// Доступно: имя, роль (`owner`, `member`, `admin`) и доступ (`rw`, `ro`).
    Bound {
        name: String,
        role: String,
        access: String,
    },
    /// 403: подробность сервера (`solution-access`: ждёт, отклонён, отозван).
    Forbidden(String),
    /// 404: решения нет у пользователя токена.
    NotFound,
    Unchecked(String),
}

/// Личный Vault (`knowledge`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultState {
    /// `locked: false` в ответе списка.
    Unlocked,
    /// Заблокирован (`locked: true`): причина сервера.
    Locked(String),
    /// Не проверен: причина (ошибка запроса, токен не принят, в ответе нет булева `locked`).
    /// «Не проверено» — не «открыт» (M2 ревью задачи 2).
    Unchecked(String),
}

/// Статус памяти для `doctor` и `memory status` (D136).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryStatus {
    pub discovery: Discovery,
    pub token: TokenState,
    pub solution: SolutionState,
    pub vault: VaultState,
    /// Выключенные у пользователя типы подсказок из [`RECALL_HOOKS`] (`hooks.<тип> = false`).
    /// Значим только при `hooks_unchecked == None`.
    pub hooks_off: Vec<String>,
    /// Подсказки не проверены: причина (`/recall` не удался, в ответе нет `hooks`, токен не
    /// принят, сервер недоступен). Пустой `hooks_off` тогда ничего не значит (M2 ревью
    /// задачи 2).
    pub hooks_unchecked: Option<String>,
}

/// Обнаружение и проверки статуса: без ответа ZAM и без токена — только обнаружение;
/// токен не принят — решение, Vault и подсказки не проверяются; иначе три проверки идут
/// параллельно. `solution` — GUID решения проекта (`None` — не привязан).
pub async fn status(c: &ZamClient, solution: Option<&str>) -> MemoryStatus {
    let discovery = discover(c).await;
    let unchecked = |why: &str| MemoryStatus {
        discovery: discovery.clone(),
        token: if c.has_token() {
            TokenState::Unchecked(why.to_owned())
        } else {
            TokenState::NotSet
        },
        solution: SolutionState::Unchecked(why.to_owned()),
        vault: VaultState::Unchecked(why.to_owned()),
        hooks_off: Vec::new(),
        hooks_unchecked: Some(why.to_owned()),
    };
    if discovery.level == Level::L0 {
        return unchecked("сервер памяти недоступен");
    }
    if !c.has_token() {
        return unchecked("токен не задан (ZA_MEMORY_TOKEN)");
    }
    let token = check_token(c).await;
    if token != TokenState::Accepted {
        return MemoryStatus {
            token,
            ..unchecked("токен не принят")
        };
    }
    let (solution, vault, hooks) =
        tokio::join!(check_solution(c, solution), check_vault(c), check_hooks(c));
    let (hooks_off, hooks_unchecked) = match hooks {
        Ok(off) => (off, None),
        Err(why) => (Vec::new(), Some(why)),
    };
    MemoryStatus {
        discovery,
        token,
        solution,
        vault,
        hooks_off,
        hooks_unchecked,
    }
}

async fn check_token(c: &ZamClient) -> TokenState {
    match c
        .get_json("/api/v1/spaces", &Ctx::default(), STATUS_LIMIT)
        .await
    {
        Ok(_) => TokenState::Accepted,
        Err(e) => match e.kind {
            ErrorKind::Auth => TokenState::Rejected,
            ErrorKind::RateLimited => TokenState::RateLimited(e.retry_after),
            _ => TokenState::Unchecked(e.to_string()),
        },
    }
}

/// GUID решения в пути: только буквы, цифры и `-` (UUID), иначе запрос не идёт.
fn guid_ok(g: &str) -> bool {
    !g.is_empty() && g.len() <= 64 && g.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

async fn check_solution(c: &ZamClient, solution: Option<&str>) -> SolutionState {
    let Some(g) = solution else {
        return SolutionState::NotBound;
    };
    if !guid_ok(g) {
        return SolutionState::Unchecked("GUID решения недопустимого вида".to_owned());
    }
    let path = format!("/api/v1/solutions/{g}");
    match c.get_json(&path, &Ctx::default(), STATUS_LIMIT).await {
        Ok(v) => {
            let s = |k: &str| {
                v.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            SolutionState::Bound {
                name: s("name"),
                role: s("role"),
                access: s("access"),
            }
        }
        Err(e) => match e.kind {
            ErrorKind::Forbidden => SolutionState::Forbidden(if e.detail.is_empty() {
                e.title
            } else {
                e.detail
            }),
            ErrorKind::NotFound => SolutionState::NotFound,
            _ => SolutionState::Unchecked(e.to_string()),
        },
    }
}

async fn check_vault(c: &ZamClient) -> VaultState {
    let v = match c
        .get_json(
            "/api/v1/spaces/knowledge/vault",
            &Ctx::default(),
            VAULT_LIMIT,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return VaultState::Unchecked(e.to_string()),
    };
    match v.get("locked").and_then(Value::as_bool) {
        Some(true) => {
            let reason = v
                .get("reason")
                .and_then(Value::as_str)
                .filter(|r| !r.trim().is_empty())
                .unwrap_or("причина не указана");
            VaultState::Locked(reason.to_owned())
        }
        Some(false) => VaultState::Unlocked,
        None => VaultState::Unchecked("в ответе Vault нет поля locked".to_owned()),
    }
}

/// Выключенные типы подсказок: `hooks` ответа `/recall` `session_start` без сессии и без
/// учёта (`record: false`). Ошибка запроса или ответ без объекта `hooks` — `Err(причина)`:
/// подсказки не проверены.
async fn check_hooks(c: &ZamClient) -> Result<Vec<String>, String> {
    let body = json!({"event": "session_start", "record": false, "budget_tokens": 1});
    match c
        .send_json(
            Method::POST,
            "/api/v1/recall",
            &body,
            &Ctx::default(),
            None,
            None,
            STATUS_LIMIT,
        )
        .await
    {
        Ok((_, v)) if v.get("hooks").is_some_and(Value::is_object) => Ok(hooks_off(&v)),
        Ok(_) => Err("в ответе /recall нет hooks".to_owned()),
        Err(e) => {
            tracing::debug!(error = %e, "память: hooks не прочитаны");
            Err(e.to_string())
        }
    }
}

/// Типы из [`RECALL_HOOKS`] со значением `false` в `hooks` ответа `/recall`.
pub fn hooks_off(recall: &Value) -> Vec<String> {
    let Some(h) = recall.get("hooks") else {
        return Vec::new();
    };
    RECALL_HOOKS
        .iter()
        .filter(|k| h.get(**k).and_then(Value::as_bool) == Some(false))
        .map(|k| (*k).to_owned())
        .collect()
}
