//! Подсказки для несуществующего пути (таблица 3.4 ТЗ 02, «Выдуманные пути»; D87).

/// Наибольшее расстояние Дамерау–Левенштейна похожего имени.
const MAX_DISTANCE: usize = 2;

/// До `max` имён из `candidates` на расстоянии Дамерау–Левенштейна (вариант «optimal string
/// alignment»: вставка, удаление, замена, перестановка соседних символов) не больше 2 от
/// `name`, по возрастанию расстояния, затем по имени. Само `name` не подсказывается.
pub fn nearest(name: &str, candidates: &[String], max: usize) -> Vec<String> {
    let a: Vec<char> = name.chars().collect();
    let mut hits: Vec<(usize, &String)> = candidates
        .iter()
        .filter(|c| c.as_str() != name)
        .filter_map(|c| {
            let d = distance(&a, &c.chars().collect::<Vec<_>>());
            (d <= MAX_DISTANCE).then_some((d, c))
        })
        .collect();
    hits.sort();
    hits.dedup();
    hits.into_iter().take(max).map(|(_, c)| c.clone()).collect()
}

/// Расстояние OSA; заведомо далёкие по длине пары — сразу `MAX_DISTANCE + 1`.
fn distance(a: &[char], b: &[char]) -> usize {
    if a.len().abs_diff(b.len()) > MAX_DISTANCE {
        return MAX_DISTANCE + 1;
    }
    let w = b.len() + 1;
    // Три строки таблицы: i-2, i-1, i.
    let mut prev2 = vec![0usize; w];
    let mut prev: Vec<usize> = (0..w).collect();
    let mut cur = vec![0usize; w];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut d = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(prev2[j - 2] + 1);
            }
            cur[j] = d;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::distance;

    fn d(a: &str, b: &str) -> usize {
        distance(
            &a.chars().collect::<Vec<_>>(),
            &b.chars().collect::<Vec<_>>(),
        )
    }

    #[test]
    fn osa_distance() {
        assert_eq!(d("mian", "main"), 1);
        assert_eq!(d("main", "main"), 0);
        assert_eq!(d("", "ab"), 2);
        assert_eq!(d("kitten", "sitting"), 3);
        assert_eq!(d("ab", "ba"), 1);
        assert_eq!(d("ca", "abc"), 3);
        assert_eq!(d("привет", "првиет"), 1);
    }
}
