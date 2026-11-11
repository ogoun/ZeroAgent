//! Разбор команд оболочки для границы проекта и политики (R1-C1 D77, D80, D81, D99).
//!
//! Крейт чистый: без ввода-вывода, процессов и зависимостей. Продукт видит его как
//! `za_core::shell` (D99: общее для `za-tools` и политики `za-agent` — через `za-core`).
//!
//! [`segments`] режет команду на простые команды по разделителям диалекта (`;`, `&&`,
//! `||`, `|`, `|&`, `&`, перевод строки), снимает кавычки и экранирование, отмечает
//! подстановки, перенаправления и то, что однозначно не разбирается. Тела подстановок
//! (`$(…)`, `` `…` ``, `<(…)`, `>(…)`, PowerShell `$(…)`, `@(…)`, `(…)`, `{…}`) и строка
//! вложенной интерпретации (`bash -c`, `su -c`, `script -c`, `watch`, `eval`, `alias`,
//! `env -S`, `ssh host команда`, `pwsh -c`, `powershell -Command`, `iex`, `cmd /c`)
//! становятся отдельными сегментами после своего: составная команда проверяется по
//! каждой части (Ruling 7.16). Вложенная интерпретация разбирается на один уровень;
//! глубже и там, где строку нельзя узнать (`-EncodedCommand`, код из переменной,
//! `Start-Process`, подстановка процесса), сегмент неразобран.
//!
//! **Строгое подмножество (D81, ревью задачи 8, раунд 2).** Лексер не повторяет тонкости
//! оболочек: всё вне однозначного подмножества — продолжение строки, экранирующий символ
//! в начале токена, CR не в паре CRLF, управляющие и пробельные символы, кроме пробела,
//! табуляции и перевода строки, — помечает все сегменты неразобранными. Текст команды не
//! переписывается; особые кавычки PowerShell (U+2018–U+201E) учитываются только в этом
//! диалекте. Разбор PowerShell сверяется с его собственным парсером (`tests/pwsh_ast.rs`).
//!
//! **Контракт для вызывающего:** классификаторы вызываются на каждом сегменте из
//! [`segments`], внешний сегмент (с `has_subst`) обязателен. [`irreversible`],
//! [`is_network`], [`reads_network`] и [`path_words`] сами заходят в строку вложенной
//! интерпретации своего сегмента, но не в тела подстановок — те отдельные сегменты.
//!
//! Известные промахи (best effort до SEC-039, R1-D): алиасы и функции, заданные до
//! команды; скрипты (`./clean.sh`, `bash x.sh`, `pwsh -File`, `source x.sh`); переменные
//! окружения, меняющие поведение (`GIT_SSH_COMMAND`); код других языков, кроме явных
//! опасных вызовов в `python -c` и подобных; `docker exec`; массовое удаление без флагов
//! (`ls | xargs rm`, `gci | ri`) — его страхует чекпоинт D89; `$HOME`, `%USERPROFILE%`,
//! `$env:…` в путях не раскрываются.

mod classify;
mod command;
mod lex;
mod net;
mod words;

use std::collections::HashSet;

use classify::{base_label, check_cmd, net_cmd, reads_cmd};
use command::{Inner, analyze};
use lex::lex_code;
pub use net::{NetHosts, network_hosts};
use words::words_of;

/// Метки необратимых команд (D81). Те же строки — `za_protocol::irreversible`;
/// совпадение проверяет тест `za-core`.
pub mod label {
    /// Удаление файлов: `rm -r`/`-f`, `find -delete`, `Remove-Item -Recurse`.
    pub const DELETE: &str = "delete";
    /// Сброс рабочей копии git: `reset --hard`, `checkout .`, `clean -f`.
    pub const GIT_RESET: &str = "git_reset";
    /// Принудительный `git push`.
    pub const GIT_FORCE_PUSH: &str = "git_force_push";
    /// Публикация пакета или образа.
    pub const PUBLISH: &str = "publish";
    /// Изменение инфраструктуры: `terraform apply`, `kubectl delete`.
    pub const INFRA: &str = "infra";
    /// `DROP TABLE`, `TRUNCATE` в клиенте СУБД.
    pub const SQL_DROP: &str = "sql_drop";
    /// Запись наружу или в цель, известную только при исполнении: там нет чекпоинта.
    pub const OVERWRITE: &str = "overwrite";
    /// Команда не разобрана: неизвестное считается необратимым.
    pub const UNPARSED: &str = "unparsed";
    /// Интерпретатор исполняет код из stdin или конвейера; запуск процесса методом .NET.
    pub const EXEC_UNTRUSTED: &str = "exec_untrusted";
    /// `git config` с ключом, исполняющим код или меняющим поведение git.
    pub const GIT_CONFIG_EXEC: &str = "git_config_exec";
}

/// Диалект оболочки инструмента `bash` (D85): PowerShell на Windows, bash иначе.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Bash,
    PowerShell,
}

/// Простая команда между разделителями (D81).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Исходный текст сегмента без крайних пробелов.
    pub text: String,
    /// Слова после снятия кавычек и экранирования; переменные и глобы не раскрываются.
    /// Оператор вызова PowerShell `&` или `.` — отдельное первое слово; строковый
    /// литерал в начале сегмента PowerShell сохраняет кавычки (это выражение, не вызов).
    pub argv: Vec<String>,
    /// Подстановка команды, оператор вызова или вложенная интерпретация: правило
    /// `allow` к сегменту неприменимо (D80).
    pub has_subst: bool,
    /// Перенаправления: оператор вместе с целью без пробела (`>out.txt`, `2>&1`).
    pub redirects: Vec<String>,
    /// Сегмент однозначно не разобран: незакрытая кавычка или скобка, here-doc,
    /// here-string, `--%`, подстановка процесса, закодированная команда, код из
    /// переменной, вложенная интерпретация глубже одного уровня.
    pub unparsed: bool,
}

/// Режим лексера: диалекты инструмента и `cmd` для строки `cmd /c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Bash,
    Pwsh,
    Cmd,
}

impl From<Dialect> for Lang {
    fn from(d: Dialect) -> Self {
        match d {
            Dialect::Bash => Lang::Bash,
            Dialect::PowerShell => Lang::Pwsh,
        }
    }
}

/// Глубина вложенности тел подстановок; глубже — сегмент неразобран.
const MAX_NEST: u8 = 8;

/// Сегменты команды в порядке текста; тела подстановок и строка вложенной
/// интерпретации — следом за своим сегментом.
pub fn segments(cmd: &str, d: Dialect) -> Vec<Segment> {
    lex_code(cmd, d.into(), 0, 0)
}

/// Имя команды сегмента без обёрток (`sudo`, `env`, `xargs`, `timeout N`, `&`,
/// присваиваний…), пути, регистра и `.exe`. Слово-выражение (кавычки, скобки, `$`)
/// путём не режется. `None` — команды нет.
pub fn argv0(seg: &Segment) -> Option<String> {
    analyze(&seg.argv, Lang::Pwsh).map(|c| c.name)
}

/// Имя команды, как у [`argv0`], и её аргументы после имени (сжатие вывода D86 узнаёт
/// подкоманду: `cargo test`, `git diff`). `None` — команды нет.
pub fn command_args(seg: &Segment) -> Option<(String, Vec<String>)> {
    analyze(&seg.argv, Lang::Pwsh).map(|c| (c.name, c.args))
}

/// Слова сегмента, похожие на путь наружу (D77, best effort): абсолютные пути, `~`,
/// компонент `..`, цели перенаправлений, аргументы `cd`, `Set-Location`, `pushd`,
/// `git -C`, файлы `git config --global|--system|--file`. Слова с `~` вызывающий
/// считает внешними сам: `paths::classify` `~` не раскрывает. URL, шаблоны
/// `grep`/`sed`/`awk`, аргументы `echo`, ключи `/x` Windows и `/dev/null` не попадают.
pub fn path_words(seg: &Segment, d: Dialect) -> Vec<String> {
    let lang = Lang::from(d);
    let mut out = Vec::new();
    words_of(seg, lang, &mut out);
    if let Some(Inner::Code(code, l)) = analyze(&seg.argv, lang).and_then(|c| c.inner) {
        for s in lex_code(&code, l, 1, 0) {
            words_of(&s, l, &mut out);
        }
    }
    let mut seen = HashSet::new();
    out.retain(|w| seen.insert(w.clone()));
    out
}

/// Сегмент обращается к сети (D77): `curl`, `wget`, `ssh`, `scp`, `Invoke-WebRequest`,
/// `git clone|fetch|pull|push|ls-remote`, `gh`, `/dev/tcp`, `Net.WebClient` и подобные.
pub fn is_network(seg: &Segment, d: Dialect) -> bool {
    with_inner(seg, d.into(), &|s, l| net_cmd(s, l))
}

/// Сегмент читает данные из сети — вывод получает метку `tainted` (D82): `curl`, `wget`,
/// `iwr`, `irm`, `gh api|pr view|issue view`, `git fetch|pull|clone|ls-remote`. `git push`
/// сетевой, но не читающий.
pub fn reads_network(seg: &Segment, d: Dialect) -> bool {
    with_inner(seg, d.into(), &|s, l| reads_cmd(s, l))
}

/// Предикат на сегменте и на сегментах его строки вложенной интерпретации.
fn with_inner(seg: &Segment, lang: Lang, f: &dyn Fn(&Segment, Lang) -> bool) -> bool {
    if f(seg, lang) {
        return true;
    }
    match analyze(&seg.argv, lang).and_then(|c| c.inner) {
        Some(Inner::Code(code, l)) => lex_code(&code, l, 1, 0).iter().any(|s| f(s, l)),
        _ => false,
    }
}

/// Метка необратимости сегмента ([`label`]) или `None` (D81).
/// `outside` — лежит ли путь вне проекта (лексически, без ввода-вывода).
pub fn irreversible(
    seg: &Segment,
    d: Dialect,
    outside: &dyn Fn(&str) -> bool,
) -> Option<&'static str> {
    let lang = Lang::from(d);
    base_label(seg, outside)
        .or_else(|| analyze(&seg.argv, lang).and_then(|c| check_cmd(&c, lang, outside, true)))
}

/// Один сегмент без подстановок, конвейера и перенаправлений, кроме `2>&1` (D86).
pub fn is_simple(segs: &[Segment]) -> bool {
    match segs {
        [s] => !s.has_subst && !s.unparsed && s.redirects.iter().all(|r| r == "2>&1"),
        _ => false,
    }
}
