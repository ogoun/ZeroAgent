//! Декодирование вывода (D84; ревью задачи 5, M4): UTF-8, UTF-16 по BOM и UTF-16LE
//! без BOM, если весь текст ASCII (`cmd /u`); OEM 866 по кодовой странице консоли
//! Windows (`cmd`, `ipconfig`, `net` на русской Windows). Иначе — `from_utf8_lossy`.
//! Затем `\r\n` → `\n` и обрезка до предела по границе символа.

/// Кодовая страница OEM «кириллица DOS».
const CP866: u32 = 866;

/// Символы CP866 для байтов `0x80..=0xFF`.
const CP866_HIGH: [char; 128] = [
    'А', 'Б', 'В', 'Г', 'Д', 'Е', 'Ж', 'З', 'И', 'Й', 'К', 'Л', 'М', 'Н', 'О', 'П', //
    'Р', 'С', 'Т', 'У', 'Ф', 'Х', 'Ц', 'Ч', 'Ш', 'Щ', 'Ъ', 'Ы', 'Ь', 'Э', 'Ю', 'Я', //
    'а', 'б', 'в', 'г', 'д', 'е', 'ж', 'з', 'и', 'й', 'к', 'л', 'м', 'н', 'о', 'п', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'р', 'с', 'т', 'у', 'ф', 'х', 'ц', 'ч', 'ш', 'щ', 'ъ', 'ы', 'ь', 'э', 'ю', 'я', //
    'Ё', 'ё', 'Є', 'є', 'Ї', 'ї', 'Ў', 'ў', '°', '∙', '·', '√', '№', '¤', '■', '\u{a0}',
];

/// Нужна ли кодовая страница консоли: вывод не UTF-8 и не UTF-16.
pub(crate) fn needs_code_page(buf: &[u8]) -> bool {
    utf16_order(buf).is_none() && std::str::from_utf8(strip_utf8_bom(buf)).is_err()
}

/// Последнее число в выводе `chcp` («Active code page: 866», «Текущая кодовая страница:
/// 866» в любой кодировке — цифры ASCII).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn last_number(buf: &[u8]) -> Option<u32> {
    let end = buf.iter().rposition(u8::is_ascii_digit)? + 1;
    let start = buf[..end]
        .iter()
        .rposition(|b| !b.is_ascii_digit())
        .map_or(0, |i| i + 1);
    std::str::from_utf8(&buf[start..end]).ok()?.parse().ok()
}

/// Текст вывода не длиннее `limit` байт; `oem` — кодовая страница консоли.
pub(crate) fn decode(buf: &[u8], limit: usize, oem: Option<u32>) -> String {
    let text = match utf16_order(buf) {
        Some(little) => utf16(buf, little),
        None => {
            let b = strip_utf8_bom(buf);
            match std::str::from_utf8(b) {
                Ok(s) => s.to_owned(),
                Err(_) if oem == Some(CP866) => cp866(b),
                Err(_) => String::from_utf8_lossy(b).into_owned(),
            }
        }
    };
    let mut s = text.replace("\r\n", "\n");
    if s.len() > limit {
        let mut cut = limit;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

fn strip_utf8_bom(buf: &[u8]) -> &[u8] {
    buf.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(buf)
}

/// UTF-16: `Some(true)` — LE, `Some(false)` — BE. По BOM или для LE без BOM, если каждый
/// нечётный байт нулевой, а чётный нет (ASCII в UTF-16LE).
fn utf16_order(buf: &[u8]) -> Option<bool> {
    match buf {
        [0xFF, 0xFE, ..] => Some(true),
        [0xFE, 0xFF, ..] => Some(false),
        _ if buf.len() >= 4
            && buf.len().is_multiple_of(2)
            && buf.chunks(2).all(|p| p[0] != 0 && p[1] == 0) =>
        {
            Some(true)
        }
        _ => None,
    }
}

fn utf16(buf: &[u8], little: bool) -> String {
    let body = if buf.starts_with(&[0xFF, 0xFE]) || buf.starts_with(&[0xFE, 0xFF]) {
        &buf[2..]
    } else {
        buf
    };
    let units = body.as_chunks::<2>().0.iter().map(|p| {
        if little {
            u16::from_le_bytes(*p)
        } else {
            u16::from_be_bytes(*p)
        }
    });
    char::decode_utf16(units)
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

fn cp866(buf: &[u8]) -> String {
    buf.iter()
        .map(|&b| {
            if b < 0x80 {
                char::from(b)
            } else {
                CP866_HIGH[usize::from(b - 0x80)]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_and_crlf() {
        assert_eq!(decode("a\r\nб\n".as_bytes(), 100, None), "a\nб\n");
        assert_eq!(decode(b"\xEF\xBB\xBFok", 100, None), "ok");
        assert!(!needs_code_page("привет".as_bytes()));
    }

    #[test]
    fn utf16_with_bom_and_ascii_without() {
        let mut le = vec![0xFF, 0xFE];
        le.extend("Привет\r\n".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode(&le, 100, None), "Привет\n");
        let mut be = vec![0xFE, 0xFF];
        be.extend("ok".encode_utf16().flat_map(u16::to_be_bytes));
        assert_eq!(decode(&be, 100, None), "ok");
        let bare: Vec<u8> = "hello\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(decode(&bare, 100, None), "hello\n");
        assert!(!needs_code_page(&bare));
    }

    #[test]
    fn cp866_by_console_code_page() {
        // «Привет, Ёж ║» в CP866.
        let b = [
            0x8F, 0xE0, 0xA8, 0xA2, 0xA5, 0xE2, b',', b' ', 0xF0, 0xA6, b' ', 0xBA,
        ];
        assert!(needs_code_page(&b));
        assert_eq!(decode(&b, 100, Some(866)), "Привет, Ёж ║");
        // Другая страница — lossy UTF-8.
        assert!(decode(&b, 100, Some(437)).contains('\u{FFFD}'));
        assert!(decode(&b, 100, None).contains('\u{FFFD}'));
    }

    #[test]
    fn limit_is_kept_after_expansion() {
        let b = [0xFFu8; 10];
        assert!(decode(&b, 10, None).len() <= 10);
        assert!(decode(&[0x8F; 10], 10, Some(866)).len() <= 10);
    }

    #[test]
    fn chcp_number() {
        assert_eq!(last_number(b"Active code page: 866\r\n"), Some(866));
        assert_eq!(
            last_number(&[
                0x92, 0xA5, b':', b' ', b'6', b'5', b'0', b'0', b'1', b'\r', b'\n'
            ]),
            Some(65001)
        );
        assert_eq!(last_number(b"none"), None);
    }
}
