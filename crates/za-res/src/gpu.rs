//! GPU через `nvidia-smi` (D6): отдельный процесс на замер, таймаут, `kill_on_drop`.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::Instant;

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
    let mut cmd = Command::new(exe);
    cmd.args(QUERY_ARGS)
        .env("TMP", tmp_dir)
        .env("TEMP", tmp_dir)
        .env("TMPDIR", tmp_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        /// Без консольного окна у дочернего процесса.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => QueryError::NotFound,
        _ => QueryError::Spawn(e),
    })?;
    // По таймауту future ожидания бросается вместе с `Child`, `kill_on_drop` убивает процесс.
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| QueryError::Timeout(timeout))?
        .map_err(QueryError::Wait)?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // nvidia-smi пишет причину отказа драйвера в stdout, прочие — в stderr.
        let message = first_line(&stderr)
            .or_else(|| first_line(&stdout))
            .unwrap_or_default();
        let code = out
            .status
            .code()
            .map_or_else(|| "без кода".to_string(), |c| format!("код {c}"));
        return Err(QueryError::Failed { code, message });
    }
    let gpus = parse_nvidia_smi_csv(&stdout).map_err(QueryError::Parse)?;
    if gpus.is_empty() {
        return Err(QueryError::NoGpu);
    }
    Ok(gpus)
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
