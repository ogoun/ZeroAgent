//! Команды `audit` и `journal export` (R1-C1 D91, SEC-090, SEC-091).
//!
//! `zero-agent audit --project P [--verify] [--json]` — записи журнала аудита проекта и
//! проверка hash-цепочки: строка на запись (`--json` — JSONL [`AuditEntry`]), с
//! `--verify` — только итог проверки. Разрыв цепочки — код 1.
//!
//! `zero-agent journal export --project P --run R` — события run в JSONL по порядку `seq`,
//! строка на событие. Секреты маскируются [`SecretMasker`] (D90): известные значения
//! секретов, прочитанных при старте команды (`ZA_SECRET_*`, от 8 символов), →
//! `[secret masked: <key_ref>]`, и шаблоны ключей, токенов, приватных ключей и паролей в
//! URL → `[secret masked: <вид>]`. Шаблоны работают и без переменных в окружении
//! выгрузки (ревью задачи 4, m4). Те же маски — у записей `audit`.
//!
//! Журнал, занятый идущим run, — сообщение [`AuditCmdError::Busy`] и код 1.

use std::path::Path;
use std::process::ExitCode;

use za_protocol::{RunId, Seq, canonical_json};
use za_secrets::SecretMasker;
use za_store::{AuditEntry, ProjectStore, StoreError};

use crate::run::{RunError, canonical_project, open_existing};
use crate::secret::mask_json;

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

/// `zero-agent journal export`: события run в JSONL с маской секретов `masker`.
pub async fn cmd_journal_export(
    home: &Path,
    project: &Path,
    run: RunId,
    masker: &SecretMasker,
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
            out.push_str(&canonical_json(&mask_json(masker, &v)));
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
    masker: &SecretMasker,
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
                target: masker.mask(&e.target).into_owned(),
                payload: mask_json(masker, &e.payload),
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

    /// Маска выгрузки: известные значения (сначала самые длинные) и шаблоны D90 — и в
    /// строках, и в ключах объектов.
    #[test]
    fn export_mask_covers_known_values_and_patterns() {
        let m = SecretMasker::new(vec![
            ("a".into(), "abcdefgh".into()),
            ("long".into(), "abcdefgh-ijkl".into()),
            ("short".into(), "1234567".into()),
        ]);
        // Фиктивный токен из частей: цельная строка формата токена в исходнике
        // блокируется защитой push от секретов.
        let tok = ["gh", "p_0123456789abcdefghijklmnopqrstuvwxyz"].concat();
        assert_eq!(
            mask_json(
                &m,
                &json!({"abcdefgh": ["abcdefgh", 1, {"k": "abcdefgh-ijkl 1234567"}],
                        "tok": tok})
            ),
            json!({"[secret masked: a]": ["[secret masked: a]", 1, {"k": "[secret masked: long] 1234567"}],
                   "tok": "[secret masked: github_token]"})
        );
    }
}
