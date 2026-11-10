//! Слова-пути наружу (D77).

use crate::classify::{git_config_key, git_split, null_dev, parse_redirect};
use crate::command::{analyze, pwsh_expression};
use crate::{Lang, Segment};

/// Похоже на путь наружу: абсолютный путь, `~`, UNC, компонент `..`.
pub(crate) fn path_like(w: &str) -> bool {
    if w.is_empty() || w.contains("://") || null_dev(w) {
        return false;
    }
    let b = w.as_bytes();
    w.starts_with(['/', '\\', '~'])
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'\\' | b'/'))
        || w.split(['/', '\\']).any(|c| c == "..")
}

/// Разбор аргументов для путей.
#[derive(Default)]
struct WordSpec {
    /// Первое позиционное слово — шаблон или программа, если нет `pattern_flags`.
    skip_first: bool,
    /// Опции со значением, которое не путь.
    skip_vals: &'static [&'static str],
    /// Опции со значением-файлом.
    file_vals: &'static [&'static str],
    /// Опции, после которых шаблон уже задан.
    pattern_flags: &'static [&'static str],
    /// Ключи `/x` Windows — не пути.
    win_switches: bool,
    /// Опции без учёта регистра (PowerShell).
    ci: bool,
}

#[rustfmt::skip]
const GREP_SKIP: &[&str] = &[
    "-e", "--regexp", "-g", "--glob", "--iglob", "-t", "--type", "-T", "--type-not", "-m", "-A",
    "-B", "-C",
];

#[rustfmt::skip]
const WIN_NATIVE: &[&str] = &[
    "cmd", "del", "erase", "rd", "rmdir", "md", "dir", "copy", "xcopy", "robocopy", "move", "ren",
    "rename", "type", "findstr", "attrib", "start", "taskkill", "tasklist", "sc", "reg", "icacls",
    "where", "more", "sort", "msbuild", "cl", "link", "nmake", "shutdown", "wmic", "net",
    "schtasks", "fc", "comp", "tree", "chcp", "mklink",
];

fn spec_for(name: &str) -> WordSpec {
    match name {
        "grep" | "egrep" | "fgrep" | "zgrep" | "rg" | "ag" | "ack" => WordSpec {
            skip_first: true,
            skip_vals: GREP_SKIP,
            file_vals: &["-f", "--file"],
            pattern_flags: &["-e", "--regexp", "-f", "--file"],
            ..WordSpec::default()
        },
        "sed" | "gsed" => WordSpec {
            skip_first: true,
            skip_vals: &["-e", "--expression"],
            file_vals: &["-f", "--file"],
            pattern_flags: &["-e", "--expression", "-f", "--file"],
            ..WordSpec::default()
        },
        "awk" | "gawk" | "mawk" | "nawk" => WordSpec {
            skip_first: true,
            skip_vals: &["-v", "-F"],
            file_vals: &["-f"],
            pattern_flags: &["-f"],
            ..WordSpec::default()
        },
        "select-string" | "sls" => WordSpec {
            skip_first: true,
            skip_vals: &["-pattern"],
            file_vals: &["-path", "-literalpath"],
            pattern_flags: &["-pattern"],
            ci: true,
            ..WordSpec::default()
        },
        "perl" | "python" | "python3" | "py" | "node" | "ruby" | "deno" | "bun" | "php" => {
            WordSpec {
                skip_vals: &[
                    "-e", "-E", "-c", "--eval", "-p", "--print", "-m", "-pe", "-ne", "-lne", "-lpe",
                ],
                ..WordSpec::default()
            }
        }
        n if WIN_NATIVE.contains(&n) => WordSpec {
            win_switches: true,
            ..WordSpec::default()
        },
        _ => WordSpec::default(),
    }
}

pub(crate) fn words_of(seg: &Segment, lang: Lang, out: &mut Vec<String>) {
    for r in &seg.redirects {
        if let (_, Some(t)) = parse_redirect(r)
            && path_like(t)
        {
            out.push(t.to_string());
        }
    }
    let Some(c) = analyze(&seg.argv, lang) else {
        return;
    };
    if lang == Lang::Pwsh && !c.call && pwsh_expression(&c.raw0) {
        // Операнды выражения (`$x -replace '/', '\'`) — не пути.
        return;
    }
    let a = c.args.as_slice();
    // Ключи `/x` в PowerShell и cmd — у любой программы (m1).
    let win = matches!(lang, Lang::Pwsh | Lang::Cmd);
    match c.name.as_str() {
        "echo" | "printf" | "write-host" | "write-output" | "write" | "write-information"
        | "write-warning" | "write-error" | "write-verbose" | "write-debug" => {}
        "cd" | "chdir" | "pushd" | "set-location" | "sl" | "push-location" => {
            let pos: Vec<&String> = a.iter().filter(|x| !x.starts_with('-')).collect();
            if pos.is_empty() {
                if a.is_empty() {
                    out.push("~".to_string());
                }
            } else {
                out.extend(pos.into_iter().cloned());
            }
        }
        "git" => {
            let mut i = 0;
            while let Some(x) = a.get(i) {
                if !x.starts_with('-') {
                    break;
                }
                if x == "-C" {
                    if let Some(v) = a.get(i + 1) {
                        out.push(v.clone());
                    }
                    i += 1;
                }
                i += 1;
            }
            let (sub, _) = git_split(a);
            if a.get(sub).is_some_and(|s| s == "config") {
                let rest = &a[sub + 1..];
                if git_config_key(rest).is_some() {
                    // Запись конфига: `--global`/`--system` — файлы вне проекта (I5).
                    for (k, x) in rest.iter().enumerate() {
                        match x.as_str() {
                            "--global" => out.push("~/.gitconfig".to_string()),
                            "--system" => out.push("/etc/gitconfig".to_string()),
                            "-f" | "--file" => out.extend(rest.get(k + 1).cloned()),
                            _ => {}
                        }
                    }
                }
            }
            let sp = WordSpec {
                win_switches: win,
                ..WordSpec::default()
            };
            generic_words(a, &sp, out);
        }
        n => {
            let mut sp = spec_for(n);
            sp.win_switches |= win;
            generic_words(a, &sp, out);
        }
    }
}

fn generic_words(a: &[String], sp: &WordSpec, out: &mut Vec<String>) {
    let eq = |x: &str, f: &str| {
        if sp.ci {
            x.eq_ignore_ascii_case(f)
        } else {
            x == f
        }
    };
    let pattern_given = a.iter().any(|x| {
        sp.pattern_flags
            .iter()
            .any(|f| eq(x, f) || x.starts_with(&format!("{f}=")))
    });
    let mut skip_first = sp.skip_first && !pattern_given;
    let mut opts_done = false;
    let mut i = 0;
    while let Some(x) = a.get(i) {
        i += 1;
        if !opts_done && x == "--" {
            opts_done = true;
            continue;
        }
        if !opts_done && x.len() > 1 && x.starts_with('-') {
            if sp.skip_vals.iter().any(|f| eq(x, f)) {
                i += 1;
            } else if sp.file_vals.iter().any(|f| eq(x, f)) {
                if let Some(v) = a.get(i).filter(|v| path_like(v)) {
                    out.push(v.clone());
                }
                i += 1;
            } else if let Some((_, v)) = x.split_once('=').or_else(|| x.split_once(':'))
                && path_like(v)
            {
                out.push(v.to_string());
            }
            continue;
        }
        if skip_first {
            skip_first = false;
            continue;
        }
        if sp.win_switches && x.len() > 1 && x.starts_with('/') && !x[1..].contains(['/', '\\']) {
            continue;
        }
        if path_like(x) {
            out.push(x.clone());
        } else if let Some((k, v)) = x.split_once('=')
            && k.chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-'))
            && path_like(v)
        {
            // `of=/x`, но не `trx;LogFileName=/r.trx` (m1).
            out.push(v.to_string());
        }
    }
}
