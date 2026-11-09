//! Hermes (Hermes-2-Pro/Hermes-3, Qwen2.5, Qwen3):
//! `<tool_call>\n{"name": …, "arguments": {…}}\n</tool_call>`, несколько подряд.
//!
//! - Признак вызова: после `<tool_call>` и пробелов идёт `{`. Иначе тег — обычный
//!   текст (упоминание в прозе; Qwen XML в обёртке `<tool_call>` разбирает свой формат).
//! - JSON читается потоковым десериализатором от `{`, `</tool_call>` ищется только
//!   после конца JSON: закрывающий тег внутри строкового аргумента вызов не ломает.
//! - Внутри блока допускается несколько JSON-объектов подряд.
//! - Целый JSON без `</tool_call>` принимается (сервер мог остановиться на стоп-строке),
//!   текст после него остаётся текстом. Битый или обрезанный JSON — отклонение до
//!   ближайшего `</tool_call>` после места ошибки или до конца текста.
//! - `arguments` (или `parameters`): объект — `Json`, строка — `Text`, нет — `{}`.

use serde_json::{Map, Value};

use super::{CodeFences, Extraction, Finder, RawArgs, RawCall, splice};

const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";

pub(super) fn extract(text: &str) -> Extraction {
    let mut open = Finder::new(text, OPEN);
    let mut close = Finder::new(text, CLOSE);
    let mut fences = CodeFences::new(text);
    let mut calls = Vec::new();
    let mut spans = Vec::new();
    let mut cur = 0;
    while let Some(start) = open.find(cur) {
        if let Some(code_end) = fences.code_end(cur, start) {
            cur = code_end;
            continue;
        }
        let body_start = start + OPEN.len();
        let json_start = text.len() - text[body_start..].trim_start().len();
        if !text[json_start..].starts_with('{') {
            cur = body_start;
            continue;
        }
        let end = match parse_values(text, json_start) {
            Ok((values, json_end)) => {
                let after = &text[json_end..];
                let trimmed = after.trim_start();
                let end = if trimmed.starts_with(CLOSE) {
                    json_end + (after.len() - trimmed.len()) + CLOSE.len()
                } else {
                    json_end
                };
                calls.extend(values.into_iter().map(call_from_value));
                end
            }
            Err(error_at) => {
                let end = close.find(error_at).map_or(text.len(), |c| c + CLOSE.len());
                calls.push(RawCall::broken(&text[start..end]));
                end
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

/// JSON-объекты подряд от `from`: (значения, конец последнего) или позиция ошибки.
fn parse_values(text: &str, from: usize) -> Result<(Vec<Value>, usize), usize> {
    let mut values = Vec::new();
    let mut pos = from;
    loop {
        let slice = &text[pos..];
        let mut stream = serde_json::Deserializer::from_str(slice).into_iter::<Value>();
        match stream.next() {
            Some(Ok(v)) => {
                pos += stream.byte_offset();
                values.push(v);
            }
            Some(Err(e)) => return Err(pos + error_offset(slice, &e)),
            None => break,
        }
        let rest = &text[pos..];
        let trimmed = rest.trim_start();
        if !trimmed.starts_with('{') {
            break;
        }
        pos += rest.len() - trimmed.len();
    }
    Ok((values, pos))
}

/// Байтовая позиция ошибки `serde_json` (строка и столбец в байтах) в `slice`.
fn error_offset(slice: &str, e: &serde_json::Error) -> usize {
    if e.is_eof() {
        return slice.len();
    }
    let line_start = if e.line() <= 1 {
        0
    } else {
        slice
            .match_indices('\n')
            .nth(e.line() - 2)
            .map_or(slice.len(), |(i, _)| i + 1)
    };
    let mut at = (line_start + e.column()).min(slice.len());
    while !slice.is_char_boundary(at) {
        at += 1;
    }
    at
}

fn call_from_value(v: Value) -> RawCall {
    let Value::Object(mut obj) = v else {
        return RawCall::broken(&v.to_string());
    };
    let name = match obj.get("name") {
        Some(Value::String(n)) if !n.trim().is_empty() => n.trim().to_owned(),
        _ => return RawCall::broken(&Value::Object(obj).to_string()),
    };
    let args = obj.remove("arguments").or_else(|| obj.remove("parameters"));
    let args = match args {
        None | Some(Value::Null) => RawArgs::Json(Value::Object(Map::new())),
        Some(Value::String(s)) => RawArgs::Text(s),
        Some(v) => RawArgs::Json(v),
    };
    RawCall { name, args }
}
