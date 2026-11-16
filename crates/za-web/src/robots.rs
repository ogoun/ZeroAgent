//! Разбор `robots.txt` (RFC 9309; D107) — свой, без зависимостей.
//!
//! Группа — строки `User-agent` подряд и правила после них. Действуют группы с токеном
//! агента (без учёта регистра; все такие группы объединяются), иначе группы `*`, иначе
//! разрешено всё. `Allow`/`Disallow` с `*` (любая последовательность) и `$` (конец пути,
//! только последним символом); побеждает самое длинное совпадение в октетах, при равенстве
//! — `Allow`. Пути сравниваются после нормализации процентного кодирования (RFC 9309,
//! 2.2.2): не-ASCII кодируется, `%XX` незарезервированного символа раскрывается.
//! `/robots.txt` разрешён всегда.

/// Разрешён ли путь (`/a/b?q`) агенту `agent` по тексту `robots`.
pub fn robots_allows(robots: &str, agent: &str, path: &str) -> bool {
    let path = normalize(path);
    if path == "/robots.txt" {
        return true;
    }
    let agent = agent.to_ascii_lowercase();
    let rules = rules_for(robots, &agent);
    let mut best: Option<(usize, bool)> = None;
    for (allow, pat) in &rules {
        if !matches(pat.as_bytes(), path.as_bytes()) {
            continue;
        }
        let len = pat.len();
        best = match best {
            Some((l, a)) if l > len || (l == len && a) => Some((l, a)),
            _ => Some((len, *allow)),
        };
    }
    best.is_none_or(|(_, allow)| allow)
}

/// Правила групп агента, иначе групп `*`: `(allow, нормализованный шаблон)`.
fn rules_for(robots: &str, agent: &str) -> Vec<(bool, String)> {
    let (mut own, mut star) = (Vec::new(), Vec::new());
    let (mut own_seen, mut star_seen) = (false, false);
    // Агенты текущей группы; `in_rules` — после агентов уже шли правила.
    let (mut is_own, mut is_star, mut in_rules) = (false, false, false);
    let text = robots.strip_prefix('\u{feff}').unwrap_or(robots);
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        match key.as_str() {
            "user-agent" => {
                if in_rules {
                    (is_own, is_star, in_rules) = (false, false, false);
                }
                let token = value
                    .split(['/', ' ', '\t'])
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if token == "*" {
                    is_star = true;
                    star_seen = true;
                } else if token == agent {
                    is_own = true;
                    own_seen = true;
                }
            }
            "allow" | "disallow" => {
                in_rules = true;
                if value.is_empty() {
                    continue;
                }
                let rule = (key == "allow", normalize(value));
                if is_own {
                    own.push(rule.clone());
                }
                if is_star {
                    star.push(rule);
                }
            }
            _ => {}
        }
    }
    if own_seen {
        own
    } else if star_seen {
        star
    } else {
        Vec::new()
    }
}

/// Совпадение шаблона с началом пути: `*` — любая последовательность, `$` в конце —
/// конец пути. Динамика по позициям пути: O(длина шаблона × длина пути), без возвратов.
fn matches(pat: &[u8], path: &[u8]) -> bool {
    let (pat, anchored) = match pat.strip_suffix(b"$") {
        Some(p) => (p, true),
        None => (pat, false),
    };
    // `cur[j]` — префикс шаблона длины `i` совпадает с `path[..j]`.
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
        if !cur.contains(&true) {
            return false;
        }
    }
    if anchored {
        cur[path.len()]
    } else {
        cur.contains(&true)
    }
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
