//! `bash(cmd, cwd?, timeout?)` (R1-C1 D85, таблица D74; D77, D82, D84, D86; Carry 3).
//!
//! Команда запускается только через [`za_jail::Runner`] (SEC-006): Windows — PowerShell
//! (`pwsh`, иначе `powershell.exe`) с `-EncodedCommand` ([`powershell_encoded`]), Unix —
//! `bash -c`, иначе `sh -c`. Оболочку выбирает [`Shell::detect`] при создании инструмента.
//! `TMP`, `TEMP`, `TMPDIR` дочернего процесса — `<project>/.zero-agent/work/tmp/<run>`
//! (`za_core::paths::work_dir`), системный каталог временных файлов не передаётся. На
//! Linux запись реестра групп — `ZA_HOME/projects/<project_id>/procs/<file_stem>.json`
//! ([`EditHome`], `home_dir`): по ней `reap_leftovers` убирает сирот после смерти агента.
//!
//! Результат ([`format_output`]): `exit code: N`, затем `--- stdout ---` и `--- stderr ---`;
//! пустые секции опускаются. Ненулевой код выхода — данные (`is_error = false`), иначе
//! детектор D57 паузил бы цикл TDD (Carry 3). `is_error` — только сбой запуска
//! (`failed to start …`) и тайм-аут (`timeout after N s; process tree killed` и
//! частичный вывод). Фоновые потомки, убитые после выхода главного процесса, — строка
//! `[background processes killed]`.
//!
//! Инструмент сам ничего не разрешает и не запрещает: предметы вызова ([`Tool::subjects`]:
//! сегменты `za_core::shell`, слова-пути, метки необратимости, сеть) решает политика
//! (D77, D80, D81). Вывод — недоверенный текст, его экранирует пакет (D94). Метка
//! происхождения (`ToolOutput.taint`, D82) — только у команд, читающих из сети, и у
//! команд с путём вне проекта: обычный цикл тестов идёт без неё (Ruling).
//!
//! Вывод не режется: предел `output_limit` (64 КиБ) и выгрузку держит пакет (D44, D75).
//! Для одной простой команды известного типа есть сжатое представление
//! ([`crate::digest()`], D86) — в `ToolOutput.digest`.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use za_core::paths::{PathError, classify, within, work_dir};
use za_core::shell::{self, Dialect, Segment};
use za_core::{
    Access, Approval, BoxFuture, RawPath, RiskClass, Subjects, Tool, ToolCx, ToolKind, ToolOutput,
    ToolSchema, ToolSpec,
};
use za_jail::{CAPTURE_LIMIT, CommandSpec, Exit, Output, Runner, RunnerError};
use za_protocol::{EffectClass, RunRole, Tier, taint_origin};

use crate::EditHome;
use crate::digest::{
    GAP, HUNKS_HEADER, STDERR_HEADER, TAIL_HEADER, diff_header, digest, more_entries, more_keys,
    more_lines, more_matches, tests_header,
};
use crate::fs::{invalid_args_text, parse};

/// Тайм-аут команды по умолчанию, секунд (D85).
pub const BASH_DEFAULT_TIMEOUT_S: u64 = 120;
/// Наибольший тайм-аут команды, секунд (D85).
pub const BASH_MAX_TIMEOUT_S: u64 = 600;
/// Предел вызова инструмента (D74): тайм-аут команды и запас на убийство и дочитывание.
const SPEC_TIMEOUT_S: u64 = 630;
/// Предел результата (таблица D74).
const OUTPUT_LIMIT: usize = 64 * 1024;
/// Подкаталог рабочих каталогов процессов в `work_dir` и реестра групп в `ZA_HOME`.
const TMP_DIR: &str = "tmp";
#[cfg(target_os = "linux")]
const PROCS_DIR: &str = "procs";
/// Корень-заглушка для лексической границы в [`Tool::subjects`], где корня проекта нет:
/// относительный путь, уходящий выше него через `..`, и любой абсолютный — снаружи.
#[cfg(windows)]
const LEX_ROOT: &str = r"C:\za-project-root";
#[cfg(not(windows))]
const LEX_ROOT: &str = "/za-project-root";

/// Начало обёртки PowerShell (D85). `$ProgressPreference` глушит записи прогресса: без
/// него PowerShell 5.1 пишет их в stderr как CLIXML.
const PS_PREFIX: &str = "$ProgressPreference='SilentlyContinue'; \
    [Console]::OutputEncoding=[Text.Encoding]::UTF8; $OutputEncoding=[Text.Encoding]::UTF8; & {";
/// Конец обёртки: `$?` последней команды запоминается внутри блока (после `}` его уже
/// сбросило бы условие `if`); `}` — на своей строке, чтобы `#` в команде не съел скобку.
const PS_SUFFIX: &str = "$global:ZaLastOk = $?\n}; \
    if ($LASTEXITCODE) { exit $LASTEXITCODE } elseif (-not $global:ZaLastOk) { exit 1 }";

// Тексты для модели (D23, D94): английский, только ASCII-пунктуация.
const DESCRIPTION: &str = "Run a shell command in the project directory. \
    PowerShell on Windows, bash elsewhere; see shell in <env>. \
    A non-zero exit code is reported in the result, it is not a tool error.";
const STDOUT_HEADER: &str = "--- stdout ---";
const BACKGROUND_KILLED: &str = "[background processes killed]";
const EMPTY_CMD: &str = "cmd is empty";

fn exit_text(code: i32) -> String {
    format!("exit code: {code}")
}

fn signal_text(sig: i32) -> String {
    format!("killed by signal {sig}")
}

fn timeout_text(secs: u64) -> String {
    format!("timeout after {secs} s; process tree killed")
}

fn truncated_text(stream: &str) -> String {
    format!("[{stream} truncated after {CAPTURE_LIMIT} bytes]")
}

fn timeout_range_text() -> String {
    invalid_args_text(&format!(
        "timeout must be from 1 to {BASH_MAX_TIMEOUT_S} seconds"
    ))
}

fn failed_text(e: &str) -> String {
    format!("failed to start: {e}")
}

fn cwd_missing_text(cwd: &str) -> String {
    format!("cwd not found: {cwd}")
}

fn cwd_not_dir_text(cwd: &str) -> String {
    format!("cwd is not a directory: {cwd}")
}

fn tmp_error_text(kind: io::ErrorKind) -> String {
    failed_text(&format!("cannot create the temporary directory: {kind}"))
}

/// Оболочка инструмента: программа и диалект лексера (D85).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub program: PathBuf,
    pub dialect: Dialect,
}

impl Shell {
    /// Ищет оболочку по `PATH`, процесс не запускается. Windows: `pwsh.exe`, затем
    /// `powershell.exe` (и `%SystemRoot%\System32\WindowsPowerShell\v1.0`); Unix: `bash`,
    /// затем `sh` (и `/bin/sh`). Относительные элементы `PATH` пропускаются: оболочка из
    /// каталога проекта не подменит системную.
    pub fn detect() -> Option<Shell> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let dirs: Vec<PathBuf> = std::env::split_paths(&path)
            .filter(|d| d.is_absolute())
            .collect();
        let find = |name: &str| dirs.iter().map(|d| d.join(name)).find(|p| p.is_file());
        if cfg!(windows) {
            let fallback = std::env::var_os("SystemRoot")
                .map(|r| PathBuf::from(r).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"));
            find("pwsh.exe")
                .or_else(|| find("powershell.exe"))
                .or_else(|| fallback.filter(|p| p.is_file()))
                .map(|program| Shell {
                    program,
                    dialect: Dialect::PowerShell,
                })
        } else {
            find("bash")
                .or_else(|| find("sh"))
                .or_else(|| Some(PathBuf::from("/bin/sh")).filter(|p| p.is_file()))
                .map(|program| Shell {
                    program,
                    dialect: Dialect::Bash,
                })
        }
    }

    /// Аргументы запуска команды.
    fn args(&self, cmd: &str) -> Vec<OsString> {
        match self.dialect {
            Dialect::PowerShell => [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                &powershell_encoded(cmd),
            ]
            .map(OsString::from)
            .to_vec(),
            Dialect::Bash => vec!["-c".into(), cmd.into()],
        }
    }
}

/// Обёртка PowerShell (D85) → UTF-16LE → base64 для `-EncodedCommand`: кавычки команды
/// не проходят через разбор командной строки `CreateProcess`.
pub fn powershell_encoded(cmd: &str) -> String {
    let script = format!("{PS_PREFIX}\n{cmd}\n{PS_SUFFIX}");
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64(&bytes)
}

/// Стандартный base64 с дополнением (RFC 4648).
fn base64(b: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(c.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(c.get(2).copied().unwrap_or(0));
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= c.len() {
                out.push(char::from(A[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Текст результата (D85): первая строка — `exit code: N`, `killed by signal N` или
/// `timeout after N s; process tree killed` (N — длительность запуска в целых секундах),
/// затем непустые секции вывода и пометки.
pub fn format_output(o: &Output) -> String {
    render(o, &o.stderr, o.duration.as_secs())
}

/// [`format_output`] с stderr после декодирования и тайм-аутом из запроса.
fn render(o: &Output, stderr: &str, timeout_s: u64) -> String {
    let mut s = head(o, timeout_s);
    s.push('\n');
    for (header, text, truncated, name) in [
        (
            STDOUT_HEADER,
            o.stdout.as_str(),
            o.stdout_truncated,
            "stdout",
        ),
        (STDERR_HEADER, stderr, o.stderr_truncated, "stderr"),
    ] {
        if text.is_empty() && !truncated {
            continue;
        }
        s.push_str(header);
        s.push('\n');
        s.push_str(text);
        if !text.is_empty() && !text.ends_with('\n') {
            s.push('\n');
        }
        if truncated {
            s.push_str(&truncated_text(name));
            s.push('\n');
        }
    }
    if o.background_killed {
        s.push_str(BACKGROUND_KILLED);
        s.push('\n');
    }
    s
}

fn head(o: &Output, timeout_s: u64) -> String {
    match o.exit {
        Exit::Code(c) => exit_text(c),
        Exit::Signal(n) => signal_text(n),
        Exit::Timeout => timeout_text(timeout_s),
    }
}

/// PowerShell 5.1 с `-EncodedCommand` пишет записи ошибок в stderr как CLIXML
/// (`#< CLIXML` и `<Objs>…</Objs>`): из них берётся текст потоков `<S S="…">`, записи
/// прогресса и прочие объекты отбрасываются. Остальной текст stderr не меняется.
fn decode_clixml(stderr: &str) -> String {
    const MARK: &str = "#< CLIXML\n";
    const END: &str = "</Objs>";
    if !stderr.contains(MARK) {
        return stderr.to_owned();
    }
    let mut out = String::new();
    let mut rest = stderr;
    while let Some(at) = rest.find(MARK) {
        out.push_str(&rest[..at]);
        let xml = &rest[at + MARK.len()..];
        let Some(end) = xml.find(END) else {
            // Обрыв (тайм-аут, предел захвата): что есть — как текст.
            out.push_str(&clixml_text(xml));
            rest = "";
            break;
        };
        out.push_str(&clixml_text(&xml[..end]));
        rest = xml[end + END.len()..]
            .strip_prefix('\n')
            .unwrap_or(&xml[end + END.len()..]);
    }
    out.push_str(rest);
    out.replace("\r\n", "\n")
}

/// Текст элементов `<S S="…">…</S>` фрагмента CLIXML по порядку.
fn clixml_text(xml: &str) -> String {
    let mut out = String::new();
    let mut rest = xml;
    while let Some(at) = rest.find("<S S=\"") {
        let after = &rest[at + 6..];
        let Some(open_end) = after.find("\">") else {
            break;
        };
        let body = &after[open_end + 2..];
        let Some(close) = body.find("</S>") else {
            out.push_str(&unescape_xml(body));
            break;
        };
        out.push_str(&unescape_xml(&body[..close]));
        rest = &body[close + 4..];
    }
    out
}

/// Сущности XML и коды `_xHHHH_` сериализатора PowerShell (UTF-16, с суррогатами).
fn unescape_xml(s: &str) -> String {
    let s = s
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    let mut units: Vec<u16> = Vec::with_capacity(s.len());
    let mut i = 0;
    let b = s.as_bytes();
    while i < b.len() {
        if b[i] == b'_'
            && i + 7 <= b.len()
            && b[i + 1] == b'x'
            && b[i + 6] == b'_'
            && let Ok(u) = u16::from_str_radix(&s[i + 2..i + 6], 16)
        {
            units.push(u);
            i += 7;
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or('\u{FFFD}');
        let mut buf = [0u16; 2];
        units.extend_from_slice(ch.encode_utf16(&mut buf));
        i += ch.len_utf8();
    }
    String::from_utf16_lossy(&units)
}

/// Аргументы вызова.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    cmd: String,
    cwd: Option<String>,
    timeout: Option<u64>,
}

struct BashTool {
    spec: ToolSpec,
    runner: Arc<dyn Runner>,
    shell: Shell,
    home: EditHome,
}

/// Инструмент `bash(cmd, cwd?, timeout?)`. `home` — `ZA_HOME` и проект для реестра групп
/// процессов на Linux (D84; на других ОС не используется).
pub fn bash_tool(runner: Arc<dyn Runner>, shell: Shell, home: EditHome) -> Arc<dyn Tool> {
    let schema = ToolSchema {
        name: "bash".into(),
        description: DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string", "description": "Command to run" },
                "cwd": { "type": "string", "description": "Working directory, relative to the project root (default: the root)" },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": BASH_MAX_TIMEOUT_S,
                    "description": "Timeout in seconds (default 120)"
                }
            },
            "required": ["cmd"],
            "additionalProperties": false
        }),
    };
    Arc::new(BashTool {
        spec: ToolSpec {
            schema,
            class: EffectClass::NonIdempotent,
            kind: ToolKind::Exec,
            approval: Approval::Mode,
            risk: RiskClass::Exec,
            roles: vec![RunRole::Top, RunRole::Orchestrator, RunRole::Worker],
            min_tier: Tier::Small,
            timeout: Duration::from_secs(SPEC_TIMEOUT_S),
            output_limit: Some(OUTPUT_LIMIT),
            tainted: false,
        },
        runner,
        shell,
        home,
    })
}

/// Слово-путь относительно: не от корня, не `~`, не `X:`.
fn is_relative(w: &str) -> bool {
    let b = w.as_bytes();
    !(w.starts_with(['/', '\\', '~'])
        || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'))
}

/// Слова-пути команды (D77) по сегментам; относительные — от `cwd`.
fn path_args(segs: &[Segment], d: Dialect, cwd: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in segs {
        for w in shell::path_words(s, d) {
            let arg = match cwd {
                Some(c) if is_relative(&w) => format!("{}/{w}", c.trim_end_matches(['/', '\\'])),
                _ => w,
            };
            if !out.contains(&arg) {
                out.push(arg);
            }
        }
    }
    out
}

/// Путь вне проекта `root` лексически (без ввода-вывода): `~`, `..` выше корня, другой
/// абсолютный путь; сетевой путь и неразборчивый — тоже. Имя устройства (`NUL`) — нет.
fn outside_lex(root: &Path, w: &str) -> bool {
    if w.starts_with('~') {
        return true;
    }
    match classify(root, w) {
        Ok(lex) => !within(root, lex.as_path()),
        Err(PathError::Device(_)) => false,
        Err(_) => true,
    }
}

impl BashTool {
    fn segments(&self, cmd: &str) -> Vec<Segment> {
        shell::segments(cmd, self.shell.dialect)
    }

    /// Происхождение недоверенного вывода (D82): сеть — по `reads_network`, иначе путь
    /// или `cwd` вне проекта.
    fn taint(&self, segs: &[Segment], root: &Path, cwd: Option<&str>) -> Option<String> {
        let d = self.shell.dialect;
        if segs.iter().any(|s| shell::reads_network(s, d)) {
            return Some(taint_origin::NETWORK.to_owned());
        }
        let outside = cwd.is_some_and(|c| outside_lex(root, c))
            || path_args(segs, d, cwd).iter().any(|w| outside_lex(root, w));
        outside.then(|| taint_origin::OUTSIDE_PROJECT.to_owned())
    }
}

/// Подготовка запуска в `spawn_blocking`: рабочий каталог, каталог временных файлов,
/// запись реестра групп (Linux).
struct Prepared {
    cwd: PathBuf,
    tmp: PathBuf,
    leftover: Option<za_jail::Leftover>,
}

fn prepare(
    root: &Path,
    cwd: Option<&str>,
    run: &str,
    stem: &str,
    home: &EditHome,
) -> Result<Prepared, ToolOutput> {
    let cwd_path = match cwd {
        None => root.to_path_buf(),
        Some(c) => {
            let lex = classify(root, c).map_err(|e| ToolOutput::error(e.to_string()))?;
            match std::fs::metadata(lex.as_path()) {
                Ok(m) if m.is_dir() => lex.as_path().to_path_buf(),
                Ok(_) => return Err(ToolOutput::error(cwd_not_dir_text(c))),
                Err(e) if crate::fs::is_missing(&e) => {
                    return Err(ToolOutput::error(cwd_missing_text(c)));
                }
                Err(e) => return Err(ToolOutput::error(failed_text(&e.kind().to_string()))),
            }
        }
    };
    let tmp = work_dir(root, &format!("{TMP_DIR}/{run}")).map_err(|e| {
        tracing::warn!(error = %e, "каталог временных файлов команды не создан");
        ToolOutput::error(tmp_error_text(e.kind()))
    })?;
    Ok(Prepared {
        cwd: cwd_path,
        tmp,
        leftover: leftover(home, run, stem),
    })
}

/// Запись реестра групп D84 (только Linux). Каталог не создался или он не свой и
/// доступен на запись другим (`za-jail` такой реестр отвергнет) — запуск без записи и
/// `warn`: дерево всё равно убивается при броске future, теряется только уборка после
/// смерти агента.
#[cfg(target_os = "linux")]
fn leftover(home: &EditHome, run: &str, stem: &str) -> Option<za_jail::Leftover> {
    use std::os::unix::fs::MetadataExt;
    let dir = za_core::paths::home_dir(
        &home.za_home,
        &format!("projects/{}/{PROCS_DIR}", home.project_id),
    )
    .and_then(|dir| {
        let m = std::fs::symlink_metadata(&dir)?;
        if m.uid() != nix::unistd::geteuid().as_raw() || m.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{}: не свой каталог или доступен на запись другим (права {:o})",
                    dir.display(),
                    m.mode() & 0o777
                ),
            ));
        }
        Ok(dir)
    });
    match dir {
        Ok(dir) => Some(za_jail::Leftover {
            file: dir.join(format!("{stem}.json")),
            run: run.to_owned(),
        }),
        Err(e) => {
            tracing::warn!(error = %e, "реестр групп процессов недоступен, запуск без записи");
            None
        }
    }
}

/// Реестр групп нужен только на Linux (D84): на Windows дерево держит Job Object.
#[cfg(not(target_os = "linux"))]
fn leftover(_home: &EditHome, _run: &str, _stem: &str) -> Option<za_jail::Leftover> {
    None
}

impl Tool for BashTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let req: Result<Args, ToolOutput> = parse(args);
        let runner = self.runner.clone();
        let shell = self.shell.clone();
        let home = self.home.clone();
        let segs = req
            .as_ref()
            .map(|r| self.segments(&r.cmd))
            .unwrap_or_default();
        let taint = req
            .as_ref()
            .ok()
            .and_then(|r| self.taint(&segs, &cx.project_dir, r.cwd.as_deref()));
        Box::pin(async move {
            let req = match req {
                Ok(r) => r,
                Err(o) => return o,
            };
            if req.cmd.trim().is_empty() {
                return ToolOutput::error(invalid_args_text(EMPTY_CMD));
            }
            let secs = req.timeout.unwrap_or(BASH_DEFAULT_TIMEOUT_S);
            if !(1..=BASH_MAX_TIMEOUT_S).contains(&secs) {
                return ToolOutput::error(timeout_range_text());
            }
            let root = cx.project_dir.clone();
            let cwd = req.cwd.clone();
            let run = cx.run.to_string();
            let stem = cx.effect_key.file_stem();
            let prep = tokio::task::spawn_blocking(move || {
                prepare(&root, cwd.as_deref(), &run, &stem, &home)
            })
            .await;
            let prep = match prep {
                Ok(Ok(p)) => p,
                Ok(Err(o)) => return o,
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                Err(e) => return ToolOutput::error(failed_text(&e.to_string())),
            };
            let mut spec =
                CommandSpec::new(shell.program.clone(), prep.cwd, Duration::from_secs(secs));
            spec.args = shell.args(&req.cmd);
            spec.tmp_dir = Some(prep.tmp);
            spec.leftover = prep.leftover;
            let o = match runner.run(spec).await {
                Ok(o) => o,
                Err(RunnerError::Spawn { program, source }) => {
                    return ToolOutput::error(failed_text(&format!("{program}: {source}")));
                }
                Err(e) => return ToolOutput::error(failed_text(&e.to_string())),
            };
            let stderr = match shell.dialect {
                Dialect::PowerShell => decode_clixml(&o.stderr),
                Dialect::Bash => o.stderr.clone(),
            };
            let content = render(&o, &stderr, secs);
            let header = head(&o, secs);
            let digest = digest(
                &segs,
                &o.stdout,
                &stderr,
                OUTPUT_LIMIT.saturating_sub(header.len() + BACKGROUND_KILLED.len() + 2),
            )
            .map(|d| {
                let mut s = format!("{header}\n{d}");
                if o.background_killed {
                    s.push('\n');
                    s.push_str(BACKGROUND_KILLED);
                }
                s
            });
            ToolOutput {
                content,
                is_error: o.exit == Exit::Timeout,
                taint,
                digest,
                post: Vec::new(),
            }
        })
    }

    fn subjects(&self, args: &Value) -> Subjects {
        let Some(cmd) = args.get("cmd").and_then(Value::as_str) else {
            return Subjects::default();
        };
        let cwd = args.get("cwd").and_then(Value::as_str);
        let d = self.shell.dialect;
        let segs = self.segments(cmd);
        let root = Path::new(LEX_ROOT);
        // `cwd` вне проекта сдвигает и относительные пути: граница — от него.
        let outside = |w: &str| match cwd {
            Some(c) if is_relative(w) => outside_lex(root, &format!("{c}/{w}")),
            _ => outside_lex(root, w),
        };
        let mut irreversible: Vec<&'static str> = Vec::new();
        for s in &segs {
            if let Some(l) = shell::irreversible(s, d, &outside)
                && !irreversible.contains(&l)
            {
                irreversible.push(l);
            }
        }
        Subjects {
            paths: path_args(&segs, d, cwd)
                .into_iter()
                .map(|arg| RawPath {
                    arg,
                    access: Access::Read,
                })
                .collect(),
            cwd: cwd.map(str::to_owned),
            command: Some(cmd.to_owned()),
            network: segs.iter().any(|s| shell::is_network(s, d)),
            irreversible,
            segments: segs,
        }
    }

    fn texts(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "runtime/bash.background_killed",
                BACKGROUND_KILLED.to_owned(),
            ),
            ("runtime/bash.cwd_missing", cwd_missing_text("sub")),
            ("runtime/bash.cwd_not_dir", cwd_not_dir_text("sub")),
            ("runtime/bash.digest.diff", diff_header(3, 5, 2)),
            ("runtime/bash.digest.gap", GAP.to_owned()),
            ("runtime/bash.digest.hunks", HUNKS_HEADER.to_owned()),
            ("runtime/bash.digest.more_entries", more_entries(800)),
            ("runtime/bash.digest.more_keys", more_keys(12)),
            ("runtime/bash.digest.more_lines", more_lines(120)),
            ("runtime/bash.digest.more_matches", more_matches(7)),
            ("runtime/bash.digest.tail", TAIL_HEADER.to_owned()),
            ("runtime/bash.digest.tests", tests_header(5000, 1)),
            ("runtime/bash.empty_cmd", invalid_args_text(EMPTY_CMD)),
            ("runtime/bash.exit", exit_text(3)),
            (
                "runtime/bash.failed",
                failed_text("pwsh: program not found"),
            ),
            ("runtime/bash.signal", signal_text(9)),
            ("runtime/bash.stderr", STDERR_HEADER.to_owned()),
            ("runtime/bash.stdout", STDOUT_HEADER.to_owned()),
            ("runtime/bash.timeout", timeout_text(10)),
            ("runtime/bash.timeout_range", timeout_range_text()),
            (
                "runtime/bash.tmp_error",
                tmp_error_text(io::ErrorKind::PermissionDenied),
            ),
            ("runtime/bash.truncated", truncated_text("stdout")),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_known_vectors() {
        // RFC 4648, раздел 10.
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o, "{i}");
        }
    }

    #[test]
    fn clixml_errors_become_text() {
        let err = "plain\n#< CLIXML\n<Objs Version=\"1.1.0.1\" xmlns=\"x\"><Obj S=\"progress\" RefId=\"0\"><TN RefId=\"0\"><T>X</T></TN></Obj><S S=\"Error\">Get-Item : Cannot find path &apos;C:\\x&apos; &lt;1&gt;_x000D__x000A_</S><S S=\"Error\">+ ~~~ \u{43f}_x000D__x000A_</S></Objs>\ntail\n";
        assert_eq!(
            decode_clixml(err),
            "plain\nGet-Item : Cannot find path 'C:\\x' <1>\n+ ~~~ \u{43f}\ntail\n"
        );
        // Суррогатная пара кодами `_xHHHH_`.
        assert_eq!(unescape_xml("_xD83D__xDE00_"), "\u{1F600}");
        // Без маркера — как есть, обрыв — текст до конца.
        assert_eq!(decode_clixml("a_x000D_b"), "a_x000D_b");
        assert_eq!(decode_clixml("#< CLIXML\n<Objs><S S=\"Error\">cut"), "cut");
    }

    #[test]
    fn outside_lex_rules() {
        let root = Path::new(LEX_ROOT);
        for (w, want) in [
            ("~/x", true),
            ("../x", true),
            ("a/../../x", true),
            ("a/../b", false),
            ("src/main.rs", false),
            ("NUL", false),
        ] {
            assert_eq!(outside_lex(root, w), want, "{w}");
        }
        assert!(outside_lex(
            root,
            if cfg!(windows) { r"D:\x" } else { "/etc/x" }
        ));
    }
}
