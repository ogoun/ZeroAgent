//! Код Markdown в тексте ответа не сканируется на вызовы (решения контроллера по
//! ревью задачи 7: I2, раунд 2 — Minor B и C).
//!
//! Области кода вычисляются один раз по исходному тексту, до вырезания вызовов, и
//! заменяются заглушками `U+E000 <номер> U+E001`. Форматы видят текст с заглушками:
//! маркеры внутри кода им не видны, а вырезание вызовов не сдвигает области — они
//! едут вместе с текстом. После разбора заглушки возвращаются в оставшийся текст и
//! в аргументы извлечённых вызовов (значение Qwen XML с блоком кода, строка JSON с
//! `` `x` ``).
//!
//! Области:
//! - блок с забором ```` ``` ```` или `~~~` (от трёх символов) в начале строки, отступ
//!   до трёх пробелов; закрывает строка из того же символа не короче открывающей и
//!   без другого текста; незакрытый блок — до конца текста;
//! - инлайн-код: серия обратных кавычек и следующая серия той же длины в той же
//!   строке (CommonMark); непарная серия — обычный текст;
//! - символы `U+E000`/`U+E001` самого текста — чтобы заглушки нельзя было подделать.
//!
//! Заборы и обратные кавычки внутри тел вызовов (блок `<tool_call>…</tool_call>`,
//! `<function=…>…</function>`, вызов Harmony `to=functions.…<|message|>…<|call|>`
//! или `…<|end|>`; сообщения обычных каналов — не тела вызовов) разметкой
//! Markdown не считаются: нечётный забор в значении `write_file` иначе спарился бы
//! с забором примера после вызова и открыл бы пример для исполнения. Тела ищутся по
//! маркерам по исходному тексту: закрытие должно стоять раньше следующего открытия
//! того же вида.
//!
//! Работа линейна по длине текста.

use std::collections::HashMap;

use serde_json::Value;

use super::formats::{Finder, RawArgs, RawCall, earliest};

const OPEN: char = '\u{E000}';
const CLOSE: char = '\u{E001}';

/// Текст с заглушками вместо областей кода.
pub(crate) struct Masked {
    pub(crate) text: String,
    regions: Vec<String>,
}

impl Masked {
    pub(crate) fn new(text: &str) -> Self {
        let spans = regions(text);
        if spans.is_empty() {
            return Self {
                text: text.to_owned(),
                regions: Vec::new(),
            };
        }
        let mut out = String::with_capacity(text.len());
        let mut kept = Vec::with_capacity(spans.len());
        let mut pos = 0;
        for (i, (s, e)) in spans.into_iter().enumerate() {
            out.push_str(&text[pos..s]);
            out.push(OPEN);
            out.push_str(&i.to_string());
            out.push(CLOSE);
            kept.push(text[s..e].to_owned());
            pos = e;
        }
        out.push_str(&text[pos..]);
        Self {
            text: out,
            regions: kept,
        }
    }

    /// Строка с возвращёнными областями кода.
    pub(crate) fn restore(&self, s: &str) -> String {
        if self.regions.is_empty() || !s.contains(OPEN) {
            return s.to_owned();
        }
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(i) = rest.find(OPEN) {
            out.push_str(&rest[..i]);
            let after = &rest[i + OPEN.len_utf8()..];
            let digits = after.bytes().take_while(u8::is_ascii_digit).count();
            let region = after[digits..]
                .starts_with(CLOSE)
                .then(|| after[..digits].parse::<usize>().ok())
                .flatten()
                .and_then(|n| self.regions.get(n));
            match region {
                Some(r) => {
                    out.push_str(r);
                    rest = &after[digits + CLOSE.len_utf8()..];
                }
                None => {
                    out.push(OPEN);
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// Вызов с возвращёнными областями кода в имени и аргументах.
    pub(crate) fn restore_call(&self, call: RawCall) -> RawCall {
        if self.regions.is_empty() {
            return call;
        }
        let args = match call.args {
            RawArgs::Json(v) => RawArgs::Json(self.restore_value(v)),
            RawArgs::Text(s) => RawArgs::Text(self.restore(&s)),
            RawArgs::Pairs(p) => RawArgs::Pairs(
                p.into_iter()
                    .map(|(k, v)| (self.restore(&k), self.restore(&v)))
                    .collect(),
            ),
        };
        RawCall {
            name: self.restore(&call.name),
            args,
        }
    }

    fn restore_value(&self, v: Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.restore(&s)),
            Value::Array(a) => Value::Array(a.into_iter().map(|x| self.restore_value(x)).collect()),
            Value::Object(m) => Value::Object(
                m.into_iter()
                    .map(|(k, x)| (self.restore(&k), self.restore_value(x)))
                    .collect(),
            ),
            other => other,
        }
    }
}

/// Области кода и символов-заглушек: отсортированы, не пересекаются.
fn regions(text: &str) -> Vec<(usize, usize)> {
    let mut bodies = Bodies::new(text);
    let mut out = Vec::new();
    // Открытый забор: (символ, длина, начало блока).
    let mut fence: Option<(u8, usize, usize)> = None;
    let mut line_start = 0;
    while line_start < text.len() {
        let line_end = text[line_start..]
            .find('\n')
            .map_or(text.len(), |i| line_start + i + 1);
        let line = &text[line_start..line_end];
        let indent = line.bytes().take(4).take_while(|b| *b == b' ').count();
        let body = if indent <= 3 { &line[indent..] } else { "" };
        let in_call = bodies.contains(line_start + indent.min(line.len()));
        let run = |c: u8| {
            if in_call {
                0
            } else {
                body.bytes().take_while(|b| *b == c).count()
            }
        };
        match fence {
            Some((c, len, start)) => {
                let n = run(c);
                if n >= len && body[n..].trim().is_empty() {
                    // Перевод строки после закрывающего забора — уже не код: пробелы
                    // у вырезанного рядом вызова схлопываются как обычно.
                    let content_end = line_start + line.trim_end_matches(['\n', '\r']).len();
                    out.push((start, content_end));
                    fence = None;
                }
            }
            None => {
                let opener = b"`~".iter().map(|&c| (c, run(c))).find(|(_, n)| *n >= 3);
                match opener {
                    Some((c, n)) => fence = Some((c, n, line_start)),
                    None => inline_spans(text, line_start, line_end, &mut bodies, &mut out),
                }
            }
        }
        line_start = line_end;
    }
    if let Some((_, _, start)) = fence {
        out.push((start, text.len()));
    }

    // Символы заглушек вне областей кода.
    let mut extra = Vec::new();
    let mut r = 0;
    for (p, ch) in text.char_indices() {
        if ch != OPEN && ch != CLOSE {
            continue;
        }
        while r < out.len() && out[r].1 <= p {
            r += 1;
        }
        if r < out.len() && out[r].0 <= p {
            continue;
        }
        extra.push((p, p + ch.len_utf8()));
    }
    if !extra.is_empty() {
        out.extend(extra);
        out.sort_unstable();
    }
    out
}

/// Инлайн-код строки `[start, end)`: серия обратных кавычек закрывается следующей
/// серией той же длины в этой строке.
fn inline_spans(
    text: &str,
    start: usize,
    end: usize,
    bodies: &mut Bodies,
    out: &mut Vec<(usize, usize)>,
) {
    let bytes = &text.as_bytes()[start..end];
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let n = bytes[i..].iter().take_while(|b| **b == b'`').count();
            if !bodies.contains(start + i) {
                runs.push((start + i, n));
            }
            i += n;
        } else {
            i += 1;
        }
    }
    if runs.len() < 2 {
        return;
    }
    // Следующая серия той же длины — проход справа налево.
    let mut next_same = vec![None; runs.len()];
    let mut last: HashMap<usize, usize> = HashMap::new();
    for k in (0..runs.len()).rev() {
        next_same[k] = last.get(&runs[k].1).copied();
        last.insert(runs[k].1, k);
    }
    let mut k = 0;
    while k < runs.len() {
        match next_same[k] {
            Some(j) => {
                out.push((runs[k].0, runs[j].0 + runs[j].1));
                k = j + 1;
            }
            None => k += 1,
        }
    }
}

/// Тела вызовов P0 в исходном тексте; запросы `contains` — с неубывающими позициями.
struct Bodies {
    spans: Vec<(usize, usize)>,
    next: usize,
}

impl Bodies {
    fn new(text: &str) -> Self {
        let mut spans = Vec::new();
        paired(text, "<tool_call>", &["</tool_call>"], &mut spans);
        paired(text, "<function=", &["</function>"], &mut spans);
        harmony_calls(text, &mut spans);
        // Тела разных видов могут вкладываться (`<tool_call><function=…>…`): нужна
        // только принадлежность, поэтому они сливаются в непересекающиеся отрезки.
        spans.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        for (s, e) in spans {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        Self {
            spans: merged,
            next: 0,
        }
    }

    fn contains(&mut self, p: usize) -> bool {
        while self.next < self.spans.len() && self.spans[self.next].1 <= p {
            self.next += 1;
        }
        self.spans
            .get(self.next)
            .is_some_and(|(s, e)| *s <= p && p < *e)
    }
}

/// Тела `open … close`, где закрытие стоит раньше следующего `open`.
fn paired(text: &str, open: &'static str, closes: &[&'static str], out: &mut Vec<(usize, usize)>) {
    let mut opens = Finder::new(text, open);
    let mut ends: Vec<Finder<'_>> = closes.iter().map(|c| Finder::new(text, c)).collect();
    let mut cur = 0;
    while let Some(o) = opens.find(cur) {
        let body = o + open.len();
        let next_open = opens.find(body);
        match earliest(&mut ends, body) {
            Some((c, t)) if next_open.is_none_or(|n| c < n) => {
                out.push((o, c + t.len()));
                cur = c + t.len();
            }
            _ => cur = body,
        }
    }
}

/// Тела вызовов Harmony: `to=functions.… <|message|> … <|call|>` (или `<|end|>`).
/// Сообщения обычных каналов (`final`, `analysis` до `<|end|>`/`<|return|>`) телами
/// вызовов не считаются: в сыром ответе gpt-oss весь ответ лежит в таком канале, и
/// код Markdown в нём должен оставаться скрытым (раунд 3, I-H1).
fn harmony_calls(text: &str, out: &mut Vec<(usize, usize)>) {
    const TO: &str = "to=functions.";
    const MESSAGE: &str = "<|message|>";
    let mut to = Finder::new(text, TO);
    let mut message = Finder::new(text, MESSAGE);
    // Заголовок вызова кончается на `<|message|>` раньше конца сообщения и нового
    // заголовка.
    let mut header_stops = [
        Finder::new(text, "<|start|>"),
        Finder::new(text, "<|end|>"),
        Finder::new(text, "<|call|>"),
        Finder::new(text, "<|return|>"),
        Finder::new(text, TO),
    ];
    let mut body_ends = [Finder::new(text, "<|call|>"), Finder::new(text, "<|end|>")];
    let mut cur = 0;
    while let Some(p) = to.find(cur) {
        let after = p + TO.len();
        cur = after;
        let Some(m) = message.find(after) else {
            break;
        };
        if earliest(&mut header_stops, after).is_some_and(|(s, _)| s < m) {
            continue;
        }
        let body = m + MESSAGE.len();
        let next_message = message.find(body);
        if let Some((c, t)) = earliest(&mut body_ends, body)
            && next_message.is_none_or(|n| c < n)
        {
            out.push((p, c + t.len()));
            cur = c + t.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masked_parts(text: &str) -> Vec<String> {
        Masked::new(text).regions
    }

    #[test]
    fn regions_of_markdown() {
        assert_eq!(masked_parts("a `x` b ``y`` `z"), ["`x`", "``y``"]);
        assert_eq!(
            masked_parts("t\n```rs\ncode\n```\nafter"),
            ["```rs\ncode\n```"]
        );
        assert_eq!(masked_parts("t\n  ~~~\nopen"), ["  ~~~\nopen"]);
        assert_eq!(masked_parts("    ```\nindented"), Vec::<String>::new());
        assert_eq!(masked_parts("````\n```\n````\n"), ["````\n```\n````"]);
        assert_eq!(masked_parts("a\u{E000}0\u{E001}"), ["\u{E000}", "\u{E001}"]);
        // Забор и кавычки внутри тела вызова — не Markdown.
        assert_eq!(
            masked_parts("<function=w>\n```\n`a` </function>\n```\nx\n```"),
            ["```\nx\n```"]
        );
    }

    #[test]
    fn mask_and_restore_roundtrip() {
        for text in [
            "",
            "plain",
            "a `x` b\n```\nc\n```\nd",
            "\u{E000}0\u{E001} `q`",
        ] {
            let m = Masked::new(text);
            assert_eq!(m.restore(&m.text), text);
        }
    }
}
