//! `edit` и `multi_edit` (R1-C1 D74, D88, D98): толерантный патчер, защита от перезаписи,
//! трекер чтений, сохранение формата файла, запись по каноническому пути, маркер повтора,
//! отказы по векторам обхода границы и атомарность при подмене между проверкой и записью.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use za_core::paths::{Roots, classify, resolve};
use za_core::{Access, Approval, Postcondition, RiskClass, Tool, ToolCx, ToolKind, ToolOutput};
use za_protocol::{EffectClass, EffectKey, RunId, RunRole, Tier};
use za_tools::{
    EditHook, EditPoint, MatchLevel, PatchError, ReadTracker, TrackError, apply, edit_tool,
    edit_tool_with_hook, multi_edit_tool, read_tool, shrink_guard,
};

#[path = "../../../test-support/unique_dir.rs"]
mod shared_dir;

/// Чистый каталог теста `CARGO_TARGET_TMPDIR/za-tools-edit/<name>-<pid>-<n>`.
fn unique_dir(name: &str) -> PathBuf {
    shared_dir::unique_dir("za-tools-edit", name)
}

fn write(root: &Path, rel: &str, bytes: impl AsRef<[u8]>) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

fn bytes(root: &Path, rel: &str) -> Vec<u8> {
    std::fs::read(root.join(rel)).unwrap()
}

/// Множитель тайм-аутов теста `ZA_TEST_TIME_SCALE` (целое 1–6).
fn scale() -> u32 {
    std::env::var("ZA_TEST_TIME_SCALE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1)
        .clamp(1, 6)
}

/// Run, корень и ключ эффекта одного вызова.
#[derive(Clone)]
struct Cx {
    root: PathBuf,
    run: RunId,
    step: u64,
}

impl Cx {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            run: RunId::new(),
            step: 0,
        }
    }

    /// Следующий ключ эффекта (новый вызов).
    fn next(&mut self) -> ToolCx {
        self.step += 1;
        self.same()
    }

    /// Тот же ключ эффекта, что у прошлого вызова (повтор).
    fn same(&self) -> ToolCx {
        ToolCx {
            run: self.run,
            project_dir: self.root.clone(),
            call_id: format!("c{}", self.step),
            effect_key: EffectKey::derive(self.run, self.step, 1),
        }
    }
}

/// Вызов инструмента под тайм-аутом (никакое ожидание теста не виснет).
async fn call_cx(tool: &Arc<dyn Tool>, cx: ToolCx, args: Value) -> ToolOutput {
    let limit = Duration::from_secs(60 * u64::from(scale()));
    tokio::time::timeout(limit, tool.call(args, cx))
        .await
        .unwrap_or_else(|_| panic!("инструмент не ответил за {limit:?}"))
}

/// Набор `read` + `edit` + `multi_edit` с общим трекером.
struct Kit {
    cx: Cx,
    read: Arc<dyn Tool>,
    edit: Arc<dyn Tool>,
    multi: Arc<dyn Tool>,
}

impl Kit {
    fn new(root: &Path) -> Self {
        let t = ReadTracker::new();
        Self {
            cx: Cx::new(root),
            read: read_tool(t.clone()),
            edit: edit_tool(t.clone()),
            multi: multi_edit_tool(t),
        }
    }

    async fn read(&mut self, path: &str) -> ToolOutput {
        let o = call_cx(&self.read, self.cx.next(), json!({ "path": path })).await;
        assert!(!o.is_error, "read {path}: {}", o.content);
        o
    }

    async fn edit(&mut self, args: Value) -> ToolOutput {
        call_cx(&self.edit, self.cx.next(), args).await
    }

    async fn multi(&mut self, args: Value) -> ToolOutput {
        call_cx(&self.multi, self.cx.next(), args).await
    }
}

fn canonical(root: &Path, rel: &str) -> PathBuf {
    let r = Roots::canonical(root, &root.join("no-home")).unwrap();
    resolve(&r, &classify(&r.project, rel).unwrap(), Access::Read)
        .unwrap()
        .path
}

/// Временные файлы правки (`.za-edit-*.tmp`) в каталоге.
fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".za-edit-"))
        .collect()
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

/// Пропуск теста ссылок без прав. `ZA_REQUIRE_LINK_TESTS=1` (CI) превращает пропуск в
/// падение: доказательство обязательно. На Unix ссылки создаются без прав.
#[cfg(windows)]
fn skip(why: &str) {
    if std::env::var("ZA_REQUIRE_LINK_TESTS").as_deref() == Ok("1") {
        panic!("ZA_REQUIRE_LINK_TESTS=1, а проверка пропущена: {why}");
    }
    eprintln!("ПРОПУСК: {why}");
}

// ---- патчер ----

#[test]
fn exact_then_fuzzy_levels() {
    assert_eq!(
        apply("a\nb\n", "b", "c").unwrap(),
        ("a\nc\n".into(), MatchLevel::Exact)
    );
    assert_eq!(apply("a\r\nb\r\n", "a\nb", "x\ny").unwrap().0, "x\r\ny\r\n");
    assert_eq!(
        apply("\tif a {\n\t\tb()\n", "if a {\n    b()", "if a {\n    c()").unwrap(),
        ("\tif a {\n\t\tc()\n".into(), MatchLevel::Indent)
    );
    assert_eq!(apply("a\nlast", "last", "end").unwrap().0, "a\nend"); // без \n в конце
}

#[test]
fn levels_line_endings_and_trailing_space() {
    // Уровень 2 называется LineEndings.
    assert_eq!(
        apply("a\r\nb\r\n", "a\nb", "x").unwrap().1,
        MatchLevel::LineEndings
    );
    // Хвостовые пробелы файла и old не мешают; new вставляется как есть.
    assert_eq!(
        apply("x = 1;  \ny = 2;\n", "x = 1;\ny = 2;", "z = 3;").unwrap(),
        ("z = 3;\n".into(), MatchLevel::TrailingSpace)
    );
    // new в CRLF-файле получает CRLF и на точном уровне.
    assert_eq!(
        apply("a\r\nb\r\n", "b", "c\nd").unwrap().0,
        "a\r\nc\r\nd\r\n"
    );
    // Смешанный файл: замена трогает только найденное место.
    assert_eq!(apply("a\r\nb\nc\r\n", "b", "B").unwrap().0, "a\r\nB\nc\r\n");
    // old с завершающим \n на построчном уровне.
    assert_eq!(
        apply("    a\n    b\nc\n", "a\nb\n", "x\n").unwrap(),
        ("    x\nc\n".into(), MatchLevel::Indent)
    );
}

#[test]
fn ambiguous_lists_lines() {
    assert_eq!(
        apply("x\ny\nx\n", "x", "z"),
        Err(PatchError::Ambiguous { lines: vec![1, 3] })
    );
    // Неоднозначность на построчном уровне — тоже с номерами строк.
    assert_eq!(
        apply("  f()\n g()\n\tf()\n", "f()", "h()"),
        Err(PatchError::Ambiguous { lines: vec![1, 3] })
    );
}

#[test]
fn not_found_offers_closest_region() {
    let text = "fn a() {}\nfn main() {\n    run();\n}\n";
    match apply(text, "fn mian() {\n    run();", "x") {
        Err(PatchError::NotFound {
            closest: Some((2, 3, region)),
        }) => assert_eq!(region, "fn main() {\n    run();"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        apply("alpha\nbeta\n", "zzzzzzzz qqqq", "x"),
        Err(PatchError::NotFound { closest: None })
    );
    assert_eq!(apply("a\n", "", "b"), Err(PatchError::Exists));
    assert_eq!(apply("", "", "b").unwrap().0, "b");
}

#[test]
fn shrink_guard_corpus() {
    let lines = |n: usize| "x\n".repeat(n);
    // Срабатывает: 40 → 10 строк без allow.
    assert_eq!(
        shrink_guard(&lines(40), &lines(10), false),
        Err(PatchError::Shrink {
            removed: 30,
            total: 40
        })
    );
    // Молчит: 40 → 25; 10 → 1 (файл короче 20 строк); 40 → 10 с allow; рост; ровно половина.
    assert_eq!(shrink_guard(&lines(40), &lines(25), false), Ok(()));
    assert_eq!(shrink_guard(&lines(10), &lines(1), false), Ok(()));
    assert_eq!(shrink_guard(&lines(40), &lines(10), true), Ok(()));
    assert_eq!(shrink_guard(&lines(20), &lines(40), false), Ok(()));
    assert_eq!(shrink_guard(&lines(40), &lines(20), false), Ok(()));
}

// ---- трекер, результат, формат ----

#[tokio::test]
async fn edit_requires_read_and_detects_change() {
    let root = unique_dir("requires_read");
    write(&root, "a.rs", "1\n");
    let mut k = Kit::new(&root);
    let args = json!({"path": "a.rs", "old": "1", "new": "2"});
    let o = k.edit(args.clone()).await;
    assert!(o.is_error);
    assert_eq!(o.content, "file not read in this run; read it first");
    k.read("a.rs").await;
    write(&root, "a.rs", "1\n\n");
    let o = k.edit(args.clone()).await;
    assert!(o.is_error);
    assert_eq!(o.content, "file changed since last read; read it again");
    assert_eq!(bytes(&root, "a.rs"), b"1\n\n");
    // Своя правка обновляет трекер: вторая правка подряд без чтения проходит.
    k.read("a.rs").await;
    assert!(!k.edit(args).await.is_error);
    let o = k
        .edit(json!({"path": "a.rs", "old": "2", "new": "3"}))
        .await;
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(bytes(&root, "a.rs"), b"3\n\n");
}

#[tokio::test]
async fn edit_result_has_diff_and_hash() {
    let root = unique_dir("result");
    write(&root, "a.rs", "1\n");
    let mut k = Kit::new(&root);
    k.read("a.rs").await;
    let o = k
        .edit(json!({"path": "a.rs", "old": "1", "new": "2"}))
        .await;
    assert!(!o.is_error, "{}", o.content);
    assert!(
        o.content.starts_with("edited a.rs: +1 -1 lines, blake3 "),
        "{}",
        o.content
    );
    assert!(o.content.contains("-1\n+2"), "{}", o.content);
    assert!(o.content.is_ascii() || o.content.contains("a.rs")); // пунктуация ASCII
    let before = blake3::hash(b"1\n").to_hex().to_string();
    let after = blake3::hash(b"2\n").to_hex().to_string();
    assert!(
        o.content
            .contains(&format!("blake3 {}->{}", &before[..8], &after[..8])),
        "{}",
        o.content
    );
    match &o.post[..] {
        [Postcondition::FileHash { path, blake3 }] => {
            assert_eq!(path, &canonical(&root, "a.rs"));
            assert_eq!(blake3, &after);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn edit_keeps_bom_permissions_refuses_cp1251() {
    let root = unique_dir("format");
    write(&root, "bom.txt", b"\xEF\xBB\xBFone\r\ntwo\r\n");
    write(&root, "lf.txt", "one\ntwo");
    // «Привет» в Windows-1251.
    let cp1251: &[u8] = b"\xCF\xF0\xE8\xE2\xE5\xF2\n";
    write(&root, "ru.txt", cp1251);
    let mut k = Kit::new(&root);

    // BOM и CRLF на месте, new в стиле файла.
    k.read("bom.txt").await;
    let o = k
        .edit(json!({"path": "bom.txt", "old": "one\ntwo", "new": "1\n2\n3"}))
        .await;
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(bytes(&root, "bom.txt"), b"\xEF\xBB\xBF1\r\n2\r\n3\r\n");

    // Без \n в конце — так и остаётся.
    k.read("lf.txt").await;
    assert!(
        !k.edit(json!({"path": "lf.txt", "old": "two", "new": "2"}))
            .await
            .is_error
    );
    assert_eq!(bytes(&root, "lf.txt"), b"one\n2");

    // Не UTF-8: отказ, байты целы; трекер такой файл не отмечает (Carry задачи 6).
    let tracker = ReadTracker::new();
    let o = call_cx(
        &read_tool(tracker.clone()),
        k.cx.next(),
        json!({"path": "ru.txt"}),
    )
    .await;
    assert!(o.content.contains("[file is not UTF-8]"), "{}", o.content);
    let hash = blake3::hash(cp1251).to_hex().to_string();
    assert_eq!(
        tracker.check(k.cx.run, &canonical(&root, "ru.txt"), &hash),
        Err(TrackError::NotRead)
    );
    k.read("ru.txt").await;
    let o = k
        .edit(json!({"path": "ru.txt", "old": "\u{FFFD}", "new": "x"}))
        .await;
    assert!(o.is_error);
    assert_eq!(o.content, "file is not UTF-8; edit refused");
    assert_eq!(bytes(&root, "ru.txt"), cp1251);

    // Unix: +x сохранён.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        write(&root, "run.sh", "echo 1\n");
        let p = root.join("run.sh");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o750)).unwrap();
        k.read("run.sh").await;
        let o = k
            .edit(json!({"path": "run.sh", "old": "1", "new": "2"}))
            .await;
        assert!(!o.is_error, "{}", o.content);
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o750);
    }
    assert!(leftovers(&root).is_empty());
}

#[tokio::test]
async fn edit_reports_match_level_and_ambiguity() {
    let root = unique_dir("level");
    write(&root, "a.rs", "\tif a {\n\t\tb()\n\t}\n");
    write(&root, "b.rs", "x\ny\nx\n");
    let mut k = Kit::new(&root);
    k.read("a.rs").await;
    let o = k
        .edit(json!({"path": "a.rs", "old": "if a {\n    b()", "new": "if a {\n    c()"}))
        .await;
    assert!(!o.is_error, "{}", o.content);
    assert!(
        o.content
            .contains("[old matched ignoring indentation; new was re-indented]")
    );
    assert_eq!(bytes(&root, "a.rs"), b"\tif a {\n\t\tc()\n\t}\n");
    k.read("b.rs").await;
    let o = k
        .edit(json!({"path": "b.rs", "old": "x", "new": "z"}))
        .await;
    assert!(o.is_error);
    assert_eq!(
        o.content,
        "old matches 2 places (lines 1, 3); add surrounding lines"
    );
    let o = k
        .edit(json!({"path": "b.rs", "old": "nothing like it at all", "new": "z"}))
        .await;
    assert_eq!(o.content, "old not found");
    let o = k
        .edit(json!({"path": "b.rs", "old": "x", "new": "x"}))
        .await;
    assert_eq!(o.content, "new is the same as old; nothing to change");
    assert_eq!(bytes(&root, "b.rs"), b"x\ny\nx\n");
}

#[tokio::test]
async fn shrink_needs_allow() {
    let root = unique_dir("shrink");
    let body: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    let keep: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    write(&root, "a.txt", &body);
    let mut k = Kit::new(&root);
    k.read("a.txt").await;
    let old = &body[keep.len()..];
    let o = k
        .edit(json!({"path": "a.txt", "old": old, "new": ""}))
        .await;
    assert!(o.is_error);
    assert_eq!(
        o.content,
        "edit removes 30 of 40 lines; pass allow_shrink if intended"
    );
    assert_eq!(bytes(&root, "a.txt"), body.as_bytes());
    let o = k
        .edit(json!({"path": "a.txt", "old": old, "new": "", "allow_shrink": true}))
        .await;
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(bytes(&root, "a.txt"), keep.as_bytes());
}

#[tokio::test]
async fn edit_through_link_writes_target_not_link() {
    let root = unique_dir("through_link");
    write(&root, "b.rs", "1\n");
    if file_link(&root.join("a.rs"), &root.join("b.rs")).is_none() {
        return;
    }
    let mut k = Kit::new(&root);
    k.read("a.rs").await;
    let o = k
        .edit(json!({"path": "a.rs", "old": "1", "new": "2"}))
        .await;
    assert!(!o.is_error, "{}", o.content);
    assert!(
        std::fs::symlink_metadata(root.join("a.rs"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(bytes(&root, "b.rs"), b"2\n");
    // Путь в выводе — канонический (цель ссылки).
    assert!(o.content.starts_with("edited b.rs:"), "{}", o.content);
}

#[tokio::test]
async fn empty_old_creates_but_not_overwrites() {
    let root = unique_dir("create");
    write(&root, "full.txt", "keep\n");
    let mut k = Kit::new(&root);
    // Новый файл без read, с недостающими каталогами.
    let o = k
        .edit(json!({"path": "src/new/a.rs", "old": "", "new": "fn a() {}\n"}))
        .await;
    assert!(!o.is_error, "{}", o.content);
    assert!(
        o.content
            .starts_with("created src/new/a.rs: +1 -0 lines, blake3 none->"),
        "{}",
        o.content
    );
    assert_eq!(bytes(&root, "src/new/a.rs"), b"fn a() {}\n");
    // Созданный файл можно править без чтения: трекер отмечен своей правкой.
    let o = k
        .edit(json!({"path": "src/new/a.rs", "old": "a()", "new": "b()"}))
        .await;
    assert!(!o.is_error, "{}", o.content);
    // Существующий непустой — ошибка, файл цел.
    let o = k
        .edit(json!({"path": "full.txt", "old": "", "new": "x"}))
        .await;
    assert!(o.is_error);
    assert_eq!(bytes(&root, "full.txt"), b"keep\n");
    let o = k
        .edit(json!({"path": "full.txt", "old": "", "new": "x"}))
        .await;
    assert!(o.is_error);
    k.read("full.txt").await;
    let o = k
        .edit(json!({"path": "full.txt", "old": "", "new": "x"}))
        .await;
    assert_eq!(
        o.content,
        "file already exists and is not empty; put the text to replace in old"
    );
    // Нет файла и old не пуст — «не найден» с похожими именами.
    let o = k
        .edit(json!({"path": "ful.txt", "old": "a", "new": "b"}))
        .await;
    assert_eq!(o.content, "not found: ful.txt; similar: full.txt");
}

#[tokio::test]
async fn retry_with_marker_is_idempotent_after_restart() {
    let root = unique_dir("marker");
    write(&root, "a.rs", "1\n");
    let mut k = Kit::new(&root);
    k.read("a.rs").await;
    let args = json!({"path": "a.rs", "old": "1", "new": "2"});
    let first = k.edit(args.clone()).await;
    assert!(!first.is_error, "{}", first.content);
    let cx = k.cx.same();
    // Маркер называется по file_stem (без ':').
    let stem = cx.effect_key.file_stem();
    assert!(!stem.contains(':'));
    let marker = root
        .join(".zero-agent/work/edits")
        .join(format!("{stem}.json"));
    assert!(marker.is_file(), "{}", marker.display());
    // Перезапуск: новый трекер, тот же ключ эффекта — тот же результат, файл изменён раз.
    let fresh = edit_tool(ReadTracker::new());
    let again = call_cx(&fresh, cx.clone(), args.clone()).await;
    assert_eq!(again, first);
    assert_eq!(bytes(&root, "a.rs"), b"2\n");
    // Файл изменён третьим содержимым — повтор отвергается.
    write(&root, "a.rs", "3\n");
    let o = call_cx(&edit_tool(ReadTracker::new()), cx, args).await;
    assert!(o.is_error);
    assert_eq!(
        o.content,
        "file changed since the interrupted edit; read it again"
    );
    assert_eq!(bytes(&root, "a.rs"), b"3\n");
}

#[tokio::test]
async fn multi_edit_is_atomic() {
    let root = unique_dir("multi");
    write(&root, "a.rs", "one\ntwo\nthree\n");
    let mut k = Kit::new(&root);
    k.read("a.rs").await;
    let o = k
        .multi(json!({"path": "a.rs", "edits": [
            {"old": "one", "new": "1"},
            {"old": "missing", "new": "x"},
        ]}))
        .await;
    assert!(o.is_error);
    assert!(
        o.content.starts_with("edit 2: old not found"),
        "{}",
        o.content
    );
    assert_eq!(bytes(&root, "a.rs"), b"one\ntwo\nthree\n");
    // Правки по порядку: вторая видит результат первой.
    let o = k
        .multi(json!({"path": "a.rs", "edits": [
            {"old": "one", "new": "1"},
            {"old": "1\ntwo", "new": "1\n2"},
        ]}))
        .await;
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(bytes(&root, "a.rs"), b"1\n2\nthree\n");
    let o = k
        .multi(json!({"path": "a.rs", "edits": [{"old": "", "new": "x"}]}))
        .await;
    assert!(
        o.content.starts_with("edit 1: old is empty"),
        "{}",
        o.content
    );
    let o = k.multi(json!({"path": "a.rs", "edits": []})).await;
    assert_eq!(o.content, "invalid arguments: edits is empty");
}

// ---- векторы обхода: отказ ----

#[tokio::test]
async fn protected_places_are_denied() {
    let root = unique_dir("protected");
    write(&root, ".git/config", "[core]\n");
    write(&root, ".zero-agent/config.toml", "a = 1\n");
    write(&root, ".zero-agent/work/edits/x.json", "{}\n");
    let mut k = Kit::new(&root);
    for p in [
        ".git/config",
        ".GIT/config",
        ".zero-agent/config.toml",
        ".zero-agent/work/edits/x.json",
        ".git/new-file",
    ] {
        let o = k.edit(json!({"path": p, "old": "", "new": "x"})).await;
        // На Unix `.GIT` — другой каталог: его просто нет, создать файл там можно.
        if cfg!(unix) && p.starts_with(".GIT") {
            continue;
        }
        assert!(o.is_error, "{p}");
        assert_eq!(o.content, "denied: protected path", "{p}");
    }
    assert_eq!(bytes(&root, ".git/config"), b"[core]\n");
    assert!(!root.join(".git/new-file").exists());
}

#[tokio::test]
async fn junction_into_git_is_denied() {
    let root = unique_dir("junction");
    write(&root, ".git/config", "[core]\n");
    if dir_link(&root.join("sub"), &root.join(".git")).is_none() {
        return;
    }
    let mut k = Kit::new(&root);
    k.read("sub/config").await;
    let o = k
        .edit(json!({"path": "sub/config", "old": "[core]", "new": "[x]"}))
        .await;
    assert!(o.is_error);
    assert_eq!(o.content, "denied: protected path");
    assert_eq!(bytes(&root, ".git/config"), b"[core]\n");
}

#[tokio::test]
async fn link_to_protected_file_is_denied() {
    let root = unique_dir("link_protected");
    write(&root, ".git/config", "[core]\n");
    if file_link(&root.join("cfg"), &root.join(".git/config")).is_none() {
        return;
    }
    let mut k = Kit::new(&root);
    k.read("cfg").await;
    let o = k
        .edit(json!({"path": "cfg", "old": "[core]", "new": "[x]"}))
        .await;
    assert_eq!(o.content, "denied: protected path");
    assert_eq!(bytes(&root, ".git/config"), b"[core]\n");
}

#[tokio::test]
async fn hard_link_is_refused() {
    let root = unique_dir("hardlink");
    write(&root, "a.txt", "1\n");
    std::fs::hard_link(root.join("a.txt"), root.join("b.txt")).unwrap();
    let mut k = Kit::new(&root);
    k.read("b.txt").await;
    let o = k
        .edit(json!({"path": "b.txt", "old": "1", "new": "2"}))
        .await;
    assert!(o.is_error);
    assert!(
        o.content
            .starts_with("refusing to write a file with several hard links"),
        "{}",
        o.content
    );
    assert_eq!(bytes(&root, "a.txt"), b"1\n");
    assert_eq!(bytes(&root, "b.txt"), b"1\n");
    assert!(leftovers(&root).is_empty());
}

#[cfg(windows)]
#[tokio::test]
async fn ads_and_device_names_are_refused_without_io() {
    let root = unique_dir("ads_device");
    write(&root, "a.txt", "1\n");
    let mut k = Kit::new(&root);
    for (p, want) in [
        ("a.txt:evil", "alternate data streams are not supported"),
        ("a.txt::$DATA", "alternate data streams are not supported"),
        ("nul", "device paths are not supported"),
        ("sub/con.txt", "device paths are not supported"),
        ("\\\\.\\PhysicalDrive0", "device paths are not supported"),
        (
            "\\\\server\\share\\a.txt",
            "network paths are not supported",
        ),
        ("a.txt.", "path components must not end with a dot or space"),
    ] {
        let o = k.edit(json!({"path": p, "old": "", "new": "x"})).await;
        assert!(o.is_error, "{p}");
        assert!(o.content.starts_with(want), "{p}: {}", o.content);
    }
    assert_eq!(bytes(&root, "a.txt"), b"1\n");
    assert!(!root.join("sub").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn device_file_is_refused() {
    let root = unique_dir("device");
    let mut k = Kit::new(&root);
    let o = k
        .edit(json!({"path": "/dev/null", "old": "a", "new": "b"}))
        .await;
    assert!(o.is_error);
    assert!(o.content.contains("not a regular file"), "{}", o.content);
}

#[tokio::test]
async fn outside_path_is_a_write_subject() {
    // Граница — решение политики (D77): инструмент сообщает путь с Access::Write.
    let tool = edit_tool(ReadTracker::new());
    let s = tool.subjects(&json!({"path": "../x.rs", "old": "a", "new": "b"}));
    assert_eq!(s.paths.len(), 1);
    assert_eq!(s.paths[0].arg, "../x.rs");
    assert_eq!(s.paths[0].access, Access::Write);
    let s = multi_edit_tool(ReadTracker::new()).subjects(&json!({"path": "a.rs", "edits": []}));
    assert_eq!(s.paths[0].access, Access::Write);
    assert!(tool.subjects(&json!({})).paths.is_empty());
}

// ---- большой файл ----

#[tokio::test]
async fn large_file_edit_and_limit() {
    let root = unique_dir("large");
    // ~3,5 МиБ: правка в середине и в конце.
    let body: String = (0..100_000)
        .map(|i| format!("line {i:06} of a large file\n"))
        .collect();
    assert!(body.len() < 4 << 20);
    write(&root, "big.txt", &body);
    let mut k = Kit::new(&root);
    k.read("big.txt").await;
    let o = k
        .edit(json!({"path": "big.txt", "old": "line 050000 of", "new": "LINE 050000 of"}))
        .await;
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.contains("+LINE 050000 of"), "{}", o.content);
    let o = k
        .edit(json!({"path": "big.txt", "old": "line 099999 of a large file\n", "new": "end\n"}))
        .await;
    assert!(!o.is_error, "{}", o.content);
    let now = String::from_utf8(bytes(&root, "big.txt")).unwrap();
    assert!(now.ends_with("line 099998 of a large file\nend\n"));
    // Не найдено в большом файле: ответ с ближайшим участком, без зависания.
    let o = k
        .edit(json!({"path": "big.txt", "old": "line 070000 of a lrage file", "new": "x"}))
        .await;
    assert!(o.is_error);
    assert!(o.content.starts_with("old not found"), "{}", o.content);
    // Больше 4 МиБ — отказ, байты целы.
    let huge = "y".repeat((4 << 20) + 10);
    write(&root, "huge.txt", &huge);
    let o = k
        .edit(json!({"path": "huge.txt", "old": "y", "new": "z"}))
        .await;
    assert_eq!(o.content, "file is larger than 4 MiB; edit refused");
    assert_eq!(bytes(&root, "huge.txt").len(), huge.len());
}

// ---- атомарность: подмена между проверкой и записью, сбой ----

/// Крюк, который один раз выполняет `act` в точке `at`; `crash` — затем «сбой процесса».
fn hook_once(at: EditPoint, crash: bool, act: impl Fn(&Path) + Send + Sync + 'static) -> EditHook {
    let done = Arc::new(AtomicBool::new(false));
    Arc::new(move |p, path| {
        if p == at && !done.swap(true, Ordering::SeqCst) {
            act(path);
            return crash;
        }
        false
    })
}

#[tokio::test]
async fn swap_before_rename_is_refused() {
    let root = unique_dir("swap_content");
    write(&root, "a.rs", "1\n");
    let tracker = ReadTracker::new();
    let mut cx = Cx::new(&root);
    let r = call_cx(
        &read_tool(tracker.clone()),
        cx.next(),
        json!({"path": "a.rs"}),
    )
    .await;
    assert!(!r.is_error);
    let hook = hook_once(EditPoint::BeforeRename, false, |p| {
        std::fs::write(p, "third\n").unwrap();
    });
    let edit = edit_tool_with_hook(tracker, hook);
    let o = call_cx(
        &edit,
        cx.next(),
        json!({"path": "a.rs", "old": "1", "new": "2"}),
    )
    .await;
    assert!(o.is_error);
    assert_eq!(o.content, "file changed during the edit; read it again");
    assert_eq!(bytes(&root, "a.rs"), b"third\n");
    assert!(leftovers(&root).is_empty());
}

#[tokio::test]
async fn link_swapped_in_before_rename_is_refused() {
    let root = unique_dir("swap_link");
    write(&root, "a.rs", "1\n");
    write(&root, "other.rs", "other\n");
    // Ссылки без прав на Windows может не быть: проверка заранее.
    if file_link(&root.join("probe"), &root.join("other.rs")).is_none() {
        return;
    }
    let tracker = ReadTracker::new();
    let mut cx = Cx::new(&root);
    let r = call_cx(
        &read_tool(tracker.clone()),
        cx.next(),
        json!({"path": "a.rs"}),
    )
    .await;
    assert!(!r.is_error);
    let other = root.join("other.rs");
    let hook = hook_once(EditPoint::BeforeRename, false, move |p| {
        std::fs::remove_file(p).unwrap();
        file_link(p, &other).unwrap();
    });
    let edit = edit_tool_with_hook(tracker, hook);
    let o = call_cx(
        &edit,
        cx.next(),
        json!({"path": "a.rs", "old": "1", "new": "2"}),
    )
    .await;
    assert!(o.is_error);
    assert!(
        o.content.contains("refusing to follow a link") || o.content.contains("changed"),
        "{}",
        o.content
    );
    // Ссылка осталась ссылкой, её цель не тронута.
    assert!(
        std::fs::symlink_metadata(root.join("a.rs"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(bytes(&root, "other.rs"), b"other\n");
    assert!(leftovers(&root).is_empty());
}

#[tokio::test]
async fn crash_before_rename_keeps_file_and_retry_completes() {
    let root = unique_dir("crash");
    write(&root, "a.rs", "1\n");
    let tracker = ReadTracker::new();
    let mut cx = Cx::new(&root);
    let r = call_cx(
        &read_tool(tracker.clone()),
        cx.next(),
        json!({"path": "a.rs"}),
    )
    .await;
    assert!(!r.is_error);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let hook = hook_once(EditPoint::BeforeRename, true, move |p| {
        seen2.lock().unwrap().push(p.to_path_buf());
    });
    let args = json!({"path": "a.rs", "old": "1", "new": "2"});
    let o = call_cx(&edit_tool_with_hook(tracker, hook), cx.next(), args.clone()).await;
    assert!(o.is_error);
    // Сбой до rename: файл прежний целиком (атомарность), временный файл остался.
    assert_eq!(bytes(&root, "a.rs"), b"1\n");
    assert_eq!(seen.lock().unwrap().as_slice(), [canonical(&root, "a.rs")]);
    assert_eq!(leftovers(&root).len(), 1);
    // Перезапуск: новый трекер, тот же ключ — маркер «before» продолжает правку.
    let o = call_cx(&edit_tool(ReadTracker::new()), cx.same(), args).await;
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(bytes(&root, "a.rs"), b"2\n");
    assert!(leftovers(&root).is_empty());
}

// ---- контракт (таблица D74) ----

#[test]
fn contract_matches_d74() {
    let roles = vec![RunRole::Top, RunRole::Orchestrator, RunRole::Worker];
    for (tool, name, tier) in [
        (edit_tool(ReadTracker::new()), "edit", Tier::Small),
        (
            multi_edit_tool(ReadTracker::new()),
            "multi_edit",
            Tier::Standard,
        ),
    ] {
        let s = tool.spec();
        assert_eq!(s.schema.name, name);
        assert_eq!(s.kind, ToolKind::Edit, "{name}");
        assert_eq!(s.class, EffectClass::Idempotent, "{name}");
        assert_eq!(s.risk, RiskClass::LocalWrite, "{name}");
        assert_eq!(s.min_tier, tier, "{name}");
        assert_eq!(s.timeout, Duration::from_secs(30), "{name}");
        assert_eq!(s.output_limit, Some(64 * 1024), "{name}");
        assert!(!s.tainted, "{name}");
        assert_eq!(s.approval, Approval::Mode, "{name}");
        assert_eq!(s.roles, roles, "{name}");
        let schema = serde_json::to_string(&s.schema.parameters).unwrap();
        assert!(
            s.schema.description.is_ascii() && schema.is_ascii(),
            "{name}"
        );
    }
}
