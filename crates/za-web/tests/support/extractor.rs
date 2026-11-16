//! Заглушка извлекателя (D113, D127) для тестов `web_fetch`: HTML → текст без тегов,
//! PDF → заданные страницы. Настоящий разбор — `za-extract` в подпроцессе (задача 10).

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use za_core::BoxFuture;
use za_core::extract::{ExtractError, Extractor, HtmlRequest, Markdown, PdfRequest, PdfText};

/// Заглушка: запоминает запросы; `html_error` — ответ вместо разбора HTML.
#[derive(Default)]
pub struct StubExtractor {
    pub html_seen: Mutex<Vec<HtmlRequest>>,
    pub html_error: Option<ExtractError>,
    pub pdf_seen: Mutex<Vec<PdfRequest>>,
    pub pdf_pages: PdfText,
}

impl StubExtractor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Разбор HTML всегда завершается ошибкой `e`.
    pub fn failing(e: ExtractError) -> Arc<Self> {
        Arc::new(Self {
            html_error: Some(e),
            ..Self::default()
        })
    }

    /// Принятые запросы HTML.
    pub fn html_requests(&self) -> Vec<HtmlRequest> {
        self.html_seen.lock().unwrap().clone()
    }
}

impl Extractor for StubExtractor {
    fn html(&self, req: HtmlRequest) -> BoxFuture<'static, Result<Markdown, ExtractError>> {
        let r = match &self.html_error {
            Some(e) => Err(e.clone()),
            None => Ok(strip_tags(&req.html)),
        };
        self.html_seen.lock().unwrap().push(req);
        Box::pin(async move { r })
    }

    fn pdf(&self, req: PdfRequest) -> BoxFuture<'static, Result<PdfText, ExtractError>> {
        self.pdf_seen.lock().unwrap().push(req);
        let pages = self.pdf_pages.clone();
        Box::pin(async move { Ok(pages) })
    }
}

/// Заголовок — текст первого `<title>…</title>`; тело — всё прочее без тегов, без пробелов
/// по краям.
pub fn strip_tags(html: &str) -> Markdown {
    let lower = html.to_ascii_lowercase();
    let mut body = html.to_string();
    let mut title = None;
    if let (Some(a), Some(b)) = (lower.find("<title>"), lower.find("</title>"))
        && a < b
    {
        title = Some(html[a + "<title>".len()..b].to_string());
        body = format!("{}{}", &html[..a], &html[b + "</title>".len()..]);
    }
    let mut text = String::new();
    let mut in_tag = false;
    for c in body.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    Markdown {
        title,
        text: text.trim().to_string(),
    }
}
