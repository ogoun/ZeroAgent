//! Извлечение текста из враждебного содержимого (R1-C2 D113, D127): трейт [`Extractor`]
//! и его типы.
//!
//! Разборщики HTML и PDF работают только в отдельном процессе: падение, переполнение стека
//! и зависание разборщика убивают подпроцесс, а не агента. Здесь — только контракт; его
//! реализует бинарник (`zero_agent::extract::ProcessExtractor`, подкоманда
//! `zero-agent extract <html|pdf>`), потребители — `web_fetch` (`za-web`) и `read` у PDF.
//! [`Markdown`] и [`PdfText`] — JSON в stdout подкоманды: та же форма, что у `za-extract`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::BoxFuture;

/// Страница HTML для извлечения (D127): текст уже в UTF-8 (`za_web::convert`, D109).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlRequest {
    pub html: String,
    /// Итоговый адрес страницы: для относительных ссылок.
    pub base_url: String,
}

/// Итог разбора HTML (D109): Markdown тела и текст `<title>` отдельным полем.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Markdown {
    /// Текст первого `<title>`; нет или пустой — `None`. Длину не ограничивает: это
    /// дело потребителя.
    pub title: Option<String>,
    pub text: String,
    /// `true`, если страница оказалась слишком сложной для полного разбора и текст получен
    /// линейным извлечением без DOM (D127, ревью задачи 7). `#[serde(default)]`: квитанции
    /// и JSON подпроцесса без поля читаются как `false`.
    #[serde(default)]
    pub simplified: bool,
}

/// Страницы PDF для извлечения (D113): копия файла, а не путь проекта (TOCTOU, D98).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfRequest {
    pub path: PathBuf,
    /// Первая страница, с 1.
    pub first: u32,
    /// Сколько страниц.
    pub count: u32,
    /// Предел текста всех страниц, символов.
    pub max_chars: usize,
}

/// Текст страниц PDF (D113).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdfText {
    /// Всего страниц в документе.
    pub pages_total: u32,
    /// Номер страницы (с 1) и её текст; пустой текст — страница без текстового слоя.
    pub pages: Vec<(u32, String)>,
}

/// Ошибка извлечения. Текст — для модели (английский, ASCII, D23); потребитель добавляет
/// вид содержимого: `html: <текст>`, `pdf: <текст>`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtractError {
    /// Документ зашифрован.
    #[error("document is encrypted")]
    Encrypted,
    /// Документ испорчен; строка — что не так.
    #[error("malformed document: {0}")]
    Malformed(String),
    /// Разборщик не уложился в свой предел времени.
    #[error("extractor timed out")]
    Timeout,
    /// Вход или вывод разборщика больше предела.
    #[error("extractor output too large")]
    TooLarge,
    /// Подпроцесс упал или вернул неразбираемый ответ; строка — причина.
    #[error("extractor failed: {0}")]
    Failed(String),
}

/// Извлекатель текста (D113, D127). `Send + Sync`: его держат инструменты в `Arc`.
pub trait Extractor: Send + Sync {
    /// HTML → Markdown и заголовок.
    fn html(&self, req: HtmlRequest) -> BoxFuture<'static, Result<Markdown, ExtractError>>;
    /// Текст страниц PDF.
    fn pdf(&self, req: PdfRequest) -> BoxFuture<'static, Result<PdfText, ExtractError>>;
}
