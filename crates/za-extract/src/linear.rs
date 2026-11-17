//! Запасное линейное извлечение текста без построителя дерева (R1-C2 D127, ревью задачи 7).
//!
//! Когда [`crate::guard`] отвергает страницу как слишком сложную, текст извлекается
//! токенизатором `html5ever` напрямую: он линеен (нет квадратичных проходов построителя
//! дерева и рекурсии `htmd`), верно разбирает сущности и raw-text и применяет тот же список
//! пропуска [`crate::html::SKIPPED_TAGS`]. Результат — только текст и заголовок, без ссылок
//! и разметки; потребитель помечает его как упрощённый.
//!
//! **Стек пропускаемых элементов** (I2 повторного ревью прохода fix-7). Открытые элементы из
//! списка пропуска лежат в стеке; текст виден, только когда стек пуст. Построителя дерева
//! нет, поэтому несколько его правил повторены явно — без них один элемент без закрывающего
//! тега прятал весь остальной текст страницы:
//! - пустые элементы (`input`, `embed`) и самозакрытые теги в стек не кладутся; raw-text
//!   элементы кладутся всегда — токенизатор и после `<script/>` читает сырой текст до
//!   `</script>`;
//! - закрывающий тег снимает стек до ближайшего элемента с тем же именем, закрывающий без
//!   пары игнорируется;
//! - `head` закрывается неявно: открывающим тегом, которому не место в `head` (в том числе
//!   `<body>`), и непробельным текстом — как в режиме вставки «in head»;
//! - `option` закрывается следующим `option`, `optgroup`, `hr` и закрытием родителя
//!   (`select`, `datalist`, `optgroup`); `button` — следующим `button`;
//! - глубина стека ограничена [`MAX_STACK`]: проход остаётся линейным, лишние вложенные
//!   элементы в стек не кладутся.
//!
//! Квадратичная часть самого токенизатора — проверка повторов атрибутов в одном теге,
//! поэтому вход сначала проходит [`crate::attrs::sanitize`]: ни у одного тега не остаётся
//! больше [`crate::attrs::LINEAR_ATTRS`] атрибутов.

use std::cell::{Cell, RefCell};

use html5ever::buffer_queue::BufferQueue;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{
    Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};

use crate::attrs::sanitize;
use crate::html::{Markdown, SKIPPED_TAGS, collapse_blank_lines, cut_title};

/// Предел глубины стека пропускаемых элементов.
pub const MAX_STACK: usize = 64;

/// Приёмник токенов: собирает видимый текст и первый `<title>` из пространства имён HTML.
#[derive(Default)]
struct LinearSink {
    out: RefCell<String>,
    title: RefCell<String>,
    /// Заголовок уже взят (первый `<title>` вне SVG).
    title_done: Cell<bool>,
    /// Вершина стека — `<title>`, текст которого идёт в заголовок.
    in_title: Cell<bool>,
    /// Открытые пропускаемые элементы (имена из [`SKIPPED_TAGS`]).
    stack: RefCell<Vec<&'static str>>,
}

impl LinearSink {
    /// Текст: пробелы (включая переводы строк внутри текстовых узлов) схлопнуты в один
    /// пробел; границы абзацев дают теги блочных элементов.
    fn push_text(&self, text: &str) {
        let mut out = self.out.borrow_mut();
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !matches!(out.chars().last(), None | Some(' ') | Some('\n')) {
                    out.push(' ');
                }
            } else {
                out.push(ch);
            }
        }
    }

    fn push_break(&self) {
        let mut out = self.out.borrow_mut();
        if !matches!(out.chars().last(), None | Some('\n')) {
            out.push('\n');
        }
    }

    fn top(&self) -> Option<&'static str> {
        self.stack.borrow().last().copied()
    }

    /// Снять вершину, если это `name`.
    fn pop_if(&self, name: &str) {
        if self.top() == Some(name) {
            let len = self.stack.borrow().len();
            self.pop_to(len - 1);
        }
    }

    /// Снять стек до длины `len`; снятый `<title>` с заголовком завершает заголовок.
    fn pop_to(&self, len: usize) {
        let mut stack = self.stack.borrow_mut();
        if self.in_title.get() && stack.len() > len {
            self.in_title.set(false);
            self.title_done.set(true);
        }
        stack.truncate(len);
    }

    fn text(&self, text: &str) {
        if self.in_title.get() {
            self.title.borrow_mut().push_str(text);
            return;
        }
        // Непробельный текст закрывает `head`: построитель переносит его в тело.
        if self.top() == Some("head") && !text.chars().all(char::is_whitespace) {
            self.pop_if("head");
        }
        if self.stack.borrow().is_empty() {
            self.push_text(text);
        }
    }

    fn start_tag(&self, tag: &Tag) {
        let name = &*tag.name;
        if self.top() == Some("head") && !in_head(name) {
            self.pop_if("head");
        }
        match name {
            "option" | "optgroup" | "hr" => self.pop_if("option"),
            "button" => self.pop_if("button"),
            _ => {}
        }
        if let Some(&skipped) = SKIPPED_TAGS.iter().find(|t| **t == name) {
            let raw = raw_kind(name).is_some();
            let open = !is_void(name) && (raw || !tag.self_closing);
            if open && self.stack.borrow().len() < MAX_STACK {
                // `<title>` в SVG — не заголовок страницы (как на полном пути, D109).
                let svg = self.stack.borrow().contains(&"svg");
                self.stack.borrow_mut().push(skipped);
                if name == "title" && !self.title_done.get() && !svg {
                    self.in_title.set(true);
                }
            }
        } else if is_break(name) && self.stack.borrow().is_empty() {
            self.push_break();
        }
    }

    fn end_tag(&self, tag: &Tag) {
        let name = &*tag.name;
        let found = self.stack.borrow().iter().rposition(|t| *t == name);
        if let Some(at) = found {
            self.pop_to(at);
            return;
        }
        // Неявные закрытия построителя: родитель `option` и `</body>`, `</html>`, `</br>` в
        // `head`.
        match name {
            "select" | "datalist" | "optgroup" => self.pop_if("option"),
            "body" | "html" | "br" => self.pop_if("head"),
            _ => {}
        }
        if is_break(name) && self.stack.borrow().is_empty() {
            self.push_break();
        }
    }
}

impl TokenSink for LinearSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        match token {
            Token::CharacterTokens(text) => self.text(&text),
            Token::TagToken(tag) => match tag.kind {
                TagKind::StartTag => {
                    self.start_tag(&tag);
                    // Плотный raw-text (`<script>`, `<style>`, `<title>`, …): токенизатор сам
                    // поглотит содержимое как сырой текст до парного закрытия.
                    if &*tag.name == "plaintext" {
                        return TokenSinkResult::Plaintext;
                    }
                    if let Some(kind) = raw_kind(&tag.name) {
                        return TokenSinkResult::RawData(kind);
                    }
                }
                TagKind::EndTag => self.end_tag(&tag),
            },
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

/// Линейное извлечение: текст и заголовок без ссылок и разметки. Вход уже обрезан до
/// предела (вызывающий, [`crate::html::html_to_markdown`]); теги с множеством атрибутов
/// закрываются до токенизатора ([`sanitize`]).
pub fn linear_extract(html: &str) -> Markdown {
    let html = sanitize(html);
    let tok = Tokenizer::new(LinearSink::default(), TokenizerOpts::default());
    let input = BufferQueue::default();
    input.push_back(StrTendril::from_slice(&html));
    let _ = tok.feed(&input);
    tok.end();
    let sink = tok.sink;
    // Заголовок — не длиннее `TITLE_MAX_BYTES`, как на полном пути (m1 финального ревью).
    let title = cut_title(
        sink.title
            .borrow()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    );
    let text = collapse_blank_lines(sink.out.borrow().trim());
    Markdown {
        title,
        text,
        simplified: true,
    }
}

/// Элементы, которые дают перенос строки (грубая разбивка на абзацы в упрощённом тексте).
fn is_break(tag: &str) -> bool {
    const BREAK: &[&str] = &[
        "p",
        "div",
        "br",
        "li",
        "tr",
        "td",
        "th",
        "caption",
        "section",
        "article",
        "table",
        "ul",
        "ol",
        "pre",
        "blockquote",
        "hr",
        "dd",
        "dt",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "header",
        "footer",
        "nav",
        "main",
        "figure",
    ];
    BREAK.contains(&tag)
}

/// Пустые элементы HTML: закрывающего тега у них нет (WHATWG, «void elements»).
fn is_void(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "basefont"
            | "bgsound"
            | "br"
            | "col"
            | "embed"
            | "frame"
            | "hr"
            | "img"
            | "input"
            | "keygen"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Элементы, которые остаются в `head` (режим вставки «in head»): любой другой открывающий
/// тег закрывает `head`.
fn in_head(tag: &str) -> bool {
    matches!(
        tag,
        "head"
            | "html"
            | "base"
            | "basefont"
            | "bgsound"
            | "link"
            | "meta"
            | "title"
            | "noscript"
            | "noframes"
            | "style"
            | "script"
            | "template"
    )
}

/// Состояние raw-text/rcdata для тега: токенизатор поглотит содержимое как сырой текст,
/// не разбирая внутри теги (как построитель дерева, спецификация HTML).
fn raw_kind(name: &str) -> Option<RawKind> {
    match name {
        "title" | "textarea" => Some(RawKind::Rcdata),
        "style" | "xmp" | "iframe" | "noembed" | "noframes" | "noscript" => Some(RawKind::Rawtext),
        "script" => Some(RawKind::ScriptData),
        _ => None,
    }
}
