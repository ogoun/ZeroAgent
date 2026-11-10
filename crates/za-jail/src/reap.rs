//! Реестр групп процессов и остатки деревьев, переживших агента (D84; ревью задачи 5,
//! C1 и I1).
//!
//! Реестр лежит вне проекта: `ZA_HOME/projects/<project_id>/procs`. Каталог создаёт
//! вызывающий через `za_core::paths::home_dir`, без ссылок. Здесь каталог проверяется
//! ещё раз. Файл — запись [`Record`] на вызов: версия, run, группа, лидер, его
//! `starttime`, `boot_id` загрузки и маркер дерева.
//!
//! [`reap_leftovers`] читает только обычные файлы `*.json` своего формата. Неразобранный
//! файл не трогается. Разобранный удаляется после обработки. Сигнал идёт, только если
//! `boot_id` совпадает с текущим, а лидер:
//! - жив, и его `starttime` совпадает точно;
//! - или уже мёртв, но в группе есть процессы, стартовавшие не раньше него. Linux не
//!   выдаёт номер группы повторно, пока группа не пуста.
//!
//! Работает только на Linux (нужен `/proc`). На прочих ОС возвращает 0: Windows держит
//! дерево Job Object, на прочих Unix остатки — записанный риск (D84, ADR 0005).

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Версия формата записи реестра.
pub(crate) const RECORD_VERSION: u32 = 1;
/// Предел размера файла записи: больше — не наш файл.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const RECORD_MAX: u64 = 4096;

/// Запись реестра: одна группа процессов одного вызова.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub v: u32,
    pub run: String,
    pub pgid: i32,
    pub leader_pid: i32,
    /// `starttime` лидера из `/proc/<pid>/stat` (тики с загрузки), всегда > 0.
    pub leader_starttime: u64,
    /// `/proc/sys/kernel/random/boot_id` на момент записи.
    pub boot_id: String,
    /// Значение переменной `ZA_JAIL_CALL` у процессов дерева.
    pub marker: String,
}

/// Разбор записи своего формата: версия, лидер — лидер своей группы, `starttime` > 0,
/// непустые `boot_id` и маркер. Иначе `None` (чужой файл).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_record(bytes: &[u8]) -> Option<Record> {
    let r: Record = serde_json::from_slice(bytes).ok()?;
    let ok = r.v == RECORD_VERSION
        && r.pgid > 1
        && r.leader_pid == r.pgid
        && r.leader_starttime > 0
        && !r.boot_id.trim().is_empty()
        && !r.marker.is_empty();
    ok.then_some(r)
}

/// Поля `/proc/<pid>/stat`, нужные дереву.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct Stat {
    pub pid: i32,
    pub state: char,
    pub ppid: i32,
    pub pgrp: i32,
    pub starttime: u64,
}

/// Разбор текста `/proc/<pid>/stat`. Имя процесса в скобках может содержать пробелы и
/// скобки, поэтому поля считаются от последней `)`: после неё идут поле 3 (`state`), 4
/// (`ppid`), 5 (`pgrp`) … 22 (`starttime`) (proc(5)).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_stat(text: &str) -> Option<Stat> {
    let open = text.find(" (")?;
    let pid = text[..open].trim().parse().ok()?;
    let rest = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let ppid = fields.get(4 - 3)?.parse().ok()?;
    let pgrp = fields.get(5 - 3)?.parse().ok()?;
    let starttime = fields.get(22 - 3)?.parse().ok()?;
    Some(Stat {
        pid,
        state,
        ppid,
        pgrp,
        starttime,
    })
}

/// См. [`crate::DirectRunner::reap_leftovers`].
pub(crate) fn reap_leftovers(dir: &Path) -> usize {
    #[cfg(target_os = "linux")]
    {
        linux_reap(dir)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dir;
        0
    }
}

#[cfg(target_os = "linux")]
fn linux_reap(dir: &Path) -> usize {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    use nix::fcntl::OFlag;

    use crate::linux;

    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_dir() => {}
        Ok(_) => {
            tracing::warn!(dir = %dir.display(), "реестр групп — не каталог или ссылка, пропущен");
            return 0;
        }
        Err(_) => return 0,
    }
    let Some(boot) = linux::boot_id() else {
        tracing::warn!("нет boot_id, остатки не убираются");
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut killed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        // `DirEntry::file_type` не идёт по ссылке: ссылка на файл — не наш файл.
        let regular = entry.file_type().is_ok_and(|t| t.is_file());
        if !regular || path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let mut bytes = Vec::new();
        let read = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(OFlag::O_NOFOLLOW.bits())
            .open(&path)
            .and_then(|f| f.take(RECORD_MAX + 1).read_to_end(&mut bytes));
        let rec = match read {
            Ok(n) if (n as u64) <= RECORD_MAX => parse_record(&bytes),
            _ => None,
        };
        let Some(rec) = rec else {
            tracing::warn!(file = %path.display(), "чужой файл в реестре групп, не тронут");
            continue;
        };
        if rec.boot_id.trim() == boot {
            killed += kill_recorded(&rec);
        } else {
            tracing::info!(file = %path.display(), run = %rec.run, "запись прошлой загрузки, без сигнала");
        }
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!(file = %path.display(), error = %e, "запись реестра не удалена");
        }
    }
    killed
}

/// Убивает дерево записи, если группа — та самая (см. модуль). Возвращает число процессов,
/// получивших сигнал.
#[cfg(target_os = "linux")]
fn kill_recorded(rec: &Record) -> usize {
    use crate::linux;

    let procs = linux::snapshot();
    let mut roots = Vec::new();
    match procs.iter().find(|p| p.pid == rec.leader_pid) {
        Some(l) if l.starttime == rec.leader_starttime => roots.push(l.pid),
        Some(_) => {
            tracing::info!(pid = rec.leader_pid, run = %rec.run, "номер лидера занят другим процессом, без сигнала");
            return 0;
        }
        None => {
            let members = procs
                .iter()
                .any(|p| p.pgrp == rec.pgid && p.starttime >= rec.leader_starttime);
            if !members {
                return 0;
            }
        }
    }
    // Процессы с маркером дерева: ушедшие из группы и потерявшие родителя.
    roots.extend(
        procs
            .iter()
            .filter(|p| linux::has_marker(p.pid, &rec.marker))
            .map(|p| p.pid),
    );
    let targets = linux::targets(&procs, &roots, Some(rec.pgid), rec.leader_starttime);
    let n = linux::kill_all(&targets);
    tracing::info!(pgid = rec.pgid, run = %rec.run, killed = n, "убраны остатки дерева");
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_spaces_and_parens_in_name() {
        // pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt majflt cmajflt
        // utime stime cutime cstime priority nice threads itreal starttime ...
        let s =
            "4242 (a) b (c)) S 1 4240 4240 0 -1 4194560 10 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10";
        assert_eq!(
            parse_stat(s),
            Some(Stat {
                pid: 4242,
                state: 'S',
                ppid: 1,
                pgrp: 4240,
                starttime: 987_654
            })
        );
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (x) S 1"), None);
    }

    #[test]
    fn record_format_is_strict() {
        let good = r#"{"v":1,"run":"r1","pgid":100,"leader_pid":100,"leader_starttime":5,"boot_id":"b","marker":"m"}"#;
        assert!(parse_record(good.as_bytes()).is_some());
        for bad in [
            // Старый формат и чужие поля.
            r#"{"pgid":100,"starttime":0}"#,
            r#"{"v":1,"run":"r1","pgid":100,"leader_pid":100,"leader_starttime":5,"boot_id":"b","marker":"m","x":1}"#,
            // starttime 0 — неизвестен.
            r#"{"v":1,"run":"r1","pgid":100,"leader_pid":100,"leader_starttime":0,"boot_id":"b","marker":"m"}"#,
            // Другая версия, лидер не лидер группы, группа 1, пустые boot_id и маркер.
            r#"{"v":2,"run":"r1","pgid":100,"leader_pid":100,"leader_starttime":5,"boot_id":"b","marker":"m"}"#,
            r#"{"v":1,"run":"r1","pgid":100,"leader_pid":101,"leader_starttime":5,"boot_id":"b","marker":"m"}"#,
            r#"{"v":1,"run":"r1","pgid":1,"leader_pid":1,"leader_starttime":5,"boot_id":"b","marker":"m"}"#,
            r#"{"v":1,"run":"r1","pgid":100,"leader_pid":100,"leader_starttime":5,"boot_id":" ","marker":"m"}"#,
            r#"{"v":1,"run":"r1","pgid":100,"leader_pid":100,"leader_starttime":5,"boot_id":"b","marker":""}"#,
            "not json",
        ] {
            assert!(parse_record(bad.as_bytes()).is_none(), "{bad}");
        }
    }
}
