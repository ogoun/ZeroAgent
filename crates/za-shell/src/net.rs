//! Хосты сетевых команд для правил по домену (R1-C2 D104).
//!
//! Хост отдаётся сырым — как в тексте команды, без порта и userinfo, с регистром и
//! последней точкой: крейт без зависимостей (D99), нормализация — одна функция
//! `za_core::net::normalize_host` у потребителей (D124).
//!
//! **Строгое подмножество.** Хосты ищутся только у сегментов, которые сетевыми считает
//! [`crate::is_network`]. Хост берётся из слова-URL со схемой, из адреса `ssh`, `sftp`,
//! из scp-формы `[user@]host:path` у `scp`, `rsync`, из слова-репозитория подкоманды git
//! (refspec не разбирается). Опции всех утилит разбирает одна функция [`walk`] по таблице
//! утилиты ([`Spec`]): опция не точно из таблицы — незнакомая буква или имя, сокращение
//! имени, значение у флага — даёт `unknown`. Слово, которое нельзя однозначно отнести к
//! хосту, — `unknown`, а не хост: хост не найден; имя удалённого репозитория git или адрес
//! git `<транспорт>::…`; у команд загрузки (`curl`, `wget`, `iwr`…) — позиционное слово без
//! схемы; адрес не URL (`nc`, `socat`, `telnet`…: хосты из URL только добавляются); хост
//! записан не буквально (`$`, `%`, `{…}`, текст после `]`); сокет `/dev/tcp`, `/dev/udp` в
//! слове или перенаправлении; сегмент с подстановкой или вложенной интерпретацией
//! (`has_subst`, в том числе `ssh host команда`) или неразобран; ключ подмены адреса —
//! `ssh -J`, `-o ProxyJump|ProxyCommand|HostName|Canonical*|Include` (и в связке `-vo`),
//! `-F`, `sftp|scp -S|-D`, `curl -x`, `--proxy`, `--connect-to`, `--resolve`, `-K`,
//! `wget -e|-i|--config`, `rsync -e`, `git -c` с прокси, `sshCommand` или `insteadOf`,
//! переменные `*_proxy`, `GIT_SSH*`, `POSIXLY_CORRECT`. Без правил по домену ни хосты, ни
//! `unknown` ни на что не влияют (D105).

use crate::classify::{git_split, net_cmd, uses_dev_net};
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

/// Таблица опций утилиты для [`walk`] — одна модель разбора для всех утилит (повторное
/// ревью задачи 3). Таблица не обязана быть полной: опция вне неё даёт `unknown` (вопрос),
/// но не хост. Ключ подмены адреса (`bad_*`) берёт значение. Без букв (`flags` и `vals`
/// пусты) слово с `-` — имя целиком: PowerShell, git (`-q`; связка `-fu` и `-j4` —
/// `unknown`).
struct Spec {
    /// Опции, значение которых — адрес запроса (`curl --url`, `iwr -Uri`).
    urls: &'static [&'static str],
    /// Буквы без значения и со значением: связка `-sLo файл`, значение в слове `-ofile`.
    flags: &'static str,
    vals: &'static str,
    flags_long: &'static [&'static str],
    vals_long: &'static [&'static str],
    bad_short: &'static str,
    bad_long: &'static [&'static str],
    /// Имена без учёта регистра (PowerShell; в таблице — нижний регистр).
    ci: bool,
    /// Опции только до первого позиционного слова (getopt OpenSSH у `scp`).
    posix: bool,
}

/// Опции неизвестны (`http`, `xh`, `aria2c`): любая опция — `unknown`.
#[rustfmt::skip]
const NONE: Spec = Spec { urls: &[], flags: "", vals: "", flags_long: &[], vals_long: &[],
    bad_short: "", bad_long: &[], ci: false, posix: false };

/// curl 8.x: длинные имена — только точные (сокращений curl не принимает).
#[rustfmt::skip]
const CURL: Spec = Spec {
    urls: &["--url"],
    flags: "#012346aBfgGiIjJklLMnNOpqRsSvVZ",
    vals: "AbcCdDeEFHmoPQrtTuUwXyYz",
    flags_long: &["--silent", "--show-error", "--location", "--location-trusted", "--fail",
        "--fail-with-body", "--fail-early", "--insecure", "--verbose", "--include",
        "--show-headers", "--head", "--get", "--globoff", "--compressed", "--remote-name",
        "--remote-name-all", "--remote-header-name", "--remote-time", "--create-dirs",
        "--no-buffer", "--no-progress-meter", "--progress-bar", "--http1.0", "--http1.1",
        "--http2", "--http3", "--ipv4", "--ipv6", "--tlsv1.2", "--tlsv1.3", "--ssl-reqd",
        "--no-keepalive", "--raw", "--parallel", "--netrc", "--netrc-optional", "--append",
        "--list-only", "--path-as-is", "--retry-all-errors", "--retry-connrefused", "--basic",
        "--digest", "--anyauth", "--no-clobber", "--remove-on-error", "--version"],
    vals_long: &["--data", "--data-raw", "--data-binary", "--data-urlencode", "--data-ascii",
        "--json", "--url-query", "--header", "--output", "--output-dir", "--request", "--user",
        "--user-agent", "--referer", "--cookie", "--cookie-jar", "--form", "--form-string",
        "--upload-file", "--write-out", "--max-time", "--connect-timeout", "--retry",
        "--retry-delay", "--retry-max-time", "--cacert", "--capath", "--cert", "--cert-type",
        "--key", "--key-type", "--pass", "--range", "--continue-at", "--limit-rate",
        "--dump-header", "--max-filesize", "--oauth2-bearer", "--noproxy", "--trace",
        "--trace-ascii", "--stderr", "--speed-time", "--speed-limit", "--time-cond"],
    bad_short: "xK",
    bad_long: &["--proxy", "--preproxy", "--connect-to", "--resolve", "--config", "--doh-url",
        "--unix-socket", "--abstract-unix-socket", "--dns-servers"],
    ..NONE
};

/// wget 1.x (getopt_long принимает сокращения — у нас они `unknown`); `-n` берёт буквы
/// (`-nv`, `-np`); `-H` (обход чужих хостов) нет в таблице.
#[rustfmt::skip]
const WGET: Spec = Spec {
    flags: "46bcdEFhkKLmNpqrSvVx",
    vals: "aABDIlnoOPQRtTUwX",
    flags_long: &["--quiet", "--verbose", "--no-verbose", "--debug", "--continue",
        "--timestamping", "--no-clobber", "--no-check-certificate", "--server-response",
        "--spider", "--recursive", "--no-parent", "--mirror", "--page-requisites",
        "--convert-links", "--adjust-extension", "--content-disposition", "--show-progress",
        "--no-proxy", "--no-config", "--inet4-only", "--inet6-only", "--force-directories",
        "--no-directories", "--no-host-directories", "--retry-connrefused", "--no-cache",
        "--no-cookies", "--keep-session-cookies", "--https-only", "--save-headers", "--version"],
    vals_long: &["--output-file", "--append-output", "--output-document", "--directory-prefix",
        "--tries", "--timeout", "--connect-timeout", "--read-timeout", "--wait", "--waitretry",
        "--user-agent", "--header", "--user", "--password", "--http-user", "--http-password",
        "--post-data", "--post-file", "--body-data", "--body-file", "--method", "--referer",
        "--level", "--accept", "--reject", "--domains", "--cut-dirs", "--max-redirect",
        "--limit-rate", "--quota", "--load-cookies", "--save-cookies", "--ca-certificate",
        "--progress"],
    bad_short: "ei",
    bad_long: &["--execute", "--input-file", "--config"],
    ..NONE
};

/// `Invoke-WebRequest`, `Invoke-RestMethod`, `Start-BitsTransfer` (PowerShell принимает
/// однозначный префикс имени — у нас он `unknown`).
#[rustfmt::skip]
const PWSH_WEB: Spec = Spec {
    urls: &["-uri", "-source"],
    flags_long: &["-usebasicparsing", "-usedefaultcredentials", "-disablekeepalive",
        "-passthru", "-skipcertificatecheck", "-skipheadervalidation", "-skiphttperrorcheck",
        "-noproxy", "-resume", "-asynchronous", "-verbose"],
    vals_long: &["-outfile", "-method", "-custommethod", "-headers", "-body", "-contenttype",
        "-useragent", "-timeoutsec", "-credential", "-authentication", "-token", "-infile",
        "-sessionvariable", "-websession", "-maximumredirection", "-maximumretrycount",
        "-retryintervalsec", "-certificate", "-certificatethumbprint", "-transferencoding",
        "-form", "-proxycredential", "-sslprotocol", "-httpversion", "-destination",
        "-displayname", "-description", "-priority", "-transfertype", "-erroraction", "-ea"],
    bad_long: &["-proxy"],
    ci: true,
    ..NONE
};

/// `ssh` (OpenSSH 9.x; со значением — как у вложенной интерпретации `ssh host команда`).
#[rustfmt::skip]
const SSH: Spec = Spec { flags: "1246afgknqstvxyACGKMNTVXY", vals: "bceilmopBDEILOPQRSwW",
    bad_short: "FJ", ..NONE };

/// `sftp` — свои опции (`-P port`, `-s subsystem`); `-S`, `-D` — программа вместо ssh.
#[rustfmt::skip]
const SFTP: Spec = Spec { flags: "1246AafhNpqrvC", vals: "bBcilosPRX", bad_short: "DFJS",
    ..NONE };

/// `scp`: как `sftp`; getopt OpenSSH не переставляет опции — после первого позиционного
/// слова все слова позиционные (`scp b:y -P a:x` копирует и на `a`).
#[rustfmt::skip]
const SCP: Spec = Spec { flags: "346ABCOpqRrsTv", vals: "ciloPX", bad_short: "DFJS",
    posix: true, ..NONE };

/// rsync 3.x (popt).
#[rustfmt::skip]
const RSYNC: Spec = Spec {
    flags: "0468aAbcCdDEFgHhiIJkKlLmnNoOpPqrRsStuUvVWxXyz",
    vals: "BfMT@",
    flags_long: &["--archive", "--verbose", "--quiet", "--recursive", "--compress",
        "--checksum", "--delete", "--delete-after", "--delete-before", "--delete-excluded",
        "--dry-run", "--progress", "--partial", "--human-readable", "--stats", "--update",
        "--links", "--copy-links", "--safe-links", "--hard-links", "--perms", "--times",
        "--group", "--owner", "--devices", "--specials", "--acls", "--xattrs", "--sparse",
        "--whole-file", "--inplace", "--append-verify", "--backup", "--relative",
        "--itemize-changes", "--mkpath", "--remove-source-files", "--ignore-existing",
        "--existing", "--size-only", "--one-file-system", "--prune-empty-dirs", "--numeric-ids",
        "--force", "--no-motd", "--ipv4", "--ipv6", "--cvs-exclude", "--ignore-times",
        "--dirs", "--protect-args", "--secluded-args", "--list-only", "--version"],
    vals_long: &["--exclude", "--include", "--filter", "--exclude-from", "--include-from",
        "--files-from", "--chown", "--chmod", "--backup-dir", "--suffix", "--link-dest",
        "--compare-dest", "--copy-dest", "--timeout", "--contimeout", "--bwlimit",
        "--max-size", "--min-size", "--max-delete", "--info", "--out-format", "--log-file",
        "--partial-dir", "--temp-dir", "--compress-level", "--block-size", "--password-file",
        "--port"],
    bad_short: "e",
    bad_long: &["--rsh", "--rsync-path"],
    ..NONE
};

/// git `clone`, `fetch`, `pull`, `push`, `ls-remote`, `remote add`: флаги без значения и
/// опции со значением (`--depth 1`, `--depth=1`). Не входят буквы с разным смыслом у
/// подкоманд (`-u`: у `clone` — upload-pack, у `push` — set-upstream; `-s`, `-t`) и опции,
/// меняющие адрес или добавляющие хосты (`-c`, `--config`, `--upload-pack`, `--bundle-uri`,
/// `--recurse-submodules`, `--repo`). Ни один флаг не берёт значение ни у одной подкоманды.
#[rustfmt::skip]
const GIT: Spec = Spec {
    flags_long: &["-q", "--quiet", "-v", "--verbose", "--progress", "--no-progress", "-4", "-6",
        "--ipv4", "--ipv6", "--bare", "--mirror", "-n", "--no-checkout", "--dry-run",
        "--single-branch", "--no-single-branch", "--tags", "--no-tags", "-l", "--local",
        "--no-hardlinks", "--all", "-a", "--append", "-f", "--force", "-p", "--prune",
        "--atomic", "--follow-tags", "--set-upstream", "-d", "--delete", "--no-verify",
        "--unshallow", "--ff-only", "--ff", "--no-ff", "--rebase", "-r", "--no-rebase",
        "--heads", "--refs", "--force-with-lease", "--signed", "--exit-code"],
    vals_long: &["-b", "--branch", "-o", "--origin", "--push-option", "--server-option", "-j",
        "--jobs", "--depth", "--deepen", "--shallow-since", "--shallow-exclude", "--filter",
        "--reference", "--reference-if-able", "--separate-git-dir", "--template", "-m",
        "--sort", "-X", "--strategy", "--strategy-option"],
    ..NONE
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
    // Сокет bash (`> /dev/tcp/h/80`) соединяется с хостом мимо разбора аргументов.
    out.unknown |=
        seg.has_subst || uses_dev_net(seg) || seg.argv.iter().any(|w| redirecting_word(w));
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
            let r = git_repo(a, i).unwrap_or_default();
            // `<транспорт>::<адрес>` — адрес толкует помощник `git remote-<транспорт>`: хост
            // неизвестен (`found` не ставится), хост адреса-URL только добавляется.
            match r.split_once("::").filter(|(t, _)| scheme(t)) {
                Some((_, x)) => url_word(x).into_iter().for_each(|h| push(out, Some(h))),
                None => add(out, url_word(r).or_else(|| scp_host(r))),
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

/// Позиционные слова и признак `unknown` по таблице утилиты: ключ подмены адреса или опция
/// не точно из таблицы. Опции — в любом месте (GNU getopt, popt, git, PowerShell), у
/// `posix` — до первого позиционного слова.
fn walk<'a>(args: &'a [String], s: &Spec) -> (Vec<&'a str>, bool) {
    let (mut pos, mut bad, mut i) = (Vec::new(), false, 0);
    let letters = !(s.flags.is_empty() && s.vals.is_empty());
    while let Some(a) = args.get(i).map(String::as_str) {
        i += 1;
        if a == "--" {
            pos.extend(args[i..].iter().map(String::as_str));
            break;
        }
        if a.len() < 2 || !a.starts_with('-') || (s.posix && !pos.is_empty()) {
            pos.push(a);
            continue;
        }
        if a.starts_with("--") || !letters {
            let (n, v) = a
                .split_once(['=', ':'])
                .map_or((a, None), |(n, v)| (n, Some(v)));
            let n = if s.ci {
                n.to_lowercase()
            } else {
                n.to_string()
            };
            let has = |l: &[&str]| l.contains(&n.as_str());
            let takes = has(s.urls) || has(s.vals_long) || has(s.bad_long);
            bad |= has(s.bad_long) || !(takes || (has(s.flags_long) && v.is_none()));
            let v = match v {
                None if takes => args.get(i).map(String::as_str).inspect(|_| i += 1),
                v => v,
            };
            if has(s.urls) {
                pos.extend(v);
            }
            continue;
        }
        // Связка букв: буква со значением забирает остаток слова или следующее слово,
        // незнакомая буква — `unknown`, остаток слова не разбирается.
        for (k, ch) in a[1..].char_indices() {
            let takes = s.vals.contains(ch) || s.bad_short.contains(ch);
            bad |= s.bad_short.contains(ch) || !(takes || s.flags.contains(ch));
            if !s.flags.contains(ch) {
                i += usize::from(takes && k + ch.len_utf8() == a.len() - 1);
                break;
            }
        }
    }
    (pos, bad)
}

/// Слово меняет адрес или способ соединения: опция с `proxy`/`socks` в имени,
/// присваивание переменной прокси или команды ssh у git (`https_proxy=…`, `GIT_SSH=…`) или
/// `POSIXLY_CORRECT` (wget, rsync перестают переставлять опции: URL после опции — адрес).
fn redirecting_word(w: &str) -> bool {
    let l = w.to_lowercase();
    if l.starts_with('-') {
        let name = l
            .split(['=', ':'])
            .next()
            .unwrap_or("")
            .trim_start_matches('-');
        // Отказ от прокси (`wget --no-proxy`, `curl --noproxy`, `iwr -NoProxy`) адрес не меняет.
        return !matches!(name, "no-proxy" | "noproxy")
            && (name.contains("proxy") || name.contains("socks"));
    }
    let Some((name, _)) = l.split_once('=') else {
        return false;
    };
    !name.contains("://")
        && (name.contains("proxy")
            || name.contains("sshcommand")
            || name.contains("ssh_command")
            || matches!(name, "git_ssh" | "posixly_correct"))
}

/// `-o` с ключом, меняющим адрес (`ProxyJump`, `ProxyCommand`, `HostName`, `Canonical*`,
/// `Include`), у `ssh`, `scp`, `sftp`, `rsync`, git — и в связке коротких опций (`-vo`,
/// `-qoKey=…`). Буква `o` внутри значения другой опции даёт лишний `unknown`, не пропуск.
fn ssh_opts(a: &[String]) -> bool {
    const KEYS: [&str; 5] = [
        "proxyjump",
        "proxycommand",
        "hostname",
        "canonical",
        "include",
    ];
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
        KEYS.iter().any(|p| v.starts_with(p))
    })
}

/// Слово-репозиторий подкоманды git `a[i]` (D104, C1 ревью задачи 3): первое позиционное
/// слово, у `remote add` — второе (после имени); refspec и каталог назначения не
/// разбираются. `None` — слова нет или опция не из таблицы [`GIT`] (до или после адреса: git
/// переставляет опции): её значение могло стать словом-репозиторием, а сама она — сменить
/// адрес.
fn git_repo(a: &[String], i: usize) -> Option<&str> {
    let rest = a.get(i + 1..)?;
    let (skip, rest) = match (a[i].as_str(), rest.first().map(String::as_str)) {
        ("remote", Some("add")) => (1, &rest[1..]),
        ("clone" | "fetch" | "pull" | "push" | "ls-remote", _) => (0, rest),
        _ => return None,
    };
    let (pos, bad) = walk(rest, &GIT);
    pos.get(skip).copied().filter(|_| !bad)
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

/// Слово целиком — URL со схемой: до `://` только схема.
fn url_word(w: &str) -> Option<&str> {
    w.split_once("://")
        .is_some_and(|(s, _)| scheme(s))
        .then(|| url_host(w))?
}

/// Схема URL и имя транспорта git: буква, затем буквы, цифры, `+`, `.`, `-`.
fn scheme(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphabetic()) && s.chars().all(scheme_char)
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

/// Адрес `ssh`, `sftp`: `[user@]host[:path]`; после `]` — только `:…` (как [`strip_port`]).
fn dest_host(d: &str) -> Option<&str> {
    let h = d.rsplit_once('@').map_or(d, |(_, h)| h);
    if h.starts_with('[') {
        return Some(strip_port(h));
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
