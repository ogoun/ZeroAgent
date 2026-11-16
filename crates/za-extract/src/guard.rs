//! Онлайн-страж разбора HTML (R1-C2 D127, ревью задачи 7).
//!
//! Обёртка [`Guard`] над `RcDom` реализует `TreeSink` делегированием. Вход подаётся
//! разборщику кусками по [`CHUNK`]; после каждого куска страж проверяет пороги и при
//! превышении разбор прекращается ([`guarded_parse`]). Так квадратичные классы `html5ever`
//! (проверка области видимости при глубокой вложенности, разрастание дерева при
//! реконструкции активных элементов форматирования, «Noah's Ark») прерываются на первых
//! килобайтах входа, а не после полного разбора 2 МиБ.
//!
//! Счёт идёт по событиям самого разборщика — созданные элементы, глубина вставки и её
//! сумма, — поэтому модели построителя дерева нет и расходиться не с чем (в отличие от
//! текстовой предпроверки, которую обходит `<div></span>` или реконструкция, M2 ревью).
//!
//! Перед `htmd` — точная проверка готового DOM ([`dom_check`]): максимальная глубина
//! (рекурсивный обход `htmd` переполняет стек) и цена слияния соседних одинаковых строчных
//! элементов (`htmd` удаляет их `Vec::remove` в цикле — квадратично по числу детей
//! родителя). Пороги — по замеру рецензента (`.tmp/review7-bench.log`), с запасом на
//! медленный CI; на обычных страницах не срабатывают.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use html5ever::interface::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{Attribute, ExpandedName, ParseOpts, QualName, parse_document};
use markup5ever_rcdom::{Handle, Node, NodeData, RcDom};

/// Куски входа для разборщика: порог проверяется после каждого (ревью задачи 7).
const CHUNK: usize = 16 * 1024;

/// Предельная глубина вставки/DOM. Стек `htmd` в release — 1–2 КиБ на уровень; подпроцесс
/// извлечения запускается с большим стеком (задача 10), на 256 уровнях — двукратный запас.
pub const MAX_DEPTH: usize = 256;
/// Предел суммы глубин вставок: цена проверки области видимости `html5ever` (разбор ≈ 0,3 с
/// при 1,7·10^7, замер рецензента).
pub const MAX_SUM_DEPTH: u64 = 20_000_000;
/// Предел числа созданных элементов: память дерева ≈ 100–200 МБ при 600 тыс.
pub const MAX_CREATED: usize = 600_000;
/// Предел цены слияния соседей `htmd` (Σ по родителям числа сливаемых пар на число детей):
/// при 2·10^9 слияние ≈ 0,25 с.
pub const MAX_MERGE: u128 = 2_000_000_000;

/// Причина отказа стража: имя превышенной величины (для отчёта и тестов). Пустой `Ok`
/// означает, что разбор прошёл под порогами.
pub type GuardResult = Result<Handle, &'static str>;

/// Страж: делегирует всё `RcDom`, параллельно считая события разбора.
struct Guard {
    dom: RcDom,
    /// Глубина вставки по указателю узла (глубина родителя + 1).
    depth: RefCell<HashMap<usize, u32>>,
    created: Cell<usize>,
    sum_depth: Cell<u64>,
    /// Первая превышенная величина; дальше разбор можно прекращать.
    over: Cell<Option<&'static str>>,
}

fn key(h: &Handle) -> usize {
    Rc::as_ptr(h) as usize
}

impl Guard {
    fn note_append(&self, parent: &Handle, child: &NodeOrText<Handle>) {
        let NodeOrText::AppendNode(node) = child else {
            return;
        };
        if !matches!(node.data, NodeData::Element { .. }) {
            return;
        }
        let parent_depth = self.depth.borrow().get(&key(parent)).copied().unwrap_or(0);
        let depth = parent_depth + 1;
        self.depth.borrow_mut().insert(key(node), depth);
        self.sum_depth.set(self.sum_depth.get() + u64::from(depth));
        if self.over.get().is_some() {
            return;
        }
        if depth as usize > MAX_DEPTH {
            self.over.set(Some("depth"));
        } else if self.sum_depth.get() > MAX_SUM_DEPTH {
            self.over.set(Some("sum_depth"));
        }
    }
}

// `TreeSink` делегируется `RcDom`; свои только `create_element` и `append` — точки счёта.
impl TreeSink for Guard {
    type Handle = Handle;
    type Output = Self;
    type ElemName<'a>
        = ExpandedName<'a>
    where
        Self: 'a;

    fn finish(self) -> Self {
        self
    }
    fn parse_error(&self, msg: Cow<'static, str>) {
        self.dom.parse_error(msg);
    }
    fn get_document(&self) -> Handle {
        self.dom.get_document()
    }
    fn elem_name<'a>(&'a self, target: &'a Handle) -> ExpandedName<'a> {
        self.dom.elem_name(target)
    }
    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> Handle {
        let created = self.created.get() + 1;
        self.created.set(created);
        if self.over.get().is_none() && created > MAX_CREATED {
            self.over.set(Some("created"));
        }
        self.dom.create_element(name, attrs, flags)
    }
    fn create_comment(&self, text: StrTendril) -> Handle {
        self.dom.create_comment(text)
    }
    fn create_pi(&self, target: StrTendril, data: StrTendril) -> Handle {
        self.dom.create_pi(target, data)
    }
    fn append(&self, parent: &Handle, child: NodeOrText<Handle>) {
        self.note_append(parent, &child);
        self.dom.append(parent, child);
    }
    fn append_based_on_parent_node(
        &self,
        element: &Handle,
        prev: &Handle,
        child: NodeOrText<Handle>,
    ) {
        self.dom.append_based_on_parent_node(element, prev, child);
    }
    fn append_doctype_to_document(&self, name: StrTendril, public: StrTendril, system: StrTendril) {
        self.dom.append_doctype_to_document(name, public, system);
    }
    fn mark_script_already_started(&self, node: &Handle) {
        self.dom.mark_script_already_started(node);
    }
    fn pop(&self, node: &Handle) {
        self.dom.pop(node);
    }
    fn get_template_contents(&self, target: &Handle) -> Handle {
        self.dom.get_template_contents(target)
    }
    fn same_node(&self, x: &Handle, y: &Handle) -> bool {
        self.dom.same_node(x, y)
    }
    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.dom.set_quirks_mode(mode);
    }
    fn append_before_sibling(&self, sibling: &Handle, new_node: NodeOrText<Handle>) {
        self.dom.append_before_sibling(sibling, new_node);
    }
    fn add_attrs_if_missing(&self, target: &Handle, attrs: Vec<Attribute>) {
        self.dom.add_attrs_if_missing(target, attrs);
    }
    fn associate_with_form(
        &self,
        target: &Handle,
        form: &Handle,
        nodes: (&Handle, Option<&Handle>),
    ) {
        self.dom.associate_with_form(target, form, nodes);
    }
    fn remove_from_parent(&self, target: &Handle) {
        self.dom.remove_from_parent(target);
    }
    fn reparent_children(&self, node: &Handle, new_parent: &Handle) {
        self.dom.reparent_children(node, new_parent);
    }
    fn is_mathml_annotation_xml_integration_point(&self, handle: &Handle) -> bool {
        self.dom.is_mathml_annotation_xml_integration_point(handle)
    }
    fn set_current_line(&self, line: u64) {
        self.dom.set_current_line(line);
    }
    fn allow_declarative_shadow_roots(&self, intended_parent: &Handle) -> bool {
        self.dom.allow_declarative_shadow_roots(intended_parent)
    }
    fn attach_declarative_shadow(
        &self,
        location: &Handle,
        template: &Handle,
        attrs: &[Attribute],
    ) -> bool {
        self.dom
            .attach_declarative_shadow(location, template, attrs)
    }
    fn maybe_clone_an_option_into_selectedcontent(&self, option: &Handle) {
        self.dom.maybe_clone_an_option_into_selectedcontent(option);
    }
}

/// Разбор под стражем. `Ok(document)` — дерево в пределах порогов (глубина, сумма глубин,
/// число элементов); `Err(reason)` — порог превышен, дальше нужно линейное извлечение.
/// Вход подаётся кусками по [`CHUNK`]; `scripting_enabled` — как у `htmd` по умолчанию.
pub fn guarded_parse(html: &str) -> GuardResult {
    let guard = Guard {
        dom: RcDom::default(),
        depth: RefCell::new(HashMap::new()),
        created: Cell::new(0),
        sum_depth: Cell::new(0),
        over: Cell::new(None),
    };
    let mut parser = parse_document(
        guard,
        ParseOpts {
            tree_builder: TreeBuilderOpts {
                scripting_enabled: true,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let mut pos = 0;
    while pos < html.len() {
        let mut end = html.floor_char_boundary((pos + CHUNK).min(html.len()));
        if end == pos {
            end = html.ceil_char_boundary(pos + 1);
        }
        parser.process(StrTendril::from_slice(&html[pos..end]));
        pos = end;
        if let Some(reason) = parser.tokenizer.sink.sink.over.get() {
            return Err(reason);
        }
    }
    let guard = parser.finish();
    match guard.over.get() {
        Some(reason) => Err(reason),
        None => Ok(guard.dom.document.clone()),
    }
}

/// Точная проверка готового DOM до `htmd`: максимальная глубина и цена слияния соседей.
/// `Ok(())` — можно звать `htmd`; `Err(reason)` — нужно линейное извлечение. Обход
/// итеративный (стек не переполняется), 1–50 мс на 2 МиБ.
pub fn dom_check(root: &Handle) -> Result<(), &'static str> {
    let mut stack = vec![(Rc::clone(root), 0usize)];
    let mut merge = 0u128;
    while let Some((node, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            return Err("dom_depth");
        }
        let children = node.children.borrow();
        if children.len() > 1 {
            let pairs = children
                .windows(2)
                .filter(|w| combinable(&w[0], &w[1]))
                .count() as u128;
            merge = merge.saturating_add(pairs * children.len() as u128);
            if merge > MAX_MERGE {
                return Err("merge");
            }
        }
        for child in children.iter() {
            stack.push((Rc::clone(child), depth + 1));
        }
    }
    Ok(())
}

/// Два соседних узла, которые `htmd` 0.5.6 сольёт (`can_combine`, `src/dom_walker.rs`):
/// оба — не блочные строчные элементы с одним текстовым ребёнком, одинаковым именем (с
/// учётом пар `i`/`em`, `b`/`strong`) и атрибутами, не `a`. Блочные (`is_block`) `htmd`
/// не сливает, поэтому страницы с множеством `<li>`/`<p>` цены слияния не набирают.
fn combinable(a: &Node, b: &Node) -> bool {
    let (
        NodeData::Element {
            name: n1,
            attrs: a1,
            ..
        },
        NodeData::Element {
            name: n2,
            attrs: a2,
            ..
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
    same_name && *a1.borrow() == *a2.borrow() && one_text_child(a) && one_text_child(b)
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
