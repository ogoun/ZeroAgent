//! Qwen XML (Qwen3-Coder, шаблон чата модели):
//! `<tool_call>\n<function=NAME>\n<parameter=KEY>\nVALUE\n</parameter>\n</function>\n</tool_call>`.
//!
//! - Признак вызова: `<function=ИМЯ>`, имя известно или исправимо (`accept_name`, D28),
//!   а тело после пробелов начинается с `<parameter=` или `</function>` (или обрывается).
//!   Иначе — обычный текст.
//! - Внешний `<tool_call>` необязателен; `</tool_call>` после `</function>` вырезается.
//! - У значения снимается по одному краевому переводу строки с каждой стороны
//!   (шаблон ставит `\n` после `>` и перед `</parameter>`).
//! - Пропущенный `</parameter>` терпится: значение до следующего `<parameter=` или
//!   `</function>`.
//! - Без `</function>` вызов считается обрезанным (значение могло оборваться,
//!   исполнять его нельзя) и отклоняется.
//! - Известное ограничение (как у парсера vLLM): значение, содержащее `</parameter>`
//!   или `<function=`, режется.

use super::{CodeFences, Extraction, Finder, RawArgs, RawCall, splice};

const OUTER_OPEN: &str = "<tool_call>";
const OUTER_CLOSE: &str = "</tool_call>";
const FN_OPEN: &str = "<function=";
const FN_CLOSE: &str = "</function>";
const P_OPEN: &str = "<parameter=";
const P_CLOSE: &str = "</parameter>";

pub(super) fn extract(text: &str, accept_name: &dyn Fn(&str) -> bool) -> Extraction {
    let mut fn_open = Finder::new(text, FN_OPEN);
    let mut fn_close = Finder::new(text, FN_CLOSE);
    let mut outer_close = Finder::new(text, OUTER_CLOSE);
    let mut fences = CodeFences::new(text);
    let mut calls = Vec::new();
    let mut spans = Vec::new();
    let mut cur = 0;
    while let Some(fstart) = fn_open.find(cur) {
        if let Some(code_end) = fences.code_end(cur, fstart) {
            cur = code_end;
            continue;
        }
        let name_start = fstart + FN_OPEN.len();
        let Some(name_end) = tag_end(text, name_start) else {
            cur = name_start;
            continue;
        };
        let name = text[name_start..name_end].trim();
        let body_start = name_end + 1;
        let lead = text[body_start..].trim_start();
        let looks_like_body = lead.starts_with(P_OPEN)
            || lead.starts_with(FN_CLOSE)
            // Обрыв посреди первого тега тела.
            || P_OPEN.starts_with(lead)
            || FN_CLOSE.starts_with(lead);
        if name.is_empty() || !looks_like_body || !accept_name(name) {
            cur = name_start;
            continue;
        }

        let before = text[cur..fstart].trim_end();
        let outer = before.ends_with(OUTER_OPEN);
        let start = if outer {
            cur + before.len() - OUTER_OPEN.len()
        } else {
            fstart
        };
        let next_fn = fn_open.find(name_start);
        let close = fn_close
            .find(body_start)
            .filter(|c| next_fn.is_none_or(|n| *c < n));
        let Some(close) = close else {
            // Обрезанный вызов: до `</tool_call>` (если была обёртка), следующего
            // `<function=` или конца текста.
            let limit = next_fn.unwrap_or(text.len());
            let end = match outer_close.find(body_start) {
                Some(c) if outer && c < limit => c + OUTER_CLOSE.len(),
                _ => limit,
            };
            calls.push(RawCall::broken(&text[start..end]));
            spans.push((start, end));
            cur = end;
            continue;
        };
        let mut end = close + FN_CLOSE.len();
        let tail = &text[end..];
        let tail_trimmed = tail.trim_start();
        if tail_trimmed.starts_with(OUTER_CLOSE) {
            end += tail.len() - tail_trimmed.len() + OUTER_CLOSE.len();
        }
        let call = match parse_params(&text[body_start..close]) {
            Some(pairs) => RawCall {
                name: name.to_owned(),
                args: RawArgs::Pairs(pairs),
            },
            None => RawCall::broken(&text[start..end]),
        };
        calls.push(call);
        spans.push((start, end));
        cur = end;
    }
    Extraction {
        remaining: splice(text, &spans),
        calls,
    }
}

/// Позиция `>`, закрывающего `<function=…` или `<parameter=…`; `None`, если раньше
/// встретился `<`, перевод строки или конец текста. Поиск останавливается на
/// первом из этих символов.
fn tag_end(text: &str, from: usize) -> Option<usize> {
    let rest = &text[from..];
    let i = rest.find(['>', '<', '\n'])?;
    (rest.as_bytes()[i] == b'>').then_some(from + i)
}

/// Параметры тела функции; `None` — битый заголовок параметра.
fn parse_params(body: &str) -> Option<Vec<(String, String)>> {
    let mut open = Finder::new(body, P_OPEN);
    let mut close = Finder::new(body, P_CLOSE);
    let mut pairs = Vec::new();
    let mut cur = 0;
    while let Some(p) = open.find(cur) {
        let key_start = p + P_OPEN.len();
        let key_end = tag_end(body, key_start)?;
        let key = body[key_start..key_end].trim();
        if key.is_empty() {
            return None;
        }
        let value_start = key_end + 1;
        let (value_end, after) = match (close.find(value_start), open.find(value_start)) {
            (Some(c), Some(n)) if n < c => (n, n),
            (Some(c), _) => (c, c + P_CLOSE.len()),
            (None, Some(n)) => (n, n),
            (None, None) => (body.len(), body.len()),
        };
        pairs.push((
            key.to_owned(),
            strip_edge_newlines(&body[value_start..value_end]).to_owned(),
        ));
        cur = after;
    }
    Some(pairs)
}

/// Снимает по одному переводу строки (`\n` или `\r\n`) с каждого края; остальное
/// содержимое значения сохраняется как есть.
fn strip_edge_newlines(v: &str) -> &str {
    let v = v
        .strip_prefix("\r\n")
        .or_else(|| v.strip_prefix('\n'))
        .unwrap_or(v);
    v.strip_suffix("\r\n")
        .or_else(|| v.strip_suffix('\n'))
        .unwrap_or(v)
}
