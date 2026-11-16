//! HTML в Markdown (R1-C2 D109, D127).
//!
//! [`html_to_markdown`] — `htmd` с пропуском служебных, встраиваемых элементов и элементов
//! управления ([`SKIPPED_TAGS`]), заголовок — текст `<title>` отдельным полем, не длиннее
//! [`TITLE_MAX_BYTES`]. Вход длиннее
//! [`HTML_MAX_BYTES`] обрезается до разбора. После преобразования картинки `data:`
//! отсутствуют, серии пустых строк сжаты в одну.
//!
//! До токенизатора — линейный подсчёт атрибутов в тегах ([`crate::attrs`]): проверка
//! повторов атрибута у `html5ever` квадратична по их числу в одном теге. Разбор идёт под
//! онлайн-стражем ([`crate::guard`]): квадратичные классы `html5ever` (глубокая вложенность,
//! реконструкция активных элементов форматирования, «Noah's Ark») прерываются до их
//! разрастания. Соседние одинаковые строчные элементы сливаются до `htmd` за линейное время
//! ([`crate::merge`]): у `htmd` это слияние квадратично. Тот же проход считает верхнюю оценку
//! вывода `htmd` ([`crate::bound`]: префиксы строк цитат и списков, ограды кода, таблицы без
//! дополнения); больше [`HTML_OUTPUT_BUDGET`] — линейное извлечение. Выравнивание таблиц идёт
//! под бюджетом надбавки на страницу (`table::TableLayout`) — не больше остатка бюджета вывода
//! после оценки: сверх него таблица выводится без дополнения пробелами. При превышении любого
//! порога текст извлекается линейно без DOM ([`crate::linear`]) и результат несёт признак
//! [`Markdown::simplified`].
//!
//! Вызывается только в подпроцессе (D127, задача 10): обход DOM у `htmd` рекурсивный, и
//! даже под стражем остаётся жёсткая граница — тайм-аут и предел вывода подпроцесса.

use std::rc::Rc;

use htmd::element_handler::{HandlerResult, Handlers};
use htmd::{Element, HtmlToMarkdown};
use markup5ever_rcdom::{Node, NodeData};
use serde::{Deserialize, Serialize};

use crate::table::{TABLE_ALIGN_BUDGET, TableLayout};
use crate::{attrs, guard, linear};

/// Предел входа: дальше HTML обрезается до разбора (D109).
pub const HTML_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Бюджет вывода `htmd` одной страницы, байт (I-1 повторного ревью fix-10): верхняя оценка
/// вывода ([`crate::bound`]) вместе с заголовком ([`TITLE_MAX_BYTES`], m1 финального ревью)
/// больше него — линейное извлечение, а сумма оценки, заголовка и надбавки выравнивания
/// таблиц ([`TABLE_ALIGN_BUDGET`]) его не превышает. Равен пределу вывода
/// подпроцесса `zero_agent::extract::HTML_OUTPUT_MAX` (проверка при сборке там): страница,
/// чей текст заведомо больше предела, не строится, чтобы затем упасть.
pub const HTML_OUTPUT_BUDGET: usize = 4 * 1024 * 1024;

/// Предел заголовка страницы, байт (m1 финального ревью извлечения): заголовок режется по
/// границе символа на обоих путях и входит в бюджет вывода [`HTML_OUTPUT_BUDGET`] вместе с
/// текстом. Прежде заголовок в 0,9 МБ при тексте в бюджете давал ответ подпроцесса больше
/// предела вывода, и страница целиком падала. Потребитель (`za-web`, строка `title:`)
/// показывает не больше 200 символов — до 800 байт UTF-8, предел их вмещает.
pub const TITLE_MAX_BYTES: usize = 1024;

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
    // (рекурсия `htmd`), сливает соседей до `htmd` за линейное время и оценивает вывод сверху.
    let Ok(tree) = guard::guarded_parse(html) else {
        return linear::linear_extract(html);
    };
    let Ok(bound) = guard::prepare_dom(&tree) else {
        return linear::linear_extract(html);
    };
    // Заголовок входит в бюджет вывода вместе с текстом (m1 финального ревью извлечения).
    let title = document_title(&tree);
    let used = bound.saturating_add(title.as_deref().map_or(0, str::len) as u64);
    if used > HTML_OUTPUT_BUDGET as u64 {
        return linear::linear_extract(html);
    }
    Markdown {
        title,
        text: render_with(&tree, TableLayout::with_budget(align_budget(used))),
        simplified: false,
    }
}

/// Бюджет надбавки выравнивания таблиц страницы, у которой верхняя оценка вывода вместе с
/// заголовком — `used` (не больше [`HTML_OUTPUT_BUDGET`]): [`TABLE_ALIGN_BUDGET`], но не
/// больше остатка бюджета вывода.
fn align_budget(used: u64) -> u64 {
    TABLE_ALIGN_BUDGET.min((HTML_OUTPUT_BUDGET as u64).saturating_sub(used))
}

/// Заголовок страницы из слов `<title>` (пробелы уже схлопнуты): не длиннее
/// [`TITLE_MAX_BYTES`] — обрезка по границе символа, без пробела в конце; пустой — `None`.
/// Общий у полного и линейного пути.
pub(crate) fn cut_title(mut title: String) -> Option<String> {
    title.truncate(title.floor_char_boundary(TITLE_MAX_BYTES));
    title.truncate(title.trim_end_matches(' ').len());
    (!title.is_empty()).then_some(title)
}

/// Готовое дерево в Markdown с полным бюджетом выравнивания таблиц (модульные тесты).
#[cfg(test)]
pub(crate) fn render(tree: &Rc<Node>) -> String {
    render_with(tree, TableLayout::new())
}

/// Готовое дерево в Markdown: `htmd` со списком пропуска [`SKIPPED_TAGS`], без картинок
/// `data:` и с выравниванием таблиц под бюджетом страницы `tables` ([`TableLayout`]), серии
/// пустых строк сжаты.
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
/// `document.title`): текстовые дети подряд, пробелы ASCII схлопнуты, не длиннее
/// [`TITLE_MAX_BYTES`] ([`cut_title`]). Обход — явным стеком: глубина дерева не ограничена.
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
            return cut_title(raw.split_ascii_whitespace().collect::<Vec<_>>().join(" "));
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
