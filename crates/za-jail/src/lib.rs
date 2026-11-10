//! `za-jail` — единственное место запуска дочерних процессов (R1-C1 D73, D84, SEC-006).
//!
//! Трейт [`Runner`] и реализация без ОС-изоляции [`DirectRunner`] (решение заказчика N-15):
//! процесс запускается деревом (Windows — Job Object с `KILL_ON_JOB_CLOSE`, Unix — своя
//! группа процессов), тайм-аут и бросок future убивают всё дерево, вывод читается потоково
//! и ограничен [`CAPTURE_LIMIT`], окружение — разрешённый набор ([`child_env`]).
//!
//! Ненулевой код выхода — данные ([`Exit::Code`]), а не ошибка: [`RunnerError`] — только
//! сбой запуска или ввода-вывода.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

mod direct;
mod env;
mod reap;

pub use direct::DirectRunner;
pub use env::child_env;

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

/// Что и как запустить.
#[derive(Debug, Clone)]
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
    /// Unix: файл остатка D84.
    pub leftover_file: Option<PathBuf>,
    /// Для тестов: окружение-основа вместо std::env::vars_os().
    pub base_env: Option<Vec<(OsString, OsString)>>,
}

impl CommandSpec {
    /// Спецификация с умолчаниями: `EnvMode::Inherit`, без доп. переменных, тайм-аут
    /// `timeout`, предел [`CAPTURE_LIMIT`].
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
            leftover_file: None,
            base_env: None,
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
    /// Вывод: `from_utf8_lossy`, `\r\n` → `\n`, не длиннее `capture_limit` байт.
    pub stdout: String,
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
