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
//! - Вызов — объект, все ключи которого из `name`, `parameters`, `arguments`:
//!   строковое `name` и одно из двух других обязательны. Объект с другими ключами
//!   (описание инструмента OpenAI, сериализованный `ToolSchema` с `description`) —
//!   не вызов, весь текст остаётся текстом (раунд исправлений 1, Important 1).
//! - Вызовы извлекаются, только если хотя бы одно имя принято (`accept_name`, D28);
//!   иначе текст не меняется.
//! - Обрыв JSON концом текста — отклонение всего текста, только если имя готового
//!   или оборванного объекта принято. Синтаксическая ошибка — текст (правило R2
//!   задачи 7, как vLLM).
//! - Встроенные инструменты Llama 3.1 (`<|python_tag|>brave_search.call(…)`) и код
//!   после `<|python_tag|>` не разбираются (вне MDL-007).

use serde_json::Value;

use super::{
    Extraction, ParseError, RawCall, call_from_value, json_at, skip_ws, strip_edge_tokens,
};

const TAG: &str = "<|python_tag|>";
/// Токены конца ответа, допустимые после вызовов.
const ENDS: [&str; 3] = ["<|eom_id|>", "<|eot_id|>", "<|end_of_text|>"];
/// Допустимые ключи объекта вызова.
const CALL_KEYS: [&str; 3] = ["name", "parameters", "arguments"];

pub(super) fn extract(text: &str, accept_name: &dyn Fn(&str) -> bool) -> Extraction {
    let body = strip_edge_tokens(text, &[TAG], &ENDS);
    if !body.starts_with('{') {
        return Extraction::unchanged(text);
    }
    let mut values: Vec<Value> = Vec::new();
    let mut pos = 0;
    loop {
        match json_at::<Value>(body, pos) {
            Ok((v, end)) => {
                values.push(v);
                pos = skip_ws(body, end);
                if pos == body.len() {
                    break;
                }
                if !body[pos..].starts_with(';') {
                    // После вызова — проза: весь текст не вызов.
                    return Extraction::unchanged(text);
                }
                pos = skip_ws(body, pos + 1);
                if pos == body.len() {
                    break;
                }
            }
            Err(ParseError::Eof) => {
                // Обрыв: отклонение, если имя готового или оборванного объекта принято.
                let sniffed = sniff_name(body, pos);
                let names = values.iter().filter_map(call_name);
                if names.chain(sniffed.as_deref()).any(accept_name) {
                    return Extraction {
                        calls: vec![RawCall::broken(text.trim())],
                        remaining: String::new(),
                    };
                }
                return Extraction::unchanged(text);
            }
            Err(ParseError::Syntax(_)) => return Extraction::unchanged(text),
        }
    }
    let names: Vec<&str> = values.iter().filter_map(call_name).collect();
    if names.len() != values.len() || !names.iter().any(|n| accept_name(n)) {
        return Extraction::unchanged(text);
    }
    Extraction {
        calls: values.into_iter().map(call_from_value).collect(),
        remaining: String::new(),
    }
}

/// Имя объекта формы вызова: только ключи [`CALL_KEYS`], строковое `name` и
/// `parameters` или `arguments`.
fn call_name(v: &Value) -> Option<&str> {
    let obj = v.as_object()?;
    if !obj.keys().all(|k| CALL_KEYS.contains(&k.as_str()))
        || !(obj.contains_key("parameters") || obj.contains_key("arguments"))
    {
        return None;
    }
    obj.get("name")?.as_str()
}

/// Имя оборванного объекта, если он начинается с `{"name": "…"` и дальше идёт конец
/// текста или ключ `parameters`/`arguments` (возможно, оборванный). Оборванное
/// описание инструмента (`"description"` после имени) — не вызов.
fn sniff_name(body: &str, pos: usize) -> Option<String> {
    let p = skip_ws(body, pos);
    let p = skip_ws(body, p + body[p..].strip_prefix('{').map(|_| 1)?);
    let p = skip_ws(body, p + body[p..].strip_prefix("\"name\"").map(|_| 6)?);
    let p = skip_ws(body, p + body[p..].strip_prefix(':').map(|_| 1)?);
    let (name, end) = json_at::<String>(body, p).ok()?;
    let p = skip_ws(body, end);
    if p == body.len() {
        return Some(name);
    }
    let p = skip_ws(body, p + body[p..].strip_prefix(',').map(|_| 1)?);
    let rest = &body[p..];
    ["\"parameters\"", "\"arguments\""]
        .iter()
        .any(|k| rest.starts_with(k) || k.starts_with(rest))
        .then_some(name)
}
