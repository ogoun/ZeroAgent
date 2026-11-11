//! Сканер секретов и маскировка (D90, D100).
//!
//! [`SecretMasker::mask`] заменяет в тексте:
//! - известные значения (от [`MIN_SECRET_LEN`] символов) и их форму в строке JSON
//!   (`\"`, `\\`, управляющие символы) → `[secret masked: <имя>]`;
//! - шаблоны ключей и токенов с границей слова слева → `[secret masked: <вид>]`;
//! - блок приватного ключа PEM до `-----END … -----`, а без `END` — до конца текста;
//! - пароль в URL (`://user:pass@`) и в строке подключения (`;Password=…;`) — только
//!   сам пароль.
//!
//! Без `regex` (D90): один проход по байтам с таблицей префиксов. Сначала собираются все
//! найденные участки (вхождения известных значений — и перекрывающиеся), потом
//! пересекающиеся участки сливаются в один: в маскированном тексте не остаётся частей
//! секрета, даже если шаблон кончается внутри известного значения.
//!
//! Ограничения (записанный риск): значение в base64, URL-кодировке или с экранированием
//! `\uXXXX` обычных символов не узнаётся; шаблоны — только перечисленные в [`RULES`].

use std::borrow::Cow;
use std::fmt;

use crate::store::MIN_SECRET_LEN;

/// Начало метки маски: `[secret masked: <имя или вид>]`.
pub const MASK_PREFIX: &str = "[secret masked: ";

/// Маскировщик: известные значения и шаблоны D90.
#[derive(Clone, Default)]
pub struct SecretMasker {
    /// Искомые строки по убыванию длины, без повторов.
    known: Vec<Needle>,
}

#[derive(Clone)]
struct Needle {
    text: String,
    label: String,
}

/// Найденный участок `start..end` (байты) и метка.
struct Span<'m> {
    start: usize,
    end: usize,
    label: &'m str,
}

impl SecretMasker {
    /// Маскировщик по парам (имя, значение); значения короче [`MIN_SECRET_LEN`]
    /// символов не ищутся.
    pub fn new(known: Vec<(String, String)>) -> Self {
        let mut needles = Vec::new();
        for (label, value) in known {
            if value.chars().count() < MIN_SECRET_LEN {
                continue;
            }
            let escaped = json_escaped(&value);
            if escaped != value {
                needles.push(Needle {
                    text: escaped,
                    label: label.clone(),
                });
            }
            needles.push(Needle { text: value, label });
        }
        needles.sort_by(|a, b| {
            b.text
                .len()
                .cmp(&a.text.len())
                .then_with(|| a.text.cmp(&b.text))
        });
        needles.dedup_by(|a, b| a.text == b.text);
        Self { known: needles }
    }

    /// Текст с замаскированными секретами; без находок — исходный текст без копии.
    pub fn mask<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let mut spans = Vec::new();
        for n in &self.known {
            let mut from = 0;
            while let Some(p) = s[from..].find(n.text.as_str()) {
                let start = from + p;
                spans.push(Span {
                    start,
                    end: start + n.text.len(),
                    label: &n.label,
                });
                // Следующее вхождение может начинаться внутри этого.
                from = start + s[start..].chars().next().map_or(1, char::len_utf8);
            }
        }
        pattern_spans(s, &mut spans);
        if spans.is_empty() {
            return Cow::Borrowed(s);
        }
        // Устойчивая сортировка: при равных участках метка известного значения раньше.
        spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        let mut out = String::with_capacity(s.len());
        let mut pos = 0;
        let mut it = spans.into_iter().peekable();
        while let Some(first) = it.next() {
            let mut end = first.end;
            while let Some(next) = it.peek() {
                if next.start >= end {
                    break;
                }
                end = end.max(next.end);
                it.next();
            }
            out.push_str(&s[pos..first.start]);
            out.push_str(MASK_PREFIX);
            out.push_str(first.label);
            out.push(']');
            pos = end;
        }
        out.push_str(&s[pos..]);
        Cow::Owned(out)
    }
}

impl fmt::Debug for SecretMasker {
    /// Только имена известных значений: сами значения не печатаются.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut labels: Vec<&str> = self.known.iter().map(|n| n.label.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        f.debug_struct("SecretMasker")
            .field("known", &labels)
            .finish()
    }
}

/// Форма значения внутри строки JSON (как пишет `serde_json`).
fn json_escaped(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}

/// Шаблон токена: префиксы, вид, класс символов тела, длина тела и доп. проверка.
struct Rule {
    prefixes: &'static [&'static str],
    kind: &'static str,
    body: fn(u8) -> bool,
    min: usize,
    max: usize,
    /// После тела не должно идти символа слова (`AKIA…` ровно из 20 символов).
    right_boundary: bool,
    /// Проверка тела; `None` — не нужна.
    check: Option<fn(&[u8]) -> bool>,
}

/// Шаблоны ключей и токенов D90 (и несколько распространённых форматов провайдеров).
/// Каждый требует границы слова слева: `myghp_value` и `task-runner` не находятся.
const RULES: &[Rule] = &[
    Rule {
        // OpenAI, Anthropic (`sk-ant-`), OpenRouter (`sk-or-`), DeepSeek и др.
        prefixes: &["sk-"],
        kind: "api_key",
        body: is_b64url,
        min: 20,
        max: usize::MAX,
        right_boundary: false,
        check: Some(random_looking),
    },
    Rule {
        prefixes: &["sk_live_", "sk_test_", "rk_live_", "rk_test_"],
        kind: "stripe_key",
        body: is_alnum,
        min: 16,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        prefixes: &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"],
        kind: "github_token",
        body: is_alnum,
        min: 36,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        prefixes: &["github_pat_"],
        kind: "github_token",
        body: is_word,
        min: 22,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        prefixes: &["AKIA", "ASIA"],
        kind: "aws_access_key",
        body: is_upper_digit,
        min: 16,
        max: 16,
        right_boundary: true,
        check: None,
    },
    Rule {
        prefixes: &["xoxa-", "xoxb-", "xoxp-", "xoxo-", "xoxs-", "xoxr-"],
        kind: "slack_token",
        body: is_alnum_dash,
        min: 10,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        prefixes: &["AIza"],
        kind: "google_api_key",
        body: is_b64url,
        min: 35,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        prefixes: &["glpat-"],
        kind: "gitlab_token",
        body: is_b64url,
        min: 20,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        prefixes: &["hf_"],
        kind: "huggingface_token",
        body: is_alnum,
        min: 30,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        prefixes: &["npm_"],
        kind: "npm_token",
        body: is_alnum,
        min: 36,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        // Макарон PyPI: `pypi-` и base64 заголовка `pypi.org`.
        prefixes: &["pypi-AgEIcHlwaS5vcmc"],
        kind: "pypi_token",
        body: is_b64url,
        min: 50,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
];

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}
fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}
fn is_alnum_dash(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-'
}
fn is_b64url(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_'
}
fn is_upper_digit(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit()
}

/// Тело `sk-` похоже на случайное: есть цифра и участок из 16 букв и цифр подряд.
/// Имена вида `sk-video-processing-toolkit` (слова через дефис) не маскируются.
fn random_looking(body: &[u8]) -> bool {
    let digit = body.iter().any(u8::is_ascii_digit);
    let run = body
        .split(|c| !c.is_ascii_alphanumeric())
        .map(<[u8]>::len)
        .max()
        .unwrap_or(0);
    digit && run >= 16
}

/// Граница слова слева от `i`: начало текста или не `[A-Za-z0-9_]` (не-ASCII — граница).
fn boundary(b: &[u8], i: usize) -> bool {
    i == 0 || !is_word(b[i - 1])
}

/// Все участки шаблонов в `s`.
fn pattern_spans(s: &str, out: &mut Vec<Span<'_>>) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b':'
            && let Some((start, end)) = url_password(b, i)
        {
            out.push(Span {
                start,
                end,
                label: "url_password",
            });
        }
        if c == b'-'
            && let Some(end) = pem_block(s, i)
        {
            out.push(Span {
                start: i,
                end,
                label: "private_key",
            });
            i = end;
            continue;
        }
        if c.is_ascii_alphabetic() && boundary(b, i) {
            if let Some((end, kind)) = token(b, i) {
                out.push(Span {
                    start: i,
                    end,
                    label: kind,
                });
                i = end;
                continue;
            }
            if let Some((start, end)) = connection_password(b, i) {
                out.push(Span {
                    start,
                    end,
                    label: "connection_password",
                });
                i = end;
                continue;
            }
        }
        i += 1;
    }
}

/// Токен по [`RULES`] или JWT, начинающийся в `i`: (конец, вид).
fn token(b: &[u8], i: usize) -> Option<(usize, &'static str)> {
    let rest = &b[i..];
    for r in RULES {
        let Some(p) = r.prefixes.iter().find(|p| rest.starts_with(p.as_bytes())) else {
            continue;
        };
        let from = i + p.len();
        let end = from + b[from..].iter().take_while(|c| (r.body)(**c)).count();
        let len = end - from;
        if len < r.min || len > r.max {
            continue;
        }
        if r.right_boundary && b.get(end).is_some_and(|c| is_word(*c)) {
            continue;
        }
        if r.check.is_some_and(|f| !f(&b[from..end])) {
            continue;
        }
        return Some((end, r.kind));
    }
    jwt(b, i).map(|end| (end, "jwt"))
}

/// JWT: `eyJ…` (заголовок), `.`, `eyJ…` (данные), `.`, подпись (может быть пустой).
fn jwt(b: &[u8], i: usize) -> Option<usize> {
    let seg = |from: usize| from + b[from..].iter().take_while(|c| is_b64url(**c)).count();
    if !b[i..].starts_with(b"eyJ") {
        return None;
    }
    let h = seg(i);
    if h - i < 10 || b.get(h) != Some(&b'.') || !b[h + 1..].starts_with(b"eyJ") {
        return None;
    }
    let p = seg(h + 1);
    if p - (h + 1) < 10 || b.get(p) != Some(&b'.') {
        return None;
    }
    Some(seg(p + 1))
}

/// Блок приватного ключа PEM с `-----BEGIN … PRIVATE KEY-----` в `i`: конец блока —
/// после `-----END …-----`, без `END` — конец текста.
fn pem_block(s: &str, i: usize) -> Option<usize> {
    const BEGIN: &str = "-----BEGIN ";
    const END: &str = "-----END ";
    const DASHES: &str = "-----";
    let rest = s[i..].strip_prefix(BEGIN)?;
    let h = rest.find(DASHES)?;
    let label = &rest[..h];
    if label.contains('\n') || !label.contains("PRIVATE KEY") {
        return None;
    }
    let body = i + BEGIN.len() + h + DASHES.len();
    let Some(e) = s[body..].find(END) else {
        return Some(s.len());
    };
    let after = body + e + END.len();
    Some(
        s[after..]
            .find(DASHES)
            .map_or(s.len(), |f| after + f + DASHES.len()),
    )
}

/// Пароль в URL: `i` — двоеточие `://`; участок пароля в `scheme://user:pass@host`.
fn url_password(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if !b[i..].starts_with(b"://") || i == 0 || !b[i - 1].is_ascii_alphanumeric() {
        return None;
    }
    let a = i + 3;
    let stop = |c: &u8| {
        matches!(
            c,
            b'/' | b'?' | b'#' | b'"' | b'\'' | b'<' | b'>' | b'`' | b'\\'
        ) || c.is_ascii_whitespace()
    };
    let e = a + b[a..].iter().position(stop).unwrap_or(b.len() - a);
    let at = a + b[a..e].iter().rposition(|c| *c == b'@')?;
    let colon = a + b[a..at].iter().position(|c| *c == b':')?;
    (colon + 1 < at).then_some((colon + 1, at))
}

/// Пароль строки подключения: `Password=…` или `Pwd=…` (без учёта регистра) после
/// начала текста, `;`, `?`, `&`, кавычки или пробела; значение — до `;`, `&`, кавычки
/// или пробела. Пустое значение и подстановки (`$X`, `%X%`, `{x}`, `<x>`) не трогаются.
fn connection_password(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if i > 0
        && !matches!(b[i - 1], b';' | b'?' | b'&' | b'"' | b'\'')
        && !b[i - 1].is_ascii_whitespace()
    {
        return None;
    }
    let key = [&b"password="[..], &b"pwd="[..]]
        .into_iter()
        .find(|k| b.len() - i >= k.len() && b[i..i + k.len()].eq_ignore_ascii_case(k))?;
    let start = i + key.len();
    let end = start
        + b[start..]
            .iter()
            .position(|c| matches!(c, b';' | b'&' | b'"' | b'\'') || c.is_ascii_whitespace())
            .unwrap_or(b.len() - start);
    let value = &b[start..end];
    if value.is_empty()
        || matches!(value[0], b'$' | b'%' | b'{' | b'<')
        || b[start..].starts_with(MASK_PREFIX.as_bytes())
    {
        return None;
    }
    Some((start, end))
}
