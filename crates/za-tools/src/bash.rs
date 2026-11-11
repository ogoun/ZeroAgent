//! `bash(cmd, cwd?, timeout?)` (R1-C1 D85, таблица D74; D77, D82, D84, D86; Carry 3).
//!
//! Команда запускается только через [`za_jail::Runner`] (SEC-006): Windows — PowerShell
//! (`pwsh`, иначе `powershell.exe`) с `-EncodedCommand` ([`powershell_encoded`]), Unix —
//! `bash -c`, иначе `sh -c`. Оболочку выбирает [`Shell::detect`] при создании инструмента.
//! `TMP`, `TEMP`, `TMPDIR` дочернего процесса — `<project>/.zero-agent/work/tmp/<run>`
//! (`za_core::paths::work_dir`), системный каталог временных файлов не передаётся. На
//! Linux запись реестра групп — `ZA_HOME/projects/<project_id>/procs/<file_stem>.json`
//! ([`AgentHome`], `home_dir`): по ней `reap_leftovers` убирает сирот после смерти агента.
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

use crate::AgentHome;
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
/// `$global:ZaLastOk = $true` до блока: `return` в команде выходит из блока раньше
/// строки, запоминающей `$?`, и без начального значения давал бы код 1 (m1 ревью).
const PS_PREFIX: &str = "$ProgressPreference='SilentlyContinue'; \
    [Console]::OutputEncoding=[Text.Encoding]::UTF8; $OutputEncoding=[Text.Encoding]::UTF8; \
    $global:ZaLastOk = $true; & {";
/// Конец обёртки: `$?` последней команды запоминается внутри блока (после `}` его уже
/// сбросило бы условие `if`); `}` — на своей строке, чтобы `#` в команде не съел скобку.
/// Перед ним — пустая строка: конечный `` ` `` команды не продолжит её текстом обёртки.
const PS_SUFFIX: &str = "$global:ZaLastOk = $?\n}; \
    if ($LASTEXITCODE) { exit $LASTEXITCODE } elseif (-not $global:ZaLastOk) { exit 1 }";

// Тексты для модели (D23, D94): английский, только ASCII-пунктуация.
const DESCRIPTION: &str = "Run a shell command in the project directory. \
    PowerShell on Windows, bash elsewhere; see shell in <env>. \
    A non-zero exit code is reported in the result, it is not a tool error.";
const STDOUT_HEADER: &str = "--- stdout ---";
const BACKGROUND_KILLED: &str = "[background processes killed]";
const EMPTY_CMD: &str = "cmd is empty";
/// Предел длины командной строки Windows (`CreateProcess`), символов UTF-16.
const WINDOWS_CMDLINE_MAX: usize = 32_767;
/// Предел длины одного аргумента Linux (`MAX_ARG_STRLEN`) без завершающего NUL.
const UNIX_ARG_MAX: usize = 128 * 1024 - 1;

fn too_long_text(n: usize, max: usize) -> String {
    invalid_args_text(&format!(
        "cmd is too long for the command line ({n} of {max} allowed); write a script file and run it"
    ))
}

/// Ошибка ОС для модели: вид и код, без локализованного текста (m2 ревью, D23).
fn os_error_text(e: &io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => format!("{} (os error {code})", e.kind()),
        None => e.kind().to_string(),
    }
}

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

    /// Длина команды в единицах предела ОС и сам предел (m3 ревью): Windows — вся
    /// командная строка с `-EncodedCommand`, Unix — один аргумент `-c`.
    fn length(&self, cmd: &str) -> (usize, usize) {
        match self.dialect {
            Dialect::PowerShell => {
                let line: usize = self.args(cmd).iter().map(|a| a.len() + 1).sum::<usize>()
                    + self.program.as_os_str().len()
                    + 2;
                (line, WINDOWS_CMDLINE_MAX)
            }
            Dialect::Bash => (cmd.len(), UNIX_ARG_MAX),
        }
    }

    /// Команда, которую лексер может разобрать не так, как оболочка (C1 ревью):
    /// управляющие символы (кроме табуляции, перевода строки и `\r\n`), нестандартные
    /// кавычки, пробелы Unicode, символы нулевой ширины и направления текста; у `sh`
    /// вместо `bash` — ещё `$'…'` (dash разбирает его иначе, m7).
    fn suspicious(&self, cmd: &str) -> bool {
        // Bash: внутри обычных кавычек нестандартная кавычка — литерал (раунд 2 ревью:
        // сообщения коммитов), вне кавычек — подозрительна. У PowerShell U+2018–U+201E —
        // настоящие кавычки, поэтому подозрительны везде.
        let bash = self.dialect == Dialect::Bash;
        let (mut single, mut double, mut escaped) = (false, false, false);
        let mut it = cmd.chars().peekable();
        while let Some(c) = it.next() {
            let quoted = single || double;
            if bash {
                match c {
                    _ if escaped => escaped = false,
                    '\\' if !single => escaped = true,
                    '\'' if !double => single = !single,
                    '"' if !single => double = !double,
                    _ => {}
                }
            }
            let bad = match c {
                '\t' | '\n' | ' ' => false,
                '\r' => it.peek() != Some(&'\n'),
                '\u{2018}'..='\u{201F}'
                | '\u{2032}'..='\u{2037}'
                | '\u{00AB}'
                | '\u{00BB}'
                | '\u{02BC}'
                | '\u{2039}'
                | '\u{203A}'
                | '\u{300C}'..='\u{300F}'
                | '\u{301D}'..='\u{301F}'
                | '\u{FF02}'
                | '\u{FF07}' => !(bash && quoted),
                '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}' => true,
                c => c.is_control() || c.is_whitespace(),
            };
            if bad {
                return true;
            }
        }
        self.dialect == Dialect::Bash && !self.is_bash() && cmd.contains("$'")
    }

    /// Программа — именно `bash` (не `sh`).
    fn is_bash(&self) -> bool {
        self.program
            .file_stem()
            .is_some_and(|n| n.eq_ignore_ascii_case("bash"))
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
    let script = format!("{PS_PREFIX}\n{cmd}\n\n{PS_SUFFIX}");
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
/// прогресса и прочие объекты отбрасываются. Разбирается только правильно сформированный
/// блок (m4 ревью): маркер в начале строки, сразу за ним `<Objs `, конец `</Objs>`;
/// иначе текст — недоверенный вывод команды — остаётся как есть.
fn decode_clixml(stderr: &str) -> String {
    const MARK: &str = "#< CLIXML\n";
    const OPEN: &str = "<Objs ";
    const END: &str = "</Objs>";
    if !stderr.contains(MARK) {
        return stderr.to_owned();
    }
    let mut out = String::new();
    let mut rest = stderr;
    while let Some(at) = rest.find(MARK) {
        let xml = &rest[at + MARK.len()..];
        let line_start = at == 0 || rest[..at].ends_with('\n');
        let (true, true, Some(end)) = (line_start, xml.starts_with(OPEN), xml.find(END)) else {
            // Не блок обёртки: маркер — обычный текст.
            out.push_str(&rest[..at + MARK.len()]);
            rest = xml;
            continue;
        };
        out.push_str(&rest[..at]);
        out.push_str(&clixml_text(&xml[..end]).replace("\r\n", "\n"));
        let after = &xml[end + END.len()..];
        rest = after.strip_prefix('\n').unwrap_or(after);
    }
    out.push_str(rest);
    out
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
    home: AgentHome,
}

/// Инструмент `bash(cmd, cwd?, timeout?)`. `home` — `ZA_HOME` и проект для реестра групп
/// процессов на Linux (D84; на других ОС не используется).
pub fn bash_tool(runner: Arc<dyn Runner>, shell: Shell, home: AgentHome) -> Arc<dyn Tool> {
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
    /// Сегменты команды; команда, которую лексер может разобрать не так, как оболочка
    /// ([`Shell::suspicious`]), — все сегменты неразобраны (Ask, D81).
    fn segments(&self, cmd: &str) -> Vec<Segment> {
        let mut segs = shell::segments(cmd, self.shell.dialect);
        if self.shell.suspicious(cmd) {
            for s in &mut segs {
                s.unparsed = true;
            }
        }
        segs
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
    home: &AgentHome,
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

/// Запись реестра групп D84 (только Linux). Каталог не создался или он не свой
/// (`za_jail::check_registry`) — запуск без записи и `warn`: дерево всё равно убивается
/// при броске future, теряется только уборка после смерти агента.
#[cfg(target_os = "linux")]
fn leftover(home: &AgentHome, run: &str, stem: &str) -> Option<za_jail::Leftover> {
    // Каталог 0700 при любом umask (ревью задачи 8, I1), проверка — та же, что у
    // `za-jail` при записи (без повтора здесь).
    let dir = za_core::paths::private_home_dir(
        &home.za_home,
        &format!("projects/{}/{PROCS_DIR}", home.project_id),
    )
    .and_then(|dir| za_jail::check_registry(&dir).map(|()| dir));
    match dir {
        Ok(dir) => Some(za_jail::Leftover {
            file: dir.join(format!("{stem}.json")),
            run: run.to_owned(),
        }),
        Err(e) => {
            // Один `warn` на процесс (m9): дальше — `debug`.
            static WARNED: std::sync::Once = std::sync::Once::new();
            let mut first = false;
            WARNED.call_once(|| first = true);
            if first {
                tracing::warn!(error = %e, "реестр групп процессов недоступен, запуск без записи");
            } else {
                tracing::debug!(error = %e, "реестр групп процессов недоступен");
            }
            None
        }
    }
}

/// Реестр групп нужен только на Linux (D84): на Windows дерево держит Job Object.
#[cfg(not(target_os = "linux"))]
fn leftover(_home: &AgentHome, _run: &str, _stem: &str) -> Option<za_jail::Leftover> {
    None
}

/// Текст результата, сжатое представление и признак ошибки по выводу процесса.
fn finish(
    o: &Output,
    dialect: Dialect,
    segs: &[Segment],
    secs: u64,
) -> (String, Option<String>, bool) {
    let stderr = match dialect {
        Dialect::PowerShell => decode_clixml(&o.stderr),
        Dialect::Bash => o.stderr.clone(),
    };
    let content = render(o, &stderr, secs);
    let header = head(o, secs);
    let digest = digest(
        segs,
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
    (content, digest, o.exit == Exit::Timeout)
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
            let (len, max) = shell.length(&req.cmd);
            if len > max {
                return ToolOutput::error(too_long_text(len, max));
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
                // Сообщение ОС локализовано: модели — вид и код ошибки, полный текст — в
                // лог (m2 ревью, D23).
                Err(RunnerError::Spawn { program, source }) => {
                    tracing::warn!(%program, error = %source, "оболочка не запустилась");
                    return ToolOutput::error(failed_text(&format!(
                        "{program}: {}",
                        os_error_text(&source)
                    )));
                }
                Err(RunnerError::Io(e)) => {
                    tracing::warn!(error = %e, "сбой ввода-вывода команды");
                    return ToolOutput::error(failed_text(&format!("io: {}", os_error_text(&e))));
                }
            };
            // Разбор CLIXML и сжатие вывода до 32 МиБ — вне потоков tokio (m6 ревью).
            let dialect = shell.dialect;
            let post = tokio::task::spawn_blocking(move || finish(&o, dialect, &segs, secs)).await;
            match post {
                Ok((content, digest, is_error)) => ToolOutput {
                    content,
                    is_error,
                    taint,
                    digest,
                    post: Vec::new(),
                },
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                Err(e) => ToolOutput::error(failed_text(&e.to_string())),
            }
        })
    }

    fn subjects(&self, args: &Value) -> Subjects {
        let Some(cmd) = args.get("cmd").and_then(Value::as_str) else {
            return Subjects::default();
        };
        let cwd = args.get("cwd").and_then(Value::as_str);
        let d = self.shell.dialect;
        // Метки — по разбору лексера (их видно пользователю в вопросе); страховка
        // добавляет `unparsed` и снимает применимость `allow` ко всем сегментам.
        let mut segs = shell::segments(cmd, d);
        let suspicious = self.shell.suspicious(cmd);
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
        let paths = path_args(&segs, d, cwd)
            .into_iter()
            .map(|arg| RawPath {
                arg,
                access: Access::Read,
            })
            .collect();
        let network = segs.iter().any(|s| shell::is_network(s, d));
        if suspicious {
            if !irreversible.contains(&shell::label::UNPARSED) {
                irreversible.push(shell::label::UNPARSED);
            }
            for s in &mut segs {
                s.unparsed = true;
            }
        }
        Subjects {
            paths,
            cwd: cwd.map(str::to_owned),
            command: Some(cmd.to_owned()),
            network,
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
                failed_text("pwsh: entity not found (os error 2)"),
            ),
            ("runtime/bash.signal", signal_text(9)),
            ("runtime/bash.stderr", STDERR_HEADER.to_owned()),
            ("runtime/bash.stdout", STDOUT_HEADER.to_owned()),
            ("runtime/bash.timeout", timeout_text(10)),
            ("runtime/bash.timeout_range", timeout_range_text()),
            (
                "runtime/bash.too_long",
                too_long_text(40_000, WINDOWS_CMDLINE_MAX),
            ),
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
        // Без маркера — как есть.
        assert_eq!(decode_clixml("a_x000D_b"), "a_x000D_b");
        // m4: неправильно сформированный блок (обрыв, не `<Objs `, маркер не в начале
        // строки) — недоверенный текст команды, остаётся как есть.
        for raw in [
            "#< CLIXML\n<Objs Version=\"1\"><S S=\"Error\">cut",
            "#< CLIXML\nerror: real line\n<S S=\"Error\">x</S></Objs>\n",
            "log: #< CLIXML\n<Objs V=\"1\"><S S=\"Error\">x</S></Objs>\n",
        ] {
            assert_eq!(decode_clixml(raw), raw, "{raw}");
        }
        // Текст до блока обёртки сохраняется, блок разбирается.
        assert_eq!(
            decode_clixml(
                "err\n#< CLIXML\n<Objs V=\"1\"><S S=\"Error\">boom_x000D__x000A_</S></Objs>"
            ),
            "err\nboom\n"
        );
    }

    #[test]
    fn suspicious_commands() {
        let ps = Shell {
            program: PathBuf::from("powershell.exe"),
            dialect: Dialect::PowerShell,
        };
        let bash = Shell {
            program: PathBuf::from("/bin/bash"),
            dialect: Dialect::Bash,
        };
        let sh = Shell {
            program: PathBuf::from("/bin/sh"),
            dialect: Dialect::Bash,
        };
        for c in [
            "echo a\rrm -rf x",
            "echo x\u{2019}; rm x",
            "echo \u{201C}x\u{201D}",
            "echo \u{ab}x\u{bb}",
            "rm\u{a0}-rf x",
            "rm\u{2028}-rf x",
            "rm\u{3000}-rf x",
            "rm\u{0b}-rf x",
            "rm\u{0c}-rf x",
            "rm\u{85}-rf x",
            "rm\0-rf x",
            "rm\u{1b}[0m x",
            "echo a\u{200b}b",
            "echo \u{202e}x",
            "echo \u{feff}x",
        ] {
            assert!(ps.suspicious(c), "{c:?}");
            assert!(bash.suspicious(c), "{c:?}");
        }
        // Раунд 2: в bash нестандартные кавычки внутри обычных — литералы.
        for c in [
            "git commit -m \"исправлено \u{ab}x\u{bb}\"",
            "git commit -m 'it\u{2019}s done'",
            "echo \"a \\\" \u{201C}b\u{201D}\"",
        ] {
            assert!(!bash.suspicious(c), "{c:?}");
            assert!(ps.suspicious(c), "{c:?}");
        }
        assert!(bash.suspicious("echo \u{2019}x"));
        assert!(
            bash.suspicious("echo \"a\u{200b}\""),
            "нулевая ширина — везде"
        );
        for c in [
            "cargo test -p za-core",
            "echo 'привет, мир'",
            "a\tb\nc\r\nd",
            "git commit -m \"fix: it's done\"",
        ] {
            assert!(!ps.suspicious(c), "{c:?}");
            assert!(!bash.suspicious(c), "{c:?}");
        }
        // m7: `$'…'` у `sh` — неразобрано, у `bash` — нет.
        let c = r"echo $'\'; rm -rf ../x; echo \''";
        assert!(sh.suspicious(c));
        assert!(!bash.suspicious(c));
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
