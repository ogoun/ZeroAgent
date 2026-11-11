//! Команда сегмента: снятие обёрток, строка вложенной интерпретации, выражения
//! PowerShell.

use crate::{Lang, label};

/// Строка вложенной интерпретации.
pub(crate) enum Inner {
    Code(String, Lang),
    /// Код есть, но его нельзя узнать: закодирован, из переменной, из файла-канала.
    Opaque,
    /// Интерпретатор читает код из stdin или конвейера (I4).
    Stdin,
}

/// Команда после снятия обёрток.
pub(crate) struct Cmd {
    /// Имя без пути, регистра и `.exe`.
    pub name: String,
    /// Слово команды как в `argv`.
    pub raw0: String,
    pub args: Vec<String>,
    /// Вызов через `&` или `.` PowerShell.
    pub call: bool,
    pub inner: Option<Inner>,
}

/// Служебные слова, после которых идёт команда.
#[rustfmt::skip]
const RESERVED: &[&str] = &[
    "then", "do", "else", "elif", "elseif", "if", "while", "until", "for", "switch", "try",
    "catch", "finally", "function", "filter", "trap", "!", "{",
];

/// Обёртки (I3): `(имя, опции со значением, позиционных слов перед командой)`.
#[rustfmt::skip]
const WRAPPERS: &[(&str, &[&str], usize)] = &[
    ("sudo", &["-u", "-g", "-p", "-C", "-D", "-r", "-t", "-T", "-U", "-h", "-R"], 0),
    ("doas", &["-u", "-C"], 0),
    ("pkexec", &["--user"], 0),
    ("runuser", &["-u", "-g", "-G", "--user", "--group", "--supp-group"], 0),
    ("setsid", &[], 0),
    ("nohup", &[], 0),
    ("builtin", &[], 0),
    ("exec", &["-a"], 0),
    ("nice", &["-n", "--adjustment"], 0),
    ("ionice", &["-c", "-n", "-p", "-P", "-u"], 0),
    ("chrt", &[], 1),
    ("taskset", &[], 1),
    ("timeout", &["-s", "-k", "--signal", "--kill-after"], 1),
    ("time", &["-o", "-f", "--output", "--format"], 0),
    ("stdbuf", &["-i", "-o", "-e"], 0),
    ("unbuffer", &[], 0),
    ("strace", &["-e", "-o", "-p", "-s", "-u", "-E", "-a", "-b", "-I", "-O", "-P", "-S", "-X"], 0),
    ("ltrace", &["-e", "-o", "-p", "-s", "-u", "-n", "-a", "-A", "-D", "-F", "-l"], 0),
    ("parallel", &["-j", "-S", "--jobs", "-a", "--arg-file", "-d", "-I", "--colsep"], 0),
    ("xargs", &["-n", "-I", "-P", "-d", "-L", "-s", "-E", "-a", "--max-args", "--max-procs",
        "--delimiter", "--arg-file"], 0),
    ("busybox", &[], 0),
    ("chroot", &["--userspec", "--groups"], 1),
    ("unshare", &["-S", "-G", "--setuid", "--setgid", "--wd", "-w", "-R", "--root"], 0),
    ("nsenter", &["-t", "--target", "-S", "-G", "--wd", "-w", "-R", "--root"], 0),
    ("firejail", &[], 0),
    ("flock", &["-w", "--timeout", "-E", "--conflict-exit-code"], 1),
    ("systemd-run", &["-u", "--unit", "-p", "--property", "-E", "--setenv", "-M", "--machine",
        "--uid", "--gid", "--nice", "--slice", "-H", "--host"], 0),
    ("numactl", &["-N", "-m", "-C", "-p", "-i"], 0),
    ("fakeroot", &["-l", "--lib", "-s", "-i", "--faked"], 0),
    ("wsl", &["-d", "--distribution", "-u", "--user", "--cd", "--shell-type"], 0),
    ("npx", &["-p", "--package"], 0),
    ("pnpx", &[], 0),
    ("bunx", &[], 0),
    ("uvx", &["--from", "--with"], 0),
];

pub(crate) fn analyze(argv: &[String], lang: Lang) -> Option<Cmd> {
    let mut w: Vec<String> = argv.to_vec();
    let mut i = 0;
    let mut call = false;
    loop {
        let x = w.get(i)?.clone();
        if i == 0 && (x == "&" || x == ".") {
            if x == "." && w.get(1).is_some_and(|a| stdin_script(a)) {
                return Some(cmd(".", x, &w[1..], false, Some(Inner::Opaque)));
            }
            call = true;
            i += 1;
            continue;
        }
        if lang == Lang::Pwsh && x.starts_with('$') {
            // Присваивание `$x = …`: команда — правая часть.
            if w.get(i + 1)
                .is_some_and(|o| matches!(o.as_str(), "=" | "+=" | "-=" | "*=" | "/=" | "??="))
            {
                i += 2;
                continue;
            }
            if let Some(pos) = x.find('=') {
                let var = &x[1..pos];
                if !var.is_empty()
                    && var
                        .chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '{' | '}'))
                {
                    let rhs = x[pos + 1..].to_string();
                    if rhs.is_empty() {
                        i += 1;
                    } else {
                        w[i] = rhs;
                    }
                    continue;
                }
            }
        }
        let n = norm(&x);
        if let Some(inner) = wrapper_code(&n, &w[i + 1..]) {
            return Some(cmd(&n, x, &w[i + 1..], call, Some(inner)));
        }
        match n.as_str() {
            r if RESERVED.contains(&r) => i += 1,
            r if WRAPPERS.iter().any(|(n, ..)| *n == r) => {
                let (_, vals, pos) =
                    WRAPPERS
                        .iter()
                        .find(|(n, ..)| *n == r)
                        .copied()
                        .unwrap_or(("", &[], 0));
                let start = i + 1;
                i = skip_opts(&w, start, vals) + pos;
                if i >= w.len()
                    && matches!(r, "sudo" | "doas")
                    && w[start..]
                        .iter()
                        .any(|a| matches!(a.as_str(), "-s" | "-i" | "--shell" | "--login"))
                {
                    // Оболочка без команды читает код из stdin.
                    return Some(cmd(&n, x, &[], call, Some(Inner::Stdin)));
                }
            }
            "command" => {
                if matches!(w.get(i + 1).map(String::as_str), Some("-v" | "-V")) {
                    return None;
                }
                i = skip_opts(&w, i + 1, &[]);
            }
            "env" => match env_skip(&w, i + 1) {
                Ok(j) => i = j,
                Err(code) => {
                    let inner = code_or_opaque(code, Lang::Bash);
                    return Some(cmd(&n, x, &w[i + 1..], call, Some(inner)));
                }
            },
            _ if is_assignment(&x) => i += 1,
            _ => break,
        }
    }
    let raw0 = w.get(i)?.clone();
    // cmd склеивает ключи с именем (`rd/s/q`): имя — до первого `/`, остаток — ключи (N1).
    if lang == Lang::Cmd
        && let Some((head, tail)) = raw0.split_once('/')
        && !head.is_empty()
    {
        let mut args: Vec<String> = tail.split('/').map(|s| format!("/{s}")).collect();
        args.extend(w[i + 1..].iter().cloned());
        let name = norm(head);
        let inner = inner_of(&name, &args);
        return Some(cmd(&name, head.to_string(), &args, call, inner));
    }
    let name = norm(&raw0);
    let inner = inner_of(&name, &w[i + 1..]);
    Some(cmd(&name, raw0, &w[i + 1..], call, inner))
}

fn cmd(name: &str, raw0: String, args: &[String], call: bool, inner: Option<Inner>) -> Cmd {
    Cmd {
        name: name.to_string(),
        raw0,
        args: args.to_vec(),
        call,
        inner,
    }
}

/// Опции обёртки до первого слова-не-опции; `vals` — опции со значением.
fn skip_opts(w: &[String], mut i: usize, vals: &[&str]) -> usize {
    while let Some(a) = w.get(i) {
        if a == "--" {
            return i + 1;
        }
        if a.len() > 1 && a.starts_with('-') {
            i += if vals.contains(&a.as_str()) { 2 } else { 1 };
        } else {
            break;
        }
    }
    i
}

/// Короткая опция (`-qc`) содержит букву `c`, или `--command`.
fn code_flag(a: &str) -> bool {
    a == "--command"
        || (a.len() > 1 && a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
}

/// Строка кода обёрток `su -c`, `runuser -c`, `script -c`, `flock -c`, `watch`, `alias`,
/// `ssh host команда` (I3).
fn wrapper_code(name: &str, args: &[String]) -> Option<Inner> {
    let after_c = || {
        let i = args.iter().position(|a| code_flag(a))?;
        Some(match args.get(i + 1) {
            Some(c) => code_or_opaque(c.clone(), Lang::Bash),
            None => Inner::Opaque,
        })
    };
    match name {
        "su" => Some(after_c().unwrap_or(Inner::Stdin)),
        "script" => Some(after_c().unwrap_or(Inner::Stdin)),
        "runuser" | "flock" => after_c(),
        "watch" => {
            let i = skip_opts(args, 0, &["-n", "--interval", "-q", "--equexit"]);
            Some(code_or_opaque(args.get(i..)?.join(" "), Lang::Bash))
        }
        "alias" => {
            let defs: Vec<&str> = args
                .iter()
                .filter_map(|a| a.split_once('='))
                .map(|d| d.1)
                .collect();
            (!defs.is_empty()).then(|| code_or_opaque(defs.join("; "), Lang::Bash))
        }
        "ssh" => {
            const SSH_VALS: &[&str] = &[
                "-b", "-c", "-D", "-E", "-e", "-F", "-I", "-i", "-J", "-L", "-l", "-m", "-O", "-o",
                "-p", "-Q", "-R", "-S", "-W", "-w", "-B",
            ];
            let i = skip_opts(args, 0, SSH_VALS);
            let rest = args.get(i + 1..).filter(|r| !r.is_empty())?;
            Some(code_or_opaque(rest.join(" "), Lang::Bash))
        }
        _ => None,
    }
}

/// Опции и присваивания `env`: `Ok(индекс команды)` или `Err(строка -S)`.
fn env_skip(w: &[String], mut i: usize) -> Result<usize, String> {
    while let Some(a) = w.get(i) {
        let s = a.as_str();
        match s {
            "-i" | "-0" | "-v" | "-" | "--ignore-environment" | "--null" | "--debug" => i += 1,
            "-u" | "-C" | "--unset" | "--chdir" => i += 2,
            "-S" | "--split-string" => return Err(w[i + 1..].join(" ")),
            "--" => return Ok(i + 1),
            _ if s.starts_with("--split-string=") || s.starts_with("-S") => {
                let first = s.strip_prefix("--split-string=").unwrap_or(&s[2..]);
                let mut parts = vec![first.to_string()];
                parts.extend(w[i + 1..].iter().cloned());
                return Err(parts.join(" "));
            }
            _ if s.starts_with("--unset=")
                || s.starts_with("--chdir=")
                || s.starts_with("-u")
                || s.starts_with("-C")
                || is_assignment(s) =>
            {
                i += 1
            }
            _ => return Ok(i),
        }
    }
    Ok(i)
}

/// `NAME=…` или `NAME+=…` перед командой.
fn is_assignment(w: &str) -> bool {
    let Some(pos) = w.find('=') else { return false };
    let name = w[..pos].strip_suffix('+').unwrap_or(&w[..pos]);
    let mut cs = name.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Имя команды: последний компонент пути, нижний регистр, без `.exe`, `.cmd`, `.bat`,
/// `.com` и `@` cmd. Слово-выражение (кавычки, скобки, `$`, пробел) путём не режется.
pub(crate) fn norm(w: &str) -> String {
    if w.contains(['(', ')', '[', ']', '{', '}', '$', '\'', '"', ' '])
        || w.contains(|c: char| matches!(c, '\u{2018}'..='\u{201E}'))
    {
        return w.to_lowercase();
    }
    let t = w.trim_end_matches(['/', '\\']);
    let t = if t.is_empty() { w } else { t };
    let base = t.rsplit(['/', '\\']).next().unwrap_or(t);
    let mut b = base.trim_start_matches('@').to_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".com"] {
        if b.len() > ext.len() && b.ends_with(ext) {
            b.truncate(b.len() - ext.len());
            break;
        }
    }
    b
}

/// `%NAME%` cmd.
pub(crate) fn percent_var(s: &str) -> bool {
    s.match_indices('%').any(|(i, _)| {
        s[i + 1..]
            .find('%')
            .is_some_and(|j| j > 0 && !s[i + 1..i + 1 + j].contains(' '))
    })
}

/// Слово, значение которого известно только при исполнении.
fn dynamic(raw: &str) -> bool {
    raw.contains('$')
        || raw.contains('`')
        || percent_var(raw)
        || (raw != "[" && raw != "[[" && (raw.contains('*') || raw.contains('[')))
        || (raw != "?" && raw.contains('?'))
        || raw.starts_with(['(', '{'])
        || raw.starts_with("@(")
        || raw.starts_with("@{")
}

/// Слово PowerShell в начале сегмента — выражение, а не имя команды.
pub(crate) fn pwsh_expression(raw: &str) -> bool {
    raw.starts_with(['$', '(', '[', '{', '\'', '"'])
        || raw.starts_with(|c: char| matches!(c, '\u{2018}'..='\u{201E}'))
        || raw.starts_with("@(")
        || raw.starts_with("@{")
        || raw.starts_with(|c: char| c.is_ascii_digit())
}

/// Слово-аргумент PowerShell — выражение с вызовом метода (`(gi x).Delete($true)`),
/// которое нужно прогнать через [`pwsh_expr`] (N1).
pub(crate) fn arg_is_expr(raw: &str) -> bool {
    (raw.starts_with(['(', '[']) || raw.starts_with("@(")) && raw.contains(").")
        || raw.starts_with('$') && raw.contains('.') && raw.contains('(')
}

/// Имя команды вычисляется при исполнении — команда неизвестна.
pub(crate) fn dynamic_cmd(c: &Cmd) -> bool {
    dynamic(&c.raw0)
}

/// Скрипт-аргумент, читающий код из канала (I4).
pub(crate) fn stdin_script(a: &str) -> bool {
    a == "-"
        || a == "/dev/stdin"
        || a.starts_with("/dev/fd/")
        || a.starts_with("<(")
        || a.starts_with(">(")
}

fn code_or_opaque(code: String, lang: Lang) -> Inner {
    let t = code.trim_start();
    if t.is_empty()
        || t == "-"
        || t.starts_with('$')
        || t.contains("{}")
        || (t.starts_with('%') && percent_var(t))
    {
        Inner::Opaque
    } else {
        Inner::Code(code, lang)
    }
}

/// Строка вложенной интерпретации команды, если команда — интерпретатор.
fn inner_of(name: &str, args: &[String]) -> Option<Inner> {
    match name {
        "bash" | "sh" | "zsh" | "dash" | "ksh" | "ash" | "mksh" | "yash" | "fish" | "tcsh"
        | "csh" | "nu" | "xonsh" | "elvish" => posix_inner(args),
        "eval" => (!args.is_empty()).then(|| code_or_opaque(args.join(" "), Lang::Bash)),
        "source" => args
            .first()
            .filter(|a| stdin_script(a))
            .map(|_| Inner::Opaque),
        "pwsh" | "powershell" | "pwsh-preview" => pwsh_inner(args),
        "iex" | "invoke-expression" => {
            let rest = match args.first() {
                Some(a) if a.len() > 2 && "-command".starts_with(&a.to_lowercase()) => &args[1..],
                _ => args,
            };
            let code = rest.join(" ");
            Some(if code.trim().is_empty() {
                Inner::Stdin
            } else if code.trim_start().starts_with(['(', '@', '[']) {
                Inner::Opaque
            } else {
                code_or_opaque(code, Lang::Pwsh)
            })
        }
        "cmd" => Some(cmd_inner(args)),
        // Вложенный запуск: аргументы собираются в строку по правилам Win32 (I2).
        "start" | "start-process" | "saps" => Some(Inner::Opaque),
        n => foreign_inner(n, args),
    }
}

/// `sh -c [опции] строка` (C1): строка — первое слово-не-опция после всех опций; без
/// `-c` и без скрипта код идёт из stdin.
fn posix_inner(args: &[String]) -> Option<Inner> {
    let mut i = 0;
    let (mut has_c, mut stdin, mut unknown_long, mut info) = (false, false, false, false);
    while let Some(a) = args.get(i) {
        if a == "--" || a == "-" {
            i += 1;
            break;
        }
        if let Some(l) = a.strip_prefix("--") {
            match l {
                "rcfile" | "init-file" => i += 2,
                "version" | "help" => {
                    info = true;
                    i += 1;
                }
                _ if l.starts_with("rcfile=") || l.starts_with("init-file=") => i += 1,
                "norc" | "noprofile" | "login" | "posix" | "noediting" | "restricted"
                | "verbose" | "debugger" | "dump-strings" | "pretty-print" => i += 1,
                _ => {
                    unknown_long = true;
                    i += 1;
                }
            }
            continue;
        }
        if a.len() > 1 && (a.starts_with('-') || a.starts_with('+')) {
            let flags = &a[1..];
            has_c |= flags.contains('c');
            stdin |= flags.contains('s');
            i += if flags.ends_with(['o', 'O']) { 2 } else { 1 };
            continue;
        }
        break;
    }
    if has_c {
        if unknown_long {
            return Some(Inner::Opaque);
        }
        return Some(match args.get(i) {
            Some(code) => code_or_opaque(code.clone(), Lang::Bash),
            None => Inner::Opaque,
        });
    }
    if stdin {
        return Some(Inner::Stdin);
    }
    match args.get(i) {
        Some(s) if stdin_script(s) => Some(Inner::Opaque),
        Some(_) => None,
        None if info => None,
        None => Some(Inner::Stdin),
    }
}

/// Опасные вызовы во встроенном коде других языков (`python -c`, `node -e`).
#[rustfmt::skip]
const FOREIGN_DANGER: &[&str] = &[
    "rmtree", "unlink", "remove(", "rmdir", "os.system", "subprocess", "popen", "exec(",
    "eval(", "spawn", "child_process", "urlopen", "urllib", "requests.", "socket",
    "http.client", "fs.rm", "rmsync", "writefile", "shutil.", "system(", "`", "kill(",
    "os.remove", "net::http", "lwp::", "io::socket", "fileutils.rm", "file.delete",
];

/// Опасный вызов на границе слова: токен не следует за буквой, цифрой, `_` или `.`.
/// Так `platform.system()` и `.remove(1)` не считаются `os.system`/`os.remove` (N5b).
/// Эвристика — защита «в глубину», а не граница (обфускация её обходит, записанный риск).
fn contains_danger(src: &str) -> bool {
    let b = src.as_bytes();
    FOREIGN_DANGER.iter().any(|d| {
        src.match_indices(d).any(|(i, _)| {
            i == 0 || !matches!(b[i - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'.')
        })
    })
}

/// `python`, `perl`, `ruby`, `node`, `php`, `Rscript`, `lua` (I4): без кода и без
/// скрипта — код из stdin; встроенный код с опасными вызовами — неразобран.
fn foreign_inner(name: &str, args: &[String]) -> Option<Inner> {
    let python = name.starts_with("python") || name == "py" || name.starts_with("pypy");
    // `code` — длинные флаги кода, `letters` — короткие буквы кода (в т. ч. в кластере
    // `-Ic`, N5a), `vals` — опции со значением.
    let (code, letters, vals): (&[&str], &[char], &[&str]) = match name {
        _ if python => (&[], &['c'], &["-W", "-X", "-Q"]),
        "node" => (
            &["--eval", "--print"],
            &['e', 'p'],
            &["-r", "--require", "--import"],
        ),
        "perl" | "ruby" => (&[], &['e', 'E'], &["-I", "-M", "-r"]),
        "php" => (&["-r"], &['r'], &["-c", "-d"]),
        "rscript" | "lua" | "luajit" => (&["-e"], &['e'], &[]),
        _ => return None,
    };
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        if a.len() > 1 && a.starts_with('-') {
            let cluster = !a.starts_with("--") && a[1..].contains(letters);
            if code.contains(&a.as_str()) || cluster {
                let src = args
                    .get(i + 1)
                    .map(|s| s.to_lowercase())
                    .unwrap_or_default();
                return contains_danger(&src).then_some(Inner::Opaque);
            }
            if (python && a == "-m")
                || matches!(
                    a.as_str(),
                    "--version" | "-V" | "--help" | "-h" | "-v" | "-version"
                )
            {
                return None;
            }
            i += if vals.contains(&a.as_str()) { 2 } else { 1 };
            continue;
        }
        break;
    }
    match args.get(i) {
        Some(s) if s == "-" => Some(Inner::Stdin),
        Some(s) if stdin_script(s) => Some(Inner::Opaque),
        Some(_) => None,
        None => Some(Inner::Stdin),
    }
}

/// Полные имена параметров `pwsh` со значением (сопоставляются по префиксу от 2 букв).
#[rustfmt::skip]
const PS_VALUE_FULL: &[&str] = &[
    "executionpolicy", "windowstyle", "workingdirectory", "configurationname", "outputformat",
    "inputformat", "psconsolefile", "settingsfile", "custompipename", "configurationfile",
    "version",
];

/// `pwsh -Command`, `powershell "код"`; `-EncodedCommand` не разбирается, без кода —
/// stdin.
fn pwsh_inner(args: &[String]) -> Option<Inner> {
    let mut i = 0;
    let mut info = false;
    while let Some(a) = args.get(i) {
        let Some(p) = a.strip_prefix('-').or_else(|| a.strip_prefix('/')) else {
            if a.to_lowercase().ends_with(".ps1") {
                return None;
            }
            return Some(code_or_opaque(args[i..].join(" "), Lang::Pwsh));
        };
        let p = p.split(':').next().unwrap_or("").to_lowercase();
        if p.is_empty() {
            return Some(Inner::Stdin);
        }
        if p == "e" || p == "ec" || p == "ea" || p.starts_with("en") {
            return Some(Inner::Opaque);
        }
        if "command".starts_with(&p) || p == "cwa" || p.starts_with("commandwith") {
            let code = args[i + 1..].join(" ");
            if code.trim().is_empty() || code.trim() == "-" {
                return Some(Inner::Stdin);
            }
            return Some(code_or_opaque(code, Lang::Pwsh));
        }
        if "file".starts_with(&p) {
            return None;
        }
        if matches!(p.as_str(), "v" | "version" | "h" | "help" | "?") {
            info = true;
        }
        let value = matches!(
            p.as_str(),
            "ex" | "ep" | "w" | "wd" | "of" | "if" | "o" | "i"
        ) || (p.len() >= 2 && PS_VALUE_FULL.iter().any(|f| f.starts_with(&p)));
        i += if value { 2 } else { 1 };
    }
    if info { None } else { Some(Inner::Stdin) }
}

/// `cmd /c код`, `cmd /k код`; без них cmd читает stdin.
fn cmd_inner(args: &[String]) -> Inner {
    for (i, a) in args.iter().enumerate() {
        let l = a.to_lowercase();
        let Some(r) = l.strip_prefix('/') else {
            return Inner::Opaque;
        };
        if matches!(r.trim_start_matches('/'), "c" | "k" | "r") {
            return code_or_opaque(args[i + 1..].join(" "), Lang::Cmd);
        }
    }
    Inner::Stdin
}

// --- Выражения PowerShell (I2, I6) ----------------------------------------------------

/// Итог разбора выражения PowerShell.
pub(crate) enum Expr {
    /// Без побочных эффектов или чтение.
    Safe,
    /// Метод с меткой необратимости.
    Label(&'static str),
    /// Метод обращается к сети.
    Network,
    /// Неизвестный метод неизвестного типа.
    Unknown,
}

/// Статические типы без побочных эффектов (I6).
#[rustfmt::skip]
const SAFE_TYPES: &[&str] = &[
    "math", "string", "io.path", "char", "int", "int32", "int64", "long", "double", "decimal",
    "float", "single", "byte", "bool", "boolean", "datetime", "timespan", "regex",
    "text.regularexpressions.regex", "convert", "guid", "version", "bitconverter",
    "text.encoding", "uri", "array",
];

/// Методы без побочных эффектов или только чтение (I6).
#[rustfmt::skip]
const SAFE_METHODS: &[&str] = &[
    "tostring", "toupper", "tolower", "toupperinvariant", "tolowerinvariant", "trim",
    "trimstart", "trimend", "replace", "split", "substring", "contains", "startswith",
    "endswith", "join", "combine", "getfilename", "getfilenamewithoutextension",
    "getextension", "getdirectoryname", "getfullpath", "getpathroot", "ispathrooted",
    "changeextension", "equals", "indexof", "lastindexof", "padleft", "padright", "format",
    "round", "floor", "ceiling", "truncate", "abs", "max", "min", "pow", "sqrt", "parse",
    "tryparse", "tochararray", "toarray", "gettype", "compareto", "isnullorempty",
    "isnullorwhitespace", "adddays", "addhours", "addminutes", "addseconds", "match",
    "matches", "escape", "ismatch", "tobase64string", "frombase64string", "toint32",
    "toint64", "todouble", "toboolean", "newguid", "getenvironmentvariable",
    "getenvironmentvariables", "getfolderpath", "readalltext", "readalllines",
    "readallbytes", "exists", "getfiles", "getdirectories", "enumeratefiles",
    "enumeratedirectories", "getcurrentdirectory", "normalize", "insert", "getbytes",
    "getstring", "getvalue", "foreach", "where", "count", "sum",
];

/// Метка опасного метода объекта (I2): удаление, запись, запуск, сеть.
pub(crate) fn method_expr(m: &str) -> Option<Expr> {
    let m = m.to_lowercase();
    let m = m.as_str();
    if matches!(
        m,
        "delete" | "remove" | "deletesubkey" | "deletesubkeytree" | "deletevalue"
    ) {
        return Some(Expr::Label(label::DELETE));
    }
    if m.starts_with("writeall")
        || m.starts_with("appendall")
        || matches!(
            m,
            "moveto"
                | "copyto"
                | "move"
                | "copy"
                | "setvalue"
                | "create"
                | "createtext"
                | "appendtext"
                | "createsubkey"
                | "setaccesscontrol"
        )
    {
        return Some(Expr::Label(label::OVERWRITE));
    }
    if m.starts_with("invoke") || matches!(m, "start" | "kill") {
        return Some(Expr::Label(label::EXEC_UNTRUSTED));
    }
    if m.starts_with("download")
        || m.starts_with("upload")
        || matches!(
            m,
            "openread"
                | "openwrite"
                | "getresponse"
                | "send"
                | "sendasync"
                | "getasync"
                | "postasync"
                | "getstringasync"
                | "connect"
        )
    {
        return Some(Expr::Network);
    }
    None
}

/// Разбор выражения PowerShell: статический тип и вызовы методов верхнего уровня.
pub(crate) fn pwsh_expr(raw: &str) -> Expr {
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    let mut ty = None;
    if chars.first() == Some(&'[') {
        let mut depth = 0usize;
        while let Some(&c) = chars.get(i) {
            depth = match c {
                '[' => depth + 1,
                ']' => depth.saturating_sub(1),
                _ => depth,
            };
            i += 1;
            if depth == 0 {
                break;
            }
        }
        let t: String = chars[1..i.saturating_sub(1).max(1)].iter().collect();
        let t = t.to_lowercase();
        ty = Some(t.strip_prefix("system.").unwrap_or(&t).to_string());
    }
    // Пары `(имя метода, первая строка-аргумент)` вызовов верхнего уровня.
    let mut methods: Vec<(String, Option<String>)> = Vec::new();
    let mut depth = 0usize;
    while let Some(&c) = chars.get(i) {
        match c {
            '\'' | '"' => {
                i += 1;
                while chars.get(i).is_some_and(|&x| x != c) {
                    i += if chars[i] == '`' { 2 } else { 1 };
                }
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            '.' | ':' if depth == 0 => {
                let mut j = i + 1;
                if c == ':' && chars.get(j) == Some(&':') {
                    j += 1;
                }
                // Динамическое имя члена `.(…)`, `.$x`, `.@(…)` — имя неизвестно (N1).
                if c == '.' && matches!(chars.get(j), Some('(' | '$' | '@')) {
                    return Expr::Unknown;
                }
                let start = j;
                while chars
                    .get(j)
                    .is_some_and(|x| x.is_alphanumeric() || *x == '_')
                {
                    j += 1;
                }
                let first_digit = chars.get(start).is_some_and(|x| x.is_ascii_digit());
                if j > start && !first_digit && chars.get(j) == Some(&'(') {
                    let name = chars[start..j].iter().collect::<String>().to_lowercase();
                    let arg = first_string_arg(&chars, j);
                    methods.push((name, arg));
                }
                i = j;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    let safe_type = ty.as_deref().is_some_and(|t| SAFE_TYPES.contains(&t));
    let fs_type = ty
        .as_deref()
        .is_some_and(|t| matches!(t, "io.file" | "io.directory" | "file" | "directory"));
    for (m, arg) in &methods {
        if let Some(e) = method_expr(m) {
            return e;
        }
        // `.ForEach('Delete')`/`.Where('Delete')`: имя метода задано строкой (N1).
        if matches!(m.as_str(), "foreach" | "where")
            && let Some(e) = arg.as_deref().and_then(method_expr)
        {
            return e;
        }
        // На `[IO.File]`/`[IO.Directory]` безопасно только чтение (N1).
        if fs_type
            && !m.starts_with("readall")
            && !matches!(
                m.as_str(),
                "exists"
                    | "getfiles"
                    | "getdirectories"
                    | "enumeratefiles"
                    | "enumeratedirectories"
                    | "readlines"
            )
        {
            return Expr::Label(label::OVERWRITE);
        }
        if !SAFE_METHODS.contains(&m.as_str()) && !safe_type {
            return Expr::Unknown;
        }
    }
    Expr::Safe
}

/// Первая строка-аргумент вызова, открытая `(` в позиции `open`.
fn first_string_arg(chars: &[char], open: usize) -> Option<String> {
    let mut i = open + 1;
    while let Some(&c) = chars.get(i) {
        match c {
            ' ' | '\t' => i += 1,
            '\'' | '"' => {
                let q = c;
                i += 1;
                let start = i;
                while chars.get(i).is_some_and(|&x| x != q) {
                    i += 1;
                }
                return Some(chars[start..i].iter().collect());
            }
            _ => return None,
        }
    }
    None
}
