//! Runs: создание, статусы с `generation`, снимки состояния.
//!
//! Журнал — источник истины: каждое изменение строки `runs` сопровождается
//! событием (`run.created`, `run.status`) в той же транзакции. Статус меняется
//! только по таблице [`RunStatus::can_transition`] и только при совпадении
//! `generation` (SWM-064); каждый переход увеличивает `generation` на единицу.

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use za_protocol::{Actor, EventKind, NewEvent, RunId, RunRole, RunStatus, Seq, canonical_json};

use crate::events::{self, corrupt, name_of, parse_name, seq_from, seq_sql, uuid};
use crate::{ProjectStore, StoreError};

/// Параметры создания run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRun {
    pub id: RunId,
    pub project_id: String,
    /// Родитель в дереве; `None` у корня.
    pub parent_id: Option<RunId>,
    pub role: RunRole,
    pub model_profile: Option<String>,
}

/// Run, как он хранится в таблице `runs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub id: RunId,
    pub parent_id: Option<RunId>,
    /// Корень дерева; у корня совпадает с `id`.
    pub root_id: RunId,
    pub role: RunRole,
    pub status: RunStatus,
    /// Растёт на единицу при каждой смене статуса.
    pub generation: u64,
}

/// Итог смены статуса: те `from` и `generation`, что записаны в событие `run.status`
/// той же транзакции. По ним шина событий публикует копию события без чтения (D34).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusChange {
    /// Статус до перехода.
    pub from: RunStatus,
    /// Новая `generation`.
    pub generation: u64,
}

impl ProjectStore {
    /// Создаёт run в статусе `Queued` с `generation` 0 и пишет событие `run.created`.
    /// `root_id` наследуется от родителя; неизвестный родитель даёт
    /// [`StoreError::UnknownRun`], повторный `id` — ошибку SQLite.
    pub async fn create_run(&self, r: NewRun) -> Result<Run, StoreError> {
        let dir = self.dir();
        self.write(move |c| {
            let root_id = match r.parent_id {
                None => r.id,
                Some(p) => c
                    .query_row(
                        "SELECT root_id FROM runs WHERE id = ?1",
                        [id_sql(p)],
                        |row| row.get::<_, Vec<u8>>(0),
                    )
                    .optional()?
                    .map(|b| uuid(&b).map(RunId))
                    .transpose()?
                    .ok_or(StoreError::UnknownRun(p))?,
            };
            let status = RunStatus::Queued;
            let now = Timestamp::now().to_string();
            c.execute(
                "INSERT INTO runs (id, project_id, parent_id, root_id, role, status, \
                 model_profile, created_at, updated_at, generation) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 0)",
                params![
                    id_sql(r.id),
                    r.project_id,
                    r.parent_id.map(id_sql),
                    id_sql(root_id),
                    name_of(r.role)?,
                    name_of(status)?,
                    r.model_profile,
                    now,
                ],
            )?;
            let payload = json!({
                "project_id": r.project_id,
                "parent_id": r.parent_id.map(|p| p.to_string()),
                "root_id": root_id.to_string(),
                "role": r.role,
                "status": status,
                "model_profile": r.model_profile,
            });
            events::insert(
                c,
                &dir,
                &runtime_event(r.id, EventKind::RunCreated, payload),
            )?;
            Ok(Run {
                id: r.id,
                parent_id: r.parent_id,
                root_id,
                role: r.role,
                status,
                generation: 0,
            })
        })
        .await
    }

    /// Run по идентификатору; `None`, если его нет.
    pub async fn get_run(&self, id: RunId) -> Result<Option<Run>, StoreError> {
        self.read(move |c| {
            c.query_row(
                "SELECT parent_id, root_id, role, status, generation FROM runs WHERE id = ?1",
                [id_sql(id)],
                |row| {
                    Ok((
                        row.get::<_, Option<Vec<u8>>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?
            .map(|(parent, root, role, status, generation)| {
                Ok(Run {
                    id,
                    parent_id: parent.map(|b| uuid(&b).map(RunId)).transpose()?,
                    root_id: RunId(uuid(&root)?),
                    role: parse_name(role, "role")?,
                    status: parse_name(status, "status")?,
                    generation: generation_from(generation)?,
                })
            })
            .transpose()
        })
        .await
    }

    /// Меняет статус run и возвращает [`StatusChange`]: прежний статус и новую `generation`.
    ///
    /// Отказы ничего не меняют: [`StoreError::UnknownRun`], если run нет;
    /// [`StoreError::StaleGeneration`], если `expected_generation` не текущая;
    /// [`StoreError::InvalidTransition`], если переход запрещён. Успех пишет
    /// событие `run.status` `{from, to, generation}` в той же транзакции.
    pub async fn set_status(
        &self,
        id: RunId,
        to: RunStatus,
        expected_generation: u64,
    ) -> Result<StatusChange, StoreError> {
        self.set_status_with_reason(id, to, expected_generation, None)
            .await
    }

    /// Как [`Self::set_status`], но с причиной перехода: `Some(reason)` добавляет
    /// в payload `run.status` поле `reason`, `None` пишет payload без него.
    pub async fn set_status_with_reason(
        &self,
        id: RunId,
        to: RunStatus,
        expected_generation: u64,
        reason: Option<&str>,
    ) -> Result<StatusChange, StoreError> {
        let dir = self.dir();
        let reason = reason.map(str::to_owned);
        self.write(move |c| {
            let (from, current) = current_status(c, id)?;
            if current != expected_generation {
                return Err(StoreError::StaleGeneration { current });
            }
            if !from.can_transition(to) {
                return Err(StoreError::InvalidTransition { from, to });
            }
            let next = current
                .checked_add(1)
                .ok_or_else(|| corrupt("generation переполнена"))?;
            let n = c.execute(
                "UPDATE runs SET status = ?1, generation = ?2, updated_at = ?3 \
                 WHERE id = ?4 AND generation = ?5",
                params![
                    name_of(to)?,
                    generation_sql(next)?,
                    Timestamp::now().to_string(),
                    id_sql(id),
                    generation_sql(current)?,
                ],
            )?;
            if n != 1 {
                return Err(corrupt("строка run изменилась внутри транзакции"));
            }
            let payload = run_status_payload(from, to, next, reason.as_deref());
            events::insert(c, &dir, &runtime_event(id, EventKind::RunStatus, payload))?;
            Ok(StatusChange {
                from,
                generation: next,
            })
        })
        .await
    }

    /// Сохраняет снимок состояния run на событии `seq`; снимок с тем же `seq` заменяется.
    pub async fn save_snapshot(&self, id: RunId, seq: Seq, state: Value) -> Result<(), StoreError> {
        self.write(move |c| {
            c.execute(
                "INSERT INTO snapshots (run_id, seq, state) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (run_id, seq) DO UPDATE SET state = excluded.state",
                params![id_sql(id), seq_sql(seq), canonical_json(&state)],
            )?;
            Ok(())
        })
        .await
    }

    /// Снимок run с наибольшим `seq`; `None`, если снимков нет.
    pub async fn latest_snapshot(&self, id: RunId) -> Result<Option<(Seq, Value)>, StoreError> {
        self.read(move |c| {
            c.query_row(
                "SELECT seq, state FROM snapshots WHERE run_id = ?1 ORDER BY seq DESC LIMIT 1",
                [id_sql(id)],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(seq, state)| {
                let state =
                    serde_json::from_str(&state).map_err(|e| corrupt(&format!("снимок: {e}")))?;
                Ok((seq_from(seq)?, state))
            })
            .transpose()
        })
        .await
    }
}

impl ProjectStore {
    /// Все снимки run по возрастанию `seq` (проверки и диагностика; resume берёт
    /// [`Self::latest_snapshot`]).
    pub async fn snapshots(&self, id: RunId) -> Result<Vec<(Seq, Value)>, StoreError> {
        self.read(move |c| {
            let mut stmt =
                c.prepare("SELECT seq, state FROM snapshots WHERE run_id = ?1 ORDER BY seq")?;
            let rows = stmt
                .query_map([id_sql(id)], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|(seq, state)| {
                    let state = serde_json::from_str(&state)
                        .map_err(|e| corrupt(&format!("снимок: {e}")))?;
                    Ok((seq_from(seq)?, state))
                })
                .collect()
        })
        .await
    }
}

/// Проверяет, что run существует; иначе [`StoreError::UnknownRun`].
pub(crate) fn ensure_run(c: &Connection, id: RunId) -> Result<(), StoreError> {
    current_status(c, id).map(|_| ())
}

/// Текущие статус и `generation` run в транзакции писателя.
fn current_status(c: &Connection, id: RunId) -> Result<(RunStatus, u64), StoreError> {
    let (status, generation) = c
        .query_row(
            "SELECT status, generation FROM runs WHERE id = ?1",
            [id_sql(id)],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
        .ok_or(StoreError::UnknownRun(id))?;
    Ok((parse_name(status, "status")?, generation_from(generation)?))
}

/// Payload события `run.status`: `{from, to, generation}` и `reason`, если задана.
/// Одна функция для журнала и шины событий, чтобы их копии не расходились.
pub fn run_status_payload(
    from: RunStatus,
    to: RunStatus,
    generation: u64,
    reason: Option<&str>,
) -> Value {
    let mut payload = json!({ "from": from, "to": to, "generation": generation });
    if let Some(reason) = reason {
        payload["reason"] = Value::String(reason.to_owned());
    }
    payload
}

pub(crate) fn runtime_event(run: RunId, kind: EventKind, payload: Value) -> NewEvent {
    NewEvent {
        run_id: run,
        parent_seq: None,
        actor: Actor::Runtime,
        kind,
        payload,
        tainted: false,
    }
}

pub(crate) fn id_sql(id: RunId) -> Vec<u8> {
    id.0.as_bytes().to_vec()
}

fn generation_from(v: i64) -> Result<u64, StoreError> {
    u64::try_from(v).map_err(|_| corrupt("отрицательная generation"))
}

fn generation_sql(v: u64) -> Result<i64, StoreError> {
    i64::try_from(v).map_err(|_| corrupt("generation вне диапазона"))
}
