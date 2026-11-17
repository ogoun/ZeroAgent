//! Тип и кодировка ответа `web_fetch` (R1-C2 D109). Враждебный HTML здесь не разбирается
//! (D127): HTML уходит извлекателю уже в UTF-8.
//!
//! - **Тип — строго по `Content-Type`** (таблицы [`HTML_TYPES`], [`TEXT_TYPES`], `application/pdf`),
//!   прочее — [`Converted::Unsupported`]. Без заголовка — только сигнатура в начале тела
//!   после BOM и пробелов: `%PDF-` → PDF, `<!doctype html` или `<html` (без учёта регистра,
//!   целым именем тега) → HTML, иначе `text/plain`. Других эвристик типа нет.
//! - **Кодировка:** BOM; иначе `charset` заголовка; иначе (только HTML) `<meta charset>` или
//!   `http-equiv` в первых [`META_SCAN_BYTES`] байтах; иначе UTF-8. Декодирование
//!   `encoding_rs` с заменой. Неизвестная метка не действует (WHATWG).
//! - **`Content-Encoding`**, отличный от `identity`, — ошибка без распаковки (D107).

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE};
use za_core::extract::PDF_SIGNATURE;

/// Отметка порядка байт UTF-8.
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Типы, которые уходят извлекателю HTML (D109, D127).
pub const HTML_TYPES: &[&str] = &["text/html", "application/xhtml+xml"];

/// Типы, которые читаются как текст (D109).
pub const TEXT_TYPES: &[&str] = &[
    "text/plain",
    "text/markdown",
    "text/csv",
    "application/json",
    "application/xml",
    "text/xml",
    "application/rss+xml",
    "application/atom+xml",
];

/// Сколько первых байт тела просматривается в поисках `<meta>` с кодировкой (D109).
pub const META_SCAN_BYTES: usize = 1024;

/// Предел длины имени типа в [`Converted::Unsupported`]: значение уходит модели.
const TYPE_SHOWN_MAX: usize = 80;

/// Итог определения типа и кодировки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Converted {
    /// HTML, уже в UTF-8: дальше — извлекатель в подпроцессе (D127).
    Html { html: String },
    /// Текст, уже в UTF-8.
    Text { text: String },
    /// PDF: байты разбирает извлекатель (D113).
    Pdf,
    /// Тип вне таблицы D109: имя типа (нижний регистр, без параметров) для текста
    /// `unsupported content type <type>`.
    Unsupported(String),
}

/// Тип и кодировка ответа по заголовкам `Content-Type`, `Content-Encoding` и телу (D109).
/// `Err` — `unsupported content encoding <x>`.
pub fn convert(
    content_type: Option<&str>,
    content_encoding: Option<&str>,
    bytes: &[u8],
) -> Result<Converted, String> {
    if let Some(coding) = content_encoding.and_then(non_identity_coding) {
        return Err(format!("unsupported content encoding {coding}"));
    }
    let declared = content_type.and_then(MediaType::parse);
    let kind = match &declared {
        Some(t) if HTML_TYPES.contains(&t.essence.as_str()) => Kind::Html,
        Some(t) if TEXT_TYPES.contains(&t.essence.as_str()) => Kind::Text,
        Some(t) if t.essence == "application/pdf" => Kind::Pdf,
        Some(t) => return Ok(Converted::Unsupported(shown_type(&t.essence))),
        None => sniff(bytes),
    };
    if kind == Kind::Pdf {
        return Ok(Converted::Pdf);
    }
    let declared_charset = declared
        .as_ref()
        .and_then(|t| t.charset.as_deref())
        .and_then(|l| Encoding::for_label(l.as_bytes()));
    let encoding = declared_charset
        .or_else(|| (kind == Kind::Html).then(|| meta_charset(bytes)).flatten())
        .unwrap_or(UTF_8);
    // `decode` смотрит BOM первым и снимает его (BOM сильнее заголовка, WHATWG).
    let (text, _, _) = encoding.decode(bytes);
    let text = text.into_owned();
    Ok(match kind {
        Kind::Html => Converted::Html { html: text },
        _ => Converted::Text { text },
    })
}

/// Имя типа для строки `type:` результата `web_fetch` (D110): объявленный тип из таблиц
/// D109, если ответ разобран по нему, иначе тип по сигнатуре (`text/html`, `text/plain`).
/// Значение всегда из таблиц: заголовок сервера модели как есть не уходит.
pub(crate) fn type_name(content_type: Option<&str>, conv: &Converted) -> &'static str {
    let declared = content_type.and_then(MediaType::parse).map(|t| t.essence);
    let from = |table: &[&'static str], default: &'static str| {
        table
            .iter()
            .copied()
            .find(|t| declared.as_deref() == Some(*t))
            .unwrap_or(default)
    };
    match conv {
        Converted::Html { .. } => from(HTML_TYPES, "text/html"),
        Converted::Text { .. } => from(TEXT_TYPES, "text/plain"),
        Converted::Pdf => "application/pdf",
        Converted::Unsupported(_) => "application/octet-stream",
    }
}

/// Первое кодирование из значения `Content-Encoding`, кроме `identity`: нижний регистр,
/// только `[a-z0-9.+_-]`, до 40 символов (значение уходит модели); `None` — кодирования нет.
/// Тем же правилом пользуется шлюз (D107).
pub(crate) fn non_identity_coding(value: &str) -> Option<String> {
    let value = value.to_ascii_lowercase();
    for token in value.split(',').map(str::trim) {
        if token.is_empty() || token == "identity" {
            continue;
        }
        let shown: String = token
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '_' | '-'))
            .take(40)
            .collect();
        return Some(if shown.is_empty() {
            "unknown".into()
        } else {
            shown
        });
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Html,
    Text,
    Pdf,
}

/// Разобранный `Content-Type`: тип без параметров в нижнем регистре и `charset`.
struct MediaType {
    essence: String,
    charset: Option<String>,
}

impl MediaType {
    /// `None` — пустое значение (как отсутствие заголовка).
    fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split(';');
        let essence = parts.next().unwrap_or("").trim().to_ascii_lowercase();
        if essence.is_empty() {
            return None;
        }
        let charset = parts.find_map(|p| {
            let (name, v) = p.split_once('=')?;
            name.trim().eq_ignore_ascii_case("charset").then(|| {
                v.trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .trim()
                    .to_string()
            })
        });
        Some(Self { essence, charset })
    }
}

/// Имя типа для модели: символы имён RFC 6838 (`[a-z0-9!#$&^_.+-/]`), до 80 символов.
fn shown_type(essence: &str) -> String {
    let shown: String = essence
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "!#$&^_.+-/".contains(*c))
        .take(TYPE_SHOWN_MAX)
        .collect();
    if shown.is_empty() {
        "unknown".into()
    } else {
        shown
    }
}

/// Пробел ASCII по WHATWG: TAB, LF, FF, CR, SPACE.
fn is_ws(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\x0C' | b'\r' | b' ')
}

/// Тип по сигнатуре в начале тела после BOM и пробелов (D109).
fn sniff(bytes: &[u8]) -> Kind {
    if starts_with_pdf(bytes, &[]) {
        return Kind::Pdf;
    }
    let body = bytes.strip_prefix(BOM).unwrap_or(bytes);
    let start = body.iter().position(|&b| !is_ws(b)).unwrap_or(body.len());
    let body = &body[start..];
    if tag_prefix(body, b"<!doctype html") || tag_prefix(body, b"<html") {
        return Kind::Html;
    }
    Kind::Text
}

/// Тело, начало которого — `head`, а продолжение — `tail`, по сигнатуре — PDF: `%PDF-` после
/// BOM и пробелов (то же правило, что у [`convert`] без заголовка типа). Две части — для шлюза:
/// он решает на границе `max_bytes`, не склеивая уже прочитанное с новым куском (D113).
pub(crate) fn starts_with_pdf(head: &[u8], tail: &[u8]) -> bool {
    let all = head.iter().chain(tail).copied();
    let bom = all.clone().take(BOM.len()).eq(BOM.iter().copied());
    all.skip(if bom { BOM.len() } else { 0 })
        .skip_while(|&b| is_ws(b))
        .take(PDF_SIGNATURE.len())
        .eq(PDF_SIGNATURE.iter().copied())
}

/// Заголовок `Content-Type` объявляет PDF: `Some(true)` — `application/pdf`, `Some(false)` —
/// другой тип, `None` — заголовка нет или он пуст (тип — по сигнатуре, D109).
pub(crate) fn pdf_declared(content_type: Option<&str>) -> Option<bool> {
    content_type
        .and_then(MediaType::parse)
        .map(|t| t.essence == "application/pdf")
}

/// `body` начинается с `prefix` без учёта регистра, и дальше — конец, пробел или `>`
/// (имя тега целиком, как в сигнатурах WHATWG MIME Sniffing).
fn tag_prefix(body: &[u8], prefix: &[u8]) -> bool {
    body.len() >= prefix.len()
        && body[..prefix.len()].eq_ignore_ascii_case(prefix)
        && body
            .get(prefix.len())
            .is_none_or(|&b| is_ws(b) || b == b'>')
}

/// Кодировка из `<meta charset>` или `<meta http-equiv="content-type" content="…charset=…">`
/// в первых [`META_SCAN_BYTES`] байтах — упрощённый prescan WHATWG: комментарии
/// пропускаются, атрибуты в кавычках и без. UTF-16 из `<meta>` — это UTF-8 (байты, в которых
/// нашёлся `<meta>`, совместимы с ASCII).
fn meta_charset(bytes: &[u8]) -> Option<&'static Encoding> {
    let head = &bytes[..bytes.len().min(META_SCAN_BYTES)];
    let mut i = 0;
    while i < head.len() {
        let rest = &head[i..];
        if rest.starts_with(b"<!--") {
            // Незакрытый комментарий — до конца просмотра `<meta>` нет.
            i += 4 + find(&rest[4..], b"-->")? + 3;
            continue;
        }
        if rest.len() > 5
            && rest[..5].eq_ignore_ascii_case(b"<meta")
            && (is_ws(rest[5]) || rest[5] == b'/')
        {
            let (attrs, used) = attributes(&rest[5..]);
            if let Some(enc) = meta_encoding(&attrs) {
                return Some(if enc == UTF_16BE || enc == UTF_16LE {
                    UTF_8
                } else {
                    enc
                });
            }
            i += 5 + used;
            continue;
        }
        i += 1;
    }
    None
}

/// Кодировка одного `<meta>` по его атрибутам (имена в нижнем регистре).
fn meta_encoding(attrs: &[(String, String)]) -> Option<&'static Encoding> {
    let get = |n: &str| attrs.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
    if let Some(label) = get("charset") {
        return Encoding::for_label(label.trim().as_bytes());
    }
    let pragma = get("http-equiv").is_some_and(|v| v.trim().eq_ignore_ascii_case("content-type"));
    if pragma {
        return get("content").and_then(charset_in_content);
    }
    None
}

/// `charset=…` внутри атрибута `content` (алгоритм WHATWG «extracting a character encoding
/// from a meta element», упрощённо).
fn charset_in_content(content: &str) -> Option<&'static Encoding> {
    let lower = content.to_ascii_lowercase();
    let pos = lower.find("charset")?;
    let rest = lower[pos + "charset".len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let value = match rest.chars().next()? {
        q @ ('"' | '\'') => rest[1..].split(q).next().unwrap_or(""),
        _ => rest
            .split(|c: char| c.is_ascii_whitespace() || c == ';')
            .next()
            .unwrap_or(""),
    };
    Encoding::for_label(value.as_bytes())
}

/// Атрибуты тега до `>`: пары (имя в нижнем регистре, значение) и число прочитанных байт.
fn attributes(s: &[u8]) -> (Vec<(String, String)>, usize) {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        while i < s.len() && (is_ws(s[i]) || s[i] == b'/') {
            i += 1;
        }
        if i >= s.len() {
            return (out, i);
        }
        if s[i] == b'>' {
            return (out, i + 1);
        }
        let start = i;
        while i < s.len() && !is_ws(s[i]) && !matches!(s[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        let name = String::from_utf8_lossy(&s[start..i]).to_ascii_lowercase();
        while i < s.len() && is_ws(s[i]) {
            i += 1;
        }
        let mut value = String::new();
        if i < s.len() && s[i] == b'=' {
            i += 1;
            while i < s.len() && is_ws(s[i]) {
                i += 1;
            }
            if i < s.len() && matches!(s[i], b'"' | b'\'') {
                let q = s[i];
                let vstart = i + 1;
                let vend = s[vstart..]
                    .iter()
                    .position(|&b| b == q)
                    .map_or(s.len(), |p| vstart + p);
                value = String::from_utf8_lossy(&s[vstart..vend]).into_owned();
                i = (vend + 1).min(s.len());
            } else {
                let vstart = i;
                while i < s.len() && !is_ws(s[i]) && s[i] != b'>' {
                    i += 1;
                }
                value = String::from_utf8_lossy(&s[vstart..i]).into_owned();
            }
        }
        if !name.is_empty() {
            out.push((name, value));
        }
    }
}

/// Позиция `needle` в `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_signature_across_parts() {
        assert!(starts_with_pdf(b"%PDF-1.7", b""));
        assert!(starts_with_pdf(b"%PD", b"F-1.7"));
        assert!(starts_with_pdf(b"", b"%PDF-1.7"));
        assert!(starts_with_pdf(b"\xEF\xBB", b"\xBF \r\n%PDF-"));
        assert!(!starts_with_pdf(b"%PDF", b""));
        assert!(!starts_with_pdf(b"<html>", b"%PDF-"));
        assert!(!starts_with_pdf(b"%pdf-1.4", b""));
        assert_eq!(pdf_declared(Some("Application/PDF; q=1")), Some(true));
        assert_eq!(pdf_declared(Some("text/html")), Some(false));
        assert_eq!(pdf_declared(Some(" ")), None);
        assert_eq!(pdf_declared(None), None);
    }
}
