//! Своё строгое подмножество YAML для frontmatter `SKILL.md` (D115), без зависимостей.
//!
//! Поддерживается:
//! - карта верхнего уровня `key: value` и одна вложенная карта под ключом (`metadata`);
//! - скаляры: простые (в том числе многострочные), `'…'` (`''` — кавычка), `"…"` с
//!   экранированиями YAML 1.2 (`\n`, `\t`, `\"`, `\\`, `\uXXXX` и прочие), многострочные
//!   кавычки со сворачиванием строк;
//! - блоки `|` и `>` с индикаторами `-` и `+`;
//! - списки скаляров `[a, b]` (в одну строку) и блочные `- a` (в том числе на отступе ключа);
//! - комментарии `#`.
//!
//! Всё — строки: `no`, `true`, `~`, `1e3` остаются текстом (сюрпризов YAML 1.1 нет).
//!
//! Отказ двух видов. [`YamlError::Unsupported`] — корректный YAML вне подмножества:
//! якоря, алиасы, теги, flow-карты, второй документ, директивы, сложные ключи, ключи в
//! кавычках, вложенность глубже одного уровня, многострочный flow-список, индикатор
//! отступа блока. [`YamlError::Unparseable`] — ошибка синтаксиса: табуляция в отступе,
//! повтор ключа, незакрытая кавычка, неизвестное экранирование и прочее.
//!
//! Разбор однопроходный по строкам, рекурсия ограничена двумя уровнями карт: время линейно
//! по длине входа, глубина стека не зависит от входа («billion laughs» и вложенность не
//! проходят по построению).

use std::collections::BTreeMap;

/// Значение поля frontmatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    List(Vec<String>),
    /// Вложенная карта (один уровень): значения — строки и списки.
    Map(BTreeMap<String, Value>),
}

/// Ошибка разбора подмножества.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YamlError {
    /// Корректный YAML вне подмножества.
    Unsupported(String),
    /// Ошибка синтаксиса.
    Unparseable(String),
}

type Res<T> = Result<T, YamlError>;

/// Разбор текста frontmatter; `first_line` — номер его первой строки в файле (для текста
/// ошибок).
pub fn parse(src: &str, first_line: usize) -> Res<BTreeMap<String, Value>> {
    let lines = src
        .split('\n')
        .enumerate()
        .map(|(n, raw)| {
            let raw = raw.strip_suffix('\r').unwrap_or(raw);
            let rest = raw.trim_start_matches(' ');
            Line {
                no: first_line + n,
                indent: raw.len() - rest.len(),
                rest,
            }
        })
        .collect();
    let mut p = Parser { lines, i: 0 };
    let map = p.map(0, 0)?;
    if let Some(l) = p.peek_content() {
        p.check_tab(l)?;
        return Err(unparseable(l.no, "unexpected indentation"));
    }
    Ok(map)
}

#[derive(Debug, Clone, Copy)]
struct Line<'a> {
    /// Номер строки в файле.
    no: usize,
    /// Число ведущих пробелов.
    indent: usize,
    /// Текст после ведущих пробелов, без `\r` в конце.
    rest: &'a str,
}

impl Line<'_> {
    /// Пустая строка: только пробелы и табуляции.
    fn blank(&self) -> bool {
        self.rest.trim_matches([' ', '\t']).is_empty()
    }

    /// Строка-комментарий.
    fn comment(&self) -> bool {
        self.rest.trim_start_matches([' ', '\t']).starts_with('#')
    }
}

fn unsupported(no: usize, what: &str) -> YamlError {
    YamlError::Unsupported(format!("{what} (line {no})"))
}

fn unparseable(no: usize, what: &str) -> YamlError {
    YamlError::Unparseable(format!("{what} (line {no})"))
}

/// `s` начинается с индикатора `c`, за которым пробел, табуляция или конец строки.
fn is_marker(s: &str, c: char) -> bool {
    let mut it = s.chars();
    it.next() == Some(c) && matches!(it.next(), None | Some(' ' | '\t'))
}

/// Текст до комментария (` #` или табуляция и `#`) и признак, что комментарий был.
fn cut_comment(s: &str) -> (&str, bool) {
    let b = s.as_bytes();
    for i in 1..b.len() {
        if b[i] == b'#' && matches!(b[i - 1], b' ' | b'\t') {
            return (&s[..i], true);
        }
    }
    (s, false)
}

/// Позиция `:` ключа: первое двоеточие, за которым пробел, табуляция или конец строки.
fn key_colon(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    (0..b.len()).find(|&i| b[i] == b':' && matches!(b.get(i + 1), None | Some(b' ' | b'\t')))
}

/// Строка похожа на элемент карты `key: …` (не начинается с индикатора скаляра).
fn looks_like_entry(s: &str) -> bool {
    let first = s.chars().next();
    !matches!(
        first,
        Some('"' | '\'' | '[' | '{' | '&' | '*' | '!' | '|' | '>' | '#')
    ) && key_colon(s).is_some_and(|k| !cut_comment(&s[..k]).1)
}

/// Индикатор в начале скаляра, после которого текст не разбирается.
fn scalar_indicator(no: usize, first: char, rest: &str) -> Option<YamlError> {
    match first {
        '&' => Some(unsupported(no, "anchor")),
        '*' => Some(unsupported(no, "alias")),
        '!' => Some(unsupported(no, "tag")),
        '{' => Some(unsupported(no, "flow mapping")),
        '?' if is_marker(rest, '?') => Some(unsupported(no, "complex mapping key")),
        '@' | '`' | ',' | ']' | '}' | '%' => {
            Some(unparseable(no, &format!("unexpected '{first}'")))
        }
        '-' if is_marker(rest, '-') => Some(unparseable(no, "list item on the key line")),
        _ => None,
    }
}

/// Итог разбора строки внутри кавычек.
enum Scan {
    /// Кавычка закрыта; байтовая позиция после неё.
    Closed(usize),
    /// Строка кончилась; `true` — конец строки экранирован `\`.
    Open(bool),
}

/// Разбор строки внутри `"…"` от позиции после открывающей кавычки. Текст дописывается в
/// `out`; `keep` — длина `out` без хвостовых пробелов из самого файла (экранированные
/// символы хвостом не считаются).
fn scan_double(no: usize, s: &str, out: &mut String, keep: &mut usize) -> Res<Scan> {
    let mut it = s.char_indices();
    while let Some((i, c)) = it.next() {
        match c {
            '"' => return Ok(Scan::Closed(i + 1)),
            '\\' => {
                let Some((_, e)) = it.next() else {
                    return Ok(Scan::Open(true));
                };
                let ch = match e {
                    '0' => '\0',
                    'a' => '\u{7}',
                    'b' => '\u{8}',
                    't' | '\t' => '\t',
                    'n' => '\n',
                    'v' => '\u{b}',
                    'f' => '\u{c}',
                    'r' => '\r',
                    'e' => '\u{1b}',
                    ' ' => ' ',
                    '"' => '"',
                    '/' => '/',
                    '\\' => '\\',
                    'N' => '\u{85}',
                    '_' => '\u{a0}',
                    'L' => '\u{2028}',
                    'P' => '\u{2029}',
                    'x' | 'u' | 'U' => {
                        let n = match e {
                            'x' => 2,
                            'u' => 4,
                            _ => 8,
                        };
                        let code = hex_digits(no, &mut it, n)?;
                        let code = if (0xD800..0xDC00).contains(&code) && e == 'u' {
                            // Суррогатная пара JSON: `\uD83D\uDE00`.
                            let low = match (it.next(), it.next()) {
                                (Some((_, '\\')), Some((_, 'u'))) => hex_digits(no, &mut it, 4)?,
                                _ => return Err(unparseable(no, "unpaired surrogate escape")),
                            };
                            if !(0xDC00..0xE000).contains(&low) {
                                return Err(unparseable(no, "unpaired surrogate escape"));
                            }
                            0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00)
                        } else {
                            code
                        };
                        char::from_u32(code)
                            .ok_or_else(|| unparseable(no, "invalid escape code point"))?
                    }
                    other => {
                        return Err(unparseable(
                            no,
                            &format!("unknown escape '\\{}'", other.escape_debug()),
                        ));
                    }
                };
                out.push(ch);
                *keep = out.len();
            }
            c => {
                out.push(c);
                if c != ' ' && c != '\t' {
                    *keep = out.len();
                }
            }
        }
    }
    Ok(Scan::Open(false))
}

fn hex_digits(no: usize, it: &mut std::str::CharIndices<'_>, n: usize) -> Res<u32> {
    let mut v: u32 = 0;
    for _ in 0..n {
        let d = it
            .next()
            .and_then(|(_, c)| c.to_digit(16))
            .ok_or_else(|| unparseable(no, "invalid hex escape"))?;
        v = (v << 4) | d;
    }
    Ok(v)
}

/// Разбор строки внутри `'…'` от позиции после открывающей кавычки (см. [`scan_double`]).
fn scan_single(s: &str, out: &mut String, keep: &mut usize) -> Scan {
    let b = s.as_bytes();
    let mut i = 0;
    let mut from = 0;
    while i < b.len() {
        if b[i] == b'\'' {
            if b.get(i + 1) == Some(&b'\'') {
                push_literal(out, keep, &s[from..i]);
                out.push('\'');
                *keep = out.len();
                i += 2;
                from = i;
                continue;
            }
            push_literal(out, keep, &s[from..i]);
            return Scan::Closed(i + 1);
        }
        i += 1;
    }
    push_literal(out, keep, &s[from..]);
    Scan::Open(false)
}

fn push_literal(out: &mut String, keep: &mut usize, s: &str) {
    out.push_str(s);
    let trimmed = s.trim_end_matches([' ', '\t']);
    if !trimmed.is_empty() {
        *keep = out.len() - (s.len() - trimmed.len());
    }
}

/// Хвост строки после закрывающей кавычки: пусто или комментарий.
fn after_quoted(no: usize, after: &str) -> Res<()> {
    let a = after.trim_start_matches([' ', '\t']);
    if a.is_empty() || (a.starts_with('#') && a.len() < after.len()) {
        return Ok(());
    }
    if a.starts_with(':') {
        return Err(unsupported(no, "quoted key"));
    }
    Err(unparseable(no, "text after a quoted scalar"))
}

/// Вид блочного скаляра по заголовку.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chomp {
    Clip,
    Strip,
    Keep,
}

struct Parser<'a> {
    lines: Vec<Line<'a>>,
    i: usize,
}

impl<'a> Parser<'a> {
    /// Следующая значимая строка (пустые и комментарии пропускаются и съедаются).
    fn peek_content(&mut self) -> Option<Line<'a>> {
        while let Some(l) = self.lines.get(self.i) {
            if l.blank() || l.comment() {
                self.i += 1;
            } else {
                return Some(*l);
            }
        }
        None
    }

    /// Табуляция в отступе структурной строки — ошибка (в YAML отступ — только пробелы).
    fn check_tab(&self, l: Line<'_>) -> Res<()> {
        if l.rest.starts_with('\t') {
            return Err(unparseable(l.no, "tab in indentation"));
        }
        Ok(())
    }

    /// Карта с отступом `indent`; `depth` 0 — верхний уровень, 1 — вложенная.
    fn map(&mut self, indent: usize, depth: u8) -> Res<BTreeMap<String, Value>> {
        let mut out = BTreeMap::new();
        while let Some(l) = self.peek_content() {
            if l.indent < indent {
                break;
            }
            self.check_tab(l)?;
            if l.indent > indent {
                return Err(unparseable(l.no, "unexpected indentation"));
            }
            let (key, rest) = entry(l, depth == 0 && out.is_empty())?;
            if out.contains_key(key) {
                return Err(unparseable(l.no, &format!("duplicate key {key}")));
            }
            self.i += 1;
            let v = self.value(rest, l, depth)?;
            out.insert(key.to_owned(), v);
        }
        Ok(out)
    }

    /// Значение после `key:` строки `l`.
    fn value(&mut self, rest: &'a str, l: Line<'a>, depth: u8) -> Res<Value> {
        let r = rest.trim_start_matches([' ', '\t']);
        if r.is_empty() || r.starts_with('#') {
            return self.value_below(l.indent, depth);
        }
        self.inline(r, l.no, l.indent)
    }

    /// Значение на следующих строках: блочный список, вложенная карта или скаляр.
    fn value_below(&mut self, key_indent: usize, depth: u8) -> Res<Value> {
        let Some(n) = self.peek_content() else {
            return Ok(Value::Str(String::new()));
        };
        if n.indent > key_indent {
            self.check_tab(n)?;
            if is_marker(n.rest, '-') {
                return self.block_list(n.indent).map(Value::List);
            }
            if looks_like_entry(n.rest) {
                if depth >= 1 {
                    return Err(unsupported(n.no, "nested mapping deeper than one level"));
                }
                return self.map(n.indent, depth + 1).map(Value::Map);
            }
            self.i += 1;
            return self.inline(n.rest, n.no, key_indent);
        }
        if n.indent == key_indent && is_marker(n.rest, '-') {
            return self.block_list(n.indent).map(Value::List);
        }
        Ok(Value::Str(String::new()))
    }

    /// Скаляр или flow-список, начатый текстом `r` строки `no`; продолжения — строки с
    /// отступом больше `parent`.
    fn inline(&mut self, r: &'a str, no: usize, parent: usize) -> Res<Value> {
        let Some(first) = r.chars().next() else {
            return Ok(Value::Str(String::new()));
        };
        if let Some(e) = scalar_indicator(no, first, r) {
            return Err(e);
        }
        match first {
            '[' => self.flow_list(&r[1..], no, parent).map(Value::List),
            '|' | '>' => self.block_scalar(r, no, parent).map(Value::Str),
            '"' | '\'' => self.quoted(r, no, parent).map(Value::Str),
            _ => Ok(Value::Str(self.plain(r, parent)?)),
        }
    }

    /// Простой скаляр: первая строка `first`, продолжения складываются через пробел, пустые
    /// строки дают перевод строки. Комментарий завершает скаляр.
    fn plain(&mut self, first: &str, parent: usize) -> Res<String> {
        let (text, commented) = cut_comment(first);
        let mut out = text.trim_end_matches([' ', '\t']).to_owned();
        if commented {
            return Ok(out);
        }
        let mut newlines = 0;
        while let Some(l) = self.lines.get(self.i).copied() {
            if l.blank() {
                newlines += 1;
                self.i += 1;
                continue;
            }
            if l.indent <= parent || l.comment() {
                break;
            }
            self.check_tab(l)?;
            let (t, commented) = cut_comment(l.rest);
            if newlines > 0 {
                out.extend(std::iter::repeat_n('\n', newlines));
            } else {
                out.push(' ');
            }
            out.push_str(t.trim_matches([' ', '\t']));
            newlines = 0;
            self.i += 1;
            if commented {
                break;
            }
        }
        Ok(out)
    }

    /// Скаляр в кавычках (`r` начинается с кавычки), возможно многострочный.
    fn quoted(&mut self, r: &'a str, no: usize, parent: usize) -> Res<String> {
        let double = r.starts_with('"');
        let mut out = String::new();
        let mut keep = 0;
        let mut text: &str = &r[1..];
        let mut no = no;
        loop {
            let scan = if double {
                scan_double(no, text, &mut out, &mut keep)?
            } else {
                scan_single(text, &mut out, &mut keep)
            };
            let escaped = match scan {
                Scan::Closed(at) => {
                    after_quoted(no, &text[at..])?;
                    return Ok(out);
                }
                Scan::Open(escaped) => escaped,
            };
            if !escaped {
                out.truncate(keep);
            }
            let mut empties = 0;
            let next = loop {
                let Some(l) = self.lines.get(self.i).copied() else {
                    return Err(unparseable(no, "unterminated quoted scalar"));
                };
                if l.blank() {
                    empties += 1;
                    self.i += 1;
                    continue;
                }
                if l.indent <= parent {
                    return Err(unparseable(l.no, "unterminated quoted scalar"));
                }
                self.i += 1;
                break l;
            };
            if empties > 0 {
                out.extend(std::iter::repeat_n('\n', empties));
            } else if !escaped {
                out.push(' ');
            }
            keep = out.len();
            no = next.no;
            text = next.rest.trim_start_matches([' ', '\t']);
        }
    }

    /// Flow-список скаляров в одну строку; `s` — текст после `[`.
    fn flow_list(&mut self, s: &str, no: usize, parent: usize) -> Res<Vec<String>> {
        let mut items = Vec::new();
        let b = s.as_bytes();
        let mut i = 0;
        // Ждём элемент (после `[` или `,`); `]` сразу после `,` — допустимая висячая запятая.
        let mut want_item = true;
        loop {
            while i < b.len() && matches!(b[i], b' ' | b'\t') {
                i += 1;
            }
            let Some(&c) = b.get(i) else {
                return Err(self.flow_unclosed(no, parent));
            };
            match c {
                b']' => {
                    after_quoted(no, &s[i + 1..])
                        .map_err(|_| unparseable(no, "text after a flow sequence"))?;
                    return Ok(items);
                }
                b',' => {
                    if want_item {
                        return Err(unparseable(no, "empty entry in a flow sequence"));
                    }
                    want_item = true;
                    i += 1;
                    continue;
                }
                b'#' if i > 0 && matches!(b[i - 1], b' ' | b'\t') => {
                    return Err(self.flow_unclosed(no, parent));
                }
                _ => {}
            }
            if !want_item {
                return Err(unparseable(no, "expected ',' or ']' in a flow sequence"));
            }
            let rest = &s[i..];
            let first = rest.chars().next().unwrap_or(' ');
            match first {
                '[' | '{' => return Err(unsupported(no, "nested flow collection")),
                '&' => return Err(unsupported(no, "anchor")),
                '*' => return Err(unsupported(no, "alias")),
                '!' => return Err(unsupported(no, "tag")),
                '?' if is_marker(rest, '?') => {
                    return Err(unsupported(no, "complex mapping key"));
                }
                '"' | '\'' => {
                    let mut out = String::new();
                    let mut keep = 0;
                    let scan = if first == '"' {
                        scan_double(no, &rest[1..], &mut out, &mut keep)?
                    } else {
                        scan_single(&rest[1..], &mut out, &mut keep)
                    };
                    let Scan::Closed(at) = scan else {
                        return Err(self.flow_unclosed(no, parent));
                    };
                    items.push(out);
                    i += 1 + at;
                }
                _ => {
                    let end = rest
                        .char_indices()
                        .find(|&(j, c)| {
                            c == ','
                                || c == ']'
                                || (c == '#' && j > 0 && rest[..j].ends_with([' ', '\t']))
                        })
                        .map_or(rest.len(), |(j, _)| j);
                    let item = rest[..end].trim_end_matches([' ', '\t']);
                    if item.contains(['[', '{', '}']) {
                        return Err(unparseable(no, "flow indicator inside a flow scalar"));
                    }
                    if key_colon(item).is_some() {
                        return Err(unsupported(no, "mapping in a flow sequence"));
                    }
                    items.push(item.to_owned());
                    i += end;
                }
            }
            want_item = false;
        }
    }

    /// Flow-список не закрыт в строке: продолжение на следующих строках — вне подмножества,
    /// иначе — ошибка синтаксиса.
    fn flow_unclosed(&self, no: usize, parent: usize) -> YamlError {
        let continued = self.lines[self.i..]
            .iter()
            .find(|l| !l.blank())
            .is_some_and(|l| l.indent > parent);
        if continued {
            unsupported(no, "multi-line flow sequence")
        } else {
            unparseable(no, "unterminated flow sequence")
        }
    }

    /// Блочный список скаляров с отступом `indent`.
    fn block_list(&mut self, indent: usize) -> Res<Vec<String>> {
        let mut items = Vec::new();
        while let Some(l) = self.peek_content() {
            if l.indent < indent {
                break;
            }
            self.check_tab(l)?;
            if l.indent > indent {
                return Err(unparseable(l.no, "unexpected indentation"));
            }
            if !is_marker(l.rest, '-') {
                break;
            }
            self.i += 1;
            let r = l.rest[1..].trim_start_matches([' ', '\t']);
            if r.is_empty() || r.starts_with('#') {
                if self.peek_content().is_some_and(|n| n.indent > indent) {
                    return Err(unsupported(l.no, "list item value on the next line"));
                }
                items.push(String::new());
                continue;
            }
            let first = r.chars().next().unwrap_or(' ');
            let item = match first {
                '-' if is_marker(r, '-') => return Err(unsupported(l.no, "nested list")),
                '[' | '{' => return Err(unsupported(l.no, "nested collection in a list")),
                '|' | '>' => return Err(unsupported(l.no, "block scalar in a list")),
                '"' | '\'' => self.quoted(r, l.no, indent)?,
                _ => {
                    if let Some(e) = scalar_indicator(l.no, first, r) {
                        return Err(e);
                    }
                    if key_colon(cut_comment(r).0).is_some() {
                        return Err(unsupported(l.no, "mapping in a list"));
                    }
                    self.plain(r, indent)?
                }
            };
            items.push(item);
        }
        Ok(items)
    }

    /// Блочный скаляр `|` или `>`; `r` — заголовок. Содержимое — строки с отступом больше
    /// `parent`; отступ блока — по первой непустой строке.
    fn block_scalar(&mut self, r: &str, no: usize, parent: usize) -> Res<String> {
        let folded = r.starts_with('>');
        let header = &r[1..];
        let mut chomp = Chomp::Clip;
        let mut h = header;
        if let Some(rest) = h.strip_prefix('-') {
            chomp = Chomp::Strip;
            h = rest;
        } else if let Some(rest) = h.strip_prefix('+') {
            chomp = Chomp::Keep;
            h = rest;
        }
        if h.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(unsupported(no, "block scalar indentation indicator"));
        }
        let t = h.trim_start_matches([' ', '\t']);
        if !(t.is_empty() || (t.starts_with('#') && t.len() < h.len())) {
            return Err(unparseable(no, "invalid block scalar header"));
        }
        // Строки содержимого: `None` — пустая.
        let mut content: Vec<Option<String>> = Vec::new();
        let mut block: Option<usize> = None;
        while let Some(l) = self.lines.get(self.i).copied() {
            if l.blank() {
                content.push(None);
                self.i += 1;
                continue;
            }
            let bi = match block {
                Some(b) => b,
                None if l.indent > parent => {
                    block = Some(l.indent);
                    l.indent
                }
                None => break,
            };
            if l.indent < bi {
                break;
            }
            let mut s = " ".repeat(l.indent - bi);
            s.push_str(l.rest);
            content.push(Some(s));
            self.i += 1;
        }
        let trail = content.iter().rev().take_while(|c| c.is_none()).count();
        let body = &content[..content.len() - trail];
        let has_text = body.iter().any(Option::is_some);
        let mut out = String::new();
        if folded {
            // Сворачивание YAML: перевод между двумя обычными строками — пробел, пустые строки
            // между ними — переводы; строки с большим отступом сохраняют переводы.
            let mut prev: Option<bool> = None;
            let mut empties = 0;
            for item in body {
                let Some(s) = item else {
                    empties += 1;
                    continue;
                };
                let more = s.starts_with([' ', '\t']);
                match prev {
                    None => out.extend(std::iter::repeat_n('\n', empties)),
                    Some(prev_more) if !more && !prev_more => {
                        if empties == 0 {
                            out.push(' ');
                        } else {
                            out.extend(std::iter::repeat_n('\n', empties));
                        }
                    }
                    Some(_) => out.extend(std::iter::repeat_n('\n', empties + 1)),
                }
                out.push_str(s);
                prev = Some(more);
                empties = 0;
            }
        } else {
            for (k, item) in body.iter().enumerate() {
                if k > 0 {
                    out.push('\n');
                }
                if let Some(s) = item {
                    out.push_str(s);
                }
            }
        }
        match chomp {
            Chomp::Strip => {}
            Chomp::Clip if has_text => out.push('\n'),
            Chomp::Clip => {}
            Chomp::Keep => {
                let n = if has_text { 1 + trail } else { trail };
                out.extend(std::iter::repeat_n('\n', n));
            }
        }
        Ok(out)
    }
}

/// Ключ и текст после `:` строки `l`; `first` — первая запись верхнего уровня.
fn entry<'a>(l: Line<'a>, first: bool) -> Res<(&'a str, &'a str)> {
    let t = l.rest;
    let doc_marker = (t.starts_with("---") || t.starts_with("..."))
        && matches!(t.as_bytes().get(3), None | Some(b' ' | b'\t'));
    if l.indent == 0 && doc_marker {
        return Err(unsupported(l.no, "second document"));
    }
    let c = t.chars().next().unwrap_or(' ');
    match c {
        '-' if is_marker(t, '-') => {
            return Err(if first {
                unsupported(l.no, "sequence at top level")
            } else {
                unparseable(l.no, "unexpected list item")
            });
        }
        '?' if is_marker(t, '?') => return Err(unsupported(l.no, "complex mapping key")),
        '&' => return Err(unsupported(l.no, "anchor")),
        '*' => return Err(unsupported(l.no, "alias")),
        '!' => return Err(unsupported(l.no, "tag")),
        '{' => return Err(unsupported(l.no, "flow mapping")),
        '[' => return Err(unsupported(l.no, "flow sequence as a key")),
        '"' | '\'' => return Err(unsupported(l.no, "quoted key")),
        '%' if l.indent == 0 => return Err(unsupported(l.no, "directive")),
        '|' | '>' | '@' | '`' | ',' | ']' | '}' | '%' => {
            return Err(unparseable(l.no, &format!("unexpected '{c}'")));
        }
        _ => {}
    }
    let Some(k) = key_colon(t) else {
        return Err(unparseable(l.no, "expected 'key: value'"));
    };
    let key = t[..k].trim_end_matches([' ', '\t']);
    if key.is_empty() || cut_comment(key).1 {
        return Err(unparseable(l.no, "expected 'key: value'"));
    }
    Ok((key, &t[k + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> BTreeMap<String, Value> {
        parse(src, 1).unwrap_or_else(|e| panic!("{src:?}: {e:?}"))
    }

    fn s(v: &str) -> Value {
        Value::Str(v.to_owned())
    }

    fn unsupported_kind(src: &str) -> String {
        match parse(src, 1) {
            Err(YamlError::Unsupported(w)) => w,
            other => panic!("{src:?}: ожидалось Unsupported, получено {other:?}"),
        }
    }

    fn unparseable_kind(src: &str) -> String {
        match parse(src, 1) {
            Err(YamlError::Unparseable(w)) => w,
            other => panic!("{src:?}: ожидалось Unparseable, получено {other:?}"),
        }
    }

    #[test]
    fn scalars_and_escapes() {
        let m =
            ok("a: \"\\x41\\u00e9\\U0001F600\\uD83D\\uDE00\\0\\/\\ \"\nb: 'x''y'\nc: plain  # c\n");
        assert_eq!(m["a"], s("A\u{e9}\u{1F600}\u{1F600}\0/ "));
        assert_eq!(m["b"], s("x'y"));
        assert_eq!(m["c"], s("plain"));
        // Многострочные кавычки: перевод — пробел, пустая строка — перевод, `\` в конце
        // строки — без пробела.
        let m = ok("a: \"one\n  two\n\n  three \\\n  four\"\nb: 'x\n  y'\n");
        assert_eq!(m["a"], s("one two\nthree four"));
        assert_eq!(m["b"], s("x y"));
        // Пустое значение — пустая строка.
        assert_eq!(ok("a:\nb: x\n")["a"], s(""));
        assert_eq!(ok("a: # only comment\n")["a"], s(""));
        // Лояльность: `: ` внутри простого скаляра — текст (строгий YAML отверг бы).
        let m = ok("a: Use when: asked\nb: x\n  y: z\nc: a#b\n");
        assert_eq!(m["a"], s("Use when: asked"));
        assert_eq!(m["b"], s("x y: z"));
        assert_eq!(m["c"], s("a#b"));
        // Значение на следующей строке: простое и в кавычках.
        let m = ok("a:\n  text\n  more\nb:\n  \"q\"\n");
        assert_eq!(m["a"], s("text more"));
        assert_eq!(m["b"], s("q"));
    }

    #[test]
    fn block_scalars_chomping() {
        let m = ok("a: |+\n  x\n\n\nb: >\n  p\n    more\n  q\nc: |\n\nd: x\n");
        assert_eq!(m["a"], s("x\n\n\n"));
        assert_eq!(m["b"], s("p\n  more\nq\n"));
        assert_eq!(m["c"], s(""));
        assert_eq!(m["d"], s("x"));
        // Табуляция внутри содержимого блока — текст.
        assert_eq!(ok("a: |\n  \tcode\n")["a"], s("\tcode\n"));
        assert!(unsupported_kind("a: |2\n  x\n").contains("indentation indicator"));
        assert!(unparseable_kind("a: | x\n").contains("block scalar header"));
    }

    #[test]
    fn lists() {
        let m = ok("a:\n  - x\n  - 'y'\n  -\nb:\n- z\nc: []\nd: [p, q,]\n");
        assert_eq!(
            m["a"],
            Value::List(vec!["x".into(), "y".into(), String::new()])
        );
        assert_eq!(m["b"], Value::List(vec!["z".into()]));
        assert_eq!(m["c"], Value::List(vec![]));
        assert_eq!(m["d"], Value::List(vec!["p".into(), "q".into()]));
        assert!(unsupported_kind("a:\n  - k: v\n").contains("mapping in a list"));
        assert!(unsupported_kind("a:\n  - - x\n").contains("nested list"));
        assert!(unsupported_kind("a: [[x]]\n").contains("nested flow"));
        assert!(unsupported_kind("a: [x, &y z]\n").contains("anchor"));
        assert!(unsupported_kind("a: [x: y]\n").contains("mapping in a flow"));
        assert!(unsupported_kind("a: [x,\n  y]\n").contains("multi-line flow"));
        assert!(unparseable_kind("a: [x, y\nb: z\n").contains("unterminated flow"));
        assert!(unparseable_kind("a: [x,, y]\n").contains("empty entry"));
        assert!(unparseable_kind("a: [x] y\n").contains("after a flow"));
    }

    #[test]
    fn nested_map_one_level_only() {
        let m = ok("m:\n  a: 1\n  b:\n    - x\n  c: [y]\nn: z\n");
        let Value::Map(inner) = &m["m"] else { panic!() };
        assert_eq!(inner["a"], s("1"));
        assert_eq!(inner["b"], Value::List(vec!["x".into()]));
        assert_eq!(m["n"], s("z"));
        assert!(unsupported_kind("m:\n  a:\n    b: c\n").contains("deeper than one level"));
    }

    #[test]
    fn unsupported_features() {
        for (src, what) in [
            ("a: &x y\n", "anchor"),
            ("a: *x\n", "alias"),
            ("a: !t y\n", "tag"),
            ("a: {b: c}\n", "flow mapping"),
            ("- a\n", "sequence at top level"),
            ("? a\n: b\n", "complex mapping key"),
            ("\"a\": b\n", "quoted key"),
            ("%YAML 1.2\n", "directive"),
            ("a: b\n...\n", "second document"),
            ("a: b\n--- c\n", "second document"),
            ("a:\n  \"k\": v\n", "quoted key"),
            ("a:\n  - \"k\": v\n", "quoted key"),
            ("<<: *base\n", "alias"),
        ] {
            assert!(unsupported_kind(src).contains(what), "{src:?}");
        }
    }

    #[test]
    fn syntax_errors() {
        for (src, what) in [
            ("\ta: b\n", "tab in indentation"),
            ("a: b\n\tc: d\n", "tab in indentation"),
            ("a: b\na: c\n", "duplicate key"),
            ("a: \"x\n", "unterminated quoted"),
            ("a: \"x\nb: y\n", "unterminated quoted"),
            ("a: \"\\q\"\n", "unknown escape"),
            ("a: \"\\uD800\"\n", "surrogate"),
            ("a: \"\\xZZ\"\n", "hex escape"),
            ("a: \"x\" y\n", "after a quoted"),
            ("just text\n", "expected 'key: value'"),
            ("a: 'x' # c\n  more\n", "unexpected indentation"),
            ("a: b # c\n  more\n", "unexpected indentation"),
            ("a: @x\n", "unexpected '@'"),
            ("a: - x\n", "list item on the key line"),
            ("m:\n  a: b\n c: d\n", "unexpected indentation"),
        ] {
            let got = parse(src, 1);
            assert!(
                matches!(&got, Err(YamlError::Unparseable(w)) if w.contains(what)),
                "{src:?}: {got:?}"
            );
        }
    }

    #[test]
    fn line_numbers_are_file_relative() {
        assert_eq!(
            parse("a: b\nc: *d\n", 2),
            Err(YamlError::Unsupported("alias (line 3)".into()))
        );
    }
}
