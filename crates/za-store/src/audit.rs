//! Журнал аудита (таблица `audit`, миграция 0003; SEC-090, SEC-091, R1-C1 D91).
//!
//! Одобрения, гранты, доверие к проекту, загрузка политики, восстановление чекпоинтов.
//! Записи связаны hash-цепочкой: `hash` записи — BLAKE3 от `prev_hash`, одного нулевого
//! байта и канонического JSON объекта `{seq, actor, action, target, payload, ts}`;
//! `prev_hash` первой записи — [`AUDIT_GENESIS`] (64 символа «0»).
//!
//! Цепочку продолжает только поток-писатель: последний `hash` читается в той же
//! транзакции, что и вставка, `seq` и `ts` ставит писатель. Один писатель на файл
//! (ARC-030) исключает гонку `prev_hash`. Записи аудита фиксируются с
//! `synchronous=FULL`: это решения человека, они редки и переживают отключение питания.
//!
//! Проверка цепочки — [`ProjectStore::verify_audit`] (команда `zero-agent audit
//! --verify`). Решения по аудиту принимаются только на целой цепочке:
//! [`ProjectStore::last_audit`] на разорванной цепочке даёт [`StoreError::Corrupt`].
//! Удаление хвоста цепочки (последних записей) хэшами не обнаруживается — это свойство
//! любой цепочки без внешнего якоря; якорь — голова цепочки [`ProjectStore::audit_head`],
//! сохранённая вне таблицы (снимок run, задача 13). Против процесса с правом записи в
//! `journal.db` цепочка без ключа не защищает: он пересчитает её целиком.

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use za_protocol::{Actor, canonical_json};

use crate::effects::resolve_in;
use crate::events::corrupt;
use crate::{ProjectStore, StoreError};

/// `prev_hash` первой записи цепочки: 64 символа «0».
pub const AUDIT_GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Запись журнала аудита, как она хранится.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Номер записи с 1, подряд и без пропусков.
    pub seq: u64,
    /// `hash` предыдущей записи; у первой — [`AUDIT_GENESIS`].
    pub prev_hash: String,
    /// BLAKE3 записи в hex.
    pub hash: String,
    /// Кто действовал: `user`, `runtime` и т. п.
    pub actor: String,
    /// Действие — константа `za_protocol::audit_action`.
    pub action: String,
    /// Объект действия: id прерывания, путь проекта или гранта.
    pub target: String,
    pub payload: Value,
    /// Время записи, миллисекунды Unix (UTC); ставит писатель.
    pub ts: i64,
}

/// Разрыв цепочки аудита: запись `seq` не сходится с хэшем, предыдущей записью или
/// номером (первая такая запись).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("разрыв цепочки аудита на записи {seq}")]
pub struct AuditBreak {
    pub seq: u64,
}

/// Новая запись аудита; `seq`, `prev_hash`, `hash` и `ts` назначает писатель.
#[derive(Debug, Clone)]
pub struct AuditNew {
    pub actor: String,
    pub action: String,
    pub target: String,
    pub payload: Value,
}

/// Столбцы записи в порядке [`raw_entry`].
const COLS: &str = "SELECT seq, prev_hash, hash, actor, action, target, payload, ts FROM audit";

impl ProjectStore {
    /// Добавляет запись в конец цепочки; возвращает её такой, какой она записана.
    pub async fn append_audit(&self, a: AuditNew) -> Result<AuditEntry, StoreError> {
        self.write_durable(move |c| insert(c, &a)).await
    }

    /// Закрыть прерывание и записать аудит одной транзакцией (D91); false — прерывание уже решено.
    ///
    /// Прерывание решается от имени пользователя ([`Actor::User`]) с событием
    /// `interrupt.resolved`, как [`ProjectStore::resolve_interrupt`]. Записи `audit`
    /// добавляются по порядку только вместе с решением: уже решённое прерывание даёт
    /// `Ok(false)` без записей, неизвестное — [`StoreError::UnknownInterrupt`].
    pub async fn resolve_with_audit(
        &self,
        id: &str,
        response: Value,
        audit: Vec<AuditNew>,
    ) -> Result<bool, StoreError> {
        let dir = self.dir();
        let id = id.to_owned();
        self.write_durable(move |c| {
            if !resolve_in(c, &dir, &id, response, Actor::User)? {
                return Ok(false);
            }
            for a in &audit {
                insert(c, a)?;
            }
            Ok(true)
        })
        .await
    }

    /// Записи с `seq > after` по возрастанию `seq`, не больше `limit`. Цепочка не
    /// проверяется: это просмотр (для решений — [`Self::last_audit`]).
    pub async fn audit_entries(
        &self,
        after: u64,
        limit: usize,
    ) -> Result<Vec<AuditEntry>, StoreError> {
        self.read(move |c| {
            let mut stmt = c.prepare(&format!("{COLS} WHERE seq > ?1 ORDER BY seq LIMIT ?2"))?;
            let rows = stmt.query_map(
                params![
                    i64::try_from(after).unwrap_or(i64::MAX),
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                raw_entry,
            )?;
            rows.map(|r| {
                let raw = r?;
                let seq = raw.seq;
                entry_from(raw).ok_or_else(|| corrupt(&format!("запись аудита {seq}")))
            })
            .collect()
        })
        .await
    }

    /// Последняя запись, у которой `action` начинается с `action_prefix` (`"trust."` —
    /// последнее решение о доверии, D83). Читается только из целой цепочки: разрыв даёт
    /// [`StoreError::Corrupt`], и решение по подделанной записи не принимается.
    pub async fn last_audit(&self, action_prefix: &str) -> Result<Option<AuditEntry>, StoreError> {
        let prefix = action_prefix.to_owned();
        self.read(move |c| {
            // Проверка и выборка — в одном снимке чтения.
            let tx = c.unchecked_transaction()?;
            if let Err(b) = verify(&tx)? {
                return Err(StoreError::Corrupt(b.to_string()));
            }
            let raw = tx
                .query_row(
                    &format!(
                        "{COLS} WHERE substr(action, 1, length(?1)) = ?1 \
                         ORDER BY seq DESC LIMIT 1"
                    ),
                    [&prefix],
                    raw_entry,
                )
                .optional()?;
            let out = match raw {
                None => None,
                Some(raw) => {
                    let seq = raw.seq;
                    Some(entry_from(raw).ok_or_else(|| corrupt(&format!("запись аудита {seq}")))?)
                }
            };
            tx.finish()?;
            Ok(out)
        })
        .await
    }

    /// Голова цепочки: `(seq, hash)` последней записи; `None` у пустого аудита. Цепочка
    /// не проверяется. Внешний якорь против удаления хвоста (ревью задачи 4, m1): голову,
    /// сохранённую раньше (снимок run, задача 13), сверяют с записью `seq` — запись
    /// пропала или хэш другой, значит хвост удалён или подменён.
    pub async fn audit_head(&self) -> Result<Option<(u64, String)>, StoreError> {
        self.read(|c| {
            c.query_row(
                "SELECT seq, hash FROM audit ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(s, h)| {
                u64::try_from(s)
                    .map(|s| (s, h))
                    .map_err(|_| corrupt("отрицательный seq аудита"))
            })
            .transpose()
        })
        .await
    }

    /// Проверяет цепочку целиком: `Ok(Err(AuditBreak))` — первая запись, где цепочка
    /// разорвана (подделано поле, хэш, пропущена или переставлена запись).
    pub async fn verify_audit(&self) -> Result<Result<(), AuditBreak>, StoreError> {
        self.read(|c| {
            let tx = c.unchecked_transaction()?;
            let r = verify(&tx)?;
            tx.finish()?;
            Ok(r)
        })
        .await
    }
}

/// BLAKE3 записи: `prev_hash`, нулевой байт, канонический JSON полей записи.
fn chain_hash(
    prev_hash: &str,
    seq: u64,
    actor: &str,
    action: &str,
    target: &str,
    payload: &Value,
    ts: i64,
) -> String {
    let body = json!({
        "seq": seq,
        "actor": actor,
        "action": action,
        "target": target,
        "payload": payload,
        "ts": ts,
    });
    let mut h = blake3::Hasher::new();
    h.update(prev_hash.as_bytes());
    h.update(&[0]);
    h.update(canonical_json(&body).as_bytes());
    h.finalize().to_hex().to_string()
}

/// Вставка записи в открытой транзакции писателя: продолжает цепочку от последней записи.
fn insert(c: &Connection, a: &AuditNew) -> Result<AuditEntry, StoreError> {
    let last = c
        .query_row(
            "SELECT seq, hash FROM audit ORDER BY seq DESC LIMIT 1",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    let (seq, prev_hash) = match last {
        None => (1, AUDIT_GENESIS.to_owned()),
        Some((s, h)) => {
            let s = u64::try_from(s).map_err(|_| corrupt("отрицательный seq аудита"))?;
            (s + 1, h)
        }
    };
    let ts = Timestamp::now().as_millisecond();
    let hash = chain_hash(
        &prev_hash, seq, &a.actor, &a.action, &a.target, &a.payload, ts,
    );
    c.execute(
        "INSERT INTO audit (seq, prev_hash, hash, actor, action, target, payload, ts) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            i64::try_from(seq).map_err(|_| corrupt("seq аудита вне диапазона"))?,
            prev_hash,
            hash,
            a.actor,
            a.action,
            a.target,
            canonical_json(&a.payload),
            ts
        ],
    )?;
    Ok(AuditEntry {
        seq,
        prev_hash,
        hash,
        actor: a.actor.clone(),
        action: a.action.clone(),
        target: a.target.clone(),
        payload: a.payload.clone(),
        ts,
    })
}

/// Проход по цепочке в снимке `c`: номер подряд с 1, `prev_hash` — хэш предыдущей,
/// `hash` сходится с формулой, поля на месте и `payload` — JSON.
fn verify(c: &Connection) -> Result<Result<(), AuditBreak>, StoreError> {
    let mut stmt = c.prepare(&format!("{COLS} ORDER BY seq"))?;
    let mut rows = stmt.query_map([], raw_entry)?;
    let mut expected_seq: i64 = 1;
    let mut prev = AUDIT_GENESIS.to_owned();
    while let Some(raw) = rows.next().transpose()? {
        // Разрыв — на этой записи: она не продолжает цепочку (после пропуска — первая
        // запись за ним).
        let at = u64::try_from(raw.seq).unwrap_or(0);
        if raw.seq != expected_seq || raw.prev_hash != prev {
            return Ok(Err(AuditBreak { seq: at }));
        }
        let stored = raw.hash.clone();
        let Some(e) = entry_from(raw) else {
            return Ok(Err(AuditBreak { seq: at }));
        };
        let h = chain_hash(
            &e.prev_hash,
            e.seq,
            &e.actor,
            &e.action,
            &e.target,
            &e.payload,
            e.ts,
        );
        // Хэш считается по разобранному payload: иная запись того же JSON (пробелы,
        // порядок ключей) смысла не меняет и разрывом не считается.
        if h != stored {
            return Ok(Err(AuditBreak { seq: e.seq }));
        }
        prev = stored;
        expected_seq += 1;
    }
    Ok(Ok(()))
}

/// Строка `audit` в исходном виде; необязательные по схеме поля — `Option`.
struct RawEntry {
    seq: i64,
    prev_hash: String,
    hash: String,
    actor: Option<String>,
    action: Option<String>,
    target: Option<String>,
    payload: Option<String>,
    ts: Option<i64>,
}

fn raw_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawEntry> {
    Ok(RawEntry {
        seq: r.get(0)?,
        prev_hash: r.get(1)?,
        hash: r.get(2)?,
        actor: r.get(3)?,
        action: r.get(4)?,
        target: r.get(5)?,
        payload: r.get(6)?,
        ts: r.get(7)?,
    })
}

/// Запись из строки; `None`, если поля нет или `payload` не JSON.
fn entry_from(r: RawEntry) -> Option<AuditEntry> {
    Some(AuditEntry {
        seq: u64::try_from(r.seq).ok()?,
        prev_hash: r.prev_hash,
        hash: r.hash,
        actor: r.actor?,
        action: r.action?,
        target: r.target?,
        payload: serde_json::from_str(&r.payload?).ok()?,
        ts: r.ts?,
    })
}
