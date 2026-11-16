//! Извлекатель текста в отдельном процессе (R1-C2 D113, D127): родитель —
//! [`ProcessExtractor`] (реализация `za_core::extract::Extractor`), дочерняя сторона —
//! скрытая подкоманда `zero-agent extract <html|pdf>` ([`cmd_html`], [`cmd_pdf`]).
//!
//! Разборщики враждебного содержимого (`htmd`, `html5ever`, `pdf-extract`, `lopdf`) идут
//! только в подпроцессе: паника, переполнение стека, вечный цикл и квадратичный разбор
//! убивают его, а не агента. Подпроцесс запускается через `za_jail::Runner`
//! (`EnvMode::Clear`: остаются `PATH`, `SystemRoot`, `WINDIR`; временный каталог —
//! `work_dir("tmp")/<run>`), дерево процессов убивает тайм-аут и бросок future вызова.
//!
//! - `extract html`: HTML (UTF-8, не больше 2 МиБ — родитель режет до записи в stdin) на stdin,
//!   тайм-аут [`HTML_TIMEOUT`], вывод не больше [`HTML_OUTPUT_MAX`].
//! - `extract pdf <path> <first> <count> <max_chars>`: путь копии (не файл проекта, TOCTOU
//!   D98), файл не больше 32 МиБ, тайм-аут [`PDF_TIMEOUT`], вывод не больше [`PDF_OUTPUT_MAX`].
//!
//! Ответ — одна строка JSON в stdout ([`Reply`]): `{"ok": …}` (та же форма, что
//! `za_core::extract::{Markdown, PdfText}`) или причина отказа разборщика. Ненулевой код
//! выхода, сигнал, неразбираемый ответ — `ExtractError::Failed`, тайм-аут — `Timeout`,
//! вывод больше предела — `TooLarge`.
//!
//! **Память подпроцесса не ограничена** (принятый риск R1-C2, ADR 0006): бомба распаковки
//! `FlateDecode` в PDF может занять память машины. Предел — Job Object и `RLIMIT_AS` через
//! `za-jail` в R2.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use za_core::BoxFuture;
use za_core::extract::{
    ExtractError, Extractor, FAILED_REPLY, FAILED_START, HtmlRequest, Markdown, PDF_MAX_BYTES,
    PdfRequest, PdfText, failed_exit_text,
};
use za_jail::{CommandSpec, EnvMode, Exit, Output, Runner, RunnerError};

/// Предел времени `extract html` (D127).
pub const HTML_TIMEOUT: Duration = Duration::from_secs(10);
/// Предел вывода `extract html`, байт (D127).
pub const HTML_OUTPUT_MAX: usize = 4 * 1024 * 1024;
/// Предел времени `extract pdf` (D113).
pub const PDF_TIMEOUT: Duration = Duration::from_secs(30);
/// Предел вывода `extract pdf`, байт (D113).
pub const PDF_OUTPUT_MAX: usize = 16 * 1024 * 1024;
/// Стек потока разбора в подпроцессе: обход `htmd` рекурсивный, страж держит глубину до 256
/// уровней (`za_extract::guard::MAX_DEPTH`, 1–2 КиБ стека на уровень в release); основной
/// поток на Windows — 1 МиБ. Память выделяется по мере роста стека.
const PARSE_STACK: usize = 64 * 1024 * 1024;

/// Ответ подпроцесса (одна строка JSON в stdout).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply<T> {
    Ok(T),
    /// Документ зашифрован.
    Encrypted,
    /// Документ испорчен; строка — причина (ASCII, коротко).
    Malformed(String),
    /// Вход больше предела.
    TooLarge,
}

/// Извлекатель в отдельном процессе (D113, D127): `exe` — бинарник `zero-agent` (обычно
/// `std::env::current_exe()`), запуск — только через `runner`.
pub struct ProcessExtractor {
    runner: Arc<dyn Runner>,
    exe: PathBuf,
    /// `work_dir("tmp")/<run>`: рабочий и временный каталог подпроцесса; создаёт вызывающий.
    tmp_dir: Option<PathBuf>,
}

impl ProcessExtractor {
    /// Извлекатель без своего временного каталога: у `pdf` им служит каталог копии
    /// (`work_dir("tmp")/<run>`, туда её кладут `read` и `web_fetch`), у `html` временного
    /// каталога нет (разбор файлов не создаёт), рабочий — каталог `exe`.
    pub fn new(runner: Arc<dyn Runner>, exe: PathBuf) -> Self {
        Self {
            runner,
            exe,
            tmp_dir: None,
        }
    }

    /// Рабочий и временный каталог подпроцесса — `work_dir("tmp")/<run>` проекта run
    /// (Global Constraints: системные `%TEMP%` и `/tmp` запрещены).
    pub fn with_tmp_dir(mut self, dir: PathBuf) -> Self {
        self.tmp_dir = Some(dir);
        self
    }

    /// Спецификация запуска: `EnvMode::Clear`, вывод сырыми байтами (JSON).
    fn spec(
        &self,
        args: Vec<OsString>,
        timeout: Duration,
        capture_limit: usize,
        near: Option<&Path>,
    ) -> CommandSpec {
        let tmp = self.tmp_dir.clone().or_else(|| near.map(Path::to_path_buf));
        let cwd = tmp
            .clone()
            .or_else(|| self.exe.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        CommandSpec {
            args,
            env_mode: EnvMode::Clear,
            tmp_dir: tmp,
            capture_limit,
            raw_stdout: true,
            ..CommandSpec::new(&self.exe, cwd, timeout)
        }
    }
}

impl Extractor for ProcessExtractor {
    /// HTML длиннее 2 МиБ режется по границе символа до записи в stdin (D109): подпроцесс
    /// дальше не читает. Бросок future убивает подпроцесс (`Runner`).
    fn html(&self, req: HtmlRequest) -> BoxFuture<'static, Result<Markdown, ExtractError>> {
        let mut html = req.html;
        html.truncate(html.floor_char_boundary(za_extract::HTML_MAX_BYTES));
        let mut spec = self.spec(
            vec!["extract".into(), "html".into()],
            HTML_TIMEOUT,
            HTML_OUTPUT_MAX,
            None,
        );
        spec.stdin = Some(html.into_bytes());
        let run = self.runner.run(spec);
        Box::pin(async move { reply(run.await) })
    }

    /// Путь — копия в `work_dir("tmp")/<run>` (D113). Бросок future убивает подпроцесс.
    fn pdf(&self, req: PdfRequest) -> BoxFuture<'static, Result<PdfText, ExtractError>> {
        let args = vec![
            "extract".into(),
            "pdf".into(),
            req.path.clone().into_os_string(),
            req.first.to_string().into(),
            req.count.to_string().into(),
            req.max_chars.to_string().into(),
        ];
        let spec = self.spec(args, PDF_TIMEOUT, PDF_OUTPUT_MAX, req.path.parent());
        let run = self.runner.run(spec);
        Box::pin(async move { reply(run.await) })
    }
}

/// Исход подпроцесса → результат извлечения.
fn reply<T: DeserializeOwned>(r: Result<Output, RunnerError>) -> Result<T, ExtractError> {
    let out = r.map_err(|e| {
        tracing::warn!(error = %e, "подпроцесс извлечения не запущен");
        ExtractError::Failed(FAILED_START.into())
    })?;
    match out.exit {
        Exit::Timeout => return Err(ExtractError::Timeout),
        Exit::Code(0) => {}
        Exit::Code(c) => {
            tracing::warn!(code = c, stderr = %out.stderr, "подпроцесс извлечения упал");
            return Err(ExtractError::Failed(failed_exit_text(&exit_code(c))));
        }
        Exit::Signal(s) => {
            tracing::warn!(signal = s, "подпроцесс извлечения убит сигналом");
            return Err(ExtractError::Failed(failed_exit_text(&format!(
                "signal {s}"
            ))));
        }
    }
    if out.stdout_truncated {
        return Err(ExtractError::TooLarge);
    }
    let bytes = out.stdout_raw.unwrap_or_default();
    match serde_json::from_slice::<Reply<T>>(&bytes) {
        Ok(Reply::Ok(v)) => Ok(v),
        Ok(Reply::Encrypted) => Err(ExtractError::Encrypted),
        Ok(Reply::Malformed(why)) => Err(ExtractError::Malformed(why)),
        Ok(Reply::TooLarge) => Err(ExtractError::TooLarge),
        Err(e) => {
            tracing::warn!(error = %e, "ответ подпроцесса извлечения не разобран");
            Err(ExtractError::Failed(FAILED_REPLY.into()))
        }
    }
}

/// Код выхода для модели: отрицательный (статус NTSTATUS на Windows: `0xC00000FD` —
/// переполнение стека) — шестнадцатеричным.
fn exit_code(c: i32) -> String {
    if c < 0 {
        format!("exit code {:#010X}", c.cast_unsigned())
    } else {
        format!("exit code {c}")
    }
}

// ---- дочерняя сторона: скрытая подкоманда `zero-agent extract` ----

/// `zero-agent extract html`: HTML на stdin (дальше 2 МиБ не читается), JSON `Markdown` в
/// stdout. Разбор — в потоке с большим стеком.
pub fn cmd_html() -> ExitCode {
    let mut buf = Vec::new();
    if let Err(e) = std::io::stdin()
        .lock()
        .take(za_extract::HTML_MAX_BYTES as u64)
        .read_to_end(&mut buf)
    {
        eprintln!("zero-agent extract html: stdin: {e}");
        return ExitCode::FAILURE;
    }
    let html = String::from_utf8_lossy(&buf).into_owned();
    parse_and_print(move || {
        let md = za_extract::html_to_markdown(&html);
        Reply::Ok(Markdown {
            title: md.title,
            text: md.text,
            simplified: md.simplified,
        })
    })
}

/// `zero-agent extract pdf <path> <first> <count> <max_chars>`: файл не больше 32 МиБ,
/// JSON `PdfText` в stdout.
pub fn cmd_pdf(path: &Path, first: u32, count: u32, max_chars: usize) -> ExitCode {
    let bytes = match read_capped(path, PDF_MAX_BYTES) {
        Ok(Some(b)) => b,
        Ok(None) => return print_reply(&Reply::<PdfText>::TooLarge),
        Err(e) => {
            eprintln!("zero-agent extract pdf: {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    parse_and_print(
        move || match za_extract::pdf_pages(&bytes, first, count, max_chars) {
            Ok(t) => Reply::Ok(PdfText {
                pages_total: t.pages_total,
                pages: t.pages,
            }),
            Err(za_extract::ExtractError::Encrypted) => Reply::Encrypted,
            Err(za_extract::ExtractError::Malformed(why)) => Reply::Malformed(why),
        },
    )
}

/// Файл целиком, если он не больше `max` байт; больше — `None` (по прочитанным байтам: файл
/// мог вырасти после `metadata`).
fn read_capped(path: &Path, max: usize) -> std::io::Result<Option<Vec<u8>>> {
    let f = std::fs::File::open(path)?;
    if f.metadata()?.len() > max as u64 {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    f.take(max as u64 + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() <= max).then_some(bytes))
}

/// Разбор `f` в потоке с большим стеком и печать ответа. Паника потока — код выхода 101,
/// как у паники основного потока: родитель видит `Failed`.
fn parse_and_print<T, F>(f: F) -> ExitCode
where
    T: Serialize + Send + 'static,
    F: FnOnce() -> Reply<T> + Send + 'static,
{
    let joined = std::thread::Builder::new()
        .name("extract".into())
        .stack_size(PARSE_STACK)
        .spawn(f)
        .map(std::thread::JoinHandle::join);
    match joined {
        Ok(Ok(reply)) => print_reply(&reply),
        Ok(Err(_)) => ExitCode::from(101),
        Err(e) => {
            eprintln!("zero-agent extract: поток разбора не запущен: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_reply<T: Serialize>(reply: &Reply<T>) -> ExitCode {
    let line = match serde_json::to_string(reply) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("zero-agent extract: ответ не сериализован: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut out = std::io::stdout().lock();
    match writeln!(out, "{line}").and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zero-agent extract: stdout: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Тестовые крюки подкоманды (D128): только с признаком `test-hooks`.
#[cfg(feature = "test-hooks")]
pub mod hooks {
    use std::time::Duration;

    /// Имя файла с pid подпроцесса `--hang` во временном каталоге (`TMP`).
    pub const HANG_PID_FILE: &str = "extract-hang.pid";

    /// `extract --hang`: пишет `pid <n>` в `$TMP/extract-hang.pid` и не завершается (тест
    /// тайм-аута и броска future).
    pub fn hang() -> ! {
        if let Some(tmp) = std::env::var_os("TMP") {
            let path = std::path::Path::new(&tmp).join(HANG_PID_FILE);
            let _ = std::fs::write(path, format!("pid {}\n", std::process::id()));
        }
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }

    /// `extract --crash`: аварийное завершение (сквозной тест падения подпроцесса).
    pub fn crash() -> ! {
        std::process::abort()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_text() {
        assert_eq!(exit_code(101), "exit code 101");
        assert_eq!(exit_code(-1_073_741_571), "exit code 0xC00000FD");
    }

    #[test]
    fn reply_wire_form() {
        let ok: Reply<PdfText> = Reply::Ok(PdfText {
            pages_total: 2,
            pages: vec![(1, "a".into())],
        });
        assert_eq!(
            serde_json::to_string(&ok).unwrap(),
            r#"{"ok":{"pages_total":2,"pages":[[1,"a"]]}}"#
        );
        assert_eq!(
            serde_json::to_string(&Reply::<PdfText>::Encrypted).unwrap(),
            r#""encrypted""#
        );
        // Форма `za_extract::PdfText` и `za_core::extract::PdfText` одна.
        let t = za_extract::PdfText {
            pages_total: 3,
            pages: vec![(2, "b".into())],
        };
        let back: PdfText = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!((back.pages_total, back.pages), (3, vec![(2, "b".into())]));
    }
}
