//! Запуск бинарника `zero-agent` из теста: чистые `ZA_HOME` и каталог проекта (свой у
//! каждого теста и процесса, [`unique_dir`]) внутри
//! `CARGO_TARGET_TMPDIR`, предел жизни процесса 60 с × `ZA_TEST_TIME_SCALE` (по
//! истечении процесс убивается, тест падает с понятной паникой), чтение журнала run.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;
use za_protocol::{EventKind, RunId, Seq};
use za_store::ProjectStore;
use zero_agent::home::project_id;
use zero_agent::journal::open_retrying;

/// Множитель времени тестов: `ZA_TEST_TIME_SCALE` (целое 1–6, по умолчанию 1).
pub fn time_scale() -> u32 {
    static SCALE: OnceLock<u32> = OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("ZA_TEST_TIME_SCALE")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map_or(1, |n| n.clamp(1, 6))
    })
}

/// Предел жизни процесса бинарника.
pub fn process_limit() -> Duration {
    Duration::from_secs(60) * time_scale()
}

pub struct Env {
    pub home: PathBuf,
    pub project: PathBuf,
}

#[path = "../../../../test-support/unique_dir.rs"]
mod unique;

/// Чистый каталог теста `<CARGO_TARGET_TMPDIR>/<group>/<name>-<pid>-<n>` — общий помощник
/// workspace (`test-support/unique_dir.rs`).
pub use unique::unique_dir;

/// Чистые `ZA_HOME` и каталог проекта в [`unique_dir`]; `config` — текст `config.toml`
/// (пустой — без файла).
pub fn env_in(group: &str, name: &str, config: &str) -> Env {
    let root = unique_dir(group, name);
    let e = Env {
        home: root.join("home"),
        project: root.join("project"),
    };
    std::fs::create_dir_all(&e.home).unwrap();
    std::fs::create_dir_all(&e.project).unwrap();
    if !config.is_empty() {
        std::fs::write(e.home.join("config.toml"), config).unwrap();
    }
    e
}

/// Итог процесса бинарника.
#[derive(Debug)]
pub struct Out {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn code(&self) -> Option<i32> {
        self.status.code()
    }
}

fn read_all(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        buf
    })
}

impl Env {
    pub fn project_arg(&self) -> &str {
        self.project.to_str().unwrap()
    }

    /// Запуск бинарника с пределом [`process_limit`]; `LLAMA_URL`, `ZA_LOG`, `RUST_LOG`
    /// и ключи окружения тестов не наследуются. Кэши cargo и uv (категории монитора
    /// ресурсов) — пустые каталоги теста.
    pub fn run(&self, args: &[&str]) -> Out {
        self.run_with(args, &[])
    }

    /// [`Env::run`] с дополнительными переменными окружения.
    pub fn run_with(&self, args: &[&str], vars: &[(&str, &str)]) -> Out {
        let caches = self.home.with_extension("caches");
        let _ = std::fs::create_dir_all(caches.join("cargo"));
        let _ = std::fs::create_dir_all(caches.join("uv"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_zero-agent"));
        cmd.args(args)
            .env("CARGO_HOME", caches.join("cargo"))
            .env("UV_CACHE_DIR", caches.join("uv"))
            .env("ZA_HOME", &self.home)
            .env_remove("LLAMA_URL")
            .env_remove("ZA_LOG")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, _) in std::env::vars() {
            if k.starts_with("ZA_SECRET_") {
                cmd.env_remove(k);
            }
        }
        for (k, v) in vars {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let out = read_all(child.stdout.take().unwrap());
        let err = read_all(child.stderr.take().unwrap());
        let until = Instant::now() + process_limit();
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            if Instant::now() >= until {
                let _ = child.kill();
                let _ = child.wait();
                let err = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
                panic!(
                    "zero-agent {args:?} не завершился за {:?} и убит; stderr: {err}",
                    process_limit()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        Out {
            status,
            stdout: String::from_utf8(out.join().unwrap()).unwrap(),
            stderr: String::from_utf8(err.join().unwrap()).unwrap(),
        }
    }

    /// `run --project <dir>` и аргументы.
    pub fn run_cmd(&self, extra: &[&str]) -> Out {
        let mut args = vec!["run", "--project", self.project_arg()];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    /// `answer --project <dir> --interrupt <id> --text <text>`.
    pub fn answer(&self, interrupt: &str, text: &str) -> Out {
        self.run(&[
            "answer",
            "--project",
            self.project_arg(),
            "--interrupt",
            interrupt,
            "--text",
            text,
        ])
    }

    pub fn journal_dir(&self) -> PathBuf {
        self.home
            .join("projects")
            .join(project_id(&self.project).unwrap())
    }

    pub fn store(&self) -> ProjectStore {
        open_retrying(&self.journal_dir()).unwrap()
    }

    /// События run по порядку `seq`.
    pub fn events(&self, run: RunId) -> Vec<(EventKind, Value)> {
        let store = self.store();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(store.read_run(run, Seq(0), usize::MAX))
            .unwrap()
            .into_iter()
            .map(|e| (e.kind, e.payload))
            .collect()
    }
}

/// id run из первой строки stderr `run <id>`.
pub fn run_id(o: &Out) -> RunId {
    let first = o.stderr.lines().next().unwrap_or_default();
    let id = first
        .strip_prefix("run ")
        .unwrap_or_else(|| panic!("первая строка stderr не `run <id>`: {o:?}"));
    RunId(uuid::Uuid::parse_str(id.trim()).unwrap())
}

/// id прерывания из подсказки `answer --interrupt <id>` в stderr.
pub fn interrupt_id(o: &Out) -> String {
    let at = o
        .stderr
        .find("--interrupt ")
        .unwrap_or_else(|| panic!("нет подсказки answer: {o:?}"));
    o.stderr[at + "--interrupt ".len()..]
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned()
}

pub fn count(events: &[(EventKind, Value)], kind: EventKind) -> usize {
    events.iter().filter(|(k, _)| *k == kind).count()
}

/// `needle` — подпоследовательность `hay`.
pub fn is_subsequence(hay: &[&str], needle: &[&str]) -> bool {
    let mut it = hay.iter();
    needle.iter().all(|n| it.any(|h| h == n))
}

pub fn journal_exists(e: &Env) -> bool {
    Path::new(&e.journal_dir()).exists()
}
