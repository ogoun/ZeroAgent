//! llama3_json (Llama 3.1–3.3, JSON-вызовы): необязательный `<|python_tag|>`,
//! затем весь ответ — `{"name": …, "parameters": {…}}`; несколько вызовов
//! разделяются `;`.
//!
//! Источники (сверено 2026-11-09): формат вендора `meta-llama/llama-models`
//! `models/llama3_1/prompt_format.md` (ответ с `<|python_tag|>`, завершение
//! `<|eom_id|>`/`<|eot_id|>`; JSON-вызов `{"name": function name, "parameters":
//! dictionary of argument name and its value}`); парсер vLLM
//! `vllm/parser/llama_json.py` (`_validate_complete_tool_output`: начало с
//! `<|python_tag|>` или `{`, объекты через `;`, у каждого `name` и `parameters` или
//! `arguments`, после — только пробелы и токены конца).
//!
//! - Формат без явных маркеров: разбирается только весь текст целиком. JSON внутри
//!   прозы — не вызов.
//! - Вызовы извлекаются, только если хотя бы одно имя принято (`accept_name`, D28);
//!   иначе текст не меняется. Объект без `name` и `parameters`/`arguments` — не вызов.
//! - Обрыв или синтаксическая ошибка JSON — отклонение всего текста, только если имя
//!   готового или оборванного объекта принято; иначе текст.
//! - Встроенные инструменты Llama 3.1 (`<|python_tag|>brave_search.call(…)`) и код
//!   после `<|python_tag|>` не разбираются (вне MDL-007).

use serde_json::Value;

use super::hermes::call_from_value;
use super::{Extraction, ParseError, RawCall, json_at, skip_ws};

const TAG: &str = "<|python_tag|>";
/// Токены конца ответа, допустимые после вызовов.
const ENDS: [&str; 3] = ["<|eom_id|>", "<|eot_id|>", "<|end_of_text|>"];

pub(super) fn extract(text: &str, accept_name: &dyn Fn(&str) -> bool) -> Extraction {
    let mut body = text.trim();
    if let Some(b) = body.strip_prefix(TAG) {
        body = b.trim_start();
    }
    while let Some(b) = ENDS.iter().find_map(|e| body.strip_suffix(e)) {
        body = b.trim_end();
    }
    if !body.starts_with('{') {
        return Extraction::unchanged(text);
    }
    let mut values: Vec<Value> = Vec::new();
    let mut pos = 0;
    let broken = loop {
        match json_at::<Value>(body, pos) {
            Ok((v, end)) => {
                values.push(v);
                pos = skip_ws(body, end);
                if pos == body.len() {
                    break false;
                }
                if !body[pos..].starts_with(';') {
                    // После вызова — проза: весь текст не вызов.
                    return Extraction::unchanged(text);
                }
                pos = skip_ws(body, pos + 1);
                if pos == body.len() {
                    break false;
                }
            }
            Err(_) => break true,
        }
    };
    let names: Vec<&str> = values.iter().filter_map(call_name).collect();
    if broken {
        let sniffed = sniff_name(body, pos);
        let accepted = names
            .iter()
            .copied()
            .chain(sniffed.as_deref())
            .any(accept_name);
        return if accepted {
            Extraction {
                calls: vec![RawCall::broken(text.trim())],
                remaining: String::new(),
            }
        } else {
            Extraction::unchanged(text)
        };
    }
    if names.len() != values.len() || !names.iter().any(|n| accept_name(n)) {
        return Extraction::unchanged(text);
    }
    Extraction {
        calls: values.into_iter().map(call_from_value).collect(),
        remaining: String::new(),
    }
}

/// Имя объекта формы вызова: строковое `name` и `parameters` или `arguments`.
fn call_name(v: &Value) -> Option<&str> {
    let obj = v.as_object()?;
    if !obj.contains_key("parameters") && !obj.contains_key("arguments") {
        return None;
    }
    obj.get("name")?.as_str()
}

/// Имя оборванного объекта, если он начинается с `{"name": "…"`.
fn sniff_name(body: &str, pos: usize) -> Option<String> {
    let p = skip_ws(body, pos);
    let p = skip_ws(body, p + body[p..].strip_prefix('{').map(|_| 1)?);
    let p = skip_ws(body, p + body[p..].strip_prefix("\"name\"").map(|_| 6)?);
    let p = skip_ws(body, p + body[p..].strip_prefix(':').map(|_| 1)?);
    match json_at::<String>(body, p) {
        Ok((name, _)) => Some(name),
        Err(ParseError::Eof | ParseError::Syntax(_)) => None,
    }
}
