//! Разбор команд оболочки для границы проекта и политики (R1-C1 D77, D80, D81, D99).
//!
//! Модуль чистый: без ввода-вывода и процессов. [`segments`] режет команду на простые
//! команды по разделителям диалекта (`;`, `&&`, `||`, `|`, `|&`, `&`, перевод строки),
//! снимает кавычки и экранирование, отмечает подстановки, перенаправления и то, что
//! однозначно не разбирается. Тела подстановок (`$(…)`, `` `…` ``, `<(…)`, `>(…)`,
//! PowerShell `$(…)`, `@(…)`, `(…)`, `{…}`) и строка вложенной интерпретации
//! (`bash -c`, `sh -c`, `eval`, `env -S`, `pwsh -c`, `powershell -Command`, `iex`,
//! `Invoke-Expression`, `cmd /c`, `Start-Process`) становятся отдельными сегментами
//! после своего: составная команда проверяется по каждой части (Ruling 7.16).
//! Вложенная интерпретация разбирается на один уровень; глубже и там, где строку нельзя
//! узнать (`-EncodedCommand`, код из переменной или stdin), сегмент неразобран.
//!
//! Классификаторы работают по разобранной команде (Ruling 7.17): [`irreversible`] —
//! метки `za_protocol::irreversible`, [`is_network`] — сетевые команды D77,
//! [`path_words`] — слова-пути наружу D77. Политика (D80) вызывает их на каждом
//! сегменте из [`segments`]; [`irreversible`], [`is_network`] и [`path_words`] сами
//! заходят в строку вложенной интерпретации, но не в тела подстановок — те и так
//! отдельные сегменты.
//!
//! Известные промахи (best effort до SEC-039, R1-D): алиасы и функции пользователя,
//! скрипты (`./clean.sh`, `bash x.sh`, `pwsh -File`, `source`), переменные окружения,
//! меняющие поведение (`GIT_SSH_COMMAND`), код интерпретаторов других языков
//! (`python -c`), удалённые команды (`ssh host 'rm -rf x'`, `docker exec`), `$HOME`,
//! `%USERPROFILE%` и `$env:…` в путях не раскрываются.

use std::collections::HashSet;

use za_protocol::irreversible as label;

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
    /// Оператор вызова PowerShell `&` — отдельное первое слово.
    pub argv: Vec<String>,
    /// Подстановка команды, оператор вызова `&` или вложенная интерпретация: правило
    /// `allow` к сегменту неприменимо (D80).
    pub has_subst: bool,
    /// Перенаправления: оператор вместе с целью без пробела (`>out.txt`, `2>&1`).
    pub redirects: Vec<String>,
    /// Сегмент однозначно не разобран: незакрытая кавычка или скобка, here-doc,
    /// here-string, `--%`, закодированная команда, код из переменной или stdin,
    /// вложенная интерпретация глубже одного уровня.
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
/// присваиваний…), пути, регистра и `.exe`. `None` — команды нет.
pub fn argv0(seg: &Segment) -> Option<String> {
    analyze(&seg.argv, Lang::Pwsh).map(|c| c.name)
}

/// Слова сегмента, похожие на путь наружу (D77, best effort): абсолютные пути, `~`,
/// компонент `..`, цели перенаправлений, аргументы `cd`, `Set-Location`, `pushd`,
/// `git -C`. Слова с `~` вызывающий считает внешними сам: `paths::classify` `~` не
/// раскрывает. URL, шаблоны `grep`/`sed`/`awk`, аргументы `echo` и `/dev/null` не
/// попадают.
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
/// `git clone|fetch|pull|push|ls-remote`, `gh` и подобные.
pub fn is_network(seg: &Segment, d: Dialect) -> bool {
    let lang = Lang::from(d);
    let Some(c) = analyze(&seg.argv, lang) else {
        return false;
    };
    if net_cmd(&c) {
        return true;
    }
    match &c.inner {
        Some(Inner::Code(code, l)) => lex_code(code, *l, 1, 0)
            .iter()
            .any(|s| analyze(&s.argv, *l).is_some_and(|c| net_cmd(&c))),
        _ => false,
    }
}

/// Метка необратимости сегмента (`za_protocol::irreversible`) или `None` (D81).
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

// --- Лексер ----------------------------------------------------------------------------

fn lex_code(code: &str, lang: Lang, interp: u8, nest: u8) -> Vec<Segment> {
    let chars: Vec<char> = code.chars().collect();
    let raws = Lexer {
        s: &chars,
        i: 0,
        lang,
        depth: nest,
    }
    .list();
    let mut out = Vec::new();
    for raw in raws {
        let text: String = chars[raw.start..raw.end.max(raw.start)].iter().collect();
        let mut seg = Segment {
            text: text.trim().to_string(),
            argv: raw.argv,
            has_subst: raw.has_subst,
            redirects: raw.redirects,
            unparsed: raw.unparsed,
        };
        let inner = if seg.unparsed {
            None
        } else {
            analyze(&seg.argv, lang).and_then(|c| c.inner)
        };
        let mut nested_code = None;
        match inner {
            Some(Inner::Code(c, l)) => {
                seg.has_subst = true;
                if interp == 0 {
                    nested_code = Some((c, l));
                } else {
                    seg.unparsed = true;
                }
            }
            Some(Inner::Opaque) => {
                seg.has_subst = true;
                seg.unparsed = true;
            }
            None => {}
        }
        if nest >= MAX_NEST && !raw.bodies.is_empty() {
            seg.unparsed = true;
        }
        out.push(seg);
        if nest < MAX_NEST {
            for b in &raw.bodies {
                out.extend(lex_code(b, lang, interp, nest + 1));
            }
        }
        if let Some((c, l)) = nested_code {
            out.extend(lex_code(&c, l, interp + 1, nest));
        }
    }
    out
}

/// Сегмент в процессе разбора.
struct Raw {
    start: usize,
    end: usize,
    argv: Vec<String>,
    redirects: Vec<String>,
    has_subst: bool,
    unparsed: bool,
    /// Тела подстановок для отдельного разбора.
    bodies: Vec<String>,
}

impl Raw {
    fn new(start: usize) -> Self {
        Self {
            start,
            end: start,
            argv: Vec::new(),
            redirects: Vec::new(),
            has_subst: false,
            unparsed: false,
            bodies: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.argv.is_empty() && self.redirects.is_empty() && !self.unparsed && !self.has_subst
    }
}

/// Конечный автомат по символам.
struct Lexer<'a> {
    s: &'a [char],
    i: usize,
    lang: Lang,
    /// Вложенность `${…}` и тел подстановок: глубже [`MAX_NEST`] — неразобрано.
    depth: u8,
}

impl Lexer<'_> {
    fn at(&self, k: usize) -> Option<char> {
        self.s.get(self.i + k).copied()
    }

    fn peek(&self) -> Option<char> {
        self.at(0)
    }

    fn rest_starts(&self, p: &str) -> bool {
        p.chars().enumerate().all(|(k, c)| self.at(k) == Some(c))
    }

    fn list(&mut self) -> Vec<Raw> {
        let mut out = Vec::new();
        let mut cur = Raw::new(0);
        loop {
            self.skip_blank();
            let Some(c) = self.peek() else { break };
            let start = self.i;
            if c == '#' && self.lang != Lang::Cmd {
                while self.peek().is_some_and(|c| c != '\n') {
                    self.i += 1;
                }
                continue;
            }
            if self.lang == Lang::Pwsh && self.rest_starts("<#") {
                self.block_comment(&mut cur);
                continue;
            }
            if let Some(n) = self.separator(c, &cur) {
                cur.end = start;
                out.push(std::mem::replace(&mut cur, Raw::new(start + n)));
                self.i += n;
                continue;
            }
            if self.lang == Lang::Pwsh
                && (c == '&' || (c == '.' && self.at(1) == Some(' ')))
                && cur.argv.is_empty()
            {
                // Оператор вызова или dot-source: что вызывается, решает выражение.
                cur.argv.push(c.to_string());
                cur.has_subst = true;
                self.i += 1;
                continue;
            }
            if self.lang == Lang::Bash && (c == '<' || c == '>') && self.at(1) == Some('(') {
                self.i += 1;
                match self.balanced('(', ')') {
                    Some(b) => {
                        cur.has_subst = true;
                        cur.argv.push(format!("{c}({b})"));
                        cur.bodies.push(b);
                    }
                    None => self.fail(&mut cur),
                }
                continue;
            }
            if self.redirect_start(c) {
                self.redirect(&mut cur);
                continue;
            }
            let before = self.i;
            let w = self.word(&mut cur);
            if self.i == before {
                cur.unparsed = true;
                self.i += 1;
                continue;
            }
            if self.lang == Lang::Bash && matches!(&self.s[before..self.i], ['{'] | ['}']) {
                // Группа `{ …; }`: скобки — границы, а не слова.
                cur.end = before;
                out.push(std::mem::replace(&mut cur, Raw::new(self.i)));
                continue;
            }
            cur.argv.push(w);
        }
        cur.end = self.s.len();
        out.push(cur);
        out.retain(|r| !r.is_empty());
        out
    }

    /// Неразобранный остаток: сегмент помечается, разбор заканчивается.
    fn fail(&mut self, cur: &mut Raw) {
        cur.unparsed = true;
        self.i = self.s.len();
    }

    fn skip_blank(&mut self) {
        while let Some(c) = self.peek() {
            let cont = match self.lang {
                Lang::Bash => '\\',
                Lang::Pwsh => '`',
                Lang::Cmd => '^',
            };
            if matches!(c, ' ' | '\t' | '\r') {
                self.i += 1;
            } else if c == cont && self.at(1) == Some('\n') {
                self.i += 2;
            } else if c == cont && self.at(1) == Some('\r') && self.at(2) == Some('\n') {
                self.i += 3;
            } else {
                break;
            }
        }
    }

    fn block_comment(&mut self, cur: &mut Raw) {
        self.i += 2;
        while self.i < self.s.len() {
            if self.rest_starts("#>") {
                self.i += 2;
                return;
            }
            self.i += 1;
        }
        cur.unparsed = true;
    }

    /// Длина разделителя в текущей позиции.
    fn separator(&self, c: char, cur: &Raw) -> Option<usize> {
        let next = self.at(1);
        match (self.lang, c) {
            (_, '\n') => Some(1),
            (Lang::Bash, ';') => Some(if self.rest_starts(";;&") {
                3
            } else if next == Some(';') || next == Some('&') {
                2
            } else {
                1
            }),
            (Lang::Pwsh, ';') => Some(1),
            (Lang::Bash, '&') if next == Some('>') => None,
            (Lang::Pwsh, '&') if next != Some('&') && cur.argv.is_empty() => None,
            (_, '&') => Some(if next == Some('&') { 2 } else { 1 }),
            (Lang::Bash, '|') if next == Some('&') => Some(2),
            (_, '|') => Some(if next == Some('|') { 2 } else { 1 }),
            (Lang::Bash, '(' | ')') | (Lang::Pwsh, ')' | '}') => Some(1),
            _ => None,
        }
    }

    fn redirect_start(&self, c: char) -> bool {
        let digits = self.s[self.i..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        let after = self.at(digits);
        match self.lang {
            Lang::Bash => {
                matches!(c, '<' | '>')
                    || (c == '&' && self.at(1) == Some('>'))
                    || (digits > 0 && matches!(after, Some('<' | '>')))
            }
            Lang::Pwsh => {
                matches!(c, '<' | '>')
                    || (c == '*' && self.at(1) == Some('>'))
                    || (digits > 0 && after == Some('>'))
            }
            Lang::Cmd => matches!(c, '<' | '>') || (digits > 0 && matches!(after, Some('<' | '>'))),
        }
    }

    fn redirect(&mut self, cur: &mut Raw) {
        let mut op = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == '*' || (c == '&' && op.is_empty()) {
                op.push(c);
                self.i += 1;
            } else {
                break;
            }
        }
        match self.peek() {
            Some('>') => {
                op.push('>');
                self.i += 1;
                match self.peek() {
                    Some('>') => {
                        op.push('>');
                        self.i += 1;
                    }
                    Some('|') if self.lang == Lang::Bash => {
                        op.push('|');
                        self.i += 1;
                    }
                    Some('&') => {
                        op.push('&');
                        self.i += 1;
                        if self.fd_dup(&mut op) {
                            cur.redirects.push(op);
                            return;
                        }
                    }
                    _ => {}
                }
            }
            Some('<') => {
                op.push('<');
                self.i += 1;
                if self.lang == Lang::Pwsh {
                    // `<` в PowerShell зарезервирован: команда не разбирается.
                    cur.unparsed = true;
                }
                match self.peek() {
                    Some('<') if self.lang == Lang::Bash => {
                        // here-doc `<<` и here-string `<<<`: тело не разбирается.
                        op.push('<');
                        cur.redirects.push(op);
                        self.fail(cur);
                        return;
                    }
                    Some('&') => {
                        op.push('&');
                        self.i += 1;
                        if self.fd_dup(&mut op) {
                            cur.redirects.push(op);
                            return;
                        }
                    }
                    Some('>') => {
                        op.push('>');
                        self.i += 1;
                    }
                    _ => {}
                }
            }
            _ => {
                cur.unparsed = true;
                return;
            }
        }
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.i += 1;
        }
        let before = self.i;
        let target = self.word(cur);
        if self.i == before || target.is_empty() {
            cur.unparsed = true;
            cur.redirects.push(op);
            return;
        }
        cur.redirects.push(op + &target);
    }

    /// Дублирование дескриптора (`2>&1`, `>&-`): номер дописывается к оператору.
    fn fd_dup(&mut self, op: &mut String) -> bool {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit() || c == '-') {
            self.i += 1;
        }
        let fd: String = self.s[start..self.i].iter().collect();
        let dup = !fd.is_empty() && self.peek().is_none_or(|c| !is_word_char(c));
        if dup {
            op.push_str(&fd);
        } else {
            self.i = start;
        }
        dup
    }

    fn word(&mut self, cur: &mut Raw) -> String {
        match self.lang {
            Lang::Bash => self.word_bash(cur),
            Lang::Pwsh => self.word_pwsh(cur),
            Lang::Cmd => self.word_cmd(cur),
        }
    }

    fn word_bash(&mut self, cur: &mut Raw) -> String {
        let mut w = String::new();
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' | '\r' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => break,
                '\\' => {
                    self.i += 1;
                    match self.peek() {
                        None => cur.unparsed = true,
                        Some('\n') => self.i += 1,
                        Some(x) => {
                            w.push(x);
                            self.i += 1;
                        }
                    }
                }
                '\'' => {
                    self.i += 1;
                    if !self.until_quote(&mut w, '\'') {
                        cur.unparsed = true;
                    }
                }
                '"' => {
                    self.i += 1;
                    self.dquote_bash(&mut w, cur);
                }
                '$' => self.dollar_bash(&mut w, cur, false),
                '`' => self.backtick(&mut w, cur),
                _ => {
                    w.push(c);
                    self.i += 1;
                }
            }
        }
        w
    }

    /// Литерал до закрывающей кавычки; `false` — кавычка не закрыта.
    fn until_quote(&mut self, w: &mut String, q: char) -> bool {
        while let Some(c) = self.peek() {
            self.i += 1;
            if c == q {
                return true;
            }
            w.push(c);
        }
        false
    }

    fn dquote_bash(&mut self, w: &mut String, cur: &mut Raw) {
        loop {
            match self.peek() {
                None => {
                    cur.unparsed = true;
                    return;
                }
                Some('"') => {
                    self.i += 1;
                    return;
                }
                Some('\\') => match self.at(1) {
                    Some('\n') => self.i += 2,
                    Some(x @ ('$' | '`' | '"' | '\\')) => {
                        w.push(x);
                        self.i += 2;
                    }
                    _ => {
                        w.push('\\');
                        self.i += 1;
                    }
                },
                Some('$') => self.dollar_bash(w, cur, true),
                Some('`') => self.backtick(w, cur),
                Some(x) => {
                    w.push(x);
                    self.i += 1;
                }
            }
        }
    }

    fn dollar_bash(&mut self, w: &mut String, cur: &mut Raw, in_dq: bool) {
        match self.at(1) {
            Some('(') => {
                self.i += 1;
                match self.balanced('(', ')') {
                    Some(b) => {
                        cur.has_subst = true;
                        w.push_str(&format!("$({b})"));
                        cur.bodies.push(b);
                    }
                    None => self.fail(cur),
                }
            }
            Some('{') => {
                self.i += 1;
                match self.balanced('{', '}') {
                    Some(_) if self.depth >= MAX_NEST => self.fail(cur),
                    Some(b) => {
                        w.push_str(&format!("${{{b}}}"));
                        // `${X:-$(cmd)}`: подстановки внутри параметра — свои тела.
                        let chars: Vec<char> = b.chars().collect();
                        let mut sub = Lexer {
                            s: &chars,
                            i: 0,
                            lang: Lang::Bash,
                            depth: self.depth + 1,
                        };
                        let mut tmp = Raw::new(0);
                        while sub.i < chars.len() {
                            let before = sub.i;
                            sub.word_bash(&mut tmp);
                            if sub.i == before {
                                sub.i += 1;
                            }
                        }
                        cur.has_subst |= tmp.has_subst;
                        cur.unparsed |= tmp.unparsed;
                        cur.bodies.extend(tmp.bodies);
                    }
                    None => self.fail(cur),
                }
            }
            Some('\'') if !in_dq => {
                // ANSI-C `$'…'`: `\'` не закрывает строку.
                self.i += 2;
                loop {
                    match self.peek() {
                        None => {
                            cur.unparsed = true;
                            return;
                        }
                        Some('\'') => {
                            self.i += 1;
                            return;
                        }
                        Some('\\') if self.at(1).is_some() => {
                            if let Some(x) = self.at(1) {
                                w.push(x);
                            }
                            self.i += 2;
                        }
                        Some(x) => {
                            w.push(x);
                            self.i += 1;
                        }
                    }
                }
            }
            Some('"') if !in_dq => self.i += 1,
            _ => {
                w.push('$');
                self.i += 1;
            }
        }
    }

    fn backtick(&mut self, w: &mut String, cur: &mut Raw) {
        self.i += 1;
        let mut body = String::new();
        loop {
            match self.peek() {
                None => {
                    cur.unparsed = true;
                    return;
                }
                Some('`') => {
                    self.i += 1;
                    break;
                }
                Some('\\') if matches!(self.at(1), Some('`' | '\\' | '$')) => {
                    if let Some(x) = self.at(1) {
                        body.push(x);
                    }
                    self.i += 2;
                }
                Some(x) => {
                    body.push(x);
                    self.i += 1;
                }
            }
        }
        cur.has_subst = true;
        w.push_str(&format!("`{body}`"));
        cur.bodies.push(body);
    }

    /// Тело в скобках от открывающей в текущей позиции до парной закрывающей с учётом
    /// кавычек и вложенности; `None` — скобка не закрыта.
    fn balanced(&mut self, open: char, close: char) -> Option<String> {
        self.i += 1;
        let start = self.i;
        let mut depth = 1usize;
        while let Some(c) = self.peek() {
            match c {
                '\\' if self.lang == Lang::Bash => self.i += 2,
                '`' if self.lang == Lang::Pwsh => self.i += 2,
                '`' if self.lang == Lang::Bash => self.skip_quoted('`')?,
                '\'' => self.skip_quoted('\'')?,
                '"' => self.skip_quoted('"')?,
                _ if c == open => {
                    depth += 1;
                    self.i += 1;
                }
                _ if c == close => {
                    depth -= 1;
                    if depth == 0 {
                        let body = self.s[start..self.i].iter().collect();
                        self.i += 1;
                        return Some(body);
                    }
                    self.i += 1;
                }
                _ => self.i += 1,
            }
        }
        None
    }

    /// Пропуск строки в кавычках `q` внутри тела скобок.
    fn skip_quoted(&mut self, q: char) -> Option<()> {
        self.i += 1;
        while let Some(c) = self.peek() {
            match c {
                '\\' if self.lang == Lang::Bash && q != '\'' => self.i += 2,
                '`' if self.lang == Lang::Pwsh && q == '"' => self.i += 2,
                _ if c == q => {
                    self.i += 1;
                    return Some(());
                }
                _ => self.i += 1,
            }
        }
        None
    }

    fn word_pwsh(&mut self, cur: &mut Raw) -> String {
        let mut w = String::new();
        if self.rest_starts("--%") || self.rest_starts("@'") || self.rest_starts("@\"") {
            // Стоп-разбор и here-string: текст уходит как есть.
            self.fail(cur);
            return w;
        }
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' | '\r' | '\n' | ';' | '|' | '&' | '<' | '>' | ')' | '}' => break,
                '`' => {
                    self.i += 1;
                    match self.peek() {
                        None => cur.unparsed = true,
                        Some('\n') => self.i += 1,
                        Some(x) => {
                            w.push(x);
                            self.i += 1;
                        }
                    }
                }
                '\'' => {
                    self.i += 1;
                    loop {
                        match self.peek() {
                            None => {
                                cur.unparsed = true;
                                break;
                            }
                            Some('\'') if self.at(1) == Some('\'') => {
                                w.push('\'');
                                self.i += 2;
                            }
                            Some('\'') => {
                                self.i += 1;
                                break;
                            }
                            Some(x) => {
                                w.push(x);
                                self.i += 1;
                            }
                        }
                    }
                }
                '"' => {
                    self.i += 1;
                    self.dquote_pwsh(&mut w, cur);
                }
                '$' | '@' if self.at(1) == Some('(') || (c == '@' && self.at(1) == Some('{')) => {
                    let open = self.at(1).unwrap_or('(');
                    self.i += 1;
                    self.pwsh_body(&mut w, cur, c, open);
                }
                '(' | '{' => self.pwsh_body(&mut w, cur, '\0', c),
                _ => {
                    w.push(c);
                    self.i += 1;
                }
            }
        }
        w
    }

    /// Подвыражение `$(…)`, `@(…)`, `@{…}`, группа `(…)` или блок `{…}`: тело —
    /// отдельный разбор, слово сохраняет исходный текст.
    fn pwsh_body(&mut self, w: &mut String, cur: &mut Raw, sigil: char, open: char) {
        let close = if open == '{' { '}' } else { ')' };
        match self.balanced(open, close) {
            Some(b) => {
                cur.has_subst = true;
                if sigil != '\0' {
                    w.push(sigil);
                }
                w.push(open);
                w.push_str(&b);
                w.push(close);
                cur.bodies.push(b);
            }
            None => self.fail(cur),
        }
    }

    fn dquote_pwsh(&mut self, w: &mut String, cur: &mut Raw) {
        loop {
            match self.peek() {
                None => {
                    cur.unparsed = true;
                    return;
                }
                Some('`') => {
                    if let Some(x) = self.at(1) {
                        w.push(x);
                    }
                    self.i += 2;
                }
                Some('"') if self.at(1) == Some('"') => {
                    w.push('"');
                    self.i += 2;
                }
                Some('"') => {
                    self.i += 1;
                    return;
                }
                Some('$') if self.at(1) == Some('(') => {
                    self.i += 1;
                    self.pwsh_body(w, cur, '$', '(');
                }
                Some(x) => {
                    w.push(x);
                    self.i += 1;
                }
            }
        }
    }

    fn word_cmd(&mut self, cur: &mut Raw) -> String {
        let mut w = String::new();
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' | '\r' | '\n' | '&' | '|' | '<' | '>' => break,
                '^' => {
                    self.i += 1;
                    match self.peek() {
                        None => cur.unparsed = true,
                        Some(x) => {
                            w.push(x);
                            self.i += 1;
                        }
                    }
                }
                '"' => {
                    self.i += 1;
                    if !self.until_quote(&mut w, '"') {
                        cur.unparsed = true;
                    }
                }
                '(' | ')' => {
                    // Блоки `if (…)`, `for … do (…)` не разбираются.
                    cur.unparsed = true;
                    w.push(c);
                    self.i += 1;
                }
                _ => {
                    w.push(c);
                    self.i += 1;
                }
            }
        }
        w
    }
}

fn is_word_char(c: char) -> bool {
    !matches!(
        c,
        ' ' | '\t' | '\r' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
    )
}

// --- Команда сегмента ----------------------------------------------------------------

/// Строка вложенной интерпретации.
enum Inner {
    Code(String, Lang),
    /// Код есть, но его нельзя узнать: закодирован, из переменной или stdin.
    Opaque,
}

/// Команда после снятия обёрток.
struct Cmd {
    /// Имя без пути, регистра и `.exe`.
    name: String,
    /// Слово команды как в `argv`.
    raw0: String,
    args: Vec<String>,
    /// Вызов через `&` или `.` PowerShell.
    call: bool,
    inner: Option<Inner>,
}

/// Служебные слова, после которых идёт команда.
const RESERVED: &[&str] = &[
    "then", "do", "else", "elif", "elseif", "if", "while", "until", "for", "foreach", "switch",
    "try", "catch", "finally", "function", "filter", "trap", "!", "{", "nohup", "builtin",
];

/// Обёртки, после опций которых идёт команда: `(имя, опции со значением)`.
#[rustfmt::skip]
const WRAPPERS: &[(&str, &[&str])] = &[
    ("sudo", &["-u", "-g", "-p", "-C", "-D", "-r", "-t", "-T", "-U", "-h", "-R"]),
    ("doas", &["-u", "-C"]),
    ("exec", &["-a"]),
    ("nice", &["-n", "--adjustment"]),
    ("ionice", &["-c", "-n", "-p", "-P", "-u"]),
    ("stdbuf", &["-i", "-o", "-e"]),
    ("xargs", &["-n", "-I", "-P", "-d", "-L", "-s", "-E", "-a", "--max-args", "--max-procs",
        "--delimiter", "--arg-file"]),
    ("wsl", &["-d", "--distribution", "-u", "--user", "--cd", "--shell-type"]),
];

fn analyze(argv: &[String], lang: Lang) -> Option<Cmd> {
    let mut w: Vec<String> = argv.to_vec();
    let mut i = 0;
    let mut call = false;
    loop {
        let x = w.get(i)?.clone();
        if i == 0 && (x == "&" || x == ".") {
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
        match n.as_str() {
            r if RESERVED.contains(&r) => i += 1,
            "time" => {
                i += 1;
                while w.get(i).is_some_and(|a| a == "-p") {
                    i += 1;
                }
            }
            r if WRAPPERS.iter().any(|(n, _)| *n == r) => {
                let vals = WRAPPERS
                    .iter()
                    .find(|(n, _)| *n == r)
                    .map_or(&[][..], |w| w.1);
                i = skip_opts(&w, i + 1, vals);
            }
            "command" => {
                if matches!(w.get(i + 1).map(String::as_str), Some("-v" | "-V")) {
                    return None;
                }
                i = skip_opts(&w, i + 1, &[]);
            }
            "timeout" => {
                i = skip_opts(&w, i + 1, &["-s", "-k", "--signal", "--kill-after"]) + 1;
            }
            "env" => match env_skip(&w, i + 1) {
                Ok(j) => i = j,
                Err(code) => {
                    return Some(Cmd {
                        name: n,
                        raw0: x,
                        args: w[i + 1..].to_vec(),
                        call,
                        inner: Some(code_or_opaque(code, Lang::Bash)),
                    });
                }
            },
            _ if is_assignment(&x) => i += 1,
            _ => break,
        }
    }
    let raw0 = w.get(i)?.clone();
    let name = norm(&raw0);
    let args = w[i + 1..].to_vec();
    let inner = inner_of(&name, &args);
    Some(Cmd {
        name,
        raw0,
        args,
        call,
        inner,
    })
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
/// `.com` и `@` cmd.
fn norm(w: &str) -> String {
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
fn percent_var(s: &str) -> bool {
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

/// Имя команды вычисляется при исполнении — команда неизвестна.
fn dynamic_cmd(c: &Cmd, lang: Lang) -> bool {
    let r = c.raw0.as_str();
    if lang == Lang::Pwsh && !c.call {
        // Выражение, а не вызов: PowerShell не исполняет значение переменной без `&`,
        // тела групп и блоков разбираются отдельными сегментами.
        if r.starts_with(['(', '{']) || r.starts_with("@(") || r.starts_with("@{") {
            return false;
        }
        if r.starts_with('$') && !r.contains('(') {
            return false;
        }
    }
    dynamic(r)
}

fn code_or_opaque(code: String, lang: Lang) -> Inner {
    let t = code.trim_start();
    if t.is_empty() || t == "-" || t.starts_with('$') || (t.starts_with('%') && percent_var(t)) {
        Inner::Opaque
    } else {
        Inner::Code(code, lang)
    }
}

/// Строка вложенной интерпретации команды, если команда — интерпретатор.
fn inner_of(name: &str, args: &[String]) -> Option<Inner> {
    match name {
        "bash" | "sh" | "zsh" | "dash" | "ksh" | "ash" | "mksh" | "yash" => posix_inner(args),
        "eval" => (!args.is_empty()).then(|| code_or_opaque(args.join(" "), Lang::Bash)),
        "pwsh" | "powershell" | "pwsh-preview" => pwsh_inner(args),
        "iex" | "invoke-expression" => {
            let rest = match args.first() {
                Some(a) if a.len() > 2 && "-command".starts_with(&a.to_lowercase()) => &args[1..],
                _ => args,
            };
            let code = rest.join(" ");
            if code.trim_start().starts_with(['(', '@', '[']) {
                Some(Inner::Opaque)
            } else {
                Some(code_or_opaque(code, Lang::Pwsh))
            }
        }
        "cmd" => Some(cmd_inner(args)),
        "start-process" | "saps" => start_inner(args),
        _ => None,
    }
}

/// `sh -c 'код'`; без `-c` и без скрипта код идёт из stdin.
fn posix_inner(args: &[String]) -> Option<Inner> {
    let mut i = 0;
    let mut long = false;
    while let Some(a) = args.get(i) {
        if a == "--" || a == "-" {
            i += 1;
            break;
        }
        if a.starts_with("--") {
            long = true;
            i += 1;
            continue;
        }
        if a.len() > 1 && (a.starts_with('-') || a.starts_with('+')) {
            let flags = &a[1..];
            if flags.contains('c') {
                return Some(match args.get(i + 1) {
                    Some(code) => code_or_opaque(code.clone(), Lang::Bash),
                    None => Inner::Opaque,
                });
            }
            if flags.contains('s') {
                return Some(Inner::Opaque);
            }
            i += if matches!(flags, "o" | "O") { 2 } else { 1 };
            continue;
        }
        break;
    }
    if i < args.len() || long {
        None
    } else {
        Some(Inner::Opaque)
    }
}

/// Полные имена параметров `pwsh` со значением (сопоставляются по префиксу от 2 букв).
#[rustfmt::skip]
const PS_VALUE_FULL: &[&str] = &[
    "executionpolicy", "windowstyle", "workingdirectory", "configurationname", "outputformat",
    "inputformat", "psconsolefile", "settingsfile", "custompipename", "configurationfile",
    "version",
];

/// `pwsh -Command`, `powershell "код"`; `-EncodedCommand` и stdin не разбираются.
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
        if p.is_empty() || p == "e" || p == "ec" || p == "ea" || p.starts_with("en") {
            return Some(Inner::Opaque);
        }
        if "command".starts_with(&p) || p == "cwa" || p.starts_with("commandwith") {
            return Some(code_or_opaque(args[i + 1..].join(" "), Lang::Pwsh));
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
    if info { None } else { Some(Inner::Opaque) }
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
    Inner::Opaque
}

/// `Start-Process путь -ArgumentList аргументы` как строка PowerShell.
fn start_inner(args: &[String]) -> Option<Inner> {
    let mut parts = Vec::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        i += 1;
        let Some(p) = a.strip_prefix('-') else {
            parts.push(a.replace(',', " "));
            continue;
        };
        let p = p.split(':').next().unwrap_or("").to_lowercase();
        if p.is_empty() {
            continue;
        }
        if "filepath".starts_with(&p) || "argumentlist".starts_with(&p) || p == "args" {
            if let Some(v) = args.get(i) {
                parts.push(v.replace(',', " "));
                i += 1;
            }
        } else if !["wa", "no", "pa", "us", "lo"]
            .iter()
            .any(|s| p.starts_with(s))
        {
            i += 1;
        }
    }
    (!parts.is_empty()).then(|| Inner::Code(parts.join(" "), Lang::Pwsh))
}

// --- Необратимые команды (D81) -------------------------------------------------------

/// Неразобранный сегмент и перенаправление вывода наружу или в неизвестную цель.
fn base_label(seg: &Segment, outside: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    if seg.unparsed {
        return Some(label::UNPARSED);
    }
    seg.redirects.iter().find_map(|r| match parse_redirect(r) {
        (true, Some(t)) if !null_dev(t) && (dynamic_path(t) || outside(t)) => {
            Some(label::OVERWRITE)
        }
        _ => None,
    })
}

/// Сегмент без рекурсии во вложенную интерпретацию: она здесь — уже уровень глубже.
fn direct(seg: &Segment, lang: Lang, outside: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    base_label(seg, outside)
        .or_else(|| analyze(&seg.argv, lang).and_then(|c| check_cmd(&c, lang, outside, false)))
}

fn check_cmd(
    c: &Cmd,
    lang: Lang,
    outside: &dyn Fn(&str) -> bool,
    recurse: bool,
) -> Option<&'static str> {
    if matches!(c.inner, Some(Inner::Opaque)) || dynamic_cmd(c, lang) {
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
        _ => None,
    }
}

/// `(вывод, цель-файл)` перенаправления; дублирование дескриптора и here-doc — без цели.
fn parse_redirect(r: &str) -> (bool, Option<&str>) {
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

fn null_dev(t: &str) -> bool {
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
    match c.name.as_str() {
        "rm" => hit(rm_flags(a) || (pw && ps_remove(a)), label::DELETE),
        "remove-item" => hit(ps_remove(a), label::DELETE),
        "ri" if pw => hit(ps_remove(a), label::DELETE),
        "del" | "erase" | "rd" | "rmdir" => hit(slash_s(a) || (pw && ps_remove(a)), label::DELETE),
        "shred" | "wipefs" | "srm" | "format-volume" | "clear-disk" | "remove-partition"
        | "clear-recyclebin" => Some(label::DELETE),
        n if n.starts_with("mkfs") => Some(label::DELETE),
        "robocopy" => hit(
            a.iter()
                .any(|x| x.eq_ignore_ascii_case("/mir") || x.eq_ignore_ascii_case("/purge")),
            label::DELETE,
        ),
        "dd" => hit(
            a.iter()
                .filter_map(|x| x.strip_prefix("of="))
                .any(|t| dev_path(t) || dynamic_path(t) || outside(t)),
            label::OVERWRITE,
        ),
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
            ("volume", "rm" | "prune") => Some(label::DELETE),
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

/// `Remove-Item` с `-Recurse` (префикс от 1 буквы) или `-Force` (от 2), кроме `:$false`.
fn ps_remove(a: &[String]) -> bool {
    a.iter().any(|x| {
        let Some(p) = x.strip_prefix('-') else {
            return false;
        };
        if !p.starts_with(|c: char| c.is_ascii_alphabetic()) {
            return false;
        }
        let (name, val) = match p.split_once(':') {
            Some((n, v)) => (n.to_lowercase(), Some(v)),
            None => (p.to_lowercase(), None),
        };
        let on = val.is_none_or(|v| !(v.eq_ignore_ascii_case("$false") || v == "0"));
        on && ("recurse".starts_with(&name) || (name.len() >= 2 && "force".starts_with(&name)))
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

/// Ключи `git -c`, исполняющие команды или меняющие алиасы.
fn git_exec_key(kv: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "pager",
        "sshcommand",
        "fsmonitor",
        "hookspath",
        "editor",
        "command",
        "external",
        "helper",
        "textconv",
        "program",
        "askpass",
        "gitproxy",
        "clean",
        "smudge",
        "process",
        "hook",
        "cmd",
    ];
    let key = kv.split('=').next().unwrap_or("").to_lowercase();
    ["alias.", "include.", "includeif."]
        .iter()
        .any(|p| key.starts_with(p))
        || SUFFIXES.iter().any(|s| key.ends_with(s))
}

/// Индекс подкоманды `git` после глобальных опций и признак опасного `-c`.
fn git_split(a: &[String]) -> (usize, bool) {
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
    let reset = match sub {
        "reset" => has("--hard"),
        "checkout" => short('f') || has("--force") || all_paths,
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

// --- Сеть и пути (D77) ---------------------------------------------------------------

fn net_cmd(c: &Cmd) -> bool {
    match c.name.as_str() {
        "curl" | "wget" | "wget2" | "ssh" | "scp" | "sftp" | "ftp" | "tftp" | "telnet" | "nc"
        | "ncat" | "netcat" | "socat" | "invoke-webrequest" | "iwr" | "invoke-restmethod"
        | "irm" | "start-bitstransfer" | "aria2c" | "http" | "https" | "xh" | "gh" => true,
        "rsync" => c.args.iter().any(|x| !x.starts_with('-') && remote_spec(x)),
        "git" => {
            let (i, _) = git_split(&c.args);
            matches!(
                c.args.get(i).map(String::as_str),
                Some("clone" | "fetch" | "pull" | "push" | "ls-remote")
            )
        }
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

/// Похоже на путь наружу: абсолютный путь, `~`, UNC, компонент `..`.
fn path_like(w: &str) -> bool {
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
            skip_vals: &[
                "-e",
                "--regexp",
                "-g",
                "--glob",
                "--iglob",
                "-t",
                "--type",
                "-T",
                "--type-not",
                "-m",
                "-A",
                "-B",
                "-C",
            ],
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

fn words_of(seg: &Segment, lang: Lang, out: &mut Vec<String>) {
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
    let a = c.args.as_slice();
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
            generic_words(a, &WordSpec::default(), out);
        }
        n => generic_words(a, &spec_for(n), out),
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
        if sp.win_switches && x.starts_with('/') && !x[1..].contains(['/', '\\']) {
            continue;
        }
        if path_like(x) {
            out.push(x.clone());
        } else if let Some((_, v)) = x.split_once('=')
            && path_like(v)
        {
            out.push(v.to_string());
        }
    }
}
