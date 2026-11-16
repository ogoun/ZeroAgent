//! Текст страниц PDF (R1-C2 D113; таблица 5.2: `read` — «PDF-страницы», CAP-002).
//!
//! [`pdf_pages`] — разбор `lopdf` и текст страницы `pdf-extract` (постранично, только
//! страницы окна). У `pdf-extract` и `lopdf` есть паники на битых данных (`unwrap`,
//! `panic!` на неожиданных шрифтах и кодировках): разбор и каждая страница идут под
//! `catch_unwind`, паника — [`ExtractError::Malformed`]. Переполнение стека, вечный цикл и
//! бомба распаковки так не ловятся — поэтому функция вызывается только в подпроцессе
//! `zero-agent extract pdf` (тайм-аут и предел вывода `za_jail::Runner`; предела памяти в
//! R1-C2 нет — принятый риск ADR 0006).
//!
//! Документ с паролем пользователя — [`ExtractError::Encrypted`]: пустой пароль `lopdf`
//! пробует сам при загрузке (так читаются документы, зашифрованные только ради прав).

use std::panic::{AssertUnwindSafe, catch_unwind};

use pdf_extract::{Document, Error as PdfError, PlainTextOutput, output_doc_page};
use serde::{Deserialize, Serialize};

/// Текст страниц PDF; та же форма, что `za_core::extract::PdfText` (JSON подкоманды).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdfText {
    /// Всего страниц в документе.
    pub pages_total: u32,
    /// Номер страницы (с 1) и её текст без пробелов по краям; пустой — страница без
    /// текстового слоя.
    pub pages: Vec<(u32, String)>,
}

/// Ошибка разбора PDF. Текст — для модели (английский, ASCII): потребитель пишет
/// `pdf: <текст>`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtractError {
    /// Нужен пароль пользователя.
    #[error("document is encrypted")]
    Encrypted,
    /// Документ испорчен или разборщик упал; строка — что не так (ASCII, коротко).
    #[error("malformed document: {0}")]
    Malformed(String),
}

/// Предел длины причины в [`ExtractError::Malformed`], символов: текст ошибки `lopdf`
/// бывает с данными документа.
const REASON_MAX: usize = 160;

/// Текст страниц `first..first + count` (нумерация с 1; `first` 0 — как 1) документа
/// `bytes`, всего не больше `max_chars` символов: страница, на которой предел исчерпан,
/// обрезается, следующие не читаются. Страницы за концом документа не возвращаются,
/// [`PdfText::pages_total`] известно всегда. Ошибка страницы — ошибка вызова с номером
/// страницы: модель может читать окно после неё.
pub fn pdf_pages(
    bytes: &[u8],
    first: u32,
    count: u32,
    max_chars: usize,
) -> Result<PdfText, ExtractError> {
    let doc = match catch_unwind(AssertUnwindSafe(|| Document::load_mem(bytes))) {
        Ok(Ok(doc)) => doc,
        Ok(Err(PdfError::Decryption(_) | PdfError::InvalidPassword)) => {
            return Err(ExtractError::Encrypted);
        }
        Ok(Err(e)) => return Err(ExtractError::Malformed(reason(&e.to_string()))),
        Err(_) => return Err(ExtractError::Malformed("parser panicked".into())),
    };
    // Пустой пароль не подошёл: `lopdf` загрузил документ без расшифровки.
    if doc.is_encrypted() {
        return Err(ExtractError::Encrypted);
    }
    let total = u32::try_from(doc.get_pages().len()).unwrap_or(u32::MAX);
    let first = first.max(1);
    let last = first.saturating_add(count).saturating_sub(1).min(total);
    let mut pages = Vec::new();
    let mut left = max_chars;
    for n in first..=last {
        if left == 0 || count == 0 {
            break;
        }
        let text = page_text(&doc, n)?;
        let text = text.trim();
        let cut = match text.char_indices().nth(left) {
            Some((i, _)) => &text[..i],
            None => text,
        };
        left -= cut.chars().count();
        pages.push((n, cut.to_owned()));
    }
    Ok(PdfText {
        pages_total: total,
        pages,
    })
}

/// Текст одной страницы под `catch_unwind`.
fn page_text(doc: &Document, n: u32) -> Result<String, ExtractError> {
    let run = catch_unwind(AssertUnwindSafe(|| {
        let mut s = String::new();
        let r = output_doc_page(doc, &mut PlainTextOutput::new(&mut s), n);
        r.map(|()| s)
    }));
    match run {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(ExtractError::Malformed(reason(&format!("page {n}: {e}")))),
        Err(_) => Err(ExtractError::Malformed(format!(
            "parser panicked on page {n}"
        ))),
    }
}

/// Причина для модели: печатные ASCII и пробел, не длиннее [`REASON_MAX`] символов.
fn reason(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| if c.is_ascii_graphic() { c } else { ' ' })
        .collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s: String = s.chars().take(REASON_MAX).collect();
    if s.is_empty() {
        "unknown error".into()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_table() {
        assert_eq!(reason("bad\n\tthing  é"), "bad thing");
        assert_eq!(reason(""), "unknown error");
        assert_eq!(reason(&"x".repeat(500)).len(), REASON_MAX);
    }
}
