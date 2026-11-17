//! Ранжирование абзацев длинной страницы по запросу `web_fetch(url, prompt)` (R1-C2 D110,
//! preflight 5.3). Без модели (MDL-014 — R4): совпадение слов.
//!
//! - **Слова:** `za_core::text::words` — нижний регистр, буквы и цифры, длина от 3 символов,
//!   без 40 стоп-слов (тот же разбор у роутера карточек скиллов, D119).
//! - **Отрывки:** абзацы (серии непустых строк); абзац больше доли совпадений режется по
//!   строкам, строка больше неё — по границе символа.
//! - **Выборка по совпадениям:** отрывки с общими словами по убыванию их числа (при равенстве —
//!   раньше в тексте), пока помещаются в 60 % бюджета. Отрывки без общих слов в неё не входят.
//! - **Голова страницы** (первые [`HEAD_LINES`] строк): без совпадений вовсе — только она (в
//!   пределах бюджета); иначе — её ещё не взятые отрывки, если они целиком помещаются в
//!   оставшиеся 40 %. Частью голова не добавляется: на длинной странице она вытеснила бы
//!   совпадения посторонним текстом.
//! - **Итог** — в исходном порядке, отрывки через пустую строку, не длиннее бюджета.
//! - **Бюджет** — в байтах текста после `escape_untrusted` (I2 ревью задачи 8): так длину
//!   результата меряет слот пакета, а `<` и `>` после экранирования весят по 4 байта.

use std::collections::BTreeSet;
use std::ops::Range;

use za_core::text::{escaped_char_len, escaped_len, words};

/// Голова страницы — столько первых строк (D110).
const HEAD_LINES: usize = 20;
/// Доля бюджета под выборку по совпадениям, процентов (D110); остальное — голова.
const MATCH_SHARE_PERCENT: usize = 60;
/// Разделитель отрывков в итоге.
const SEP: &str = "\n\n";

/// Отрывки `text`, близкие к `prompt`, в пределах `budget_bytes` байт после экранирования
/// (D110). Текст не длиннее бюджета возвращается как есть.
pub fn rank_passages(text: &str, prompt: &str, budget_bytes: usize) -> String {
    if escaped_len(text) <= budget_bytes {
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
        let cost = escaped_len(&text[pieces[i].clone()]) + SEP.len();
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
    let head_cost: usize = head
        .iter()
        .map(|r| escaped_len(&text[r.clone()]) + SEP.len())
        .sum();

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

/// Только голова страницы, обрезанная по бюджету (по границе символа, длина — после
/// экранирования).
fn head_only(text: &str, budget: usize) -> String {
    let head = text[..head_end(text)].trim_end();
    head[..prefix_end(head, 0, head.len(), budget)]
        .trim_end()
        .to_string()
}

/// Конец самого длинного начала `text[a..end]`, которое после экранирования не длиннее
/// `limit` байт (по границе символа); `a`, если и первый символ длиннее.
fn prefix_end(text: &str, a: usize, end: usize, limit: usize) -> usize {
    let mut shown = 0usize;
    for (i, c) in text[a..end].char_indices() {
        shown += escaped_char_len(c);
        if shown > limit {
            return a + i;
        }
    }
    end
}

/// Строка абзаца: диапазон байт и прирост длины от экранирования (`<`, `>` — по 3 байта).
/// Длина диапазона из целых строк после экранирования — его длина плюс сумма приростов:
/// переводы строк между ними не экранируются.
struct Line {
    range: Range<usize>,
    extra: usize,
}

/// Отрывки текста — диапазоны байт: абзацы (серии строк с непробельным текстом), каждый
/// не длиннее `limit` байт после экранирования (длинный режется по строкам, длинная
/// строка — по границе символа).
fn passages(text: &str, limit: usize) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut para: Vec<Line> = Vec::new();
    let mut pos = 0usize;
    for line in text.split('\n') {
        let start = pos;
        pos += line.len() + 1;
        if line.trim().is_empty() {
            split_paragraph(text, &para, limit, &mut out);
            para.clear();
        } else {
            let body = line.trim_end_matches('\r');
            para.push(Line {
                range: start..start + body.len(),
                extra: escaped_len(body) - body.len(),
            });
        }
    }
    split_paragraph(text, &para, limit, &mut out);
    out
}

/// Абзац из строк `lines` — одним отрывком или частями не длиннее `limit` после
/// экранирования.
fn split_paragraph(text: &str, lines: &[Line], limit: usize, out: &mut Vec<Range<usize>>) {
    let (Some(first), Some(last)) = (lines.first(), lines.last()) else {
        return;
    };
    let extra: usize = lines.iter().map(|l| l.extra).sum();
    if last.range.end - first.range.start + extra <= limit {
        out.push(first.range.start..last.range.end);
        return;
    }
    // Текущий отрывок из целых строк и его прирост от экранирования.
    let mut cur: Option<(Range<usize>, usize)> = None;
    for line in lines {
        if line.range.len() + line.extra > limit {
            out.extend(cur.take().map(|(r, _)| r));
            let mut a = line.range.start;
            while a < line.range.end {
                let mut b = prefix_end(text, a, line.range.end, limit);
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
            Some((c, e)) if line.range.end - c.start + e + line.extra <= limit => {
                Some((c.start..line.range.end, e + line.extra))
            }
            Some((c, _)) => {
                out.push(c);
                Some((line.range.clone(), line.extra))
            }
            None => Some((line.range.clone(), line.extra)),
        };
    }
    out.extend(cur.map(|(r, _)| r));
}
