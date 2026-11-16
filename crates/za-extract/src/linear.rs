//! Запасное линейное извлечение текста без построителя дерева (R1-C2 D127, ревью задачи 7).
//!
//! Когда [`crate::guard`] отвергает страницу как слишком сложную, текст извлекается
//! токенизатором `html5ever` напрямую: он линеен (нет квадратичных проходов построителя
//! дерева и рекурсии `htmd`), верно разбирает сущности и raw-text и применяет тот же список
//! пропуска [`crate::html::SKIPPED_TAGS`]. Результат — только текст и заголовок, без ссылок
//! и разметки; потребитель помечает его как упрощённый.
//!
//! Квадратичная часть самого токенизатора — проверка повторов атрибутов в одном теге,
//! поэтому вход сначала проходит [`crate::attrs::sanitize`]: ни у одного тега не остаётся
//! больше [`crate::attrs::LINEAR_ATTRS`] атрибутов.

use std::cell::{Cell, RefCell};

use html5ever::buffer_queue::BufferQueue;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts};

use crate::attrs::sanitize;
use crate::html::{Markdown, SKIPPED_TAGS, collapse_blank_lines};

/// Приёмник токенов: собирает видимый текст и первый `<title>`. Содержимое элементов из
/// [`SKIPPED_TAGS`] не попадает в текст (`skip_depth`); `<title>` при этом перехватывается
/// в заголовок.
#[derive(Default)]
struct LinearSink {
    out: RefCell<String>,
    title: RefCell<String>,
    /// Заголовок уже взят (первый `<title>`).
    title_done: Cell<bool>,
    in_title: Cell<bool>,
    /// Вложенность внутри пропускаемых элементов.
    skip_depth: Cell<usize>,
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
}

impl TokenSink for LinearSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        match token {
            Token::CharacterTokens(text) => {
                if self.in_title.get() {
                    self.title.borrow_mut().push_str(&text);
                } else if self.skip_depth.get() == 0 {
                    self.push_text(&text);
                }
            }
            Token::TagToken(tag) => {
                let name = &*tag.name;
                let raw = raw_kind(name);
                match tag.kind {
                    html5ever::tokenizer::TagKind::StartTag => {
                        if SKIPPED_TAGS.contains(&name) {
                            self.skip_depth.set(self.skip_depth.get() + 1);
                            if name == "title" && !self.title_done.get() {
                                self.in_title.set(true);
                            }
                        } else if is_break(name) {
                            self.push_break();
                        }
                        // Плотный raw-text (`<script>`, `<style>`, `<title>`, …): токенизатор
                        // сам поглотит содержимое как сырой текст до парного закрытия.
                        if name == "plaintext" {
                            return TokenSinkResult::Plaintext;
                        }
                        if let Some(kind) = raw {
                            return TokenSinkResult::RawData(kind);
                        }
                    }
                    html5ever::tokenizer::TagKind::EndTag => {
                        if SKIPPED_TAGS.contains(&name) {
                            self.skip_depth.set(self.skip_depth.get().saturating_sub(1));
                            if name == "title" && self.in_title.get() {
                                self.in_title.set(false);
                                self.title_done.set(true);
                            }
                        } else if is_break(name) {
                            self.push_break();
                        }
                    }
                }
            }
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
    let title = {
        let raw = sink.title.borrow();
        let joined = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        (!joined.is_empty()).then_some(joined)
    };
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
