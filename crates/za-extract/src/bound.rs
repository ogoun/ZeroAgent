//! Верхняя оценка размера вывода `htmd` по всему DOM (R1-C2, I-1 повторного ревью fix-10).
//!
//! `htmd` 0.5.6 строит Markdown снизу вверх: обработчик элемента получает готовый текст детей
//! и дописывает к нему своё. Большинство обработчиков добавляют постоянную обёртку (`\n\n`,
//! `**`, `# `), но три класса растят вывод не линейно по входу:
//! - префиксы строк: цитата ставит `> ` перед каждой строкой содержимого
//!   (`element_handler/blockquote.rs`), пункт списка — отступ шириной маркера
//!   (`li.rs`, `list.rs`, `text_util::indent_text_except_first_line`). Под глубокой вложенностью
//!   строка `pre` получает префикс каждого уровня: 2 МиБ строк под 120 уровнями — сотни МБ.
//!   Время тоже растёт как строки × глубина, даже когда пустые строки отступа не получают;
//! - выравнивание таблиц — его держит бюджет надбавки ([`crate::table::TableLayout`]);
//! - разделители кода: длина ограды и обратных кавычек строчного кода зависит от серий `` ` ``
//!   внутри (`code.rs`).
//!
//! [`Size`] — консервативная верхняя граница вывода поддерева: байты, переводы строк и
//! обратные кавычки. Её считает тот же итеративный проход, что готовит DOM
//! ([`crate::guard::prepare_dom`]): узел получает [`Scope`] от родителя при входе, а свою
//! оценку — по сумме оценок детей при выходе. Правила повторяют обработчики `htmd` и
//! обработчики `html.rs` (пропуск [`SKIPPED_TAGS`], картинки `data:`):
//! - текст вне `pre` и `code`: каждый байт не больше двух (экранирование `\`, `*`, `_`,
//!   `` ` ``, `[`, `]`, `<`), плюс два байта на узел (`\` в начале, `\.` после номера); в `pre`
//!   и `code` текст как есть, плюс `\` перед оградой. `|` в ячейке таблицы — шесть байт
//!   (`&#124;`). Переводы строк считаются в **любом** текстовом узле: обычный текст
//!   `compress_whitespace` схлопывает, но обработчик может вывести его как есть — формула
//!   `<span class="math …">` (I-1 повторного ревью fix-10b). Общее правило, а не условие для
//!   одного обработчика: оценка остаётся верхней при любом элементе;
//! - атрибуты, которые попадают в вывод (`href`, `title` ссылки; `src`, `href`, `alt`, `title`
//!   картинки): каждый байт не больше двух (экранирование скобок и кавычек), их переводы строк
//!   считаются;
//! - префикс строк: цитата — 2 байта, пункт `ul` — 4 (`*` и три пробела), пункт `ol` —
//!   `3 + digits(highest)` и единица запаса (`htmd` считает цифры через `f32`), на **каждую**
//!   строку содержимого, включая первую и пустые: так оценка покрывает и время обхода строк
//!   на каждом уровне;
//! - код: ограда и обратные кавычки — не длиннее числа `` ` `` в содержимом плюс постоянная;
//! - таблица: большее из обхода детей и строк Markdown без дополнения (3 байта на ячейку и
//!   строку, 9 на столбец — разделитель и недостающие ячейки заголовка). Дополнение пробелами
//!   сюда не входит: его ограничивает бюджет надбавки, который вызывающий берёт из остатка
//!   ([`crate::html::html_to_markdown`]);
//! - прочие элементы с обработчиком-обёрткой — 4 байта и 4 перевода строки; элементы без
//!   обработчика `htmd` — только дети.
//!
//! Суммы по узлам насыщающие. Правила повторяют `htmd` 0.5.6; при его обновлении их нужно
//! сверить, расхождение ловят модульные тесты `bound_covers_*`.

use std::rc::Rc;

use markup5ever_rcdom::{Handle, Node, NodeData};

use crate::html::SKIPPED_TAGS;

/// Верхняя оценка вывода `htmd` для поддерева.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Size {
    /// Байты вывода.
    pub(crate) bytes: u64,
    /// Переводы строк вывода: строк — на одну больше.
    pub(crate) newlines: u64,
    /// Обратные кавычки вывода: длина самой длинной серии не больше их числа.
    pub(crate) ticks: u64,
}

impl Size {
    /// Сумма оценок соседних поддеревьев: вывод родителя — их склейка.
    pub(crate) fn add(&mut self, other: Size) {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.newlines = self.newlines.saturating_add(other.newlines);
        self.ticks = self.ticks.saturating_add(other.ticks);
    }

    /// Обёртка: `bytes` байт и `newlines` переводов строк сверх содержимого.
    fn wrap(self, bytes: u64, newlines: u64) -> Size {
        Size {
            bytes: self.bytes.saturating_add(bytes),
            newlines: self.newlines.saturating_add(newlines),
            ticks: self.ticks,
        }
    }

    /// Префикс шириной `width` на каждой строке содержимого.
    fn prefixed(self, width: u64) -> Size {
        let lines = self.newlines.saturating_add(1);
        Size {
            bytes: self.bytes.saturating_add(width.saturating_mul(lines)),
            ..self
        }
    }

    /// Разделитель кода с обеих сторон: не длиннее `ticks + extra` обратных кавычек каждый.
    fn fenced(self, extra: u64) -> Size {
        let fence = self.ticks.saturating_add(extra).saturating_mul(2);
        Size {
            bytes: self.bytes.saturating_add(fence),
            newlines: self.newlines,
            ticks: self.ticks.saturating_add(fence),
        }
    }
}

/// Что узел наследует от предков.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Scope {
    /// Предок из [`SKIPPED_TAGS`] (или картинка `data:`): вывода нет.
    skipped: bool,
    /// Предок `pre` или `code`: текст идёт как есть, без экранирования.
    pre: bool,
    /// Предок `td` или `th`: `|` в ячейке таблицы становится `&#124;`.
    cell: bool,
    /// Родитель — `ol`: ширина маркера пункта с запасом ([`ol_marker`]), иначе 0.
    ol_marker: u64,
    /// Родитель — `pre`: `code` здесь — блок кода с оградой.
    in_pre: bool,
}

/// Элементы с обработчиком-обёрткой `htmd` 0.5.6 (`\n\n` до и после содержимого): `p`, `pre`,
/// `html`, `body`, `head`, секции и ячейки таблиц, списки и список `block_handler`
/// (`element_handler/mod.rs`). Элементы, которых нет ни здесь, ни среди особых правил
/// [`element`], обработчика у `htmd` нет: их вывод — вывод детей.
const WRAPPED: &[&str] = &[
    "p",
    "pre",
    "html",
    "body",
    "head",
    "td",
    "th",
    "tr",
    "tbody",
    "thead",
    "tfoot",
    "caption",
    "ol",
    "ul",
    "address",
    "article",
    "aside",
    "base",
    "basefont",
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
    "header",
    "iframe",
    "legend",
    "link",
    "main",
    "menu",
    "menuitem",
    "nav",
    "noframes",
    "optgroup",
    "option",
    "param",
    "script",
    "search",
    "section",
    "style",
    "summary",
    "textarea",
    "title",
    "track",
];

/// Локальное имя элемента в любом пространстве имён (так `htmd` выбирает обработчик).
fn tag(node: &Node) -> Option<&str> {
    match &node.data {
        NodeData::Element { name, .. } => Some(&*name.local),
        _ => None,
    }
}

/// Значения атрибутов с именем из `names`.
fn attr_values(node: &Node, names: &[&str]) -> Vec<String> {
    let NodeData::Element { attrs, .. } = &node.data else {
        return Vec::new();
    };
    attrs
        .borrow()
        .iter()
        .filter(|a| names.contains(&&*a.name.local))
        .map(|a| a.value.to_string())
        .collect()
}

/// Картинка, которую `html::drop_data_image` выбрасывает: `src` или `href` с адресом `data:`.
fn is_data_image(node: &Node) -> bool {
    attr_values(node, &["src", "href"]).iter().any(|v| {
        v.trim_start()
            .get(..5)
            .is_some_and(|s| s.eq_ignore_ascii_case("data:"))
    })
}

/// Число десятичных цифр.
fn digits(n: u64) -> u64 {
    let mut d = 1;
    let mut n = n / 10;
    while n > 0 {
        d += 1;
        n /= 10;
    }
    d
}

/// Ширина маркера пункта `ol` у `htmd`: номер, точка и пробелы до `2 + digits(highest)`
/// (`list.rs`, `add_ol_li_marker`), то есть `3 + digits(highest)`. `digits` у `htmd` считается
/// через `f32` и может быть на единицу больше точного — отсюда ещё единица.
fn ol_marker(ol: &Node) -> u64 {
    let start = attr_values(ol, &["start"])
        .first()
        .map_or(1, |v| v.parse::<i32>().unwrap_or(1).max(1));
    let items = ol
        .children
        .borrow()
        .iter()
        .filter(|c| tag(c) == Some("li"))
        .count() as u64;
    let highest = u64::from(start.unsigned_abs()).saturating_add(items.saturating_sub(1));
    4 + digits(highest)
}

/// Оценка значения атрибута в выводе: каждый байт не больше двух (`\(`, `\)`, `\"`), `|` в
/// ячейке — шесть.
fn attr_size(value: &str, scope: Scope) -> Size {
    let mut size = Size::default();
    for b in value.bytes() {
        size.bytes += if b == b'|' && scope.cell { 6 } else { 2 };
        size.newlines += u64::from(b == b'\n');
        size.ticks += u64::from(b == b'`');
    }
    size
}

/// Оценка текстового узла (см. документацию модуля).
pub(crate) fn text(contents: &str, scope: Scope) -> Size {
    if scope.skipped {
        return Size::default();
    }
    let mut size = Size {
        // Вне `pre`: `\` в начале и `\.` после номера; в `pre`: `\` перед оградой.
        bytes: if scope.pre { 1 } else { 2 },
        ..Size::default()
    };
    for b in contents.bytes() {
        size.bytes += match b {
            b'|' if scope.cell => 6,
            b'\\' | b'*' | b'_' | b'`' | b'[' | b']' | b'<' if !scope.pre => 2,
            _ => 1,
        };
        size.ticks += u64::from(b == b'`');
        // Перевод строки — строка под префиксом в любом текстовом узле, а не только в `pre`:
        // обработчик может вывести текст мимо `compress_whitespace` (формула `span.math`,
        // I-1 повторного ревью fix-10b). Общее правило, а не условие для одного обработчика.
        size.newlines += u64::from(b == b'\n');
    }
    size
}

/// Что наследуют дети узла `node`, если сам он получил `scope`.
pub(crate) fn scope_of_children(node: &Node, scope: Scope) -> Scope {
    let Some(t) = tag(node) else {
        return Scope {
            ol_marker: 0,
            in_pre: false,
            ..scope
        };
    };
    Scope {
        skipped: scope.skipped || SKIPPED_TAGS.contains(&t) || (t == "img" && is_data_image(node)),
        pre: scope.pre || t == "pre" || t == "code",
        cell: scope.cell || t == "td" || t == "th",
        ol_marker: if t == "ol" { ol_marker(node) } else { 0 },
        in_pre: t == "pre",
    }
}

/// Оценка элемента `node` по сумме оценок его детей `content`; `scope` — что он получил от
/// предков. Не элемент (документ) — вывод детей.
pub(crate) fn element(node: &Handle, scope: Scope, content: Size) -> Size {
    let Some(t) = tag(node) else {
        return content;
    };
    if scope.skipped || SKIPPED_TAGS.contains(&t) || (t == "img" && is_data_image(node)) {
        return Size::default();
    }
    match t {
        "br" => content.wrap(3, 1),
        "hr" => content.wrap(9, 4),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => content.wrap(11, 4),
        "b" | "strong" | "i" | "em" | "span" => content.wrap(4, 0),
        "blockquote" => content.prefixed(2).wrap(4, 4),
        "li" if scope.ol_marker > 0 => content.prefixed(scope.ol_marker).wrap(3, 3),
        "li" => content.prefixed(4).wrap(1, 1),
        "a" => {
            let link = attr_values(node, &["href"]);
            if link.is_empty() {
                return content;
            }
            let mut size = content.wrap(9, 0);
            for v in link.iter().chain(&attr_values(node, &["title"])) {
                size.add(attr_size(v, scope));
            }
            size
        }
        "img" => {
            let mut size = content.wrap(10, 0);
            for v in attr_values(node, &["src", "href", "alt", "title"]) {
                size.add(attr_size(&v, scope));
            }
            size
        }
        "code" if scope.in_pre => {
            // Ограда `max(3, серия + 1)` с обеих сторон, язык из `class` кода или `pre`, два
            // перевода строки.
            let mut lang = Size::default();
            for v in attr_values(node, &["class"])
                .iter()
                .chain(&parent_class(node))
            {
                lang.add(attr_size(v, scope));
            }
            let mut size = content.fenced(3).wrap(2, 2);
            size.add(lang);
            size
        }
        // Строчный код: разделитель не длиннее серии + 1, плюс два пробела.
        "code" => content.fenced(1).wrap(2, 0),
        "table" => table(node, content),
        _ if WRAPPED.contains(&t) => content.wrap(4, 4),
        _ => content,
    }
}

/// `class` родителя (`pre` блока кода).
fn parent_class(node: &Handle) -> Vec<String> {
    let weak = node.parent.take();
    let up = weak.as_ref().and_then(std::rc::Weak::upgrade);
    node.parent.set(weak);
    up.map_or_else(Vec::new, |p| attr_values(&p, &["class"]))
}

/// Таблица: большее из обхода детей (`\n\n` вокруг) и строк Markdown без дополнения
/// (`table::Grid::plain`: `\n\n`, подписи с `\n`, строка `2 + Σ(3 + bytes)`, разделитель
/// `2 + 6n`, недостающие ячейки заголовка по 3 байта, `\n`). Ячейки и подписи уже в `content`
/// (с обёрткой, которую таблица срезает). Строки, ячейки и столбцы — по секциям и строкам
/// таблицы, как их читает `htmd`; каждый узел считает одна таблица.
fn table(node: &Handle, content: Size) -> Size {
    let (mut rows, mut cells, mut columns, mut captions) = (0u64, 0u64, 0u64, 0u64);
    let mut count_row = |row: &Rc<Node>| {
        let n = row
            .children
            .borrow()
            .iter()
            .filter(|c| matches!(tag(c), Some("td" | "th")))
            .count() as u64;
        rows += 1;
        cells += n;
        columns = columns.max(n);
    };
    for child in node.children.borrow().iter() {
        match tag(child) {
            Some("thead" | "tbody" | "tfoot") => {
                for row in child.children.borrow().iter() {
                    if tag(row) == Some("tr") {
                        count_row(row);
                    }
                }
            }
            Some("tr") => count_row(child),
            Some("caption") => captions += 1,
            _ => {}
        }
    }
    let bytes = cells
        .saturating_mul(3)
        .saturating_add(rows.saturating_mul(3))
        .saturating_add(columns.saturating_mul(9))
        .saturating_add(captions)
        .saturating_add(8);
    content.wrap(bytes, rows.saturating_add(captions).saturating_add(5))
}

#[cfg(test)]
mod tests {
    use crate::guard::{guarded_parse, prepare_dom};
    use crate::html::render_with;
    use crate::table::TableLayout;

    /// Оценка и фактический вывод полного пути без дополнения таблиц.
    fn bound_and_output(html: &str) -> (u64, usize) {
        let tree = guarded_parse(html).expect("разбор под порогами");
        let bound = prepare_dom(&tree).expect("оценка в бюджете");
        let md = render_with(&tree, TableLayout::with_budget(0));
        (bound, md.len())
    }

    /// Формы, у которых вывод растёт сверх входа: префиксы цитат и списков, нумерация,
    /// ограды кода, ссылки и картинки с атрибутами, экранирование, `|` в ячейках, таблицы.
    const FORMS: &[&str] = &[
        "<blockquote><blockquote><pre>a\nb\n\nc</pre></blockquote></blockquote>",
        "<ul><li>one<ul><li>two<pre>x\ny\nz</pre></li></ul></li></ul>",
        "<ol start=98><li>a</li><li>b<ol><li>c<br>d<br>e</li></ol></li><li>z</li></ol>",
        "<ol start=2147483647><li>big</li><li>bigger</li></ol>",
        "<blockquote><ol><li><blockquote><p>p1</p><p>p2</p></blockquote></li></ol></blockquote>",
        "<pre><code class=\"language-rust\">fn x() {}\n```\n````inner\n</code></pre>",
        "<p><code>a`b``c```d</code> and <code>`edge`</code></p>",
        "<p><code><code><code>`x`</code></code></code></p>",
        "<p><a href=\"/a(b)c\" title=\"t&quot;q\nline\">link</a> <img src=\"/i(1).png\" alt=\"a\nb\" title=\"x\"></p>",
        "<p>*star* _u_ [b] \\back `t` &lt;tag&gt; 1. item # h - d + p > q = e ~ f</p>",
        "<h1>T</h1><h6>t<br>u</h6><hr><p>x<br><br>y</p>",
        "<table><tr><th>h|1</th><th>h2</th></tr><tr><td>a|b</td></tr><tr><td><pre>p\n|q</pre></td><td>c</td><td>d</td></tr></table>",
        "<table><caption>cap\nx</caption><tr><th>h</th></tr><tr><td><table><tr><td>in</td></tr></table></td></tr></table>",
        "<blockquote><table><thead><tr><th>a</th><th>b</th></tr></thead><tbody><tr><td>1</td><td>2</td></tr></tbody></table></blockquote>",
        "<ul><li><table><tr><th>h</th></tr><tr><td>v<br>w</td></tr></table></li></ul>",
        "<div><span>s</span><b>b</b><i>i</i><em>e</em><strong>st</strong><u>u</u><sup>1</sup></div>",
        "<p><span class=\"math math-inline\">x^2</span><span class=\"math math-display\">y</span></p>",
        "<dl><dt>term</dt><dd>def<blockquote>q\nr</blockquote></dd></dl><details><summary>s</summary>d</details>",
        "<pre>```fence\n~~~tilde\n</pre><pre><span>in span\nline</span><b>bold\nb</b></pre>",
        "<ul><li><p>a</p><p>b</p></li><li>c\n\n\nd</li></ul><ol><li></li><li>  </li></ol>",
        // Текст формулы `htmd` выводит как есть, с переводами строк (I-1 ревью fix-10b).
        "<blockquote><span class=\"math math-display\">a\nb\n\nc</span></blockquote>",
        "<ol start=7><li><span class=\"math math-inline\">x\ny\nz</span></li></ol>",
    ];

    /// Оценка — верхняя граница вывода `htmd` на формах роста (I-1 повторного ревью fix-10).
    /// Падение после обновления `htmd` — повод сверить правила модуля с его обработчиками.
    #[test]
    fn bound_covers_htmd_output() {
        for html in FORMS {
            let (bound, out) = bound_and_output(html);
            assert!(bound >= out as u64, "{bound} < {out}: {html}");
        }
    }

    /// Плотные формы, где вывод задаёт одно правило: без него оценка была бы меньше вывода
    /// (каждую строку ловит мутация своего правила).
    #[test]
    fn bound_covers_dense_forms() {
        let forms = [
            // `|` в ячейке — `&#124;`.
            format!(
                "<table><tr><th>h</th></tr><tr><td>{}</td></tr></table>",
                "|".repeat(500)
            ),
            // Разделитель и недостающие ячейки — по столбцам.
            format!(
                "<table><tr><th>h</th></tr><tr>{}</tr></table>",
                "<td></td>".repeat(300)
            ),
            // Экранирование текста.
            format!("<p>{}</p>", "*".repeat(1000)),
            // Экранирование адреса ссылки.
            format!("<p><a href=\"{}\">x</a></p>", "(".repeat(500)),
            // Префикс цитаты на каждой строке.
            format!("<blockquote><pre>{}</pre></blockquote>", "\n x".repeat(500)),
            // Переводы строк текста вне `pre` — строки под префиксом (формула `span.math`).
            format!(
                "<blockquote><span class=\"math math-display\">{}</span></blockquote>",
                "\n x".repeat(500)
            ),
            // Маркер пункта `ol` — по номеру последнего пункта.
            format!(
                "<ol start=99999><li><pre>{}</pre></li></ol>",
                "x\n".repeat(300)
            ),
        ];
        for html in &forms {
            let (bound, out) = bound_and_output(html);
            assert!(bound >= out as u64, "{bound} < {out}: {}", &html[..60]);
        }
    }

    /// Псевдослучайные деревья из элементов, растящих вывод, с особыми символами в тексте и
    /// атрибутах: оценка не меньше вывода ни на одном.
    #[test]
    fn bound_covers_generated_trees() {
        const OPEN: &[(&str, &str)] = &[
            ("<blockquote>", "</blockquote>"),
            ("<ul><li>", "</li></ul>"),
            ("<ol start=9><li>", "</li></ol>"),
            ("<pre>", "</pre>"),
            ("<code>", "</code>"),
            ("<p>", "</p>"),
            ("<b>", "</b>"),
            ("<a href=\"/x(y)|z\" title=\"q\n|\">", "</a>"),
            ("<table><tr><th>h</th></tr><tr><td>", "</td></tr></table>"),
            ("<div>", "</div>"),
            ("<h2>", "</h2>"),
        ];
        const TEXT: &[&str] = &[
            "plain words ",
            "line\nbreak\n\n",
            "`tick` ``two`` ",
            "*_[]\\<a ",
            "| pipe |",
            "1. num ",
            "<br>",
            "<img src=\"/p(1)\" alt=\"a|b\">",
            "   ",
            "<span class=\"math math-display\">m\n\nn\n</span>",
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..600 {
            let mut html = String::new();
            let mut open = Vec::new();
            for _ in 0..(4 + next(24)) {
                match next(3) {
                    0 if open.len() < 12 => {
                        let (o, c) = OPEN[next(OPEN.len())];
                        html.push_str(o);
                        open.push(c);
                    }
                    1 if !open.is_empty() => html.push_str(open.pop().unwrap_or_default()),
                    _ => html.push_str(TEXT[next(TEXT.len())]),
                }
            }
            while let Some(c) = open.pop() {
                html.push_str(c);
            }
            let (bound, out) = bound_and_output(&html);
            assert!(bound >= out as u64, "{bound} < {out}: {html}");
        }
    }

    /// Префикс считается на каждой строке каждого уровня: оценка растёт как строки × глубина,
    /// а не как вход.
    #[test]
    fn prefix_bound_grows_with_depth() {
        let page = |depth: usize| {
            format!(
                "{}<pre>{}</pre>{}",
                "<blockquote>".repeat(depth),
                "x\n".repeat(1000),
                "</blockquote>".repeat(depth)
            )
        };
        let (shallow, _) = bound_and_output(&page(1));
        let (deep, out) = bound_and_output(&page(50));
        assert!(deep >= out as u64);
        assert!(deep > shallow + 49 * 2 * 1000, "{shallow} → {deep}");
    }
}
