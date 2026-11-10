//! GPU через `nvidia-smi` (D6): отдельный процесс на замер с таймаутом через
//! `za_jail::Runner` (R1-C1 D73: дерево процесса убивается по таймауту и при броске future).

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use tokio::time::Instant;
use za_jail::{CommandSpec, DirectRunner, EnvMode, Exit, Runner, RunnerError};

use crate::snapshot::{GpuSample, GpuStatus};

/// Пауза до повторной попытки, когда GPU недоступен (D6).
pub const GPU_RETRY_UNAVAILABLE: Duration = Duration::from_secs(60);

/// Причина `Unavailable` в снимке, где GPU ещё не опрашивался.
pub const GPU_NOT_POLLED: &str = "не опрашивался";

/// Аргументы запроса (D6); порядок полей — порядок колонок в разборе.
const QUERY_ARGS: [&str; 2] = [
    "--query-gpu=index,name,uuid,utilization.gpu,memory.total,memory.used,memory.free,temperature.gpu",
    "--format=csv,noheader,nounits",
];
const FIELDS: usize = 8;
/// Предел длины причины недоступности.
const REASON_MAX_CHARS: usize = 256;
/// Предел захвата вывода `nvidia-smi` (строка на GPU, десятки байт).
const OUTPUT_LIMIT: usize = 1024 * 1024;

/// Ошибка опроса `nvidia-smi`; текст попадает в `GpuStatus::Unavailable::reason`.
#[derive(Debug, thiserror::Error)]
enum QueryError {
    #[error("nvidia-smi не найден")]
    NotFound,
    #[error("nvidia-smi не запустился: {0}")]
    Spawn(std::io::Error),
    #[error("nvidia-smi не ответил за {0:?}")]
    Timeout(Duration),
    #[error("ошибка ожидания nvidia-smi: {0}")]
    Wait(std::io::Error),
    #[error("nvidia-smi завершился с ошибкой ({code}): {message}")]
    Failed { code: String, message: String },
    #[error("непонятный вывод nvidia-smi: {0}")]
    Parse(String),
    #[error("nvidia-smi не нашёл GPU")]
    NoGpu,
}

/// Разбирает вывод `--format=csv,noheader,nounits`: строка на GPU, восемь полей через запятую.
///
/// Пустые строки пропускаются (пустой вывод — пустой список). Значения в квадратных скобках
/// (`[N/A]`, `[Not Supported]` и т. п.) и `N/A` у необязательных полей — `None`. Любая другая
/// неразборчивая строка — ошибка всего вывода.
pub fn parse_nvidia_smi_csv(out: &str) -> Result<Vec<GpuSample>, String> {
    let mut gpus = Vec::new();
    for (n, line) in out.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        gpus.push(parse_line(line).map_err(|e| format!("строка {}: {e}", n + 1))?);
    }
    Ok(gpus)
}

fn parse_line(line: &str) -> Result<GpuSample, String> {
    let f: Vec<&str> = line.split(',').map(str::trim).collect();
    if f.len() != FIELDS {
        return Err(format!("полей {}, ожидалось {FIELDS}", f.len()));
    }
    let index = f[0]
        .parse::<u32>()
        .map_err(|_| "index: не целое число".to_string())?;
    let name = f[1].to_string();
    let uuid = f[2].to_string();
    if name.is_empty() || uuid.is_empty() {
        return Err("пустое имя или uuid".into());
    }
    Ok(GpuSample {
        index,
        name,
        uuid,
        util_percent: optional(f[3], "utilization.gpu")?,
        mem_total_mib: number(f[4], "memory.total")?,
        mem_used_mib: optional(f[5], "memory.used")?,
        mem_free_mib: optional(f[6], "memory.free")?,
        temp_c: optional(f[7], "temperature.gpu")?,
    })
}

/// Неотрицательное число. Дробное округляется до целого: драйвер 621 отдаёт
/// `temperature.gpu` с сотыми (`45.12`), прежние — целым.
fn number<T: TryFrom<u64>>(v: &str, what: &str) -> Result<T, String> {
    let n = match v.parse::<u64>() {
        Ok(n) => n,
        Err(_) => match v.parse::<f64>() {
            // `as` после проверки диапазона: значение конечно и в пределах u64.
            Ok(x) if x.is_finite() && x >= 0.0 && x < u64::MAX as f64 => x.round() as u64,
            _ => return Err(format!("{what}: не число")),
        },
    };
    T::try_from(n).map_err(|_| format!("{what}: вне диапазона"))
}

/// Необязательное поле: `[…]` или `N/A` — `None`, иначе число как у [`number`].
fn optional<T: TryFrom<u64>>(v: &str, what: &str) -> Result<Option<T>, String> {
    if (v.starts_with('[') && v.ends_with(']')) || v.eq_ignore_ascii_case("n/a") {
        return Ok(None);
    }
    number(v, what).map(Some)
}

/// Один замер GPU (D6). Никогда не паникует и не ждёт дольше `timeout`: любая неудача —
/// `Unavailable`. Процессу выставляются `TMP`, `TEMP`, `TMPDIR` = `tmp_dir` (D33).
pub async fn query_nvidia_smi(exe: &Path, timeout: Duration, tmp_dir: &Path) -> GpuStatus {
    match run_query(exe, timeout, tmp_dir).await {
        Ok(gpus) => GpuStatus::Available(gpus),
        Err(e) => GpuStatus::Unavailable {
            reason: truncate(&e.to_string()),
        },
    }
}

async fn run_query(
    exe: &Path,
    timeout: Duration,
    tmp_dir: &Path,
) -> Result<Vec<GpuSample>, QueryError> {
    // Окружение — только белый список: ключи `ZA_SECRET_*` и прочее окружение daemon
    // дочернему процессу не достаются (путь к нему настраивается в `[resources]`).
    // `EnvMode::Clear` берёт из родителя `PATH`, `SystemRoot`, `WINDIR`; остальное из
    // прежнего белого списка и `TMP`/`TEMP`/`TMPDIR` — через `env_set`.
    let cwd = std::env::current_dir().unwrap_or_else(|_| tmp_dir.to_path_buf());
    let mut spec = CommandSpec::new(exe, cwd, timeout);
    spec.args = QUERY_ARGS.iter().map(OsString::from).collect();
    spec.env_mode = EnvMode::Clear;
    spec.env_set = child_env(std::env::vars_os(), tmp_dir);
    // Вывод `nvidia-smi` — строки на GPU, предел большого захвата не нужен.
    spec.capture_limit = OUTPUT_LIMIT;
    let out = DirectRunner.run(spec).await.map_err(|e| match e {
        RunnerError::Spawn { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            QueryError::NotFound
        }
        RunnerError::Spawn { source, .. } => QueryError::Spawn(source),
        RunnerError::Io(e) => QueryError::Wait(e),
    })?;
    let failed = |code: String| {
        // nvidia-smi пишет причину отказа драйвера в stdout, прочие — в stderr.
        let message = first_line(&out.stderr)
            .or_else(|| first_line(&out.stdout))
            .unwrap_or_default();
        QueryError::Failed { code, message }
    };
    match out.exit {
        Exit::Code(0) => {}
        Exit::Code(c) => return Err(failed(format!("код {c}"))),
        Exit::Signal(s) => return Err(failed(format!("сигнал {s}"))),
        Exit::Timeout => return Err(QueryError::Timeout(timeout)),
    }
    let gpus = parse_nvidia_smi_csv(&out.stdout).map_err(QueryError::Parse)?;
    if gpus.is_empty() {
        return Err(QueryError::NoGpu);
    }
    Ok(gpus)
}

/// Переменные родителя, нужные для запуска `nvidia-smi`: поиск по `PATH`, на Windows —
/// системные каталоги (без `SystemRoot` процесс не стартует), на Unix — `LD_LIBRARY_PATH`
/// для NVML вне стандартных путей. Плюс `TMP`, `TEMP`, `TMPDIR` = `tmp_dir` (D33).
fn child_env(
    parent: impl Iterator<Item = (OsString, OsString)>,
    tmp_dir: &Path,
) -> Vec<(OsString, OsString)> {
    const KEEP: &[&str] = if cfg!(windows) {
        &[
            "PATH",
            "SYSTEMROOT",
            "WINDIR",
            "SYSTEMDRIVE",
            "PROGRAMFILES",
            "PROGRAMDATA",
        ]
    } else {
        &["PATH", "LD_LIBRARY_PATH"]
    };
    let mut env: Vec<(OsString, OsString)> = parent
        .filter(|(k, _)| {
            let k = k.to_string_lossy();
            // Имена переменных Windows не различают регистр (`Path`, `SystemRoot`).
            KEEP.iter().any(|w| {
                if cfg!(windows) {
                    k.eq_ignore_ascii_case(w)
                } else {
                    k == *w
                }
            })
        })
        .collect();
    for k in ["TMP", "TEMP", "TMPDIR"] {
        env.push((k.into(), tmp_dir.as_os_str().to_owned()));
    }
    env
}

fn first_line(s: &str) -> Option<String> {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= REASON_MAX_CHARS {
        return s.to_string();
    }
    let mut t: String = s.chars().take(REASON_MAX_CHARS - 1).collect();
    t.push('…');
    t
}

/// Момент следующего замера GPU: доступен — через `every`, недоступен — через 60 с (D6).
pub fn next_gpu_poll(last: Instant, status: &GpuStatus, every: Duration) -> Instant {
    match status {
        GpuStatus::Available(_) => last + every,
        GpuStatus::Unavailable { .. } => last + GPU_RETRY_UNAVAILABLE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_reason_is_truncated() {
        let s = "я".repeat(1000);
        let t = truncate(&s);
        assert_eq!(t.chars().count(), REASON_MAX_CHARS);
        assert!(t.ends_with('…'));
        assert_eq!(truncate("коротко"), "коротко");
    }

    #[test]
    fn child_env_is_whitelisted() {
        let parent = [
            ("ZA_SECRET_OPENAI", "sk-test"),
            ("OPENAI_API_KEY", "sk-test"),
            (if cfg!(windows) { "Path" } else { "PATH" }, "/bin"),
            ("TMP", "C:/system-temp"),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = child_env(parent, Path::new("home/work/tmp/monitor"));
        let names: Vec<String> = env
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(
            !names
                .iter()
                .any(|k| k.contains("SECRET") || k.contains("API_KEY"))
        );
        assert!(names.iter().any(|k| k.eq_ignore_ascii_case("PATH")));
        let tmp: Vec<_> = env.iter().filter(|(k, _)| k == "TMP").collect();
        assert_eq!(tmp.len(), 1, "системный TMP родителя не наследуется");
        assert_eq!(tmp[0].1, OsString::from("home/work/tmp/monitor"));
        assert!(env.iter().any(|(k, _)| k == "TEMP"));
        assert!(env.iter().any(|(k, _)| k == "TMPDIR"));
    }

    #[test]
    fn bracketed_values_are_none() {
        assert_eq!(optional::<u32>("[Unknown Error]", "x"), Ok(None));
        assert_eq!(optional::<u32>("42", "x"), Ok(Some(42)));
        assert_eq!(optional::<u32>("N/A", "x"), Ok(None));
        assert!(optional::<u32>("abc", "x").is_err());
        assert!(optional::<u32>("-1", "x").is_err());
        assert_eq!(optional::<u32>("45.5", "x"), Ok(Some(46)));
        assert_eq!(optional::<u32>("45.12", "x"), Ok(Some(45)));
        assert!(optional::<u32>("-0.5", "x").is_err());
        assert!(optional::<u32>("NaN", "x").is_err());
        assert!(optional::<u32>("inf", "x").is_err());
        assert!(optional::<u32>("5000000000", "x").is_err());
    }
}
