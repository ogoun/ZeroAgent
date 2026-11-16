//! Извлечение текста из враждебного содержимого (R1-C2 D113, D127): трейт [`Extractor`]
//! и его типы.
//!
//! Разборщики HTML и PDF работают только в отдельном процессе: падение, переполнение стека
//! и зависание разборщика убивают подпроцесс, а не агента. Здесь — только контракт; его
//! реализует бинарник (`zero_agent::extract::ProcessExtractor`, подкоманда
//! `zero-agent extract <html|pdf>`), потребители — `web_fetch` (`za-web`) и `read` у PDF.
//! [`Markdown`] и [`PdfText`] — JSON в stdout подкоманды: та же форма, что у `za-extract`.
//!
//! Общее у потребителей PDF (задача 10): пределы ([`PDF_MAX_BYTES`], [`PDF_MAX_CHARS`]),
//! копия байт в рабочий каталог run ([`PdfCopy`]: подпроцесс не открывает путь проекта,
//! TOCTOU D98) и тексты для модели ([`render_pdf`], [`pdf_error_text`] и образцы для хэша
//! промптов).

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use za_protocol::RunId;

use crate::BoxFuture;
use crate::paths::work_dir;

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
    /// Текст окна обрезан по `max_chars`: последняя страница обрезана или следующие страницы
    /// окна не прочитаны (m5 ревью задачи 10). `#[serde(default)]`: ответ без поля —
    /// `false`.
    #[serde(default)]
    pub truncated: bool,
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
    /// Вывод разборщика больше предела.
    #[error("extractor output too large")]
    TooLarge,
    /// Вход больше предела разборщика (PDF больше [`PDF_MAX_BYTES`]): разбора не было (m2
    /// ревью задачи 10). Потребители проверяют предел сами, до вызова.
    #[error("document too large for the extractor")]
    InputTooLarge,
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

// ---- PDF у потребителей (задача 10): общие пределы и тексты `read` и `web_fetch` ----

/// Сигнатура PDF в начале файла или тела (D109, D113).
pub const PDF_SIGNATURE: &[u8] = b"%PDF-";
/// Предел PDF, байт (D113): копия у `read`, тело `web_fetch` (вместо `max_bytes`: обрезанный
/// PDF не разобрать — таблица `xref` в конце файла), файл подкоманды `extract pdf`.
pub const PDF_MAX_BYTES: usize = 32 * 1024 * 1024;
/// Предел текста страниц одного вызова, символов: вывод подпроцесса и результат инструмента
/// (длинный результат выгружает пакет, D44).
pub const PDF_MAX_CHARS: usize = 256 * 1024;

/// Причина [`ExtractError::Failed`]: подпроцесс извлекателя не запустился.
pub const FAILED_START: &str = "cannot start the process";
/// Причина [`ExtractError::Failed`]: ответ подпроцесса не разобран.
pub const FAILED_REPLY: &str = "invalid reply from the process";

/// Причина [`ExtractError::Failed`]: подпроцесс завершился аварийно (`detail` — код выхода
/// или сигнал: `exit code 101`).
pub fn failed_exit_text(detail: &str) -> String {
    format!("process crashed ({detail})")
}

/// Копия PDF не записана в рабочий каталог (`kind` — вид ошибки ввода-вывода).
pub fn pdf_copy_error_text(kind: io::ErrorKind) -> String {
    format!("cannot copy the pdf to the work directory: {kind}")
}

/// PDF больше [`PDF_MAX_BYTES`]: разбора нет (D113).
pub fn pdf_too_large_text() -> String {
    format!("pdf larger than {PDF_MAX_BYTES} bytes")
}

/// Ошибка извлечения PDF для модели: `pdf: <причина>` (D113); вход больше предела — тот же
/// текст, что у проверки потребителя до разбора ([`pdf_too_large_text`]).
pub fn pdf_error_text(e: &ExtractError) -> String {
    match e {
        ExtractError::InputTooLarge => pdf_too_large_text(),
        _ => format!("pdf: {e}"),
    }
}

/// Первая страница окна за концом документа.
pub fn pdf_past_end_text(first: u32, total: u32) -> String {
    format!("start {first} is past the last page of the pdf ({total} pages)")
}

/// Текст страниц PDF для модели (D113): `[pdf: N pages; showing a-b]`, затем по странице
/// `--- page n ---` и её текст; страница без текста — `[page n: no text layer]`. Текст,
/// обрезанный извлекателем ([`PdfText::truncated`]), — пометка обрыва на последней странице
/// с пределом `max_chars`. `first` — первая запрошенная страница; окно за концом документа —
/// `Err` с текстом для модели.
pub fn render_pdf(t: &PdfText, first: u32, max_chars: usize) -> Result<String, String> {
    let (Some(a), Some(b)) = (t.pages.first(), t.pages.last()) else {
        if t.pages_total == 0 {
            return Ok("[pdf: 0 pages]".to_owned());
        }
        return Err(pdf_past_end_text(first.max(1), t.pages_total));
    };
    let mut out = format!("[pdf: {} pages; showing {}-{}]", t.pages_total, a.0, b.0);
    for (n, text) in &t.pages {
        out.push_str(&format!("\n--- page {n} ---\n"));
        if text.trim().is_empty() {
            out.push_str(&format!("[page {n}: no text layer]"));
        } else {
            out.push_str(text.trim());
        }
    }
    if t.truncated {
        out.push_str(&format!(
            "\n[text cut at {max_chars} characters on page {}]",
            b.0
        ));
    }
    Ok(out)
}

/// Образец [`render_pdf`] для хэша промптов (D94): страница с текстом, страница без
/// текстового слоя и пометка обрыва.
pub fn pdf_sample() -> String {
    let t = PdfText {
        pages_total: 12,
        pages: vec![(1, "Text".to_owned()), (2, String::new())],
        truncated: true,
    };
    render_pdf(&t, 1, 4).unwrap_or_default()
}

/// Копия байт PDF для извлекателя (D113): `<root>/.zero-agent/work/tmp/<run>/pdf-<stem>.pdf`
/// (`work_dir`: компоненты без ссылок, внутри проекта). Удаляется при броске — после вызова
/// извлекателя, при тайм-ауте и при отмене вызова. Блокирующая запись: только вне потоков
/// рантайма.
#[derive(Debug)]
pub struct PdfCopy(PathBuf);

impl PdfCopy {
    /// Записать `bytes` копией для вызова с основой имени `stem` (`EffectKey::file_stem`).
    /// Прежний файл с тем же именем (повтор вызова после сбоя) удаляется, новый создаётся
    /// заново (`create_new`: не через подложенную ссылку).
    pub fn write(root: &Path, run: RunId, stem: &str, bytes: &[u8]) -> io::Result<Self> {
        use std::io::Write;
        let path = work_dir(root, &format!("tmp/{run}"))?.join(format!("pdf-{stem}.pdf"));
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let copy = Self(path);
        f.write_all(bytes)?;
        Ok(copy)
    }

    /// Путь копии.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PdfCopy {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %self.0.display(), error = %e, "копия PDF не удалена"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// m5 ревью задачи 10: пометка обрыва — только у текста, который извлекатель обрезал;
    /// текст ровно в предел без обрыва её не получает.
    #[test]
    fn cut_marker_follows_truncated() {
        let mut t = PdfText {
            pages_total: 3,
            pages: vec![(1, "abcd".to_owned())],
            truncated: false,
        };
        assert_eq!(
            render_pdf(&t, 1, 4).unwrap(),
            "[pdf: 3 pages; showing 1-1]
--- page 1 ---
abcd"
        );
        t.truncated = true;
        assert!(render_pdf(&t, 1, 4).unwrap().ends_with(
            "
[text cut at 4 characters on page 1]"
        ));
    }

    /// m2 ревью задачи 10: отказ подпроцесса по размеру входа — тот же текст, что у
    /// потребителя до разбора, а не «вывод слишком велик».
    #[test]
    fn input_too_large_text() {
        assert_eq!(
            pdf_error_text(&ExtractError::InputTooLarge),
            "pdf larger than 33554432 bytes"
        );
        assert_eq!(
            pdf_error_text(&ExtractError::TooLarge),
            "pdf: extractor output too large"
        );
    }
}
