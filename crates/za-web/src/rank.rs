//! Ранжирование абзацев длинной страницы по запросу `web_fetch(url, prompt)` (R1-C2 D110,
//! preflight 5.3). Без модели (MDL-014 — R4): совпадение слов.
//!
//! - **Слова:** нижний регистр, буквы и цифры, длина от 3 символов, без [`STOP_WORDS`].
//! - **Отрывки:** абзацы (серии непустых строк); абзац больше доли совпадений режется по
//!   строкам, строка больше неё — по границе символа.
//! - **Выборка по совпадениям:** отрывки с общими словами по убыванию их числа (при равенстве —
//!   раньше в тексте), пока помещаются в 60 % бюджета. Отрывки без общих слов в неё не входят.
//! - **Голова страницы** (первые [`HEAD_LINES`] строк): без совпадений вовсе — только она (в
//!   пределах бюджета); иначе — её ещё не взятые отрывки, если они целиком помещаются в
//!   оставшиеся 40 %. Частью голова не добавляется: на длинной странице она вытеснила бы
//!   совпадения посторонним текстом.
//! - **Итог** — в исходном порядке, отрывки через пустую строку, не длиннее бюджета.

use std::collections::BTreeSet;
use std::ops::Range;

/// Голова страницы — столько первых строк (D110).
const HEAD_LINES: usize = 20;
/// Доля бюджета под выборку по совпадениям, процентов (D110); остальное — голова.
const MATCH_SHARE_PERCENT: usize = 60;
/// Разделитель отрывков в итоге.
const SEP: &str = "\n\n";

/// 40 английских стоп-слов (короче трёх символов слова и так не считаются).
const STOP_WORDS: [&str; 40] = [
    "the", "and", "for", "are", "but", "not", "you", "your", "with", "this", "that", "these",
    "from", "into", "about", "there", "their", "they", "them", "then", "than", "was", "were",
    "been", "have", "has", "had", "does", "did", "can", "will", "would", "should", "how", "what",
    "when", "where", "which", "who", "why",
];

/// Отрывки `text`, близкие к `prompt`, в пределах `budget_bytes` байт (D110). Текст не
/// длиннее бюджета возвращается как есть.
pub fn rank_passages(text: &str, prompt: &str, budget_bytes: usize) -> String {
    if text.len() <= budget_bytes {
        return text.to_string();
    }
    let query: BTreeSet<String> = words(prompt).collect();
    let match_budget = budget_bytes * MATCH_SHARE_PERCENT / 100;
    let head_budget = budget_bytes - match_budget;
    let pieces = passages(text, match_budget.saturating_sub(SEP.len()).max(1));
    let scores: Vec<usize> = pieces
        .iter()
        .map(|r| common(&text[r.clone()], &query))
        .collect();

    let mut order: Vec<usize> = (0..pieces.len()).filter(|&i| scores[i] > 0).collect();
    order.sort_by(|&a, &b| scores[b].cmp(&scores[a]).then(a.cmp(&b)));
    let mut taken = vec![false; pieces.len()];
    let mut used = 0usize;
    for i in order {
        let cost = pieces[i].len() + SEP.len();
        if used + cost <= match_budget {
            taken[i] = true;
            used += cost;
        }
    }
    if !taken.contains(&true) {
        // Совпадений нет вовсе (или ни одно не помещается в крошечный бюджет).
        return head_only(text, budget_bytes);
    }

    let head_end = head_end(text);
    let head: Vec<Range<usize>> = pieces
        .iter()
        .zip(&taken)
        .filter(|(r, t)| !**t && r.start < head_end)
        .map(|(r, _)| r.start..r.end.min(head_end))
        .collect();
    let head_cost: usize = head.iter().map(|r| r.len() + SEP.len()).sum();

    let mut chosen: Vec<Range<usize>> = pieces
        .iter()
        .zip(&taken)
        .filter(|(_, t)| **t)
        .map(|(r, _)| r.clone())
        .collect();
    if head_cost <= head_budget {
        chosen.extend(head);
    }
    chosen.sort_by_key(|r| r.start);
    chosen
        .iter()
        .map(|r| &text[r.clone()])
        .collect::<Vec<_>>()
        .join(SEP)
}

/// Слова для сравнения: нижний регистр, буквы и цифры, от 3 символов, без стоп-слов.
fn words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().nth(2).is_some())
        .map(str::to_lowercase)
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
}

/// Число разных слов запроса в отрывке.
fn common(piece: &str, query: &BTreeSet<String>) -> usize {
    if query.is_empty() {
        return 0;
    }
    let mut seen = BTreeSet::new();
    for w in words(piece) {
        if query.contains(&w) {
            seen.insert(w);
        }
    }
    seen.len()
}

/// Конец головы страницы: конец строки [`HEAD_LINES`] (без перевода строки) или конец текста.
fn head_end(text: &str) -> usize {
    text.match_indices('\n')
        .nth(HEAD_LINES - 1)
        .map_or(text.len(), |(i, _)| i)
}

/// Только голова страницы, обрезанная по бюджету (по границе символа).
fn head_only(text: &str, budget: usize) -> String {
    let head = text[..head_end(text)].trim_end();
    head[..head.floor_char_boundary(budget)]
        .trim_end()
        .to_string()
}

/// Отрывки текста — диапазоны байт: абзацы (серии строк с непробельным текстом), каждый
/// не длиннее `limit` (длинный режется по строкам, длинная строка — по границе символа).
fn passages(text: &str, limit: usize) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut para: Vec<Range<usize>> = Vec::new();
    let mut pos = 0usize;
    for line in text.split('\n') {
        let start = pos;
        pos += line.len() + 1;
        if line.trim().is_empty() {
            split_paragraph(text, &para, limit, &mut out);
            para.clear();
        } else {
            para.push(start..start + line.trim_end_matches('\r').len());
        }
    }
    split_paragraph(text, &para, limit, &mut out);
    out
}

/// Абзац из строк `lines` — одним отрывком или частями не длиннее `limit`.
fn split_paragraph(text: &str, lines: &[Range<usize>], limit: usize, out: &mut Vec<Range<usize>>) {
    let (Some(first), Some(last)) = (lines.first(), lines.last()) else {
        return;
    };
    if last.end - first.start <= limit {
        out.push(first.start..last.end);
        return;
    }
    let mut cur: Option<Range<usize>> = None;
    for line in lines {
        if line.len() > limit {
            out.extend(cur.take());
            let mut a = line.start;
            while a < line.end {
                let mut b = text.floor_char_boundary((a + limit).min(line.end));
                if b == a {
                    // Символ длиннее предела: целиком (в бюджет такой отрывок не войдёт).
                    b = a + text[a..].chars().next().map_or(1, char::len_utf8);
                }
                out.push(a..b);
                a = b;
            }
            continue;
        }
        cur = match cur.take() {
            Some(c) if line.end - c.start <= limit => Some(c.start..line.end),
            Some(c) => {
                out.push(c);
                Some(line.clone())
            }
            None => Some(line.clone()),
        };
    }
    out.extend(cur);
}
