//! Роутер карточек tier small (SKL-004, R1-C2 D119): по вводу пользователя — скилл
//! каталога, чья карточка дописывается к сообщению. Эвристика: ошибка роутера стоит одну
//! карточку в контексте; полный текст на small приходит только по `/имя`.
//!
//! - **Слова** — `za_core::text::words` (как у `web_fetch(url, prompt)`, D110): нижний
//!   регистр, буквы и цифры, от трёх символов, без стоп-слов.
//! - **Слова скилла** — из имени (`domain-model` → `domain`, `model`) и описания.
//! - **Вес слова** — `idf = ln(1 + N / df)` по каталогу (`N` — скиллов, `df` — скиллов со
//!   словом) в целых миллионных: сумма весов не зависит от порядка сложения, и ничья
//!   определяется точно.
//! - **Кандидат** — наибольшая сумма весов общих слов среди скиллов, у которых общих слов
//!   не меньше [`ROUTE_MIN_WORDS`]; две лучшие суммы равны — кандидата нет.

use std::collections::{BTreeMap, BTreeSet};

use za_core::text::words;

use crate::discover::{Catalog, SkillMeta};

/// Минимум разных общих слов ввода и скилла (D119).
pub const ROUTE_MIN_WORDS: usize = 2;

/// Множитель фиксированной точки весов.
const SCALE: f64 = 1_000_000.0;

/// Скилл для карточки по тексту ввода (D119); `None` — кандидата нет.
pub fn route<'a>(catalog: &'a Catalog, text: &str) -> Option<&'a SkillMeta> {
    let query: BTreeSet<String> = words(text).collect();
    if query.len() < ROUTE_MIN_WORDS {
        return None;
    }
    let sets: Vec<BTreeSet<String>> = catalog
        .skills
        .iter()
        .map(|s| words(&s.name).chain(words(&s.description)).collect())
        .collect();
    // Документная частота — только у слов ввода: остальные на итог не влияют.
    let mut df: BTreeMap<&str, usize> = BTreeMap::new();
    for set in &sets {
        for w in query.iter().filter(|w| set.contains(*w)) {
            *df.entry(w.as_str()).or_insert(0) += 1;
        }
    }
    let n = sets.len() as f64;
    let weight = |w: &str| -> u64 {
        let d = df.get(w).copied().unwrap_or(1).max(1) as f64;
        // ln(1 + N/df) > 0 при N ≥ 1; округление до миллионных — целое без знака.
        ((1.0 + n / d).ln() * SCALE).round() as u64
    };
    let mut best: Option<(u64, usize)> = None;
    let mut tie = false;
    for (i, set) in sets.iter().enumerate() {
        let common: Vec<&str> = query
            .iter()
            .filter(|w| set.contains(*w))
            .map(String::as_str)
            .collect();
        if common.len() < ROUTE_MIN_WORDS {
            continue;
        }
        let score: u64 = common.iter().map(|w| weight(w)).sum();
        match best {
            Some((b, _)) if score < b => {}
            Some((b, _)) if score == b => tie = true,
            _ => {
                best = Some((score, i));
                tie = false;
            }
        }
    }
    match best {
        Some((_, i)) if !tie => catalog.skills.get(i),
        _ => None,
    }
}
