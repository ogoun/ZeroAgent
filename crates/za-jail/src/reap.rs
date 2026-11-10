//! Остатки деревьев, переживших агента (D84, Ruling 7.21): файл `{pgid, starttime}` на
//! вызов и перебор `/proc/*/stat` при старте и resume run. Процесс, ушедший через `setsid`
//! в свою группу, не находится — принятый риск OPS-013.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Каталог файлов остатка от корня проекта: `za_core::paths::work_dir(root, "procs")`.
const PROCS_DIR: [&str; 3] = [".zero-agent", "work", "procs"];

/// Запись файла остатка: группа и время старта её лидера (в тиках с загрузки).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Leftover {
    pub pgid: i32,
    pub starttime: u64,
}

/// `(pgrp, starttime)` из текста `/proc/<pid>/stat`. Имя процесса в скобках может содержать
/// пробелы и скобки, поэтому поля считаются от последней `)`: после неё идут поля 3 (`state`)
/// и далее; `pgrp` — поле 5, `starttime` — поле 22 (proc(5)).
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn parse_stat(text: &str) -> Option<(i32, u64)> {
    let rest = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let pgrp = fields.get(5 - 3)?.parse().ok()?;
    let starttime = fields.get(22 - 3)?.parse().ok()?;
    Some((pgrp, starttime))
}

/// См. [`crate::DirectRunner::reap_leftovers`].
pub(crate) fn reap_leftovers(root: &Path) -> usize {
    let dir = PROCS_DIR.iter().fold(root.to_path_buf(), |p, c| p.join(c));
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut killed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        match std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Leftover>(&b).ok())
        {
            Some(rec) => killed += kill_group(rec),
            None => tracing::warn!(file = %path.display(), "непонятный файл остатка"),
        }
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!(file = %path.display(), error = %e, "файл остатка не удалён");
        }
    }
    killed
}

/// Убивает процессы группы `rec.pgid`, стартовавшие не раньше её лидера.
#[cfg(unix)]
fn kill_group(rec: Leftover) -> usize {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let Ok(procs) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let mut killed = 0;
    for p in procs.flatten() {
        let Some(pid) = p.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(p.path().join("stat")) else {
            continue;
        };
        let Some((pgrp, starttime)) = parse_stat(&stat) else {
            continue;
        };
        if pgrp == rec.pgid && starttime >= rec.starttime {
            match kill(Pid::from_raw(pid), Signal::SIGKILL) {
                Ok(()) => {
                    tracing::info!(pid, pgid = rec.pgid, "убит остаток дерева");
                    killed += 1;
                }
                Err(e) => tracing::debug!(pid, error = %e, "остаток не убит"),
            }
        }
    }
    killed
}

/// Windows: Job закрывается вместе с агентом, остатков нет.
#[cfg(not(unix))]
fn kill_group(_rec: Leftover) -> usize {
    0
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
        assert_eq!(parse_stat(s), Some((4240, 987_654)));
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (x) S 1"), None);
    }
}
