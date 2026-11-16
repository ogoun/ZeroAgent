//! Разбор враждебного содержимого (R1-C2 D113, D127): HTML в Markdown ([`html_to_markdown`])
//! и текст страниц PDF ([`pdf_pages`]).
//!
//! Функции крейта вызываются **только в подпроцессе** `zero-agent extract <html|pdf>` через
//! `za_jail::Runner` (тайм-аут, предел вывода, `EnvMode::Clear`). Разборщики обходят дерево
//! рекурсивно: глубокая вложенность переполняет стек, а переполнение стека `catch_unwind` не
//! ловит и роняет весь процесс. Зависание и квадратичные случаи разборщика тоже не
//! прерываются изнутри. Отдельный процесс снимает эти риски архитектурно: падает и
//! убивается только он, инструмент получает ошибку.
//!
//! Крейт не зависит от `za-*`: связь с `za_core::extract` — JSON подкоманды той же формы.

pub mod attrs;
mod bound;
pub mod guard;
pub mod html;
pub mod linear;
mod merge;
mod pdf;
mod table;

pub use html::{HTML_MAX_BYTES, HTML_OUTPUT_BUDGET, Markdown, html_to_markdown};
pub use pdf::{ExtractError, PdfText, pdf_pages};
pub use table::TABLE_ALIGN_BUDGET;
