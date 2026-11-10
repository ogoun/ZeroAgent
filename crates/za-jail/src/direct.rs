//! `DirectRunner` — запуск без ОС-изоляции (D84): дерево процессов, потоковый вывод с
//! пределом, тайм-аут, фоновые потомки, убийство дерева при броске future.

use std::path::Path;
use std::pin::pin;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::{Instant, sleep_until};

use crate::{BoxFuture, CommandSpec, Exit, KILL_GRACE, Output, Runner, RunnerError, child_env};

/// Размер куска потокового чтения вывода.
const CHUNK: usize = 64 * 1024;
/// Срок вместо тайм-аута, который не помещается в `Instant` (`Duration::MAX`).
const FAR: Duration = Duration::from_secs(10 * 365 * 24 * 3600);

/// Запуск без изоляции: Windows — Job Object с `KILL_ON_JOB_CLOSE` и `CREATE_NO_WINDOW`,
/// Unix — процесс лидер своей группы (на Linux ещё subreaper и маркер дерева); бросок
/// future убивает всё дерево.
#[derive(Debug, Clone, Copy, Default)]
pub struct DirectRunner;

impl DirectRunner {
    /// Убивает остатки деревьев, переживших агента (Linux, D84): по записям реестра
    /// групп `dir` (`ZA_HOME/projects/<project_id>/procs`, создаёт вызывающий через
    /// `za_core::paths::home_dir`). Читаются только обычные файлы своего формата; сигнал —
    /// только при совпадении `boot_id` и `starttime` лидера (см. модуль `reap`).
    /// Разобранные записи удаляются, чужие файлы не трогаются. Возвращает число процессов,
    /// получивших `SIGKILL`; на прочих ОС — сразу 0 (Windows держит дерево Job Object).
    ///
    /// Блокирующий ввод-вывод: из `async` — через `spawn_blocking`.
    pub fn reap_leftovers(dir: &Path) -> usize {
        crate::reap::reap_leftovers(dir)
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

/// Итог запуска до декодирования вывода.
struct Raw {
    exit: Exit,
    out: Capture,
    err: Capture,
    background_killed: bool,
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
    let limit = spec.capture_limit;
    let raw = run_raw(spec).await?;
    let oem = if crate::decode::needs_code_page(&raw.out.buf)
        || crate::decode::needs_code_page(&raw.err.buf)
    {
        console_code_page().await
    } else {
        None
    };
    Ok(Output {
        exit: raw.exit,
        stdout: crate::decode::decode(&raw.out.buf, limit, oem),
        stderr: crate::decode::decode(&raw.err.buf, limit, oem),
        stdout_truncated: raw.out.truncated,
        stderr_truncated: raw.err.truncated,
        background_killed: raw.background_killed,
        duration: started.elapsed(),
    })
}

/// Кодовая страница консоли дочерних процессов (Windows): они получают свою скрытую
/// консоль (`CREATE_NO_WINDOW`), как и `chcp`, запущенный так же. Один раз на процесс.
#[cfg(windows)]
async fn console_code_page() -> Option<u32> {
    static CP: tokio::sync::OnceCell<Option<u32>> = tokio::sync::OnceCell::const_new();
    *CP.get_or_init(|| async {
        let cwd = std::env::current_dir().ok()?;
        let spec = CommandSpec::new("chcp.com", cwd, Duration::from_secs(10));
        let raw = run_raw(spec).await.ok()?;
        let cp = crate::decode::last_number(&raw.out.buf);
        tracing::debug!(?cp, "кодовая страница консоли");
        cp
    })
    .await
}

/// Вне Windows вывод — UTF-8.
#[cfg(not(windows))]
async fn console_code_page() -> Option<u32> {
    None
}

async fn run_raw(spec: CommandSpec) -> Result<Raw, RunnerError> {
    let started = Instant::now();
    let base = spec
        .base_env
        .clone()
        .unwrap_or_else(|| std::env::vars_os().collect());
    #[allow(unused_mut)]
    let mut env = child_env(base.into_iter(), &spec, cfg!(windows));
    #[cfg(target_os = "linux")]
    let marker = {
        crate::linux::ensure_subreaper();
        let m = crate::linux::new_marker();
        env.push((crate::linux::MARKER_VAR.into(), m.clone().into()));
        m
    };
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
    let spawned = {
        // Linux: запуск и регистрация лидера — под замком сбора зомби, чтобы субрипер не
        // собрал лидера, которого ждёт tokio.
        #[cfg(target_os = "linux")]
        let mut leaders = crate::linux::leaders();
        let r = cmd.spawn();
        #[cfg(target_os = "linux")]
        if let Some(pid) = r.as_ref().ok().and_then(|c| c.id()) {
            leaders.push(pid as i32);
        }
        r
    };
    let mut child = spawned.map_err(|source| RunnerError::Spawn {
        program: spec.program.display().to_string(),
        source,
    })?;
    // Страж дерева: бросок future убивает всё дерево и убирает запись реестра. Объявлен
    // после `child`, поэтому срабатывает раньше, чем tokio соберёт лидера.
    #[cfg(target_os = "linux")]
    let mut guard = unix::Guard::new(child.id(), marker);
    #[cfg(all(unix, not(target_os = "linux")))]
    let mut guard = unix::Guard::new(child.id());
    #[cfg(windows)]
    let mut guard = windows_guard::Guard;
    #[cfg(target_os = "linux")]
    if let Some(left) = &spec.leftover {
        guard.write_leftover(left).await.map_err(RunnerError::Io)?;
    }

    let stdout = child.stdout().take();
    let stderr = child.stderr().take();
    let mut out = Capture::default();
    let mut err = Capture::default();
    let deadline = started
        .checked_add(spec.timeout)
        .unwrap_or_else(|| started + FAR);
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
            // Инвариант: `inner_mut()` — сам tokio `Child` главного процесса, а не обёртка
            // Job или группы (их `wait()` ждёт всех потомков). Так устроен порядок обёрток
            // process-wrap: `CreationFlags` и `KillOnDrop` child не оборачивают. Нарушение
            // ловит тест `background_child_does_not_hold_call`.
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
                    guard.leader_reaped();
                    if !killed {
                        // Фоновые потомки держат каналы: EOF ждётся не дольше KILL_GRACE.
                        drain = Some(Instant::now() + KILL_GRACE);
                    }
                }
                // Страж убьёт дерево при выходе из функции.
                Event::Exited(Err(e)) => return Err(RunnerError::Io(e)),
                Event::OutDone => out_done = true,
                Event::ErrDone => err_done = true,
                Event::Deadline => {
                    tracing::debug!(program = %spec.program.display(), "тайм-аут, дерево убивается");
                    guard.kill(child.as_mut());
                    timed_out = true;
                    killed = true;
                    drain = Some(Instant::now() + KILL_GRACE);
                }
                Event::Drain if !killed => {
                    tracing::debug!(program = %spec.program.display(), "фоновые потомки держат вывод, дерево убивается");
                    guard.kill(child.as_mut());
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
        guard.kill(child.as_mut());
    }
    guard.finish().await;
    let exit = match status {
        _ if timed_out => Exit::Timeout,
        Some(s) => exit_of(s),
        // Без статуса цикл выходит только после убийства по тайм-ауту или фоновым потомкам,
        // а во втором случае статус уже есть.
        None => Exit::Timeout,
    };
    Ok(Raw {
        exit,
        out,
        err,
        background_killed,
    })
}

/// Убивает Job или группу через process-wrap: `TerminateJobObject` или `killpg(SIGKILL)`.
/// Ошибка (группа уже пуста) не важна.
fn start_kill(child: &mut dyn ChildWrapper) {
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

/// Windows: дерево держит Job Object, закрытие описателя вместе с `child` убивает его.
#[cfg(windows)]
mod windows_guard {
    use process_wrap::tokio::ChildWrapper;

    pub(super) struct Guard;

    impl Guard {
        pub(super) fn leader_reaped(&mut self) {}

        pub(super) fn kill(&mut self, child: &mut dyn ChildWrapper) {
            super::start_kill(child);
        }

        pub(super) async fn finish(self) {}
    }
}

#[cfg(unix)]
mod unix {
    #[cfg(target_os = "linux")]
    use std::path::PathBuf;

    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    use process_wrap::tokio::ChildWrapper;

    /// Страж дерева на Unix: бросок future убивает группу (у `ProcessGroupChild` нет
    /// `Drop`, а `kill_on_drop` tokio убивает только лидера), на Linux — и ушедших из неё
    /// потомков; удаляет запись реестра и снимает лидера с учёта.
    pub(super) struct Guard {
        leader: Option<i32>,
        armed: bool,
        leader_alive: bool,
        #[cfg(target_os = "linux")]
        marker: String,
        /// `starttime` лидера; 0 — неизвестен.
        #[cfg(target_os = "linux")]
        since: u64,
        #[cfg(target_os = "linux")]
        file: Option<PathBuf>,
    }

    impl Guard {
        /// Группа лидера `pid` (`ProcessGroup::leader()`: pgid == pid).
        #[cfg(target_os = "linux")]
        pub(super) fn new(pid: Option<u32>, marker: String) -> Self {
            let leader = pid.and_then(|p| i32::try_from(p).ok());
            let since = leader
                .and_then(crate::linux::stat)
                .map_or(0, |s| s.starttime);
            Self {
                leader,
                armed: true,
                leader_alive: true,
                marker,
                since,
                file: None,
            }
        }

        #[cfg(not(target_os = "linux"))]
        pub(super) fn new(pid: Option<u32>) -> Self {
            Self {
                leader: pid.and_then(|p| i32::try_from(p).ok()),
                armed: true,
                leader_alive: true,
            }
        }

        /// Пишет запись реестра (D84). Без `starttime` лидера или `boot_id` запись не
        /// пишется: такую группу потом не отличить от чужой.
        #[cfg(target_os = "linux")]
        pub(super) async fn write_leftover(
            &mut self,
            left: &crate::Leftover,
        ) -> std::io::Result<()> {
            let (Some(leader), Some(boot_id)) = (self.leader, crate::linux::boot_id()) else {
                tracing::warn!("нет лидера или boot_id, запись реестра групп не пишется");
                return Ok(());
            };
            if self.since == 0 {
                tracing::warn!("нет starttime лидера, запись реестра групп не пишется");
                return Ok(());
            }
            let rec = crate::reap::Record {
                v: crate::reap::RECORD_VERSION,
                run: left.run.clone(),
                pgid: leader,
                leader_pid: leader,
                leader_starttime: self.since,
                boot_id,
                marker: self.marker.clone(),
            };
            let text = serde_json::to_vec(&rec).map_err(std::io::Error::other)?;
            tokio::fs::write(&left.file, text).await?;
            self.file = Some(left.file.clone());
            Ok(())
        }

        /// Tokio собрал лидера: его номер больше не наш.
        pub(super) fn leader_reaped(&mut self) {
            self.leader_alive = false;
            #[cfg(target_os = "linux")]
            if let Some(l) = self.leader {
                crate::linux::forget_leader(l);
            }
        }

        /// Убивает дерево.
        pub(super) fn kill(&mut self, child: &mut dyn ChildWrapper) {
            super::start_kill(child);
            self.kill_rest();
        }

        /// Linux: ушедшие из группы потомки и усыновлённые процессы с маркером.
        fn kill_rest(&self) {
            let Some(leader) = self.leader else {
                return;
            };
            #[cfg(target_os = "linux")]
            crate::linux::kill_tree(leader, &self.marker, self.leader_alive, self.since);
            #[cfg(not(target_os = "linux"))]
            let _ = killpg(Pid::from_raw(leader), Signal::SIGKILL);
        }

        /// Штатное завершение: дерево уже убито, запись реестра удаляется.
        pub(super) async fn finish(mut self) {
            self.armed = false;
            #[cfg(target_os = "linux")]
            if let Some(file) = self.file.take()
                && let Err(e) = tokio::fs::remove_file(&file).await
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(file = %file.display(), error = %e, "запись реестра групп не удалена");
            }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if self.armed
                && let Some(leader) = self.leader
            {
                let _ = killpg(Pid::from_raw(leader), Signal::SIGKILL);
                self.kill_rest();
            }
            #[cfg(target_os = "linux")]
            {
                // Короткая синхронная операция в `Drop` (ревью задачи 5, I1): иначе запись
                // отменённого вызова сработала бы позже на чужую группу с тем же номером.
                if let Some(file) = self.file.take()
                    && let Err(e) = std::fs::remove_file(&file)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(file = %file.display(), error = %e, "запись реестра групп не удалена");
                }
                if self.leader_alive
                    && let Some(l) = self.leader
                {
                    crate::linux::forget_leader(l);
                }
            }
        }
    }
}
