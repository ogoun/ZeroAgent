//! Harmony (gpt-oss, OpenAI «Harmony response format»):
//! `<|start|>assistant<|channel|>commentary to=functions.NAME <|constrain|>json<|message|>{…}<|call|>`.
//!
//! - Получатель `to=functions.NAME` может стоять и в разделе роли, до `<|channel|>`.
//! - Вырез начинается с `<|start|>` заголовка, иначе с `<|channel|>`, иначе с `to=`.
//! - Тело — после `<|message|>` до `<|call|>`, `<|end|>`, `<|return|>`, следующего
//!   `<|start|>` или конца текста; тело — JSON, иначе отклонение.
//! - `<|message|>` заголовка ищется до конца сообщения, нового `<|start|>` или
//!   следующего `to=functions.`: упоминание в прозе не захватывает чужой вызов.
//! - `to=functions.` без маркеров Harmony (`<|start|>`/`<|channel|>` перед ним или
//!   `<|message|>` после) — обычный текст.
//! - Заголовок без `<|message|>` — отклонение, только если он похож на обрезанный
//!   вызов: имя принято (`accept_name`), а остаток заголовка короткий (≤ 64 байт) и
//!   без перевода строки. Иначе — текст.
//! - Известное ограничение: ответ сервера с `skip_special_tokens` (маркеры вырезаны)
//!   не распознаётся.

use serde_json::Value;

use super::{CodeFences, Extraction, Finder, RawArgs, RawCall, earliest, splice};

const TO: &str = "to=functions.";
const START: &str = "<|start|>";
const CHANNEL: &str = "<|channel|>";
const MESSAGE: &str = "<|message|>";
const END: &str = "<|end|>";
const CALL: &str = "<|call|>";
const RETURN: &str = "<|return|>";

/// Наибольшая длина остатка обрезанного заголовка после имени.
const MAX_HEADER_TAIL: usize = 64;

pub(super) fn extract(text: &str, accept_name: &dyn Fn(&str) -> bool) -> Extraction {
    let mut to = Finder::new(text, TO);
    let mut message = Finder::new(text, MESSAGE);
    let mut header_stops = [
        Finder::new(text, START),
        Finder::new(text, END),
        Finder::new(text, CALL),
        Finder::new(text, RETURN),
        Finder::new(text, TO),
    ];
    let mut body_stops = [
        Finder::new(text, CALL),
        Finder::new(text, END),
        Finder::new(text, RETURN),
        Finder::new(text, START),
    ];
    let mut fences = CodeFences::new(text);
    let mut calls = Vec::new();
    let mut spans = Vec::new();
    let mut cur = 0;
    while let Some(p) = to.find(cur) {
        // Заголовок начинается после конца предыдущего сообщения или его тела.
        // Области `[cur, p)` разных итераций не пересекаются — поиск линейный.
        let region = &text[cur..p];
        let boundary = cur
            + [MESSAGE, END, CALL, RETURN]
                .iter()
                .filter_map(|t| region.rfind(t).map(|i| i + t.len()))
                .max()
                .unwrap_or(0);
        let head = &text[boundary..p];
        let header_start = head
            .rfind(START)
            .or_else(|| head.rfind(CHANNEL))
            .map(|i| boundary + i);
        let start = header_start.unwrap_or(p);
        if let Some(code_end) = fences.code_end(cur, start) {
            cur = code_end.max(p + 1);
            continue;
        }

        let name_start = p + TO.len();
        let name_len = text[name_start..]
            .find(|c: char| c.is_whitespace() || c == '<')
            .unwrap_or(text.len() - name_start);
        let name = &text[name_start..name_start + name_len];
        let after_name = name_start + name_len;

        let stop = earliest(&mut header_stops, after_name);
        let msg = message
            .find(after_name)
            .filter(|m| stop.is_none_or(|(s, _)| *m < s));

        let Some(msg) = msg else {
            let tail_end = stop.map_or(text.len(), |(s, _)| s);
            let tail = &text[after_name..tail_end];
            let truncated = header_start.is_some()
                && !name.is_empty()
                && tail.len() <= MAX_HEADER_TAIL
                && !tail.contains('\n')
                && accept_name(name);
            if !truncated {
                cur = after_name;
                continue;
            }
            let end = match stop {
                Some((s, t)) if t == START || t == TO => s,
                Some((s, t)) => s + t.len(),
                None => text.len(),
            };
            calls.push(RawCall::broken(&text[start..end]));
            spans.push((start, end));
            cur = end;
            continue;
        };
        let body_start = msg + MESSAGE.len();
        let (body_end, end) = match earliest(&mut body_stops, body_start) {
            Some((s, t)) if t == START => (s, s),
            Some((s, t)) => (s, s + t.len()),
            None => (text.len(), text.len()),
        };
        let body = text[body_start..body_end].trim();
        let call = match serde_json::from_str::<Value>(body) {
            Ok(v) if !name.is_empty() => RawCall {
                name: name.to_owned(),
                args: RawArgs::Json(v),
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
