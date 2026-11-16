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
    assert!(t.truncated);
}

/// m5 ревью задачи 10: текст ровно в предел обрезанным не считается, если страницы окна
/// прочитаны все; непрочитанная страница окна после исчерпанного предела — обрыв.
#[test]
fn exact_fit_is_not_truncated() {
    let doc = pdf_with_pages(&["aaaaaaaaaa", "bbbbbbbbbb", "cccccccccc"]);
    let t = pdf_pages(&doc, 1, 2, 20).unwrap();
    assert_eq!(t.pages.len(), 2);
    assert!(!t.truncated, "{t:?}");
    let t = pdf_pages(&doc, 1, 3, 20).unwrap();
    assert_eq!(t.pages.len(), 2);
    assert!(t.truncated, "{t:?}");
    let t = pdf_pages(&doc, 1, 3, 100).unwrap();
    assert!(!t.truncated, "{t:?}");
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

/// PDF двух страниц, зашифрованный средствами `lopdf` (реэкспорт `pdf-extract`, только в
/// тесте): RC4 40 бит (V1, R2), пароль владельца задан, пароль пользователя — `user`.
fn rc4_encrypted(user: &str) -> Vec<u8> {
    use pdf_extract::{Document, EncryptionState, EncryptionVersion, Object, Permissions};
    let mut doc = Document::load_mem(&two_page_pdf()).expect("исходный PDF");
    let id = Object::string_literal("0123456789abcdef");
    doc.trailer.set("ID", Object::Array(vec![id.clone(), id]));
    let state = EncryptionState::try_from(EncryptionVersion::V1 {
        document: &doc,
        owner_password: "owner secret",
        user_password: user,
        permissions: Permissions::PRINTABLE,
    })
    .expect("параметры шифрования");
    doc.encrypt(&state).expect("шифрование");
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).expect("запись PDF");
    // Документ действительно зашифрован: текста страниц в открытом виде нет.
    assert!(bytes.windows(8).any(|w| w == b"/Encrypt"));
    assert!(!bytes.windows(5).any(|w| w == b"alpha"));
    bytes
}

/// m7 ревью задачи 10: документ, закрытый только паролем владельца (пароль пользователя
/// пуст, шифрование ради прав), читается: `lopdf` при загрузке сам пробует пустой пароль и
/// расшифровывает. С непустым паролем пользователя тот же документ — `Encrypted`.
#[test]
fn owner_password_only_is_read() {
    let t = pdf_pages(&rc4_encrypted(""), 1, 10, 100_000).expect("пустой пароль подходит");
    assert_eq!(t.pages, vec![(1, "alpha".into()), (2, "beta".into())]);
    assert_eq!(
        pdf_pages(&rc4_encrypted("user"), 1, 10, 100_000),
        Err(ExtractError::Encrypted)
    );
}

/// Номер первой страницы 0 — как 1 (страницы нумеруются с 1).
#[test]
fn first_page_zero_is_one() {
    let t = pdf_pages(&two_page_pdf(), 0, 1, 100_000).unwrap();
    assert_eq!(t.pages, vec![(1, "alpha".into())]);
}
