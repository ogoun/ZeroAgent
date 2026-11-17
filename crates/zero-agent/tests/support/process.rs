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
/// Чистый каталог `<root>/<name>-<pid>-<n>` (git-репозитории тестов в `.tmp/r1c1-git`).
#[allow(unused_imports)]
pub use unique::unique_dir_in;

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

/// Префиксы переменных окружения разработчика, которых бинарник теста не наследует: ключи
/// `ZA_SECRET_*`, токен памяти агента `ZA_MEMORY_*` и переменные клиента `zam` (`ZAM_*`,
/// R1-M D159) — как `za_jail::child_env` у детей агента.
const PRIVATE_PREFIXES: [&str; 3] = ["ZA_SECRET_", "ZA_MEMORY_", "ZAM_"];

/// Снимает с `cmd` переменные окружения процесса теста с префиксами
/// [`PRIVATE_PREFIXES`]: токен владельца не уходит поддельному серверу и не ломает
/// проверки «токена нет». Один помощник у всех запускателей бинарника в тестах (Minor 1
/// ревью задачи 1 R1-M).
pub fn strip_private_env(cmd: &mut Command) -> &mut Command {
    strip_private_env_of(cmd, std::env::vars_os().map(|(k, _)| k))
}

/// [`strip_private_env`] по списку имён `names`: префикс без учёта регистра ASCII на любой
/// ОС (`zam_token` — тоже), `ZAMBA_X` и `ZAM` остаются.
pub fn strip_private_env_of(
    cmd: &mut Command,
    names: impl IntoIterator<Item = std::ffi::OsString>,
) -> &mut Command {
    for k in names {
        let up = k.to_string_lossy().to_ascii_uppercase();
        if PRIVATE_PREFIXES.iter().any(|p| up.starts_with(p)) {
            cmd.env_remove(k);
        }
    }
    cmd
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

    /// Запуск бинарника с пределом [`process_limit`]; `LLAMA_URL`, `ZA_LOG`, `RUST_LOG`,
    /// ключи окружения тестов и переменные памяти (`ZA_MEMORY_*`, `ZAM_*`) не наследуются.
    /// Кэши cargo и uv (категории монитора ресурсов) — пустые каталоги теста.
    pub fn run(&self, args: &[&str]) -> Out {
        self.run_with(args, &[])
    }

    /// [`Env::run`] с дополнительными переменными окружения.
    pub fn run_with(&self, args: &[&str], vars: &[(&str, &str)]) -> Out {
        self.run_env(args, vars, &[])
    }

    /// [`Env::run_with`], а переменные `remove` процесс не наследует (`NO_PROXY` у теста
    /// прокси: исключение из прокси сделало бы проверку пустой).
    pub fn run_env(&self, args: &[&str], vars: &[(&str, &str)], remove: &[&str]) -> Out {
        self.run_env_in(None, args, vars, remove)
    }

    /// [`Env::run_env`] с текущим каталогом процесса `cwd` (`None` — каталог теста):
    /// команда без `--project` (`doctor`) смотрит на свой текущий каталог.
    pub fn run_env_in(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        vars: &[(&str, &str)],
        remove: &[&str],
    ) -> Out {
        let mut child = self.command(cwd, args, vars, remove).spawn().unwrap();
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

    /// Запуск бинарника без ожидания (R1-M задача 5): строки stderr приходят в канал по мере
    /// вывода — тест ждёт события процесса (строку «ждёт Vault»), а не таймер. Окружение — как
    /// у [`Env::run_with`].
    pub fn spawn_with(&self, args: &[&str], vars: &[(&str, &str)]) -> Running {
        let mut child = self.command(None, args, vars, &[]).spawn().unwrap();
        let out = read_all(child.stdout.take().unwrap());
        let err = child.stderr.take().unwrap();
        let (tx, lines) = tokio::sync::mpsc::unbounded_channel();
        let all = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let shared = all.clone();
        let reader = std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(err).lines() {
                let Ok(line) = line else { break };
                {
                    let mut a = shared.lock().unwrap();
                    a.push_str(&line);
                    a.push('\n');
                }
                let _ = tx.send(line);
            }
        });
        Running {
            child,
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            lines,
            stderr: all,
            err_reader: Some(reader),
            out_reader: Some(out),
        }
    }

    /// Процесс бинарника с окружением теста: `ZA_HOME`, пустые кэши cargo и uv, без
    /// `LLAMA_URL`, фильтров логов и личных переменных разработчика.
    fn command(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        vars: &[(&str, &str)],
        remove: &[&str],
    ) -> Command {
        let caches = self.home.with_extension("caches");
        let _ = std::fs::create_dir_all(caches.join("cargo"));
        let _ = std::fs::create_dir_all(caches.join("uv"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_zero-agent"));
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
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
        // Нужное тест задаёт сам через `vars`.
        strip_private_env(&mut cmd);
        for k in remove {
            cmd.env_remove(k);
        }
        for (k, v) in vars {
            cmd.env(k, v);
        }
        cmd
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

    /// `trust --project <dir>` (R1-C1 D83): доверие проекту до run (набор доверенных
    /// файлов сейчас — в аудит); ожидается код 0.
    pub fn trust(&self) -> Out {
        let o = self.run(&["trust", "--project", self.project_arg()]);
        assert_eq!(o.code(), Some(0), "trust: {o:?}");
        o
    }

    /// Последний снимок run (JSON); снимка нет — паника.
    pub fn snapshot(&self, run: RunId) -> Value {
        let store = self.store();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(store.latest_snapshot(run))
            .unwrap()
            .unwrap_or_else(|| panic!("у run {run} нет снимка"))
            .1
    }

    /// Последняя запись аудита с действием на `prefix` (`trust.`, `approval.` и т. п.).
    pub fn last_audit(&self, prefix: &str) -> Option<za_store::AuditEntry> {
        let store = self.store();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(store.last_audit(prefix)).unwrap()
    }

    /// Все записи аудита с действием `action` по порядку.
    pub fn audit(&self, action: &str) -> Vec<za_store::AuditEntry> {
        let store = self.store();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(store.audit_entries(0, usize::MAX))
            .unwrap()
            .into_iter()
            .filter(|a| a.action == action)
            .collect()
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

/// Бинарник, запущенный [`Env::spawn_with`].
pub struct Running {
    child: std::process::Child,
    args: Vec<String>,
    lines: tokio::sync::mpsc::UnboundedReceiver<String>,
    /// Весь stderr к этому моменту.
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
    err_reader: Option<std::thread::JoinHandle<()>>,
    out_reader: Option<std::thread::JoinHandle<Vec<u8>>>,
}

impl Running {
    /// Ждёт строку stderr с `needle` не дольше [`process_limit`]; процесс кончился раньше —
    /// паника со всем stderr.
    pub async fn line_with(&mut self, needle: &str) -> String {
        let limit = process_limit();
        let found = tokio::time::timeout(limit, async {
            while let Some(l) = self.lines.recv().await {
                if l.contains(needle) {
                    return Some(l);
                }
            }
            None
        })
        .await;
        match found {
            Ok(Some(l)) => l,
            Ok(None) => panic!(
                "zero-agent {:?} завершился без строки {needle:?}; stderr: {}",
                self.args,
                self.stderr()
            ),
            Err(_) => panic!(
                "строки {needle:?} нет за {limit:?}; stderr: {}",
                self.stderr()
            ),
        }
    }

    /// stderr к этому моменту.
    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Ждёт завершения не дольше [`process_limit`] (дольше — процесс убивается, паника).
    pub async fn wait(mut self) -> Out {
        let until = tokio::time::Instant::now() + process_limit();
        let status = loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                break s;
            }
            if tokio::time::Instant::now() >= until {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!(
                    "zero-agent {:?} не завершился за {:?} и убит; stderr: {}",
                    self.args,
                    process_limit(),
                    self.stderr()
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        self.finish(status)
    }

    /// Снимает процесс (`Child::kill`): на Windows Ctrl+C из теста не посылается.
    pub fn kill(mut self) -> Out {
        let _ = self.child.kill();
        let status = self.child.wait().unwrap();
        self.finish(status)
    }

    fn finish(&mut self, status: ExitStatus) -> Out {
        if let Some(r) = self.err_reader.take() {
            let _ = r.join();
        }
        let stdout = self
            .out_reader
            .take()
            .map(|r| String::from_utf8(r.join().unwrap()).unwrap())
            .unwrap_or_default();
        Out {
            status,
            stdout,
            stderr: self.stderr(),
        }
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

/// Ссылка на каталог: junction на Windows (`mklink /J` прав не требует), символическая
/// ссылка на Unix.
pub fn dir_link(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
        // `mklink` понимает только `\`: путь собирается заново по компонентам.
        let native = |p: &Path| p.components().collect::<PathBuf>();
        let out = Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(native(link))
            .arg(native(target))
            .output()
            .expect("cmd не запустился");
        assert!(
            out.status.success(),
            "mklink /J: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}
