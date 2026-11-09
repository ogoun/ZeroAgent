//! Qwen XML (Qwen3-Coder, шаблон чата модели):
//! `<tool_call>\n<function=NAME>\n<parameter=KEY>\nVALUE\n</parameter>\n</function>\n</tool_call>`.
//!
//! - Внешний `<tool_call>` необязателен; `</tool_call>` после `</function>` вырезается.
//! - У значения снимается по одному краевому переводу строки с каждой стороны
//!   (шаблон ставит `\n` после `>` и перед `</parameter>`).
//! - Пропущенный `</parameter>` терпится: значение до следующего `<parameter=` или
//!   `</function>`.
//! - Без `</function>` вызов считается обрезанным (значение могло оборваться,
//!   исполнять его нельзя) и отклоняется.
//! - `<function=` без `>` в обычном тексте вызовом не считается, если только не
//!   обрывает текст или не стоит внутри `<tool_call>`.

use super::{Extraction, RawArgs, RawCall, find_from, splice};

const OUTER_OPEN: &str = "<tool_call>";
const OUTER_CLOSE: &str = "</tool_call>";
const FN_OPEN: &str = "<function=";
const FN_CLOSE: &str = "</function>";
const P_OPEN: &str = "<parameter=";
const P_CLOSE: &str = "</parameter>";

pub(super) fn extract(text: &str) -> Extraction {
    let mut calls = Vec::new();
    let mut spans = Vec::new();
    let mut cur = 0;
    while let Some(fstart) = find_from(text, cur, FN_OPEN) {
        let before = text[cur..fstart].trim_end();
        let outer = before.ends_with(OUTER_OPEN);
        let start = if outer {
            cur + before.len() - OUTER_OPEN.len()
        } else {
            fstart
        };
        let name_start = fstart + FN_OPEN.len();
        let next_fn = find_from(text, name_start, FN_OPEN);

        let Some(name_end) = tag_end(text, name_start) else {
            // Имя без `>`: обрыв текста или битая обёртка — отклонение; иначе проза.
            let rest = &text[name_start..];
            if outer || !rest.contains(['<', '\n']) {
                let end = broken_end(text, name_start, outer, next_fn);
                calls.push(RawCall::broken(&text[start..end]));
                spans.push((start, end));
                cur = end;
            } else {
                cur = name_start;
            }
            continue;
        };
        let name = text[name_start..name_end].trim();
        let body_start = name_end + 1;

        let close =
            find_from(text, body_start, FN_CLOSE).filter(|c| next_fn.is_none_or(|n| *c < n));
        let Some(close) = close else {
            let end = broken_end(text, body_start, outer, next_fn);
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
            Some(pairs) if !name.is_empty() => RawCall {
                name: name.to_owned(),
                args: RawArgs::Pairs(pairs),
            },
            _ => RawCall::broken(&text[start..end]),
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
/// встретился `<`, перевод строки или конец текста.
fn tag_end(text: &str, from: usize) -> Option<usize> {
    let rest = &text[from..];
    let gt = rest.find('>')?;
    match rest.find(['<', '\n']) {
        Some(bad) if bad < gt => None,
        _ => Some(from + gt),
    }
}

/// Конец битого вызова: `</tool_call>` (если была обёртка), следующий `<function=`
/// или конец текста.
fn broken_end(text: &str, from: usize, outer: bool, next_fn: Option<usize>) -> usize {
    let limit = next_fn.unwrap_or(text.len());
    if outer && let Some(c) = find_from(text, from, OUTER_CLOSE).filter(|c| *c < limit) {
        return c + OUTER_CLOSE.len();
    }
    limit
}

/// Параметры тела функции; `None` — битый заголовок параметра.
fn parse_params(body: &str) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    let mut cur = 0;
    while let Some(p) = find_from(body, cur, P_OPEN) {
        let key_start = p + P_OPEN.len();
        let key_end = tag_end(body, key_start)?;
        let key = body[key_start..key_end].trim();
        if key.is_empty() {
            return None;
        }
        let value_start = key_end + 1;
        let close = find_from(body, value_start, P_CLOSE);
        let next = find_from(body, value_start, P_OPEN);
        let (value_end, after) = match (close, next) {
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
