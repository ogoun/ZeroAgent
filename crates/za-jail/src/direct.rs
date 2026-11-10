//! `DirectRunner` — запуск без ОС-изоляции (D84): дерево процессов, потоковый вывод с
//! пределом, тайм-аут, фоновые потомки, убийство дерева при броске future.

use std::path::Path;
use std::pin::pin;
use std::process::{ExitStatus, Stdio};

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::{Instant, sleep_until};

use crate::{BoxFuture, CommandSpec, Exit, KILL_GRACE, Output, Runner, RunnerError, child_env};

/// Размер куска потокового чтения вывода.
const CHUNK: usize = 64 * 1024;

/// Запуск без изоляции: Windows — Job Object с `KILL_ON_JOB_CLOSE` и `CREATE_NO_WINDOW`,
/// Unix — процесс лидер своей группы; бросок future убивает всю группу или Job.
#[derive(Debug, Clone, Copy, Default)]
pub struct DirectRunner;

impl DirectRunner {
    /// Убивает остатки деревьев, переживших агента (Unix, D84, Ruling 7.21): по файлам
    /// `<root>/.zero-agent/work/procs/*.json` процессы с `pgrp == pgid` и `starttime` не
    /// раньше записанного получают `SIGKILL` (лидер может быть уже мёртв). Файлы удаляются.
    /// Возвращает число убитых процессов; на Windows — 0 (Job закрывается вместе с агентом).
    ///
    /// Блокирующий ввод-вывод: из `async` — через `spawn_blocking`.
    pub fn reap_leftovers(root: &Path) -> usize {
        crate::reap::reap_leftovers(root)
    }
}

impl Runner for DirectRunner {
    fn run(&self, spec: CommandSpec) -> BoxFuture<'static, Result<Output, RunnerError>> {
        Box::pin(run(spec))
    }
}

/// Захваченный поток вывода.
#[derive(Default)]
struct Capture {
    buf: Vec<u8>,
    truncated: bool,
}

/// Событие цикла ожидания.
enum Event {
    Exited(std::io::Result<ExitStatus>),
    OutDone,
    ErrDone,
    Deadline,
    Drain,
}

async fn run(spec: CommandSpec) -> Result<Output, RunnerError> {
    let started = Instant::now();
    let base = spec
        .base_env
        .clone()
        .unwrap_or_else(|| std::env::vars_os().collect());
    let env = child_env(base.into_iter(), &spec, cfg!(windows));
    let mut cmd = CommandWrap::with_new(&spec.program, |c| {
        c.args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    });
    #[cfg(windows)]
    {
        use process_wrap::tokio::{CreationFlags, JobObject};
        use windows::Win32::System::Threading::CREATE_NO_WINDOW;
        cmd.wrap(CreationFlags(CREATE_NO_WINDOW));
        cmd.wrap(JobObject);
    }
    #[cfg(unix)]
    {
        use process_wrap::tokio::ProcessGroup;
        cmd.wrap(ProcessGroup::leader());
    }
    // Обязательна: без неё Job Object не получает `KILL_ON_JOB_CLOSE` (D73).
    cmd.wrap(KillOnDrop);
    let mut child = cmd.spawn().map_err(|source| RunnerError::Spawn {
        program: spec.program.display().to_string(),
        source,
    })?;
    // Unix: бросок future убивает группу (у `ProcessGroupChild` нет своего `Drop`, а
    // `kill_on_drop` tokio убивает только лидера). Windows: Job закрывается вместе с `child`.
    #[cfg(unix)]
    let mut group = unix::GroupGuard::new(child.id());
    #[cfg(unix)]
    if let Some(file) = &spec.leftover_file
        && let Err(e) = unix::write_leftover(file, child.id()).await
    {
        let _ = child.start_kill();
        return Err(RunnerError::Io(e));
    }

    let stdout = child.stdout().take();
    let stderr = child.stderr().take();
    let mut out = Capture::default();
    let mut err = Capture::default();
    let deadline = started + spec.timeout;
    let mut status: Option<ExitStatus> = None;
    let mut timed_out = false;
    let mut background_killed = false;
    let mut killed = false;
    {
        let mut out_fut = pin!(read_capped(stdout, spec.capture_limit, &mut out));
        let mut err_fut = pin!(read_capped(stderr, spec.capture_limit, &mut err));
        let (mut out_done, mut err_done) = (false, false);
        // Предел ожидания EOF после выхода главного процесса или после убийства дерева.
        let mut drain: Option<Instant> = None;
        loop {
            if out_done && err_done && status.is_some() {
                break;
            }
            let drain_at = drain.unwrap_or(deadline);
            let event = tokio::select! {
                r = child.inner_mut().wait(), if status.is_none() => Event::Exited(r),
                () = &mut out_fut, if !out_done => Event::OutDone,
                () = &mut err_fut, if !err_done => Event::ErrDone,
                () = sleep_until(deadline), if status.is_none() && !killed => Event::Deadline,
                () = sleep_until(drain_at), if drain.is_some() => Event::Drain,
            };
            match event {
                Event::Exited(Ok(s)) => {
                    status = Some(s);
                    if !killed {
                        // Фоновые потомки держат каналы: EOF ждётся не дольше KILL_GRACE.
                        drain = Some(Instant::now() + KILL_GRACE);
                    }
                }
                Event::Exited(Err(e)) => {
                    let _ = child.start_kill();
                    return Err(RunnerError::Io(e));
                }
                Event::OutDone => out_done = true,
                Event::ErrDone => err_done = true,
                Event::Deadline => {
                    tracing::debug!(program = %spec.program.display(), "тайм-аут, дерево убивается");
                    kill_tree(child.as_mut());
                    timed_out = true;
                    killed = true;
                    drain = Some(Instant::now() + KILL_GRACE);
                }
                Event::Drain if !killed => {
                    tracing::debug!(program = %spec.program.display(), "фоновые потомки держат вывод, дерево убивается");
                    kill_tree(child.as_mut());
                    background_killed = true;
                    killed = true;
                    drain = Some(Instant::now() + KILL_GRACE);
                }
                // Дочитывание после убийства не уложилось в KILL_GRACE: что прочитано — то есть.
                Event::Drain => break,
            }
        }
    }
    // Потомки, закрывшие вывод и не ушедшие сами, не переживают вызов (на Windows Job
    // закрылся бы и так вместе с `child`).
    if !killed {
        kill_tree(child.as_mut());
    }
    #[cfg(unix)]
    group.disarm();
    #[cfg(unix)]
    if let Some(file) = &spec.leftover_file
        && let Err(e) = tokio::fs::remove_file(file).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(file = %file.display(), error = %e, "файл остатка не удалён");
    }
    let exit = match status {
        _ if timed_out => Exit::Timeout,
        Some(s) => exit_of(s),
        // Без статуса цикл выходит только после убийства по тайм-ауту или фоновым потомкам,
        // а во втором случае статус уже есть.
        None => Exit::Timeout,
    };
    Ok(Output {
        exit,
        stdout: decode(&out.buf, spec.capture_limit),
        stderr: decode(&err.buf, spec.capture_limit),
        stdout_truncated: out.truncated,
        stderr_truncated: err.truncated,
        background_killed,
        duration: started.elapsed(),
    })
}

/// Убивает всё дерево: `TerminateJobObject` или `killpg(SIGKILL)`. Ошибка (группа уже пуста)
/// не важна.
fn kill_tree(child: &mut dyn ChildWrapper) {
    if let Err(e) = child.start_kill() {
        tracing::debug!(error = %e, "дерево уже завершено");
    }
}

/// Читает поток до EOF: первые `limit` байт — в `cap`, остаток отбрасывается с пометкой.
/// Ошибка чтения — конец потока (канал закрыт убитым процессом).
async fn read_capped<R: AsyncRead + Unpin>(r: Option<R>, limit: usize, cap: &mut Capture) {
    let Some(mut r) = r else {
        return;
    };
    let mut chunk = vec![0u8; CHUNK];
    loop {
        let n = match r.read(&mut chunk).await {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) => {
                tracing::debug!(error = %e, "чтение вывода прервано");
                return;
            }
        };
        let take = n.min(limit.saturating_sub(cap.buf.len()));
        cap.buf.extend_from_slice(&chunk[..take]);
        if take < n {
            cap.truncated = true;
        }
    }
}

/// `from_utf8_lossy`, `\r\n` → `\n`; замены недопустимых байтов не выводят за `limit`.
fn decode(buf: &[u8], limit: usize) -> String {
    let mut s = String::from_utf8_lossy(buf).replace("\r\n", "\n");
    if s.len() > limit {
        let mut cut = limit;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

fn exit_of(s: ExitStatus) -> Exit {
    if let Some(code) = s.code() {
        return Exit::Code(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = s.signal() {
            return Exit::Signal(sig);
        }
    }
    // Без кода и сигнала (не бывает на поддерживаемых ОС).
    Exit::Code(-1)
}

#[cfg(unix)]
mod unix {
    use std::path::Path;

    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;

    use crate::reap::{Leftover, parse_stat};

    /// Убивает группу процессов при броске future.
    pub(super) struct GroupGuard {
        pgid: Option<i32>,
    }

    impl GroupGuard {
        /// Группа лидера `pid` (`ProcessGroup::leader()`: pgid == pid).
        pub(super) fn new(pid: Option<u32>) -> Self {
            Self {
                pgid: pid.and_then(|p| i32::try_from(p).ok()),
            }
        }

        /// Дерево уже убито штатно.
        pub(super) fn disarm(&mut self) {
            self.pgid = None;
        }
    }

    impl Drop for GroupGuard {
        fn drop(&mut self) {
            if let Some(pgid) = self.pgid {
                let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
            }
        }
    }

    /// Пишет `{pgid, starttime}` лидера в `file` (D84). Без `/proc` (не Linux) файл не
    /// пишется: без времени старта нельзя отличить группу от чужой с тем же номером.
    pub(super) async fn write_leftover(file: &Path, pid: Option<u32>) -> std::io::Result<()> {
        let Some(pid) = pid else {
            return Ok(());
        };
        let stat = match tokio::fs::read_to_string(format!("/proc/{pid}/stat")).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "нет /proc/<pid>/stat, файл остатка не пишется");
                return Ok(());
            }
        };
        let Some((pgrp, starttime)) = parse_stat(&stat) else {
            tracing::warn!("непонятный /proc/<pid>/stat, файл остатка не пишется");
            return Ok(());
        };
        let rec = Leftover {
            pgid: pgrp,
            starttime,
        };
        let text = serde_json::to_vec(&rec).map_err(std::io::Error::other)?;
        tokio::fs::write(file, text).await
    }
}
