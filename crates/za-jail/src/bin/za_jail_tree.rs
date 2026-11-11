//! Тестовый помощник `za-jail`: дерево процессов из самого себя, без `sleep` и оболочки
//! (одинаково на Windows и Unix).
//!
//! Каждый процесс печатает `pid <n>` в stdout и, если задан `--pid-file`, дописывает ту же
//! строку в файл. Флаги:
//! - `--depth N` — запустить себя с `--depth N-1` (потомки наследуют stdout и stderr);
//! - `--hold` — не завершаться (держит унаследованные каналы);
//! - `--background` — запустить потомков с `--hold` и выйти, не дожидаясь их; с
//!   `--pid-file` — после того как в файле появятся строки всех `N+1` процессов;
//! - `--newpgrp` — Unix: потомок уходит в свою группу (`setpgid`, как GNU `timeout`);
//! - `--exit C` — код выхода (после ожидания потомка);
//! - `--spam BYTES` — напечатать BYTES байт `x` в stdout и выйти;
//! - `--cat` — скопировать stdin в stdout байт в байт и выйти;
//! - `--runner FILE` — стать родителем: через `DirectRunner` запустить себя с остальными
//!   флагами и записью реестра групп `FILE`, ждать без конца.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use za_jail::{CommandSpec, DirectRunner, Leftover, Runner};

/// Сколько `--background` ждёт строки потомков в `--pid-file`.
const BACKGROUND_WAIT: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Args {
    depth: u32,
    hold: bool,
    background: bool,
    newpgrp: bool,
    exit: u8,
    spam: Option<usize>,
    cat: bool,
    pid_file: Option<PathBuf>,
    runner: Option<PathBuf>,
}

fn parse() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag}: нет значения"));
        match flag.as_str() {
            "--depth" => a.depth = value()?.parse().map_err(|e| format!("--depth: {e}"))?,
            "--hold" => a.hold = true,
            "--cat" => a.cat = true,
            "--background" => a.background = true,
            "--newpgrp" => a.newpgrp = true,
            "--exit" => a.exit = value()?.parse().map_err(|e| format!("--exit: {e}"))?,
            "--spam" => a.spam = Some(value()?.parse().map_err(|e| format!("--spam: {e}"))?),
            "--pid-file" => a.pid_file = Some(value()?.into()),
            "--runner" => a.runner = Some(value()?.into()),
            other => return Err(format!("неизвестный флаг {other}")),
        }
    }
    Ok(a)
}

/// Флаги дерева уровнем ниже (`depth` — глубина потомка).
fn tree_args(a: &Args, depth: u32, hold: bool) -> Vec<String> {
    let mut v = vec!["--depth".to_string(), depth.to_string()];
    if hold {
        v.push("--hold".into());
    }
    if a.newpgrp {
        v.push("--newpgrp".into());
    }
    v.push("--exit".into());
    v.push(a.exit.to_string());
    if let Some(p) = &a.pid_file {
        v.push("--pid-file".into());
        v.push(p.display().to_string());
    }
    v
}

fn announce(a: &Args) -> std::io::Result<()> {
    let line = format!("pid {}\n", std::process::id());
    let mut out = std::io::stdout().lock();
    out.write_all(line.as_bytes())?;
    out.flush()?;
    if let Some(p) = &a.pid_file {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)?;
        f.write_all(line.as_bytes())?;
    }
    Ok(())
}

fn hold_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Ждёт `n` строк `pid` в файле (не дольше [`BACKGROUND_WAIT`]).
fn wait_lines(file: &Path, n: usize) {
    let start = Instant::now();
    while start.elapsed() < BACKGROUND_WAIT {
        let lines = std::fs::read_to_string(file)
            .map(|t| t.lines().filter(|l| l.starts_with("pid ")).count())
            .unwrap_or(0);
        if lines >= n {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn run_as_runner(a: &Args, leftover: PathBuf) -> Result<ExitCode, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut spec = CommandSpec::new(exe, cwd, Duration::from_secs(24 * 3600));
    // Дерево под родителем — те же флаги без `--runner`.
    spec.args = tree_args(a, a.depth, a.hold)
        .into_iter()
        .map(Into::into)
        .collect();
    spec.leftover = Some(Leftover {
        file: leftover,
        run: "test-run".into(),
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let out = rt
        .block_on(DirectRunner.run(spec))
        .map_err(|e| e.to_string())?;
    println!("{out:?}");
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match real_main() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("za_jail_tree: {e}");
            ExitCode::from(101)
        }
    }
}

fn spawn_child(a: &Args) -> Result<std::process::Child, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.args(tree_args(a, a.depth - 1, a.hold || a.background));
    #[cfg(unix)]
    if a.newpgrp {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map_err(|e| e.to_string())
}

fn real_main() -> Result<ExitCode, String> {
    let a = parse()?;
    if let Some(leftover) = a.runner.clone() {
        return run_as_runner(&a, leftover);
    }
    if a.cat {
        let mut out = std::io::stdout().lock();
        std::io::copy(&mut std::io::stdin().lock(), &mut out).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(n) = a.spam {
        let mut out = std::io::stdout().lock();
        let block = vec![b'x'; 64 * 1024];
        let mut left = n;
        while left > 0 {
            let k = left.min(block.len());
            out.write_all(&block[..k]).map_err(|e| e.to_string())?;
            left -= k;
        }
        out.flush().map_err(|e| e.to_string())?;
        return Ok(ExitCode::SUCCESS);
    }
    announce(&a).map_err(|e| e.to_string())?;
    let child = if a.depth > 0 {
        Some(spawn_child(&a)?)
    } else {
        None
    };
    if a.background {
        if let Some(p) = &a.pid_file {
            wait_lines(p, a.depth as usize + 1);
        }
        return Ok(ExitCode::from(a.exit));
    }
    if a.hold {
        hold_forever();
    }
    if let Some(mut c) = child {
        c.wait().map_err(|e| e.to_string())?;
    }
    Ok(ExitCode::from(a.exit))
}
