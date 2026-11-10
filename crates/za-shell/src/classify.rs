//! Классификаторы по разобранной команде: необратимые (D81), сетевые и читающие из сети
//! (D77, D82).

use crate::command::{
    Cmd, Expr, Inner, analyze, dynamic_cmd, method_expr, norm, percent_var, pwsh_expr,
    pwsh_expression,
};
use crate::lex::lex_code;
use crate::{Lang, Segment, label};

/// Неразобранный сегмент и перенаправление вывода наружу или в неизвестную цель.
pub(crate) fn base_label(seg: &Segment, outside: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    if seg.unparsed {
        return Some(label::UNPARSED);
    }
    seg.redirects.iter().find_map(|r| match parse_redirect(r) {
        (true, Some(t)) if write_out(t, outside) => Some(label::OVERWRITE),
        _ => None,
    })
}

/// Запись в эту цель необратима: она снаружи, устройство или известна только при
/// исполнении.
fn write_out(t: &str, outside: &dyn Fn(&str) -> bool) -> bool {
    !null_dev(t) && (dynamic_path(t) || dev_path(t) || outside(t))
}

/// Сегмент без рекурсии во вложенную интерпретацию: она здесь — уже уровень глубже.
fn direct(seg: &Segment, lang: Lang, outside: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    base_label(seg, outside)
        .or_else(|| analyze(&seg.argv, lang).and_then(|c| check_cmd(&c, lang, outside, false)))
}

/// Опасные команды, спрятанные в аргументах неизвестной команды (I3).
#[rustfmt::skip]
const DANGER_WORDS: &[&str] = &[
    "rm", "del", "erase", "rd", "remove-item", "ri", "shred", "wipefs", "dd", "git", "curl",
    "wget", "sh", "bash", "zsh", "dash", "pwsh", "powershell", "cmd", "iex",
    "invoke-expression", "eval", "sudo", "su", "nc", "rimraf",
];

/// Известные команды, у которых имена команд в аргументах — данные (печать, поиск, сборка).
#[rustfmt::skip]
const KNOWN: &[&str] = &[
    "echo", "printf", "write-host", "write-output", "write", "grep", "egrep", "fgrep", "zgrep",
    "rg", "ag", "ack", "findstr", "select-string", "sls", "man", "help", "get-help", "which",
    "where", "whereis", "type", "whatis", "apropos", "info", "tldr", "get-command", "gcm",
    "hash", "cargo", "rustup", "rustc", "npm", "pnpm", "yarn", "make", "cmake", "just", "go",
    "dotnet", "mvn", "gradle", "pip", "pip3", "uv", "poetry", "tox", "nox", "pytest", "jest",
    "vitest", "docker", "podman", "kubectl", "terraform", "helm", "gh", "apt", "apt-get",
    "brew", "choco", "winget", "scoop", "code", "vim", "nvim", "nano", "less", "more", "cat",
    "head", "tail", "wc", "sort", "uniq", "ls", "dir", "test", "[", "[[", "true", "false",
    "set", "unset", "export", "complete", "compgen", "sleep", "date", "tar", "zip", "unzip",
    "7z", "cp", "mv", "ln", "mkdir", "touch", "chmod", "chown", "stat", "file", "du", "df",
    "diff", "patch", "sed", "awk", "cut", "tr", "jq", "yq", "base64", "tee", "tee-object",
    "out-file", "set-content", "add-content", "get-content", "get-childitem", "gci",
    "select-object", "where-object", "foreach-object", "%", "?", "measure-object", "new-item",
    "copy-item", "move-item", "test-path", "resolve-path", "join-path", "split-path", "git",
    "find", "rsync", "robocopy", "psql", "mysql", "sqlite3", "twine", "gem", "nuget",
];

pub(crate) fn check_cmd(
    c: &Cmd,
    lang: Lang,
    outside: &dyn Fn(&str) -> bool,
    recurse: bool,
) -> Option<&'static str> {
    match &c.inner {
        Some(Inner::Opaque) => return Some(label::UNPARSED),
        Some(Inner::Stdin) => return Some(label::EXEC_UNTRUSTED),
        _ => {}
    }
    if lang == Lang::Pwsh && !c.call && pwsh_expression(&c.raw0) {
        // Выражение: тела групп и блоков — отдельные сегменты, здесь — методы (I6).
        return match pwsh_expr(&c.raw0) {
            Expr::Label(l) => Some(l),
            Expr::Unknown => Some(label::UNPARSED),
            Expr::Safe | Expr::Network => None,
        };
    }
    if dynamic_cmd(c) {
        return Some(label::UNPARSED);
    }
    if let Some(l) = classify(c, lang, outside, recurse) {
        return Some(l);
    }
    match &c.inner {
        Some(Inner::Code(code, l)) if recurse => lex_code(code, *l, 1, 0)
            .iter()
            .find_map(|s| direct(s, *l, outside)),
        Some(Inner::Code(..)) => Some(label::UNPARSED),
        _ => {
            (!KNOWN.contains(&c.name.as_str()) && hides_danger(&c.args)).then_some(label::UNPARSED)
        }
    }
}

/// Отдельное слово аргументов — известная опасная команда.
fn hides_danger(args: &[String]) -> bool {
    args.iter().any(|a| {
        !a.starts_with('-') && !a.contains(' ') && {
            let n = norm(a);
            DANGER_WORDS.contains(&n.as_str()) || n.starts_with("mkfs")
        }
    })
}

/// `(вывод, цель-файл)` перенаправления; дублирование дескриптора и here-doc — без цели.
pub(crate) fn parse_redirect(r: &str) -> (bool, Option<&str>) {
    let body = r.trim_start_matches(|c: char| c.is_ascii_digit() || c == '*' || c == '&');
    for op in [">>", ">|", ">&", ">", "<<", "<&", "<>", "<"] {
        if let Some(t) = body.strip_prefix(op) {
            let output = op.starts_with('>') || op == "<>";
            if op.starts_with("<<")
                || (op.ends_with('&')
                    && (t.is_empty() || t.chars().all(|c| c.is_ascii_digit() || c == '-')))
            {
                return (output, None);
            }
            return (output, (!t.is_empty()).then_some(t));
        }
    }
    (false, None)
}

pub(crate) fn null_dev(t: &str) -> bool {
    let l = t.to_lowercase();
    matches!(
        l.as_str(),
        "/dev/null"
            | "/dev/stdout"
            | "/dev/stderr"
            | "/dev/stdin"
            | "/dev/tty"
            | "/dev/zero"
            | "/dev/random"
            | "/dev/urandom"
            | "nul"
            | "nul:"
            | "$null"
    ) || l.starts_with("/dev/fd/")
}

/// Устройство, кроме пустых (`/dev/sda`).
fn dev_path(t: &str) -> bool {
    t.starts_with("/dev/") && !null_dev(t)
}

/// Сокет bash `/dev/tcp/…`, `/dev/udp/…` (m2).
fn dev_net(t: &str) -> bool {
    t.starts_with("/dev/tcp/") || t.starts_with("/dev/udp/")
}

/// Цель записи, известная только при исполнении (`$X`, `~`, глоб, `%X%`).
fn dynamic_path(t: &str) -> bool {
    t.contains(['$', '`', '*', '?']) || percent_var(t) || t.starts_with('~')
}

/// Первое и второе позиционное слово (подкоманды) с пропуском опций; `vals` — опции со
/// значением.
fn subcommands<'a>(args: &'a [String], vals: &[&str]) -> (&'a str, &'a str) {
    let mut found = Vec::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if found.len() == 2 {
            break;
        }
        if a.len() > 1 && (a.starts_with('-') || (found.is_empty() && a.starts_with('+'))) {
            i += if vals.contains(&a.as_str()) { 2 } else { 1 };
            continue;
        }
        found.push(a.as_str());
        i += 1;
    }
    (
        found.first().copied().unwrap_or(""),
        found.get(1).copied().unwrap_or(""),
    )
}

#[rustfmt::skip]
fn value_opts(name: &str) -> &'static [&'static str] {
    match name {
        "kubectl" => &["-n", "--namespace", "--context", "--kubeconfig", "--cluster", "--user",
            "-s", "--server", "--token"],
        "helm" => &["-n", "--namespace", "--kube-context", "--kubeconfig"],
        "docker" | "podman" => &["-H", "--host", "-c", "--context", "--config", "-l",
            "--log-level"],
        "npm" | "pnpm" | "yarn" => &["--prefix", "-w", "--workspace", "-C", "--dir", "--filter",
            "--cwd"],
        "gh" => &["-R", "--repo"],
        "cargo" => &["-Z", "--config", "-C"],
        "pulumi" => &["-s", "--stack", "-C", "--cwd"],
        _ => &[],
    }
}

fn classify(
    c: &Cmd,
    lang: Lang,
    outside: &dyn Fn(&str) -> bool,
    recurse: bool,
) -> Option<&'static str> {
    let a = c.args.as_slice();
    let pw = lang == Lang::Pwsh;
    let dry = a
        .iter()
        .any(|x| x.starts_with("--dry-run") && x != "--dry-run=none");
    let (s1, s2) = subcommands(a, value_opts(&c.name));
    let hit = |cond: bool, l: &'static str| cond.then_some(l);
    let ps_family = matches!(c.name.as_str(), "remove-item" | "rm")
        || (pw && matches!(c.name.as_str(), "ri" | "del" | "erase" | "rd" | "rmdir"));
    if ps_family && pw && a.iter().any(|x| x.starts_with('@') && !x.starts_with("@(")) {
        // Сплаттинг `@p`: параметры известны только при исполнении.
        return Some(label::UNPARSED);
    }
    match c.name.as_str() {
        "rm" => hit(rm_flags(a) || (pw && ps_remove(a)), label::DELETE),
        "remove-item" => hit(ps_remove(a), label::DELETE),
        "ri" if pw => hit(ps_remove(a), label::DELETE),
        "del" | "erase" | "rd" | "rmdir" => hit(slash_s(a) || (pw && ps_remove(a)), label::DELETE),
        "shred" | "wipefs" | "srm" | "format-volume" | "clear-disk" | "remove-partition"
        | "clear-recyclebin" | "rimraf" => Some(label::DELETE),
        n if n.starts_with("mkfs") => Some(label::DELETE),
        "robocopy" => hit(
            a.iter()
                .any(|x| x.eq_ignore_ascii_case("/mir") || x.eq_ignore_ascii_case("/purge")),
            label::DELETE,
        ),
        "rsync" => hit(a.iter().any(|x| x.starts_with("--delete")), label::DELETE),
        "mv" | "move" | "move-item" | "mi" => hit(
            a.iter()
                .rfind(|x| !x.starts_with('-'))
                .is_some_and(|t| null_dev(t)),
            label::DELETE,
        ),
        "dd" => hit(
            a.iter()
                .filter_map(|x| x.strip_prefix("of="))
                .any(|t| write_out(t, outside)),
            label::OVERWRITE,
        ),
        "tee" | "tee-object" | "out-file" | "set-content" | "add-content" | "ac" => hit(
            write_targets(a).any(|t| write_out(t, outside)),
            label::OVERWRITE,
        ),
        "%" | "foreach-object" => {
            a.iter()
                .find(|x| !x.starts_with(['-', '{']))
                .and_then(|m| match method_expr(m) {
                    Some(Expr::Label(l)) => Some(l),
                    _ => None,
                })
        }
        "find" => find_label(a, lang, outside, recurse),
        "git" => git_label(a),
        "cargo" => hit(s1 == "publish" && !dry, label::PUBLISH),
        "npm" | "pnpm" => hit(
            matches!(s1, "publish" | "unpublish") && !dry,
            label::PUBLISH,
        ),
        "yarn" => hit(
            (s1 == "publish" || (s1 == "npm" && s2 == "publish")) && !dry,
            label::PUBLISH,
        ),
        "docker" | "podman" => match (s1, s2) {
            ("push", _) | ("image", "push") => Some(label::PUBLISH),
            ("volume", "rm" | "prune") | ("system", "prune") => Some(label::DELETE),
            _ => None,
        },
        "twine" => hit(s1 == "upload", label::PUBLISH),
        "gh" => match (s1, s2) {
            ("release", "create" | "upload") => Some(label::PUBLISH),
            ("repo", "delete") => Some(label::DELETE),
            _ => None,
        },
        "gem" | "nuget" => hit(s1 == "push", label::PUBLISH),
        "poetry" | "uv" => hit(s1 == "publish" && !dry, label::PUBLISH),
        "dotnet" => hit(s1 == "nuget" && s2 == "push", label::PUBLISH),
        "mvn" => hit(a.iter().any(|x| x == "deploy"), label::PUBLISH),
        "terraform" | "tofu" => hit(matches!(s1, "apply" | "destroy"), label::INFRA),
        "kubectl" => hit(matches!(s1, "delete" | "apply") && !dry, label::INFRA),
        "helm" => hit(matches!(s1, "uninstall" | "delete"), label::INFRA),
        "pulumi" => hit(matches!(s1, "destroy" | "up"), label::INFRA),
        "psql" | "mysql" | "mariadb" | "sqlite3" | "sqlcmd" | "duckdb" | "clickhouse-client" => {
            hit(sql_drop(a), label::SQL_DROP)
        }
        _ => None,
    }
}

/// Файлы записи `tee`, `Out-File`, `Set-Content`, `Add-Content`: позиционные слова и
/// значения `-FilePath`, `-Path`, `-LiteralPath`.
fn write_targets(a: &[String]) -> impl Iterator<Item = &str> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(x) = a.get(i) {
        let l = x.to_lowercase();
        if matches!(l.as_str(), "-filepath" | "-path" | "-literalpath" | "-lp") {
            if let Some(v) = a.get(i + 1) {
                out.push(v.as_str());
            }
            i += 2;
            continue;
        }
        if !x.starts_with('-') {
            out.push(x.as_str());
        }
        i += 1;
    }
    out.into_iter()
}

/// `rm` с `-r`, `-R`, `-f`, `--recursive`, `--force` до `--`.
fn rm_flags(a: &[String]) -> bool {
    for x in a {
        if x == "--" {
            break;
        }
        if x.starts_with("--") {
            if x == "--recursive" || x == "--force" {
                return true;
            }
        } else if x.len() > 1 && x.starts_with('-') && x[1..].contains(['r', 'R', 'f']) {
            return true;
        }
    }
    false
}

/// Параметр PowerShell: имя в нижнем регистре и включён ли переключатель.
fn ps_switch(x: &str) -> Option<(String, bool)> {
    let p = x.strip_prefix('-')?;
    if !p.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let (name, val) = match p.split_once(':') {
        Some((n, v)) => (n.to_lowercase(), Some(v)),
        None => (p.to_lowercase(), None),
    };
    let on = val.is_none_or(|v| !(v.eq_ignore_ascii_case("$false") || v == "0"));
    Some((name, on))
}

/// `Remove-Item` с `-Recurse` (префикс от 1 буквы) или `-Force` (от 2), кроме `:$false`
/// и `-WhatIf` (m11).
fn ps_remove(a: &[String]) -> bool {
    let params: Vec<(String, bool)> = a.iter().filter_map(|x| ps_switch(x)).collect();
    let whatif = params
        .iter()
        .any(|(n, on)| *on && n.len() >= 2 && "whatif".starts_with(n.as_str()));
    !whatif
        && params.iter().any(|(n, on)| {
            *on && ("recurse".starts_with(n.as_str())
                || (n.len() >= 2 && "force".starts_with(n.as_str())))
        })
}

/// Ключ `/s` cmd (`del /s`, `rd /s /q`, `/s/q`).
fn slash_s(a: &[String]) -> bool {
    a.iter()
        .any(|x| x.starts_with('/') && x.split('/').any(|p| p.eq_ignore_ascii_case("s")))
}

fn find_label(
    a: &[String],
    lang: Lang,
    outside: &dyn Fn(&str) -> bool,
    recurse: bool,
) -> Option<&'static str> {
    if a.iter().any(|x| x == "-delete") {
        return Some(label::DELETE);
    }
    let mut i = 0;
    while i < a.len() {
        if matches!(a[i].as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
            let end = a[i + 1..]
                .iter()
                .position(|x| matches!(x.as_str(), ";" | "+" | "\\;"))
                .map_or(a.len(), |p| i + 1 + p);
            match analyze(&a[i + 1..end], lang) {
                // `find -exec find …` не разбирается: цепочка ограничена только длиной.
                Some(c) if c.name == "find" => return Some(label::UNPARSED),
                Some(c) => {
                    if let Some(l) = check_cmd(&c, lang, outside, recurse) {
                        return Some(l);
                    }
                }
                None => {}
            }
            i = end;
        }
        i += 1;
    }
    None
}

/// Ключи git, исполняющие команды или меняющие поведение git (`git -c`, `git config`).
fn git_exec_key(kv: &str) -> bool {
    #[rustfmt::skip]
    const SUFFIXES: &[&str] = &[
        "pager", "sshcommand", "fsmonitor", "hookspath", "editor", "command", "external",
        "helper", "textconv", "program", "askpass", "gitproxy", "clean", "smudge", "process",
        "hook", "cmd", "driver",
    ];
    let key = kv.split('=').next().unwrap_or("").to_lowercase();
    ["alias.", "include.", "includeif.", "filter."]
        .iter()
        .any(|p| key.starts_with(p))
        || SUFFIXES.iter().any(|s| key.ends_with(s))
}

/// Индекс подкоманды `git` после глобальных опций и признак опасного `-c`.
pub(crate) fn git_split(a: &[String]) -> (usize, bool) {
    let mut i = 0;
    let mut exec = false;
    while let Some(x) = a.get(i) {
        match x.as_str() {
            "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix" => i += 2,
            "-c" | "--config-env" => {
                exec |= a.get(i + 1).is_some_and(|kv| git_exec_key(kv));
                i += 2;
            }
            s if s.starts_with("--config-env=") => {
                exec |= git_exec_key(&s["--config-env=".len()..]);
                i += 1;
            }
            s if s.starts_with("--exec-path=") => {
                exec = true;
                i += 1;
            }
            s if s.starts_with('-') => i += 1,
            _ => break,
        }
    }
    (i, exec)
}

/// Ключ записи `git config [set] [опции] ключ значение` (I5); чтение и `--unset` — `None`.
pub(crate) fn git_config_key(rest: &[String]) -> Option<&str> {
    const VALS: &[&str] = &["-f", "--file", "--blob", "--type", "--default", "--comment"];
    let mut pos = Vec::new();
    let mut write_flag = false;
    let mut i = 0;
    while let Some(x) = rest.get(i) {
        let s = x.as_str();
        if VALS.contains(&s) {
            i += 2;
            continue;
        }
        if matches!(s, "--add" | "--replace-all") {
            write_flag = true;
        }
        if s.starts_with("--get") || s.starts_with("--unset") || matches!(s, "-l" | "--list") {
            return None;
        }
        if !s.starts_with('-') {
            pos.push(s);
        }
        i += 1;
    }
    if pos.first() == Some(&"set") {
        pos.remove(0);
        write_flag = true;
    }
    if matches!(
        pos.first(),
        Some(&("get" | "list" | "unset" | "edit" | "rename-section"))
    ) {
        return None;
    }
    (write_flag || pos.len() >= 2)
        .then(|| pos.first().copied())
        .flatten()
}

fn git_label(a: &[String]) -> Option<&'static str> {
    let (i, exec) = git_split(a);
    if exec {
        return Some(label::UNPARSED);
    }
    let sub = a.get(i)?.as_str();
    let rest = &a[i + 1..];
    let has = |f: &str| rest.iter().any(|x| x == f);
    let short = |c: char| {
        rest.iter().any(|x| {
            x.len() > 1 && x.starts_with('-') && !x.starts_with("--") && x[1..].contains(c)
        })
    };
    let all_paths = rest.iter().any(|x| {
        matches!(
            x.as_str(),
            "." | "./" | ".\\" | ":/" | ":/." | "*" | ":(top)"
        )
    });
    let after_dashdash = rest
        .iter()
        .position(|x| x == "--")
        .is_some_and(|p| p + 1 < rest.len());
    let reset = match sub {
        "config" => {
            return git_config_key(rest)
                .is_some_and(git_exec_key)
                .then_some(label::GIT_CONFIG_EXEC);
        }
        "rm" => return (short('f') || has("--force")).then_some(label::DELETE),
        "reset" => has("--hard"),
        "checkout" => short('f') || has("--force") || all_paths || after_dashdash,
        "switch" => short('f') || has("--force") || has("--discard-changes"),
        "restore" => {
            let staged_only = (has("--staged") || short('S')) && !(has("--worktree") || short('W'));
            !staged_only && all_paths
        }
        "clean" => (short('f') || has("--force")) && !(short('n') || has("--dry-run")),
        "stash" => matches!(rest.first().map(String::as_str), Some("drop" | "clear")),
        "branch" => {
            short('D') || ((short('d') || has("--delete")) && (short('f') || has("--force")))
        }
        "reflog" => matches!(rest.first().map(String::as_str), Some("expire" | "delete")),
        "update-ref" => has("-d") || has("--delete"),
        "filter-branch" | "filter-repo" => true,
        "push" => {
            return rest
                .iter()
                .any(|x| {
                    let s = x.as_str();
                    s == "--force"
                        || s.starts_with("--force-with-lease")
                        || matches!(s, "--mirror" | "--delete" | "--prune")
                        || (s.len() > 1
                            && s.starts_with('-')
                            && !s.starts_with("--")
                            && s[1..].contains(['f', 'd']))
                        || (!s.starts_with('-')
                            && (s.starts_with('+') || (s.starts_with(':') && s.len() > 1)))
                })
                .then_some(label::GIT_FORCE_PUSH);
        }
        _ => false,
    };
    reset.then_some(label::GIT_RESET)
}

/// `DROP TABLE|DATABASE|SCHEMA` или `TRUNCATE` в аргументах клиента СУБД.
fn sql_drop(a: &[String]) -> bool {
    let text = a.join(" ").to_uppercase();
    let toks: Vec<&str> = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .collect();
    toks.contains(&"TRUNCATE")
        || toks
            .windows(2)
            .any(|w| w[0] == "DROP" && matches!(w[1], "TABLE" | "DATABASE" | "SCHEMA"))
}

// --- Сеть (D77, D82) -----------------------------------------------------------------

/// Подкоманда git после глобальных опций и следующее слово.
fn git_subs(a: &[String]) -> (&str, &str) {
    let (i, _) = git_split(a);
    let get = |k: usize| a.get(k).map_or("", String::as_str);
    (get(i), get(i + 1))
}

/// Тип `New-Object` — сетевой клиент.
fn net_object(a: &[String]) -> bool {
    a.iter().any(|x| {
        let l = x.to_lowercase();
        ["webclient", "httpclient", "sockets", "webrequest"]
            .iter()
            .any(|t| l.contains(t))
    })
}

/// Сокет `/dev/tcp`, `/dev/udp` в словах или перенаправлениях.
fn uses_dev_net(seg: &Segment) -> bool {
    seg.argv
        .iter()
        .any(|a| dev_net(a) || a.contains("/dev/tcp/"))
        || seg
            .redirects
            .iter()
            .any(|r| parse_redirect(r).1.is_some_and(dev_net))
}

/// Сегмент обращается к сети.
pub(crate) fn net_cmd(seg: &Segment, lang: Lang) -> bool {
    if uses_dev_net(seg) {
        return true;
    }
    let Some(c) = analyze(&seg.argv, lang) else {
        return false;
    };
    if lang == Lang::Pwsh && !c.call && pwsh_expression(&c.raw0) {
        return matches!(pwsh_expr(&c.raw0), Expr::Network);
    }
    let a = c.args.as_slice();
    match c.name.as_str() {
        "curl" | "wget" | "wget2" | "ssh" | "scp" | "sftp" | "ftp" | "tftp" | "telnet" | "nc"
        | "ncat" | "netcat" | "socat" | "invoke-webrequest" | "iwr" | "invoke-restmethod"
        | "irm" | "start-bitstransfer" | "aria2c" | "http" | "https" | "xh" | "gh" | "nslookup"
        | "dig" | "bitsadmin" => true,
        "certutil" => a.iter().any(|x| x.eq_ignore_ascii_case("-urlcache")),
        "new-object" => net_object(a),
        "rsync" => a.iter().any(|x| !x.starts_with('-') && remote_spec(x)),
        "git" => matches!(
            git_subs(a),
            ("clone" | "fetch" | "pull" | "push" | "ls-remote", _)
                | ("submodule" | "remote", "update")
        ),
        _ => false,
    }
}

/// Сегмент читает данные из сети: вывод недоверенный (D82; решение контроллера —
/// `git push` сетевой, но не читающий).
pub(crate) fn reads_cmd(seg: &Segment, lang: Lang) -> bool {
    if uses_dev_net(seg) {
        return true;
    }
    let Some(c) = analyze(&seg.argv, lang) else {
        return false;
    };
    if lang == Lang::Pwsh && !c.call && pwsh_expression(&c.raw0) {
        return matches!(pwsh_expr(&c.raw0), Expr::Network);
    }
    let a = c.args.as_slice();
    let (s1, s2) = subcommands(a, value_opts(&c.name));
    match c.name.as_str() {
        "curl" | "wget" | "wget2" | "invoke-webrequest" | "iwr" | "invoke-restmethod" | "irm"
        | "start-bitstransfer" | "aria2c" | "http" | "https" | "xh" | "bitsadmin" => true,
        "certutil" => a.iter().any(|x| x.eq_ignore_ascii_case("-urlcache")),
        "new-object" => net_object(a),
        "gh" => matches!((s1, s2), ("api", _) | ("pr" | "issue", "view")),
        "git" => matches!(
            git_subs(a),
            ("clone" | "fetch" | "pull" | "ls-remote", _) | ("submodule" | "remote", "update")
        ),
        _ => false,
    }
}

/// `host:path`, `rsync://…`, `host::module`.
fn remote_spec(x: &str) -> bool {
    x.contains("://")
        || x.contains("::")
        || x.split_once(':')
            .is_some_and(|(h, _)| h.len() > 1 && !h.contains(['/', '\\']))
}
