//! Hermes (Hermes-2-Pro/Hermes-3, Qwen2.5, Qwen3):
//! `<tool_call>\n{"name": …, "arguments": {…}}\n</tool_call>`, несколько подряд.
//!
//! - Признак вызова: после `<tool_call>` и пробелов идёт `{`. Иначе тег — обычный
//!   текст (упоминание в прозе; Qwen XML в обёртке `<tool_call>` разбирает свой формат).
//! - JSON читается потоковым десериализатором от `{`, `</tool_call>` ищется только
//!   после конца JSON: закрывающий тег внутри строкового аргумента вызов не ломает.
//! - Внутри блока допускается несколько JSON-объектов подряд.
//! - Целый JSON без `</tool_call>` принимается (сервер мог остановиться на стоп-строке),
//!   текст после него остаётся текстом.
//! - JSON оборвался концом текста — обрезанный вызов, отклонение до конца.
//! - Синтаксическая ошибка: если до следующего `<tool_call>` есть `</tool_call>` —
//!   битый блок, отклонение до него; иначе тег — обычный текст (`<tool_call>{...}` в
//!   прозе), ничего не вырезается (решение контроллера, раунд 2, R2).
//! - `arguments` (или `parameters`): объект — `Json`, строка — `Text`, нет — `{}`.

use serde_json::Value;

use super::{
    Extraction, Finder, ParseError, RawCall, call_from_value, json_at, qwen_xml, skip_ws, splice,
};

const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";

pub(super) fn extract(text: &str) -> Extraction {
    let mut open = Finder::new(text, OPEN);
    let mut close = Finder::new(text, CLOSE);
    let mut calls = Vec::new();
    let mut spans = Vec::new();
    let mut cur = 0;
    while let Some(start) = open.find(cur) {
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
            Err(ParseError::Eof) => {
                calls.push(RawCall::broken(&text[start..]));
                text.len()
            }
            Err(ParseError::Syntax(error_at)) => {
                let next_open = open.find(body_start);
                match close.find(error_at) {
                    Some(c) if next_open.is_none_or(|o| c < o) => {
                        let end = c + CLOSE.len();
                        calls.push(RawCall::broken(&text[start..end]));
                        end
                    }
                    _ => {
                        // Не вызов. Маркеры до места ошибки разбор уже прошёл как
                        // JSON — с них не начинать (линейность).
                        cur = error_at.max(body_start);
                        continue;
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

/// `<tool_call>` в `start` открывает вызов (для `code.rs`): после пробелов `{`
/// (Hermes) или `<function=` с признаком вызова Qwen XML.
pub(crate) fn opens_call(text: &str, start: usize) -> bool {
    let p = skip_ws(text, start + OPEN.len());
    text[p..].starts_with('{')
        || (text[p..].starts_with("<function=") && qwen_xml::opens_call(text, p))
}

/// JSON-объекты подряд от `from`: (значения, конец последнего) или ошибка.
fn parse_values(text: &str, from: usize) -> Result<(Vec<Value>, usize), ParseError> {
    let mut values = Vec::new();
    let mut pos = from;
    loop {
        let (v, end) = json_at::<Value>(text, pos)?;
        values.push(v);
        pos = end;
        let rest = &text[pos..];
        let trimmed = rest.trim_start();
        if !trimmed.starts_with('{') {
            break;
        }
        pos += rest.len() - trimmed.len();
    }
    Ok((values, pos))
}
