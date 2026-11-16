//! Разбор `robots.txt` (RFC 9309; D107) — свой, без зависимостей.
//!
//! Группа — строки `User-agent` подряд и правила после них. Действуют группы с токеном
//! агента (без учёта регистра; все такие группы объединяются), иначе группы `*`, иначе
//! разрешено всё. `Allow`/`Disallow` с `*` (любая последовательность) и `$` (конец пути,
//! только последним символом); побеждает самое длинное совпадение в октетах, при равенстве
//! — `Allow`. Пути сравниваются после нормализации процентного кодирования (RFC 9309,
//! 2.2.2): не-ASCII кодируется, `%XX` незарезервированного символа раскрывается.
//! `/robots.txt` разрешён всегда.
//!
//! **Пределы (M4 ревью задачи 6).** Текст — до [`ROBOTS_MAX_BYTES`] (дальше не
//! разбирается, обрезка по последней целой строке), правил группы агента — до
//! [`ROBOTS_MAX_RULES`] (дальше не действуют). Сопоставление жадное и линейное: O(путь +
//! шаблон) на правило. Худший случай у пределов текста и длины URL (2048) — единицы
//! миллисекунд в release; шлюз разбирает текст один раз на источник ([`Rules`] в кэше).

/// Предел текста `robots.txt` (RFC 9309, 2.5: не меньше 500 кибибайт; столько же читают
/// Google, Bing и Яндекс): дальше текст не разбирается.
pub const ROBOTS_MAX_BYTES: usize = 500 * 1024;
/// Предел правил группы агента (после объединения групп): дальше правила не действуют.
/// Вместе с длиной пути он ограничивает работу сопоставления на вызов.
pub const ROBOTS_MAX_RULES: usize = 2000;

/// Разрешён ли путь (`/a/b?q`) агенту `agent` по тексту `robots`.
pub fn robots_allows(robots: &str, agent: &str, path: &str) -> bool {
    Rules::parse(robots, agent).allows(path)
}

/// Правила агента из текста `robots.txt`: `(allow, нормализованный шаблон)`. Пустой набор
/// — разрешено всё (нет групп, 4xx).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Rules {
    rules: Vec<(bool, String)>,
}

impl Rules {
    /// Правила групп агента `agent`, иначе групп `*`, в пределах [`ROBOTS_MAX_BYTES`] и
    /// [`ROBOTS_MAX_RULES`].
    pub(crate) fn parse(robots: &str, agent: &str) -> Self {
        let (mut own, mut star) = (Vec::new(), Vec::new());
        let (mut own_seen, mut star_seen) = (false, false);
        // Агенты текущей группы; `in_rules` — после агентов уже шли правила.
        let (mut is_own, mut is_star, mut in_rules) = (false, false, false);
        let text = cut(robots);
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            if key.eq_ignore_ascii_case("user-agent") {
                if in_rules {
                    (is_own, is_star, in_rules) = (false, false, false);
                }
                let token = value.split(['/', ' ', '\t']).next().unwrap_or("");
                if token == "*" {
                    is_star = true;
                    star_seen = true;
                } else if token.eq_ignore_ascii_case(agent) {
                    is_own = true;
                    own_seen = true;
                }
                continue;
            }
            let allow = key.eq_ignore_ascii_case("allow");
            if !allow && !key.eq_ignore_ascii_case("disallow") {
                continue;
            }
            in_rules = true;
            let to_own = is_own && own.len() < ROBOTS_MAX_RULES;
            let to_star = is_star && star.len() < ROBOTS_MAX_RULES;
            if value.is_empty() || !(to_own || to_star) {
                continue;
            }
            let rule = (allow, normalize(value));
            if to_own {
                own.push(rule.clone());
            }
            if to_star {
                star.push(rule);
            }
        }
        let rules = if own_seen {
            own
        } else if star_seen {
            star
        } else {
            Vec::new()
        };
        Self { rules }
    }

    /// Разрешён ли путь (`/a/b?q`): самое длинное совпавшее правило, при равной длине —
    /// `Allow`; совпадений нет — разрешено.
    pub(crate) fn allows(&self, path: &str) -> bool {
        let path = normalize(path);
        if path == "/robots.txt" {
            return true;
        }
        let mut best: Option<(usize, bool)> = None;
        for (allow, pat) in &self.rules {
            let len = pat.len();
            // Правило, которое не победит уже найденное, не сопоставляется.
            if best.is_some_and(|(l, a)| l > len || (l == len && (a || !*allow))) {
                continue;
            }
            if matches(pat, &path) {
                best = Some((len, *allow));
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }
}

/// Текст в пределах [`ROBOTS_MAX_BYTES`]: длиннее — до последней целой строки предела.
fn cut(text: &str) -> &str {
    if text.len() <= ROBOTS_MAX_BYTES {
        return text;
    }
    match text.as_bytes()[..ROBOTS_MAX_BYTES]
        .iter()
        .rposition(|b| *b == b'\n')
    {
        // Перевод строки — однобайтовый символ: срез по границе символа.
        Some(i) => &text[..=i],
        None => "",
    }
}

/// Совпадение шаблона с началом пути: `*` — любая последовательность, `$` в конце —
/// конец пути. Жадно по буквальным частям между `*`: первая — префикс пути, каждая
/// следующая — самое левое вхождение после предыдущей (для шаблона только с `*` оно не
/// хуже любого другого), последняя при `$` — конец пути. Поиск подстроки `str::find`
/// линейный (Two-Way): O(длина пути + длина шаблона), без возвратов.
fn matches(pat: &str, path: &str) -> bool {
    let (pat, anchored) = match pat.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pat, false),
    };
    let mut parts = pat.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = path.strip_prefix(first) else {
        return false;
    };
    let mut parts = parts.peekable();
    if parts.peek().is_none() {
        // Шаблон без `*`: префикс пути, с `$` — путь целиком.
        return !anchored || rest.is_empty();
    }
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            // Последняя часть, перед ней `*`.
            return if anchored {
                rest.ends_with(part)
            } else {
                rest.contains(part)
            };
        }
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    true
}

/// Процентное кодирование к одному виду: байт вне ASCII и управляющий — `%XX`;
/// `%XX` незарезервированного символа (RFC 3986: буква, цифра, `-._~`) — сам символ,
/// прочие `%xx` — заглавными.
fn normalize(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'%'
            && let Some(v) = b.get(i + 1..i + 3).and_then(hex_pair)
        {
            if v.is_ascii_alphanumeric() || matches!(v, b'-' | b'.' | b'_' | b'~') {
                out.push(char::from(v));
            } else {
                out.push_str(&format!("%{v:02X}"));
            }
            i += 3;
            continue;
        }
        if c.is_ascii() && !c.is_ascii_control() && c != b' ' {
            out.push(char::from(c));
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
        i += 1;
    }
    out
}

fn hex_pair(p: &[u8]) -> Option<u8> {
    let d = |c: u8| char::from(c).to_digit(16);
    Some((d(p[0])? * 16 + d(p[1])?) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Эталон — прежняя динамика по позициям пути (O(шаблон × путь)), прямо по определению.
    fn matches_dp(pat: &[u8], path: &[u8]) -> bool {
        let (pat, anchored) = match pat.strip_suffix(b"$") {
            Some(p) => (p, true),
            None => (pat, false),
        };
        let mut cur = vec![false; path.len() + 1];
        cur[0] = true;
        for &c in pat {
            let mut next = vec![false; path.len() + 1];
            if c == b'*' {
                let mut any = false;
                for j in 0..=path.len() {
                    any |= cur[j];
                    next[j] = any;
                }
            } else {
                for j in 0..path.len() {
                    next[j + 1] = cur[j] && path[j] == c;
                }
            }
            cur = next;
        }
        if anchored {
            cur[path.len()]
        } else {
            cur.contains(&true)
        }
    }

    /// Жадный разбор совпадает с эталоном на всех шаблонах и путях из `/`, `a`, `b`, `*`
    /// (шаблоны до длины 4, с `$` и без; пути до длины 6) и на псевдослучайных длинных.
    #[test]
    fn greedy_matches_reference() {
        fn all(alpha: &[u8], len: usize) -> Vec<Vec<u8>> {
            let mut out = vec![Vec::new()];
            let mut level = vec![Vec::new()];
            for _ in 0..len {
                let mut next = Vec::new();
                for w in &level {
                    for &c in alpha {
                        let mut v: Vec<u8> = w.clone();
                        v.push(c);
                        next.push(v);
                    }
                }
                out.extend(next.iter().cloned());
                level = next;
            }
            out
        }
        let pats = all(b"/ab*", 4);
        let paths = all(b"/ab", 6);
        let mut checked = 0;
        for p in &pats {
            for anchored in [false, true] {
                let mut p = p.clone();
                if anchored {
                    p.push(b'$');
                }
                let ps = std::str::from_utf8(&p).unwrap();
                for x in &paths {
                    let xs = std::str::from_utf8(x).unwrap();
                    assert_eq!(matches(ps, xs), matches_dp(&p, x), "{ps:?} {xs:?}");
                    checked += 1;
                }
            }
        }
        // Длинные: линейный конгруэнтный генератор, без зависимостей.
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = |n: u64| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) % n
        };
        for _ in 0..20_000 {
            let plen = 1 + next(24) as usize;
            let xlen = next(40) as usize;
            let mut p: Vec<u8> = (0..plen).map(|_| b"ab**"[next(4) as usize]).collect();
            if next(2) == 0 {
                p.push(b'$');
            }
            let x: Vec<u8> = (0..xlen).map(|_| b"ab"[next(2) as usize]).collect();
            let (ps, xs) = (
                std::str::from_utf8(&p).unwrap(),
                std::str::from_utf8(&x).unwrap(),
            );
            assert_eq!(matches(ps, xs), matches_dp(&p, &x), "{ps:?} {xs:?}");
            checked += 1;
        }
        assert!(checked > 700_000, "{checked}");
    }
}
