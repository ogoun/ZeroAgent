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
//! `>` в значении в кавычках не обнулит счёт. Цена — 12 состояний на байт, единицы
//! миллисекунд на 2 МиБ.
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
/// больше [`MAX_ATTRS`] атрибутов. Линейно по длине входа.
pub fn attr_check(html: &str) -> Result<(), &'static str> {
    let mut branches = Branches::new();
    for &b in html.as_bytes() {
        // Новый атрибут в ветке, где их уже `c`.
        if branches.feed(b).is_some_and(|c| c + 1 > MAX_ATTRS) {
            return Err("attrs");
        }
    }
    Ok(())
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
