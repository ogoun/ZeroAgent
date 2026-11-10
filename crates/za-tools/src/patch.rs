//! Толерантный патчер `edit` (D88, раздел 4.1 и таблица 3.4 ТЗ 02): замена одного места
//! `old` на `new` и защита от перезаписи.
//!
//! Уровни совпадения по очереди ([`MatchLevel`]):
//! 1. точное совпадение;
//! 2. после приведения `\r\n` к `\n` у файла и у `old`;
//! 3. построчно без хвостовых пробелов;
//! 4. построчно без отступов — тогда `new` получает отступ найденного места.
//!
//! Первый уровень, где есть совпадения, решает: одно — замена, несколько —
//! [`PatchError::Ambiguous`] с номерами строк. Ни одного ни на одном уровне —
//! [`PatchError::NotFound`] с ближайшим участком (`similar`, сходство ≥ 0,6).
//!
//! Стиль переводов строк файла сохраняется: `new` приводится к нему (большинство строк
//! файла с `\r\n` — CRLF, иначе LF; файл без переводов строк — `new` как есть). Отсутствие
//! `\n` в конце файла не меняется: замена трогает только найденное место. BOM снимает и
//! возвращает вызывающий.

use std::collections::{BTreeSet, HashSet};
use std::time::{Duration, Instant};

use similar::TextDiff;

/// Порог сходства ближайшего участка (D88).
const CLOSEST_RATIO: f32 = 0.6;
/// Файл с числом строк не больше этого сравнивается со всеми окнами; длиннее — только с
/// окнами, где хоть одна строка совпала со строкой `old` без пробелов по краям.
const CLOSEST_ALL_WINDOWS: usize = 2000;
/// Предел числа окон-кандидатов ближайшего участка.
const CLOSEST_MAX_WINDOWS: usize = 5000;
/// Предел времени поиска ближайшего участка: подсказка не должна держать вызов.
const CLOSEST_BUDGET: Duration = Duration::from_secs(1);
/// Защита от перезаписи (таблица 3.4): файл от стольких строк…
const SHRINK_MIN_LINES: usize = 20;

/// Почему правка не применена.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchError {
    /// `old` не найден; ближайший участок: строки `(с, по)` (с 1) и его текст.
    NotFound {
        closest: Option<(usize, usize, String)>,
    },
    /// `old` найден в нескольких местах: номера строк начала (с 1).
    Ambiguous { lines: Vec<usize> },
    /// Правка убирает `removed` из `total` строк без `allow_shrink`.
    Shrink { removed: usize, total: usize },
    /// Пустой `old` у существующего непустого файла.
    Exists,
    /// Файл не в UTF-8.
    NotUtf8,
}

/// На каком уровне найден `old`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchLevel {
    Exact,
    LineEndings,
    TrailingSpace,
    Indent,
}

/// Заменяет единственное место `old` в `text` на `new` (см. модуль). Пустой `old` у
/// пустого `text` даёт `new`; у непустого — [`PatchError::Exists`].
pub fn apply(text: &str, old: &str, new: &str) -> Result<(String, MatchLevel), PatchError> {
    if old.is_empty() {
        return if text.is_empty() {
            Ok((new.to_owned(), MatchLevel::Exact))
        } else {
            Err(PatchError::Exists)
        };
    }
    let crlf = crlf_style(text);
    let styled = |s: &str| restyle(&s.replace("\r\n", "\n"), crlf);

    // 1. Точно.
    let starts = overlapping(text, old);
    match starts.len() {
        0 => {}
        1 => {
            let s = starts[0];
            let out = splice(text, s, s + old.len(), &styled(new));
            return Ok((out, MatchLevel::Exact));
        }
        _ => {
            return Err(PatchError::Ambiguous {
                lines: line_numbers(text, &starts),
            });
        }
    }

    let norm = Normalized::new(text);
    let old_n = old.replace("\r\n", "\n");
    let new_n = new.replace("\r\n", "\n");

    // 2. Без `\r\n`.
    if !norm.removed.is_empty() || old_n.len() != old.len() {
        let starts = overlapping(&norm.text, &old_n);
        match starts.len() {
            0 => {}
            1 => {
                let s = starts[0];
                let (a, b) = (norm.to_orig(s), norm.to_orig(s + old_n.len()));
                return Ok((
                    splice(text, a, b, &restyle(&new_n, crlf)),
                    MatchLevel::LineEndings,
                ));
            }
            _ => {
                return Err(PatchError::Ambiguous {
                    lines: line_numbers(&norm.text, &starts),
                });
            }
        }
    }

    // 3–4. Построчно.
    let lines = line_spans(&norm.text);
    let (old_body, had_nl) = match old_n.strip_suffix('\n') {
        Some(b) => (b, true),
        None => (old_n.as_str(), false),
    };
    let new_body = if had_nl {
        new_n.strip_suffix('\n').unwrap_or(&new_n)
    } else {
        new_n.as_str()
    };
    let old_lines: Vec<&str> = old_body.split('\n').collect();
    for level in [MatchLevel::TrailingSpace, MatchLevel::Indent] {
        let key = |s: &str| -> String {
            match level {
                MatchLevel::Indent => s.trim().to_owned(),
                _ => s.trim_end().to_owned(),
            }
        };
        let old_keys: Vec<String> = old_lines.iter().map(|l| key(l)).collect();
        let found = find_windows(&norm.text, &lines, &old_keys, &key);
        match found.len() {
            0 => continue,
            1 => {
                let i = found[0];
                let k = old_lines.len();
                let (s, mut e) = (lines[i].0, lines[i + k - 1].1);
                // Удаление целых строк (m1): `old` с `\n` и пустой `new` убирают и перевод
                // строки найденного места, как на точном уровне.
                if had_nl && new_n.is_empty() && e < norm.text.len() {
                    e += 1;
                }
                let replacement = if level == MatchLevel::Indent {
                    let found_lines: Vec<&str> = (i..i + k)
                        .map(|j| &norm.text[lines[j].0..lines[j].1])
                        .collect();
                    reindent(new_body, &old_lines, &found_lines)
                } else {
                    new_body.to_owned()
                };
                let (a, b) = (norm.to_orig(s), norm.to_orig(e));
                return Ok((splice(text, a, b, &restyle(&replacement, crlf)), level));
            }
            _ => {
                return Err(PatchError::Ambiguous {
                    lines: found.iter().map(|i| i + 1).collect(),
                });
            }
        }
    }
    Err(PatchError::NotFound {
        closest: closest(&norm.text, &lines, old_body),
    })
}

/// Защита от перезаписи (таблица 3.4): у файла от [`SHRINK_MIN_LINES`] строк правка не
/// может без `allow` убрать больше половины строк (по числу строк до и после).
pub fn shrink_guard(before: &str, after: &str, allow: bool) -> Result<(), PatchError> {
    let total = count_lines(before);
    let removed = total.saturating_sub(count_lines(after));
    if !allow && total >= SHRINK_MIN_LINES && removed * 2 > total {
        return Err(PatchError::Shrink { removed, total });
    }
    Ok(())
}

/// Число строк текста: последняя строка без `\n` тоже считается.
pub(crate) fn count_lines(s: &str) -> usize {
    let n = s.bytes().filter(|&b| b == b'\n').count();
    if s.is_empty() || s.ends_with('\n') {
        n
    } else {
        n + 1
    }
}

/// Стиль файла — CRLF: строк с `\r\n` больше половины.
fn crlf_style(text: &str) -> Option<bool> {
    let lf = text.bytes().filter(|&b| b == b'\n').count();
    if lf == 0 {
        return None;
    }
    let crlf = text.matches("\r\n").count();
    Some(crlf * 2 > lf)
}

/// Текст с переводами `\n` в стиле файла; `None` — файл без переводов строк, как есть.
fn restyle(lf: &str, crlf: Option<bool>) -> String {
    match crlf {
        Some(true) => lf.replace('\n', "\r\n"),
        _ => lf.to_owned(),
    }
}

fn splice(text: &str, a: usize, b: usize, with: &str) -> String {
    let mut out = String::with_capacity(text.len() - (b - a) + with.len());
    out.push_str(&text[..a]);
    out.push_str(with);
    out.push_str(&text[b..]);
    out
}

/// Начала всех вхождений `pat` в `text`, в том числе перекрывающихся (I1): поиск
/// сдвигается на один символ, а не на длину совпадения. `"}\n}\n}\n"` содержит
/// `"}\n}\n"` дважды — это неоднозначность, а не одно место.
fn overlapping(text: &str, pat: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(i) = text[pos..].find(pat) {
        let at = pos + i;
        out.push(at);
        pos = at + text[at..].chars().next().map_or(1, char::len_utf8);
        if pos > text.len() {
            break;
        }
    }
    out
}

/// Номера строк (с 1) для отсортированных смещений.
fn line_numbers(text: &str, starts: &[usize]) -> Vec<usize> {
    let mut out = Vec::with_capacity(starts.len());
    let (mut line, mut pos) = (1, 0);
    for &s in starts {
        line += text.as_bytes()[pos..s]
            .iter()
            .filter(|&&b| b == b'\n')
            .count();
        pos = s;
        out.push(line);
    }
    out
}

/// Текст с `\r\n` → `\n` и обратное отображение смещений.
struct Normalized {
    text: String,
    /// Смещения в `text`, на месте которых убран `\r` (по возрастанию).
    removed: Vec<usize>,
}

impl Normalized {
    fn new(orig: &str) -> Self {
        let mut text = String::with_capacity(orig.len());
        let mut removed = Vec::new();
        let mut last = 0;
        for (i, _) in orig.match_indices("\r\n") {
            text.push_str(&orig[last..i]);
            removed.push(text.len());
            last = i + 1;
        }
        text.push_str(&orig[last..]);
        Self { text, removed }
    }

    /// Смещение в исходном тексте. Граница перед `\n` строки CRLF указывает на `\r`:
    /// участок, кончающийся концом строки, не захватывает её перевод.
    fn to_orig(&self, n: usize) -> usize {
        n + self.removed.partition_point(|&r| r < n)
    }
}

/// Строки текста: `(начало, конец)` без `\n`. Пустой хвост после последнего `\n` строкой
/// не считается.
fn line_spans(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, _) in text.match_indices('\n') {
        out.push((start, i));
        start = i + 1;
    }
    if start < text.len() {
        out.push((start, text.len()));
    }
    out
}

/// Начала окон, где ключи строк совпали с ключами `old` подряд.
fn find_windows(
    text: &str,
    lines: &[(usize, usize)],
    old_keys: &[String],
    key: &dyn Fn(&str) -> String,
) -> Vec<usize> {
    let k = old_keys.len();
    if k == 0 || lines.len() < k {
        return Vec::new();
    }
    let keys: Vec<String> = lines.iter().map(|&(a, b)| key(&text[a..b])).collect();
    (0..=lines.len() - k)
        .filter(|&i| keys[i] == old_keys[0] && (1..k).all(|j| keys[i + j] == old_keys[j]))
        .collect()
}

/// `new` с отступами найденного места: отступ строки `new`, равный отступу строки `old`,
/// заменяется отступом соответствующей найденной строки; иначе заменяется самый длинный
/// подходящий префикс-отступ `old`; иначе к нему спереди добавляется отступ первой
/// найденной строки (вместо отступа первой строки `old`, если он префикс).
fn reindent(new: &str, old_lines: &[&str], found: &[&str]) -> String {
    let indent = |s: &str| -> String { s[..s.len() - s.trim_start().len()].to_owned() };
    let mut map: Vec<(String, String)> = Vec::new();
    for (o, f) in old_lines.iter().zip(found) {
        if o.trim().is_empty() {
            continue;
        }
        let (oi, fi) = (indent(o), indent(f));
        if !map.iter().any(|(k, _)| *k == oi) {
            map.push((oi, fi));
        }
    }
    // Самые длинные отступы — первыми: префикс ищется с них.
    map.sort_by_key(|m| std::cmp::Reverse(m.0.len()));
    let old_base = old_lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| indent(l))
        .unwrap_or_default();
    let found_base = found
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| indent(l))
        .unwrap_or_default();
    new.split('\n')
        .map(|line| {
            if line.trim().is_empty() {
                return line.to_owned();
            }
            let ni = indent(line);
            let body = &line[ni.len()..];
            let mapped = map
                .iter()
                .find(|(o, _)| ni.starts_with(o.as_str()))
                .map(|(o, f)| format!("{f}{}", &ni[o.len()..]))
                .unwrap_or_else(|| match ni.strip_prefix(old_base.as_str()) {
                    Some(rest) => format!("{found_base}{rest}"),
                    None => format!("{found_base}{ni}"),
                });
            format!("{mapped}{body}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Ближайший к `old` участок из стольких же строк (сходство символов `similar` ≥ 0,6).
fn closest(text: &str, lines: &[(usize, usize)], old: &str) -> Option<(usize, usize, String)> {
    let k = old.split('\n').count();
    if lines.is_empty() {
        return None;
    }
    let k = k.min(lines.len());
    let last = lines.len() - k;
    let starts: BTreeSet<usize> = if lines.len() <= CLOSEST_ALL_WINDOWS {
        (0..=last).collect()
    } else {
        let keys: HashSet<&str> = old
            .split('\n')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        let mut s = BTreeSet::new();
        for (i, &(a, b)) in lines.iter().enumerate() {
            if keys.contains(text[a..b].trim()) {
                for j in 0..k {
                    s.insert(i.saturating_sub(j).min(last));
                }
            }
            if s.len() >= CLOSEST_MAX_WINDOWS {
                break;
            }
        }
        s
    };
    let deadline = Instant::now() + CLOSEST_BUDGET;
    let mut best: Option<(f32, usize)> = None;
    for i in starts {
        if Instant::now() >= deadline {
            break;
        }
        let window = &text[lines[i].0..lines[i + k - 1].1];
        let r = TextDiff::configure()
            .deadline(deadline)
            .diff_chars(old, window)
            .ratio();
        if best.is_none_or(|(b, _)| r > b) {
            best = Some((r, i));
        }
    }
    let (r, i) = best?;
    (r >= CLOSEST_RATIO).then(|| {
        (
            i + 1,
            i + k,
            text[lines[i].0..lines[i + k - 1].1].to_owned(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_maps_offsets_around_crlf() {
        let n = Normalized::new("a\r\nb\r\n");
        assert_eq!(n.text, "a\nb\n");
        assert_eq!(n.to_orig(0), 0);
        assert_eq!(n.to_orig(1), 1); // перед `\r`
        assert_eq!(n.to_orig(2), 3); // начало `b`
        assert_eq!(n.to_orig(4), 6); // конец
    }

    #[test]
    fn count_lines_last_without_newline() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("a"), 1);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb"), 2);
    }
}
