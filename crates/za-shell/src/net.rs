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
//! git `<транспорт>::…` и `<схема>://…` вне схем самого git; у команд загрузки (`curl`,
//! `wget`, `iwr`…) — позиционное слово без схемы; адрес не URL (`nc`, `socat`, `telnet`…:
//! хосты из URL только добавляются); хост записан не буквально (`$`, `%`, `{…}`, текст
//! после `]`); сокет `/dev/tcp`, `/dev/udp` в слове или перенаправлении; сегмент с
//! подстановкой или вложенной интерпретацией (`has_subst`, в том числе `ssh host команда`)
//! или неразобран; ключ подмены адреса — `ssh -J|-S|-F`, `sftp|scp -S|-D`, `curl -x`,
//! `--proxy`, `--connect-to`, `--resolve`, `-K`, `wget -e|-i|--config`, `rsync -e`,
//! `iwr -Proxy|-WebSession`, переменные `*_proxy`, `GIT_SSH*`, `POSIXLY_CORRECT`. Белые
//! списки (третье ревью задачи 3): ключ `-o` у `ssh`, `scp`, `sftp` и ключ `git -c` вне
//! списка — `unknown`; глобальная опция git вне таблицы — подкоманда и хост неизвестны.
//! Присваивание переменной в любом сегменте команды — хост сетевой команды неизвестен
//! ([`command_hosts`], проход 3c). Без правил по домену ни хосты, ни `unknown` ни на что не
//! влияют (D105).

use crate::classify::{git_cfg, git_split, net_cmd, uses_dev_net};
use crate::command::{Inner, analyze, is_assignment, norm};
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
    /// Опции только до первого позиционного слова (getopt OpenSSH у `scp`, глобальные
    /// опции git до подкоманды).
    posix: bool,
    /// Белый список ключей `-o` (`ssh`, `scp`, `sftp`): ключ вне него — `unknown`.
    o_keys: &'static [&'static str],
}

/// Опции неизвестны (`http`, `xh`, `aria2c`): любая опция — `unknown`.
#[rustfmt::skip]
const NONE: Spec = Spec { urls: &[], flags: "", vals: "", flags_long: &[], vals_long: &[],
    bad_short: "", bad_long: &[], ci: false, posix: false, o_keys: &[] };

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
    // `-WebSession` — объект сеанса со свойством `Proxy` (m6 третьего ревью задачи 3).
    bad_long: &["-proxy", "-websession"],
    ci: true,
    ..NONE
};

/// Ключи `-o` у `ssh`, `scp`, `sftp`, которые не меняют адрес и способ соединения (белый
/// список, m2 третьего ревью задачи 3). Прочие — `ProxyJump`, `ProxyCommand`, `HostName`,
/// `Control*` (мастер-соединение идёт к своему хосту), `LocalCommand`, `Include`, `Match`,
/// незнакомые — `unknown`.
#[rustfmt::skip]
const SSH_KEYS: &[&str] = &["port", "user", "identityfile", "identitiesonly", "certificatefile",
    "stricthostkeychecking", "userknownhostsfile", "globalknownhostsfile", "checkhostip",
    "hashknownhosts", "updatehostkeys", "batchmode", "connecttimeout", "connectionattempts",
    "serveraliveinterval", "serveralivecountmax", "tcpkeepalive", "compression", "loglevel",
    "addressfamily", "passwordauthentication", "pubkeyauthentication",
    "kbdinteractiveauthentication", "preferredauthentications", "numberofpasswordprompts",
    "ciphers", "macs", "kexalgorithms", "hostkeyalgorithms", "pubkeyacceptedalgorithms",
    "requesttty", "forwardagent", "addkeystoagent", "visualhostkey", "escapechar"];

/// `ssh` (OpenSSH 9.x; со значением — как у вложенной интерпретации `ssh host команда`);
/// `-S` — сокет мастер-соединения: соединение идёт к хосту мастера (проход 3c).
#[rustfmt::skip]
const SSH: Spec = Spec { flags: "1246afgknqstvxyACGKMNTVXY", vals: "bceilmopBDEILOPQRwW",
    bad_short: "FJS", o_keys: SSH_KEYS, ..NONE };

/// `sftp` — свои опции (`-P port`, `-s subsystem`); `-S`, `-D` — программа вместо ssh.
#[rustfmt::skip]
const SFTP: Spec = Spec { flags: "1246AafhNpqrvC", vals: "bBcilosPRX", bad_short: "DFJS",
    o_keys: SSH_KEYS, ..NONE };

/// `scp`: как `sftp`; getopt OpenSSH не переставляет опции — после первого позиционного
/// слова все слова позиционные (`scp b:y -P a:x` копирует и на `a`).
#[rustfmt::skip]
const SCP: Spec = Spec { flags: "346ABCOpqRrsTv", vals: "ciloPX", bad_short: "DFJS",
    posix: true, o_keys: SSH_KEYS, ..NONE };

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

/// Глобальные опции git до подкоманды (`git.c`, `handle_options`; I2 третьего ревью задачи
/// 3). Опция вне таблицы могла взять значение — подкоманда неизвестна.
#[rustfmt::skip]
const GIT_GLOBAL: Spec = Spec {
    flags_long: &["-p", "--paginate", "-P", "--no-pager", "--bare", "--no-replace-objects",
        "--no-lazy-fetch", "--no-optional-locks", "--no-advice", "--literal-pathspecs",
        "--no-literal-pathspecs", "--glob-pathspecs", "--noglob-pathspecs", "--icase-pathspecs",
        "--exec-path", "--html-path", "--man-path", "--info-path", "-v", "--version", "-h",
        "--help"],
    vals_long: &["-C", "-c", "--git-dir", "--work-tree", "--namespace", "--super-prefix",
        "--config-env", "--list-cmds", "--attr-source", "--shallow-file"],
    posix: true,
    ..NONE
};

/// Ключи `git -c`, `--config-env`, которые не меняют адрес соединения (белый список,
/// третье ревью задачи 3); с точкой в конце — раздел целиком. Прочие (`http.*`, `url.*`,
/// `remote.*`, `core.sshCommand`…) — `unknown`.
#[rustfmt::skip]
const GIT_KEYS: &[&str] = &["user.name", "user.email", "core.autocrlf", "core.safecrlf",
    "core.quotepath", "core.longpaths", "core.filemode", "core.ignorecase", "core.eol",
    "init.defaultbranch", "pull.rebase", "pull.ff", "fetch.prune", "rebase.autostash",
    "merge.ff", "commit.gpgsign", "tag.gpgsign", "gc.auto", "maintenance.auto",
    "safe.directory", "protocol.version", "advice.", "color."];

/// Схемы URL, которые git обслуживает сам или помощниками своей поставки (m1 третьего ревью
/// задачи 3); у прочих адрес толкует сторонний `git remote-<схема>`.
const GIT_SCHEMES: [&str; 9] = [
    "file", "git", "ssh", "git+ssh", "ssh+git", "http", "https", "ftp", "ftps",
];

/// Индекс подкоманды git после глобальных опций ([`GIT_GLOBAL`]) и признак опции не из
/// таблицы.
pub(crate) fn git_global(a: &[String]) -> (usize, bool) {
    let (pos, bad) = walk(a, &GIT_GLOBAL);
    (a.len() - pos.len(), bad)
}

/// Хосты команды целиком (D104): хосты всех сегментов без повторов и признак `unknown`.
/// `segs` — сегменты [`crate::segments`] (с телами подстановок и вложенным кодом).
/// Присваивание переменной в любом сегменте (проход 3c) делает хост сетевой команды
/// неизвестным: переменная окружения меняет адрес (`https_proxy`, `POSIXLY_CORRECT`,
/// `GIT_SSH_COMMAND`, `$PSDefaultParameterValues`).
pub fn command_hosts(segs: &[Segment], d: Dialect) -> NetHosts {
    let mut out = NetHosts::default();
    for s in segs {
        let h = network_hosts(s, d);
        out.unknown |= h.unknown;
        for x in h.hosts {
            if !out.hosts.contains(&x) {
                out.hosts.push(x);
            }
        }
    }
    // Сетевой сегмент всегда даёт хост или `unknown`: хосты есть — сетевая команда есть.
    out.unknown |= !out.hosts.is_empty() && segs.iter().any(|s| assigns(s, Lang::from(d)));
    out
}

/// Сегмент присваивает или меняет переменную (Ruling прохода 3b, сомнение 2): `VAR=…`
/// отдельно, перед командой или после обёртки; `env`, `export`, `set`, `unset`, `read`,
/// `for X in`, `source`, `.`; PowerShell — выражение `$…` или `[…]` (`$env:X = …`,
/// `$x = …`, `[Environment]::…`), `Set-Variable`, диски `env:`, `variable:`.
fn assigns(seg: &Segment, lang: Lang) -> bool {
    #[rustfmt::skip]
    const NAMES: &[&str] = &["export", "declare", "typeset", "local", "readonly", "set", "unset",
        "setenv", "env", "read", "mapfile", "readarray", "getopts", "let", "source", "select",
        "set-variable", "sv", "new-variable", "nv", "clear-variable", "clv", "remove-variable",
        "rv"];
    let (w, pwsh) = (&seg.argv, lang == Lang::Pwsh);
    if w.first()
        .is_some_and(|x| x == "." || (pwsh && x.starts_with(['$', '['])))
        || (pwsh
            && w.iter()
                .map(|x| x.to_lowercase())
                .any(|l| l.starts_with("env:") || l.starts_with("variable:")))
    {
        return true;
    }
    let named = |x: &String| is_assignment(x) || NAMES.contains(&norm(x).as_str());
    match analyze(w, lang) {
        // Команды нет: одни присваивания (`X=1`) или обёртка без команды (`env`).
        None => w.iter().any(named),
        Some(c) => {
            let prefix = &w[..w.len().saturating_sub(c.args.len() + 1)];
            prefix.iter().any(|x| named(x) || x == "for")
                || NAMES.contains(&c.name.as_str())
                || (c.name == "printf" && c.args.first().is_some_and(|a| a.starts_with("-v")))
        }
    }
}

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
            out.unknown |= bad;
            if let Some(d) = pos.first() {
                add(out, url_word(d).or_else(|| dest_host(d)));
            }
        }
        // У `rsync` `-o` — флаг (owner): ключи ssh — только в `-e`, а он подмена адреса.
        "scp" | "rsync" => {
            let (pos, bad) = walk(a, if name == "scp" { &SCP } else { &RSYNC });
            out.unknown |= bad;
            for p in pos {
                if let Some(h) = url_word(p).or_else(|| scp_host(p)) {
                    add(out, Some(h));
                }
            }
        }
        // Адрес — только слово-репозиторий; имя удалённого репозитория — `unknown`.
        "git" => {
            let (i, _, bad) = git_split(a);
            out.unknown |= bad || git_opts(&a[..i]);
            let r = git_repo(a, i).unwrap_or_default();
            // Транспорт git — `[A-Za-z0-9][A-Za-z0-9+.-]*` (`transport_get`, `is_url` в
            // `url.c`): у `<транспорт>::<адрес>` и у схемы вне [`GIT_SCHEMES`] (`9x://…`)
            // адрес толкует помощник `git remote-<транспорт>` — хост неизвестен (`found` не
            // ставится), хост адреса-URL только добавляется.
            let transport = |t: &str| {
                t.starts_with(|c: char| c.is_ascii_alphanumeric()) && t.chars().all(scheme_char)
            };
            let helper = match (r.split_once("::"), r.split_once("://")) {
                (Some((t, x)), _) if transport(t) => Some(x),
                (_, Some((t, _)))
                    if transport(t) && !GIT_SCHEMES.contains(&t.to_ascii_lowercase().as_str()) =>
                {
                    Some(r)
                }
                _ => None,
            };
            match helper {
                Some(x) => url_word(x).into_iter().for_each(|h| push(out, Some(h))),
                // Слово с `://`, которое не URL, — не scp-форма: хост неизвестен.
                None => add(
                    out,
                    url_word(r).or_else(|| scp_host(r).filter(|_| !r.contains("://"))),
                ),
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

/// Позиционные слова и признак `unknown` по таблице утилиты: ключ подмены адреса, опция
/// не точно из таблицы или ключ `-o` вне [`Spec::o_keys`]. Опции — в любом месте (GNU
/// getopt, popt, git, PowerShell), у `posix` — до первого позиционного слова (после него и
/// `--` — позиционное слово).
fn walk<'a>(args: &'a [String], s: &Spec) -> (Vec<&'a str>, bool) {
    let (mut pos, mut bad, mut i) = (Vec::new(), false, 0);
    let letters = !(s.flags.is_empty() && s.vals.is_empty());
    while let Some(a) = args.get(i).map(String::as_str) {
        i += 1;
        let rest = s.posix && !pos.is_empty();
        if a == "--" && !rest {
            pos.extend(args[i..].iter().map(String::as_str));
            break;
        }
        if rest || a.len() < 2 || !a.starts_with('-') {
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
        // незнакомая буква — `unknown`, остаток слова не разбирается. Ключ `-o` (`Key=…`,
        // `Key …`) — из белого списка.
        for (k, ch) in a[1..].char_indices() {
            let takes = s.vals.contains(ch) || s.bad_short.contains(ch);
            bad |= s.bad_short.contains(ch) || !(takes || s.flags.contains(ch));
            if !s.flags.contains(ch) {
                let v = match &a[1 + k + ch.len_utf8()..] {
                    "" if takes => args.get(i).map(String::as_str).inspect(|_| i += 1),
                    v => Some(v),
                };
                if ch == 'o' && !s.o_keys.is_empty() {
                    let key = |v: &str| {
                        v.trim_start()
                            .split(['=', ' ', '\t'])
                            .next()
                            .map(str::to_lowercase)
                    };
                    bad |= !v
                        .and_then(key)
                        .is_some_and(|k| s.o_keys.contains(&k.as_str()));
                }
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

/// Глобальный `-c` или `--config-env` git с ключом вне белого списка [`GIT_KEYS`].
fn git_opts(head: &[String]) -> bool {
    git_cfg(head).any(|kv| {
        let k = kv.split('=').next().unwrap_or("").to_lowercase();
        !GIT_KEYS
            .iter()
            .any(|s| k == *s || (s.ends_with('.') && k.starts_with(s)))
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

/// Схема URL (RFC 3986): буква, затем буквы, цифры, `+`, `.`, `-`. Имя транспорта git шире:
/// первой может быть и цифра (ветка git в [`seg_hosts`]).
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
