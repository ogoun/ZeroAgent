//! Текст страниц PDF (R1-C2 D113): окно страниц, предел текста, битый, обрезанный и
//! зашифрованный документ — ошибка, а не паника. PDF собираются из байтов теста
//! (`test-support/pdf.rs`, D121).

use za_extract::{ExtractError, pdf_pages};

#[path = "../../../test-support/pdf.rs"]
mod pdf;

use pdf::{pdf_with_pages, two_page_pdf, xref_offset};

#[test]
fn two_pages() {
    let t = pdf_pages(&two_page_pdf(), 1, 10, 100_000).unwrap();
    assert_eq!(t.pages_total, 2);
    assert_eq!(t.pages.len(), 2);
    assert_eq!(t.pages[0].0, 1);
    assert_eq!(t.pages[0].1.trim(), "alpha");
    assert_eq!(t.pages[1].0, 2);
    assert_eq!(t.pages[1].1.trim(), "beta");
}

#[test]
fn page_window() {
    assert_eq!(
        pdf_pages(&two_page_pdf(), 2, 1, 100_000).unwrap().pages,
        vec![(2, "beta".into())]
    );
    // Окно за последней страницей — пусто, число страниц известно.
    let t = pdf_pages(&two_page_pdf(), 3, 5, 100_000).unwrap();
    assert_eq!((t.pages_total, t.pages.len()), (2, 0));
    // Окно больше документа — до последней страницы.
    let t = pdf_pages(&pdf_with_pages(&["a", "b", "c", "d"]), 2, 100, 100_000).unwrap();
    let nums: Vec<u32> = t.pages.iter().map(|p| p.0).collect();
    assert_eq!(nums, vec![2, 3, 4]);
}

/// Страница без текстового слоя (скан) — пустой текст, а не ошибка.
#[test]
fn page_without_text_layer_is_empty() {
    let t = pdf_pages(&pdf_with_pages(&["alpha", "", "gamma"]), 1, 10, 100_000).unwrap();
    assert_eq!(t.pages_total, 3);
    assert_eq!(t.pages[1], (2, String::new()));
    assert_eq!(t.pages[2].1, "gamma");
}

/// Предел текста всех страниц: последняя страница обрезана, следующие не читаются.
#[test]
fn text_limit_cuts_pages() {
    let doc = pdf_with_pages(&["aaaaaaaaaa", "bbbbbbbbbb", "cccccccccc"]);
    let t = pdf_pages(&doc, 1, 10, 15).unwrap();
    assert_eq!(t.pages_total, 3);
    let total: usize = t.pages.iter().map(|p| p.1.chars().count()).sum();
    assert_eq!(total, 15, "{:?}", t.pages);
    assert_eq!(t.pages.len(), 2, "{:?}", t.pages);
    assert_eq!(t.pages[1].1, "bbbbb");
}

#[test]
fn garbage_is_malformed() {
    assert!(matches!(
        pdf_pages(b"%PDF-1.4 garbage", 1, 1, 10),
        Err(ExtractError::Malformed(_))
    ));
}

/// Документ без хвоста с таблицей `xref` (обрезанное тело ответа) — `Malformed`, без паники.
#[test]
fn truncated_pdf_is_malformed() {
    let doc = two_page_pdf();
    let cut = &doc[..xref_offset(&doc)];
    assert!(
        matches!(
            pdf_pages(cut, 1, 10, 100_000),
            Err(ExtractError::Malformed(_))
        ),
        "{:?}",
        pdf_pages(cut, 1, 10, 100_000)
    );
    // Обрезка посреди объекта тоже.
    let cut = &doc[..doc.len() / 3];
    assert!(matches!(
        pdf_pages(cut, 1, 10, 100_000),
        Err(ExtractError::Malformed(_))
    ));
}

/// Документ с паролем пользователя — `Encrypted` (пустой пароль `lopdf` пробует сам).
#[test]
fn encrypted_is_encrypted() {
    assert_eq!(
        pdf_pages(&pdf::encrypted_pdf(), 1, 10, 100_000),
        Err(ExtractError::Encrypted)
    );
}

/// Номер первой страницы 0 — как 1 (страницы нумеруются с 1).
#[test]
fn first_page_zero_is_one() {
    let t = pdf_pages(&two_page_pdf(), 0, 1, 100_000).unwrap();
    assert_eq!(t.pages, vec![(1, "alpha".into())]);
}
