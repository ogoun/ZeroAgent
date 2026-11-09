//! DSML (DeepSeek V3.2, V4):
//! `<｜DSML｜function_calls>\n<｜DSML｜invoke name="N">\n<｜DSML｜parameter name="K" string="true|false">V</｜DSML｜parameter>\n</｜DSML｜invoke>\n</｜DSML｜function_calls>`.
//!
//! Источники (сверено 2026-11-09): кодирующий скрипт вендора
//! `deepseek-ai/DeepSeek-V3.2/encoding/encoding_dsv32.py` (Hugging Face; шаблон
//! параметра, `string="true"` для строк, иначе JSON; значение без экранирования и без
//! краевых переводов строки); парсеры vLLM `vllm/parser/deepseek_v32.py` и
//! `deepseek_v4.py` (обёртка V4 — `<｜DSML｜tool_calls>`; `invoke` принимается и без
//! обёртки; без атрибута `string` значение пробуется как JSON, иначе строка; незакрытый
//! параметр — до следующего `<｜DSML｜parameter`). `｜` — U+FF5C.
//!
//! - Признак вызова: `<｜DSML｜invoke name="ИМЯ">`, тело после пробелов начинается
//!   с `<｜DSML｜parameter`, `</｜DSML｜invoke>` или закрытия обёртки (или обрывается
//!   на их префиксе). Иначе — обычный текст.
//! - Вызов кончается на `</｜DSML｜invoke>` или на закрытии обёртки, если оно раньше
//!   (как в vLLM), — оба должны стоять раньше следующего `invoke`. Полный вызов
//!   принимается при любом имени (неизвестное — `UnknownTool` с ближайшими).
//! - Без закрытия вызов обрезан: отклонение до следующего `invoke` или конца текста,
//!   только если имя принято (`accept_name`), иначе — текст.
//! - Обёртка (`function_calls` V3.2 или `tool_calls` V4, а также варианты открытия V4
//!   `toolcalls`, `tool` из vLLM) вырезается вместе с первым и последним вызовом, если
//!   стоит вплотную (через пробелы).
//! - `string="true"` — строка как есть; иначе JSON, при ошибке — строка (vLLM).
//! - Известное ограничение (как у vLLM): значение с `</｜DSML｜parameter>` или
//!   `<｜DSML｜parameter` режется.

use serde_json::{Map, Value};

use super::{Extraction, Finder, RawArgs, RawCall, earliest, splice};

pub(crate) const INVOKE: &str = "<｜DSML｜invoke name=\"";
pub(crate) const INVOKE_CLOSE: &str = "</｜DSML｜invoke>";
const PARAM: &str = "<｜DSML｜parameter";
const PARAM_CLOSE: &str = "</｜DSML｜parameter>";
/// Обёртки вызовов: V3.2 и V4.
const WRAPPERS: [(&str, &str); 2] = [
    ("<｜DSML｜function_calls>", "</｜DSML｜function_calls>"),
    ("<｜DSML｜tool_calls>", "</｜DSML｜tool_calls>"),
];
/// Открытия обёртки, которые вырезаются перед первым вызовом: V3.2, V4 и варианты
/// написания V4, встречавшиеся в работе (`DSML_TOOL_START_VARIANTS` vLLM).
const WRAPPER_OPENS: [&str; 4] = [
    WRAPPERS[0].0,
    WRAPPERS[1].0,
    "<｜DSML｜toolcalls>",
    "<｜DSML｜tool>",
];
/// Закрытия обёрток (для поиска тел в `code.rs`).
pub(crate) const WRAPPER_CLOSES: [&str; 2] = [WRAPPERS[0].1, WRAPPERS[1].1];

pub(super) fn extract(text: &str, accept_name: &dyn Fn(&str) -> bool) -> Extraction {
    let mut invoke = Finder::new(text, INVOKE);
    let mut invoke_close = Finder::new(text, INVOKE_CLOSE);
    let mut wrap_close = WRAPPER_CLOSES.map(|c| Finder::new(text, c));
    let mut calls = Vec::new();
    let mut spans = Vec::new();
    let mut cur = 0;
    while let Some(istart) = invoke.find(cur) {
        let name_start = istart + INVOKE.len();
        let Some((name, body_start)) = call_head(text, istart) else {
            cur = name_start;
            continue;
        };
        let before = text[cur..istart].trim_end();
        let start = WRAPPER_OPENS
            .iter()
            .find(|open| before.ends_with(*open))
            .map_or(istart, |open| cur + before.len() - open.len());
        let limit = invoke.find(name_start).unwrap_or(text.len());
        let close = invoke_close.find(body_start).filter(|c| *c < limit);
        let wclose = earliest(&mut wrap_close, body_start).filter(|(c, _)| *c < limit);
        let (body_end, end) = match (close, wclose) {
            (Some(c), None) => close_end(text, c),
            (Some(c), Some((wc, _))) if c < wc => close_end(text, c),
            (_, Some((wc, t))) => (wc, wc + t.len()),
            (None, None) => {
                if !accept_name(name) {
                    cur = name_start;
                    continue;
                }
                calls.push(RawCall::broken(&text[start..limit]));
                spans.push((start, limit));
                cur = limit;
                continue;
            }
        };
        let call = match parse_params(&text[body_start..body_end]) {
            Some(args) => RawCall {
                name: name.to_owned(),
                args: RawArgs::Json(Value::Object(args)),
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

/// Конец вызова с `</｜DSML｜invoke>` в `c`: (конец тела, конец выреза) — вместе с
/// закрытием обёртки, если оно идёт следом.
fn close_end(text: &str, c: usize) -> (usize, usize) {
    let mut end = c + INVOKE_CLOSE.len();
    let tail = &text[end..];
    let trimmed = tail.trim_start();
    if let Some(wc) = WRAPPER_CLOSES.iter().find(|wc| trimmed.starts_with(**wc)) {
        end += tail.len() - trimmed.len() + wc.len();
    }
    (c, end)
}

/// Признак вызова у `invoke` в `istart`: непустое имя и тело с параметра или
/// закрытия — (имя, начало тела).
fn call_head(text: &str, istart: usize) -> Option<(&str, usize)> {
    let (name, body_start) = header(text, istart + INVOKE.len())?;
    (!name.is_empty() && looks_like_body(&text[body_start..])).then_some((name, body_start))
}

/// `invoke` в `istart` открывает вызов (для `code.rs`).
pub(crate) fn opens_call(text: &str, istart: usize) -> bool {
    call_head(text, istart).is_some()
}

/// Имя вызова и начало тела: `ИМЯ">`. `None`, если до `"` встретился `<`, `>` или
/// перевод строки, или после `"` (и пробелов) нет `>`.
fn header(text: &str, from: usize) -> Option<(&str, usize)> {
    let rest = &text[from..];
    let q = rest.find(['"', '<', '>', '\n'])?;
    if rest.as_bytes()[q] != b'"' {
        return None;
    }
    let after = &rest[q + 1..];
    let lead = after.trim_start_matches([' ', '\t']);
    lead.starts_with('>').then(|| {
        (
            rest[..q].trim(),
            from + q + 1 + (after.len() - lead.len()) + 1,
        )
    })
}

/// Тело вызова начинается с параметра или закрытия (или обрывается на их префиксе).
fn looks_like_body(body: &str) -> bool {
    let lead = body.trim_start();
    [PARAM, INVOKE_CLOSE, WRAPPER_CLOSES[0], WRAPPER_CLOSES[1]]
        .iter()
        .any(|m| lead.starts_with(m) || m.starts_with(lead))
}

/// Параметры тела вызова; `None` — битый заголовок параметра.
fn parse_params(body: &str) -> Option<Map<String, Value>> {
    let mut open = Finder::new(body, PARAM);
    let mut close = Finder::new(body, PARAM_CLOSE);
    let mut args = Map::new();
    let mut cur = 0;
    while let Some(p) = open.find(cur) {
        let attrs_start = p + PARAM.len();
        let rest = &body[attrs_start..];
        let gt = rest.find(['>', '<'])?;
        if rest.as_bytes()[gt] != b'>' {
            return None;
        }
        let attrs = &rest[..gt];
        let key = attr(attrs, "name")?;
        if key.is_empty() {
            return None;
        }
        let is_string = attr(attrs, "string") == Some("true");
        let value_start = attrs_start + gt + 1;
        let (value_end, after) = match (close.find(value_start), open.find(value_start)) {
            (Some(c), Some(n)) if n < c => (n, n),
            (Some(c), _) => (c, c + PARAM_CLOSE.len()),
            (None, Some(n)) => (n, n),
            (None, None) => (body.len(), body.len()),
        };
        let raw = &body[value_start..value_end];
        let value = if is_string {
            Value::String(raw.to_owned())
        } else {
            serde_json::from_str(raw.trim()).unwrap_or_else(|_| Value::String(raw.to_owned()))
        };
        args.insert(key.to_owned(), value);
        cur = after;
    }
    Some(args)
}

/// Значение атрибута `key="…"` в заголовке параметра.
fn attr<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(i) = attrs[from..].find(key) {
        let at = from + i;
        let boundary = at == 0 || attrs.as_bytes()[at - 1].is_ascii_whitespace();
        let after = &attrs[at + key.len()..];
        if boundary && let Some(v) = after.strip_prefix("=\"") {
            return v.find('"').map(|e| &v[..e]);
        }
        from = at + key.len();
    }
    None
}
