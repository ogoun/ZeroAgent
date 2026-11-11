//! Лексер: конечный автомат по символам в режимах bash, PowerShell и cmd.

use crate::command::{Inner, analyze};
use crate::{Lang, MAX_NEST, Segment};

/// Сегменты кода; `interp` — уровень вложенной интерпретации, `nest` — подстановок.
pub(crate) fn lex_code(code: &str, lang: Lang, interp: u8, nest: u8) -> Vec<Segment> {
    let chars: Vec<char> = if lang == Lang::Pwsh {
        pwsh_chars(code)
    } else {
        code.chars().collect()
    };
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
            // Код из stdin — метка `exec_untrusted`, а не неразобранный сегмент.
            Some(Inner::Stdin) => seg.has_subst = true,
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

/// Текст PowerShell в символах, равнозначных для его токенайзера (C1 ревью задачи 8):
/// типографские кавычки U+2018–U+201B открывают и закрывают строку как `'`, U+201C–U+201E
/// — как `"`; одиночный `\r` и `\r\n` — перевод строки (разделитель операторов); пробелы
/// Unicode (Zs, Zl, Zp, NBSP, U+0085), `\v`, `\f` и NUL разделяют слова, как пробел.
/// Тексты сегментов и слова берутся из этого вида.
fn pwsh_chars(code: &str) -> Vec<char> {
    let mut out = Vec::with_capacity(code.len());
    let mut it = code.chars().peekable();
    while let Some(c) = it.next() {
        out.push(match c {
            '\u{2018}'..='\u{201B}' => '\'',
            '\u{201C}'..='\u{201E}' => '"',
            '\r' => {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                '\n'
            }
            '\0' => ' ',
            c if c.is_whitespace() && c != '\n' => ' ',
            c => c,
        });
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
                && cur.argv.is_empty()
                && (c == '&'
                    || (c == '.' && matches!(self.at(1), Some(' ' | '(' | '{' | '\'' | '"' | '$'))))
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
                        // Подстановка процесса: её вывод — код или данные, которых нет в
                        // тексте (`bash <(curl …)`), сегмент не разобран (I4).
                        cur.has_subst = true;
                        cur.unparsed = true;
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
            let mut w = self.word(&mut cur);
            if self.lang == Lang::Pwsh && cur.argv.is_empty() && matches!(c, '\'' | '"') {
                // Строковый литерал в начале сегмента — выражение: кавычки остаются.
                w = self.s[before..self.i].iter().collect();
            }
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
            if matches!(c, ' ' | '\t' | '\r') || (self.lang == Lang::Pwsh && c == '\u{a0}') {
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
                        Some('\\') => {
                            if !self.ansi_escape(w) {
                                self.fail(cur);
                                return;
                            }
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

    /// Escape-последовательность `$'…'` с `\` в текущей позиции (I1); `false` — не
    /// декодируется однозначно.
    fn ansi_escape(&mut self, w: &mut String) -> bool {
        let Some(x) = self.at(1) else { return false };
        self.i += 2;
        let simple = match x {
            'n' => Some('\n'),
            't' => Some('\t'),
            'r' => Some('\r'),
            'a' => Some('\u{7}'),
            'b' => Some('\u{8}'),
            'e' | 'E' => Some('\u{1b}'),
            'f' => Some('\u{c}'),
            'v' => Some('\u{b}'),
            '\\' | '\'' | '"' | '?' => Some(x),
            _ => None,
        };
        if let Some(ch) = simple {
            w.push(ch);
            return true;
        }
        let code = match x {
            'c' => {
                let Some(y) = self.peek() else { return false };
                self.i += 1;
                Some(u32::from(y) & 0x1f)
            }
            'x' | 'u' | 'U' | '0'..='7' => {
                let (radix, max) = match x {
                    'x' => (16, 2),
                    'u' => (16, 4),
                    'U' => (16, 8),
                    _ => {
                        self.i -= 1;
                        (8, 3)
                    }
                };
                let mut v = 0u32;
                let mut n = 0;
                while n < max {
                    let Some(d) = self.peek().and_then(|c| c.to_digit(radix)) else {
                        break;
                    };
                    v = v.saturating_mul(radix).saturating_add(d);
                    self.i += 1;
                    n += 1;
                }
                (n > 0).then_some(v)
            }
            _ => None,
        };
        match code.filter(|&v| v != 0).and_then(char::from_u32) {
            Some(ch) => {
                w.push(ch);
                true
            }
            None => false,
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
                ' ' | '\t' | '\r' | '\n' | '\u{a0}' | ';' | '|' | '&' | '<' | '>' | ')' | '}' => {
                    break;
                }
                // Типографские тире PowerShell принимает как `-` параметра (I2).
                '\u{2010}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' if w.is_empty() => {
                    w.push('-');
                    self.i += 1;
                }
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

pub(crate) fn is_word_char(c: char) -> bool {
    !matches!(
        c,
        ' ' | '\t' | '\r' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
    )
}
