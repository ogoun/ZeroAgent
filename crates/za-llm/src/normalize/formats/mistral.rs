//! Mistral `[TOOL_CALLS]`: `[TOOL_CALLS]NAME[ARGS]{json}` (токенизатор v13+),
//! `[TOOL_CALLS]NAME[CALL_ID]ID[ARGS]{json}` (v11) и старый вид
//! `[TOOL_CALLS][{"name": …, "arguments": {…}}]` (v2–v7).
//!
//! Источники (сверено 2026-11-09): кодировщик вендора `mistral-common`
//! `src/mistral_common/tokens/tokenizers/instruct.py` (`InstructTokenizerV2`,
//! `V11`, `V13`: `_encode_tool_calls_in_assistant_message`; аргументы —
//! `json.dumps`, каждый вызов со своим `[TOOL_CALLS]`); парсер vLLM
//! `vllm/parser/mistral.py` (`NAME{json}` без `[ARGS]` тоже принимается; после JSON
//! может идти обычный текст; в старом виде одиночный объект — один вызов, элементы
//! без `name` и `arguments` пропускаются).
//!
//! - Признак вызова: после `[TOOL_CALLS]` (и пробелов) имя, затем `[CALL_ID]ID`
//!   (необязательно), `[ARGS]` (необязательно) и `{`; или `[{` / `{` старого вида.
//!   Иначе маркер — обычный текст.
//! - JSON читается потоковым десериализатором; текст после него остаётся текстом.
//!   Полный вызов принимается при любом имени.
//! - Обрыв: старый вид — отклонение до конца; новый — отклонение до конца, только если
//!   имя принято (`accept_name`), иначе текст.
//! - Обрыв на имени (`[TOOL_CALLS]ИМЯ` в конце текста) считается вызовом, только если
//!   имя стоит вплотную к маркеру, как в выводе модели; `[TOOL_CALLS] read_file` в
//!   прозе — текст.
//! - Синтаксическая ошибка JSON — текст (правило R2 задачи 7: закрывающего маркера у
//!   формата нет; раунд исправлений 1).

use serde::de::IgnoredAny;
use serde_json::Value;

use super::{
    Extraction, Finder, ParseError, RawArgs, RawCall, call_from_value, json_at, name_len, skip_ws,
    splice,
};

const MARK: &str = "[TOOL_CALLS]";
const ARGS: &str = "[ARGS]";
const CALL_ID: &str = "[CALL_ID]";

/// Заголовок после `[TOOL_CALLS]`.
enum Head<'a> {
    /// Старый вид: JSON (массив или объект) с этой позиции.
    Legacy(usize),
    /// Новый вид: имя и начало JSON аргументов.
    Named(&'a str, usize),
    /// Новый вид, текст кончился до JSON.
    Truncated(&'a str),
}

pub(super) fn extract(text: &str, accept_name: &dyn Fn(&str) -> bool) -> Extraction {
    let mut mark = Finder::new(text, MARK);
    let mut calls = Vec::new();
    let mut spans = Vec::new();
    let mut cur = 0;
    while let Some(start) = mark.find(cur) {
        let after = start + MARK.len();
        let Some(head) = head(text, after) else {
            cur = after;
            continue;
        };
        let end = match head {
            Head::Truncated(name) => {
                if !accept_name(name) {
                    cur = after;
                    continue;
                }
                calls.push(RawCall::broken(&text[start..]));
                text.len()
            }
            Head::Named(name, js) => match json_at::<Value>(text, js) {
                Ok((v, end)) => {
                    let args = match v {
                        Value::String(s) => RawArgs::Text(s),
                        v => RawArgs::Json(v),
                    };
                    calls.push(RawCall {
                        name: name.to_owned(),
                        args,
                    });
                    end
                }
                Err(ParseError::Eof) => {
                    if !accept_name(name) {
                        break;
                    }
                    calls.push(RawCall::broken(&text[start..]));
                    text.len()
                }
                Err(ParseError::Syntax(error_at)) => {
                    cur = error_at.max(after);
                    continue;
                }
            },
            Head::Legacy(js) => match json_at::<Value>(text, js) {
                Ok((v, end)) => {
                    let items = match v {
                        Value::Array(items) => items,
                        v => vec![v],
                    };
                    let found: Vec<RawCall> = items
                        .into_iter()
                        .filter(|i| i.get("name").is_some() || i.get("arguments").is_some())
                        .map(call_from_value)
                        .collect();
                    if found.is_empty() {
                        cur = end;
                        continue;
                    }
                    calls.extend(found);
                    end
                }
                Err(ParseError::Eof) => {
                    calls.push(RawCall::broken(&text[start..]));
                    text.len()
                }
                Err(ParseError::Syntax(error_at)) => {
                    cur = error_at.max(after);
                    continue;
                }
            },
        };
        spans.push((start, end));
        cur = end;
    }
    Extraction {
        remaining: splice(text, &spans),
        calls,
    }
}

/// Заголовок вызова после маркера; `None` — маркер без признака вызова.
fn head(text: &str, after: usize) -> Option<Head<'_>> {
    let p = skip_ws(text, after);
    let rest = &text[p..];
    if rest.starts_with('{') {
        return Some(Head::Legacy(p));
    }
    if rest.starts_with('[') {
        let q = skip_ws(text, p + 1);
        return text[q..].starts_with('{').then_some(Head::Legacy(p));
    }
    let name_end = p + name_len(text, p);
    let name = &text[p..name_end];
    if name.is_empty() {
        return None;
    }
    let mut q = name_end;
    if text[q..].starts_with(CALL_ID) {
        q += CALL_ID.len();
        q += name_len(text, q);
    }
    let rest = &text[q..];
    // Обрыв на имени засчитывается, только если имя вплотную к маркеру.
    let truncated = (p == after).then_some(Head::Truncated(name));
    if rest.starts_with(ARGS) {
        q = skip_ws(text, q + ARGS.len());
    } else if !rest.starts_with('{') {
        // Обрыв на `[ARGS]` или `[CALL_ID]`.
        let cut = !rest.is_empty() && (ARGS.starts_with(rest) || CALL_ID.starts_with(rest));
        return if rest.is_empty() || cut {
            truncated
        } else {
            None
        };
    }
    if q == text.len() {
        return truncated;
    }
    text[q..].starts_with('{').then_some(Head::Named(name, q))
}

/// Тела вызовов (от маркера до конца JSON) в исходном тексте — для `code.rs`.
pub(crate) fn bodies(text: &str) -> Vec<(usize, usize)> {
    let mut mark = Finder::new(text, MARK);
    let mut out = Vec::new();
    let mut cur = 0;
    while let Some(start) = mark.find(cur) {
        let after = start + MARK.len();
        let js = match head(text, after) {
            Some(Head::Legacy(js) | Head::Named(_, js)) => js,
            _ => {
                cur = after;
                continue;
            }
        };
        match json_at::<IgnoredAny>(text, js) {
            Ok((_, end)) => {
                out.push((start, end));
                cur = end;
            }
            Err(ParseError::Eof) => break,
            Err(ParseError::Syntax(error_at)) => cur = error_at.max(after),
        }
    }
    out
}
