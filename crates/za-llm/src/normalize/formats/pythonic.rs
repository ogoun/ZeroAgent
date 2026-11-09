//! Pythonic (Llama 3.2, Llama 4 и др.): `[f(a=1, b="x"), g()]` — весь ответ.
//!
//! Источники (сверено 2026-11-09): формат вендора `meta-llama/llama-models`
//! `models/llama3_2/text_prompt_format.md` («you MUST put it in the format of
//! [func_name1(params_name1=params_value1, …), func_name2(params)] You SHOULD NOT
//! include any other text in the response»; ответ может начинаться с
//! `<|python_tag|>` и кончаться `<|eot_id|>`/`<|eom_id|>`); парсеры vLLM
//! `vllm/tool_parsers/pythonic_tool_parser.py` (весь вывод — список вызовов, разбор
//! `ast`, только именованные аргументы-литералы) и `llama4_pythonic_tool_parser.py`
//! (обёртка `<|python_start|>…<|python_end|>`).
//!
//! - Формат без явных маркеров: разбирается только весь текст (после снятия
//!   служебных токенов по краям), начинающийся с `[`. Текст после `]` — не вызов.
//! - Вызовы извлекаются, только если хотя бы одно имя принято (`accept_name`, D28);
//!   остальные имена дают `UnknownTool` в конвейере. Иначе текст не меняется.
//! - Литералы: строки `'…'`, `"…"`, `'''…'''`, `"""…"""` с экранированием Python,
//!   целые и дробные числа (с `_`), `True`/`False`/`None`, списки, кортежи (как
//!   списки), словари. Позиционные аргументы и выражения — синтаксическая ошибка.
//! - Ошибка или обрыв внутри списка при принятом имени — отклонение всего текста,
//!   иначе текст.

use serde_json::{Map, Number, Value};

use super::{Extraction, ParseError, RawArgs, RawCall};

/// Служебные токены, которые могут окружать список вызовов.
const PREFIXES: [&str; 2] = ["<|python_tag|>", "<|python_start|>"];
const SUFFIXES: [&str; 3] = ["<|python_end|>", "<|eot_id|>", "<|eom_id|>"];
/// Предел вложенности литералов.
const MAX_DEPTH: usize = 64;

pub(super) fn extract(text: &str, accept_name: &dyn Fn(&str) -> bool) -> Extraction {
    let body = strip_tokens(text);
    if !body.starts_with('[') {
        return Extraction::unchanged(text);
    }
    let mut p = Py {
        s: body,
        names: Vec::new(),
    };
    let accepted = |names: &[&str]| names.iter().any(|n| accept_name(n));
    match p.call_list() {
        Ok((calls, end)) => {
            if calls.is_empty() || !body[end..].trim().is_empty() || !accepted(&p.names) {
                return Extraction::unchanged(text);
            }
            Extraction {
                calls: calls
                    .into_iter()
                    .map(|(name, args)| RawCall {
                        name,
                        args: RawArgs::Json(Value::Object(args)),
                    })
                    .collect(),
                remaining: String::new(),
            }
        }
        Err(_) if accepted(&p.names) => Extraction {
            calls: vec![RawCall::broken(text.trim())],
            remaining: String::new(),
        },
        Err(_) => Extraction::unchanged(text),
    }
}

/// Текст без пробелов и служебных токенов по краям.
fn strip_tokens(text: &str) -> &str {
    let mut body = text.trim();
    if let Some(b) = PREFIXES.iter().find_map(|p| body.strip_prefix(p)) {
        body = b.trim_start();
    }
    while let Some(b) = SUFFIXES.iter().find_map(|s| body.strip_suffix(s)) {
        body = b.trim_end();
    }
    body
}

type Parsed<T> = Result<(T, usize), ParseError>;

/// Разбор списка вызовов Python по байтам; `names` — имена, после которых уже
/// встретилась `(`.
struct Py<'a> {
    s: &'a str,
    names: Vec<&'a str>,
}

impl<'a> Py<'a> {
    fn byte(&self, pos: usize) -> Option<u8> {
        self.s.as_bytes().get(pos).copied()
    }

    fn ws(&self, pos: usize) -> usize {
        self.s.len() - self.s[pos..].trim_start().len()
    }

    /// Ожидаемый байт `b` в `pos`.
    fn expect(&self, pos: usize, b: u8) -> Result<usize, ParseError> {
        match self.byte(pos) {
            None => Err(ParseError::Eof),
            Some(x) if x == b => Ok(pos + 1),
            Some(_) => Err(ParseError::Syntax(pos)),
        }
    }

    /// `[` вызов (`,` вызов)* `,`? `]`.
    fn call_list(&mut self) -> Parsed<Vec<(String, Map<String, Value>)>> {
        let mut calls = Vec::new();
        let mut pos = self.expect(0, b'[')?;
        loop {
            pos = self.ws(pos);
            if self.byte(pos) == Some(b']') {
                return Ok((calls, pos + 1));
            }
            let (call, after) = self.call(pos)?;
            calls.push(call);
            pos = self.ws(after);
            match self.byte(pos) {
                None => return Err(ParseError::Eof),
                Some(b',') => pos += 1,
                Some(b']') => return Ok((calls, pos + 1)),
                Some(_) => return Err(ParseError::Syntax(pos)),
            }
        }
    }

    /// `имя(k=литерал, …)`.
    fn call(&mut self, pos: usize) -> Parsed<(String, Map<String, Value>)> {
        let (name, after) = self.ident(pos)?;
        let mut pos = self.expect(self.ws(after), b'(')?;
        self.names.push(name);
        let mut args = Map::new();
        loop {
            pos = self.ws(pos);
            if self.byte(pos) == Some(b')') {
                return Ok(((name.to_owned(), args), pos + 1));
            }
            let (key, after_key) = self.ident(pos)?;
            pos = self.expect(self.ws(after_key), b'=')?;
            let (value, after_value) = self.literal(self.ws(pos), 0)?;
            args.insert(key.to_owned(), value);
            pos = self.ws(after_value);
            match self.byte(pos) {
                None => return Err(ParseError::Eof),
                Some(b',') => pos += 1,
                Some(b')') => return Ok(((name.to_owned(), args), pos + 1)),
                Some(_) => return Err(ParseError::Syntax(pos)),
            }
        }
    }

    /// Идентификатор Python.
    fn ident(&self, pos: usize) -> Parsed<&'a str> {
        let rest = &self.s[pos..];
        let len = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if len == rest.len() {
            return Err(ParseError::Eof);
        }
        if len == 0 || rest.as_bytes()[0].is_ascii_digit() {
            return Err(ParseError::Syntax(pos));
        }
        Ok((&rest[..len], pos + len))
    }

    fn literal(&self, pos: usize, depth: usize) -> Parsed<Value> {
        if depth >= MAX_DEPTH {
            return Err(ParseError::Syntax(pos));
        }
        match self.byte(pos) {
            None => Err(ParseError::Eof),
            Some(q @ (b'"' | b'\'')) => self.string(pos, q).map(|(s, e)| (Value::String(s), e)),
            Some(b'[') => self.sequence(pos, b']', depth),
            Some(b'(') => self.sequence(pos, b')', depth),
            Some(b'{') => self.dict(pos, depth),
            Some(b'0'..=b'9' | b'-' | b'+' | b'.') => self.number(pos),
            Some(_) => {
                let (word, end) = self.ident(pos)?;
                let v = match word {
                    "True" => Value::Bool(true),
                    "False" => Value::Bool(false),
                    "None" => Value::Null,
                    _ => return Err(ParseError::Syntax(pos)),
                };
                Ok((v, end))
            }
        }
    }

    /// Список или кортеж до `close`.
    fn sequence(&self, pos: usize, close: u8, depth: usize) -> Parsed<Value> {
        let mut items = Vec::new();
        let mut pos = pos + 1;
        loop {
            pos = self.ws(pos);
            if self.byte(pos) == Some(close) {
                return Ok((Value::Array(items), pos + 1));
            }
            let (v, after) = self.literal(pos, depth + 1)?;
            items.push(v);
            pos = self.ws(after);
            match self.byte(pos) {
                None => return Err(ParseError::Eof),
                Some(b',') => pos += 1,
                Some(b) if b == close => return Ok((Value::Array(items), pos + 1)),
                Some(_) => return Err(ParseError::Syntax(pos)),
            }
        }
    }

    fn dict(&self, pos: usize, depth: usize) -> Parsed<Value> {
        let mut map = Map::new();
        let mut pos = pos + 1;
        loop {
            pos = self.ws(pos);
            if self.byte(pos) == Some(b'}') {
                return Ok((Value::Object(map), pos + 1));
            }
            let (k, after_key) = self.literal(pos, depth + 1)?;
            let key = match k {
                Value::String(s) => s,
                Value::Number(_) | Value::Bool(_) | Value::Null => k.to_string(),
                _ => return Err(ParseError::Syntax(pos)),
            };
            pos = self.expect(self.ws(after_key), b':')?;
            let (v, after) = self.literal(self.ws(pos), depth + 1)?;
            map.insert(key, v);
            pos = self.ws(after);
            match self.byte(pos) {
                None => return Err(ParseError::Eof),
                Some(b',') => pos += 1,
                Some(b'}') => return Ok((Value::Object(map), pos + 1)),
                Some(_) => return Err(ParseError::Syntax(pos)),
            }
        }
    }

    fn number(&self, pos: usize) -> Parsed<Value> {
        let rest = &self.s[pos..];
        let mut len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '+')))
            .unwrap_or(rest.len());
        if len == rest.len() {
            return Err(ParseError::Eof);
        }
        // Знак допускается только в начале и после экспоненты.
        let bytes = rest.as_bytes();
        for i in 1..len {
            if matches!(bytes[i], b'-' | b'+') && !matches!(bytes[i - 1], b'e' | b'E') {
                len = i;
                break;
            }
        }
        let raw: String = rest[..len].chars().filter(|c| *c != '_').collect();
        let raw = raw.strip_prefix('+').unwrap_or(&raw);
        let is_float = raw.contains(['.', 'e', 'E']);
        let n = if is_float {
            raw.parse::<f64>().ok().and_then(Number::from_f64)
        } else {
            raw.parse::<i64>()
                .map(Number::from)
                .ok()
                .or_else(|| raw.parse::<u64>().ok().map(Number::from))
        };
        match n {
            Some(n) => Ok((Value::Number(n), pos + len)),
            None => Err(ParseError::Syntax(pos)),
        }
    }

    /// Строка с кавычкой `q` в `pos` (тройная — многострочная).
    fn string(&self, pos: usize, q: u8) -> Parsed<String> {
        let bytes = self.s.as_bytes();
        let triple = bytes.get(pos + 1) == Some(&q) && bytes.get(pos + 2) == Some(&q);
        let mut i = if triple { pos + 3 } else { pos + 1 };
        let mut out = String::new();
        let mut run = i;
        loop {
            let Some(&b) = bytes.get(i) else {
                return Err(ParseError::Eof);
            };
            if b == q && (!triple || (bytes.get(i + 1) == Some(&q) && bytes.get(i + 2) == Some(&q)))
            {
                out.push_str(&self.s[run..i]);
                return Ok((out, i + if triple { 3 } else { 1 }));
            }
            if b == b'\n' && !triple {
                return Err(ParseError::Syntax(i));
            }
            if b != b'\\' {
                i += 1;
                continue;
            }
            out.push_str(&self.s[run..i]);
            let Some(&e) = bytes.get(i + 1) else {
                return Err(ParseError::Eof);
            };
            i += 2;
            match e {
                b'\n' => {}
                b'\\' | b'\'' | b'"' => out.push(e as char),
                b'n' => out.push('\n'),
                b't' => out.push('\t'),
                b'r' => out.push('\r'),
                b'0' => out.push('\0'),
                b'a' => out.push('\u{7}'),
                b'b' => out.push('\u{8}'),
                b'f' => out.push('\u{c}'),
                b'v' => out.push('\u{b}'),
                b'x' | b'u' | b'U' => {
                    let n = match e {
                        b'x' => 2,
                        b'u' => 4,
                        _ => 8,
                    };
                    let Some(hex) = self.s.get(i..i + n) else {
                        return Err(ParseError::Eof);
                    };
                    let c = u32::from_str_radix(hex, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or(ParseError::Syntax(i))?;
                    out.push(c);
                    i += n;
                }
                _ => {
                    // Неизвестное экранирование Python оставляет как есть.
                    out.push('\\');
                    i -= 1;
                }
            }
            run = i;
        }
    }
}
