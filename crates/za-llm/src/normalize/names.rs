//! Проверка имени инструмента (D28): расстояние Дамерау — Левенштейна.

use std::collections::HashMap;

/// Наибольшее расстояние, при котором имя исправляется (D28).
const MAX_FIX: usize = 2;
/// Короче этого имя не исправляется (решение контроллера, I4: `get` → `set`).
const MIN_FIX_LEN: usize = 5;
/// Сколько ближайших имён возвращается при отказе (D28).
const NEAREST: usize = 3;
/// Имя модели длиннее этого обрезается для расчёта расстояния (защита от O(n·m)
/// на мусоре); такое имя всё равно не исправимо.
const MAX_NAME_CHARS: usize = 256;

/// Расстояние Дамерау — Левенштейна (неограниченное, алгоритм Лоуренса — Вагнера):
/// вставка, удаление, замена и перестановка соседних символов. Считается по символам
/// Unicode, а не по байтам.
pub fn damerau_levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let inf = n + m;
    let w = m + 2;
    // Матрица (n + 2) × (m + 2) со сдвигом на 1: строка и столбец 0 — «бесконечность».
    let mut d = vec![0usize; (n + 2) * w];
    d[0] = inf;
    for i in 0..=n {
        d[(i + 1) * w] = inf;
        d[(i + 1) * w + 1] = i;
    }
    for j in 0..=m {
        d[j + 1] = inf;
        d[w + j + 1] = j;
    }
    // Последняя строка, в которой встречался символ.
    let mut last_row: HashMap<char, usize> = HashMap::new();
    for i in 1..=n {
        // Последний столбец этой строки с совпадением.
        let mut last_match_col = 0;
        for j in 1..=m {
            let i1 = last_row.get(&b[j - 1]).copied().unwrap_or(0);
            let j1 = last_match_col;
            let cost = if a[i - 1] == b[j - 1] {
                last_match_col = j;
                0
            } else {
                1
            };
            let substitute = d[i * w + j] + cost;
            let insert = d[(i + 1) * w + j] + 1;
            let delete = d[i * w + j + 1] + 1;
            let transpose = d[i1 * w + j1] + (i - i1 - 1) + 1 + (j - j1 - 1);
            d[(i + 1) * w + j + 1] = substitute.min(insert).min(delete).min(transpose);
        }
        last_row.insert(a[i - 1], i);
    }
    d[(n + 1) * w + m + 1]
}

/// Итог проверки имени.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NameMatch {
    Exact,
    /// Единственный кандидат с расстоянием ≤ min(2, длина / 4) при длине ≥ 5.
    Corrected(String),
    /// Нет единственного кандидата: до трёх ближайших по возрастанию расстояния,
    /// при равенстве — по имени.
    Unknown(Vec<String>),
}

/// Проверка имени по списку известных (D28 с порогом по длине — решение контроллера I4).
pub(crate) fn resolve_name(name: &str, known: &[&str]) -> NameMatch {
    if known.contains(&name) {
        return NameMatch::Exact;
    }
    let len = name.chars().count();
    let probe: String = name.chars().take(MAX_NAME_CHARS).collect();
    // Порог относительно длины: d ≤ min(2, len / 4), имя не короче 5 символов.
    let max_fix = if (MIN_FIX_LEN..=MAX_NAME_CHARS).contains(&len) {
        MAX_FIX.min(len / 4)
    } else {
        0
    };
    let mut ranked: Vec<(usize, &str)> = known
        .iter()
        .map(|k| (damerau_levenshtein(&probe, k), *k))
        .collect();
    ranked.sort_unstable();
    ranked.dedup();
    let fixable: Vec<&str> = ranked
        .iter()
        .filter(|(d, _)| *d <= max_fix)
        .map(|(_, k)| *k)
        .collect();
    if let [only] = fixable[..]
        && max_fix > 0
    {
        return NameMatch::Corrected(only.to_owned());
    }
    NameMatch::Unknown(
        ranked
            .into_iter()
            .take(NEAREST)
            .map(|(_, k)| k.to_owned())
            .collect(),
    )
}
