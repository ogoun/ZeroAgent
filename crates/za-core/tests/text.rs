//! Очистка невидимых символов (D102, SEC-051): три уровня `Tags`, `Untrusted`, `Input`.

use std::borrow::Cow;

use za_core::text::{Clean, clean_invisible};

#[test]
fn tags_level_keeps_prior_untagged_behaviour() {
    // Перенос unit-теста `invisible_chars_cleaned` из `za-agent/src/tools.rs`:
    // прежний `clean_invisible(_, false)` == `Clean::Tags`, `true` == `Clean::Untrusted`.
    assert_eq!(clean_invisible("a\u{E0041}b", Clean::Tags).0, "ab");
    let zw = "\u{FEFF}x\u{200B}y\u{202E}z\u{FEFF}";
    assert_eq!(clean_invisible(zw, Clean::Tags).0, zw);
    assert_eq!(clean_invisible(zw, Clean::Untrusted).0, "\u{FEFF}xyz");
}

#[test]
fn untrusted_removes_all_classes() {
    let s = format!(
        "a{}b{}c{}d{}e",
        '\u{E0041}', '\u{200B}', '\u{202E}', '\u{200D}'
    );
    assert_eq!(
        clean_invisible(&s, Clean::Untrusted),
        (Cow::Owned("abcde".into()), 4)
    );
}

#[test]
fn input_keeps_zwj_and_zwnj() {
    let family = "\u{1F468}\u{200D}\u{1F469}";
    assert_eq!(clean_invisible(family, Clean::Input).1, 0);
    assert_eq!(clean_invisible("x\u{2066}y", Clean::Input).0, "xy");
    // U+200C (ZWNJ) нужен персидскому и индийским письменностям.
    let zwnj = "\u{0645}\u{06CC}\u{200C}\u{062E}\u{0648}\u{0627}\u{0647}\u{0645}";
    assert_eq!(
        clean_invisible(zwnj, Clean::Input),
        (Cow::Borrowed(zwnj), 0)
    );
    // Остальное нулевой ширины уходит и на уровне ввода.
    assert_eq!(
        clean_invisible("a\u{200B}b\u{2060}c\u{E0067}", Clean::Input),
        (Cow::Owned("abc".into()), 3)
    );
}

#[test]
fn leading_bom_is_kept_inner_removed() {
    assert_eq!(
        clean_invisible("\u{FEFF}a\u{FEFF}b", Clean::Untrusted).0,
        "\u{FEFF}ab"
    );
    assert_eq!(
        clean_invisible("\u{FEFF}a\u{FEFF}b", Clean::Input),
        (Cow::Owned("\u{FEFF}ab".into()), 1)
    );
}

#[test]
fn clean_text_is_borrowed() {
    assert!(matches!(
        clean_invisible("plain", Clean::Input).0,
        Cow::Borrowed(_)
    ));
    assert!(matches!(
        clean_invisible("\u{FEFF}plain", Clean::Untrusted),
        (Cow::Borrowed(_), 0)
    ));
}

#[test]
fn range_bounds_exact() {
    // Границы диапазонов D102: соседние символы не трогаются.
    for c in [
        '\u{DFFFF}',
        '\u{E0080}',
        '\u{200A}',
        '\u{2010}',
        '\u{2065}',
        '\u{2029}',
        '\u{206A}',
    ] {
        let s = format!("a{c}b");
        assert_eq!(clean_invisible(&s, Clean::Untrusted).1, 0, "{:X}", c as u32);
    }
    for c in [
        '\u{E0000}',
        '\u{E007F}',
        '\u{200F}',
        '\u{2064}',
        '\u{202A}',
        '\u{2069}',
    ] {
        let s = format!("a{c}b");
        assert_eq!(clean_invisible(&s, Clean::Untrusted).1, 1, "{:X}", c as u32);
    }
}
