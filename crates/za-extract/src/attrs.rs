//! Предел атрибутов тега до токенизатора (R1-C2 D127; Ruling прохода fix-7, сомнение 2).
//!
//! Токенизатор `html5ever` 0.38 проверяет повтор имени атрибута проходом по уже собранным
//! атрибутам тега (`finish_attribute`): тег с `k` атрибутами стоит `O(k²)` сравнений, и
//! один тег с сотнями тысяч атрибутов разбирается десятки секунд. Онлайн-страж
//! ([`crate::guard`]) этого не видит: весь тег — одно событие `create_element` уже после
//! токенизации. Линейное извлечение ([`crate::linear`]) идёт тем же токенизатором. Поэтому
//! до токенизатора — линейный байтовый проход этого модуля.
//!
//! **Модель.** Проход повторяет состояния тега токенизатора по WHATWG (§13.2.5: имя тега,
//! до и после имени атрибута, имя, значение в двойных, одинарных кавычках и без них, после
//! значения в кавычках, самозакрывающийся тег) и считает начала атрибутов, включая
//! повторы: их проверка стоит столько же. Переключений построителя дерева (raw text у
//! `script`, `style`, `textarea`, `title`, CDATA внутри SVG, комментарии) проход не знает.
//! Поэтому он ведёт **множество** возможных состояний (недетерминированный автомат):
//! гипотеза «вне тега» есть всегда, каждый `<` с буквой (и `</` с буквой) открывает ветку
//! тега, ветки в одном состоянии сливаются с наибольшим счётом. Настоящее состояние
//! токенизатора всегда среди веток, поэтому счёт — верхняя граница для настоящего тега при
//! любом поведении построителя: кавычка внутри `<script>` не спрячет следующий тег, а
//! `>` в значении в кавычках не обнулит счёт. Цена — 12 состояний на байт, около 30 мс на
//! 2 МиБ.
//!
//! **Raw text** (Ruling прохода fix-8, сомнение 3). Рядом с ветками идёт детерминированная
//! модель токенизатора ([`Model`]: данные, теги, комментарии, DOCTYPE, raw text). Где она
//! точно знает, что токенизатор в raw text (содержимое `script`, `style`, `textarea`,
//! `title`, `xmp`, `iframe`, `noembed`, `noframes`, `noscript`, `plaintext` в HTML), ветки по
//! байтам не идут и новых не открывают: тег-подобный текст скрипта (`i<n` и тысячи «слов»
//! без `>`) токенизатор тегом не считает, и ложного упрощения нет. На `>` открывающего тега
//! raw-text элемента ветки сбрасываются: открытого тега у токенизатора в этот миг нет. С
//! `<` подходящего закрывающего тега ветки идут снова: его атрибуты токенизатор тоже
//! собирает. Где построитель дерева raw text не включает или модель не может проследить
//! токенизатор (иностранное содержимое `svg`/`math`, `frameset`, `<col>` в шаблоне, `<!--`
//! внутри скрипта), модель до конца входа уступает веткам — прежней верхней границе.
//!
//! **Итоги.**
//! - [`attr_check`] — `Err`, если у какой-то ветки больше [`MAX_ATTRS`] атрибутов в одном
//!   теге. Тогда страница идёт в линейное извлечение с признаком `simplified`. Суммарного
//!   предела нет: при `k ≤ 1024` на тег 2 МиБ тегов по 1000 разных атрибутов разбираются
//!   за 0,5 с в release (замер fix-8, `.tmp/fix8/attr-bench.log`), а сумма по модели
//!   ложно срабатывала бы на тег-подобном тексте больших скриптов.
//! - [`sanitize`] — копия входа, в которой ни у одной ветки нет больше [`LINEAR_ATTRS`]
//!   атрибутов в теге: перед символом, который начал бы следующий атрибут, вставляется `>`.
//!   Её отдают токенизатору линейного извлечения: атрибуты ему не нужны, а цена проверки
//!   повторов ограничена линейно. Вставка, а не удаление: ложная ветка (тег-подобный текст
//!   внутри `<script>` или значения в кавычках) получает лишний `>` в тексте, который и так
//!   не виден, а удаление съело бы `</script>` и с ним весь текст после скрипта.

use std::borrow::Cow;

/// Предел атрибутов в одном теге (Ruling прохода fix-7): больше — `simplified`.
pub const MAX_ATTRS: u32 = 1024;
/// Предел атрибутов тега во входе линейного извлечения ([`sanitize`]): атрибуты ему не
/// нужны, а цена проверки повторов на 2 МиБ — не больше `10^6 × 64` сравнений.
pub const LINEAR_ATTRS: u32 = 64;

/// Состояние ветки тега (WHATWG §13.2.5); гипотеза «вне тега» отдельного состояния не
/// имеет — она есть всегда.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum St {
    /// После `<`.
    Lt,
    /// После `</`.
    LtSlash,
    TagName,
    BeforeAttrName,
    AttrName,
    AfterAttrName,
    BeforeAttrValue,
    ValueDq,
    ValueSq,
    ValueUnq,
    AfterValueQuoted,
    SelfClosing,
}

const STATES: usize = 12;
const ALL: [St; STATES] = [
    St::Lt,
    St::LtSlash,
    St::TagName,
    St::BeforeAttrName,
    St::AttrName,
    St::AfterAttrName,
    St::BeforeAttrValue,
    St::ValueDq,
    St::ValueSq,
    St::ValueUnq,
    St::AfterValueQuoted,
    St::SelfClosing,
];

/// Пробел HTML: `\r` токенизатор превращает в `\n`.
fn ws(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0C | b'\r' | b' ')
}

/// Переход ветки по байту: следующее состояние (`None` — тег кончился или ветка не тег) и
/// признак начала нового атрибута. «Перечитать в состоянии X» спецификации развёрнуто.
fn step(s: St, b: u8) -> (Option<St>, bool) {
    use St::*;
    match s {
        Lt if b.is_ascii_alphabetic() => (Some(TagName), false),
        Lt if b == b'/' => (Some(LtSlash), false),
        // `<!`, `<?`, `<` с прочим — комментарий или текст: это гипотеза «вне тега».
        Lt => (None, false),
        LtSlash if b.is_ascii_alphabetic() => (Some(TagName), false),
        LtSlash => (None, false),
        TagName if ws(b) => (Some(BeforeAttrName), false),
        TagName if b == b'/' => (Some(SelfClosing), false),
        TagName if b == b'>' => (None, false),
        TagName => (Some(TagName), false),
        BeforeAttrName if ws(b) => (Some(BeforeAttrName), false),
        // `/` и `>` — перечитать в «после имени атрибута».
        BeforeAttrName if b == b'/' => (Some(SelfClosing), false),
        BeforeAttrName if b == b'>' => (None, false),
        // Прочее, включая `=`, `"`, `'`, `<`, начинает атрибут.
        BeforeAttrName => (Some(AttrName), true),
        AttrName if ws(b) => (Some(AfterAttrName), false),
        AttrName if b == b'/' => (Some(SelfClosing), false),
        AttrName if b == b'>' => (None, false),
        AttrName if b == b'=' => (Some(BeforeAttrValue), false),
        AttrName => (Some(AttrName), false),
        AfterAttrName if ws(b) => (Some(AfterAttrName), false),
        AfterAttrName if b == b'/' => (Some(SelfClosing), false),
        AfterAttrName if b == b'=' => (Some(BeforeAttrValue), false),
        AfterAttrName if b == b'>' => (None, false),
        AfterAttrName => (Some(AttrName), true),
        BeforeAttrValue if ws(b) => (Some(BeforeAttrValue), false),
        BeforeAttrValue if b == b'"' => (Some(ValueDq), false),
        BeforeAttrValue if b == b'\'' => (Some(ValueSq), false),
        BeforeAttrValue if b == b'>' => (None, false),
        BeforeAttrValue => (Some(ValueUnq), false),
        ValueDq if b == b'"' => (Some(AfterValueQuoted), false),
        ValueDq => (Some(ValueDq), false),
        ValueSq if b == b'\'' => (Some(AfterValueQuoted), false),
        ValueSq => (Some(ValueSq), false),
        ValueUnq if ws(b) => (Some(BeforeAttrName), false),
        ValueUnq if b == b'>' => (None, false),
        ValueUnq => (Some(ValueUnq), false),
        AfterValueQuoted if ws(b) => (Some(BeforeAttrName), false),
        AfterValueQuoted if b == b'/' => (Some(SelfClosing), false),
        AfterValueQuoted if b == b'>' => (None, false),
        // Перечитать в «до имени атрибута»: прочее начинает атрибут.
        AfterValueQuoted => (Some(AttrName), true),
        SelfClosing if b == b'>' => (None, false),
        // Перечитать в «до имени атрибута».
        SelfClosing if ws(b) => (Some(BeforeAttrName), false),
        SelfClosing if b == b'/' => (Some(SelfClosing), false),
        SelfClosing => (Some(AttrName), true),
    }
}

/// Множество веток: наибольший счёт атрибутов текущего тега по состоянию (`None` — ветки
/// в этом состоянии нет). Гипотеза «вне тега» подразумевается всегда.
#[derive(Clone, Copy)]
struct Branches([Option<u32>; STATES]);

impl Branches {
    fn new() -> Self {
        Self([None; STATES])
    }

    /// Наибольший счёт среди веток, которые на байте `b` начали бы новый атрибут.
    fn would_start(&self, b: u8) -> Option<u32> {
        ALL.iter()
            .zip(self.0)
            .filter_map(|(&s, c)| c.filter(|_| step(s, b).1))
            .max()
    }

    /// Переход всех веток по байту `b`; `<` открывает новую ветку из гипотезы «вне тега».
    /// Итог — наибольший счёт ветки, начавшей на этом байте новый атрибут (до начала).
    fn feed(&mut self, b: u8) -> Option<u32> {
        let mut next = [None; STATES];
        let mut started = None;
        for (&s, c) in ALL.iter().zip(self.0) {
            let Some(c) = c else { continue };
            let (to, new_attr) = step(s, b);
            if new_attr {
                started = started.max(Some(c));
            }
            if let Some(to) = to {
                let n = c + u32::from(new_attr);
                let slot = &mut next[to as usize];
                *slot = (*slot).max(Some(n));
            }
        }
        if b == b'<' {
            let slot = &mut next[St::Lt as usize];
            *slot = (*slot).max(Some(0));
        }
        self.0 = next;
        started
    }
}

/// Проверка входа до токенизатора: `Err("attrs")` — в каком-то теге (в какой-то ветке)
/// больше [`MAX_ATTRS`] атрибутов. Ветки не идут по содержимому raw-text элементов, где
/// токенизатор точно в raw text ([`Model`]). Линейно по длине входа.
pub fn attr_check(html: &str) -> Result<(), &'static str> {
    let bytes = html.as_bytes();
    let mut branches = Branches::new();
    let mut model = Model::new();
    // Новый атрибут в ветке, где их уже `c`.
    let over = |started: Option<u32>| started.is_some_and(|c| c + 1 > MAX_ATTRS);
    for (i, &b) in bytes.iter().enumerate() {
        match model.feed(b) {
            Act::Feed => {
                if over(branches.feed(b)) {
                    return Err("attrs");
                }
            }
            Act::Enter => {
                if over(branches.feed(b)) {
                    return Err("attrs");
                }
                // Токенизатор в raw text: открытого тега нет, все ветки ложные.
                branches = Branches::new();
            }
            Act::Skip => {}
            Act::Resume(n) => {
                // Закрывающий тег raw text: ветки — с его `<`, атрибуты у него тоже считаются.
                branches = Branches::new();
                for &c in &bytes[i + 1 - n..=i] {
                    if over(branches.feed(c)) {
                        return Err("attrs");
                    }
                }
            }
        }
    }
    Ok(())
}

/// Действие с ветками на очередном байте ([`Model::feed`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Act {
    /// Ветки идут по байту.
    Feed,
    /// Байт — `>` открывающего тега raw-text элемента: ветки идут по нему, затем
    /// сбрасываются.
    Enter,
    /// Байт — содержимое raw text: ветки по нему не идут и новых не открывают.
    Skip,
    /// Байт завершил начало закрывающего тега raw text (`</script` и разделитель): ветки
    /// сбрасываются и идут по последним `n` байтам, с `<` этого тега.
    Resume(usize),
}

/// Состояние детерминированной модели токенизатора ([`Model`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tk {
    Data,
    /// После `<`.
    Lt,
    /// После `</`.
    LtSlash,
    /// Тег: имя и атрибуты (`St::TagName`…`St::SelfClosing`).
    Tag(St),
    /// После `<!`.
    MdOpen,
    /// После `<!-`.
    MdDash,
    /// Ложный комментарий и DOCTYPE: до первого `>`.
    Bogus,
    CommentStart,
    CommentStartDash,
    Comment,
    CommentEndDash,
    CommentEnd,
    CommentEndBang,
    /// Содержимое raw-text элемента [`RAW_TEXT`]`[elem]`; `end` — сколько байт `</имя`
    /// совпало, `bang` — сколько байт `<!--` (только у `script`).
    Raw {
        elem: u8,
        end: u8,
        bang: u8,
    },
    /// `<plaintext>`: до конца входа — текст.
    Plaintext,
    /// Модель больше не уверена в состоянии токенизатора: дальше — только ветки.
    Unsure,
}

/// Элементы, после открывающего тега которых построитель дерева переключает токенизатор
/// в raw text до подходящего закрывающего тега (html5ever 0.38, `tree_builder/rules.rs`;
/// `noscript` — при `scripting_enabled`, как у [`crate::guard`]). `plaintext` переключает
/// до конца входа и в таблице не нужен.
const RAW_TEXT: &[&[u8]] = &[
    b"script",
    b"style",
    b"textarea",
    b"title",
    b"xmp",
    b"iframe",
    b"noembed",
    b"noframes",
    b"noscript",
];

/// Длина самого длинного имени, которое модель различает (`plaintext`).
const NAME_MAX: usize = 9;

/// Детерминированная модель токенизатора HTML (WHATWG §13.2.5): данные, теги, комментарии,
/// DOCTYPE, raw text (Ruling прохода fix-8, сомнение 3). Нужна, чтобы ветки не шли по
/// содержимому raw-text элементов: тег-подобный текст скрипта (`i<n` и тысячи «слов»)
/// токенизатор тегом не считает, а ветки считали — ложное упрощение.
///
/// Модель точна, пока построитель дерева переключает токенизатор так, как она ждёт: каждый
/// открывающий тег из [`RAW_TEXT`] в HTML начинает raw text. Где построитель поступает
/// иначе, модель переходит в [`Tk::Unsure`] до конца входа, и дальше работают только ветки
/// (прежняя верхняя граница):
/// - `<svg>`, `<math>` — иностранное содержимое: `<script>`, `<style>`, `<title>` в нём не raw
///   text, а `<![CDATA[` — раздел CDATA;
/// - `<frameset>` — в режимах frameset построитель пропускает `<script>`;
/// - `<col>` после `<template>` — в шаблоне режим «in column group» пропускает `<script>`;
/// - `<!--` внутри скрипта — экранирование: после `<!--<script>` первый `</script>` скрипт
///   не закрывает.
///
/// Ошибка модели в безопасную сторону (не пропустить raw text) даёт только прежнее ложное
/// упрощение. Пропуском модель пользуется лишь там, где токенизатор точно в raw text.
struct Model {
    st: Tk,
    /// Имя текущего тега (нижний регистр ASCII), первые [`NAME_MAX`] байт.
    name: [u8; NAME_MAX],
    name_len: usize,
    end_tag: bool,
    template_seen: bool,
}

impl Model {
    fn new() -> Self {
        Self {
            st: Tk::Data,
            name: [0; NAME_MAX],
            name_len: 0,
            end_tag: false,
            template_seen: false,
        }
    }

    fn start_name(&mut self, b: u8, end_tag: bool) {
        self.name[0] = b.to_ascii_lowercase();
        self.name_len = 1;
        self.end_tag = end_tag;
        self.st = Tk::Tag(St::TagName);
    }

    /// Тег кончился на `>`: состояние после него. Имя длиннее [`NAME_MAX`] модели не нужно
    /// и считается пустым.
    fn emit(&mut self) -> Act {
        self.st = Tk::Data;
        if self.end_tag {
            return Act::Feed;
        }
        let buf = self.name;
        let name = if self.name_len <= NAME_MAX {
            &buf[..self.name_len]
        } else {
            &[]
        };
        if matches!(name, b"svg" | b"math" | b"frameset") || (name == b"col" && self.template_seen)
        {
            self.st = Tk::Unsure;
            return Act::Feed;
        }
        if name == b"template" {
            self.template_seen = true;
        }
        if name == b"plaintext" {
            self.st = Tk::Plaintext;
            return Act::Enter;
        }
        match RAW_TEXT.iter().position(|r| *r == name) {
            Some(elem) => {
                self.st = Tk::Raw {
                    elem: elem as u8,
                    end: 0,
                    bang: 0,
                };
                Act::Enter
            }
            None => Act::Feed,
        }
    }

    fn feed(&mut self, b: u8) -> Act {
        use Tk::*;
        match self.st {
            Unsure => Act::Feed,
            Plaintext => Act::Skip,
            Raw { elem, end, bang } => self.raw(elem, end, bang, b),
            Data => {
                if b == b'<' {
                    self.st = Lt;
                }
                Act::Feed
            }
            Lt => {
                self.st = match b {
                    b'/' => LtSlash,
                    b'!' => MdOpen,
                    b'?' => Bogus,
                    b'<' => Lt,
                    _ if b.is_ascii_alphabetic() => {
                        self.start_name(b, false);
                        return Act::Feed;
                    }
                    _ => Data,
                };
                Act::Feed
            }
            LtSlash => {
                self.st = match b {
                    b'>' => Data,
                    _ if b.is_ascii_alphabetic() => {
                        self.start_name(b, true);
                        return Act::Feed;
                    }
                    _ => Bogus,
                };
                Act::Feed
            }
            Tag(s) => match step(s, b) {
                (None, _) => self.emit(),
                (Some(next), _) => {
                    if s == St::TagName && next == St::TagName {
                        if self.name_len < NAME_MAX {
                            self.name[self.name_len] = b.to_ascii_lowercase();
                        }
                        self.name_len = self.name_len.saturating_add(1);
                    }
                    self.st = Tag(next);
                    Act::Feed
                }
            },
            MdOpen | MdDash => {
                self.st = match b {
                    b'-' if self.st == MdOpen => MdDash,
                    b'-' => CommentStart,
                    b'>' => Data,
                    _ => Bogus,
                };
                Act::Feed
            }
            Bogus => {
                if b == b'>' {
                    self.st = Data;
                }
                Act::Feed
            }
            CommentStart | CommentStartDash => {
                self.st = match b {
                    b'-' if self.st == CommentStart => CommentStartDash,
                    b'-' => CommentEnd,
                    b'>' => Data,
                    _ => Comment,
                };
                Act::Feed
            }
            Comment | CommentEndDash => {
                self.st = match b {
                    b'-' if self.st == Comment => CommentEndDash,
                    b'-' => CommentEnd,
                    _ => Comment,
                };
                Act::Feed
            }
            CommentEnd | CommentEndBang => {
                self.st = match (self.st, b) {
                    (_, b'>') => Data,
                    (CommentEnd, b'!') => CommentEndBang,
                    (CommentEnd, b'-') => CommentEnd,
                    (_, b'-') => CommentEndDash,
                    _ => Comment,
                };
                Act::Feed
            }
        }
    }

    /// Байт содержимого raw text. Конец — `</имя` (без учёта регистра) и пробел, `/` или `>`
    /// (подходящий закрывающий тег). `<!--` в скрипте — экранирование, модель не уверена.
    fn raw(&mut self, elem: u8, end: u8, bang: u8, b: u8) -> Act {
        let name = RAW_TEXT[usize::from(elem)];
        let full = 2 + name.len();
        let restart = u8::from(b == b'<');
        let end = usize::from(end);
        if end == full && (ws(b) || b == b'/' || b == b'>') {
            self.st = match b {
                b'>' => Tk::Data,
                b'/' => Tk::Tag(St::SelfClosing),
                _ => Tk::Tag(St::BeforeAttrName),
            };
            self.end_tag = true;
            return Act::Resume(full + 1);
        }
        let next_end = match end {
            0 => restart,
            1 if b == b'/' => 2,
            _ if (2..full).contains(&end) && b.to_ascii_lowercase() == name[end - 2] => {
                u8::try_from(end + 1).unwrap_or(0)
            }
            _ => restart,
        };
        let next_bang = if name == b"script" {
            match (bang, b) {
                (1, b'!') => 2,
                (2, b'-') => 3,
                (3, b'-') => {
                    self.st = Tk::Unsure;
                    return Act::Feed;
                }
                _ => restart,
            }
        } else {
            0
        };
        self.st = Tk::Raw {
            elem,
            end: next_end,
            bang: next_bang,
        };
        Act::Skip
    }
}

/// Вход для линейного извлечения: перед символом, который начал бы в какой-то ветке атрибут
/// сверх [`LINEAR_ATTRS`], вставляется `>`. Во всех состояниях, где начинается атрибут
/// (до и после имени, после значения в кавычках, после `/`), `>` кончает тег, поэтому
/// такие ветки — и настоящий тег среди них — закрываются; в значении в кавычках, в
/// raw text и комментарии `>` — обычный символ. Ветки идут по уже исправленному тексту:
/// в итоге ни у одной ветки атрибутов не больше предела. Текст без таких тегов
/// возвращается без копии.
pub fn sanitize(html: &str) -> Cow<'_, str> {
    let mut branches = Branches::new();
    let mut out: Option<String> = None;
    for (i, ch) in html.char_indices() {
        let mut buf = [0u8; 4];
        let bytes = ch.encode_utf8(&mut buf).as_bytes();
        // Байты продолжения UTF-8 — «прочее» во всех состояниях: решает первый байт.
        if branches
            .would_start(bytes[0])
            .is_some_and(|c| c + 1 > LINEAR_ATTRS)
        {
            out.get_or_insert_with(|| html[..i].to_owned()).push('>');
            branches.feed(b'>');
        }
        for &b in bytes {
            branches.feed(b);
        }
        if let Some(o) = out.as_mut() {
            o.push(ch);
        }
    }
    match out {
        Some(o) => Cow::Owned(o),
        None => Cow::Borrowed(html),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Наибольший счёт атрибутов любой ветки на всём входе.
    fn max_attrs(html: &str) -> u32 {
        let mut branches = Branches::new();
        let mut max = 0;
        for &b in html.as_bytes() {
            if let Some(c) = branches.feed(b) {
                max = max.max(c + 1);
            }
        }
        max
    }

    #[test]
    fn counts_attributes_like_tokenizer() {
        assert_eq!(max_attrs("<p>text</p>"), 0);
        assert_eq!(max_attrs("<a href=x>"), 1);
        assert_eq!(max_attrs("<a b c d>"), 3);
        assert_eq!(max_attrs("<a b=1 c='2' d=\"3\">"), 3);
        // Без пробелов после значения в кавычках — тоже новые атрибуты.
        assert_eq!(max_attrs("<a b=\"1\"c=\"2\"d=\"3\">"), 3);
        // `/` разделяет атрибуты, `=` в начале — имя атрибута.
        assert_eq!(max_attrs("<a b/c/d>"), 3);
        assert_eq!(max_attrs("<a =x>"), 1);
        // `>` в значении в кавычках тег не кончает.
        assert_eq!(max_attrs("<a b=\">\" c=\">\" d>"), 3);
        // Атрибуты у закрывающего тега токенизатор тоже собирает.
        assert_eq!(max_attrs("</a b c>"), 2);
        // Кавычка в имени атрибута значение не открывает.
        assert_eq!(max_attrs("<a b\"c d>"), 2);
    }

    #[test]
    fn quote_in_script_does_not_hide_next_tag() {
        // Кавычка внутри `<script>` для ветки «тег» открыла бы значение до конца входа;
        // ветка «вне тега» видит настоящий `<div …>` после `</script>`.
        let mut html = String::from("<script>x<a b='</script><div");
        for i in 0..2000 {
            html.push_str(&format!(" a{i}=\">\""));
        }
        html.push('>');
        assert!(max_attrs(&html) > MAX_ATTRS);
        assert_eq!(attr_check(&html), Err("attrs"));
    }

    #[test]
    fn sanitize_caps_every_branch() {
        let mut html = String::from("<p>before</p><div");
        for i in 0..5000 {
            html.push_str(&format!(" a{i}"));
        }
        html.push_str(">after</div>");
        let clean = sanitize(&html);
        assert!(max_attrs(&clean) <= LINEAR_ATTRS, "{}", max_attrs(&clean));
        assert!(clean.contains("<p>before</p>") && clean.contains(">after</div>"));
        // Обычный текст — без копии.
        assert!(matches!(sanitize("<a b c>x</a>"), Cow::Borrowed(_)));
        // Многобайтовые имена атрибутов: вставка — между целыми символами.
        let wide = format!("<b{}>", " я".repeat(500));
        assert!(max_attrs(&sanitize(&wide)) <= LINEAR_ATTRS);
        // Ложная ветка внутри скрипта (`i<n` и сотни слов без `>`) получает `>` в тексте
        // скрипта, а `</script>` и текст после него остаются.
        let mut js = String::from("<script>if (i<n) {");
        for k in 0..200 {
            js.push_str(&format!(" x{k} = {k};"));
        }
        js.push_str(" }</script><p>after script</p>");
        let clean = sanitize(&js);
        assert!(matches!(clean, Cow::Owned(_)), "ложная ветка за пределом");
        assert!(clean.contains("</script><p>after script</p>"), "{clean}");
        assert!(max_attrs(&clean) <= LINEAR_ATTRS);
    }

    /// Состояние модели после входа.
    fn model_after(html: &str) -> Tk {
        let mut model = Model::new();
        for &b in html.as_bytes() {
            model.feed(b);
        }
        model.st
    }

    /// Комментарии — как у токенизатора: конец только `-->` или `--!>`, а также `<!-->` и
    /// `<!--->`. Ложный комментарий и DOCTYPE — до первого `>`.
    #[test]
    fn model_tracks_comments_like_tokenizer() {
        for done in [
            "<!---->",
            "<!-->",
            "<!--->",
            "<!--a-->",
            "<!--a--!>",
            "<!--a--->",
            "<!--<!-->",
            "<!--a--!-->",
            "<!x>",
            "<!->",
            "<!DOCTYPE html PUBLIC \"a>",
            "<?xml x>",
            "</ x>",
            "</>",
            "<p>",
            "</p a=\">\">",
        ] {
            assert_eq!(model_after(done), Tk::Data, "{done}");
        }
        for open in [
            "<!--!>",
            "<!-- -- >",
            "<!--a-- >",
            "<!--a->",
            "<!--a--x>",
            "<!--a--!x>",
        ] {
            assert_eq!(model_after(open), Tk::Comment, "{open}");
        }
        assert_eq!(model_after("<p a='>'"), Tk::Tag(St::AfterValueQuoted));
    }

    /// Raw text кончается только подходящим закрывающим тегом с разделителем.
    #[test]
    fn model_ends_raw_text_on_appropriate_end_tag() {
        assert!(matches!(model_after("<script>x"), Tk::Raw { .. }));
        assert!(matches!(model_after("<script/>x"), Tk::Raw { .. }));
        assert!(matches!(model_after("<SCRIPT >x"), Tk::Raw { .. }));
        for inside in [
            "<script>a</scriptx>",
            "<script>a</scrip>",
            "<script>a</style>",
            "<style>a</script>",
            "<title><p>",
            "<textarea></textarea",
            "<noscript><script></script>",
        ] {
            assert!(matches!(model_after(inside), Tk::Raw { .. }), "{inside}");
        }
        assert_eq!(model_after("<script>a</script>"), Tk::Data);
        assert_eq!(model_after("<script>a</SCRIPT>"), Tk::Data);
        assert_eq!(model_after("<script>a<</script>"), Tk::Data);
        assert_eq!(model_after("<script>a</scrip</script>"), Tk::Data);
        assert_eq!(
            model_after("<style>a</style\t"),
            Tk::Tag(St::BeforeAttrName)
        );
        assert_eq!(model_after("<title>a</title/"), Tk::Tag(St::SelfClosing));
        assert_eq!(model_after("<plaintext></plaintext>"), Tk::Plaintext);
        // Длинное имя, начинающееся как raw-text элемент, — обычный тег.
        assert_eq!(model_after("<scripts>"), Tk::Data);
        assert_eq!(model_after("<noscriptx>"), Tk::Data);
    }

    /// Где построитель raw text не включает, модель уступает веткам.
    #[test]
    fn model_gives_up_where_tree_builder_differs() {
        for unsure in [
            "<svg>",
            "<SVG/>",
            "<math>",
            "<frameset>",
            "<template></template><col>",
            "<script><!--",
        ] {
            assert_eq!(model_after(unsure), Tk::Unsure, "{unsure}");
        }
        // `<col>` без шаблона и шаблон без `<col>` — модель уверена.
        assert_eq!(model_after("<table><col>"), Tk::Data);
        assert!(matches!(model_after("<template><script>x"), Tk::Raw { .. }));
        // `<!--` в `<style>` экранирования не включает.
        assert!(matches!(model_after("<style><!--"), Tk::Raw { .. }));
        // `svg` внутри raw text и комментария — не тег.
        assert_eq!(model_after("<script><svg></script>"), Tk::Data);
        assert_eq!(model_after("<!--<svg>-->"), Tk::Data);
    }

    /// Ветки не идут по raw text, но считают атрибуты закрывающего тега.
    #[test]
    fn raw_text_is_skipped_end_tag_attributes_counted() {
        let words: String = (0..2000).map(|k| format!(" x{k}")).collect();
        assert_eq!(attr_check(&format!("<script>i<n{words}</script>")), Ok(()));
        assert_eq!(attr_check(&format!("<i>i<n{words}</i>")), Err("attrs"));
        assert_eq!(
            attr_check(&format!("<script></script{words}>")),
            Err("attrs")
        );
        assert_eq!(
            attr_check(&format!("<svg><script>i<n{words}")),
            Err("attrs")
        );
    }

    #[test]
    fn limit_is_per_tag() {
        let at_limit = format!("<i{}>", " a".repeat(MAX_ATTRS as usize));
        assert_eq!(attr_check(&at_limit), Ok(()));
        // Много тегов по пределу — по отдельности в норме: счёт у каждого тега свой.
        assert_eq!(attr_check(&at_limit.repeat(50)), Ok(()));
        let over = format!("<i{}>", " a".repeat(MAX_ATTRS as usize + 1));
        assert_eq!(attr_check(&over), Err("attrs"));
    }
}
