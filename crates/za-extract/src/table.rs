//! Таблицы и `htmd` (R1-C2): имена ячеек до `htmd` (перенос прохода fix-9 в задачу 10) и
//! выравнивание под бюджетом (I1 ревью задачи 10, m-1, m-2, m-4 повторного ревью fix-10).
//!
//! Оба правила повторяют обработчик таблиц `htmd` 0.5.6 (`src/element_handler/table.rs`).
//! При обновлении `htmd` их нужно сверить с ним; расхождение подсчёта размера ловит модульный
//! тест `aligned_size_matches_htmd`.
//!
//! ## Имена ячеек
//!
//! `htmd` читает таблицу с явными заголовками напрямую по строкам и теряет ячейки:
//! - строка заголовка — первая строка `thead`, а без него первая строка тела с `th`; из неё
//!   берутся только `th`, её `td` пропадают;
//! - из остальных строк берутся только `td`, их `th` пропадают (строка из одних `th` —
//!   целиком).
//!
//! Так пропадают ключи карточек Википедии (`<tr><th>Paradigms</th><td>…</td></tr>`), строки
//! подзаголовков (`<tr><th colspan=2>…</th></tr>`) и пустая угловая ячейка матрицы, после
//! чего столбцы заголовка сдвигаются.
//!
//! [`normalize_table`] (вызов из `guard::prepare_dom`) меняет имена ячеек и секций так,
//! чтобы `htmd` ни одну ячейку не потерял:
//! - заголовок один: первая строка первого `thead` (её `td` становятся `th`), а если `thead`
//!   до неё не было — первая строка таблицы из одних `th`, как у `htmd`; смешанная первая
//!   строка — строка данных;
//! - остальные строки этого `thead` переносятся в новый `tbody` сразу за ним: `htmd` читает
//!   из `thead` только первую строку (m6 ревью задачи 10);
//! - `thead` после выбранного заголовка, в том числе пустой, становится `tbody`: у `htmd`
//!   он заменил бы заголовок. Пустой `thead` до заголовка (без ячеек в строке, которую `htmd`
//!   взял бы заголовком) тоже становится `tbody`, если за ним заголовок есть — строка из одних
//!   `th` или следующий непустой `thead`: иначе `htmd` считал бы заголовок выбранным, и
//!   таблица вышла бы без строки заголовка и разделителя (m-2 повторного ревью fix-10).
//!   Заголовка за ним нет — `thead` остаётся, и `htmd` печатает строки таблицы `| 1 | 2 |`
//!   без заголовка; без `thead` ячейки вышли бы отдельными абзацами (m-4 повторного ревью
//!   fix-10b). Пустые строки выбор заголовка не сдвигают — `htmd` их пропускает;
//! - во всех прочих строках `th` становятся `td`.
//!
//! Текст и порядок ячеек не меняются. Таблица, где после замены нет ни `th`, ни `thead`,
//! у `htmd` идёт обычным обходом детей (режим `Pure`) — текст ячеек сохраняется и там.
//! Вложенные таблицы — отдельные элементы `table`, их обрабатывает свой вызов. Цена — один
//! проход по секциям, строкам и ячейкам таблицы и, если до заголовка встретился пустой
//! `thead`, ещё один обратный проход: решение «за ним есть заголовок» для всех позиций
//! считается сразу (I1 ревью fix-13: поиск вперёд от каждого из подряд идущих пустых
//! `thead` был квадратичным — 224 КБ разбирались 9 с).
//!
//! ## Выравнивание
//!
//! `htmd` дополняет каждую ячейку пробелами до самой широкой ячейки столбца, а недостающие
//! ячейки строки — пустыми до ширины столбца (`format_row_padded`). Вывод растёт как
//! строки × сумма ширин столбцов: одна ячейка в 1 МиБ и 55 000 строк — около 55 ГБ в памяти
//! подпроцесса ещё до печати.
//!
//! [`TableLayout`] — свой обработчик `table` в `htmd` (вызов из `html::render_with`), он стоит
//! перед встроенным и **сам отрисовывает каждую таблицу ровно один раз**: встроенному
//! обработчику таблица не передаётся никогда (I1 финального ревью извлечения). Встроенный
//! у таблицы с признаком заголовка, но без ячеек, отрисовывал подписи дважды (сначала
//! подписи, затем обход всех детей), и таблица в подписи такой же таблицы удваивала время на
//! каждом уровне: 1,2 КБ входа — 23 с. Верхняя оценка [`crate::bound`] ограничивает размер
//! вывода и держится, только пока каждое поддерево отрисовывается один раз.
//! - таблицу, которую `htmd` выводит обходом детей (без явных заголовков или внутри ячейки),
//!   выводит тем же обходом детей;
//! - таблицу с признаком заголовка без единой ячейки — как `htmd`: обход детей в пустых
//!   строках, пустой — ничего;
//! - у остальных отрисовывает ячейки теми же обработчиками `htmd` и считает **точный** размер
//!   выровненной таблицы за O(ячеек), не строя её. Разница с той же таблицей без дополнения —
//!   надбавка выравнивания;
//! - если надбавка таблицы не больше 25 % её вывода без дополнения
//!   ([`TABLE_ALIGN_SHARE_DIVISOR`], I2 финального ревью) и сумма надбавок страницы не больше
//!   бюджета страницы, строит выровненную таблицу из уже отрисованных ячеек по правилам
//!   `htmd` (`format_row_padded`): вывод байт в байт тот же, что у `htmd`, а ячейки
//!   отрисованы один раз (m-4 повторного ревью fix-10);
//! - иначе — та же таблица Markdown без дополнения: те же подписи, строки, ячейки и их
//!   экранирование, разделитель `---`, строка не длиннее своих ячеек, заголовок — на все
//!   столбцы (m-1). Размер линеен по ячейкам.
//!
//! Бюджет страницы — [`TABLE_ALIGN_BUDGET`], но не больше остатка бюджета вывода после верхней
//! оценки вывода страницы без дополнения ([`crate::bound`], `html::html_to_markdown`).
//!
//! Размер считается по Markdown ячеек, а не по DOM: адрес ссылки, отступы вложенных списков
//! и экранирование входят в ширину, а текстом узлов не видны. Решение принимается у самого
//! элемента `table`, а не переименованием `th` в DOM: `htmd` признаёт таблицу «с
//! заголовками» по любому потомку `th` или `thead` в любом пространстве имён (`th` внутри
//! `<math>`), и переименованная таблица вернулась бы к выравниванию.
//!
//! Ячейки отрисовываются один раз: и подсчёт, и обе раскладки строятся из [`Grid`]. Таблицы
//! внутри ячеек не считаются (их `htmd` выравнивать не станет), подписи отрисовываются только
//! при построении.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use htmd::Element;
use htmd::element_handler::{ElementHandler, HandlerResult, Handlers};
use html5ever::{LocalName, QualName, ns};
use markup5ever_rcdom::{Handle, Node, NodeData};

/// Бюджет надбавки выравнивания таблиц одной страницы, байт (I1 ревью задачи 10): пробелы
/// дополнения, тире разделителя сверх `---` и пустые ячейки, которыми `htmd` выравнивает
/// таблицы. Сверх бюджета таблица идёт без дополнения (обработчик `TableLayout`).
///
/// 2 МиБ — половина бюджета вывода [`crate::HTML_OUTPUT_BUDGET`] (4 МиБ): пробелы дополнения
/// занимают не больше половины предела (m-6 повторного ревью fix-10). Страница получает
/// меньше, если верхняя оценка её вывода без дополнения ([`crate::bound`]) оставляет меньше:
/// оценка и надбавка вместе не больше бюджета вывода. Реальные страницы корпуса бюджета не
/// достигают (замер fix-10): надбавка страницы — от 0 до 98 КБ, у списка государств
/// Википедии (`wiki_states`) — 1,79 МБ, из них 1,71 МБ в одной таблице на 197 строк
/// Markdown (столбец примечаний шириной 5700 символов); оценка этой страницы — 0,52 МБ.
///
/// С пределом доли надбавки таблицы ([`TABLE_ALIGN_SHARE_DIVISOR`]) сумма надбавок страницы
/// не больше четверти вывода её таблиц без дополнения, а он сам — в бюджете вывода 4 МиБ:
/// 2 МиБ на странице не достигаются, предел остаётся страховкой.
pub const TABLE_ALIGN_BUDGET: u64 = 2 * 1024 * 1024;

/// Предел доли надбавки выравнивания одной таблицы — делитель (Ruling прохода fix-10,
/// сомнение 2; I2 финального ревью): пробелы дополнения стоят токенов модели, поэтому таблица
/// выравнивается, только если надбавка увеличивает её вывод не больше чем на 25 %
/// (`надбавка ≤ вывод / 4`; вывод без дополнения — строки заголовка, разделителя и данных,
/// без подписей). Иначе таблица идёт без дополнения, даже если бюджета страницы хватает. У
/// `wiki_states` пробелы одной таблицы занимали 85 % текста страницы.
pub const TABLE_ALIGN_SHARE_DIVISOR: u64 = 4;

/// Элемент HTML с именем `local` (иностранное содержимое SVG и MathML не трогается).
fn is_html(node: &Node, local: &str) -> bool {
    matches!(&node.data, NodeData::Element { name, .. } if name.ns == ns!(html) && *name.local == *local)
}

/// Привести ячейки и секции таблицы `table` к виду, в котором `htmd` ни одной ячейки не
/// теряет (см. документацию модуля). Узел не `table` — ничего. Список детей таблицы
/// пересобирается один раз.
pub(crate) fn normalize_table(table: &Handle) {
    if !is_html(table, "table") {
        return;
    }
    // Состояние `htmd`: `has_thead` — встречен `thead` или взята строка заголовка из тела;
    // `header` — строка заголовка выбрана; `first` — ещё не было ни одной строки.
    let mut has_thead = false;
    let mut header = false;
    let mut first = true;
    let old = std::mem::take(&mut *table.children.borrow_mut());
    let mut kept: Vec<Handle> = Vec::with_capacity(old.len());
    // Решения секций за каждой позицией ([`decisions_after`]) — один обратный проход, при
    // первом пустом `thead` до заголовка (I1 ревью fix-13: поиск вперёд от каждого из подряд
    // идущих пустых `thead` был квадратичным).
    let mut after: Option<Vec<Option<bool>>> = None;
    for (i, section) in old.iter().cloned().enumerate() {
        // Пустой `thead` до заголовка — тело, если за ним `htmd` взял бы заголовок (m-2
        // повторного ревью fix-10): иначе тот занял бы место заголовка, и таблица вышла бы без
        // строки заголовка и разделителя. Заголовка дальше нет — `thead` остаётся: без него
        // в таблице не осталось бы ни `th`, ни `thead`, и `htmd` вывел бы ячейки абзацами, а
        // не строками таблицы (m-4 повторного ревью fix-10b).
        let section = if is_html(&section, "thead")
            && !header
            && is_empty_head(&section)
            && header_follows(
                &section,
                after.get_or_insert_with(|| decisions_after(&old))[i + 1],
            ) {
            renamed(&section, "tbody", table)
        } else {
            section
        };
        if is_html(&section, "thead") && !header {
            has_thead = true;
            let rows = rows_of(&section);
            if let Some(head) = rows.first() {
                rename_cells(head, "td", "th");
                header = cell_counts(head).0 > 0;
                first = false;
            }
            let rest = (rows.len() > 1).then(|| split_after(&section, &rows[0], table));
            kept.push(section);
            if let Some(body) = rest {
                for row in rows_of(&body) {
                    rename_cells(&row, "th", "td");
                }
                kept.push(body);
            }
            continue;
        }
        // `thead` после заголовка — тело: у `htmd` он заменил бы заголовок.
        let section = if is_html(&section, "thead") {
            renamed(&section, "tbody", table)
        } else {
            section
        };
        let rows = if is_html(&section, "tr") {
            vec![Rc::clone(&section)]
        } else if is_html(&section, "tbody") || is_html(&section, "tfoot") {
            rows_of(&section)
        } else {
            Vec::new()
        };
        for row in rows {
            let (th, td) = cell_counts(&row);
            // Пустую строку `htmd` пропускает, и выбор заголовка она не сдвигает.
            if th + td == 0 {
                continue;
            }
            if first && !has_thead && td == 0 && th > 0 {
                header = true;
                has_thead = true;
            } else {
                rename_cells(&row, "th", "td");
            }
            first = false;
        }
        kept.push(section);
    }
    *table.children.borrow_mut() = kept;
}

/// `thead` без ячеек в строке, которую `htmd` взял бы заголовком: в первой `tr`, а без строк —
/// среди детей самого `thead`.
fn is_empty_head(thead: &Handle) -> bool {
    let head = rows_of(thead).into_iter().next();
    let (th, td) = cell_counts(head.as_ref().unwrap_or(thead));
    th + td == 0
}

/// Возьмёт ли `htmd` строку заголовка, если пустой `thead` станет телом: первая непустая
/// строка — его собственная (после пустой первой) или секций за ним — из одних `th`, или
/// дальше идёт непустой `thead` (его первую строку [`normalize_table`] делает заголовком).
/// Пустые строки и пустые `thead` выбор не сдвигают. `after` — решение секций за `thead`
/// ([`decisions_after`]); `None` — решающей секции нет.
fn header_follows(thead: &Handle, after: Option<bool>) -> bool {
    decide(rows_of(thead)).or(after).unwrap_or(false)
}

/// Решение по первой непустой строке: из одних `th` — `true`, иначе `false`; `None` —
/// непустых строк нет.
fn decide(rows: Vec<Handle>) -> Option<bool> {
    rows.iter().find_map(|row| {
        let (th, td) = cell_counts(row);
        (th + td > 0).then_some(td == 0)
    })
}

/// Решение секции `section` для пустого `thead` перед ней: непустой `thead` — `true`, иначе
/// первая непустая строка секции ([`decide`]); `None` — секция ничего не решает.
fn decision_of(section: &Handle) -> Option<bool> {
    if is_html(section, "thead") && !is_empty_head(section) {
        return Some(true);
    }
    let rows = if is_html(section, "tr") {
        vec![Rc::clone(section)]
    } else if ["tbody", "tfoot", "thead"]
        .iter()
        .any(|s| is_html(section, s))
    {
        rows_of(section)
    } else {
        Vec::new()
    };
    decide(rows)
}

/// `out[i]` — решение первой решающей секции из `sections[i..]` ([`decision_of`]), `None` —
/// такой нет; `out[sections.len()]` — `None`. Один обратный проход: O(секций, строк и ячеек)
/// (I1 ревью fix-13). Секции после текущей [`normalize_table`] к этому моменту не менял,
/// поэтому решения те же, что при поиске вперёд.
fn decisions_after(sections: &[Handle]) -> Vec<Option<bool>> {
    let mut out = vec![None; sections.len() + 1];
    for i in (0..sections.len()).rev() {
        out[i] = decision_of(&sections[i]).or(out[i + 1]);
    }
    out
}

/// Строки `tr` среди детей секции.
fn rows_of(section: &Node) -> Vec<Handle> {
    section
        .children
        .borrow()
        .iter()
        .filter(|r| is_html(r, "tr"))
        .cloned()
        .collect()
}

/// Перенести детей `thead` после его строки `head` в новый `tbody` (ребёнок `table`, место в
/// списке детей таблицы задаёт вызывающий).
fn split_after(thead: &Handle, head: &Handle, table: &Handle) -> Handle {
    let body = Node::new(NodeData::Element {
        name: QualName::new(None, ns!(html), LocalName::from("tbody")),
        attrs: RefCell::new(Vec::new()),
        template_contents: RefCell::new(None),
        mathml_annotation_xml_integration_point: false,
    });
    let mut kids = thead.children.borrow_mut();
    let at = kids
        .iter()
        .position(|k| Rc::ptr_eq(k, head))
        .map_or(kids.len(), |i| i + 1);
    let moved = kids.split_off(at);
    for kid in &moved {
        kid.parent.set(Some(Rc::downgrade(&body)));
    }
    *body.children.borrow_mut() = moved;
    body.parent.set(Some(Rc::downgrade(table)));
    body
}

/// Число ячеек `th` и `td` среди детей строки.
fn cell_counts(row: &Node) -> (usize, usize) {
    let children = row.children.borrow();
    let th = children.iter().filter(|c| is_html(c, "th")).count();
    let td = children.iter().filter(|c| is_html(c, "td")).count();
    (th, td)
}

/// Заменить ячейки `from` среди детей строки на `to` ([`renamed`]) на том же месте.
fn rename_cells(row: &Handle, from: &str, to: &str) {
    let mut children = row.children.borrow_mut();
    for slot in children.iter_mut() {
        if is_html(slot, from) {
            *slot = renamed(slot, to, row);
        }
    }
}

/// Элемент `node` с именем `to`: новый узел с теми же атрибутами и детьми, родитель —
/// `parent` (имя элемента в `RcDom` не меняется на месте). Прежний узел остаётся пустым и
/// без родителя; ставит новый узел на его место вызывающий.
fn renamed(node: &Handle, to: &str, parent: &Handle) -> Handle {
    let NodeData::Element {
        name,
        attrs,
        mathml_annotation_xml_integration_point,
        ..
    } = &node.data
    else {
        return Rc::clone(node);
    };
    let new = Node::new(NodeData::Element {
        name: QualName::new(name.prefix.clone(), name.ns.clone(), LocalName::from(to)),
        attrs: RefCell::new(std::mem::take(&mut *attrs.borrow_mut())),
        template_contents: RefCell::new(None),
        mathml_annotation_xml_integration_point: *mathml_annotation_xml_integration_point,
    });
    let kids = std::mem::take(&mut *node.children.borrow_mut());
    for kid in &kids {
        kid.parent.set(Some(Rc::downgrade(&new)));
    }
    *new.children.borrow_mut() = kids;
    new.parent.set(Some(Rc::downgrade(parent)));
    node.parent.set(None);
    new
}

// --- Выравнивание под бюджетом ---

/// Обработчик `table` для `htmd`: выравнивание таблиц страницы под бюджетом надбавки (см.
/// документацию модуля). Один экземпляр — на одно преобразование страницы: счётчик
/// потраченного бюджета общий для всех её таблиц.
pub(crate) struct TableLayout {
    budget: u64,
    /// Сумма надбавок уже выровненных таблиц страницы. `htmd` требует от обработчика
    /// `Send + Sync`; преобразование однопоточное, порядок `Relaxed` достаточен.
    spent: AtomicU64,
}

impl TableLayout {
    /// Обработчик с бюджетом [`TABLE_ALIGN_BUDGET`] (модульные тесты; страница получает
    /// остаток бюджета вывода, `html::html_to_markdown`).
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::with_budget(TABLE_ALIGN_BUDGET)
    }

    /// Обработчик с бюджетом `budget` (0 — ни одной выровненной таблицы с надбавкой).
    pub(crate) fn with_budget(budget: u64) -> Self {
        Self {
            budget,
            spent: AtomicU64::new(0),
        }
    }
}

impl ElementHandler for TableLayout {
    /// Каждая таблица отрисовывается здесь и ровно один раз; встроенному обработчику `htmd`
    /// она не передаётся (I1 финального ревью извлечения, см. документацию модуля).
    fn handle(&self, handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
        let node = element.node;
        // Такую таблицу `htmd` выводит обходом детей, без выравнивания: в режиме `Pure` его
        // обработчик передаёт её дальше, а обработчика после него нет, и `htmd` обходит детей.
        if inside_table_cell(node) || !has_explicit_headers(node) {
            return Some(handlers.walk_children(node));
        }
        let grid = Grid::extract(handlers, node);
        let Some(size) = grid.size() else {
            return walked(handlers, node);
        };
        let extra = size.aligned.saturating_sub(size.plain);
        let spent = self.spent.load(Ordering::Relaxed);
        let total = spent.saturating_add(extra);
        if size.padding_fits() && total <= self.budget {
            self.spent.store(total, Ordering::Relaxed);
            return Some(grid.aligned(handlers, node).into());
        }
        Some(grid.plain(handlers, node).into())
    }
}

/// Таблица с признаком заголовка, но без единой ячейки строк: как у `htmd`
/// (`table_handler`, ветка «нет ни строк, ни заголовка») — обход детей без `\n` по краям в
/// пустых строках, пустой обход — ничего. Подписи отрисовывает только этот обход, один раз:
/// встроенный обработчик сначала отрисовывал их отдельно и выбрасывал результат, поэтому
/// таблица в подписи такой же таблицы удваивала время на каждом уровне (I1 финального
/// ревью извлечения).
fn walked(handlers: &dyn Handlers, table: &Handle) -> Option<HandlerResult> {
    let content = handlers.walk_children(table).content;
    let content = content.trim_matches('\n');
    if content.is_empty() {
        return None;
    }
    Some(format!("\n\n{content}\n\n").into())
}

/// Ячейка, как её видит `htmd`: Markdown содержимого без пробелов по краям.
struct Cell {
    /// Число символов Markdown ячейки до замены переводов строк и `|` — по нему `htmd`
    /// считает ширину столбца (`compute_column_widths`).
    raw_chars: usize,
    /// Текст ячейки в строке таблицы (`normalize_cell_content`).
    text: String,
    /// Число символов `text` — по нему `htmd` считает дополнение.
    text_chars: usize,
}

impl Cell {
    fn new(markdown: &str) -> Self {
        let raw = trim_document_whitespace(markdown);
        let text = trim_document_whitespace(
            &raw.replace('\n', " ")
                .replace('\r', "")
                .replace('|', "&#124;"),
        )
        .to_owned();
        Self {
            raw_chars: raw.chars().count(),
            text_chars: text.chars().count(),
            text,
        }
    }
}

/// Строка заголовка и строки данных таблицы — те, что выводит `htmd`, с отрисованными
/// ячейками.
#[derive(Default)]
struct Grid {
    headers: Vec<Cell>,
    rows: Vec<Vec<Cell>>,
}

/// Размер строк таблицы (заголовок, разделитель, данные; без подписей и пустых строк вокруг —
/// они у обоих видов одинаковы), байт.
#[derive(Debug, PartialEq, Eq)]
struct GridSize {
    /// Выровненная таблица `htmd`.
    aligned: u64,
    /// Та же таблица без дополнения ([`Grid::plain`]).
    plain: u64,
}

impl GridSize {
    /// Надбавка выравнивания не больше 25 % таблицы без дополнения
    /// ([`TABLE_ALIGN_SHARE_DIVISOR`]): `aligned − plain ≤ plain / 4`, граница включительна
    /// (точный счёт в целых).
    fn padding_fits(&self) -> bool {
        self.aligned
            .saturating_sub(self.plain)
            .saturating_mul(TABLE_ALIGN_SHARE_DIVISOR)
            <= self.plain
    }
}

impl Grid {
    /// Строки и ячейки таблицы в том порядке и с тем выбором, что у `htmd` 0.5.6
    /// (`table_handler`): первая строка `thead` (или сам `thead` без строк) — заголовок, сначала
    /// `th`, при пустом — `td`; без `thead` заголовок — первая строка с `th` (у прямой `tr` —
    /// и с `td`); в прочих строках — только `td`, пустые строки пропускаются. Подписи не
    /// отрисовываются (см. документацию модуля).
    fn extract(handlers: &dyn Handlers, table: &Handle) -> Self {
        let mut grid = Self::default();
        let mut has_thead = false;
        for child in children(table) {
            match tag(&child) {
                Some("thead") => {
                    let row = children(&child)
                        .into_iter()
                        .find(|r| tag(r) == Some("tr"))
                        .unwrap_or(child);
                    has_thead = true;
                    grid.headers = cells(handlers, &row, "th");
                    if grid.headers.is_empty() {
                        grid.headers = cells(handlers, &row, "td");
                    }
                }
                Some("tbody" | "tfoot") => {
                    for row in children(&child) {
                        if tag(&row) != Some("tr") {
                            continue;
                        }
                        if !has_thead && grid.headers.is_empty() {
                            grid.headers = cells(handlers, &row, "th");
                            has_thead = !grid.headers.is_empty();
                            if has_thead {
                                continue;
                            }
                        }
                        grid.push_row(cells(handlers, &row, "td"));
                    }
                }
                Some("tr") => {
                    if !has_thead && grid.headers.is_empty() {
                        grid.headers = cells(handlers, &child, "th");
                        if grid.headers.is_empty() {
                            grid.headers = cells(handlers, &child, "td");
                        }
                        has_thead = !grid.headers.is_empty();
                    } else {
                        grid.push_row(cells(handlers, &child, "td"));
                    }
                }
                _ => {}
            }
        }
        grid
    }

    fn push_row(&mut self, row: Vec<Cell>) {
        if !row.is_empty() {
            self.rows.push(row);
        }
    }

    /// Число столбцов: самая длинная строка (как у `htmd`).
    fn columns(&self) -> usize {
        self.rows
            .iter()
            .map(Vec::len)
            .chain([self.headers.len()])
            .max()
            .unwrap_or(0)
    }

    /// Точный размер строк выровненной таблицы `htmd` и той же таблицы без дополнения, за
    /// O(ячеек): строки × столбцы не перебираются. `None` — таблицы нет (ни одной ячейки),
    /// `htmd` выводит обход детей.
    ///
    /// Строка `htmd` — `|`, для каждого столбца `i` ` {ячейка}{пробелы до w[i]} |`, затем `\n`.
    /// Отсутствующая ячейка — пустая. Значит, строка стоит `2 + 3n + Σw` плюс по каждой своей
    /// ячейке `bytes − min(w[i], chars)`. Разделитель — `2 + 3n + Σw`. Без дополнения:
    /// строка — `2 + Σ(3 + bytes)` по своим ячейкам, у заголовка ещё 3 байта на каждую
    /// недостающую ячейку, разделитель — `2 + 6n`.
    fn size(&self) -> Option<GridSize> {
        let n = self.columns();
        if n == 0 {
            return None;
        }
        let widths = self.widths();
        let n64 = n as u64;
        let base = widths
            .iter()
            .fold(2 + 3 * n64, |acc, &w| acc.saturating_add(w as u64));
        let line = |row: &[Cell]| -> (u64, u64) {
            let mut aligned = base;
            let mut plain = 2u64;
            for (i, cell) in row.iter().enumerate() {
                let bytes = cell.text.len() as u64;
                aligned = aligned.saturating_add(bytes - widths[i].min(cell.text_chars) as u64);
                plain = plain.saturating_add(3 + bytes);
            }
            (aligned, plain)
        };
        let mut size = GridSize {
            aligned: 0,
            plain: 0,
        };
        if !self.headers.is_empty() {
            let (aligned, plain) = line(&self.headers);
            size.aligned = aligned.saturating_add(base);
            // Недостающие ячейки заголовка без дополнения — по 3 байта (`  |`, m-1), затем
            // разделитель.
            let missing = (n - self.headers.len()) as u64;
            size.plain = plain.saturating_add(3 * missing + 2 + 6 * n64);
        }
        for row in &self.rows {
            let (aligned, plain) = line(row);
            size.aligned = size.aligned.saturating_add(aligned);
            size.plain = size.plain.saturating_add(plain);
        }
        Some(size)
    }

    /// Ширина столбцов `htmd` (`compute_column_widths`): самая длинная ячейка столбца в
    /// символах Markdown до замены переводов строк и `|`.
    fn widths(&self) -> Vec<usize> {
        let mut widths = vec![0usize; self.columns()];
        for (i, cell) in self.headers.iter().enumerate() {
            widths[i] = cell.raw_chars;
        }
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.raw_chars);
            }
        }
        widths
    }

    /// Начало таблицы, как у `htmd`: пустая строка и подписи, каждая без пробелов по краям и
    /// с `\n`.
    fn head(handlers: &dyn Handlers, table: &Handle) -> String {
        let mut out = String::from("\n\n");
        for child in children(table) {
            if tag(&child) == Some("caption")
                && let Some(res) = handlers.handle(&child)
            {
                out.push_str(trim_document_whitespace(&res.content));
                out.push('\n');
            }
        }
        out
    }

    /// Выровненная таблица — байт в байт вывод `htmd` 0.5.6 (`table_handler`,
    /// `format_row_padded`, `format_separator_padded`), но из уже отрисованных ячеек: второй
    /// отрисовки ячеек нет (m-4 повторного ревью fix-10). Строка — `|`, по каждому столбцу
    /// ` {ячейка}{пробелы до ширины} |`, недостающая ячейка пустая; разделитель — `-` на ширину
    /// столбца. Совпадение с `htmd` проверяет тест `aligned_size_matches_htmd`.
    fn aligned(&self, handlers: &dyn Handlers, table: &Handle) -> String {
        let widths = self.widths();
        let mut out = Self::head(handlers, table);
        let line = |out: &mut String, row: &[Cell]| {
            out.push('|');
            for (i, &w) in widths.iter().enumerate() {
                let (text, chars) = row
                    .get(i)
                    .map_or(("", 0), |c| (c.text.as_str(), c.text_chars));
                out.push(' ');
                out.push_str(text);
                out.extend(std::iter::repeat_n(' ', w.saturating_sub(chars)));
                out.push_str(" |");
            }
            out.push('\n');
        };
        if !self.headers.is_empty() {
            line(&mut out, &self.headers);
            out.push('|');
            for &w in &widths {
                out.push(' ');
                out.extend(std::iter::repeat_n('-', w));
                out.push_str(" |");
            }
            out.push('\n');
        }
        for row in &self.rows {
            line(&mut out, row);
        }
        out.push('\n');
        out
    }

    /// Таблица Markdown без дополнения: подписи (как у `htmd`), строка заголовка на все столбцы
    /// (недостающие ячейки пустые) и разделитель `| --- |` на каждый столбец, строки данных —
    /// только свои ячейки (GFM дополняет их сам). Пустые строки вокруг — как у `htmd`.
    fn plain(&self, handlers: &dyn Handlers, table: &Handle) -> String {
        let mut out = Self::head(handlers, table);
        let line = |out: &mut String, row: &[Cell]| {
            out.push('|');
            for cell in row {
                out.push(' ');
                out.push_str(&cell.text);
                out.push_str(" |");
            }
            out.push('\n');
        };
        if !self.headers.is_empty() {
            // GFM: ячеек в заголовке столько же, сколько в разделителе, иначе таблица не
            // распознаётся (m-1 повторного ревью fix-10); недостающие — пустые.
            line(&mut out, &self.headers);
            out.pop();
            for _ in self.headers.len()..self.columns() {
                out.push_str("  |");
            }
            out.push_str("\n|");
            for _ in 0..self.columns() {
                out.push_str(" --- |");
            }
            out.push('\n');
        }
        for row in &self.rows {
            line(&mut out, row);
        }
        out.push('\n');
        out
    }
}

/// Ячейки строки с именем `cell_tag` (любое пространство имён, как у `htmd`), отрисованные
/// обработчиками `htmd`.
fn cells(handlers: &dyn Handlers, row: &Handle, cell_tag: &str) -> Vec<Cell> {
    children(row)
        .iter()
        .filter(|c| tag(c) == Some(cell_tag))
        .filter_map(|c| handlers.handle(c))
        .map(|res| Cell::new(&res.content))
        .collect()
}

/// Копия списка детей: отрисовка ячейки может менять дерево внутри неё (`htmd` сливает
/// соседей), заём списка на это время не держится.
fn children(node: &Handle) -> Vec<Handle> {
    node.children.borrow().iter().cloned().collect()
}

/// Локальное имя элемента в любом пространстве имён (так сравнивает `htmd`).
fn tag(node: &Node) -> Option<&str> {
    match &node.data {
        NodeData::Element { name, .. } => Some(&*name.local),
        _ => None,
    }
}

/// Пробельные символы документа по `htmd` (`is_document_whitespace`).
fn trim_document_whitespace(s: &str) -> &str {
    s.trim_matches(['\t', '\n', '\r', ' '])
}

/// Есть ли у таблицы предок-элемент `td` или `th` (`htmd` `is_inside_table_cell`): такую
/// таблицу `htmd` не выравнивает.
fn inside_table_cell(node: &Handle) -> bool {
    let mut cur = parent(node);
    while let Some(p) = cur {
        if matches!(tag(&p), Some("td" | "th")) {
            return true;
        }
        cur = parent(&p);
    }
    false
}

/// Родитель узла по указателю `parent` `RcDom`.
fn parent(node: &Handle) -> Option<Handle> {
    let weak = node.parent.take();
    let up = weak.as_ref().and_then(std::rc::Weak::upgrade);
    node.parent.set(weak);
    up
}

/// Есть ли в таблице потомок `th` или `thead` вне вложенных таблиц (`htmd`
/// `has_explicit_headers`; прямой ребёнок `table` тоже просматривается). Явный стек вместо
/// рекурсии. Ошибка в сторону «есть» стоит только лишнего подсчёта, в сторону «нет» — обход
/// бюджета, поэтому правило — точная копия.
fn has_explicit_headers(table: &Handle) -> bool {
    let mut stack = vec![(Rc::clone(table), true)];
    while let Some((node, is_root)) = stack.pop() {
        for child in node.children.borrow().iter() {
            match tag(child) {
                Some("table") if !is_root => continue,
                Some("th" | "thead") => return true,
                _ => stack.push((Rc::clone(child), false)),
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use html5ever::parse_document;
    use html5ever::tendril::TendrilSink;
    use markup5ever_rcdom::RcDom;

    /// Родитель узла по указателю — `parent`.
    fn assert_parent(node: &Handle, parent: &Handle) {
        let p = node.parent.take();
        let up = p.as_ref().and_then(std::rc::Weak::upgrade);
        node.parent.set(p);
        assert!(up.is_some_and(|u| Rc::ptr_eq(&u, parent)));
    }

    /// Имя элемента.
    fn name_of(node: &Node) -> Option<String> {
        match &node.data {
            NodeData::Element { name, .. } => Some(name.local.to_string()),
            _ => None,
        }
    }

    /// Первая таблица документа после [`normalize_table`]: имена секций и имена ячеек строк
    /// по строкам. Указатели `parent` секций, строк и детей ячеек проверены.
    fn layout(html: &str) -> (Vec<String>, Vec<Vec<String>>) {
        let dom = parse_document(RcDom::default(), Default::default()).one(html);
        let mut stack = vec![dom.document.clone()];
        while let Some(n) = stack.pop() {
            if is_html(&n, "table") {
                normalize_table(&n);
                let mut sections = Vec::new();
                let mut out = Vec::new();
                for s in n.children.borrow().iter() {
                    assert_parent(s, &n);
                    sections.extend(name_of(s));
                    for r in s.children.borrow().iter().filter(|r| is_html(r, "tr")) {
                        assert_parent(r, s);
                        let names = r
                            .children
                            .borrow()
                            .iter()
                            .filter_map(|c| name_of(c))
                            .collect();
                        for c in r.children.borrow().iter() {
                            assert_parent(c, r);
                            for k in c.children.borrow().iter() {
                                assert_parent(k, c);
                            }
                        }
                        out.push(names);
                    }
                }
                return (sections, out);
            }
            stack.extend(n.children.borrow().iter().cloned());
        }
        (Vec::new(), Vec::new())
    }

    /// Имена ячеек строк первой таблицы документа, по строкам.
    fn cells(html: &str) -> Vec<Vec<String>> {
        layout(html).1
    }

    #[test]
    fn rename_table() {
        let v = |rows: &[&[&str]]| -> Vec<Vec<String>> {
            rows.iter()
                .map(|r| r.iter().map(|s| (*s).to_owned()).collect())
                .collect()
        };
        // Карточка: первая строка из `th` — заголовок, остальные `th` → `td`.
        assert_eq!(
            cells(
                "<table><tr><th>T</th></tr><tr><th>k</th><td>v</td></tr><tr><th>S</th></tr></table>"
            ),
            v(&[&["th"], &["td", "td"], &["td"]])
        );
        // Смешанная первая строка без `thead` — данные.
        assert_eq!(
            cells("<table><tr><th>k</th><td>v</td></tr></table>"),
            v(&[&["td", "td"]])
        );
        // Угловая `td` в `thead` → `th`; `th` строк тела → `td`.
        assert_eq!(
            cells(
                "<table><thead><tr><td></td><th>A</th></tr></thead><tbody><tr><th>r</th><td>1</td></tr></tbody></table>"
            ),
            v(&[&["th", "th"], &["td", "td"]])
        );
        // Обычная таблица не меняется.
        assert_eq!(
            cells(
                "<table><thead><tr><th>A</th></tr></thead><tbody><tr><td>1</td></tr></tbody></table>"
            ),
            v(&[&["th"], &["td"]])
        );
    }

    /// Секции (m6 ревью задачи 10): строки `thead` после первой — в новом `tbody` сразу за ним,
    /// `thead` после заголовка — `tbody`, пустой `thead` до заголовка остаётся, а строка из
    /// `th` после него — данные.
    #[test]
    fn normalize_sections() {
        let names = |n: &[&str]| -> Vec<String> { n.iter().map(|s| (*s).to_owned()).collect() };
        let v = |rows: &[&[&str]]| -> Vec<Vec<String>> {
            rows.iter()
                .map(|r| r.iter().map(|s| (*s).to_owned()).collect())
                .collect()
        };
        assert_eq!(
            layout(
                "<table><thead><tr><th>A</th></tr><tr><th>a</th><td>b</td></tr></thead><tbody><tr><th>r</th></tr></tbody></table>"
            ),
            (
                names(&["thead", "tbody", "tbody"]),
                v(&[&["th"], &["td", "td"], &["td"]])
            )
        );
        assert_eq!(
            layout(
                "<table><thead><tr><th>A</th></tr></thead><tbody><tr><td>1</td></tr></tbody><thead><tr><th>B</th></tr></thead><thead></thead></table>"
            ),
            (
                names(&["thead", "tbody", "tbody", "tbody"]),
                v(&[&["th"], &["td"], &["td"]])
            )
        );
        // Пустой `thead` до заголовка — тело (m-2 повторного ревью fix-10): первая непустая
        // строка из одних `th` — заголовок, пустые строки выбор не сдвигают.
        assert_eq!(
            layout(
                "<table><thead></thead><tbody><tr><th>A</th><th>B</th></tr><tr><td>1</td></tr></tbody></table>"
            ),
            (names(&["tbody", "tbody"]), v(&[&["th", "th"], &["td"]]))
        );
        assert_eq!(
            layout(
                "<table><thead><tr></tr><tr><th>A</th></tr></thead><tbody><tr><td>1</td></tr></tbody></table>"
            ),
            (names(&["tbody", "tbody"]), v(&[&[], &["th"], &["td"]]))
        );
        // m-4 повторного ревью fix-10b: за пустым `thead` нет строки из одних `th` — он
        // остаётся, и `htmd` печатает строки таблицы, а не обходит детей.
        assert_eq!(
            layout(
                "<table><thead></thead><tbody><tr><td>1</td><td>2</td></tr><tr><th>k</th><td>3</td></tr></tbody></table>"
            ),
            (
                names(&["thead", "tbody"]),
                v(&[&["td", "td"], &["td", "td"]])
            )
        );
        assert_eq!(
            layout("<table><thead><tr></tr></thead><tr><th>k</th><td>v</td></tr></table>"),
            (names(&["thead", "tbody"]), v(&[&[], &["td", "td"]]))
        );
        // Тело до `thead`: заголовок — строка `thead` (у `htmd` он и так первый в выводе).
        assert_eq!(
            layout(
                "<table><tbody><tr><td>1</td></tr></tbody><thead><tr><td>H</td></tr></thead></table>"
            ),
            (names(&["tbody", "thead"]), v(&[&["td"], &["th"]]))
        );
    }

    // --- Выравнивание под бюджетом ---

    use std::sync::{Arc, Mutex};

    use crate::guard::{guarded_parse, prepare_dom};
    use crate::html::{SKIPPED_TAGS, render_with};
    use htmd::HtmlToMarkdown;

    /// Таблицы разного строения: заголовки, матрица, «ключ — значение», недостающие ячейки,
    /// многобайтовые символы, `|` и переводы строк в ячейках, вложенная таблица, подпись,
    /// пустые ячейки и строки, ссылки и форматирование, несколько `tbody`, `tfoot`, список в
    /// ячейке, пустой, многострочный и повторный `thead`.
    const TABLES: &[&str] = &[
        "<table><thead><tr><th>Name</th><th>Kind</th></tr></thead><tbody><tr><td>alpha</td><td>first</td></tr><tr><td>beta</td><td>second item</td></tr></tbody></table>",
        "<table><thead><tr><td></td><th>A</th><th>B</th></tr></thead><tbody><tr><th>r1</th><td>one</td><td>two</td></tr><tr><th>r2</th><td>three</td><td>four</td></tr></tbody></table>",
        "<table><tr><th colspan=2>Rust</th></tr><tr><th>Paradigms</th><td>Concurrent, functional</td></tr><tr><th colspan=2>Impl</th></tr><tr><th>License</th><td>MIT</td></tr></table>",
        "<table><tr><th>a</th><th>b</th><th>c</th></tr><tr><td>1</td></tr><tr><td>1</td><td>2</td><td>3</td><td>4</td><td>5</td></tr><tr><td></td><td>x</td></tr></table>",
        "<table><thead><tr><th>Город</th><th>描述</th></tr></thead><tbody><tr><td>Москва 🦀</td><td>首都 и ещё</td></tr><tr><td>é&nbsp;ü</td><td>x</td></tr></tbody></table>",
        "<table><tr><th>p|q</th></tr><tr><td>a | b || c</td></tr><tr><td>line<br>break<br><br>again</td></tr></table>",
        "<table><tr><th>outer</th></tr><tr><td>before<table><tr><th>in</th></tr><tr><td>inner cell</td></tr></table>after</td></tr></table>",
        "<table><caption> Cap <b>bold</b> </caption><thead><tr><th>h</th></tr></thead><tbody><tr><td>v</td></tr></tbody></table>",
        "<table><thead><tr><th>h1</th><th>h2</th></tr></thead><tbody><tr></tr><tr><td></td><td></td></tr><tr><td> spaced  text </td><td>\n</td></tr></tbody></table>",
        "<table><tr><th>Link</th><th>Code</th></tr><tr><td><a href=\"/very/long/target/path?x=1\" title=\"T\">x</a></td><td><code>fn main() {}</code> and <b>bold</b> *star*</td></tr></table>",
        "<table><thead><tr><th>k</th></tr></thead><tbody><tr><td>1</td></tr></tbody><tbody><tr><td>22</td></tr></tbody><tfoot><tr><td>total 333</td></tr></tfoot></table>",
        "<table><thead><tr><th>only header, no rows</th></tr></thead></table>",
        "<table><tr><th>x</th></tr><tr><td><ul><li>one</li><li>two<ul><li>deep</li></ul></li></ul></td></tr></table>",
        "<table><thead></thead><tbody><tr><th>Alpha</th><th>Beta</th></tr><tr><td>one</td><td>two</td></tr></tbody></table>",
        "<table><thead><tr><th>Name</th><th>Size</th></tr><tr><th>first</th><th>bytes</th></tr></thead><tbody><tr><td>alpha</td><td>1</td></tr></tbody></table>",
        "<table><thead><tr><th>Head</th></tr></thead><tbody><tr><td>value</td></tr></tbody><thead><tr><th>Later</th></tr><tr><td>more</td></tr></thead><thead></thead></table>",
        // m-3 повторного ревью fix-10: `th` внутри `<math>` в ячейке, таблица в подписи,
        // вложенная таблица в ячейке заголовка.
        "<table><tr><td>a</td></tr><tr><td>x<math><th>m</th></math></td><td>y</td></tr></table>",
        "<table><caption>c<table><tr><th>i</th></tr><tr><td>j</td></tr></table></caption><tr><th>h</th></tr><tr><td>v</td></tr></table>",
        "<table><tr><th>h<table><tr><th>i</th></tr><tr><td>j</td></tr></table></th><th>k</th></tr><tr><td>v</td><td>w</td></tr></table>",
        // m-1: заголовок короче строк — без дополнения недостающие ячейки заголовка пустые.
        "<table><tr><th>a</th></tr><tr><td>1</td><td>two</td><td>three three</td></tr></table>",
        // m-4 повторного ревью fix-10b: пустой `thead` перед строками из одних `td`.
        "<table><thead></thead><tbody><tr><td>1</td><td>2</td></tr><tr><td>3</td><td>four</td></tr></tbody></table>",
    ];

    /// Подготовленное дерево, как в `html_to_markdown`.
    fn tree(html: &str) -> Handle {
        let tree = guarded_parse(html).expect("разбор под порогами");
        prepare_dom(&tree).expect("подготовка DOM");
        tree
    }

    /// Что зонд увидел у таблицы, которую `htmd` выравнивает.
    struct Probed {
        /// Подсчитанный размер выровненной таблицы с пустыми строками и подписями.
        predicted: u64,
        /// То же для таблицы без дополнения.
        predicted_plain: u64,
        /// Вывод встроенного обработчика `htmd`.
        aligned: String,
        /// Та же таблица, построенная нами ([`Grid::aligned`]).
        built: String,
        /// Та же таблица без дополнения ([`Grid::plain`]).
        plain: String,
    }

    /// Зонд вместо [`TableLayout`]: у каждой таблицы, которую `htmd` выравнивает, считает
    /// размер, строит таблицу без дополнения и вывод встроенного обработчика.
    #[derive(Default)]
    struct Probe(Mutex<Vec<Probed>>);

    impl Probe {
        fn handle(&self, handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
            let node = element.node;
            if inside_table_cell(node) || !has_explicit_headers(node) {
                return handlers.fallback(element);
            }
            let grid = Grid::extract(handlers, node);
            let captions: u64 = children(node)
                .iter()
                .filter(|c| tag(c) == Some("caption"))
                .filter_map(|c| handlers.handle(c))
                .map(|r| trim_document_whitespace(&r.content).len() as u64 + 1)
                .sum();
            let res = handlers.fallback(element)?;
            if let Some(size) = grid.size() {
                // Отрисовка подписей может снова вызвать зонд (таблица в подписи): журнал
                // берётся только после неё.
                let probed = Probed {
                    // "\n\n", подписи, строки таблицы, "\n".
                    predicted: 2 + captions + size.aligned + 1,
                    predicted_plain: 2 + captions + size.plain + 1,
                    aligned: res.content.clone(),
                    built: grid.aligned(handlers, node),
                    plain: grid.plain(handlers, node),
                };
                self.0.lock().expect("журнал зонда").push(probed);
            }
            Some(res)
        }
    }

    /// Все выравниваемые таблицы случаев [`TABLES`] и широкой таблицы с кириллицей.
    fn probe_all() -> Vec<Probed> {
        let mut wide = String::from("<table><thead><tr><th>w</th><th>v</th></tr></thead><tbody>");
        for i in 0..50 {
            wide.push_str(&format!(
                "<tr><td>{}</td><td>{i}</td></tr>",
                "ж".repeat(i * 7)
            ));
        }
        wide.push_str("</tbody></table>");
        let mut all = Vec::new();
        for html in TABLES.iter().copied().chain([wide.as_str()]) {
            let probe = Arc::new(Probe::default());
            let seen = Arc::clone(&probe);
            let converter = HtmlToMarkdown::builder()
                .skip_tags(SKIPPED_TAGS.to_vec())
                .add_handler(vec!["table"], move |h: &dyn Handlers, e: Element| {
                    seen.handle(h, e)
                })
                .build();
            converter.tree_to_markdown(&tree(html));
            let mut log = std::mem::take(&mut *probe.0.lock().expect("журнал зонда"));
            assert!(!log.is_empty(), "таблица не выровнена: {html}");
            all.append(&mut log);
        }
        assert!(all.len() > TABLES.len(), "{}", all.len());
        all
    }

    /// Подсчёт размера выровненной таблицы совпадает с выводом `htmd` байт в байт (I1 ревью
    /// задачи 10), размер без дополнения — с [`Grid::plain`], а своя выровненная таблица
    /// ([`Grid::aligned`], m-4 повторного ревью fix-10) — с выводом `htmd` целиком. Падение
    /// после обновления `htmd` — повод сверить модуль с его обработчиком.
    #[test]
    fn aligned_size_matches_htmd() {
        for p in probe_all() {
            assert_eq!(p.predicted, p.aligned.len() as u64, "{}", p.aligned);
            assert_eq!(p.predicted_plain, p.plain.len() as u64, "{}", p.plain);
            assert_eq!(p.built, p.aligned);
        }
    }

    /// Ячейки строк таблиц Markdown (`|…|`): разделитель — `---` на столбец, пустые ячейки в
    /// конце строки (дополнение недостающих) отброшены.
    fn table_cells(md: &str) -> Vec<Vec<String>> {
        md.lines()
            .filter(|l| l.starts_with('|'))
            .map(|l| {
                let mut cells: Vec<String> = l
                    .trim_matches('|')
                    .split('|')
                    .map(|c| {
                        let c = c.trim();
                        if !c.is_empty() && c.chars().all(|ch| ch == '-') {
                            "---".to_owned()
                        } else {
                            c.to_owned()
                        }
                    })
                    .collect();
                while cells.last().is_some_and(String::is_empty) {
                    cells.pop();
                }
                cells
            })
            .collect()
    }

    /// Вывод страницы встроенным обработчиком таблиц `htmd`, без [`TableLayout`].
    fn htmd_only(html: &str) -> String {
        let converter = HtmlToMarkdown::builder()
            .skip_tags(SKIPPED_TAGS.to_vec())
            .build();
        crate::html::collapse_blank_lines(&converter.tree_to_markdown(&tree(html)))
    }

    /// Таблицы, которые `htmd` выводит обходом детей (без заголовков, внутри ячейки, с
    /// признаком заголовка без ячеек, пустые), [`TableLayout`] выводит сам — байт в байт как
    /// встроенный обработчик (I1 финального ревью извлечения), в том числе неглубокие формы
    /// с таблицей в подписи. Формы со строчным текстом вокруг таблицы без ячеек (m-A2
    /// повторного ревью финального прохода): между `x` и `y` блочных элементов нет, поэтому
    /// пустой обход (`None`) и обёртка `\n\n` различимы — `xy` у `htmd`, а не `x\n\ny`.
    #[test]
    fn walked_tables_match_htmd() {
        for html in [
            "x<table><thead></thead><caption></caption></table>y",
            "x<table><thead></thead><caption>c</caption></table>y",
            "<p>a</p><table><tr><td>a</td><td>b</td></tr><tr><td>c</td></tr></table><p>z</p>",
            "<table><tr><td>x<table><tr><th>i</th></tr><tr><td>j</td></tr></table>y</td></tr></table>",
            "<table><thead></thead><caption>c <b>bold</b></caption></table><p>after</p>",
            "<p>a</p><table><thead></thead></table><p>b</p>",
            "<table><thead><tr></tr></thead><caption></caption></table>",
            "<table><thead></thead><caption>x <table><thead></thead><caption>y <table><thead></thead><caption>z</caption></table></caption></table></caption></table>",
            "<table><caption>c <table><tr><td>d</td></tr></table></caption><tr><td>v</td></tr></table>",
            "<table><caption>c<table><thead></thead><caption>in</caption></table></caption><tr><th>h</th></tr><tr><td>v</td></tr></table>",
        ] {
            assert_eq!(crate::html::render(&tree(html)), htmd_only(html), "{html}");
        }
    }

    /// Таблица без дополнения — та же таблица: подписи и пустые строки вокруг совпадают,
    /// ячейки строк совпадают по порядку и тексту, лишних пробелов в строках нет.
    #[test]
    fn plain_layout_keeps_cells() {
        let prose = |md: &str| -> Vec<String> {
            md.split('\n')
                .filter(|l| !l.starts_with('|'))
                .map(str::to_owned)
                .collect()
        };
        for p in probe_all() {
            assert_eq!(prose(&p.aligned), prose(&p.plain), "{}", p.aligned);
            assert_eq!(
                table_cells(&p.aligned),
                table_cells(&p.plain),
                "{}",
                p.plain
            );
            // Ни одного пробела дополнения: строка — ровно её ячейки.
            for line in p.plain.lines().filter(|l| l.starts_with('|')) {
                let rebuilt: String = line
                    .trim_matches('|')
                    .split('|')
                    .map(|c| format!(" {} |", c.trim()))
                    .collect();
                assert_eq!(line, format!("|{rebuilt}"), "{}", p.plain);
            }
            // GFM: в строке заголовка столько же ячеек, сколько в разделителе (m-1).
            let lines: Vec<&str> = p.plain.lines().collect();
            for (i, line) in lines.iter().enumerate().skip(1) {
                if line.starts_with("| --- |") {
                    assert_eq!(
                        lines[i - 1].matches('|').count(),
                        line.matches('|').count(),
                        "{}",
                        p.plain
                    );
                }
            }
        }
    }

    /// Таблица, у которой надбавка не больше четверти вывода без дополнения: заголовок в 8
    /// символов, строки в 7 (по пробелу дополнения в строке и пять тире сверх `---`).
    const NARROW: &str = "<table><tr><th>abcdefgh</th></tr><tr><td>abcdefg</td></tr><tr><td>abcdefx</td></tr></table>";

    /// Вывод страницы: таблица сверх бюджета — та же, что строит [`Grid::plain`], а при
    /// бюджете — та же, что строит `htmd`.
    #[test]
    fn layout_follows_budget() {
        let html = "<table><caption>Cap</caption><tr><th>abcdefgh</th></tr><tr><td>abcdefg</td></tr></table>";
        let aligned = render_with(&tree(html), TableLayout::new());
        let plain = render_with(&tree(html), TableLayout::with_budget(0));
        assert_eq!(aligned, "Cap\n| abcdefgh |\n| -------- |\n| abcdefg  |");
        assert_eq!(plain, "Cap\n| abcdefgh |\n| --- |\n| abcdefg |");
    }

    /// Бюджет — на страницу и тратится в порядке документа: при бюджете, равном надбавке
    /// одной таблицы, первая из двух одинаковых выровнена, вторая — нет.
    #[test]
    fn budget_is_spent_in_document_order() {
        // Надбавка: две строки по пробелу дополнения и разделитель на 5 тире длиннее `---`.
        let extra = 2 + 5;
        let html = format!("{NARROW}<p>mid</p>{NARROW}");
        let md = render_with(&tree(&html), TableLayout::with_budget(extra));
        let (first, second) = md.split_once("mid").expect("абзац между таблицами");
        assert!(first.contains("| abcdefg  |"), "{md}");
        assert!(
            second.contains("\n| abcdefg |\n") && second.contains("| --- |"),
            "{md}"
        );
        // На одну таблицу бюджета не хватает — обе без дополнения.
        let md = render_with(&tree(&html), TableLayout::with_budget(extra - 1));
        assert_eq!(md.matches("\n| abcdefg |\n").count(), 2, "{md}");
    }

    /// Надбавка выравнивания таблицы — не больше 25 % её вывода без дополнения (Ruling прохода
    /// fix-10, сомнение 2; I2 финального ревью): иначе таблица идёт без дополнения, даже если
    /// бюджета страницы хватает. Граница включительна.
    #[test]
    fn padding_share_is_limited_per_table() {
        let fits = |aligned, plain| GridSize { aligned, plain }.padding_fits();
        assert!(fits(125, 100) && fits(100, 100) && fits(0, 0));
        assert!(!fits(126, 100) && !fits(1, 0));
        // 7 байт надбавки при 45 без дополнения — выровнена.
        assert_eq!(
            render_with(&tree(NARROW), TableLayout::new()),
            "| abcdefgh |\n| -------- |\n| abcdefg  |\n| abcdefx  |"
        );
        // Широкий заголовок над короткой строкой: 32 байта надбавки при 36 — без дополнения.
        let wide = "<table><tr><th>head of the column</th></tr><tr><td>a</td></tr></table>";
        assert_eq!(
            render_with(&tree(wide), TableLayout::new()),
            "| head of the column |\n| --- |\n| a |"
        );
    }
}
