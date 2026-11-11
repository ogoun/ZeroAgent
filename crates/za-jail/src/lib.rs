//! `za-jail` — единственное место запуска дочерних процессов (R1-C1 D73, D84, SEC-006).
//!
//! Трейт [`Runner`] и реализация без ОС-изоляции [`DirectRunner`] (решение заказчика N-15):
//! процесс запускается деревом (Windows — Job Object с `KILL_ON_JOB_CLOSE`, Unix — своя
//! группа процессов), тайм-аут и бросок future убивают всё дерево, вывод читается потоково
//! и ограничен [`CAPTURE_LIMIT`], окружение — разрешённый набор ([`child_env`]).
//! На Linux агент — subreaper, а дерево помечено маркером, поэтому потомки, ушедшие из
//! группы (`setpgid`, `setsid`), тоже убиваются. Остатки деревьев после смерти агента
//! убирает [`DirectRunner::reap_leftovers`] по реестру групп в `ZA_HOME`.
//!
//! Ненулевой код выхода — данные ([`Exit::Code`]), а не ошибка: [`RunnerError`] — только
//! сбой запуска или ввода-вывода.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

mod decode;
mod direct;
mod env;
#[cfg(target_os = "linux")]
mod linux;
mod reap;

pub use direct::DirectRunner;
pub use env::child_env;
pub use reap::check_registry;

/// Future, которую возвращает [`Runner::run`].
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Предел захвата каждого из stdout и stderr: остаток дочитывается и отбрасывается.
pub const CAPTURE_LIMIT: usize = 16 * 1024 * 1024;
/// Сколько ждать EOF вывода после выхода главного процесса (фоновые потомки держат каналы)
/// и дочитывания после убийства дерева (D84, Ruling 5.28).
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// Окружение дочернего процесса (D84).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvMode {
    /// Окружение родителя без секретов и `ZA_HOME`.
    Inherit,
    /// Только `env_set` и `PATH`, `SystemRoot`, `WINDIR`.
    Clear,
}

/// Запись реестра групп процессов (D84, Unix): куда и от какого run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    /// Файл в реестре `ZA_HOME/projects/<project_id>/procs/<effect_stem>.json`; каталог
    /// создаёт вызывающий через `za_core::paths::home_dir`.
    pub file: PathBuf,
    /// Run, запустивший процесс (для диагностики остатков).
    pub run: String,
}

/// Что и как запустить.
#[derive(Clone)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env_mode: EnvMode,
    pub env_set: Vec<(OsString, OsString)>,
    pub remove_env: Vec<OsString>,
    /// TMP, TEMP, TMPDIR; создаёт вызывающий через za_core::paths::work_dir.
    pub tmp_dir: Option<PathBuf>,
    pub timeout: Duration,
    pub capture_limit: usize,
    /// Unix (Linux): запись реестра групп D84; на Windows не используется.
    pub leftover: Option<Leftover>,
    /// Для тестов: окружение-основа вместо std::env::vars_os().
    pub base_env: Option<Vec<(OsString, OsString)>>,
    /// Байты на stdin процесса; после них stdin закрывается. `None` — stdin пуст
    /// (`Stdio::null`). Чекпоинты git (R1-C1 D89): `--stdin-paths`, `--index-info`,
    /// `cat-file --batch`.
    pub stdin: Option<Vec<u8>>,
    /// Вернуть stdout сырыми байтами ([`Output::stdout_raw`]) без декодирования и
    /// нормализации `\r\n`; `Output::stdout` тогда пуст. Чекпоинты git: пути `-z` и
    /// содержимое объектов — байты как есть.
    pub raw_stdout: bool,
    /// Писать stdout в этот файл (с текущей позиции), а не в память: содержимое большого
    /// объекта git при восстановлении чекпоинта (R1-C1 I3). Предел `capture_limit`
    /// действует и здесь; `Output::stdout` пуст, `stdout_raw` — `None`. Файл открывает и
    /// проверяет вызывающий.
    pub stdout_file: Option<std::sync::Arc<std::fs::File>>,
}

/// Значения окружения не печатаются: `env_set` — канал секретов инструменту (SEC-041,
/// SEC-044), `base_env` — всё окружение агента.
impl std::fmt::Debug for CommandSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = |v: &[(OsString, OsString)]| -> Vec<String> {
            v.iter()
                .map(|(k, _)| format!("{}=<redacted>", k.to_string_lossy()))
                .collect()
        };
        f.debug_struct("CommandSpec")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field("env_mode", &self.env_mode)
            .field("env_set", &names(&self.env_set))
            .field("remove_env", &self.remove_env)
            .field("tmp_dir", &self.tmp_dir)
            .field("timeout", &self.timeout)
            .field("capture_limit", &self.capture_limit)
            .field("leftover", &self.leftover)
            .field("base_env", &self.base_env.as_ref().map(|b| b.len()))
            .field("stdin", &self.stdin.as_ref().map(|b| b.len()))
            .field("raw_stdout", &self.raw_stdout)
            .field("stdout_file", &self.stdout_file.is_some())
            .finish()
    }
}

impl CommandSpec {
    /// Спецификация с умолчаниями: `EnvMode::Inherit`, без доп. переменных, тайм-аут
    /// `timeout`, предел [`CAPTURE_LIMIT`], пустой stdin, вывод декодируется.
    pub fn new(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env_mode: EnvMode::Inherit,
            env_set: Vec::new(),
            remove_env: Vec::new(),
            tmp_dir: None,
            timeout,
            capture_limit: CAPTURE_LIMIT,
            leftover: None,
            base_env: None,
            stdin: None,
            raw_stdout: false,
            stdout_file: None,
        }
    }
}

/// Как завершился процесс.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// Код выхода главного процесса (ненулевой — тоже данные).
    Code(i32),
    /// Unix: главный процесс убит сигналом.
    Signal(i32),
    /// Истёк `CommandSpec::timeout`; дерево убито.
    Timeout,
}

/// Результат запуска.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub exit: Exit,
    /// Вывод: `from_utf8_lossy`, `\r\n` → `\n`, не длиннее `capture_limit` байт. При
    /// `CommandSpec::raw_stdout` — пусто, байты — в [`Output::stdout_raw`].
    pub stdout: String,
    /// stdout как есть (не длиннее `capture_limit` байт) — только при
    /// `CommandSpec::raw_stdout`.
    pub stdout_raw: Option<Vec<u8>>,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// Фоновые потомки держали вывод дольше [`KILL_GRACE`] после выхода главного процесса
    /// и были убиты. Пометку в тексте для модели ставит потребитель.
    pub background_killed: bool,
    pub duration: Duration,
}

/// Сбой запуска или ввода-вывода (не код выхода и не тайм-аут).
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("failed to start {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Запуск дочерних процессов. Бросок future убивает всё дерево процессов.
pub trait Runner: Send + Sync {
    fn run(&self, spec: CommandSpec) -> BoxFuture<'static, Result<Output, RunnerError>>;
}
