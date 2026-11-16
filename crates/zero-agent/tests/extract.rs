//! Извлекатель в отдельном процессе (R1-C2 D113, D127): `ProcessExtractor` запускает
//! настоящий бинарник (`CARGO_BIN_EXE_zero-agent`, скрытая подкоманда `extract`) через
//! `za_jail::DirectRunner`. Разбор HTML и PDF, падение подпроцесса (крюк `--crash`),
//! тайм-аут и бросок future (крюк `--hang`) убивают только подпроцесс.
//!
//! Крюки есть только с признаком `test-hooks` (D128): весь файл — `required-features`.
//! Крюк передаёт обёртка `Hooked` над `DirectRunner`: она вставляет флаг после `extract` и
//! укорачивает тайм-аут, сам `ProcessExtractor` крюков не знает. `za_jail_tree` здесь не
//! используется: `CARGO_BIN_EXE_*` есть только у тестов пакета-владельца бинарника.

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use za_core::BoxFuture;
use za_core::extract::{
    ExtractError, Extractor, HtmlRequest, Markdown, PDF_MAX_BYTES, PdfRequest, PdfText,
};
use za_jail::{CommandSpec, DirectRunner, Output, Runner, RunnerError};
use zero_agent::extract::ProcessExtractor;
use zero_agent::extract::hooks::HANG_PID_FILE;

#[path = "../../../test-support/pdf.rs"]
mod pdf;
#[path = "../../../test-support/unique_dir.rs"]
mod shared_dir;

/// Множитель времени тестов: `ZA_TEST_TIME_SCALE` (целое 1–6, по умолчанию 1).
fn scale() -> u32 {
    std::env::var("ZA_TEST_TIME_SCALE")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map_or(1, |n| n.clamp(1, 6))
}

fn scaled(d: Duration) -> Duration {
    d * scale()
}

async fn within<T>(base: Duration, what: &str, f: impl Future<Output = T>) -> T {
    tokio::time::timeout(scaled(base), f)
        .await
        .unwrap_or_else(|_| panic!("не дождались в срок: {what}"))
}

fn dir(name: &str) -> PathBuf {
    shared_dir::unique_dir("zero-agent-extract", name)
}

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zero-agent"))
}

fn extractor(tmp: &Path) -> ProcessExtractor {
    ProcessExtractor::new(Arc::new(DirectRunner), exe()).with_tmp_dir(tmp.to_path_buf())
}

/// Обёртка запуска: флаг `flag` после `extract`, свой тайм-аут.
struct Hooked {
    flag: &'static str,
    timeout: Option<Duration>,
}

impl Runner for Hooked {
    fn run(&self, mut spec: CommandSpec) -> BoxFuture<'static, Result<Output, RunnerError>> {
        spec.args.insert(1, OsString::from(self.flag));
        if let Some(t) = self.timeout {
            spec.timeout = t;
        }
        DirectRunner.run(spec)
    }
}

/// Обёртка запуска, которая запоминает спецификации (размер stdin, окружение, каталоги).
#[derive(Default)]
struct Recording {
    specs: std::sync::Mutex<Vec<CommandSpec>>,
}

impl Runner for Recording {
    fn run(&self, spec: CommandSpec) -> BoxFuture<'static, Result<Output, RunnerError>> {
        self.specs.lock().unwrap().push(spec.clone());
        DirectRunner.run(spec)
    }
}

fn hooked(tmp: &Path, flag: &'static str, timeout: Option<Duration>) -> ProcessExtractor {
    ProcessExtractor::new(Arc::new(Hooked { flag, timeout }), exe()).with_tmp_dir(tmp.to_path_buf())
}

fn html(text: &str) -> HtmlRequest {
    HtmlRequest {
        html: text.to_owned(),
        base_url: "https://example.com/".into(),
    }
}

/// Время старта процесса (sysinfo), `None` — процесса нет или он зомби.
fn started(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessStatus, ProcessesToUpdate, System};
    let pid = Pid::from_u32(pid);
    let mut sys = System::new().expect("sysinfo");
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid)
        .filter(|p| p.status() != ProcessStatus::Zombie)
        .map(|p| p.start_time())
}

/// Ждёт, пока процесс `pid` (запущенный в `first`) умрёт: его нет, он зомби или под тем
/// же номером уже другой процесс.
async fn dead(pid: u32, first: Option<u64>, what: &str) {
    within(Duration::from_secs(10), what, async {
        while first.is_some() && started(pid) == first {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
}

/// Pid подпроцесса `--hang` из `<tmp>/extract-hang.pid` (`pid <n>`).
async fn hang_pid(tmp: &Path) -> u32 {
    let file = tmp.join(HANG_PID_FILE);
    within(
        Duration::from_secs(20),
        "pid подпроцесса --hang",
        async {
            loop {
                if let Ok(t) = std::fs::read_to_string(&file)
                    && let Some(pid) = t.trim().strip_prefix("pid ").and_then(|p| p.parse().ok())
                {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        },
    )
    .await
}

fn pdf_request(path: &Path) -> PdfRequest {
    PdfRequest {
        path: path.to_path_buf(),
        first: 1,
        count: 10,
        max_chars: 100_000,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn process_extractor_pdf_roundtrip() {
    let tmp = dir("pdf");
    let path = tmp.join("pdf-x.pdf");
    std::fs::write(&path, pdf::two_page_pdf()).unwrap();
    let ex = ProcessExtractor::new(Arc::new(DirectRunner), exe());
    let t = within(
        Duration::from_secs(60),
        "extract pdf",
        ex.pdf(pdf_request(&path)),
    )
    .await
    .expect("текст двух страниц");
    assert_eq!(
        t,
        PdfText {
            pages_total: 2,
            pages: vec![(1, "alpha".into()), (2, "beta".into())],
        }
    );
    let mut req = pdf_request(&path);
    req.first = 2;
    req.count = 1;
    let t = within(Duration::from_secs(60), "extract pdf", ex.pdf(req))
        .await
        .unwrap();
    assert_eq!(t.pages, vec![(2, "beta".into())]);
}

/// Отказы разборщика PDF доходят до родителя своими видами: битый — `Malformed`,
/// зашифрованный — `Encrypted`, файл больше 32 МиБ — `TooLarge`.
#[tokio::test(flavor = "multi_thread")]
async fn process_extractor_pdf_errors() {
    let tmp = dir("pdf_errors");
    let ex = ProcessExtractor::new(Arc::new(DirectRunner), exe());
    let bad = tmp.join("bad.pdf");
    std::fs::write(&bad, b"%PDF-1.4 garbage").unwrap();
    let r = within(
        Duration::from_secs(60),
        "битый PDF",
        ex.pdf(pdf_request(&bad)),
    )
    .await;
    assert!(matches!(r, Err(ExtractError::Malformed(_))), "{r:?}");
    let enc = tmp.join("enc.pdf");
    std::fs::write(&enc, pdf::encrypted_pdf()).unwrap();
    let r = within(
        Duration::from_secs(60),
        "зашифрованный PDF",
        ex.pdf(pdf_request(&enc)),
    )
    .await;
    assert_eq!(r, Err(ExtractError::Encrypted));
    let big = tmp.join("big.pdf");
    let mut bytes = pdf::two_page_pdf();
    bytes.resize(PDF_MAX_BYTES + 1, b' ');
    std::fs::write(&big, bytes).unwrap();
    let r = within(
        Duration::from_secs(60),
        "PDF больше 32 МиБ",
        ex.pdf(pdf_request(&big)),
    )
    .await;
    assert_eq!(r, Err(ExtractError::TooLarge));
}

#[tokio::test(flavor = "multi_thread")]
async fn process_extractor_html_roundtrip() {
    let tmp = dir("html");
    let md = within(
        Duration::from_secs(60),
        "extract html",
        extractor(&tmp).html(html("<title>T</title><h1>H</h1>")),
    )
    .await
    .expect("разбор HTML");
    assert_eq!(
        md,
        Markdown {
            title: Some("T".into()),
            text: "# H".into(),
            simplified: false,
        }
    );
}

/// HTML больше 2 МиБ родитель режет до записи в stdin (перенос задачи 8): разбор идёт по
/// первым 2 МиБ, без ошибки.
#[tokio::test(flavor = "multi_thread")]
async fn html_over_2_mib_is_cut_before_stdin() {
    let tmp = dir("html_big");
    // Многобайтовые символы: обрезка — по границе символа.
    let page = format!("<p>{}</p>", "слово ".repeat(300_000));
    assert!(page.len() > 2 * 1024 * 1024);
    let rec = Arc::new(Recording::default());
    let ex = ProcessExtractor::new(rec.clone(), exe()).with_tmp_dir(tmp.clone());
    let md = within(
        Duration::from_secs(60),
        "extract html 3 МиБ",
        ex.html(html(&page)),
    )
    .await
    .expect("разбор обрезанного HTML");
    assert!(md.text.starts_with("слово слово"));
    assert!(!md.text.contains('\u{FFFD}'), "символ разрезан");
    let specs = rec.specs.lock().unwrap().clone();
    let stdin = specs[0].stdin.as_ref().expect("HTML на stdin");
    assert!(stdin.len() <= za_extract::HTML_MAX_BYTES, "{}", stdin.len());
    assert!(std::str::from_utf8(stdin).is_ok());
    // Подпроцесс: чистое окружение, рабочий и временный каталог — каталог run.
    assert_eq!(specs[0].env_mode, za_jail::EnvMode::Clear);
    assert_eq!(specs[0].tmp_dir.as_deref(), Some(tmp.as_path()));
    assert_eq!(specs[0].cwd, tmp);
    assert_eq!(
        specs[0].args,
        vec![OsString::from("extract"), "html".into()]
    );
}

/// 300 000 вложенных `<div>` (в пределах 2 МиБ): подпроцесс укладывается (страж даёт
/// `simplified`) или падает — процесс теста жив, результат `Ok` или `Err(Failed)`.
#[tokio::test(flavor = "multi_thread")]
async fn deep_html_kills_only_child() {
    let tmp = dir("deep");
    let page = format!("{}deep text", "<div>".repeat(300_000));
    let r = within(
        Duration::from_secs(60),
        "глубокий HTML",
        extractor(&tmp).html(html(&page)),
    )
    .await;
    match r {
        Ok(md) => assert!(md.simplified, "глубокая вложенность — упрощённый разбор"),
        Err(ExtractError::Failed(_)) => {}
        Err(e) => panic!("ожидался Ok или Failed: {e:?}"),
    }
}

/// Сквозной тест падения подпроцесса через `za_jail::Runner` (перенос прохода fix-7):
/// крюк `--crash` (`abort`) — `Failed` с кодом выхода, процесс теста жив.
#[tokio::test(flavor = "multi_thread")]
async fn crashed_child_is_failed() {
    let tmp = dir("crash");
    let ex = hooked(&tmp, "--crash", None);
    let r = within(
        Duration::from_secs(60),
        "--crash",
        ex.html(html("<p>x</p>")),
    )
    .await;
    match r {
        Err(ExtractError::Failed(why)) => {
            assert!(why.starts_with("process crashed ("), "{why}");
        }
        other => panic!("ожидался Failed: {other:?}"),
    }
    let path = tmp.join("x.pdf");
    std::fs::write(&path, pdf::two_page_pdf()).unwrap();
    let r = within(
        Duration::from_secs(60),
        "--crash pdf",
        ex.pdf(pdf_request(&path)),
    )
    .await;
    assert!(matches!(r, Err(ExtractError::Failed(_))), "{r:?}");
}

/// Зависший подпроцесс (`--hang`): тайм-аут спецификации — `Timeout`, дерево убито.
#[tokio::test(flavor = "multi_thread")]
async fn process_extractor_timeout_kills_child() {
    let tmp = dir("timeout");
    let ex = hooked(&tmp, "--hang", Some(scaled(Duration::from_secs(2))));
    let mut run = ex.html(html("<p>x</p>"));
    let pid = tokio::select! {
        r = &mut run => panic!("подпроцесс --hang завершился до pid: {r:?}"),
        pid = hang_pid(&tmp) => pid,
    };
    let first = started(pid);
    assert!(first.is_some(), "подпроцесс --hang не найден");
    let r = within(Duration::from_secs(30), "тайм-аут --hang", run).await;
    assert_eq!(r, Err(ExtractError::Timeout));
    dead(pid, first, "подпроцесс после тайм-аута").await;
}

/// Бросок future вызова (тайм-аут инструмента, отмена) убивает подпроцесс (перенос задачи 8).
#[tokio::test(flavor = "multi_thread")]
async fn dropped_future_kills_child() {
    let tmp = dir("dropped");
    let ex = hooked(&tmp, "--hang", Some(Duration::from_secs(3600)));
    let mut run = ex.html(html("<p>x</p>"));
    let pid = tokio::select! {
        r = &mut run => panic!("подпроцесс --hang завершился сам: {r:?}"),
        pid = hang_pid(&tmp) => pid,
    };
    let first = started(pid);
    assert!(first.is_some(), "подпроцесс --hang не найден");
    drop(run);
    dead(pid, first, "подпроцесс после броска future").await;
}
