//! HTML в Markdown (R1-C2 D109, D127).
//!
//! [`html_to_markdown`] — `htmd` с пропуском служебных, встраиваемых элементов и элементов
//! управления ([`SKIPPED_TAGS`]), заголовок — текст `<title>` отдельным полем. Вход длиннее
//! [`HTML_MAX_BYTES`] обрезается до разбора. После преобразования картинки `data:`
//! отсутствуют, серии пустых строк сжаты в одну.
//!
//! До токенизатора — линейный подсчёт атрибутов в тегах ([`crate::attrs`]): проверка
//! повторов атрибута у `html5ever` квадратична по их числу в одном теге. Разбор идёт под
//! онлайн-стражем ([`crate::guard`]): квадратичные классы `html5ever` (глубокая вложенность,
//! реконструкция активных элементов форматирования, «Noah's Ark») прерываются до их
//! разрастания. Соседние одинаковые строчные элементы сливаются до `htmd` за линейное время
//! ([`crate::merge`]): у `htmd` это слияние квадратично. Выравнивание таблиц `htmd` идёт под
//! бюджетом надбавки на страницу (`table::TableLayout`): сверх него таблица выводится
//! без дополнения пробелами. При превышении любого порога текст извлекается линейно без DOM
//! ([`crate::linear`]) и результат несёт признак [`Markdown::simplified`].
//!
//! Вызывается только в подпроцессе (D127, задача 10): обход DOM у `htmd` рекурсивный, и
//! даже под стражем остаётся жёсткая граница — тайм-аут и предел вывода подпроцесса.

use std::rc::Rc;

use htmd::element_handler::{HandlerResult, Handlers};
use htmd::{Element, HtmlToMarkdown};
use markup5ever_rcdom::{Node, NodeData};
use serde::{Deserialize, Serialize};

use crate::table::TableLayout;
use crate::{attrs, guard, linear};

/// Предел входа: дальше HTML обрезается до разбора (D109).
pub const HTML_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Элементы, содержимое которых в текст не попадает (D109, I1 ревью задачи 7).
/// - служебные и встраиваемые: `head`, `title`, `script`, `style`, `noscript`, `template`,
///   `svg`, `canvas`, `iframe`, `object`, `embed` (`head` и `title` — чтобы текст заголовка
///   не попал в тело: он возвращается отдельным полем);
/// - элементы управления формы: `input`, `select`, `option`, `button`, `textarea`.
///
/// `form` в списке нет: страница ASP.NET WebForms целиком обёрнута в один `<form>`, и её
/// пропуск стёр бы весь текст. Пропускаются только сами элементы управления.
pub const SKIPPED_TAGS: &[&str] = &[
    "head", "title", "script", "style", "noscript", "template", "svg", "canvas", "iframe",
    "object", "embed", "input", "select", "option", "button", "textarea",
];

/// Пространство имён элементов HTML: `<title>` внутри SVG заголовком страницы не считается.
const HTML_NS: &str = "http://www.w3.org/1999/xhtml";

/// Итог разбора страницы; та же форма, что `za_core::extract::Markdown` (JSON подкоманды).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Markdown {
    /// Текст первого `<title>` документа (пробелы схлопнуты); пустой — `None`.
    pub title: Option<String>,
    pub text: String,
    /// `true`, если текст получен линейным извлечением без DOM: страница оказалась слишком
    /// сложной для полного разбора (D127, ревью задачи 7).
    #[serde(default)]
    pub simplified: bool,
}

/// HTML (UTF-8) в Markdown. Вход длиннее [`HTML_MAX_BYTES`] обрезается по границе символа
/// до разбора. Атрибуты тегов считаются до токенизатора ([`crate::attrs`]), разбор — под
/// стражем ([`crate::guard`]); при превышении порога — линейное извлечение с признаком
/// [`Markdown::simplified`]. Только для подпроцесса (D127).
pub fn html_to_markdown(html: &str) -> Markdown {
    let html = &html[..html.floor_char_boundary(HTML_MAX_BYTES)];
    // Тег с тысячами атрибутов токенизатор разбирает квадратично ещё до событий стража.
    if attrs::attr_check(html).is_err() {
        return linear::linear_extract(html);
    }
    // Страж прерывает квадратичный разбор на первых килобайтах; `prepare_dom` ловит глубину
    // (рекурсия `htmd`) и сливает соседей до `htmd` за линейное время.
    let Ok(tree) = guard::guarded_parse(html) else {
        return linear::linear_extract(html);
    };
    if guard::prepare_dom(&tree).is_err() {
        return linear::linear_extract(html);
    }
    Markdown {
        title: document_title(&tree),
        text: render(&tree),
        simplified: false,
    }
}

/// Готовое дерево в Markdown: `htmd` со списком пропуска [`SKIPPED_TAGS`], без картинок
/// `data:` и с выравниванием таблиц под бюджетом страницы ([`TableLayout`]), серии пустых
/// строк сжаты.
pub(crate) fn render(tree: &Rc<Node>) -> String {
    render_with(tree, TableLayout::new())
}

/// [`render`] со своим обработчиком таблиц (модульные тесты бюджета выравнивания).
pub(crate) fn render_with(tree: &Rc<Node>, tables: TableLayout) -> String {
    let converter = HtmlToMarkdown::builder()
        .skip_tags(SKIPPED_TAGS.to_vec())
        .add_handler(vec!["img"], drop_data_image)
        .add_handler(vec!["table"], tables)
        .build();
    collapse_blank_lines(&converter.tree_to_markdown(tree))
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
pub(crate) fn collapse_blank_lines(md: &str) -> String {
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
