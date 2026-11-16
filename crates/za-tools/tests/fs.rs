//! Инструменты `read`, `search`, `glob` (R1-C1 D74, D87, D97, D98): формат вывода,
//! пределы, подсказки путей, обход без ссылок и служебных каталогов, `.gitignore` без git
//! и с git, предметы вызова для политики, трекер чтений.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use za_core::extract::{
    ExtractError, Extractor, HtmlRequest, Markdown, PDF_MAX_CHARS, PdfRequest, PdfText,
};
use za_core::paths::{ResolvedPath, Roots, classify, resolve};
use za_core::{Access, Approval, BoxFuture, RiskClass, Tool, ToolCx, ToolKind, ToolOutput};
use za_protocol::{EffectClass, EffectKey, RunId, RunRole, Tier};
use za_tools::{
    GLOB_MAX, READ_DEFAULT_LIMIT, READ_MAX_BYTES, ReadTracker, SEARCH_MAX, TrackError, WalkLimits,
    glob_split, glob_tool, nearest, read_tool, search_in, search_tool,
};

#[path = "../../../test-support/unique_dir.rs"]
mod shared_dir;

/// Чистый каталог теста `CARGO_TARGET_TMPDIR/za-tools-fs/<name>-<pid>-<n>`.
fn unique_dir(name: &str) -> PathBuf {
    shared_dir::unique_dir("za-tools-fs", name)
}

/// Корень репозитория ZeroAgent.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Каталог временного git-репозитория `<ZeroAgent>/.tmp/r1c1-git/za-tools-fs/<name>-<pid>-<n>`
/// (Global Constraints): git теста не видит репозиторий ZeroAgent.
fn git_dir(name: &str) -> PathBuf {
    shared_dir::unique_dir_in(&repo_root().join(".tmp/r1c1-git/za-tools-fs"), name)
}

/// `git init` с явной идентичностью и `GIT_CEILING_DIRECTORIES=<ZeroAgent>/.tmp`.
fn git_init(dir: &Path) {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.autocrlf=false",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "init.defaultBranch=main",
            "init",
            "-q",
        ])
        .current_dir(dir)
        .env("GIT_CEILING_DIRECTORIES", repo_root().join(".tmp"))
        .output()
        .expect("git не запустился: нужен git в PATH");
    assert!(
        out.status.success(),
        "git init: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.join(".git").is_dir());
}

fn write(root: &Path, rel: &str, bytes: impl AsRef<[u8]>) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

/// Множитель тайм-аутов теста `ZA_TEST_TIME_SCALE` (целое 1–6).
fn scale() -> u32 {
    std::env::var("ZA_TEST_TIME_SCALE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1)
        .clamp(1, 6)
}

fn cx(root: &Path, run: RunId) -> ToolCx {
    ToolCx {
        run,
        project_dir: root.to_path_buf(),
        call_id: "c1".into(),
        effect_key: EffectKey::derive(run, 1, 1),
        paths: Vec::new(),
        deadline: None,
        output_budget: None,
    }
}

/// Вызов инструмента под тайм-аутом (никакое ожидание теста не виснет).
async fn call_in(tool: &Arc<dyn Tool>, root: &Path, run: RunId, args: Value) -> ToolOutput {
    let limit = Duration::from_secs(60 * u64::from(scale()));
    tokio::time::timeout(limit, tool.call(args, cx(root, run)))
        .await
        .unwrap_or_else(|_| panic!("инструмент не ответил за {limit:?}"))
}

async fn call(tool: &Arc<dyn Tool>, root: &Path, args: Value) -> ToolOutput {
    call_in(tool, root, RunId::new(), args).await
}

fn read() -> Arc<dyn Tool> {
    read_tool(ReadTracker::new(), None)
}

fn roots(root: &Path) -> Roots {
    Roots::canonical(root, &root.join("no-home")).unwrap()
}

fn resolved(root: &Path, arg: &str) -> ResolvedPath {
    let r = roots(root);
    resolve(&r, &classify(&r.project, arg).unwrap(), Access::Read).unwrap()
}

/// Ссылка на каталог: junction на Windows (`mklink /J` прав не требует), символическая
/// ссылка на Unix. Не создалась — `None` и пропуск с сообщением.
fn dir_link(link: &Path, target: &Path) -> Option<()> {
    #[cfg(windows)]
    {
        let native = |p: &Path| p.components().collect::<PathBuf>();
        let out = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(native(link))
            .arg(native(target))
            .output()
            .expect("cmd не запустился");
        if !out.status.success() {
            skip(&format!(
                "junction не создан: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
            return None;
        }
        Some(())
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).unwrap();
        Some(())
    }
}

/// Ссылка на файл. На Windows требует режима разработчика или прав администратора: без
/// них — `None` и пропуск с сообщением.
fn file_link(link: &Path, target: &Path) -> Option<()> {
    #[cfg(windows)]
    {
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => Some(()),
            Err(e) => {
                skip(&format!(
                    "ссылка на файл не создана ({e}); нужен режим разработчика или права администратора"
                ));
                None
            }
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).unwrap();
        Some(())
    }
}

/// Пропуск теста ссылок без прав. `ZA_REQUIRE_LINK_TESTS=1` (CI) превращает пропуск в
/// падение: доказательство обязательно. На Unix ссылки создаются без прав.
#[cfg(windows)]
fn skip(why: &str) {
    if std::env::var("ZA_REQUIRE_LINK_TESTS").as_deref() == Ok("1") {
        panic!("ZA_REQUIRE_LINK_TESTS=1, а проверка пропущена: {why}");
    }
    eprintln!("ПРОПУСК: {why}");
}

// ---- read ----

#[tokio::test]
async fn read_numbers_lines_and_range() {
    let root = unique_dir("read_range");
    write(&root, "a.txt", "a\nb\nc\nd\n");
    let read = read();
    let o = call(
        &read,
        &root,
        json!({"path": "a.txt", "start": 2, "limit": 2}),
    )
    .await;
    assert_eq!(o.content, "     2\tb\n     3\tc\n");
    assert!(!o.is_error);
    let all = call(&read, &root, json!({"path": "a.txt"})).await;
    assert_eq!(all.content, "     1\ta\n     2\tb\n     3\tc\n     4\td\n");
    // Последняя строка без перевода строки читается так же.
    write(&root, "b.txt", "x\ny");
    let o = call(&read, &root, json!({"path": "b.txt"})).await;
    assert_eq!(o.content, "     1\tx\n     2\ty\n");
}

#[tokio::test]
async fn read_default_limit_and_past_end() {
    let root = unique_dir("read_limit");
    let text: String = (1..=READ_DEFAULT_LIMIT + 10)
        .map(|i| format!("{i}\n"))
        .collect();
    write(&root, "big.txt", text);
    let read = read();
    let o = call(&read, &root, json!({"path": "big.txt"})).await;
    assert_eq!(o.content.lines().count(), READ_DEFAULT_LIMIT);
    assert!(o.content.ends_with("   500\t500\n"), "{}", o.content);
    let o = call(&read, &root, json!({"path": "big.txt", "start": 9999})).await;
    assert!(o.is_error);
    assert_eq!(
        o.content,
        "start 9999 is past the end of the file (510 lines)"
    );
    write(&root, "empty.txt", "");
    let o = call(&read, &root, json!({"path": "empty.txt"})).await;
    assert_eq!(o.content, "[file is empty]");
    assert!(!o.is_error);
}

#[tokio::test]
async fn read_missing_suggests_similar() {
    let root = unique_dir("read_missing");
    write(&root, "src/main.rs", "fn main() {}\n");
    write(&root, "src/lib.rs", "\n");
    let read = read();
    let o = call(&read, &root, json!({"path": "src/mian.rs"})).await;
    assert!(o.is_error);
    assert_eq!(o.content, "not found: src/mian.rs; similar: src/main.rs");
    // Похожих нет и каталога нет — без подсказки.
    let o = call(&read, &root, json!({"path": "src/zzzzzz.rs"})).await;
    assert_eq!(o.content, "not found: src/zzzzzz.rs");
    let o = call(&read, &root, json!({"path": "nope/x.rs"})).await;
    assert_eq!(o.content, "not found: nope/x.rs");
}

#[tokio::test]
async fn read_refuses_non_regular() {
    let root = unique_dir("read_non_regular");
    std::fs::create_dir_all(root.join("dir")).unwrap();
    let read = read();
    let o = call(&read, &root, json!({"path": "dir"})).await;
    assert!(o.is_error);
    assert!(o.content.starts_with("not a regular file"), "{}", o.content);
    #[cfg(unix)]
    {
        let st = std::process::Command::new("mkfifo")
            .arg(root.join("fifo"))
            .status()
            .expect("mkfifo не запустился");
        assert!(st.success());
        // Без писателя FIFO не блокирует чтение: отказ, а не ожидание.
        let o = call(&read, &root, json!({"path": "fifo"})).await;
        assert!(o.is_error);
        assert!(o.content.contains("not a regular file"), "{}", o.content);
    }
}

#[tokio::test]
async fn read_binary_is_described() {
    let root = unique_dir("read_binary");
    let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    png.resize(68, 0);
    write(&root, "i.png", &png);
    let read = read();
    let o = call(&read, &root, json!({"path": "i.png"})).await;
    assert_eq!(o.content, "binary file, 68 bytes, type image/png");
    assert!(!o.is_error);
    write(&root, "x.bin", b"ab\x00cd");
    let o = call(&read, &root, json!({"path": "x.bin"})).await;
    assert_eq!(
        o.content,
        "binary file, 5 bytes, type application/octet-stream"
    );
}

// ---- read: PDF через извлекатель (R1-C2 D113, задача 10) ----

/// Заглушка извлекателя: запоминает запросы и содержимое копии в момент вызова; отвечает
/// заданными страницами, ошибкой или не отвечает вовсе (`hang`).
#[derive(Default)]
struct StubPdf {
    pages: PdfText,
    error: Option<ExtractError>,
    hang: bool,
    seen: Mutex<Vec<(PdfRequest, Option<Vec<u8>>)>>,
}

impl StubPdf {
    fn pages(pages: &[(u32, &str)], total: u32) -> Arc<Self> {
        Arc::new(Self {
            pages: PdfText {
                pages_total: total,
                pages: pages.iter().map(|(n, t)| (*n, (*t).to_owned())).collect(),
            },
            ..Self::default()
        })
    }

    fn seen(&self) -> Vec<(PdfRequest, Option<Vec<u8>>)> {
        self.seen.lock().unwrap().clone()
    }
}

impl Extractor for StubPdf {
    fn html(&self, _req: HtmlRequest) -> BoxFuture<'static, Result<Markdown, ExtractError>> {
        Box::pin(async { Err(ExtractError::Failed("not used".into())) })
    }

    fn pdf(&self, req: PdfRequest) -> BoxFuture<'static, Result<PdfText, ExtractError>> {
        let copy = std::fs::read(&req.path).ok();
        self.seen.lock().unwrap().push((req, copy));
        let r = match &self.error {
            Some(e) => Err(e.clone()),
            None => Ok(self.pages.clone()),
        };
        let hang = self.hang;
        Box::pin(async move {
            if hang {
                std::future::pending::<()>().await;
            }
            r
        })
    }
}

fn read_pdf_tool(stub: &Arc<StubPdf>) -> Arc<dyn Tool> {
    let ex: Arc<dyn Extractor> = stub.clone();
    read_tool(ReadTracker::new(), Some(ex))
}

/// Байты «PDF»: сигнатура и двоичный комментарий без NUL — разбирает заглушка.
const PDF_BYTES: &[u8] = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n1 0 obj << >> endobj\n%%EOF\n";

#[tokio::test]
async fn read_pdf_pages() {
    let root = unique_dir("read_pdf_pages");
    write(&root, "doc.pdf", PDF_BYTES);
    let stub = StubPdf::pages(&[(1, "alpha"), (2, "beta")], 2);
    let o = call(
        &read_pdf_tool(&stub),
        &root,
        json!({"path": "doc.pdf", "start": 1, "limit": 2}),
    )
    .await;
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(
        o.content,
        "[pdf: 2 pages; showing 1-2]\n--- page 1 ---\nalpha\n--- page 2 ---\nbeta"
    );
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!((seen[0].0.first, seen[0].0.count), (1, 2));
    // Страница без текстового слоя и окно за концом документа.
    let stub = StubPdf::pages(&[(3, "gamma"), (4, "")], 9);
    let o = call(
        &read_pdf_tool(&stub),
        &root,
        json!({"path": "doc.pdf", "start": 3, "limit": 2}),
    )
    .await;
    assert_eq!(
        o.content,
        "[pdf: 9 pages; showing 3-4]\n--- page 3 ---\ngamma\n--- page 4 ---\n[page 4: no text layer]"
    );
    let stub = StubPdf::pages(&[], 2);
    let o = call(
        &read_pdf_tool(&stub),
        &root,
        json!({"path": "doc.pdf", "start": 5}),
    )
    .await;
    assert!(o.is_error);
    assert_eq!(
        o.content,
        "start 5 is past the last page of the pdf (2 pages)"
    );
}

#[tokio::test]
async fn read_pdf_limit_capped_at_50() {
    let root = unique_dir("read_pdf_limit");
    write(&root, "doc.pdf", PDF_BYTES);
    let stub = StubPdf::pages(&[(1, "a")], 1);
    let tool = read_pdf_tool(&stub);
    call(&tool, &root, json!({"path": "doc.pdf", "limit": 500})).await;
    call(&tool, &root, json!({"path": "doc.pdf"})).await;
    let counts: Vec<(u32, u32)> = stub
        .seen()
        .iter()
        .map(|(r, _)| (r.first, r.count))
        .collect();
    // Предел 50 страниц; по умолчанию — 10 страниц с первой (а не 500 строк).
    assert_eq!(counts, vec![(1, 50), (1, 10)]);
    assert!(
        stub.seen()
            .iter()
            .all(|(r, _)| r.max_chars == PDF_MAX_CHARS)
    );
}

#[tokio::test]
async fn read_pdf_uses_copy_in_work_tmp() {
    let root = unique_dir("read_pdf_copy");
    write(&root, "docs/doc.pdf", PDF_BYTES);
    let stub = StubPdf::pages(&[(1, "alpha")], 1);
    let run = RunId::new();
    let o = call_in(
        &read_pdf_tool(&stub),
        &root,
        run,
        json!({"path": "docs/doc.pdf"}),
    )
    .await;
    assert!(!o.is_error, "{}", o.content);
    let seen = stub.seen();
    let (req, copy) = &seen[0];
    // Копия — в `.zero-agent/work/tmp/<run>/`, а не путь проекта (TOCTOU D98), с теми же
    // байтами; после вызова её нет.
    let tmp = std::fs::canonicalize(root.join(format!(".zero-agent/work/tmp/{run}"))).unwrap();
    let parent = std::fs::canonicalize(req.path.parent().unwrap()).unwrap();
    assert_eq!(parent, tmp, "{}", req.path.display());
    assert!(
        req.path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("pdf-"),
        "{}",
        req.path.display()
    );
    assert_eq!(copy.as_deref(), Some(PDF_BYTES));
    assert!(
        !req.path.exists(),
        "копия не удалена: {}",
        req.path.display()
    );
    assert!(std::fs::read_dir(&tmp).unwrap().next().is_none());
}

#[tokio::test]
async fn read_pdf_over_32_mib_refused() {
    let root = unique_dir("read_pdf_big");
    let mut big = PDF_BYTES.to_vec();
    big.resize(33 * 1024 * 1024, b' ');
    write(&root, "big.pdf", &big);
    let stub = StubPdf::pages(&[(1, "a")], 1);
    let o = call(&read_pdf_tool(&stub), &root, json!({"path": "big.pdf"})).await;
    assert!(o.is_error);
    assert_eq!(o.content, "pdf larger than 33554432 bytes");
    assert!(stub.seen().is_empty(), "извлекатель не вызывается");
    // Ровно 32 МиБ — разбирается.
    big.truncate(32 * 1024 * 1024);
    write(&root, "edge.pdf", &big);
    let o = call(&read_pdf_tool(&stub), &root, json!({"path": "edge.pdf"})).await;
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(stub.seen().len(), 1);
}

#[tokio::test]
async fn read_pdf_without_extractor_is_binary() {
    let root = unique_dir("read_pdf_binary");
    // NUL в начале нет: PDF узнаётся по сигнатуре до проверки NUL.
    write(&root, "doc.pdf", PDF_BYTES);
    let o = call(&read(), &root, json!({"path": "doc.pdf"})).await;
    assert!(!o.is_error);
    assert_eq!(
        o.content,
        format!(
            "binary file, {} bytes, type application/pdf",
            PDF_BYTES.len()
        )
    );
}

#[tokio::test]
async fn read_pdf_extract_error_text() {
    let root = unique_dir("read_pdf_error");
    write(&root, "doc.pdf", PDF_BYTES);
    for (e, want) in [
        (ExtractError::Encrypted, "pdf: document is encrypted"),
        (
            ExtractError::Malformed("bad xref".into()),
            "pdf: malformed document: bad xref",
        ),
        (ExtractError::Timeout, "pdf: extractor timed out"),
    ] {
        let stub = Arc::new(StubPdf {
            error: Some(e),
            ..StubPdf::default()
        });
        let o = call(&read_pdf_tool(&stub), &root, json!({"path": "doc.pdf"})).await;
        assert!(o.is_error);
        assert_eq!(o.content, want);
    }
}

/// Извлекатель не ответил до дедлайна вызова: ошибка тайм-аута раньше внешнего тайм-аута
/// слота, копия удалена.
#[tokio::test]
async fn read_pdf_deadline_removes_copy() {
    let root = unique_dir("read_pdf_deadline");
    write(&root, "doc.pdf", PDF_BYTES);
    let stub = Arc::new(StubPdf {
        hang: true,
        ..StubPdf::default()
    });
    let run = RunId::new();
    let cx = ToolCx {
        deadline: Some(Instant::now() + Duration::from_millis(300)),
        ..cx(&root, run)
    };
    let limit = Duration::from_secs(30 * u64::from(scale()));
    let o = tokio::time::timeout(
        limit,
        read_pdf_tool(&stub).call(json!({"path": "doc.pdf"}), cx),
    )
    .await
    .expect("read не ответил после дедлайна");
    assert!(o.is_error);
    assert_eq!(o.content, "pdf: extractor timed out");
    let seen = stub.seen();
    assert!(!seen[0].0.path.exists(), "копия не удалена");
}

#[tokio::test]
async fn read_crlf_bom_cp1251_shown() {
    let root = unique_dir("read_encodings");
    write(&root, "crlf.txt", "x\r\ny\r\n");
    write(&root, "bom.txt", "\u{FEFF}hi\nthere\n");
    // «Привет» в Windows-1251.
    write(&root, "cp.txt", b"\xcf\xf0\xe8\xe2\xe5\xf2\nok\n");
    let read = read();
    let o = call(&read, &root, json!({"path": "crlf.txt"})).await;
    assert_eq!(o.content, "     1\tx\n     2\ty\n");
    let o = call(&read, &root, json!({"path": "bom.txt"})).await;
    assert_eq!(o.content, "     1\thi\n     2\tthere\n");
    let o = call(&read, &root, json!({"path": "cp.txt"})).await;
    assert!(!o.is_error);
    assert!(o.content.starts_with("     1\t\u{FFFD}"), "{}", o.content);
    assert!(o.content.contains("     2\tok\n"), "{}", o.content);
    assert!(
        o.content.ends_with("[file is not UTF-8]\n"),
        "{}",
        o.content
    );
}

#[tokio::test]
async fn read_counts_invisible() {
    let root = unique_dir("read_invisible");
    write(&root, "a.txt", "a\u{202E}b\nplain\n");
    let read = read();
    let o = call(&read, &root, json!({"path": "a.txt"})).await;
    // Символ не удаляется (иначе `old` у `edit` не совпал бы), а подсчитывается.
    assert_eq!(
        o.content,
        "     1\ta\u{202E}b\n     2\tplain\n[file contains 1 invisible or bidi control characters]\n"
    );
}

#[tokio::test]
async fn read_cuts_long_lines_and_caps_bytes() {
    let root = unique_dir("read_long");
    write(&root, "long.txt", format!("{}\nend\n", "x".repeat(2500)));
    let read = read();
    let o = call(&read, &root, json!({"path": "long.txt"})).await;
    assert_eq!(
        o.content,
        format!("     1\t{}... [line cut]\n     2\tend\n", "x".repeat(2000))
    );
    // Больше READ_MAX_BYTES: показано начало и пометка, трекер не отмечен.
    let line = format!("{}\n", "y".repeat(1023));
    write(&root, "huge.txt", line.repeat(READ_MAX_BYTES / 1024 + 4));
    let o = call(
        &read,
        &root,
        json!({"path": "huge.txt", "start": 4096, "limit": 2}),
    )
    .await;
    assert!(o.content.starts_with("  4096\t"), "{}", &o.content[..40]);
    assert!(
        o.content
            .ends_with("[file is larger than 4 MiB; only the first 4 MiB are read]\n"),
        "{}",
        &o.content[o.content.len() - 80..]
    );
}

#[tokio::test]
async fn read_marks_tracker_with_file_hash() {
    let root = unique_dir("read_tracker");
    write(&root, "a.txt", "\u{FEFF}one\r\ntwo\r\n");
    let tracker = ReadTracker::new();
    let read = read_tool(tracker.clone(), None);
    let run = RunId::new();
    let o = call_in(&read, &root, run, json!({"path": "a.txt", "limit": 1})).await;
    assert!(!o.is_error);
    let path = resolved(&root, "a.txt").path;
    let bytes = std::fs::read(&path).unwrap();
    let hash = blake3::hash(&bytes).to_hex().to_string();
    // Хэш — всего файла (сырые байты), даже при чтении части строк.
    assert_eq!(tracker.check(run, &path, &hash), Ok(()));
    assert_eq!(
        tracker.check(RunId::new(), &path, &hash),
        Err(TrackError::NotRead)
    );
}

#[tokio::test]
async fn read_bad_arguments_are_errors() {
    let root = unique_dir("read_args");
    let read = read();
    let o = call(&read, &root, json!({"start": 1})).await;
    assert!(o.is_error);
    assert!(
        o.content.starts_with("invalid arguments: "),
        "{}",
        o.content
    );
    let o = call(&read, &root, json!({"path": "a", "start": 0})).await;
    assert!(o.is_error);
    assert!(
        o.content.starts_with("invalid arguments: "),
        "{}",
        o.content
    );
}

#[cfg(windows)]
#[tokio::test]
async fn network_and_device_paths_rejected_before_io() {
    let root = unique_dir("network");
    let read = read();
    let o = call(&read, &root, json!({"path": r"\\server\share\x.txt"})).await;
    assert!(o.is_error);
    assert!(
        o.content.starts_with("network paths are not supported"),
        "{}",
        o.content
    );
    let o = call(&read, &root, json!({"path": r"\\.\PhysicalDrive0"})).await;
    assert!(
        o.content.starts_with("device paths are not supported"),
        "{}",
        o.content
    );
    let search = search_tool();
    let o = call(
        &search,
        &root,
        json!({"pattern": "x", "path": "//host/share"}),
    )
    .await;
    assert!(
        o.content.starts_with("network paths are not supported"),
        "{}",
        o.content
    );
    let glob = glob_tool();
    let o = call(&glob, &root, json!({"pattern": "//host/share/**/*.kdbx"})).await;
    assert!(
        o.content.starts_with("network paths are not supported"),
        "{}",
        o.content
    );
}

// ---- search ----

#[tokio::test]
async fn search_sorted_hidden_visible_gitignore_local_only() {
    let root = unique_dir("search_nogit");
    write(&root, "b.rs", "hit\n");
    write(&root, "a.rs", "hit\n");
    write(&root, ".github/w.yml", "secret: 1\n");
    write(&root, ".env", "secret=2\n");
    write(&root, ".git/config", "secret\n");
    write(&root, ".zero-agent/work/spill/s.txt", "secret\n");
    write(&root, ".gitignore", "ignored/\n");
    write(&root, "ignored/x.rs", "hit\nsecret\n");
    let search = search_tool();
    let o = call(&search, &root, json!({"pattern": "hit"})).await;
    assert_eq!(o.content, "a.rs:1:hit\nb.rs:1:hit");
    assert!(!o.is_error);
    // Скрытые файлы видны, служебные каталоги `.git` и `.zero-agent` — нет.
    let o = call(&search, &root, json!({"pattern": "secret"})).await;
    assert_eq!(o.content, ".env:1:secret=2\n.github/w.yml:1:secret: 1");
    // Каталог теста лежит в `target/` ZeroAgent: его `.gitignore` выдачу не меняет.
    assert!(root.starts_with(Path::new(env!("CARGO_TARGET_TMPDIR"))));

    // С git: `.gitignore` репозитория действует, родительский — нет.
    let g = git_dir("search_git");
    git_init(&g);
    write(&g, ".gitignore", "ignored/\n");
    write(&g, "a.rs", "hit\n");
    write(&g, "ignored/x.rs", "hit\n");
    write(&g, "target/t.rs", "hit\n");
    write(&g, ".git/info/exclude", "excluded.rs\n");
    write(&g, "excluded.rs", "hit\n");
    let o = call(&search, &g, json!({"pattern": "hit"})).await;
    assert_eq!(o.content, "a.rs:1:hit\ntarget/t.rs:1:hit");
}

#[tokio::test]
async fn search_filters_by_glob_and_path() {
    let root = unique_dir("search_glob");
    write(&root, "a.rs", "hit\n");
    write(&root, "a.txt", "hit\n");
    write(&root, "sub/b.rs", "x\nhit here\n");
    write(&root, "sub/deep/c.rs", "hit\n");
    let search = search_tool();
    let o = call(&search, &root, json!({"pattern": "hit", "glob": "*.rs"})).await;
    assert_eq!(
        o.content,
        "a.rs:1:hit\nsub/b.rs:2:hit here\nsub/deep/c.rs:1:hit"
    );
    let o = call(
        &search,
        &root,
        json!({"pattern": "hit", "path": "sub", "glob": "deep/*.rs"}),
    )
    .await;
    assert_eq!(o.content, "sub/deep/c.rs:1:hit");
    let o = call(
        &search,
        &root,
        json!({"pattern": "h.t", "path": "sub/b.rs"}),
    )
    .await;
    assert_eq!(o.content, "sub/b.rs:2:hit here");
    let o = call(&search, &root, json!({"pattern": "nothing-like-this"})).await;
    assert_eq!(o.content, "no matches");
    assert!(!o.is_error);
    let o = call(&search, &root, json!({"pattern": "("})).await;
    assert!(o.is_error);
    assert!(o.content.starts_with("invalid pattern: "), "{}", o.content);
    let o = call(&search, &root, json!({"pattern": "x", "path": "absent"})).await;
    assert!(o.is_error);
    assert_eq!(o.content, "not found: absent");
}

#[tokio::test]
async fn search_caps_matches() {
    let root = unique_dir("search_cap");
    write(&root, "many.txt", "hit\n".repeat(250));
    write(&root, "wide.txt", format!("{}hit\n", "w".repeat(400)));
    let search = search_tool();
    let o = call(
        &search,
        &root,
        json!({"pattern": "hit", "path": "many.txt"}),
    )
    .await;
    let lines: Vec<&str> = o.content.lines().collect();
    assert_eq!(lines.len(), SEARCH_MAX + 1);
    assert_eq!(lines[0], "many.txt:1:hit");
    assert_eq!(lines[SEARCH_MAX - 1], "many.txt:200:hit");
    assert_eq!(lines[SEARCH_MAX], "... 50 more matches");
    let o = call(
        &search,
        &root,
        json!({"pattern": "hit", "path": "wide.txt"}),
    )
    .await;
    assert_eq!(
        o.content,
        format!("wide.txt:1:{}... [line cut]", "w".repeat(300))
    );
}

#[tokio::test]
async fn search_stops_at_deadline() {
    let root = unique_dir("search_deadline");
    write(&root, "a.rs", "hit\n");
    let base = resolved(&root, ".");
    let r = roots(&root);
    // Дедлайн уже прошёл: обход останавливается сразу, без порогов времени.
    let limits = WalkLimits::new(Instant::now());
    let o = search_in(&r.project, &base, "hit", None, &limits);
    assert_eq!(o.content, "no matches\n... search stopped after 0 s");
    // Флаг отмены тоже останавливает обход.
    let limits = WalkLimits::new(Instant::now() + Duration::from_secs(3600));
    limits.cancel();
    let o = search_in(&r.project, &base, "hit", None, &limits);
    assert!(o.content.contains("search stopped"), "{}", o.content);
}

#[tokio::test]
async fn walk_does_not_follow_dir_links() {
    let base = unique_dir("walk_links");
    let (root, outside) = (base.join("project"), base.join("outside"));
    write(&outside, "o.rs", "hit\n");
    write(&root, "a.rs", "hit\n");
    if dir_link(&root.join("l"), &outside).is_none() {
        return;
    }
    let search = search_tool();
    let o = call(&search, &root, json!({"pattern": "hit"})).await;
    assert_eq!(o.content, "a.rs:1:hit");
    let glob = glob_tool();
    let o = call(&glob, &root, json!({"pattern": "**/*.rs"})).await;
    assert_eq!(o.content, "a.rs");
    // Ссылка на файл тоже пропускается.
    if file_link(&root.join("f.rs"), &outside.join("o.rs")).is_none() {
        return;
    }
    let o = call(&search, &root, json!({"pattern": "hit"})).await;
    assert_eq!(o.content, "a.rs:1:hit");
    let o = call(&glob, &root, json!({"pattern": "*.rs"})).await;
    assert_eq!(o.content, "a.rs");
}

/// C1 ревью: файлы правил читаются только через `open_checked`. Файл правил — ссылка (на
/// устройство, на сетевой путь), FIFO или файл больше 1 МиБ — пропускается: обход не
/// виснет, память не растёт, к цели ссылки обращения нет, а правила этого файла не
/// действуют.
#[tokio::test]
async fn unsafe_rule_files_are_skipped() {
    let search = search_tool();
    let glob = glob_tool();
    // Больше предела: первая строка `ignored/` не действует.
    let root = unique_dir("rules_big");
    write(&root, "ignored/x.rs", "hit\n");
    // Строки-комментарии: сам файл ищется как обычный, без строки длиннее предела.
    let big = format!("ignored/\n{}", "#\n".repeat(1 << 20));
    write(&root, ".gitignore", &big);
    let o = call(&search, &root, json!({"pattern": "hit"})).await;
    assert_eq!(o.content, "ignored/x.rs:1:hit");
    let o = call(&glob, &root, json!({"pattern": "**/*.rs"})).await;
    assert_eq!(o.content, "ignored/x.rs");

    // Ссылка `.gitignore` на устройство: Unix — `/dev/zero` (раньше — OOM), Windows —
    // `\\.\NUL`.
    let root = unique_dir("rules_device");
    write(&root, "a.rs", "hit\n");
    let device = if cfg!(windows) {
        r"\\.\NUL"
    } else {
        "/dev/zero"
    };
    if file_link(&root.join(".gitignore"), Path::new(device)).is_some() {
        let o = call(&search, &root, json!({"pattern": "hit"})).await;
        assert_eq!(o.content, "a.rs:1:hit");
    }

    // Windows: ссылка `.ignore` на UNC — без SMB-запроса (ссылка не открывается).
    #[cfg(windows)]
    {
        let root = unique_dir("rules_unc");
        write(&root, "a.rs", "hit\n");
        if file_link(
            &root.join(".ignore"),
            Path::new(r"\\127.0.0.1\nonexistent\x"),
        )
        .is_some()
        {
            let o = call(&search, &root, json!({"pattern": "hit"})).await;
            assert_eq!(o.content, "a.rs:1:hit");
        }
    }

    // Unix: `.gitignore` — FIFO без писателя (раньше — вечная блокировка потока).
    #[cfg(unix)]
    {
        let root = unique_dir("rules_fifo");
        write(&root, "sub/a.rs", "hit\n");
        let st = std::process::Command::new("mkfifo")
            .arg(root.join("sub/.gitignore"))
            .status()
            .expect("mkfifo не запустился");
        assert!(st.success());
        let o = call(&search, &root, json!({"pattern": "hit"})).await;
        assert_eq!(o.content, "sub/a.rs:1:hit");
    }
}

/// I1 ревью: правила от корня проекта действуют, когда поиск идёт по подкаталогу; `!` в
/// более глубоком `.gitignore` открывает путь.
#[tokio::test]
async fn root_gitignore_applies_to_subdir_search() {
    let root = unique_dir("rules_subdir");
    write(&root, ".gitignore", "sub/gen/\n*.log\n");
    write(&root, "sub/gen/x.rs", "hit\n");
    write(&root, "sub/a.rs", "hit\n");
    write(&root, "sub/drop.log", "hit\n");
    write(&root, "sub/.gitignore", "!keep.log\n");
    write(&root, "sub/keep.log", "hit\n");
    let search = search_tool();
    let o = call(&search, &root, json!({"pattern": "hit", "path": "sub"})).await;
    assert_eq!(o.content, "sub/a.rs:1:hit\nsub/keep.log:1:hit");
    let glob = glob_tool();
    let o = call(&glob, &root, json!({"pattern": "sub/**/*.rs"})).await;
    assert_eq!(o.content, "sub/a.rs");
    let o = call(&glob, &root, json!({"pattern": "sub/**"})).await;
    assert_eq!(o.content, "sub/.gitignore\nsub/a.rs\nsub/keep.log");
}

/// I2 ревью: строка длиннее предела памяти — файл пропускается с пометкой, а явно указанный
/// файл даёт ошибку (I4); отмена действует внутри файла без совпадений.
#[tokio::test]
async fn search_line_limit_and_cancel_inside_file() {
    let root = unique_dir("search_heap");
    let mut long = vec![b'x'; 2 << 20];
    long.extend_from_slice(b"hit\n");
    write(&root, "long.txt", &long);
    write(&root, "ok.txt", "hit\n");
    let search = search_tool();
    let o = call(&search, &root, json!({"pattern": "hit"})).await;
    assert_eq!(
        o.content,
        "ok.txt:1:hit\n[1 files skipped: a line is longer than 1 MiB]"
    );
    let o = call(
        &search,
        &root,
        json!({"pattern": "hit", "path": "long.txt"}),
    )
    .await;
    assert!(o.is_error);
    assert_eq!(
        o.content,
        "cannot search long.txt: a line is longer than 1 MiB"
    );
    // Отмена до первого блока явно указанного файла: обхода нет, остановку видит чтение.
    write(&root, "many.txt", "line\n".repeat(200_000));
    let file = resolved(&root, "many.txt");
    let limits = WalkLimits::new(Instant::now() + Duration::from_secs(3600));
    limits.cancel();
    let o = search_in(&roots(&root).project, &file, "hit", None, &limits);
    assert_eq!(o.content, "no matches\n... search stopped after 0 s");
    assert!(!o.is_error);
}

/// I4 ревью (Unix): явно указанный FIFO — ошибка с причиной, а не `no matches`.
#[cfg(unix)]
#[tokio::test]
async fn search_explicit_unreadable_file_is_error() {
    let root = unique_dir("search_fifo");
    let st = std::process::Command::new("mkfifo")
        .arg(root.join("p"))
        .status()
        .expect("mkfifo не запустился");
    assert!(st.success());
    let o = call(&search_tool(), &root, json!({"pattern": "x", "path": "p"})).await;
    assert!(o.is_error);
    assert!(o.content.contains("not a regular file"), "{}", o.content);
}

// ---- glob ----

#[tokio::test]
async fn glob_lists_sorted_without_work_dir() {
    let root = unique_dir("glob_list");
    write(&root, "b.txt", "");
    write(&root, "a.txt", "");
    write(&root, "a/z.txt", "");
    write(&root, "sub/c.txt", "");
    write(&root, "sub/c.rs", "");
    write(&root, ".zero-agent/work/spill/x.txt", "");
    write(&root, ".git/x.txt", "");
    let glob = glob_tool();
    let o = call(&glob, &root, json!({"pattern": "**/*.txt"})).await;
    assert_eq!(o.content, "a.txt\na/z.txt\nb.txt\nsub/c.txt");
    assert!(!o.is_error);
    let o = call(&glob, &root, json!({"pattern": "*.txt"})).await;
    assert_eq!(o.content, "a.txt\nb.txt");
    let o = call(&glob, &root, json!({"pattern": "sub/*"})).await;
    assert_eq!(o.content, "sub/c.rs\nsub/c.txt");
    let o = call(&glob, &root, json!({"pattern": "sub/c.rs"})).await;
    assert_eq!(o.content, "sub/c.rs");
    let o = call(&glob, &root, json!({"pattern": "**/*.none"})).await;
    assert_eq!(o.content, "no files matched");
    let o = call(&glob, &root, json!({"pattern": "a["})).await;
    assert!(o.is_error);
    assert!(o.content.starts_with("invalid pattern: "), "{}", o.content);
}

#[tokio::test]
async fn glob_caps_files() {
    let root = unique_dir("glob_cap");
    for i in 0..GLOB_MAX + 7 {
        write(&root, &format!("f{i:04}.txt"), "");
    }
    let glob = glob_tool();
    let o = call(&glob, &root, json!({"pattern": "*.txt"})).await;
    let lines: Vec<&str> = o.content.lines().collect();
    assert_eq!(lines.len(), GLOB_MAX + 1);
    assert_eq!(lines[0], "f0000.txt");
    assert_eq!(lines[GLOB_MAX - 1], "f0499.txt");
    assert_eq!(lines[GLOB_MAX], "... 7 more files");
}

#[test]
fn glob_subjects_absolute_prefix_is_outside() {
    let glob = glob_tool();
    let s = glob.subjects(&json!({"pattern": "C:/**/*.kdbx"}));
    assert_eq!(s.paths[0].arg, "C:/");
    assert_eq!(s.paths[0].access, Access::Read);
    let arg = |p: &str| glob.subjects(&json!({ "pattern": p })).paths[0].arg.clone();
    assert_eq!(arg("**/*.rs"), ".");
    assert_eq!(arg("src/**/*.rs"), "src/");
    assert_eq!(arg("../x/*.rs"), "../x/");
    assert_eq!(arg("/etc/*.conf"), "/etc/");
    assert_eq!(arg("src/main.rs"), "src/");
    assert_eq!(
        glob_split("src/ma*.rs"),
        ("src/".to_owned(), "ma*.rs".to_owned())
    );
    assert!(glob.subjects(&json!({})).paths.is_empty());
}

#[test]
fn read_and_search_subjects() {
    let s = read().subjects(&json!({"path": "../x.txt"}));
    assert_eq!(s.paths.len(), 1);
    assert_eq!(
        (s.paths[0].arg.as_str(), s.paths[0].access),
        ("../x.txt", Access::Read)
    );
    let search = search_tool();
    let s = search.subjects(&json!({"pattern": "x"}));
    assert_eq!(
        (s.paths[0].arg.as_str(), s.paths[0].access),
        (".", Access::Read)
    );
    let s = search.subjects(&json!({"pattern": "x", "path": "/etc"}));
    assert_eq!(s.paths[0].arg, "/etc");
    assert!(s.command.is_none() && s.segments.is_empty() && !s.network);
}

// ---- контракт (таблица D74) ----

/// Строка таблицы D74: инструмент, имя, тир, тайм-аут (с), предел вывода (байт).
type Row = (Arc<dyn Tool>, &'static str, Tier, u64, usize);

#[test]
fn contract_matches_d74() {
    let roles = vec![RunRole::Top, RunRole::Orchestrator, RunRole::Worker];
    let table: [Row; 3] = [
        (read(), "read", Tier::Small, 30, 128 * 1024),
        (search_tool(), "search", Tier::Small, 60, 64 * 1024),
        (glob_tool(), "glob", Tier::Standard, 30, 64 * 1024),
    ];
    for (tool, name, tier, secs, limit) in table {
        let s = tool.spec();
        assert_eq!(s.schema.name, name);
        assert_eq!(s.kind, ToolKind::Read, "{name}");
        assert_eq!(s.class, EffectClass::ReadOnly, "{name}");
        assert_eq!(s.risk, RiskClass::Read, "{name}");
        assert_eq!(s.min_tier, tier, "{name}");
        assert_eq!(s.timeout, Duration::from_secs(secs), "{name}");
        assert_eq!(s.output_limit, Some(limit), "{name}");
        assert!(!s.tainted, "{name}");
        assert_eq!(s.approval, Approval::Mode, "{name}");
        assert_eq!(s.roles, roles, "{name}");
        // Описания — английский текст, только ASCII (Global Constraints).
        let schema = serde_json::to_string(&s.schema.parameters).unwrap();
        assert!(
            s.schema.description.is_ascii() && schema.is_ascii(),
            "{name}"
        );
    }
}

// Golden текстов нативных инструментов (D94) — `tests/native.rs` (по `native_capability`).

// ---- трекер и подсказки ----

#[test]
fn tracker_detects_change() {
    let t = ReadTracker::new();
    let run = RunId::new();
    let (p, q) = (Path::new("/p/a.rs"), Path::new("/p/b.rs"));
    t.mark(run, p, "h1");
    assert_eq!(t.check(run, p, "h1"), Ok(()));
    assert_eq!(t.check(run, p, "h2"), Err(TrackError::Changed));
    assert_eq!(t.check(run, q, "h1"), Err(TrackError::NotRead));
    // Своя правка обновляет хэш.
    t.mark(run, p, "h2");
    assert_eq!(t.check(run, p, "h2"), Ok(()));
}

#[test]
fn nearest_by_damerau_levenshtein() {
    let c: Vec<String> = ["main.rs", "mod.rs", "lib.rs", "maim.rs", "mian.rs.bak"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    // Перестановка соседних — одна операция; сортировка — по расстоянию, затем по имени.
    assert_eq!(nearest("mian.rs", &c, 3), ["main.rs", "maim.rs"]);
    assert_eq!(nearest("mian.rs", &c, 1), ["main.rs"]);
    assert!(nearest("zzzzzz", &c, 3).is_empty());
    // Само имя не подсказывается.
    assert_eq!(nearest("lib.rs", &c, 3), Vec::<String>::new());
}
