//! Linux: `/proc`, subreaper, дерево по `ppid` и маркеру, сбор усыновлённых зомби
//! (D84; ревью задачи 5, I2).
//!
//! Потомок может уйти из группы лидера через `setpgid` (GNU `timeout`, оболочки с
//! управлением заданиями) или `setsid`; тогда `killpg` его не задевает. Поэтому:
//! - агент — subreaper (`PR_SET_CHILD_SUBREAPER`): осиротевшие потомки переходят к нему,
//!   а не к init;
//! - каждый процесс дерева наследует переменную [`MARKER_VAR`] со значением вызова;
//! - при уничтожении дерева убиваются группа, потомки лидера по `ppid` и усыновлённые
//!   агентом процессы с маркером вызова вместе с их потомками. Проходов несколько:
//!   потомки убитых переходят к агенту и попадают в следующий проход.
//!
//! Процесс, сбросивший окружение и ушедший из группы до смерти своего родителя, может
//! уцелеть. Это записанный риск D84.
//!
//! Subreaper сам собирает зомби усыновлённых процессов, но только тех, кого убил
//! [`kill_tree`] своего вызова: номер и `starttime` каждой цели запоминаются, и `waitpid`
//! идёт лишь по номеру, у которого сейчас тот же `starttime`, родитель — агент, состояние
//! `Z`. Детей процесса, запущенных мимо `Runner` (`std::process::Command`,
//! `tokio::process` в тестах и xtask), сбор не трогает — их статус получает владелец
//! (раунд 3 ревью задачи 5: перехват статуса давал ECHILD у `Command::output`). Лидеров,
//! которых ждёт tokio, сбор не трогает тоже: они не попадают в цели, а их номера ещё и
//! лежат в [`LEADERS`]. Сироты дерева, вышедшие сами до уничтожения дерева, остаются
//! зомби агента до его выхода (принятый риск D84).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill, killpg};
use nix::sys::wait::{WaitPidFlag, waitpid};
use nix::unistd::Pid;

use crate::reap::{Stat, parse_stat};

/// Переменная окружения с маркером дерева вызова.
pub(crate) const MARKER_VAR: &str = "ZA_JAIL_CALL";
/// Сколько раз обходить дерево при уничтожении.
const KILL_PASSES: usize = 4;

/// Сколько фоновый сборщик ждёт, пока убитые цели станут зомби агента.
const REAP_WINDOW: Duration = Duration::from_secs(5);
/// Шаг фонового сборщика.
const REAP_STEP: Duration = Duration::from_millis(20);

/// Лидеры, которых ждёт tokio (их зомби собирает он).
static LEADERS: Mutex<Vec<i32>> = Mutex::new(Vec::new());
/// Убитые цели, чьих зомби агент ещё не собрал: `(pid, starttime)`.
static PENDING: Mutex<Vec<(i32, u64)>> = Mutex::new(Vec::new());
/// Фоновый сборщик уже работает.
static REAPER: AtomicBool = AtomicBool::new(false);

/// Замок реестра лидеров: под ним запускается процесс и собираются зомби.
pub(crate) fn leaders() -> MutexGuard<'static, Vec<i32>> {
    LEADERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Убирает лидера из реестра: tokio его уже собрал или соберёт сам.
pub(crate) fn forget_leader(pid: i32) {
    leaders().retain(|p| *p != pid);
}

/// Делает процесс subreaper, один раз.
pub(crate) fn ensure_subreaper() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        if let Err(e) = nix::sys::prctl::set_child_subreaper(true) {
            tracing::warn!(error = %e, "PR_SET_CHILD_SUBREAPER не установлен");
        }
    });
}

/// `/proc/sys/kernel/random/boot_id` — случайный номер текущей загрузки.
pub(crate) fn boot_id() -> Option<String> {
    let s = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// `/proc/<pid>/stat` одного процесса.
pub(crate) fn stat(pid: i32) -> Option<Stat> {
    parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Снимок всех процессов.
pub(crate) fn snapshot() -> Vec<Stat> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(stat)
        .collect()
}

/// У процесса есть `ZA_JAIL_CALL=<marker>` (окружение на момент запуска процесса).
pub(crate) fn has_marker(pid: i32, marker: &str) -> bool {
    let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return false;
    };
    let needle = format!("{MARKER_VAR}={marker}");
    env.split(|b| *b == 0).any(|e| e == needle.as_bytes())
}

/// Процессы дерева: `roots`, их потомки по `ppid` и участники группы `pgid`; без самого
/// агента, init, зомби и процессов, стартовавших раньше `since` (тики с загрузки, 0 — без
/// ограничения): процесс старше лидера не может быть частью его дерева.
pub(crate) fn targets(procs: &[Stat], roots: &[i32], pgid: Option<i32>, since: u64) -> Vec<i32> {
    let me = std::process::id() as i32;
    let mut out: Vec<i32> = Vec::new();
    let mut queue: Vec<i32> = roots.to_vec();
    if let Some(g) = pgid {
        queue.extend(procs.iter().filter(|p| p.pgrp == g).map(|p| p.pid));
    }
    while let Some(pid) = queue.pop() {
        if pid <= 1 || pid == me || out.contains(&pid) {
            continue;
        }
        out.push(pid);
        queue.extend(procs.iter().filter(|p| p.ppid == pid).map(|p| p.pid));
    }
    out.retain(|pid| {
        procs
            .iter()
            .any(|p| p.pid == *pid && p.state != 'Z' && p.starttime >= since)
    });
    out
}

/// `SIGKILL` каждому; число принявших сигнал.
pub(crate) fn kill_all(pids: &[i32]) -> usize {
    pids.iter()
        .filter(|p| kill(Pid::from_raw(**p), Signal::SIGKILL).is_ok())
        .count()
}

/// Уничтожает дерево вызова: усыновлённые агентом процессы с маркером и их потомки; пока
/// лидер не собран tokio — ещё группа лидера и его потомки. После сбора номер лидера и
/// его группы мог достаться чужому процессу, поэтому группа не в счёт (ревью задачи 5,
/// N1). `since` — `starttime` лидера (0 — неизвестен). Затем собирает зомби усыновлённых.
pub(crate) fn kill_tree(leader: i32, marker: &str, leader_alive: bool, since: u64) {
    let me = std::process::id() as i32;
    if leader_alive {
        let _ = killpg(Pid::from_raw(leader), Signal::SIGKILL);
    }
    let mut killed: Vec<(i32, u64)> = Vec::new();
    for _ in 0..KILL_PASSES {
        let procs = snapshot();
        let mut roots = Vec::new();
        if leader_alive {
            roots.push(leader);
        }
        roots.extend(
            procs
                .iter()
                .filter(|p| p.ppid == me && p.pid != leader && has_marker(p.pid, marker))
                .map(|p| p.pid),
        );
        let group = leader_alive.then_some(leader);
        let live = targets(&procs, &roots, group, since);
        if live.is_empty() {
            break;
        }
        for pid in live {
            if kill(Pid::from_raw(pid), Signal::SIGKILL).is_ok()
                && pid != leader
                && let Some(p) = procs.iter().find(|p| p.pid == pid)
            {
                killed.push((pid, p.starttime));
            }
        }
    }
    reap_targets(killed);
}

/// Запоминает убитые цели и собирает их зомби; что не стало зомби сразу — собирает
/// фоновый поток в течение [`REAP_WINDOW`].
fn reap_targets(killed: Vec<(i32, u64)>) {
    if killed.is_empty() {
        return;
    }
    pending().extend(killed);
    if reap_pending() && !REAPER.swap(true, Ordering::AcqRel) {
        let spawned = std::thread::Builder::new()
            .name("za-jail-reaper".into())
            .spawn(|| {
                let start = Instant::now();
                while reap_pending() && start.elapsed() < REAP_WINDOW {
                    std::thread::sleep(REAP_STEP);
                }
                REAPER.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            REAPER.store(false, Ordering::Release);
        }
    }
}

fn pending() -> MutexGuard<'static, Vec<(i32, u64)>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Один проход сбора по запомненным целям. `waitpid` — только если номер принадлежит той
/// же цели (`starttime` совпал), её родитель — агент, она зомби и не лидер, которого ждёт
/// tokio. Пока такой зомби не собран, номер занят им и не может достаться чужому ребёнку.
/// Возвращает, остались ли цели в ожидании.
fn reap_pending() -> bool {
    let me = std::process::id() as i32;
    let leaders = leaders();
    let mut pending = pending();
    pending.retain(|&(pid, start)| match stat(pid) {
        // Цели нет или номер у другого процесса: собрал её родитель, нам нечего делать.
        None => false,
        Some(s) if s.starttime != start => false,
        // Ещё жива или родитель — не агент: ждём, пока станет нашим зомби.
        Some(s) if s.state != 'Z' || s.ppid != me => true,
        Some(_) if leaders.contains(&pid) => false,
        Some(_) => matches!(
            waitpid(Pid::from_raw(pid), Some(WaitPidFlag::WNOHANG)),
            Ok(nix::sys::wait::WaitStatus::StillAlive)
        ),
    });
    !pending.is_empty()
}

/// Уникальный маркер вызова в процессе: pid агента, время и счётчик.
pub(crate) fn new_marker() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}
