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
