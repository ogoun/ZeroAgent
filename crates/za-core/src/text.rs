//! Очистка невидимых символов Unicode (SEC-051, D102).
//!
//! Символы тегов U+E0000–U+E007F, нулевой ширины и управления направлением письма —
//! известные носители скрытых инструкций. Уровень очистки выбирает вызывающий по
//! происхождению текста ([`Clean`]); функция одна для результатов инструментов,
//! ввода пользователя и `AGENTS.md`.

use std::borrow::Cow;

/// Уровень очистки [`clean_invisible`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clean {
    /// Свои файлы проекта и непомеченные результаты нативных инструментов: только
    /// символы тегов. ZWJ, bidi и U+FEFF остаются, иначе `old` у `edit` перестаёт
    /// совпадать с файлом (D82).
    Tags,
    /// Недоверенное содержимое (результаты с меткой, веб, скиллы, PDF): теги, нулевая
    /// ширина U+200B–U+200F и U+2060–U+2064, U+FEFF не в начале, bidi U+202A–U+202E
    /// и U+2066–U+2069.
    Untrusted,
    /// Ввод пользователя, `AGENTS.md`: как [`Clean::Untrusted`], но U+200C (ZWNJ) и
    /// U+200D (ZWJ) остаются — они нужны в эмодзи и письменностях.
    Input,
}

impl Clean {
    /// Удаляется ли символ `c` на позиции байта `i` строки.
    fn drops(self, i: usize, c: char) -> bool {
        let u = u32::from(c);
        if (0xE0000..=0xE007F).contains(&u) {
            return true;
        }
        if self == Clean::Tags {
            return false;
        }
        if self == Clean::Input && (u == 0x200C || u == 0x200D) {
            return false;
        }
        (0x200B..=0x200F).contains(&u)
            || (0x2060..=0x2064).contains(&u)
            || (u == 0xFEFF && i > 0)
            || (0x202A..=0x202E).contains(&u)
            || (0x2066..=0x2069).contains(&u)
    }
}

/// Удаляет невидимые символы уровня `level`; итог — текст и число удалённых символов.
/// Чистый текст возвращается без копии (`Cow::Borrowed`). U+FEFF первым символом
/// остаётся: это BOM.
pub fn clean_invisible(s: &str, level: Clean) -> (Cow<'_, str>, usize) {
    // Один проход: копия начинается с первого удаляемого символа.
    let mut out: Option<String> = None;
    let mut removed = 0;
    for (i, c) in s.char_indices() {
        if level.drops(i, c) {
            removed += 1;
            out.get_or_insert_with(|| {
                let mut o = String::with_capacity(s.len());
                o.push_str(&s[..i]);
                o
            });
        } else if let Some(o) = out.as_mut() {
            o.push(c);
        }
    }
    match out {
        Some(o) => (Cow::Owned(o), removed),
        None => (Cow::Borrowed(s), 0),
    }
}

/// Символы, похожие на `<`: ASCII, одиночная кавычка-уголок, угловые скобки
/// (технические, математические, CJK, орнаментальные), малая и полноширинная формы,
/// гомоглифы (канадское слоговое PA, модификаторы-стрелки ˂ и ˱, «не меньше» ≮) и
/// невидимый тег-символ U+E003C (ASCII smuggling).
pub const LEFT_ANGLES: &[char] = &[
    '<',
    '\u{02C2}',
    '\u{02F1}',
    '\u{1438}',
    '\u{226E}',
    '\u{2039}',
    '\u{2329}',
    '\u{276C}',
    '\u{276E}',
    '\u{2770}',
    '\u{27E8}',
    '\u{27EA}',
    '\u{27EC}',
    '\u{2991}',
    '\u{2993}',
    '\u{2995}',
    '\u{29FC}',
    '\u{3008}',
    '\u{300A}',
    '\u{FE3F}',
    '\u{FE64}',
    '\u{FF1C}',
    '\u{E003C}',
];
/// Символы, похожие на `>` — пары к [`LEFT_ANGLES`].
pub const RIGHT_ANGLES: &[char] = &[
    '>',
    '\u{02C3}',
    '\u{02F2}',
    '\u{1433}',
    '\u{226F}',
    '\u{203A}',
    '\u{232A}',
    '\u{276D}',
    '\u{276F}',
    '\u{2771}',
    '\u{27E9}',
    '\u{27EB}',
    '\u{27ED}',
    '\u{2992}',
    '\u{2994}',
    '\u{2996}',
    '\u{29FD}',
    '\u{3009}',
    '\u{300B}',
    '\u{FE40}',
    '\u{FE65}',
    '\u{FF1E}',
    '\u{E003E}',
];

/// Замена символа при [`escape_untrusted`]: `&lt;`, `&gt;` или `None` — символ остаётся.
fn escape_of(c: char) -> Option<&'static str> {
    // Быстрый путь: из ASCII в списках только `<` и `>`.
    if c.is_ascii() && c != '<' && c != '>' {
        None
    } else if LEFT_ANGLES.contains(&c) {
        Some("&lt;")
    } else if RIGHT_ANGLES.contains(&c) {
        Some("&gt;")
    } else {
        None
    }
}

/// Экранирование разметки промпта в недоверенном тексте (D23, PRM-011, решение
/// контроллера, раунд 2; перенесено из `za-agent` в R1-C2, I2 ревью задачи 8: длину
/// экранированного текста меряют и инструменты, [`escaped_len`]).
///
/// Каждый символ из [`LEFT_ANGLES`] заменяется на `&lt;`, из [`RIGHT_ANGLES`] — на `&gt;`,
/// независимо от того, что за ним: обходы распознавания имени тега (невидимые символы
/// внутри имени, гомоглифы, полноширинные буквы, `ſ`) теряют смысл, потому что без
/// угловой скобки тега нет. Остальной текст не меняется, `&` не трогается, поэтому
/// функция идемпотентна. Цена — `Vec<String>` в выводе инструмента читается как
/// `Vec&lt;String&gt;`; кавычки «» не трогаются.
pub fn escape_untrusted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match escape_of(c) {
            Some(e) => out.push_str(e),
            None => out.push(c),
        }
    }
    out
}

/// Длина символа `c` после [`escape_untrusted`], байт.
pub fn escaped_char_len(c: char) -> usize {
    escape_of(c).map_or(c.len_utf8(), str::len)
}

/// Длина `s` после [`escape_untrusted`], байт, без копии: так меряет вывод слот пакета
/// (D44), и так инструмент укладывает `digest` в `ToolCx::output_budget` (D110).
pub fn escaped_len(s: &str) -> usize {
    s.chars().map(escaped_char_len).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_len_matches_escape() {
        for s in [
            "",
            "plain ascii",
            "<a> & <b>",
            "Vec<String>",
            "ёж \u{2039}x\u{203A} \u{FF1C}y\u{FF1E}",
            "\u{E003C}tag\u{E003E}",
            "&lt;already&gt;",
        ] {
            assert_eq!(escaped_len(s), escape_untrusted(s).len(), "{s:?}");
        }
        assert_eq!(escaped_char_len('<'), 4);
        assert_eq!(escaped_char_len('я'), 2);
        // Идемпотентность: экранированный текст не меняется.
        let once = escape_untrusted("<x>");
        assert_eq!(escape_untrusted(&once), once);
    }
}
