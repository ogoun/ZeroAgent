//! Тип и кодировка ответа (R1-C2 D109) и ранжирование абзацев `web_fetch` (D110).

use za_web::{Converted, convert, rank_passages};

#[test]
fn charset_from_header_and_meta() {
    let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode("Привет");
    assert!(
        matches!(convert(Some("text/plain; charset=windows-1251"), None, &cp1251), Ok(Converted::Text { text }) if text == "Привет")
    );
    let mut page = b"<meta charset=\"windows-1251\"><p>".to_vec();
    page.extend_from_slice(&cp1251);
    assert!(
        matches!(convert(Some("text/html"), None, &page), Ok(Converted::Html { html }) if html.contains("Привет"))
    );
}

#[test]
fn types_table() {
    assert!(matches!(
        convert(Some("application/pdf"), None, b"%PDF-1.7"),
        Ok(Converted::Pdf)
    ));
    assert!(matches!(
        convert(None, None, b"%PDF-1.4 x"),
        Ok(Converted::Pdf)
    ));
    assert!(matches!(
        convert(None, None, b"\xEF\xBB\xBF  <!DOCTYPE html><p>x"),
        Ok(Converted::Html { .. })
    )); // BOM и пробелы
    assert!(matches!(
        convert(None, None, b"just text <html> later"),
        Ok(Converted::Text { .. })
    )); // сигнатура только в начале
    assert!(
        matches!(convert(Some("image/png"), None, b"\x89PNG"), Ok(Converted::Unsupported(t)) if t == "image/png")
    );
    assert!(matches!(
        convert(Some("application/json"), None, b"{\"a\":1}"),
        Ok(Converted::Text { .. })
    ));
    assert!(
        matches!(convert(Some("text/html"), Some("gzip"), b"\x1f\x8b"), Err(e) if e == "unsupported content encoding gzip")
    );
}

/// Таблица типов D109 целиком: регистр, параметры, пробелы; заголовок решает без сигнатур.
#[test]
fn declared_types_are_strict() {
    for ct in [
        "text/html",
        "Text/HTML; Charset=UTF-8",
        " application/xhtml+xml ;q=1",
    ] {
        assert!(
            matches!(
                convert(Some(ct), None, b"%PDF-1.7 not pdf"),
                Ok(Converted::Html { .. })
            ),
            "{ct}"
        );
    }
    for ct in [
        "text/plain",
        "text/markdown",
        "text/csv",
        "application/json",
        "application/xml",
        "text/xml",
        "application/rss+xml",
        "application/atom+xml",
    ] {
        assert!(
            matches!(convert(Some(ct), None, b"<html><p>x"), Ok(Converted::Text { text }) if text == "<html><p>x"),
            "{ct}"
        );
    }
    assert!(matches!(
        convert(Some("APPLICATION/PDF; x=y"), None, b"not a pdf"),
        Ok(Converted::Pdf)
    ));
    for (ct, essence) in [
        ("image/png", "image/png"),
        ("Application/Octet-Stream", "application/octet-stream"),
        ("text/javascript; charset=utf-8", "text/javascript"),
        ("nonsense", "nonsense"),
    ] {
        assert!(
            matches!(convert(Some(ct), None, b"<html>"), Ok(Converted::Unsupported(t)) if t == essence),
            "{ct}"
        );
    }
    // Пустой заголовок — как его отсутствие: решает сигнатура.
    assert!(matches!(
        convert(Some(" "), None, b"%PDF-1.7"),
        Ok(Converted::Pdf)
    ));
}

/// Без заголовка — только сигнатура в начале (после BOM и пробелов), без учёта регистра;
/// `<html` — только целым именем тега.
#[test]
fn signatures_without_header() {
    for body in [
        &b"<html>"[..],
        b"<HTML lang=en>",
        b"\n\t <!doctype HTML>",
        b"<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0//EN\">",
        b"\xEF\xBB\xBF<html>",
        b"<html",
    ] {
        assert!(
            matches!(convert(None, None, body), Ok(Converted::Html { .. })),
            "{:?}",
            String::from_utf8_lossy(body)
        );
    }
    for body in [
        &b"<htmlx>"[..],
        b"<!doctype htm>",
        b"x<html>",
        b"",
        b"<p>no doctype",
    ] {
        assert!(
            matches!(convert(None, None, body), Ok(Converted::Text { .. })),
            "{:?}",
            String::from_utf8_lossy(body)
        );
    }
    assert!(matches!(
        convert(None, None, b"\r\n %PDF-1.4"),
        Ok(Converted::Pdf)
    ));
    assert!(matches!(
        convert(None, None, b"%pdf-1.4"),
        Ok(Converted::Text { .. })
    ));
}

/// `Content-Encoding`: `identity` и пустое значение допустимы, остальное — ошибка с именем
/// кодирования (без распаковки).
#[test]
fn content_encoding_other_than_identity_is_error() {
    assert!(matches!(
        convert(Some("text/plain"), Some("identity"), b"x"),
        Ok(Converted::Text { .. })
    ));
    assert!(matches!(
        convert(Some("text/plain"), Some(" Identity "), b"x"),
        Ok(Converted::Text { .. })
    ));
    assert!(matches!(
        convert(Some("text/plain"), Some(""), b"x"),
        Ok(Converted::Text { .. })
    ));
    for (enc, shown) in [
        ("br", "br"),
        ("GZIP", "gzip"),
        ("identity, deflate", "deflate"),
    ] {
        assert!(
            matches!(convert(Some("text/plain"), Some(enc), b"x"), Err(e) if e == format!("unsupported content encoding {shown}")),
            "{enc}"
        );
    }
}

/// Порядок источников кодировки: BOM, затем `charset` заголовка, затем `<meta>` (только у
/// HTML, в первых 1024 байтах), иначе UTF-8 с заменой.
#[test]
fn encoding_sources_order() {
    let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode("Привет");
    let html = |prefix: &[u8]| {
        let mut v = prefix.to_vec();
        v.extend_from_slice(&cp1251);
        v
    };
    let text_of = |c: Result<Converted, String>| match c {
        Ok(Converted::Html { html }) => html,
        Ok(Converted::Text { text }) => text,
        other => panic!("{other:?}"),
    };

    // Кавычки в заголовке и `http-equiv`.
    let t = text_of(convert(
        Some("text/plain; charset=\"Windows-1251\""),
        None,
        &cp1251,
    ));
    assert_eq!(t, "Привет");
    let page =
        html(b"<META HTTP-EQUIV='Content-Type' CONTENT='text/html; charset=windows-1251'><p>");
    assert!(text_of(convert(Some("text/html"), None, &page)).contains("Привет"));
    // Без заголовка: сигнатура HTML, кодировка из <meta>.
    let page = html(b"<!doctype html><meta charset=windows-1251><p>");
    assert!(text_of(convert(None, None, &page)).contains("Привет"));
    // Неизвестная метка заголовка не действует: дальше <meta>.
    let page = html(b"<meta charset=windows-1251><p>");
    assert!(text_of(convert(Some("text/html; charset=bogus-x"), None, &page)).contains("Привет"));
    // Заголовок сильнее <meta>.
    let (koi, _, _) = encoding_rs::KOI8_R.encode("Привет");
    let mut page = b"<meta charset=windows-1251><p>".to_vec();
    page.extend_from_slice(&koi);
    assert!(text_of(convert(Some("text/html; charset=koi8-r"), None, &page)).contains("Привет"));
    // BOM сильнее заголовка.
    let mut bom = b"\xEF\xBB\xBF".to_vec();
    bom.extend_from_slice("Привет".as_bytes());
    assert_eq!(
        text_of(convert(
            Some("text/plain; charset=windows-1251"),
            None,
            &bom
        )),
        "Привет"
    );
    // <meta> дальше 1024 байт не читается.
    let mut far = vec![b' '; 1100];
    far.extend_from_slice(b"<meta charset=windows-1251><p>");
    far.extend_from_slice(&cp1251);
    assert!(!text_of(convert(Some("text/html"), None, &far)).contains("Привет"));
    // <meta> в закомментированном блоке не действует.
    let page = html(b"<!-- <meta charset=windows-1251> --><p>");
    assert!(!text_of(convert(Some("text/html"), None, &page)).contains("Привет"));
    // У текстовых типов <meta> не читается.
    let page = html(b"<meta charset=windows-1251>");
    assert!(!text_of(convert(Some("text/plain"), None, &page)).contains("Привет"));
    // <meta> с UTF-16 — это UTF-8 (WHATWG: байты уже совместимы с ASCII).
    let t = text_of(convert(
        Some("text/html"),
        None,
        "<meta charset=utf-16le><p>ё".as_bytes(),
    ));
    assert!(t.contains('ё'), "{t:?}");
    // Битый UTF-8 — замена, а не ошибка.
    let t = text_of(convert(Some("text/plain"), None, b"a\xFFb"));
    assert_eq!(t, "a\u{FFFD}b");
}

/// Длинный текст: 30 абзацев без общих слов с запросом и три совпадающих в разных местах.
fn long_text() -> String {
    let mut paras: Vec<String> = (0..30)
        .map(|i| format!("unrelated cooking recipe number {i} with flour, sugar and butter"))
        .collect();
    paras.insert(3, "rust async tasks are polled by an executor".into());
    paras.insert(
        16,
        "the tokio runtime spawn function hands tasks to workers".into(),
    );
    paras.insert(28, "each runtime worker steals queued tasks".into());
    paras.join("\n\n")
}

#[test]
fn rank_keeps_matching_paragraphs_in_order() {
    let r = rank_passages(&long_text(), "how does the tokio runtime spawn tasks", 400);
    assert!(
        r.find("rust async").unwrap() < r.find("tokio runtime spawn").unwrap(),
        "{r}"
    );
    assert!(r.contains("steals queued tasks"), "{r}");
    assert!(!r.contains("cooking"), "{r}"); // голова страницы — только в остатке бюджета без повторов (D110)
    assert!(r.len() <= 400);
}

#[test]
fn rank_without_matches_returns_head() {
    let text = long_text();
    let head = text.split('\n').take(20).collect::<Vec<_>>().join("\n");
    let r = rank_passages(&text, "quantum entanglement", 1000);
    assert!(text.len() > 1000 && head.len() < 1000);
    assert_eq!(r, head.trim_end());
    // Запрос только из стоп-слов и коротких слов — тоже без совпадений.
    assert_eq!(
        rank_passages(&text, "how is it the and of", 1000),
        head.trim_end()
    );
    // Голова длиннее бюджета режется по нему.
    let r = rank_passages(&text, "quantum", 100);
    assert!(r.len() <= 100 && head.starts_with(&r), "{r:?}");
}

/// Голова, которая помещается в остаток 40 %, добавляется без повтора взятых абзацев; итог —
/// в исходном порядке.
#[test]
fn rank_adds_head_within_remaining_budget() {
    let mut text = String::from("site intro\n\ntokio runtime spawn tasks\n\n");
    for i in 1..=16 {
        text.push_str(&format!("x{i}\n"));
    }
    text.push('\n');
    for i in 0..60 {
        text.push_str(&format!("cooking filler paragraph {i}\n\n"));
    }
    text.push_str("later runtime note");
    let r = rank_passages(&text, "tokio runtime spawn tasks", 1000);
    assert!(
        r.starts_with("site intro\n\ntokio runtime spawn tasks\n\nx1\nx2"),
        "{r}"
    );
    assert!(r.ends_with("x16\n\nlater runtime note"), "{r}");
    assert_eq!(r.matches("tokio runtime spawn tasks").count(), 1, "{r}");
    assert!(!r.contains("cooking"), "{r}");
}

/// Больше общих слов — выше место: при тесном бюджете остаётся лучший абзац.
#[test]
fn rank_prefers_more_common_words() {
    let mut text = String::from("tasks only here once\n\n");
    for i in 0..40 {
        text.push_str(&format!("cooking filler paragraph {i}\n\n"));
    }
    text.push_str("tokio runtime spawn tasks all together");
    let r = rank_passages(&text, "tokio runtime spawn tasks", 80);
    assert_eq!(r, "tokio runtime spawn tasks all together");
}

/// Итог не длиннее бюджета; абзац больше доли совпадений режется по строкам; текст в пределах
/// бюджета возвращается как есть.
#[test]
fn rank_respects_budget_and_splits_huge_paragraphs() {
    let text = long_text();
    for budget in [0, 1, 7, 50, 120, 333, 999] {
        let r = rank_passages(&text, "tokio runtime spawn tasks", budget);
        assert!(r.len() <= budget, "{budget}: {}", r.len());
    }
    // Один огромный абзац без пустых строк: совпадение в середине всё равно находится.
    let mut huge = String::new();
    for i in 0..500 {
        huge.push_str(&format!("cooking line {i}\n"));
        if i == 250 {
            huge.push_str("tokio runtime spawn tasks right here\n");
        }
    }
    let r = rank_passages(&huge, "tokio runtime spawn", 300);
    assert!(r.contains("tokio runtime spawn tasks right here"), "{r}");
    assert!(r.len() <= 300);
    // Одна строка длиннее бюджета режется по границе символа.
    let line = "ёж tokio ".repeat(200);
    let r = rank_passages(&line, "tokio", 101);
    assert!(
        r.len() <= 101 && !r.is_empty() && line.starts_with(&r),
        "{r:?}"
    );
    // Текст в пределах бюджета — без изменений.
    assert_eq!(
        rank_passages("a\n\n\nb cooking", "tokio", 100),
        "a\n\n\nb cooking"
    );
}
