//! Слияние соседних одинаковых строчных элементов до `htmd` (R1-C2 D127, I1 повторного ревью
//! прохода fix-7).
//!
//! `htmd` 0.5.6 сливает соседей сам (`walk_children`, `src/dom_walker.rs`): `<b>a</b><b>b</b>`
//! становится одним `<b>ab</b>`. Каждое слияние там стоит дорого:
//! - `Vec::remove` сдвигает хвост списка детей;
//! - `contents.clone().into_inner()` и `push_tendril` копируют весь уже накопленный текст
//!   (`tendril` 0.5.1: `make_owned` копирует общий буфер).
//!
//! Серия из r элементов с текстом длины L стоит ≈ r²/2 сдвигов и ≈ L·r²/2 байт копирования:
//! при L = 40 на 2 МиБ — 13 с в release. Порогами это не лечится: цена зависит от длины текста.
//!
//! [`merge_children`] делает то же слияние за один проход по детям узла. Текст серии собирается
//! в одну строку и кладётся в первый элемент серии, список детей пересобирается один раз.
//! [`crate::guard::prepare_dom`] вызывает его для каждого узла в том же проходе, что проверка
//! глубины. После этого `htmd` сливаемых пар не находит: соседний элемент не сливается с
//! головой серии по построению. Результат преобразования тот же — это проверяют модульные
//! тесты на парах «с предварительным слиянием» и «без него».
//!
//! **Правило** — копия `can_combine` `htmd` 0.5.6 ([`combinable`]), при обновлении `htmd` сверить.
//! `htmd` сравнивает следующий элемент с головой серии, у которой после слияния прежние имя,
//! атрибуты и один текстовый ребёнок. Здесь так же.
//!
//! **Где сливает `htmd`.** Только у детей узла, которые обходит `walk_children`. Дети `<ol>`
//! обходятся напрямую (`get_ol_content`, `src/element_handler/list.rs`) и не сливаются,
//! поэтому дети `ol` здесь не трогаются. Таблица с явными заголовками тоже читает строки и
//! ячейки напрямую. Но дети `table`, `thead`, `tbody`, `tfoot`, `tr` — блочные элементы или
//! перенесённые построителем наружу, сливаемых пар среди них нет. Внутри ячеек `htmd` снова
//! идёт через `walk_children`. Слияние внутри пропускаемых поддеревьев ничего не меняет:
//! их содержимое в текст не попадает.

use std::rc::Rc;

use html5ever::tendril::StrTendril;
use markup5ever_rcdom::{Handle, Node, NodeData};

/// Слить серии соседних сливаемых детей узла (как `htmd` `walk_children`) за один проход:
/// O(число детей + длина их текста). Узел `ol` не трогается (у `htmd` его дети не сливаются).
pub(crate) fn merge_children(node: &Handle) {
    if let NodeData::Element { name, .. } = &node.data
        && *name.local == *"ol"
    {
        return;
    }
    let mut children = node.children.borrow_mut();
    // Обычный узел без сливаемых пар — без пересборки списка.
    if !children.windows(2).any(|w| combinable(&w[0], &w[1])) {
        return;
    }
    let old = std::mem::take(&mut *children);
    let mut kept: Vec<Handle> = Vec::with_capacity(old.len());
    // Текст текущей головы серии, если с ней уже что-то слито.
    let mut run: Option<String> = None;
    for child in old {
        if let Some(head) = kept.last()
            && combinable(head, &child)
        {
            let buf = run.get_or_insert_with(|| text_of(head));
            buf.push_str(&text_of(&child));
            // Слитый узел выбывает из дерева, как у `htmd` после `Vec::remove`.
            child.parent.set(None);
            continue;
        }
        if let (Some(text), Some(head)) = (run.take(), kept.last()) {
            set_text(head, text);
        }
        kept.push(child);
    }
    if let (Some(text), Some(head)) = (run.take(), kept.last()) {
        set_text(head, text);
    }
    *children = kept;
}

/// Текст единственного текстового ребёнка элемента (у сливаемых он есть, [`combinable`]).
fn text_of(element: &Node) -> String {
    let children = element.children.borrow();
    match children.first().map(|c| &c.data) {
        Some(NodeData::Text { contents }) => contents.borrow().to_string(),
        _ => String::new(),
    }
}

fn set_text(element: &Rc<Node>, text: String) {
    if let Some(NodeData::Text { contents }) = element.children.borrow().first().map(|c| &c.data) {
        *contents.borrow_mut() = StrTendril::from(text);
    }
}

/// Два соседних узла, которые `htmd` 0.5.6 сливает (`can_combine`, `src/dom_walker.rs`):
/// оба — элементы, первый не блочный ([`is_block`]) и не `a`, у обоих по одному текстовому
/// ребёнку, имена одинаковы (с учётом пар `i`/`em`, `b`/`strong`), атрибуты равны, нет
/// содержимого шаблона, признак точки интеграции MathML одинаков.
pub(crate) fn combinable(a: &Node, b: &Node) -> bool {
    let (
        NodeData::Element {
            name: n1,
            attrs: a1,
            template_contents: t1,
            mathml_annotation_xml_integration_point: m1,
        },
        NodeData::Element {
            name: n2,
            attrs: a2,
            template_contents: t2,
            mathml_annotation_xml_integration_point: m2,
        },
    ) = (&a.data, &b.data)
    else {
        return false;
    };
    if is_block(&n1.local) || *n1.local == *"a" {
        return false;
    }
    let same_name = n1 == n2
        || (*n1.local == *"i" && *n2.local == *"em")
        || (*n1.local == *"em" && *n2.local == *"i")
        || (*n1.local == *"b" && *n2.local == *"strong")
        || (*n1.local == *"strong" && *n2.local == *"b");
    same_name
        && one_text_child(a)
        && one_text_child(b)
        && t1.borrow().is_none()
        && t2.borrow().is_none()
        && *a1.borrow() == *a2.borrow()
        && m1 == m2
}

fn one_text_child(node: &Node) -> bool {
    let children = node.children.borrow();
    children.len() == 1 && matches!(children[0].data, NodeData::Text { .. })
}

/// Блочные элементы CommonMark — копия `BLOCK_ELEMENTS` из `htmd` 0.5.6
/// (`src/dom_walker.rs`); при обновлении `htmd` сверить.
fn is_block(tag: &str) -> bool {
    const BLOCK: &[&str] = &[
        "address",
        "article",
        "aside",
        "base",
        "basefont",
        "blockquote",
        "body",
        "caption",
        "center",
        "col",
        "colgroup",
        "dd",
        "details",
        "dialog",
        "dir",
        "div",
        "dl",
        "dt",
        "fieldset",
        "figcaption",
        "figure",
        "footer",
        "form",
        "frame",
        "frameset",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "head",
        "header",
        "hr",
        "html",
        "iframe",
        "legend",
        "li",
        "link",
        "main",
        "menu",
        "menuitem",
        "nav",
        "noframes",
        "ol",
        "optgroup",
        "option",
        "p",
        "param",
        "pre",
        "script",
        "search",
        "section",
        "style",
        "summary",
        "table",
        "tbody",
        "td",
        "textarea",
        "tfoot",
        "th",
        "thead",
        "title",
        "tr",
        "track",
        "ul",
    ];
    BLOCK.contains(&tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::guarded_parse;
    use crate::html::render;

    /// Все узлы дерева (явным стеком).
    fn each_node(root: &Handle, f: &mut impl FnMut(&Handle)) {
        let mut stack = vec![Rc::clone(root)];
        while let Some(node) = stack.pop() {
            f(&node);
            stack.extend(node.children.borrow().iter().cloned());
        }
    }

    /// Markdown полного пути: с предварительным слиянием или без него (тогда сливает `htmd`).
    fn markdown(html: &str, premerge: bool) -> String {
        let tree = guarded_parse(html).expect("разбор под порогами");
        if premerge {
            each_node(&tree, &mut |n| merge_children(n));
        }
        render(&tree)
    }

    const CASES: &[&str] = &[
        "<p><b>a</b><b>b</b><b>c</b></p>",
        "<p><b>a</b><strong>b</strong><b>c</b><strong>d</strong></p>",
        "<p><strong>a</strong><b>b</b> <i>c</i><em>d</em><i>e</i></p>",
        "<p><em>x</em><i>y</i><em>z</em></p>",
        "<p><b class=x>a</b><b class=y>b</b><b class=x>c</b><b class=x>d</b></p>",
        "<p><a href=/1>a</a><a href=/1>b</a></p>",
        "<p><span>a</span><span>b</span><code>c</code><code>d</code></p>",
        "<p><b><i>a</i><i>b</i></b><b>c</b><b>d</b></p>",
        "<ol><b>a</b><b>b</b><li>x</li><b>c</b><b>d</b><li><b>e</b><b>f</b></li></ol>",
        "<ul><b>a</b><b>b</b><li>x</li><li><i>y</i><i>z</i></li></ul>",
        "<table><tr><th><b>h</b><b>i</b></th></tr><tr><td><i>a</i><i>b</i></td></tr></table>",
        "<table><tr><td><b>a</b><b>b</b></td><td>c</td></tr></table>",
        "<pre><code>a</code><code>b</code></pre>",
        "<p><b>*a</b><b>_b</b><b>[c]</b><b>`d`</b></p>",
        "<p><b> a </b><b> b </b><b>\nc\n</b></p>",
        "<p><b>a</b> <b>b</b><b>c</b>tail<b>d</b></p>",
        "<div><b>a</b><b>b</b><i>c</i><i>d</i><b>e</b><b>f</b></div>",
        "<h1><b>a</b><b>b</b></h1><blockquote><i>q</i><i>r</i></blockquote>",
        "<p><b>a</b><b>b</b><script>x</script><b>c</b></p>",
        "<p><sup>1</sup><sup>2</sup><small>a</small><small>b</small><u>c</u><u>d</u></p>",
        "<p><template>x</template><template>y</template><b><!--c-->a</b><b>b</b></p>",
        "<p><b>a</b><b><i>b</i></b><b>c</b></p>",
        "<p><span class=k>a</span><span class=k>b</span><span>c</span></p>",
        "<p><svg><title>t</title></svg><b>a</b><b>b</b></p>",
        "<ul><li><a href=/x><b>a</b><b>b</b></a></li></ul>",
    ];

    /// Предварительное слияние не меняет итог `htmd` (I1 повторного ревью fix-7).
    #[test]
    fn premerge_keeps_htmd_output() {
        for html in CASES {
            assert_eq!(markdown(html, true), markdown(html, false), "{html}");
        }
        // Длинная серия и несколько серий подряд.
        let long = format!(
            "<p>{}{}</p>",
            "<b>xy</b>".repeat(300),
            "<i>q</i><em>r</em>".repeat(200)
        );
        assert_eq!(markdown(&long, true), markdown(&long, false));
    }

    /// После предварительного слияния сливаемых пар не остаётся нигде, кроме детей `ol`.
    #[test]
    fn premerge_leaves_no_pairs() {
        let html = "<div><b>a</b><b>b</b><strong>c</strong><i>d</i><em>e</em>\
                    <span>f</span><span>g</span><p><u>h</u><u>i</u></p></div>";
        let tree = guarded_parse(html).expect("разбор");
        each_node(&tree, &mut |n| merge_children(n));
        each_node(&tree, &mut |n| {
            let children = n.children.borrow();
            assert!(
                !children.windows(2).any(|w| combinable(&w[0], &w[1])),
                "остались сливаемые соседи"
            );
        });
        let md = render(&tree);
        assert_eq!(md, markdown(html, false));
        assert!(md.contains("**abc**"), "{md}");
    }
}
