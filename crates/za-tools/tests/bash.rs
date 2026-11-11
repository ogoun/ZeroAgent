//! Инструмент `bash` (R1-C1 D74, D84, D85, D86; Carry 3): код выхода как данные, тайм-аут
//! и сбой запуска как ошибка, фоновые потомки, большой вывод, обёртка PowerShell,
//! временный каталог проекта, предметы вызова и метка недоверенного вывода.
//!
//! Команды тестов безобидные: `echo`, выход с кодом, сон, большой вывод, фоновый процесс.
//! Сетевые и внешние команды проверяются только через заглушку `Runner` — без запуска.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use za_core::shell::Dialect;
use za_core::{Access, Approval, BoxFuture, RiskClass, Tool, ToolCx, ToolKind, ToolOutput};
use za_jail::{CommandSpec, DirectRunner, Exit, Output, Runner, RunnerError};
use za_protocol::{EffectClass, EffectKey, RunId, RunRole, Tier, taint_origin};
use za_tools::{
    AgentHome, BASH_DEFAULT_TIMEOUT_S, BASH_MAX_TIMEOUT_S, Shell, bash_tool, format_output,
    powershell_encoded,
};

#[path = "../../../test-support/unique_dir.rs"]
mod shared_dir;

/// Корень проекта теста `CARGO_TARGET_TMPDIR/za-tools-bash/<name>-<pid>-<n>/p`; рядом —
/// `ZA_HOME` теста `../h` (реестр групп процессов, D84).
fn unique_dir(name: &str) -> PathBuf {
    let d = shared_dir::unique_dir("za-tools-bash", name);
    std::fs::create_dir_all(d.join("h")).unwrap();
    std::fs::create_dir_all(d.join("p")).unwrap();
    d.join("p")
}

fn home(root: &Path) -> AgentHome {
    AgentHome {
        za_home: root.parent().unwrap().join("h"),
        project_id: "0123456789abcdef".into(),
    }
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
    }
}

/// Оболочка станции; её нет — тест не имеет смысла.
fn shell() -> Shell {
    Shell::detect().expect("в PATH нет оболочки (pwsh, powershell, bash, sh)")
}

/// Оболочка для тестов без запуска: диалект задан явно.
fn fake_shell(d: Dialect) -> Shell {
    Shell {
        program: PathBuf::from("shell-not-run"),
        dialect: d,
    }
}

/// Вызов под тайм-аутом `base` × `ZA_TEST_TIME_SCALE` (никакое ожидание не виснет).
async fn call_with(tool: &Arc<dyn Tool>, root: &Path, args: Value, base: Duration) -> ToolOutput {
    let limit = base * scale();
    tokio::time::timeout(limit, tool.call(args, cx(root, RunId::new())))
        .await
        .unwrap_or_else(|_| panic!("bash не ответил за {limit:?}"))
}

/// Настоящий запуск в свежем проекте.
async fn bash(name: &str, args: Value) -> ToolOutput {
    let root = unique_dir(name);
    let tool = bash_tool(Arc::new(DirectRunner), shell(), home(&root));
    call_with(&tool, &root, args, Duration::from_secs(60)).await
}

/// Команда на диалекте станции.
fn os(unix: &str, windows: &str) -> String {
    match shell().dialect {
        Dialect::Bash => unix.to_owned(),
        Dialect::PowerShell => windows.to_owned(),
    }
}

// ---- результат ----

#[tokio::test]
async fn nonzero_exit_is_data_not_error() {
    // Carry 3: ненулевой код — данные; иначе детектор D57 паузит цикл TDD.
    let cmd = os(
        "echo out; echo err 1>&2; exit 3",
        "Write-Output out; [Console]::Error.WriteLine('err'); exit 3",
    );
    let o = bash("nonzero", json!({ "cmd": cmd })).await;
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(
        o.content,
        "exit code: 3\n--- stdout ---\nout\n--- stderr ---\nerr\n"
    );
}

#[tokio::test]
async fn success_omits_empty_sections() {
    let o = bash("ok", json!({ "cmd": "echo hi" })).await;
    assert!(!o.is_error);
    assert_eq!(o.content, "exit code: 0\n--- stdout ---\nhi\n");
    let o = bash("silent", json!({ "cmd": os("true", "$null = 1") })).await;
    assert_eq!(o.content, "exit code: 0\n");
}

#[tokio::test]
async fn timeout_is_error_with_partial_output() {
    let cmd = os("echo a; sleep 60", "Write-Output a; Start-Sleep 60");
    let o = bash("timeout", json!({ "cmd": cmd, "timeout": 10 })).await;
    assert!(o.is_error, "{}", o.content);
    assert!(
        o.content
            .starts_with("timeout after 10 s; process tree killed\n"),
        "{}",
        o.content
    );
    assert!(o.content.contains('a'), "{}", o.content);
}

#[tokio::test]
async fn background_child_is_killed_and_marked() {
    // Фоновый потомок держит stdout: вызов не ждёт его дольше KILL_GRACE, дерево
    // убивается, модель видит пометку.
    let cmd = os(
        "echo started; sleep 30 &",
        "Start-Process -NoNewWindow ping '-n 30 127.0.0.1'; Write-Output started",
    );
    let o = bash("background", json!({ "cmd": cmd })).await;
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.starts_with("exit code: 0\n"), "{}", o.content);
    assert!(o.content.contains("started"), "{}", o.content);
    assert!(
        o.content.ends_with("[background processes killed]\n"),
        "{}",
        o.content
    );
}

#[tokio::test]
async fn large_output_is_returned_whole() {
    // Инструмент не режет вывод: разрез и выгрузку по `output_limit` делает пакет
    // (D44, D75), полный текст уходит в файл выгрузки.
    let cmd = os("seq 1 200000", "1..200000 -join \"`n\"");
    let o = bash("large", json!({ "cmd": cmd })).await;
    assert!(!o.is_error);
    assert!(o.content.len() > 64 * 1024, "{}", o.content.len());
    assert!(o.content.ends_with("\n199999\n200000\n"));
    // `seq` и выражение PowerShell — не известный тип вывода.
    assert_eq!(o.digest, None);
}

#[tokio::test]
async fn json_output_gets_digest() {
    let cmd = os(
        r#"echo '{"a":[1,2,3],"b":{"c":"x"}}'"#,
        r#"Write-Output '{"a":[1,2,3],"b":{"c":"x"}}'"#,
    );
    let o = bash("json", json!({ "cmd": cmd })).await;
    assert_eq!(
        o.digest.as_deref(),
        Some("exit code: 0\n{a: array[3], b: {c: string}}"),
        "{}",
        o.content
    );
}

#[tokio::test]
async fn cwd_is_relative_to_project() {
    let root = unique_dir("cwd");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/marker.txt"), "m").unwrap();
    let tool = bash_tool(Arc::new(DirectRunner), shell(), home(&root));
    let cmd = os("ls", "(Get-ChildItem -Name)");
    let o = call_with(
        &tool,
        &root,
        json!({ "cmd": cmd, "cwd": "sub" }),
        Duration::from_secs(60),
    )
    .await;
    assert!(o.content.contains("marker.txt"), "{}", o.content);
    let o = call_with(
        &tool,
        &root,
        json!({ "cmd": "echo x", "cwd": "missing" }),
        Duration::from_secs(60),
    )
    .await;
    assert!(o.is_error);
    assert_eq!(o.content, "cwd not found: missing");
}

#[tokio::test]
async fn invalid_timeout_is_rejected() {
    let root = unique_dir("bad-timeout");
    let tool = bash_tool(Arc::new(DirectRunner), shell(), home(&root));
    for t in [0, BASH_MAX_TIMEOUT_S + 1] {
        let o = call_with(
            &tool,
            &root,
            json!({ "cmd": "echo x", "timeout": t }),
            Duration::from_secs(10),
        )
        .await;
        assert!(o.is_error);
        assert_eq!(
            o.content,
            "invalid arguments: timeout must be from 1 to 600 seconds"
        );
    }
}

/// Linux: на время команды есть запись реестра групп D84 в `ZA_HOME`, после — нет.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_registry_record_lives_during_call() {
    let root = unique_dir("registry");
    let h = home(&root);
    let run = RunId::new();
    let stem = EffectKey::derive(run, 1, 1).file_stem();
    let dir = h.za_home.join("projects").join(&h.project_id).join("procs");
    let file = dir.join(format!("{stem}.json"));
    let tool = bash_tool(Arc::new(DirectRunner), shell(), h);
    let o = tokio::time::timeout(
        Duration::from_secs(60) * scale(),
        tool.call(
            json!({ "cmd": format!("cat '{}'", file.display()) }),
            cx(&root, run),
        ),
    )
    .await
    .unwrap();
    assert!(o.content.contains("\"pgid\""), "{}", o.content);
    assert!(!file.exists());
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_quotes_comments_exit_codes() {
    if shell().dialect != Dialect::PowerShell {
        return;
    }
    // Кавычки доходят до PowerShell как есть (-EncodedCommand, без разбора CreateProcess).
    let o = bash(
        "ps-quotes",
        json!({ "cmd": "Write-Output \"a `\"b`\"\"; Write-Output 'c \"d\"'" }),
    )
    .await;
    assert_eq!(
        o.content,
        "exit code: 0\n--- stdout ---\na \"b\"\nc \"d\"\n"
    );
    // Комментарий последней строкой не съедает закрывающую скобку обёртки.
    let o = bash("ps-comment", json!({ "cmd": "Write-Output hi\n# comment" })).await;
    assert_eq!(o.content, "exit code: 0\n--- stdout ---\nhi\n");
    for (name, cmd, code) in [
        ("ps-exit", "exit 3", 3),
        ("ps-native", "cmd /c exit 5", 5),
        ("ps-cmdlet", "Get-Item nope", 1),
    ] {
        let o = bash(name, json!({ "cmd": cmd })).await;
        assert!(!o.is_error, "{cmd}: {}", o.content);
        assert!(
            o.content.starts_with(&format!("exit code: {code}\n")),
            "{cmd}: {}",
            o.content
        );
    }
    // Ошибка командлета — читаемый текст в stderr, не CLIXML.
    let o = bash("ps-error-text", json!({ "cmd": "Get-Item nope" })).await;
    assert!(o.content.contains("--- stderr ---\n"), "{}", o.content);
    assert!(o.content.contains("Cannot find path"), "{}", o.content);
    assert!(!o.content.contains("CLIXML"), "{}", o.content);
    assert!(!o.content.contains("_x000D_"), "{}", o.content);
    // Кириллица без порчи в обе стороны.
    let o = bash("ps-utf8", json!({ "cmd": "Write-Output 'привет, мир'" })).await;
    assert_eq!(o.content, "exit code: 0\n--- stdout ---\nпривет, мир\n");
}

#[test]
fn powershell_encoded_wraps_utf16le_base64() {
    // "x" в обёртке D85 → UTF-16LE → base64; проверяем обратным декодированием.
    let b64 = powershell_encoded("Write-Output 1");
    let bytes = decode_base64(&b64);
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let script = String::from_utf16(&units).unwrap();
    assert!(script.contains("& {\nWrite-Output 1\n"), "{script}");
    assert!(script.starts_with("$ProgressPreference='SilentlyContinue'; "));
    assert!(script.contains("[Console]::OutputEncoding=[Text.Encoding]::UTF8"));
    assert!(script.ends_with(
        "}; if ($LASTEXITCODE) { exit $LASTEXITCODE } elseif (-not $global:ZaLastOk) { exit 1 }"
    ));
    // Известное значение: base64 без переносов, с дополнением.
    assert_eq!(powershell_encoded("").len() % 4, 0, "длина base64 кратна 4");
}

fn decode_base64(s: &str) -> Vec<u8> {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = A.iter().position(|a| *a == c).unwrap() as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

// ---- формат ----

fn output(exit: Exit, stdout: &str, stderr: &str) -> Output {
    Output {
        exit,
        stdout: stdout.into(),
        stderr: stderr.into(),
        stdout_truncated: false,
        stderr_truncated: false,
        background_killed: false,
        duration: Duration::from_millis(5),
    }
}

#[test]
fn format_output_sections_and_marks() {
    assert_eq!(
        format_output(&output(Exit::Code(0), "a\nb", "")),
        "exit code: 0\n--- stdout ---\na\nb\n"
    );
    assert_eq!(
        format_output(&output(Exit::Signal(9), "", "x\n")),
        "killed by signal 9\n--- stderr ---\nx\n"
    );
    let mut o = output(Exit::Code(1), "big", "");
    o.stdout_truncated = true;
    o.background_killed = true;
    assert_eq!(
        format_output(&o),
        "exit code: 1\n--- stdout ---\nbig\n[stdout truncated after 16777216 bytes]\n[background processes killed]\n"
    );
}

// ---- заглушка Runner: спецификация запуска и метки без настоящего процесса ----

#[derive(Default)]
struct Recorder {
    specs: Mutex<Vec<CommandSpec>>,
    fail: bool,
}

impl Runner for Recorder {
    fn run(&self, spec: CommandSpec) -> BoxFuture<'static, Result<Output, RunnerError>> {
        let program = spec.program.display().to_string();
        self.specs.lock().unwrap().push(spec);
        let fail = self.fail;
        Box::pin(async move {
            if fail {
                Err(RunnerError::Spawn {
                    program,
                    source: std::io::Error::new(std::io::ErrorKind::NotFound, "program not found"),
                })
            } else {
                Ok(output(Exit::Code(0), "fake\n", ""))
            }
        })
    }
}

#[tokio::test]
async fn spawn_failure_is_error() {
    let root = unique_dir("spawn-fail");
    let rec = Arc::new(Recorder {
        fail: true,
        ..Recorder::default()
    });
    let tool = bash_tool(rec, fake_shell(Dialect::Bash), home(&root));
    let o = call_with(
        &tool,
        &root,
        json!({"cmd": "echo x"}),
        Duration::from_secs(10),
    )
    .await;
    assert!(o.is_error);
    assert!(o.content.starts_with("failed to start"), "{}", o.content);
    assert!(o.content.contains("program not found"), "{}", o.content);
}

#[tokio::test]
async fn spec_uses_project_tmp_and_timeout() {
    let root = unique_dir("spec");
    let rec = Arc::new(Recorder::default());
    let run = RunId::new();
    for d in [Dialect::Bash, Dialect::PowerShell] {
        let tool = bash_tool(rec.clone(), fake_shell(d), home(&root));
        let o = tokio::time::timeout(
            Duration::from_secs(10) * scale(),
            tool.call(json!({"cmd": "echo x", "timeout": 7}), cx(&root, run)),
        )
        .await
        .unwrap();
        assert_eq!(o.content, "exit code: 0\n--- stdout ---\nfake\n");
    }
    let specs = rec.specs.lock().unwrap();
    let tmp = root
        .join(".zero-agent")
        .join("work")
        .join("tmp")
        .join(run.to_string());
    for s in specs.iter() {
        assert_eq!(s.tmp_dir.as_deref(), Some(tmp.as_path()));
        assert!(tmp.is_dir());
        assert_eq!(s.timeout, Duration::from_secs(7));
        assert_eq!(s.program, PathBuf::from("shell-not-run"));
        assert_eq!(s.cwd, root);
    }
    assert_eq!(
        specs[0].args,
        ["-c", "echo x"].map(std::ffi::OsString::from).to_vec()
    );
    let ps: Vec<String> = specs[1]
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        ps,
        [
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &powershell_encoded("echo x"),
        ]
    );
    // `.gitignore` рабочего каталога (SEC-004).
    assert!(root.join(".zero-agent/work/.gitignore").is_file());
}

#[tokio::test]
async fn default_timeout_is_120_s() {
    let root = unique_dir("default-timeout");
    let rec = Arc::new(Recorder::default());
    let tool = bash_tool(rec.clone(), fake_shell(Dialect::Bash), home(&root));
    call_with(
        &tool,
        &root,
        json!({"cmd": "echo x"}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(BASH_DEFAULT_TIMEOUT_S, 120);
    assert_eq!(
        rec.specs.lock().unwrap()[0].timeout,
        Duration::from_secs(BASH_DEFAULT_TIMEOUT_S)
    );
}

#[tokio::test]
async fn taint_only_for_network_reads_and_outside_paths() {
    let root = unique_dir("taint");
    let rec = Arc::new(Recorder::default());
    let tool = bash_tool(rec, fake_shell(Dialect::Bash), home(&root));
    for (cmd, want) in [
        (
            "curl -s https://example.invalid/x",
            Some(taint_origin::NETWORK),
        ),
        ("git fetch origin", Some(taint_origin::NETWORK)),
        ("cat /etc/hosts", Some(taint_origin::OUTSIDE_PROJECT)),
        ("cat ../other/file", Some(taint_origin::OUTSIDE_PROJECT)),
        ("cat ~/notes.txt", Some(taint_origin::OUTSIDE_PROJECT)),
        // Обычный цикл TDD без меток (Ruling: иначе каждый прогон тестов в auto — с вопросом).
        ("cargo test -p za-core", None),
        ("cat src/main.rs", None),
        ("git push origin main", None),
        ("echo https://example.invalid", None),
    ] {
        let o = call_with(&tool, &root, json!({"cmd": cmd}), Duration::from_secs(10)).await;
        assert_eq!(o.taint.as_deref(), want, "{cmd}");
    }
}

// ---- предметы вызова и контракт ----

#[test]
fn subjects_mark_outside_and_network() {
    let tool = bash_tool(
        Arc::new(DirectRunner),
        fake_shell(Dialect::Bash),
        home(Path::new("/p/x")),
    );
    let s = tool.subjects(&json!({"cmd": "cat /etc/hosts && curl https://x"}));
    assert_eq!(s.paths[0].arg, "/etc/hosts");
    assert_eq!(s.paths[0].access, Access::Read);
    assert!(s.network);
    assert_eq!(
        s.command.as_deref(),
        Some("cat /etc/hosts && curl https://x")
    );
    assert_eq!(s.segments.len(), 2);
    assert!(s.irreversible.is_empty());
    assert_eq!(s.cwd, None);

    // Внешний путь перенаправления — метка `overwrite`; внутри — нет.
    let s = tool.subjects(&json!({"cmd": "echo x > /etc/x.conf", "cwd": "sub"}));
    assert_eq!(s.irreversible, ["overwrite"]);
    assert_eq!(s.cwd.as_deref(), Some("sub"));
    let s = tool.subjects(&json!({"cmd": "echo x > out.txt"}));
    assert!(s.irreversible.is_empty(), "{:?}", s.irreversible);
    assert!(!s.network);

    // Относительный путь — от `cwd`: `sub/../../x` выходит за проект.
    let s = tool.subjects(&json!({"cmd": "cat ../../x", "cwd": "sub"}));
    assert_eq!(s.paths[0].arg, "sub/../../x");

    let s = tool.subjects(&json!({"cmd": "rm -rf build"}));
    assert_eq!(s.irreversible, ["delete"]);
    assert!(tool.subjects(&json!({})).command.is_none());
}

#[test]
fn subjects_powershell_dialect() {
    let tool = bash_tool(
        Arc::new(DirectRunner),
        fake_shell(Dialect::PowerShell),
        home(Path::new("/p/x")),
    );
    let s = tool.subjects(&json!({"cmd": r"Get-Content C:\Windows\win.ini; iwr https://x"}));
    assert_eq!(s.paths[0].arg, r"C:\Windows\win.ini");
    assert!(s.network);
}

#[test]
fn contract_matches_d74() {
    let tool = bash_tool(
        Arc::new(DirectRunner),
        fake_shell(Dialect::Bash),
        home(Path::new("/p/x")),
    );
    let s = tool.spec();
    assert_eq!(s.schema.name, "bash");
    assert_eq!(s.kind, ToolKind::Exec);
    assert_eq!(s.class, EffectClass::NonIdempotent);
    assert_eq!(s.risk, RiskClass::Exec);
    assert_eq!(s.min_tier, Tier::Small);
    assert_eq!(s.timeout, Duration::from_secs(630));
    assert_eq!(s.output_limit, Some(64 * 1024));
    assert!(!s.tainted);
    assert_eq!(s.approval, Approval::Mode);
    assert_eq!(
        s.roles,
        [RunRole::Top, RunRole::Orchestrator, RunRole::Worker]
    );
    assert_eq!(BASH_MAX_TIMEOUT_S, 600);
    let schema = serde_json::to_string(&s.schema.parameters).unwrap();
    assert!(s.schema.description.is_ascii() && schema.is_ascii());
    assert!(
        s.schema
            .description
            .contains("PowerShell on Windows, bash elsewhere; see shell in <env>")
    );
    // Описание одинаково для обоих диалектов (D97).
    let ps = bash_tool(
        Arc::new(DirectRunner),
        fake_shell(Dialect::PowerShell),
        home(Path::new("/p/x")),
    );
    assert_eq!(ps.spec().schema.description, s.schema.description);
    assert_eq!(ps.spec().schema.parameters, s.schema.parameters);
}

#[test]
fn detect_finds_a_shell_without_running_it() {
    let s = shell();
    assert!(s.program.is_absolute(), "{}", s.program.display());
    assert!(s.program.is_file());
    let name = s
        .program
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    match s.dialect {
        Dialect::PowerShell => assert!(name == "pwsh" || name == "powershell", "{name}"),
        Dialect::Bash => assert!(name == "bash" || name == "sh", "{name}"),
    }
}
