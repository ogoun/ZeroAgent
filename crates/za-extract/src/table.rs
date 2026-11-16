//! Ячейки таблиц до `htmd` (R1-C2, перенос прохода fix-9 в задачу 10).
//!
//! `htmd` 0.5.6 (`src/element_handler/table.rs`) читает таблицу с явными заголовками
//! напрямую по строкам и теряет ячейки:
//! - строка заголовка — первая строка `thead`, а без него первая строка тела с `th`; из неё
//!   берутся только `th`, её `td` пропадают;
//! - из остальных строк берутся только `td`, их `th` пропадают (строка из одних `th` —
//!   целиком).
//!
//! Так пропадают ключи карточек Википедии (`<tr><th>Paradigms</th><td>…</td></tr>`), строки
//! подзаголовков (`<tr><th colspan=2>…</th></tr>`) и пустая угловая ячейка матрицы, после
//! чего столбцы заголовка сдвигаются.
//!
//! [`normalize_table`] меняет имена ячеек так, чтобы `htmd` ни одну не потерял:
//! - первая строка таблицы, если она в `thead`, — заголовок: её `td` становятся `th`;
//! - первая строка таблицы вне `thead` из одних `th` — заголовок, как у `htmd`; смешанная —
//!   строка данных: её `th` становятся `td`;
//! - во всех прочих строках `th` становятся `td`.
//!
//! Текст и порядок ячеек не меняются. Таблица, где после замены нет ни `th`, ни `thead`,
//! у `htmd` идёт обычным обходом детей (режим `Pure`) — текст ячеек сохраняется и там.
//! Вложенные таблицы — отдельные элементы `table`, их обрабатывает свой вызов.
//!
//! Цена — один проход по строкам и ячейкам таблицы. При обновлении `htmd` правило сверить с
//! его обработчиком таблиц.

use std::cell::RefCell;
use std::rc::Rc;

use html5ever::{LocalName, QualName, ns};
use markup5ever_rcdom::{Handle, Node, NodeData};

/// Элемент HTML с именем `local` (иностранное содержимое SVG и MathML не трогается).
fn is_html(node: &Node, local: &str) -> bool {
    matches!(&node.data, NodeData::Element { name, .. } if name.ns == ns!(html) && *name.local == *local)
}

/// Привести ячейки строк таблицы `table` к виду, в котором `htmd` их не теряет (см.
/// документацию модуля). Узел не `table` — ничего.
pub(crate) fn normalize_table(table: &Handle) {
    if !is_html(table, "table") {
        return;
    }
    let mut first = true;
    for section in table.children.borrow().iter() {
        let in_thead = is_html(section, "thead");
        let rows: Vec<Handle> = if is_html(section, "tr") {
            vec![Rc::clone(section)]
        } else if in_thead || is_html(section, "tbody") || is_html(section, "tfoot") {
            section
                .children
                .borrow()
                .iter()
                .filter(|r| is_html(r, "tr"))
                .cloned()
                .collect()
        } else {
            continue;
        };
        for row in rows {
            let (th, td) = cell_counts(&row);
            if first && in_thead {
                rename_cells(&row, "td", "th");
            } else if !(first && td == 0 && th > 0) {
                rename_cells(&row, "th", "td");
            }
            first = false;
        }
    }
}

/// Число ячеек `th` и `td` среди детей строки.
fn cell_counts(row: &Node) -> (usize, usize) {
    let children = row.children.borrow();
    let th = children.iter().filter(|c| is_html(c, "th")).count();
    let td = children.iter().filter(|c| is_html(c, "td")).count();
    (th, td)
}

/// Заменить ячейки `from` среди детей строки на `to`: новый узел с теми же атрибутами и
/// детьми на том же месте (имя элемента в `RcDom` не меняется на месте).
fn rename_cells(row: &Handle, from: &str, to: &str) {
    let mut children = row.children.borrow_mut();
    for slot in children.iter_mut() {
        if !is_html(slot, from) {
            continue;
        }
        let NodeData::Element {
            name,
            attrs,
            mathml_annotation_xml_integration_point,
            ..
        } = &slot.data
        else {
            continue;
        };
        let cell = Node::new(NodeData::Element {
            name: QualName::new(name.prefix.clone(), name.ns.clone(), LocalName::from(to)),
            attrs: RefCell::new(std::mem::take(&mut *attrs.borrow_mut())),
            template_contents: RefCell::new(None),
            mathml_annotation_xml_integration_point: *mathml_annotation_xml_integration_point,
        });
        let kids = std::mem::take(&mut *slot.children.borrow_mut());
        for kid in &kids {
            kid.parent.set(Some(Rc::downgrade(&cell)));
        }
        *cell.children.borrow_mut() = kids;
        cell.parent.set(Some(Rc::downgrade(row)));
        slot.parent.set(None);
        *slot = cell;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use html5ever::parse_document;
    use html5ever::tendril::TendrilSink;
    use markup5ever_rcdom::RcDom;

    /// Имена ячеек строк первой таблицы документа, по строкам.
    fn cells(html: &str) -> Vec<Vec<String>> {
        let dom = parse_document(RcDom::default(), Default::default()).one(html);
        let mut stack = vec![dom.document.clone()];
        while let Some(n) = stack.pop() {
            if is_html(&n, "table") {
                normalize_table(&n);
                let mut out = Vec::new();
                for s in n.children.borrow().iter() {
                    for r in s.children.borrow().iter().filter(|r| is_html(r, "tr")) {
                        let names = r
                            .children
                            .borrow()
                            .iter()
                            .filter_map(|c| match &c.data {
                                NodeData::Element { name, .. } => Some(name.local.to_string()),
                                _ => None,
                            })
                            .collect();
                        // Родитель детей ячейки — новая ячейка.
                        for c in r.children.borrow().iter() {
                            for k in c.children.borrow().iter() {
                                let p = k.parent.take();
                                let up = p.as_ref().and_then(std::rc::Weak::upgrade);
                                k.parent.set(p);
                                assert!(up.is_some_and(|u| Rc::ptr_eq(&u, c)));
                            }
                        }
                        out.push(names);
                    }
                }
                return out;
            }
            stack.extend(n.children.borrow().iter().cloned());
        }
        Vec::new()
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
}
