//! Хосты сетевых команд для правил по домену (R1-C2 D104).
//!
//! Хост отдаётся сырым — как в тексте команды, без порта и userinfo, с регистром и
//! последней точкой: крейт без зависимостей (D99), нормализация — одна функция
//! `za_core::net::normalize_host` у потребителей (D124).
//!
//! **Строгое подмножество.** Хосты ищутся только у сегментов, которые сетевыми считает
//! [`crate::is_network`]. Хост берётся из слова-URL со схемой, из адреса `ssh`, `sftp`,
//! из scp-формы `[user@]host:path` у `scp`, `rsync`, из слова-репозитория подкоманды git
//! (refspec не разбирается). Слово, которое нельзя однозначно отнести к хосту, — `unknown`,
//! а не хост: хост не найден; имя удалённого репозитория git или незнакомая опция git; у
//! команд загрузки (`curl`, `wget`, `iwr`…) — позиционное слово без схемы (адрес без схемы,
//! значение незнакомой опции); адрес не URL (`nc`, `socat`, `telnet`…: хосты из URL только
//! добавляются); хост записан не буквально (`$`, `%`, `{…}`, текст после `]`); сегмент с
//! подстановкой или вложенной интерпретацией (`has_subst`, в том числе `ssh host команда`)
//! или неразобран; ключ подмены адреса — `ssh -J`, `-o ProxyJump|ProxyCommand|HostName|
//! Canonical*|Include` (и в связке `-vo`), `-F`, `sftp|scp -S`, `curl -x`, `--proxy`,
//! `--connect-to`, `--resolve`, `-K`, `wget -e|-i`, `rsync -e`, `git -c` с прокси,
//! `sshCommand` или `insteadOf`, переменные `*_proxy`, `GIT_SSH*`. Без правил по домену ни
//! хосты, ни `unknown` ни на что не влияют (D105).

use crate::classify::{git_split, net_cmd};
use crate::command::{Inner, analyze};
use crate::lex::lex_code;
use crate::{Dialect, Lang, Segment};

/// Хосты сетевого сегмента (D104). Потребители собирают их через
/// `za_core::net::shell_hosts`: сырой хост, который `normalize_host` не принимает (`None`),
/// — хост неизвестен (`unknown`), а не пропуск.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetHosts {
    /// Сырые хосты без повторов, в порядке текста (не нормализованы).
    pub hosts: Vec<String>,
    /// Хост определить нельзя или его можно подменить ключом команды.
    pub unknown: bool,
}

/// Опции команды: со значением отдельным словом и ключи подмены адреса. Длинные имена —
/// в нижнем регистре (PowerShell без учёта регистра), короткие — буквы.
struct Spec {
    /// Опции, значение которых — адрес запроса (`curl --url`, `iwr -Uri`).
    urls: &'static [&'static str],
    vals_short: &'static str,
    vals_long: &'static [&'static str],
    bad_short: &'static str,
    bad_long: &'static [&'static str],
}

#[rustfmt::skip]
const CURL: Spec = Spec {
    urls: &["--url"],
    vals_short: "AbcCdDeEFHKmoPQrTuUwxXyYz",
    vals_long: &["--data", "--data-raw", "--data-binary", "--data-urlencode", "--json",
        "--header", "--output", "--output-dir", "--request", "--user", "--user-agent",
        "--referer", "--cookie", "--cookie-jar", "--form", "--upload-file", "--write-out",
        "--max-time", "--connect-timeout", "--retry", "--retry-delay", "--retry-max-time",
        "--cacert", "--cert", "--key", "--range", "--continue-at", "--limit-rate",
        "--dump-header", "--max-filesize", "--oauth2-bearer", "--proxy", "--preproxy", "--noproxy",
        "--connect-to", "--resolve", "--config", "--doh-url"],
    bad_short: "xK",
    bad_long: &["--connect-to", "--resolve", "--config", "--doh-url"],
};

#[rustfmt::skip]
const WGET: Spec = Spec {
    urls: &[],
    vals_short: "aABDeIilOoPQRtTUwX",
    vals_long: &["--output-file", "--append-output", "--output-document", "--directory-prefix",
        "--tries", "--timeout", "--wait", "--user-agent", "--header", "--user", "--password",
        "--post-data", "--level", "--accept", "--reject", "--domains", "--execute",
        "--input-file"],
    bad_short: "ei",
    bad_long: &["--execute", "--input-file"],
};

#[rustfmt::skip]
const PWSH_WEB: Spec = Spec {
    urls: &["-uri", "-source"],
    vals_short: "",
    vals_long: &["-outfile", "-method", "-headers", "-body", "-contenttype", "-useragent",
        "-timeoutsec", "-credential", "-infile", "-sessionvariable", "-websession",
        "-maximumredirection", "-maximumretrycount", "-retryintervalsec", "-certificate",
        "-certificatethumbprint", "-transferencoding", "-form", "-proxy", "-proxycredential",
        "-destination"],
    bad_short: "",
    bad_long: &[],
};

/// Опции `ssh` со значением (как у вложенной интерпретации `ssh host команда`).
#[rustfmt::skip]
const SSH: Spec = Spec {
    urls: &[],
    vals_short: "bcDEeFIiJLlmOopPQRSWwB",
    vals_long: &[],
    bad_short: "JF",
    bad_long: &[],
};

/// Опции `sftp` — свои (`-P port`, `-s subsystem`), не как у `ssh`; `-S` — программа
/// соединения.
const SFTP: Spec = Spec {
    urls: &[],
    vals_short: "BbcDFiJloPRSsX",
    vals_long: &[],
    bad_short: "JFS",
    bad_long: &[],
};

const SCP: Spec = Spec {
    urls: &[],
    vals_short: "cDFiJloPSX",
    vals_long: &[],
    bad_short: "JFS",
    bad_long: &[],
};

const RSYNC: Spec = Spec {
    urls: &[],
    vals_short: "eBfMT",
    vals_long: &[
        "--rsh",
        "--exclude",
        "--include",
        "--filter",
        "--rsync-path",
    ],
    bad_short: "e",
    bad_long: &["--rsh", "--rsync-path"],
};

/// Опции неизвестны (`http`, `xh`, `aria2c`): каждая — без значения, поэтому значение любой
/// опции становится позиционным словом без схемы и даёт `unknown` (безопасная сторона).
const NONE: Spec = Spec {
    urls: &[],
    vals_short: "",
    vals_long: &[],
    bad_short: "",
    bad_long: &[],
};

/// Хосты сегмента и сегментов его строки вложенной интерпретации (D104).
pub fn network_hosts(seg: &Segment, d: Dialect) -> NetHosts {
    let lang = Lang::from(d);
    let mut out = NetHosts::default();
    seg_hosts(seg, lang, &mut out);
    if !seg.unparsed
        && let Some(Inner::Code(code, l)) = analyze(&seg.argv, lang).and_then(|c| c.inner)
    {
        for s in lex_code(&code, l, 1, 0) {
            seg_hosts(&s, l, &mut out);
        }
    }
    out
}

fn push(out: &mut NetHosts, h: Option<&str>) {
    match h.filter(|h| literal_host(h)) {
        Some(h) if !out.hosts.iter().any(|x| x == h) => out.hosts.push(h.to_string()),
        Some(_) => {}
        None => out.unknown = true,
    }
}

/// Хосты одного сегмента без захода во вложенный код.
fn seg_hosts(seg: &Segment, lang: Lang, out: &mut NetHosts) {
    // Неразобранный сегмент — `Ask` и без правил по домену (метка `unparsed`).
    out.unknown |= seg.unparsed;
    if !net_cmd(seg, lang) {
        return;
    }
    out.unknown |= seg.has_subst || seg.argv.iter().any(|w| redirecting_word(w));
    let mut found = false;
    let mut add = |out: &mut NetHosts, h: Option<&str>| {
        found = true;
        push(out, h);
    };
    let c = analyze(&seg.argv, lang);
    let (name, a) = c
        .as_ref()
        .map_or(("", &[][..]), |c| (c.name.as_str(), &c.args[..]));
    match name {
        "curl" | "wget" | "wget2" | "invoke-webrequest" | "iwr" | "invoke-restmethod" | "irm"
        | "start-bitstransfer" | "aria2c" | "http" | "https" | "xh" => {
            let spec = match name {
                "curl" => &CURL,
                "wget" | "wget2" => &WGET,
                "invoke-webrequest" | "iwr" | "invoke-restmethod" | "irm"
                | "start-bitstransfer" => &PWSH_WEB,
                _ => &NONE,
            };
            let (pos, bad) = walk(a, spec);
            out.unknown |= bad;
            // Каждое позиционное слово — URL со схемой, иначе адрес не определён.
            for p in pos {
                match url_word(p) {
                    Some(h) => add(out, Some(h)),
                    None => out.unknown = true,
                }
            }
        }
        "ssh" | "sftp" => {
            let (pos, bad) = walk(a, if name == "ssh" { &SSH } else { &SFTP });
            out.unknown |= bad || ssh_opts(a);
            if let Some(d) = pos.first() {
                add(out, url_word(d).or_else(|| dest_host(d)));
            }
        }
        "scp" | "rsync" => {
            let (pos, bad) = walk(a, if name == "scp" { &SCP } else { &RSYNC });
            out.unknown |= bad || ssh_opts(a);
            for p in pos {
                if let Some(h) = url_word(p).or_else(|| scp_host(p)) {
                    add(out, Some(h));
                }
            }
        }
        // Адрес — только слово-репозиторий; имя удалённого репозитория — `unknown`.
        "git" => {
            let (i, _) = git_split(a);
            out.unknown |= git_opts(a.get(..i).unwrap_or(a)) || ssh_opts(a);
            match git_repo(a, i) {
                Some(r) => add(out, url_word(r).or_else(|| scp_host(r))),
                None => out.unknown = true,
            }
        }
        // Прочие сетевые команды и выражения: адрес не URL (`nc`, `socat`…) или не
        // разбирается — хосты из URL в любом слове только добавляются, хост неизвестен.
        _ => {
            for w in &seg.argv {
                if let Some(h) = url_host(w) {
                    push(out, Some(h));
                }
            }
        }
    }
    if !found {
        out.unknown = true;
    }
}

/// Позиционные слова и признак ключа подмены адреса.
fn walk<'a>(args: &'a [String], spec: &Spec) -> (Vec<&'a str>, bool) {
    let mut pos = Vec::new();
    let mut bad = false;
    let mut i = 0;
    while let Some(a) = args.get(i) {
        i += 1;
        if a == "--" {
            pos.extend(args[i..].iter().map(String::as_str));
            break;
        }
        if a.len() < 2 || !a.starts_with('-') {
            pos.push(a.as_str());
            continue;
        }
        let (name, val) = match a.split_once(['=', ':']) {
            Some((n, v)) => (n.to_lowercase(), Some(v)),
            None => (a.to_lowercase(), None),
        };
        if spec.urls.contains(&name.as_str()) {
            match val {
                Some(v) => pos.push(v),
                None => pos.extend(args.get(i).map(String::as_str)),
            }
            i += usize::from(val.is_none());
            continue;
        }
        if spec.vals_long.contains(&name.as_str()) || spec.bad_long.contains(&name.as_str()) {
            bad |= spec.bad_long.contains(&name.as_str());
            i += usize::from(val.is_none());
            continue;
        }
        if a.starts_with("--") || spec.vals_short.is_empty() {
            continue;
        }
        // Связка коротких опций: буква со значением забирает остаток слова или следующее.
        let body = &a[1..];
        for (k, ch) in body.char_indices() {
            bad |= spec.bad_short.contains(ch);
            if spec.vals_short.contains(ch) {
                if k + ch.len_utf8() == body.len() {
                    i += 1;
                }
                break;
            }
        }
    }
    (pos, bad)
}

/// Слово меняет адрес или способ соединения: опция с `proxy`/`socks` в имени,
/// присваивание переменной прокси или команды ssh у git (`https_proxy=…`, `GIT_SSH=…`).
fn redirecting_word(w: &str) -> bool {
    let l = w.to_lowercase();
    if l.starts_with('-') {
        let name = l.split(['=', ':']).next().unwrap_or("");
        // Отказ от прокси (`wget --no-proxy`, `curl --noproxy`) адрес не меняет.
        return !matches!(name, "--no-proxy" | "--noproxy")
            && (name.contains("proxy") || name.contains("socks"));
    }
    let Some((name, _)) = l.split_once('=') else {
        return false;
    };
    !name.contains("://")
        && (name.contains("proxy")
            || name.contains("sshcommand")
            || name.contains("ssh_command")
            || name == "git_ssh")
}

/// `-o` с ключом, меняющим адрес (`ProxyJump`, `ProxyCommand`, `HostName`, `Canonical*`,
/// `Include`), у `ssh`, `scp`, `sftp`, `rsync`, git — и в связке коротких опций (`-vo`,
/// `-qoKey=…`). Буква `o` внутри значения другой опции даёт лишний `unknown`, не пропуск.
fn ssh_opts(a: &[String]) -> bool {
    a.iter().enumerate().any(|(i, x)| {
        let Some(k) = x
            .strip_prefix('-')
            .filter(|b| !b.starts_with('-'))
            .and_then(|b| b.find('o'))
        else {
            return false;
        };
        let v = match &x[k + 2..] {
            "" => a.get(i + 1).map_or("", String::as_str),
            v => v,
        };
        let v = v.trim_start().to_lowercase();
        [
            "proxyjump",
            "proxycommand",
            "hostname",
            "canonical",
            "include",
        ]
        .iter()
        .any(|p| v.starts_with(p))
    })
}

/// Флаги git без значения и опции со значением (`--depth 1`, `--depth=1`) у `clone`,
/// `fetch`, `pull`, `push`, `ls-remote`, `remote add`. Не входят буквы с разным смыслом у
/// подкоманд (`-u`, `-s`, `-t`) и опции, меняющие адрес или добавляющие хосты (`-c`,
/// `--config`, `--upload-pack`, `--bundle-uri`, `--recurse-submodules`, `--repo`).
#[rustfmt::skip]
const GIT_FLAGS: &[&str] = &["-q", "--quiet", "-v", "--verbose", "--progress", "--no-progress",
    "-4", "-6", "--ipv4", "--ipv6", "--bare", "--mirror", "-n", "--no-checkout", "--dry-run",
    "--single-branch", "--no-single-branch", "--tags", "--no-tags", "-l", "--local",
    "--no-hardlinks", "--all", "-a", "--append", "-f", "--force", "-p", "--prune", "--atomic",
    "--follow-tags", "--set-upstream", "-d", "--delete", "--no-verify", "--unshallow",
    "--ff-only", "--ff", "--no-ff", "--rebase", "-r", "--no-rebase", "--heads", "--refs",
    "--force-with-lease", "--signed", "--exit-code"];
#[rustfmt::skip]
const GIT_VALS: &[&str] = &["-b", "--branch", "-o", "--origin", "--push-option",
    "--server-option", "-j", "--jobs", "--depth", "--deepen", "--shallow-since",
    "--shallow-exclude", "--filter", "--reference", "--reference-if-able", "--separate-git-dir",
    "--template", "-m", "--sort", "-X", "--strategy", "--strategy-option"];

/// Слово-репозиторий подкоманды git `a[i]` (D104, C1 ревью задачи 3): первое позиционное
/// слово, у `remote add` — второе (после имени); refspec и каталог назначения не
/// разбираются. `None` — слова нет или есть опция вне [`GIT_FLAGS`] и [`GIT_VALS`] (до
/// или после адреса: git переставляет опции): её значение могло стать словом-репозиторием,
/// а сама она — сменить адрес.
fn git_repo(a: &[String], i: usize) -> Option<&str> {
    let rest = a.get(i + 1..)?;
    let (skip, rest) = match (a[i].as_str(), rest.first().map(String::as_str)) {
        ("remote", Some("add")) => (1, &rest[1..]),
        ("clone" | "fetch" | "pull" | "push" | "ls-remote", _) => (0, rest),
        _ => return None,
    };
    let (mut pos, mut opts) = (Vec::new(), true);
    let mut it = rest.iter().map(String::as_str);
    while let Some(x) = it.next() {
        if opts && x == "--" {
            opts = false;
        } else if opts && x.len() > 1 && x.starts_with('-') {
            let n = x.split_once('=').map_or(x, |(n, _)| n);
            if GIT_VALS.contains(&n) && n == x {
                it.next();
            } else if !GIT_VALS.contains(&n) && !GIT_FLAGS.contains(&n) {
                return None;
            }
        } else {
            pos.push(x);
        }
    }
    pos.get(skip).copied()
}

/// Глобальные опции git до подкоманды: `-c` и `--config-env` с прокси, `sshCommand`,
/// `insteadOf`.
fn git_opts(a: &[String]) -> bool {
    let bad = |kv: &str| {
        let k = kv.split('=').next().unwrap_or("").to_lowercase();
        k.contains("proxy") || k.contains("sshcommand") || k.contains("insteadof")
    };
    a.iter().enumerate().any(|(i, x)| match x.as_str() {
        "-c" | "--config-env" => a.get(i + 1).is_some_and(|kv| bad(kv)),
        s => s.strip_prefix("--config-env=").is_some_and(bad),
    })
}

/// Хост URL со схемой в слове (первое вхождение `://`): после userinfo до последнего
/// `@`, без порта после последнего `:` вне `[…]`. `None` — URL нет.
fn url_host(w: &str) -> Option<&str> {
    let at = w.find("://")?;
    let scheme_start = w[..at]
        .char_indices()
        .rfind(|&(_, c)| !scheme_char(c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let scheme = &w[scheme_start..at];
    if !scheme.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let rest = &w[at + 3..];
    let auth = &rest[..rest.find(['/', '?', '#', '\\']).unwrap_or(rest.len())];
    let hostport = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
    Some(strip_port(hostport))
}

/// Слово целиком — URL со схемой: до `://` только символы схемы.
fn url_word(w: &str) -> Option<&str> {
    w.split_once("://")
        .is_some_and(|(s, _)| s.chars().all(scheme_char))
        .then(|| url_host(w))?
}

fn scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')
}

/// Хост без порта: `[…]` целиком, иначе до последнего `:`. После `]` — только `:порт`,
/// иначе пустая строка: хост не буквальный, `unknown`.
fn strip_port(hp: &str) -> &str {
    if hp.starts_with('[') {
        return match hp.find(']') {
            Some(e) if hp[e + 1..].is_empty() || hp[e + 1..].starts_with(':') => &hp[..=e],
            Some(_) => "",
            None => hp,
        };
    }
    hp.rsplit_once(':').map_or(hp, |(h, _)| h)
}

/// Адрес `ssh`, `sftp`: `[user@]host[:path]`.
fn dest_host(d: &str) -> Option<&str> {
    let h = d.rsplit_once('@').map_or(d, |(_, h)| h);
    if h.starts_with('[') {
        return h.find(']').map(|e| &h[..=e]);
    }
    Some(h.split(':').next().unwrap_or(h))
}

/// scp-форма `[user@]host:path` (хост — до первого `:` после `@`, `[…]` целиком,
/// `host::module` у rsync). Буква диска (`C:`) и путь с `/` до `:` — локальные.
fn scp_host(p: &str) -> Option<&str> {
    let colon = p.find(':')?;
    let start = p[..colon].rfind('@').map_or(0, |i| i + 1);
    let rest = &p[start..];
    let host = if rest.starts_with('[') {
        let e = rest.find(']')?;
        rest[e + 1..].starts_with(':').then(|| &rest[..=e])?
    } else {
        &p[start..colon]
    };
    let local = host.is_empty() || (start == 0 && host.len() < 2) || host.contains(['/', '\\']);
    (!local).then_some(host)
}

/// Хост записан буквально: имя из букв, цифр, `-`, `_`, `.` или IP-литерал `[…]`.
fn literal_host(h: &str) -> bool {
    if let Some(ip) = h.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        return !ip.is_empty()
            && ip
                .chars()
                .all(|c| c.is_ascii_hexdigit() || ":.".contains(c));
    }
    !h.is_empty() && h.chars().all(|c| c.is_alphanumeric() || "-_.".contains(c))
}
