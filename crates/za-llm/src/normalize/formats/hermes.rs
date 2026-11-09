//! Hermes (Hermes-2-Pro/Hermes-3, Qwen2.5, Qwen3):
//! `<tool_call>\n{"name": …, "arguments": {…}}\n</tool_call>`, несколько подряд.
//!
//! - Блок, внутри которого `<function=…>`, оставляется формату Qwen XML.
//! - Незакрытый блок тянется до следующего `<tool_call>` или конца текста
//!   (сервер мог остановиться на `</tool_call>` как стоп-строке).
//! - Внутри блока допускается несколько JSON-объектов подряд.
//! - `arguments` (или `parameters`): объект — `Json`, строка — `Text`, нет — `{}`.

use serde_json::{Map, Value};

use super::{Extraction, RawArgs, RawCall, find_from, splice};

const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";

pub(super) fn extract(text: &str) -> Extraction {
    let mut calls = Vec::new();
    let mut spans = Vec::new();
    let mut cur = 0;
    while let Some(start) = find_from(text, cur, OPEN) {
        let body_start = start + OPEN.len();
        let close = find_from(text, body_start, CLOSE);
        let next_open = find_from(text, body_start, OPEN);
        let (body_end, end) = match (close, next_open) {
            (Some(c), Some(o)) if o < c => (o, o),
            (Some(c), _) => (c, c + CLOSE.len()),
            (None, Some(o)) => (o, o),
            (None, None) => (text.len(), text.len()),
        };
        let body = text[body_start..body_end].trim();
        if body.starts_with("<function=") {
            // Qwen XML в обёртке `<tool_call>` — разбирает свой формат.
            cur = body_start;
            continue;
        }
        calls.extend(parse_body(body, &text[start..end]));
        spans.push((start, end));
        cur = end;
    }
    Extraction {
        remaining: splice(text, &spans),
        calls,
    }
}

/// Вызовы из тела блока; `raw` — разметка блока целиком для отклонения.
fn parse_body(body: &str, raw: &str) -> Vec<RawCall> {
    let values: Result<Vec<Value>, _> = serde_json::Deserializer::from_str(body)
        .into_iter::<Value>()
        .collect();
    match values {
        Ok(values) if !values.is_empty() => values.into_iter().map(call_from_value).collect(),
        _ => vec![RawCall::broken(raw)],
    }
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
