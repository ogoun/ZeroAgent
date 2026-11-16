//! Таблицы и `htmd` (R1-C2): имена ячеек до `htmd` (перенос прохода fix-9 в задачу 10) и
//! выравнивание под бюджетом (I1 ревью задачи 10).
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
//!   он заменил бы заголовок. Пустой `thead` до заголовка остаётся, но строка из одних `th`
//!   после него — данные: `htmd` после `thead` заголовок из тела не берёт;
//! - во всех прочих строках `th` становятся `td`.
//!
//! Текст и порядок ячеек не меняются. Таблица, где после замены нет ни `th`, ни `thead`,
//! у `htmd` идёт обычным обходом детей (режим `Pure`) — текст ячеек сохраняется и там.
//! Вложенные таблицы — отдельные элементы `table`, их обрабатывает свой вызов. Цена — один
//! проход по секциям, строкам и ячейкам таблицы.
//!
//! ## Выравнивание
//!
//! `htmd` дополняет каждую ячейку пробелами до самой широкой ячейки столбца, а недостающие
//! ячейки строки — пустыми до ширины столбца (`format_row_padded`). Вывод растёт как
//! строки × сумма ширин столбцов: одна ячейка в 1 МиБ и 55 000 строк — около 55 ГБ в памяти
//! подпроцесса ещё до печати.
//!
//! [`TableLayout`] — свой обработчик `table` в `htmd` (вызов из `html::render`), он стоит
//! перед встроенным:
//! - таблицу, которую `htmd` выводит обходом детей (без явных заголовков или внутри ячейки),
//!   сразу отдаёт встроенному обработчику;
//! - у остальных отрисовывает ячейки теми же обработчиками `htmd` и считает **точный** размер
//!   выровненной таблицы за O(ячеек), не строя её. Разница с той же таблицей без дополнения —
//!   надбавка выравнивания;
//! - пока сумма надбавок страницы не больше [`TABLE_ALIGN_BUDGET`], таблицу строит сам
//!   `htmd`: вывод байт в байт прежний;
//! - иначе — та же таблица Markdown без дополнения: те же подписи, строки, ячейки и их
//!   экранирование, разделитель `---`, строка не длиннее своих ячеек. Размер линеен по
//!   ячейкам.
//!
//! Размер считается по Markdown ячеек, а не по DOM: адрес ссылки, отступы вложенных списков
//! и экранирование входят в ширину, а текстом узлов не видны. Решение принимается у самого
//! элемента `table`, а не переименованием `th` в DOM: `htmd` признаёт таблицу «с
//! заголовками» по любому потомку `th` или `thead` в любом пространстве имён (`th` внутри
//! `<math>`), и переименованная таблица вернулась бы к выравниванию.
//!
//! Цена — ячейки выровненной таблицы отрисовываются дважды (подсчёт и сам `htmd`). Таблицы
//! внутри ячеек не считаются (их `htmd` выравнивать не станет), подписи при подсчёте не
//! отрисовываются: повторная отрисовка не складывается по вложенности.

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
/// 2 МиБ — половина предела вывода подпроцесса (`zero_agent::extract::HTML_OUTPUT_MAX`,
/// 4 МиБ): страница не упирается в предел из-за пробелов. Реальные страницы корпуса его не
/// достигают (замер fix-10): надбавка страницы — от 0 до 98 КБ, у списка государств
/// Википедии (`wiki_states`) — 1,79 МБ, из них 1,71 МБ в одной таблице на 197 строк
/// Markdown (столбец примечаний шириной 5700 символов).
pub const TABLE_ALIGN_BUDGET: u64 = 2 * 1024 * 1024;

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
    for section in old {
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
    /// Обработчик с бюджетом [`TABLE_ALIGN_BUDGET`].
    pub(crate) fn new() -> Self {
        Self::with_budget(TABLE_ALIGN_BUDGET)
    }

    /// Обработчик со своим бюджетом (модульные тесты: 0 — ни одной выровненной таблицы с
    /// надбавкой).
    pub(crate) fn with_budget(budget: u64) -> Self {
        Self {
            budget,
            spent: AtomicU64::new(0),
        }
    }
}

impl ElementHandler for TableLayout {
    fn handle(&self, handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
        let node = element.node;
        // Такую таблицу `htmd` сам выводит обходом детей, без выравнивания.
        if inside_table_cell(node) || !has_explicit_headers(node) {
            return handlers.fallback(element);
        }
        let grid = Grid::extract(handlers, node);
        let Some(size) = grid.size() else {
            return handlers.fallback(element);
        };
        let extra = size.aligned.saturating_sub(size.plain);
        let spent = self.spent.load(Ordering::Relaxed);
        let total = spent.saturating_add(extra);
        if total <= self.budget {
            self.spent.store(total, Ordering::Relaxed);
            return handlers.fallback(element);
        }
        Some(grid.plain(handlers, node).into())
    }
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
    /// строка — `2 + Σ(3 + bytes)` по своим ячейкам, разделитель — `2 + 6n`.
    fn size(&self) -> Option<GridSize> {
        let n = self.columns();
        if n == 0 {
            return None;
        }
        let mut widths = vec![0usize; n];
        for (i, cell) in self.headers.iter().enumerate() {
            widths[i] = cell.raw_chars;
        }
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.raw_chars);
            }
        }
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
            size.plain = plain.saturating_add(2 + 6 * n64);
        }
        for row in &self.rows {
            let (aligned, plain) = line(row);
            size.aligned = size.aligned.saturating_add(aligned);
            size.plain = size.plain.saturating_add(plain);
        }
        Some(size)
    }

    /// Таблица Markdown без дополнения: подписи (как у `htmd`), строка заголовка и разделитель
    /// `| --- |` на каждый столбец, строки данных — только свои ячейки. Пустые строки вокруг —
    /// как у `htmd`.
    fn plain(&self, handlers: &dyn Handlers, table: &Handle) -> String {
        let mut out = String::from("\n\n");
        for child in children(table) {
            if tag(&child) == Some("caption")
                && let Some(res) = handlers.handle(&child)
            {
                out.push_str(trim_document_whitespace(&res.content));
                out.push('\n');
            }
        }
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
            line(&mut out, &self.headers);
            out.push('|');
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
        assert_eq!(
            layout(
                "<table><thead></thead><tbody><tr><th>A</th><th>B</th></tr><tr><td>1</td></tr></tbody></table>"
            ),
            (names(&["thead", "tbody"]), v(&[&["td", "td"], &["td"]]))
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
                self.0.lock().expect("журнал зонда").push(Probed {
                    // "\n\n", подписи, строки таблицы, "\n".
                    predicted: 2 + captions + size.aligned + 1,
                    predicted_plain: 2 + captions + size.plain + 1,
                    aligned: res.content.clone(),
                    plain: grid.plain(handlers, node),
                });
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
    /// задачи 10), размер без дополнения — с [`Grid::plain`]. Падение после обновления `htmd` —
    /// повод сверить модуль с его обработчиком.
    #[test]
    fn aligned_size_matches_htmd() {
        for p in probe_all() {
            assert_eq!(p.predicted, p.aligned.len() as u64, "{}", p.aligned);
            assert_eq!(p.predicted_plain, p.plain.len() as u64, "{}", p.plain);
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
        }
    }

    /// Вывод страницы: таблица сверх бюджета — та же, что строит [`Grid::plain`], а при
    /// бюджете — та же, что строит `htmd`.
    #[test]
    fn layout_follows_budget() {
        let html = "<table><caption>Cap</caption><tr><th>head of the column</th></tr><tr><td>a</td></tr></table>";
        let aligned = render_with(&tree(html), TableLayout::new());
        let plain = render_with(&tree(html), TableLayout::with_budget(0));
        assert_eq!(
            aligned,
            "Cap\n| head of the column |\n| ------------------ |\n| a                  |"
        );
        assert_eq!(plain, "Cap\n| head of the column |\n| --- |\n| a |");
    }

    /// Бюджет — на страницу и тратится в порядке документа: при бюджете, равном надбавке
    /// одной таблицы, первая из двух одинаковых выровнена, вторая — нет.
    #[test]
    fn budget_is_spent_in_document_order() {
        let table = "<table><tr><th>head of the column</th></tr><tr><td>a</td></tr><tr><td>b</td></tr></table>";
        // Надбавка: две строки по 17 пробелов дополнения и разделитель на 15 тире длиннее `---`.
        let extra = 2 * 17 + 15;
        let html = format!("{table}<p>mid</p>{table}");
        let md = render_with(&tree(&html), TableLayout::with_budget(extra));
        let (first, second) = md.split_once("mid").expect("абзац между таблицами");
        assert!(first.contains(&format!("| a{} |", " ".repeat(17))), "{md}");
        assert!(
            second.contains("\n| a |\n") && second.contains("| --- |"),
            "{md}"
        );
        // На одну таблицу бюджета не хватает — обе без дополнения.
        let md = render_with(&tree(&html), TableLayout::with_budget(extra - 1));
        assert_eq!(md.matches("\n| a |\n").count(), 2, "{md}");
    }
}
