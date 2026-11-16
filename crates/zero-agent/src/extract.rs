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
//! вывод больше предела — `TooLarge`, вход больше предела (ответ `too_large`) —
//! `InputTooLarge`.
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
// Бюджет вывода `htmd` в подпроцессе (верхняя оценка и надбавка выравнивания таблиц, I-1
// повторного ревью fix-10) равен пределу вывода: страница, чей текст заведомо больше
// предела, извлекается линейно, а не строится, чтобы затем упасть.
const _: () = assert!(HTML_OUTPUT_MAX == za_extract::HTML_OUTPUT_BUDGET);
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
    tmp_dir: PathBuf,
}

impl ProcessExtractor {
    /// Извлекатель с рабочим и временным каталогом подпроцесса `tmp_dir` —
    /// `work_dir("tmp")/<run>` проекта run (D127; Global Constraints: системные `%TEMP%` и
    /// `/tmp` запрещены). Каталог обязателен (m3 ревью задачи 10): без него у HTML-подпроцесса
    /// не было бы `TMP`, а рабочим каталогом стал бы каталог `exe`. Туда же `read` и
    /// `web_fetch` кладут копию PDF.
    pub fn new(runner: Arc<dyn Runner>, exe: PathBuf, tmp_dir: PathBuf) -> Self {
        Self {
            runner,
            exe,
            tmp_dir,
        }
    }

    /// Спецификация запуска: `EnvMode::Clear`, вывод сырыми байтами (JSON), рабочий и
    /// временный каталог — `tmp_dir`.
    fn spec(&self, args: Vec<OsString>, timeout: Duration, capture_limit: usize) -> CommandSpec {
        CommandSpec {
            args,
            env_mode: EnvMode::Clear,
            tmp_dir: Some(self.tmp_dir.clone()),
            capture_limit,
            raw_stdout: true,
            ..CommandSpec::new(&self.exe, self.tmp_dir.clone(), timeout)
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
        let spec = self.spec(args, PDF_TIMEOUT, PDF_OUTPUT_MAX);
        let run = self.runner.run(spec);
        Box::pin(async move { reply(run.await) })
    }
}

/// Извлекатель run (D113, D127; m-5 повторного ревью fix-10): [`ProcessExtractor`] с
/// каталогом `work_dir(project, "tmp/<run>")`, который создаётся при каждом вызове (если его
/// ещё нет) проверкой `za_core::paths::work_dir` — без перехода по ссылкам и только внутри
/// проекта. Без каталога HTML-подпроцесс не запустился бы (`cannot start the process`):
/// копию PDF `read` и `web_fetch` кладут туда сами, а у HTML копии нет. В недоверенном
/// проекте каталог появляется только при первом извлечении, а не на старте run; после run
/// его убирает бинарник.
pub struct RunExtractor {
    runner: Arc<dyn Runner>,
    exe: PathBuf,
    project: PathBuf,
    run: za_protocol::RunId,
}

impl RunExtractor {
    pub fn new(
        runner: Arc<dyn Runner>,
        exe: PathBuf,
        project: PathBuf,
        run: za_protocol::RunId,
    ) -> Self {
        Self {
            runner,
            exe,
            project,
            run,
        }
    }

    /// Каталог подпроцесса и извлекатель с ним; каталог не создан — `Failed` (подпроцесс не
    /// запускается).
    fn extractor(&self) -> impl Future<Output = Result<ProcessExtractor, ExtractError>> + use<> {
        let (runner, exe) = (Arc::clone(&self.runner), self.exe.clone());
        let (project, sub) = (self.project.clone(), format!("tmp/{}", self.run));
        async move {
            let dir = tokio::task::spawn_blocking(move || za_core::paths::work_dir(&project, &sub))
                .await
                .map_err(std::io::Error::other)
                .and_then(|r| r);
            match dir {
                Ok(dir) => Ok(ProcessExtractor::new(runner, exe, dir)),
                Err(e) => {
                    tracing::warn!(error = %e, "каталог подпроцесса извлечения не создан");
                    Err(ExtractError::Failed(FAILED_START.into()))
                }
            }
        }
    }
}

impl Extractor for RunExtractor {
    fn html(&self, req: HtmlRequest) -> BoxFuture<'static, Result<Markdown, ExtractError>> {
        let ex = self.extractor();
        Box::pin(async move { ex.await?.html(req).await })
    }

    fn pdf(&self, req: PdfRequest) -> BoxFuture<'static, Result<PdfText, ExtractError>> {
        let ex = self.extractor();
        Box::pin(async move { ex.await?.pdf(req).await })
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
        // Вход больше предела (PDF больше 32 МиБ) — не вывод (m2 ревью задачи 10).
        Ok(Reply::TooLarge) => Err(ExtractError::InputTooLarge),
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
                truncated: t.truncated,
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
            truncated: false,
        });
        assert_eq!(
            serde_json::to_string(&ok).unwrap(),
            r#"{"ok":{"pages_total":2,"pages":[[1,"a"]],"truncated":false}}"#
        );
        // Ответ без поля `truncated` (прежняя форма) читается как необрезанный.
        let old: Reply<PdfText> =
            serde_json::from_str(r#"{"ok":{"pages_total":2,"pages":[[1,"a"]]}}"#).unwrap();
        assert!(matches!(
            old,
            Reply::Ok(PdfText {
                truncated: false,
                ..
            })
        ));
        assert_eq!(
            serde_json::to_string(&Reply::<PdfText>::Encrypted).unwrap(),
            r#""encrypted""#
        );
        // Форма `za_extract::PdfText` и `za_core::extract::PdfText` одна.
        let t = za_extract::PdfText {
            pages_total: 3,
            pages: vec![(2, "b".into())],
            truncated: true,
        };
        let back: PdfText = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(
            (back.pages_total, back.pages, back.truncated),
            (3, vec![(2, "b".into())], true)
        );
    }
}
