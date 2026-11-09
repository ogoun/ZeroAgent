//! Gemma 4: `<|tool_call>call:NAME{key:<|"|>строка<|"|>,num:42}<tool_call|>`.
//!
//! Источники (сверено 2026-11-09): шаблон чата `google/gemma-4-31B-it`
//! `chat_template.jinja` (Hugging Face; макрос `format_argument`: строки в `<|"|>`,
//! `true`/`false`/`null`, числа как есть, объекты `{k:v}` и списки `[v,…]`, ключи
//! аргументов без кавычек, вложенные ключи по умолчанию в `<|"|>`); парсер vLLM
//! `vllm/parser/gemma4.py` и `vllm/tool_parsers/gemma4_utils.py` (закрывающим может
//! быть `<turn|>`; часть чекпойнтов пишет `<|tool_call>:NAME{…}` без `call`).
//!
//! - Признак вызова: `<|tool_call>`, затем `call:` (или `:`), непустое имя и `{`.
//!   Иначе — обычный текст.
//! - Аргументы разбираются от `{` до парной `}`; закрывающий тег ищется только после
//!   них (тег внутри строки `<|"|>…<|"|>` вызов не ломает). Целые аргументы без
//!   закрывающего тега принимаются (сервер мог остановиться на стоп-строке).
//! - Обрыв (конец текста посреди имени или аргументов) — отклонение до конца, если
//!   имя принято (`accept_name`), иначе текст.
//! - Синтаксическая ошибка: если до следующего `<|tool_call>` есть закрывающий тег —
//!   отклонение до него; иначе текст (как Hermes).
//! - Значения без кавычек — до `,`, `}` или `]` (как vLLM): `true`, `false`, `null` и
//!   числа — JSON, прочее — строка (типы приводит схема).
//! - Известное ограничение (как у vLLM): строка, содержащая `<|"|>`, режется.

use serde_json::{Map, Value};

use super::{
    Extraction, Finder, ParseError, RawArgs, RawCall, earliest, name_len, skip_ws, splice,
};

pub(crate) const OPEN: &str = "<|tool_call>";
pub(crate) const CLOSE: &str = "<tool_call|>";
pub(crate) const TURN: &str = "<turn|>";
const QUOTE: &str = "<|\"|>";
/// Предел вложенности объектов и списков в аргументах.
const MAX_DEPTH: usize = 64;

pub(super) fn extract(text: &str, accept_name: &dyn Fn(&str) -> bool) -> Extraction {
    let mut open = Finder::new(text, OPEN);
    let mut closes = [Finder::new(text, CLOSE), Finder::new(text, TURN)];
    let mut calls = Vec::new();
    let mut spans = Vec::new();
    let mut cur = 0;
    while let Some(start) = open.find(cur) {
        let after = start + OPEN.len();
        let Some((name, brace)) = head(text, after) else {
            cur = after;
            continue;
        };
        let end = if brace == text.len() {
            // Обрыв на имени.
            if !accept_name(name) {
                break;
            }
            calls.push(RawCall::broken(&text[start..]));
            text.len()
        } else if !text[brace..].starts_with('{') {
            cur = after;
            continue;
        } else {
            match Args::new(text).object(brace, 0) {
                Ok((args, args_end)) => {
                    let tail = &text[args_end..];
                    let trimmed = tail.trim_start();
                    let end = match [CLOSE, TURN].iter().find(|t| trimmed.starts_with(*t)) {
                        Some(t) => args_end + (tail.len() - trimmed.len()) + t.len(),
                        None => args_end,
                    };
                    calls.push(RawCall {
                        name: name.to_owned(),
                        args: RawArgs::Json(Value::Object(args)),
                    });
                    end
                }
                Err(ParseError::Eof) => {
                    // Разбор дошёл до конца текста: следующие маркеры он уже прошёл.
                    if !accept_name(name) {
                        break;
                    }
                    calls.push(RawCall::broken(&text[start..]));
                    text.len()
                }
                Err(ParseError::Syntax(error_at)) => {
                    let next_open = open.find(after);
                    match earliest(&mut closes, error_at) {
                        Some((c, t)) if next_open.is_none_or(|o| c < o) => {
                            let end = c + t.len();
                            calls.push(RawCall::broken(&text[start..end]));
                            end
                        }
                        _ => {
                            cur = error_at.max(after);
                            continue;
                        }
                    }
                }
            }
        };
        spans.push((start, end));
        cur = end;
    }
    Extraction {
        remaining: splice(text, &spans),
        calls,
    }
}

/// Заголовок после `<|tool_call>` (в `after`): `call:` или `:` и непустое имя —
/// (имя, позиция после имени).
fn head(text: &str, after: usize) -> Option<(&str, usize)> {
    let lead = skip_ws(text, after);
    let rest = &text[lead..];
    let name_start = if rest.starts_with("call:") {
        lead + "call:".len()
    } else if rest.starts_with(':') {
        lead + 1
    } else {
        return None;
    };
    let end = name_start + name_len(text, name_start);
    (end > name_start).then(|| (&text[name_start..end], end))
}

/// `<|tool_call>` в `start` открывает вызов: заголовок и `{` (для `code.rs`).
pub(crate) fn opens_call(text: &str, start: usize) -> bool {
    head(text, start + OPEN.len()).is_some_and(|(_, brace)| text[brace..].starts_with('{'))
}

/// Разбор аргументов Gemma 4 по байтам текста.
struct Args<'a> {
    s: &'a str,
}

type Parsed<T> = Result<(T, usize), ParseError>;

impl<'a> Args<'a> {
    fn new(s: &'a str) -> Self {
        Self { s }
    }

    fn byte(&self, pos: usize) -> Option<u8> {
        self.s.as_bytes().get(pos).copied()
    }

    /// Объект `{k:v,…}` с `{` в `pos`: (объект, позиция после `}`).
    fn object(&self, pos: usize, depth: usize) -> Parsed<Map<String, Value>> {
        if depth >= MAX_DEPTH {
            return Err(ParseError::Syntax(pos));
        }
        let mut map = Map::new();
        let mut pos = pos + 1;
        loop {
            pos = skip_ws(self.s, pos);
            match self.byte(pos) {
                None => return Err(ParseError::Eof),
                Some(b'}') => return Ok((map, pos + 1)),
                _ => {}
            }
            let (key, after_key) = self.key(pos)?;
            pos = skip_ws(self.s, after_key + 1);
            let (value, after_value) = self.value(pos, depth)?;
            map.insert(key, value);
            pos = skip_ws(self.s, after_value);
            match self.byte(pos) {
                None => return Err(ParseError::Eof),
                Some(b',') => pos += 1,
                Some(b'}') => return Ok((map, pos + 1)),
                Some(_) => return Err(ParseError::Syntax(pos)),
            }
        }
    }

    /// Ключ до `:`: (ключ, позиция `:`). Ключ — в `<|"|>` или без кавычек.
    fn key(&self, pos: usize) -> Parsed<String> {
        if self.s[pos..].starts_with(QUOTE) {
            let (key, after) = self.string(pos)?;
            let colon = skip_ws(self.s, after);
            return match self.byte(colon) {
                None => Err(ParseError::Eof),
                Some(b':') => Ok((key, colon)),
                Some(_) => Err(ParseError::Syntax(colon)),
            };
        }
        let rest = &self.s[pos..];
        let Some(i) = rest.find([':', ',', '{', '}', '[', ']', '<']) else {
            return Err(ParseError::Eof);
        };
        let key = rest[..i].trim();
        if rest.as_bytes()[i] != b':' || key.is_empty() {
            return Err(ParseError::Syntax(pos + i));
        }
        Ok((key.to_owned(), pos + i))
    }

    /// Строка `<|"|>…<|"|>` с открывающей кавычкой в `pos`.
    fn string(&self, pos: usize) -> Parsed<String> {
        let from = pos + QUOTE.len();
        match self.s[from..].find(QUOTE) {
            Some(i) => Ok((self.s[from..from + i].to_owned(), from + i + QUOTE.len())),
            None => Err(ParseError::Eof),
        }
    }

    fn value(&self, pos: usize, depth: usize) -> Parsed<Value> {
        match self.byte(pos) {
            None => Err(ParseError::Eof),
            Some(b'{') => self
                .object(pos, depth + 1)
                .map(|(m, e)| (Value::Object(m), e)),
            Some(b'[') => self.array(pos, depth + 1),
            Some(_) if self.s[pos..].starts_with(QUOTE) => {
                self.string(pos).map(|(s, e)| (Value::String(s), e))
            }
            Some(_) => self.bare(pos),
        }
    }

    fn array(&self, pos: usize, depth: usize) -> Parsed<Value> {
        if depth >= MAX_DEPTH {
            return Err(ParseError::Syntax(pos));
        }
        let mut items = Vec::new();
        let mut pos = pos + 1;
        loop {
            pos = skip_ws(self.s, pos);
            match self.byte(pos) {
                None => return Err(ParseError::Eof),
                Some(b']') => return Ok((Value::Array(items), pos + 1)),
                _ => {}
            }
            let (value, after) = self.value(pos, depth)?;
            items.push(value);
            pos = skip_ws(self.s, after);
            match self.byte(pos) {
                None => return Err(ParseError::Eof),
                Some(b',') => pos += 1,
                Some(b']') => return Ok((Value::Array(items), pos + 1)),
                Some(_) => return Err(ParseError::Syntax(pos)),
            }
        }
    }

    /// Значение без кавычек до `,`, `}` или `]`.
    fn bare(&self, pos: usize) -> Parsed<Value> {
        let rest = &self.s[pos..];
        let Some(i) = rest.find([',', '}', ']']) else {
            return Err(ParseError::Eof);
        };
        let raw = rest[..i].trim();
        if raw.is_empty() {
            return Err(ParseError::Syntax(pos + i));
        }
        let value = match serde_json::from_str::<Value>(raw) {
            Ok(v @ (Value::Bool(_) | Value::Null | Value::Number(_))) => v,
            _ => Value::String(raw.to_owned()),
        };
        Ok((value, pos + i))
    }
}
