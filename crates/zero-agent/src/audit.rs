//! Команды `audit` и `journal export` (R1-C1 D91, SEC-090, SEC-091).
//!
//! `zero-agent audit --project P [--verify] [--json]` — записи журнала аудита проекта и
//! проверка hash-цепочки: строка на запись (`--json` — JSONL [`AuditEntry`]), с
//! `--verify` — только итог проверки. Разрыв цепочки — код 1.
//!
//! `zero-agent journal export --project P --run R` — события run в JSONL по порядку `seq`,
//! строка на событие. Значения секретов маскируются (D90): известные значения
//! `ZA_SECRET_*` длиной от [`MIN_SECRET_LEN`] символов заменяются на
//! `[secret masked: <key_ref>]`, сначала самые длинные. Сканер шаблонов ключей D90 —
//! задача 10 плана (`za-secrets`); до него маска покрывает только известные значения.
//!
//! Журнал, занятый идущим run, — сообщение [`AuditCmdError::Busy`] и код 1.

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;
use za_protocol::{RunId, Seq, canonical_json};
use za_store::{AuditEntry, ProjectStore, StoreError};

use crate::run::{RunError, canonical_project, open_existing};

/// Префикс переменных окружения с ключами (R1-A D4).
const SECRET_PREFIX: &str = "ZA_SECRET_";
/// Значения короче не маскируются: короткая строка совпадает с обычным текстом (D90).
pub const MIN_SECRET_LEN: usize = 8;
/// Размер страницы чтения журнала.
const PAGE: usize = 1000;

/// Ошибка команд `audit` и `journal export`.
#[derive(Debug, thiserror::Error)]
pub enum AuditCmdError {
    /// Журнал держит запущенный run (D91).
    #[error("журнал проекта занят запущенным run; повторите после его остановки")]
    Busy,
    #[error(transparent)]
    Run(RunError),
    #[error(transparent)]
    Store(StoreError),
    #[error("вывод в JSON: {0}")]
    Json(serde_json::Error),
}

impl From<StoreError> for AuditCmdError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Locked => Self::Busy,
            e => Self::Store(e),
        }
    }
}

impl From<RunError> for AuditCmdError {
    fn from(e: RunError) -> Self {
        match e {
            RunError::Store(s) => s.into(),
            e => Self::Run(e),
        }
    }
}

/// Журнал существующего проекта: занят — [`AuditCmdError::Busy`].
fn open(home: &Path, project: &Path) -> Result<ProjectStore, AuditCmdError> {
    let project = canonical_project(project)?;
    Ok(open_existing(home, &project)?)
}

/// Маска известных значений секретов (D90) для выгрузки.
#[derive(Debug, Default)]
pub struct KnownSecrets {
    /// (значение, key_ref) по убыванию длины значения.
    values: Vec<(String, String)>,
}

impl KnownSecrets {
    /// Значения переменных `ZA_SECRET_<KEY_REF>` из `vars` (продукт передаёт
    /// `std::env::vars_os()`); `key_ref` — хвост имени в нижнем регистре.
    pub fn from_vars(vars: impl Iterator<Item = (OsString, OsString)>) -> Self {
        let mut values: Vec<(String, String)> = vars
            .filter_map(|(k, v)| {
                let k = k.into_string().ok()?;
                let tail = k.strip_prefix(SECRET_PREFIX)?;
                let v = v.into_string().ok()?;
                (v.chars().count() >= MIN_SECRET_LEN).then(|| (v, tail.to_ascii_lowercase()))
            })
            .collect();
        values.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        values.dedup_by(|a, b| a.0 == b.0);
        Self { values }
    }

    /// Текст с замаскированными значениями.
    pub fn mask(&self, s: &str) -> String {
        let mut out = s.to_owned();
        for (v, name) in &self.values {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), &format!("[secret masked: {name}]"));
            }
        }
        out
    }

    /// Значение JSON с замаскированными строками и ключами объектов.
    pub fn mask_value(&self, v: Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.mask(&s)),
            Value::Array(a) => Value::Array(a.into_iter().map(|x| self.mask_value(x)).collect()),
            Value::Object(o) => Value::Object(
                o.into_iter()
                    .map(|(k, x)| (self.mask(&k), self.mask_value(x)))
                    .collect(),
            ),
            other => other,
        }
    }
}

/// `zero-agent journal export`: события run в JSONL с маской секретов `secrets`.
pub async fn cmd_journal_export(
    home: &Path,
    project: &Path,
    run: RunId,
    secrets: &KnownSecrets,
) -> Result<ExitCode, AuditCmdError> {
    let store = open(home, project)?;
    if store.get_run(run).await?.is_none() {
        return Err(StoreError::UnknownRun(run).into());
    }
    let mut out = String::new();
    let mut after = Seq(0);
    loop {
        let page = store.read_run(run, after, PAGE).await?;
        let Some(last) = page.last() else { break };
        after = last.seq;
        for e in page {
            let v = serde_json::to_value(&e).map_err(AuditCmdError::Json)?;
            out.push_str(&canonical_json(&secrets.mask_value(v)));
            out.push('\n');
        }
    }
    print!("{out}");
    Ok(ExitCode::SUCCESS)
}

/// `zero-agent audit`: записи аудита или (`verify`) итог проверки цепочки.
pub async fn cmd_audit(
    home: &Path,
    project: &Path,
    verify: bool,
    json: bool,
    secrets: &KnownSecrets,
) -> Result<ExitCode, AuditCmdError> {
    let store = open(home, project)?;
    // Сначала проверка: строка с `NULL` или не-JSON даёт текст разрыва D91, а не ошибку
    // разбора записи (ревью задачи 4, m3).
    let check = store.verify_audit().await?;
    if verify {
        // Цепочка цела — записи идут подряд с 1, их число — `seq` головы.
        let count = store.audit_head().await?.map_or(0, |(seq, _)| seq);
        match (&check, json) {
            (Ok(()), false) => println!("цепочка аудита цела: записей {count}"),
            (Err(b), false) => eprintln!("zero-agent: {b}"),
            (Ok(()), true) => println!(
                "{}",
                canonical_json(&serde_json::json!({"ok": true, "entries": count}))
            ),
            (Err(b), true) => println!(
                "{}",
                canonical_json(&serde_json::json!({"ok": false, "break": b.seq}))
            ),
        }
    } else {
        // При разрыве печатаются записи до него: они разбираются и цепочку продолжают.
        let upto = check.as_ref().err().map(|b| b.seq.saturating_sub(1));
        let mut out = String::new();
        for e in entries_upto(&store, upto).await? {
            let e = AuditEntry {
                target: secrets.mask(&e.target),
                payload: secrets.mask_value(e.payload),
                ..e
            };
            if json {
                let v = serde_json::to_value(&e).map_err(AuditCmdError::Json)?;
                out.push_str(&canonical_json(&v));
            } else {
                out.push_str(&entry_line(&e));
            }
            out.push('\n');
        }
        print!("{out}");
        if let Err(b) = &check {
            eprintln!("zero-agent: {b}");
        }
    }
    Ok(if check.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Записи аудита по возрастанию `seq`, не дальше `upto` (`None` — все).
async fn entries_upto(
    store: &ProjectStore,
    upto: Option<u64>,
) -> Result<Vec<AuditEntry>, StoreError> {
    let mut all = Vec::new();
    loop {
        let after = all.last().map_or(0, |e: &AuditEntry| e.seq);
        let left = upto.map_or(PAGE, |u| {
            PAGE.min(usize::try_from(u.saturating_sub(after)).unwrap_or(PAGE))
        });
        if left == 0 {
            return Ok(all);
        }
        let page = store.audit_entries(after, left).await?;
        if page.is_empty() {
            return Ok(all);
        }
        all.extend(page);
    }
}

/// Строка записи для человека: `seq`, время UTC, actor, action, target, payload.
fn entry_line(e: &AuditEntry) -> String {
    let ts = jiff::Timestamp::from_millisecond(e.ts)
        .map_or_else(|_| e.ts.to_string(), |t| t.to_string());
    format!(
        "{} {ts} {} {} {} {}",
        e.seq,
        e.actor,
        e.action,
        e.target,
        canonical_json(&e.payload)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars(list: &[(&str, &str)]) -> KnownSecrets {
        KnownSecrets::from_vars(
            list.iter()
                .map(|(k, v)| (OsString::from(*k), OsString::from(*v))),
        )
    }

    #[test]
    fn known_secrets_mask_longest_first_and_skip_short() {
        let m = vars(&[
            ("ZA_SECRET_A", "abcdefgh"),
            ("ZA_SECRET_LONG", "abcdefgh-ijkl"),
            ("ZA_SECRET_SHORT", "1234567"),
            ("OTHER", "zzzzzzzzzz"),
        ]);
        assert_eq!(
            m.mask("x abcdefgh-ijkl y abcdefgh 1234567 zzzzzzzzzz"),
            "x [secret masked: long] y [secret masked: a] 1234567 zzzzzzzzzz"
        );
        assert_eq!(
            m.mask_value(json!({"abcdefgh": ["abcdefgh", 1, {"k": "abcdefgh-ijkl"}]})),
            json!({"[secret masked: a]": ["[secret masked: a]", 1, {"k": "[secret masked: long]"}]})
        );
        assert_eq!(vars(&[]).mask("abcdefgh"), "abcdefgh");
    }
}
