//! Онлайн-страж разбора HTML (R1-C2 D127, ревью задачи 7).
//!
//! Обёртка [`Guard`] над `RcDom` реализует `TreeSink` делегированием. Вход подаётся
//! разборщику кусками по [`CHUNK`]; после каждого куска страж проверяет пороги и при
//! превышении разбор прекращается ([`guarded_parse`]). Так квадратичные классы `html5ever`
//! (проверка области видимости при глубокой вложенности, разрастание дерева при
//! реконструкции активных элементов форматирования, «Noah's Ark») прерываются на первых
//! килобайтах входа, а не после полного разбора 2 МиБ.
//!
//! Счёт идёт по событиям самого разборщика: созданные элементы, глубина вставленного элемента,
//! сумма глубин и длина поисков узла среди детей родителя ([`MAX_SCAN`]). Глубина — настоящая глубина узла в дереве на момент вставки: подъём по
//! указателям `parent` самого `RcDom` (m1 повторного ревью fix-7). Поэтому всё равно, как
//! построитель вставил узел: обычным `append`, переносом из таблицы (foster parenting,
//! `append_before_sibling`, `append_based_on_parent_node`) или после пересадки детей
//! (`reparent_children`). Содержимое `template` — отдельный фрагмент без родителя; подъём
//! продолжается от самого шаблона ([`TreeSink::get_template_contents`] запоминает пару).
//! Своей модели построителя дерева у стража нет. Подъём стоит O(глубины) на вставку, сумма
//! глубин ограничена порогом [`MAX_SUM_DEPTH`].
//!
//! Перед `htmd` — один итеративный проход по готовому DOM ([`prepare_dom`]):
//! - точная проверка максимальной глубины: рекурсивный обход `htmd` переполняет стек;
//! - слияние соседних одинаковых строчных элементов ([`crate::merge`]). `htmd` сливает их
//!   сам квадратично по числу и длине текста, после прохода сливаемых пар нет;
//! - имена ячеек таблиц ([`crate::table`]): `htmd` теряет `td` строки заголовка и `th`
//!   остальных строк (карточки «ключ — значение» Википедии);
//! - верхняя оценка размера вывода `htmd` ([`crate::bound`]): префиксы строк цитат и списков
//!   под глубокой вложенностью растят вывод как строки × глубина (I-1 повторного ревью
//!   fix-10). Оценка больше [`HTML_OUTPUT_BUDGET`] — линейное извлечение.
//!
//! Пороги — по замеру рецензента (`.tmp/review7-bench.log`), с запасом на медленный CI; на
//! обычных страницах не срабатывают.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use html5ever::interface::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{Attribute, ExpandedName, ParseOpts, QualName, parse_document};
use markup5ever_rcdom::{Handle, Node, NodeData, RcDom};

use crate::bound::{self, Scope, Size};
use crate::html::HTML_OUTPUT_BUDGET;
use crate::{merge, table};

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
/// Предел суммарной длины поиска узла среди детей его родителя. `RcDom` ищет место вставки
/// и удаления линейным проходом (`get_parent_and_index`) при переносе из таблицы (foster
/// parenting) и при `remove_from_parent` (алгоритм усыновления). Поток элементов,
/// перенесённых из одной таблицы, стоит квадратично: 2 МиБ `<table><i></i>…` — ≈ 11 с в
/// release (замер прохода fix-9).
///
/// Единица счёта — сравнение указателей при поиске, ≈ 0,3 нс в release. Прежний порог 2·10^8
/// (≈ 70 мс поиска) давал ложное упрощение обычной кривой разметке: плоская страница из
/// 200 тыс. детей `body` и таблица с лишним текстом после 2000 строк — счётчик 4·10^8 при
/// полном разборе ≈ 130 мс (m1 ревью fix-9). 10^9 — около 0,3 с поиска: класс «поток
/// переноса» укладывается в цель ≈ 1 с и вместе с соседними классами на тех же 2 МиБ (замер
/// задачи 10 — в журнале).
pub const MAX_SCAN: u64 = 1_000_000_000;

/// Причина отказа стража: имя превышенной величины (для отчёта и тестов). Пустой `Ok`
/// означает, что разбор прошёл под порогами.
pub type GuardResult = Result<Handle, &'static str>;

/// Страж: делегирует всё `RcDom`, параллельно считая события разбора.
struct Guard {
    dom: RcDom,
    /// Фрагмент содержимого шаблона (по указателю) → сам элемент `template`: подъём от узла
    /// внутри шаблона продолжается от шаблона.
    templates: RefCell<HashMap<usize, Weak<Node>>>,
    created: Cell<usize>,
    sum_depth: Cell<u64>,
    /// Суммарная длина поисков среди детей родителя ([`MAX_SCAN`]).
    scan: Cell<u64>,
    /// Первая превышенная величина; дальше разбор можно прекращать.
    over: Cell<Option<&'static str>>,
}

fn key(h: &Handle) -> usize {
    Rc::as_ptr(h) as usize
}

impl Guard {
    /// Счёт после вставки элемента: его глубина в дереве и сумма глубин. После превышения
    /// порога глубины не считаются — разбор будет прерван после текущего куска.
    fn note_inserted(&self, node: Option<Handle>) {
        let Some(node) = node else {
            return;
        };
        if self.over.get().is_some() {
            return;
        }
        let depth = self.depth_of(&node);
        self.sum_depth.set(self.sum_depth.get() + u64::from(depth));
        if depth as usize > MAX_DEPTH {
            self.over.set(Some("depth"));
        } else if self.sum_depth.get() > MAX_SUM_DEPTH {
            self.over.set(Some("sum_depth"));
        }
    }

    /// Счёт перед вставкой рядом с `node` или его удалением: `RcDom` ищет `node` среди детей
    /// его родителя линейным проходом, цена — не больше числа этих детей.
    fn note_scan(&self, node: &Handle) {
        let parent = node.parent.take();
        let up = parent.as_ref().and_then(Weak::upgrade);
        node.parent.set(parent);
        let Some(parent) = up else {
            return;
        };
        let scan = self.scan.get() + parent.children.borrow().len() as u64;
        self.scan.set(scan);
        if self.over.get().is_none() && scan > MAX_SCAN {
            self.over.set(Some("scan"));
        }
    }

    /// Глубина элемента: он сам и его предки-элементы. Подъём по указателям `parent`;
    /// фрагмент содержимого `template` продолжается от шаблона. Подъём обрывается, когда
    /// глубина уже больше [`MAX_DEPTH`]: дальше точное число не нужно.
    fn depth_of(&self, node: &Handle) -> u32 {
        let mut depth = 0u32;
        let mut cur = Rc::clone(node);
        loop {
            if matches!(cur.data, NodeData::Element { .. }) {
                depth += 1;
                if depth as usize > MAX_DEPTH {
                    return depth;
                }
            }
            let parent = cur.parent.take();
            let up = parent.as_ref().and_then(Weak::upgrade);
            cur.parent.set(parent);
            let next = up.or_else(|| {
                self.templates
                    .borrow()
                    .get(&key(&cur))
                    .and_then(Weak::upgrade)
            });
            match next {
                Some(next) => cur = next,
                None => return depth,
            }
        }
    }
}

/// Вставляемый элемент (текст и комментарии глубину не считают).
fn element_of(child: &NodeOrText<Handle>) -> Option<Handle> {
    match child {
        NodeOrText::AppendNode(node) if matches!(node.data, NodeData::Element { .. }) => {
            Some(Rc::clone(node))
        }
        _ => None,
    }
}

// `TreeSink` делегируется `RcDom`; свои — точки счёта: `create_element`, три вида вставки и
// `get_template_contents`.
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
        let node = element_of(&child);
        self.dom.append(parent, child);
        self.note_inserted(node);
    }
    fn append_based_on_parent_node(
        &self,
        element: &Handle,
        prev: &Handle,
        child: NodeOrText<Handle>,
    ) {
        let node = element_of(&child);
        // `RcDom`: у таблицы есть родитель — вставка перед ней (поиск среди его детей).
        self.note_scan(element);
        self.dom.append_based_on_parent_node(element, prev, child);
        self.note_inserted(node);
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
        let contents = self.dom.get_template_contents(target);
        self.templates
            .borrow_mut()
            .insert(key(&contents), Rc::downgrade(target));
        contents
    }
    fn same_node(&self, x: &Handle, y: &Handle) -> bool {
        self.dom.same_node(x, y)
    }
    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.dom.set_quirks_mode(mode);
    }
    fn append_before_sibling(&self, sibling: &Handle, new_node: NodeOrText<Handle>) {
        let node = element_of(&new_node);
        self.note_scan(sibling);
        self.dom.append_before_sibling(sibling, new_node);
        self.note_inserted(node);
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
        self.note_scan(target);
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
        templates: RefCell::new(HashMap::new()),
        created: Cell::new(0),
        sum_depth: Cell::new(0),
        scan: Cell::new(0),
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

/// Шаг итеративного обхода [`prepare_dom`]: вход в узел (глубина, наследуемое от предков) или
/// выход из него (оценка по сумме детей).
enum Step {
    Enter(Handle, usize, Scope),
    Exit(Handle, Scope),
}

/// Подготовка готового DOM к `htmd`, один итеративный проход (стек не переполняется), всё за
/// линейное время:
/// - точная проверка максимальной глубины;
/// - имена ячеек таблиц ([`table::normalize_table`]) и слияние соседних одинаковых строчных
///   элементов ([`merge::merge_children`]) — при входе в узел, до обхода его детей;
/// - верхняя оценка размера вывода `htmd` ([`crate::bound`]) — при выходе, по сумме оценок
///   детей.
///
/// `Ok(bound)` — можно звать `htmd`: сливаемых пар в дереве нет, ячейки таблиц `htmd` не
/// потеряет, вывод без дополнения таблиц не больше `bound` и `bound` не больше
/// [`HTML_OUTPUT_BUDGET`]. `Err(reason)` — нужно линейное извлечение: `dom_depth` или
/// `output_bound` (оценка вывода больше бюджета). 1–50 мс на 2 МиБ.
pub fn prepare_dom(root: &Handle) -> Result<u64, &'static str> {
    let mut steps = vec![Step::Enter(Rc::clone(root), 0, Scope::default())];
    // Суммы оценок детей у открытых узлов; дно — сумма для корня.
    let mut sums = vec![Size::default()];
    while let Some(step) = steps.pop() {
        match step {
            Step::Enter(node, depth, scope) => {
                if depth > MAX_DEPTH {
                    return Err("dom_depth");
                }
                if let NodeData::Text { contents } = &node.data {
                    if let Some(sum) = sums.last_mut() {
                        sum.add(bound::text(&contents.borrow(), scope));
                    }
                    continue;
                }
                // До обхода детей: ячейки строк таблицы заменяются новыми узлами.
                table::normalize_table(&node);
                merge::merge_children(&node);
                let inner = bound::scope_of_children(&node, scope);
                steps.push(Step::Exit(Rc::clone(&node), scope));
                sums.push(Size::default());
                for child in node.children.borrow().iter().rev() {
                    steps.push(Step::Enter(Rc::clone(child), depth + 1, inner));
                }
            }
            Step::Exit(node, scope) => {
                let content = sums.pop().unwrap_or_default();
                let size = bound::element(&node, scope, content);
                if let Some(sum) = sums.last_mut() {
                    sum.add(size);
                }
            }
        }
    }
    let bound = sums.pop().unwrap_or_default().bytes;
    if bound > HTML_OUTPUT_BUDGET as u64 {
        return Err("output_bound");
    }
    Ok(bound)
}
