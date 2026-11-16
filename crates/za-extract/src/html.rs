//! HTML в Markdown (R1-C2 D109, D127).
//!
//! [`html_to_markdown`] — `htmd` с пропуском служебных и встраиваемых элементов
//! ([`SKIPPED_TAGS`]), заголовок — текст `<title>` отдельным полем. Вход длиннее
//! [`HTML_MAX_BYTES`] обрезается до разбора. После преобразования картинки `data:`
//! отсутствуют, серии пустых строк сжаты в одну.
//!
//! Вызывается только в подпроцессе (D127, задача 10): обход DOM у `htmd` рекурсивный, и
//! глубокая вложенность переполняет стек, а внутри `html5ever` и `htmd` есть проходы,
//! квадратичные на враждебной разметке. Свой обход здесь — без рекурсии.

use std::rc::Rc;

use htmd::element_handler::{HandlerResult, Handlers};
use htmd::{Element, HtmlToMarkdown};
use markup5ever_rcdom::{Node, NodeData};
use serde::{Deserialize, Serialize};

/// Предел входа: дальше HTML обрезается до разбора (D109).
pub const HTML_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Элементы, содержимое которых в текст не попадает (D109). `head` и `title` — чтобы текст
/// заголовка не попал в тело: он возвращается отдельным полем.
pub const SKIPPED_TAGS: &[&str] = &[
    "head", "title", "script", "style", "noscript", "template", "svg", "canvas", "iframe",
    "object", "embed", "form",
];

/// Пространство имён элементов HTML: `<title>` внутри SVG заголовком страницы не считается.
const HTML_NS: &str = "http://www.w3.org/1999/xhtml";

/// Итог разбора страницы; та же форма, что `za_core::extract::Markdown` (JSON подкоманды).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Markdown {
    /// Текст первого `<title>` документа (пробелы схлопнуты); пустой — `None`.
    pub title: Option<String>,
    pub text: String,
}

/// HTML (UTF-8) в Markdown. Вход длиннее [`HTML_MAX_BYTES`] обрезается по границе символа
/// до разбора. Только для подпроцесса (D127).
pub fn html_to_markdown(html: &str) -> Markdown {
    let html = &html[..html.floor_char_boundary(HTML_MAX_BYTES)];
    let converter = HtmlToMarkdown::builder()
        .skip_tags(SKIPPED_TAGS.to_vec())
        .add_handler(vec!["img"], drop_data_image)
        .build();
    // `html_to_tree` ошибки не возвращает (разбор строки целиком); на случай смены API —
    // пустой результат, инструмент ответит «нет читаемого текста».
    let Ok(tree) = converter.html_to_tree(html) else {
        return Markdown::default();
    };
    let title = document_title(&tree);
    let text = collapse_blank_lines(&converter.tree_to_markdown(&tree));
    Markdown { title, text }
}

/// Картинка с адресом `data:` (`src` или `href`, как их читает `htmd`) в текст не попадает:
/// base64 занял бы бюджет контекста. Остальные — встроенным обработчиком `htmd`.
fn drop_data_image(handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
    let inline = element.attrs.iter().any(|a| {
        let name = &*a.name.local;
        (name == "src" || name == "href")
            && a.value
                .trim_start()
                .get(..5)
                .is_some_and(|s| s.eq_ignore_ascii_case("data:"))
    });
    if inline {
        None
    } else {
        handlers.fallback(element)
    }
}

/// Текст первого элемента `title` из пространства имён HTML в порядке документа (как
/// `document.title`): текстовые дети подряд, пробелы ASCII схлопнуты. Обход — явным
/// стеком: глубина дерева не ограничена.
fn document_title(root: &Rc<Node>) -> Option<String> {
    let mut stack = vec![Rc::clone(root)];
    while let Some(node) = stack.pop() {
        if let NodeData::Element { name, .. } = &node.data
            && &*name.local == "title"
            && &*name.ns == HTML_NS
        {
            let mut raw = String::new();
            for child in node.children.borrow().iter() {
                if let NodeData::Text { contents } = &child.data {
                    raw.push_str(&contents.borrow());
                }
            }
            let title = raw.split_ascii_whitespace().collect::<Vec<_>>().join(" ");
            return (!title.is_empty()).then_some(title);
        }
        stack.extend(node.children.borrow().iter().rev().cloned());
    }
    None
}

/// Серии из двух и более пустых (или пробельных) строк — то есть три и более переводов
/// строки подряд — сжимаются в одну пустую строку; пустые строки в начале и в конце
/// снимаются (D109).
fn collapse_blank_lines(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut blank_run = 0usize;
    for line in md.split('\n') {
        if line.trim().is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank_run > 0 {
                out.push('\n');
            }
        }
        blank_run = 0;
        out.push_str(line);
    }
    out
}
