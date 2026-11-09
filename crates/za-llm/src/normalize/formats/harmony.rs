//! Harmony (gpt-oss, OpenAI «Harmony response format»):
//! `<|start|>assistant<|channel|>commentary to=functions.NAME <|constrain|>json<|message|>{…}<|call|>`.
//!
//! - Получатель `to=functions.NAME` может стоять и в разделе роли, до `<|channel|>`.
//! - Вырез начинается с `<|start|>` заголовка, иначе с `<|channel|>`, иначе с `to=`.
//! - Тело — после `<|message|>` до `<|call|>`, `<|end|>`, `<|return|>`, следующего
//!   `<|start|>` или конца текста; тело — JSON.
//! - `to=functions.` без маркеров Harmony рядом (обычная проза, текст внутри тела
//!   другого сообщения) вызовом не считается.
//! - Заголовок без `<|message|>` или тело не JSON — отклонение.

use serde_json::Value;

use super::{Extraction, RawArgs, RawCall, find_from, splice};

const TO: &str = "to=functions.";
const START: &str = "<|start|>";
const CHANNEL: &str = "<|channel|>";
const MESSAGE: &str = "<|message|>";
const END: &str = "<|end|>";
const CALL: &str = "<|call|>";
const RETURN: &str = "<|return|>";

pub(super) fn extract(text: &str) -> Extraction {
    let mut calls = Vec::new();
    let mut spans = Vec::new();
    let mut cur = 0;
    while let Some(p) = find_from(text, cur, TO) {
        // Заголовок начинается после конца предыдущего сообщения или его тела.
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

        let name_start = p + TO.len();
        let name_len = text[name_start..]
            .find(|c: char| c.is_whitespace() || c == '<')
            .unwrap_or(text.len() - name_start);
        let name = &text[name_start..name_start + name_len];
        let after_name = name_start + name_len;

        // `<|message|>` заголовка — до любого конца сообщения, нового `<|start|>` или
        // следующего получателя `to=functions.` (иначе упоминание в прозе захватило бы
        // чужой вызов).
        let stop = earliest(text, after_name, &[START, END, CALL, RETURN, TO]);
        let message =
            find_from(text, after_name, MESSAGE).filter(|m| stop.is_none_or(|(s, _)| *m < s));

        let Some(message) = message else {
            if header_start.is_none() {
                // Не заголовок Harmony.
                cur = after_name;
                continue;
            }
            // Обрезанный или битый заголовок.
            let start = header_start.unwrap_or(p);
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
        let start = header_start.unwrap_or(p);
        let body_start = message + MESSAGE.len();
        let (body_end, end) = match earliest(text, body_start, &[CALL, END, RETURN, START]) {
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

/// Ближайший из маркеров начиная с `from`: (позиция, маркер).
fn earliest(text: &str, from: usize, tokens: &[&'static str]) -> Option<(usize, &'static str)> {
    tokens
        .iter()
        .filter_map(|t| find_from(text, from, t).map(|i| (i, *t)))
        .min_by_key(|(i, _)| *i)
}
