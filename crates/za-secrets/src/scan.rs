//! Сканер секретов и маскировка (D90, D100).
//!
//! [`SecretMasker::mask`] заменяет в тексте:
//! - известные значения (от [`MIN_SECRET_LEN`] символов) и их форму в строке JSON
//!   (`\"`, `\\`, управляющие символы) → `[secret masked: <имя>]`. Перевод строки
//!   (настоящий или `\n`, `\r` в тексте JSON) внутри значения не мешает: значение,
//!   разрезанное переносом, тоже находится;
//! - шаблоны ключей и токенов с границей слова слева → `[secret masked: <вид>]`.
//!   Хвост токена, перенесённый на следующую строку, маскируется вместе с ним;
//! - блок приватного ключа PEM до `-----END … -----`, а без `END` — до конца текста;
//! - пароль в URL (`://user:pass@`) и в строке подключения (`;Password=…;`), токен после
//!   `Bearer ` — только сам секрет.
//!
//! Без `regex` (D90): проход по байтам с таблицей префиксов. Время линейно от длины
//! текста (ревью задачи 10, I1):
//! - конец цепочки символов тела шаблона считается один раз на цепочку и запоминается;
//!   шаблон, не подошедший в цепочке, в ней больше не проверяется;
//! - проверки «похоже на случайное» смотрят не больше [`CHECK_WINDOW`] байт тела;
//! - известные значения ищутся алгоритмом Кнута — Морриса — Пратта.
//!
//! Граница слова слева — начало текста, символ вне `[A-Za-z0-9_]` или экранирующая
//! последовательность JSON (`\n`, `\t`, `\r`, `\b`, `\f`, `\uXXXX`): в сыром тексте
//! аргументов вызова (`raw_arguments`) перед токеном стоит буква экранирования (I2).
//!
//! Все найденные участки (и перекрывающиеся) сливаются: в маскированном тексте не
//! остаётся частей секрета, даже если шаблон кончается внутри известного значения.
//!
//! Регистр: известные значения сравниваются точно; префиксы токенов — как в их
//! форматах (`ghp_`, `AKIA` и др. регистр значим); `Bearer`, `Password=`, `Pwd=` — без
//! учёта регистра.
//!
//! Ограничения (записанный риск): значение в base64, URL-кодировке или с экранированием
//! `\uXXXX` обычных символов в сыром тексте не узнаётся (строки JSON декодирует
//! потребитель); шаблоны — только перечисленные в [`RULES`]; хвост токена после переноса
//! узнаётся, только если он похож на случайный (от 8 символов, буквы и цифры).

use std::borrow::Cow;
use std::fmt;

use crate::store::MIN_SECRET_LEN;

/// Начало метки маски: `[secret masked: <имя или вид>]`.
pub const MASK_PREFIX: &str = "[secret masked: ";

/// Сколько байт тела смотрят проверки «похоже на случайное» (I1: работа на попытку
/// ограничена).
pub const CHECK_WINDOW: usize = 512;

/// Запас предела хвоста маски потока сверх самого длинного известного значения, байт:
/// окно проверок шаблонов и метка блока PEM с избытком.
const HOLD_SLACK: usize = 2 * CHECK_WINDOW;

/// Метка блока приватного ключа PEM.
const PRIVATE_KEY: &str = "private_key";
/// Начало метки начала блока PEM.
const PEM_BEGIN: &str = "-----BEGIN ";
/// Начало метки конца блока PEM.
const PEM_END: &str = "-----END ";
/// Конец метки блока PEM.
const PEM_DASHES: &str = "-----";
/// Сколько байт после `-----BEGIN ` или `-----END ` ищется конец метки.
const PEM_LABEL_MAX: usize = 100;

/// Сколько переносов строки подряд может пересечь хвост токена.
const MAX_WRAPS: usize = 16;
/// Наименьшая длина хвоста токена после переноса строки.
const MIN_WRAP_TAIL: usize = 8;

/// Маскировщик: известные значения и шаблоны D90.
#[derive(Clone, Default)]
pub struct SecretMasker {
    /// Искомые значения по убыванию длины, без повторов.
    known: Vec<Needle>,
}

/// Известное значение в нормализованном виде (без переводов строки) и таблица КМП.
#[derive(Clone)]
struct Needle {
    pat: Vec<u8>,
    fail: Vec<usize>,
    label: String,
}

/// Найденный участок `start..end` (байты) и метка.
struct Span<'m> {
    start: usize,
    end: usize,
    label: &'m str,
}

impl SecretMasker {
    /// Маскировщик по парам (имя, значение); значения короче [`MIN_SECRET_LEN`]
    /// символов не ищутся.
    pub fn new(known: Vec<(String, String)>) -> Self {
        let mut needles: Vec<Needle> = Vec::new();
        for (label, value) in known {
            if value.chars().count() < MIN_SECRET_LEN {
                continue;
            }
            for form in [value.clone(), json_escaped(&value)] {
                let pat = normalized(form.as_bytes());
                if pat.len() < MIN_SECRET_LEN {
                    continue;
                }
                let fail = kmp_table(&pat);
                needles.push(Needle {
                    pat,
                    fail,
                    label: label.clone(),
                });
            }
        }
        needles.sort_by(|a, b| {
            b.pat
                .len()
                .cmp(&a.pat.len())
                .then_with(|| a.pat.cmp(&b.pat))
        });
        needles.dedup_by(|a, b| a.pat == b.pat);
        Self { known: needles }
    }

    /// Текст с замаскированными секретами; без находок — исходный текст без копии.
    pub fn mask<'a>(&self, s: &'a str) -> Cow<'a, str> {
        self.mask_with_steps(s).0
    }

    /// [`SecretMasker::mask`] и число элементарных шагов сканера — для теста линейности
    /// (I1), не для продукта.
    #[doc(hidden)]
    pub fn mask_with_steps<'a>(&self, s: &'a str) -> (Cow<'a, str>, u64) {
        let mut steps = 0u64;
        let spans = self.merged(s, &mut steps);
        if spans.is_empty() {
            return (Cow::Borrowed(s), steps);
        }
        (Cow::Owned(render(s, &spans, s.len())), steps)
    }

    /// Предел удерживаемого хвоста маски потока, байт: [`HOLD_SLACK`] сверх самого
    /// длинного известного значения.
    fn hold_limit(&self) -> usize {
        HOLD_SLACK + self.known.first().map_or(0, |n| n.pat.len())
    }

    /// Шаг маски потока: `s` — удержанный хвост и новый фрагмент. Возвращает
    /// маскированный текст, который можно отдать, сколько байт `s` он покрывает (остаток
    /// держится до следующего фрагмента) и продолжение находки, отданной меткой до конца
    /// `s`. `last` — конец потока: отдаётся всё.
    ///
    /// Хвост держится только с самой ранней позиции, где может начинаться ещё не
    /// законченная находка (повторное ревью задачи 12, N1), остальное отдаётся сразу:
    /// - незаконченное слово в конце (токены и пароли в URL — без пробелов); слово перед
    ///   переводом строки (хвост токена переносится на следующую строку) и слово `Bearer`
    ///   перед пробелом (токен — в следующем фрагменте);
    /// - частичное совпадение известного значения в конце (состояние КМП по тексту без
    ///   переводов строки, поэтому учитываются и разрезанные переносом значения);
    /// - находка, доходящая до конца текста (PEM без `END`, тело токена);
    /// - граница внутри находки — на её начало.
    ///
    /// Хвост не длиннее [`Self::hold_limit`] (финальное ревью R1-C1), поэтому работа шага
    /// линейна от длины фрагмента. Незаконченная находка длиннее предела отдаётся меткой
    /// сразу, её продолжение пропускает [`MaskStream`]. Без такой находки хвост режется до
    /// предела: шаблон, начатый раньше, уже решён — его тело длиннее [`CHECK_WINDOW`].
    fn stream_step(&self, s: &str, last: bool, steps: &mut u64) -> (String, usize, Carry) {
        if last {
            let (t, n) = self.mask_with_steps(s);
            *steps += n + s.len() as u64;
            return (t.into_owned(), s.len(), Carry::None);
        }
        // Слово в конце, частичные совпадения и отрисовка — по проходу `s` на каждое.
        *steps += s.len() as u64 * (2 + self.known.len() as u64);
        let mut cut = word_hold(s).min(pem_hold(s));
        if !self.known.is_empty() && !s.is_empty() {
            let skip = skip_mask(s.as_bytes());
            for n in &self.known {
                if let Some(p) = partial_tail(s.as_bytes(), &skip, n) {
                    cut = cut.min(p);
                }
            }
        }
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        let spans = self.merged(s, steps);
        let limit = self.hold_limit();
        // Находки слиты и отсортированы: доходящая до конца текста — последняя.
        let open = spans.last().filter(|sp| sp.end >= s.len());
        if let Some(sp) = open
            && s.len() - sp.start > limit
        {
            // Длинная незаконченная находка: метка сейчас, продолжение пропускается.
            let carry = if sp.label == PRIVATE_KEY {
                Carry::Pem
            } else {
                Carry::Word
            };
            return (render(s, &spans, s.len()), s.len(), carry);
        }
        let open = open.map(|sp| sp.start);
        if let Some(start) = open {
            cut = cut.min(start);
        }
        if s.len() - cut > limit {
            // Незаконченная находка (если есть) начинается не раньше `s.len() - limit`.
            cut = s.len() - limit;
            while !s.is_char_boundary(cut) {
                cut += 1;
            }
            // Готовая находка на границе отдаётся целиком.
            if let Some(sp) = spans.iter().find(|sp| sp.start < cut && cut < sp.end) {
                cut = sp.end;
            }
            if let Some(start) = open {
                cut = cut.min(start);
            }
        }
        // Находки слиты и не пересекаются: граница внутри находки — на её начало.
        if let Some(sp) = spans.iter().find(|sp| sp.start < cut && cut < sp.end) {
            cut = sp.start;
        }
        let before: Vec<Span<'_>> = spans.into_iter().filter(|sp| sp.end <= cut).collect();
        (render(s, &before, cut), cut, Carry::None)
    }

    /// Находки в `s`, отсортированные и слитые (пересекающиеся — в одну, метка первой).
    fn merged<'m>(&'m self, s: &str, steps: &mut u64) -> Vec<Span<'m>> {
        let mut spans = Vec::new();
        if !self.known.is_empty() && !s.is_empty() {
            let skip = skip_mask(s.as_bytes());
            *steps += s.len() as u64;
            for n in &self.known {
                known_spans(s.as_bytes(), &skip, n, &mut spans, steps);
            }
        }
        Scanner::new(s, steps).run(&mut spans);
        // Устойчивая сортировка: при равных участках метка известного значения раньше.
        spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        let mut out: Vec<Span<'m>> = Vec::with_capacity(spans.len());
        for sp in spans {
            match out.last_mut() {
                Some(last) if sp.start < last.end => last.end = last.end.max(sp.end),
                _ => out.push(sp),
            }
        }
        out
    }
}

/// Продолжение находки, отданной меткой до конца удержанного хвоста.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Carry {
    #[default]
    None,
    /// Тело токена или пароль: пропускается до пробельного символа (с запасом: класс
    /// символов тела уже).
    Word,
    /// Блок PEM: пропускается до `-----END …-----` включительно.
    Pem,
}

/// Маска потока текста по фрагментам (D100, I3 ревью задачи 12): секрет на стыке
/// фрагментов не проскакивает. Держит хвост, с которого может начинаться ещё не
/// законченная находка, но не длиннее предела (финальное ревью R1-C1): работа линейна от
/// длины потока и на длинном тексте без пробелов (base64, минифицированный код).
///
/// Находка длиннее предела отдаётся меткой сразу, а её продолжение в следующих фрагментах
/// пропускается: тело токена — до пробельного символа, блок PEM — до `-----END …-----`.
/// Ограничение (записанный риск): находки, которые узнаются только по концу, — пароль в
/// URL (`://user:pass@`) и JWT с заголовком длиннее предела, — а также хвост токена после
/// переноса строки за пределом отдаются без маски.
#[derive(Default)]
pub struct MaskStream {
    held: String,
    carry: Carry,
    steps: u64,
}

impl fmt::Debug for MaskStream {
    /// Без удержанного текста: в нём может быть начало секрета.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MaskStream")
            .field("held_len", &self.held.len())
            .field("carry", &self.carry)
            .finish_non_exhaustive()
    }
}

impl MaskStream {
    /// Нечего отдавать и нечего пропускать: конец потока ничего не изменит.
    pub fn is_empty(&self) -> bool {
        self.held.is_empty() && self.carry == Carry::None
    }

    /// Следующий фрагмент `chunk`; возвращает маскированный текст, который можно отдать.
    /// `last` — конец потока: отдаётся всё, состояние сбрасывается.
    pub fn push(&mut self, m: &SecretMasker, chunk: &str, last: bool) -> String {
        let mut s = std::mem::take(&mut self.held);
        s.push_str(chunk);
        let Some(from) = self.swallow(&s, last) else {
            return String::new();
        };
        let rest = &s[from..];
        let (out, used, carry) = m.stream_step(rest, last, &mut self.steps);
        self.held = rest[used..].to_owned();
        self.carry = carry;
        out
    }

    /// Число элементарных шагов с начала потока — для теста линейности, не для продукта.
    #[doc(hidden)]
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Пропуск продолжения находки в начале `s`: позиция, с которой текст обрабатывается
    /// как обычно, или `None` — пропущен весь `s` (короткий хвост для поиска метки конца
    /// PEM на стыке фрагментов остаётся в `held`).
    fn swallow(&mut self, s: &str, last: bool) -> Option<usize> {
        let carry = self.carry;
        if last {
            self.carry = Carry::None;
        }
        match carry {
            Carry::None => Some(0),
            Carry::Word => {
                let p = s.find(char::is_whitespace);
                self.steps += p.unwrap_or(s.len()) as u64 + 1;
                if p.is_some() {
                    self.carry = Carry::None;
                }
                p
            }
            Carry::Pem => {
                self.steps += s.len() as u64 + 1;
                let Some(e) = s.find(PEM_END) else {
                    if !last {
                        // Метка конца может быть разрезана фрагментами.
                        let mut keep = s.len().saturating_sub(PEM_END.len() - 1);
                        while !s.is_char_boundary(keep) {
                            keep += 1;
                        }
                        self.held = s[keep..].to_owned();
                    }
                    return None;
                };
                let after = e + PEM_END.len();
                let head = &s.as_bytes()[after..];
                let window = &head[..head.len().min(PEM_LABEL_MAX)];
                if let Some(f) = window
                    .windows(PEM_DASHES.len())
                    .position(|w| w == PEM_DASHES.as_bytes())
                {
                    self.carry = Carry::None;
                    return Some(after + f + PEM_DASHES.len());
                }
                if !last && head.len() < PEM_LABEL_MAX {
                    // Конец метки ещё не пришёл.
                    self.held = s[e..].to_owned();
                }
                // Иначе метка без конца: как у [`pem_block`], блок — до конца текста.
                None
            }
        }
    }
}

/// Начало хвоста, который маска потока держит из-за незаконченной метки начала блока PEM
/// (`-----BEGIN ` без конца метки на той же строке): метка содержит пробелы, и
/// [`word_hold`] её не удержит. Иначе — длина текста.
fn pem_hold(s: &str) -> usize {
    let mut from = s.len().saturating_sub(PEM_BEGIN.len() + PEM_LABEL_MAX);
    while !s.is_char_boundary(from) {
        from += 1;
    }
    let Some(i) = s[from..].rfind(PEM_BEGIN).map(|i| from + i) else {
        return s.len();
    };
    let label = &s[i + PEM_BEGIN.len()..];
    if label.contains(PEM_DASHES) || label.contains('\n') {
        s.len()
    } else {
        i
    }
}

/// Начало хвоста, который маска потока держит из-за незаконченного слова (N1): слово в
/// конце текста; слово перед завершающими пробельными символами, если среди них перевод
/// строки или это слово `Bearer`. Иначе — длина текста (держать нечего).
fn word_hold(s: &str) -> usize {
    let t = s.trim_end_matches(char::is_whitespace);
    let start = t
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let trailing = &s[t.len()..];
    if trailing.is_empty() || trailing.contains('\n') || t[start..].eq_ignore_ascii_case("bearer") {
        start
    } else {
        s.len()
    }
}

/// Начало частичного совпадения известного значения `n` в самом конце `b` (КМП по байтам
/// без пропускаемых [`skip_mask`]); полные совпадения находит [`known_spans`].
fn partial_tail(b: &[u8], skip: &[bool], n: &Needle) -> Option<usize> {
    let m = n.pat.len();
    if m == 0 {
        return None;
    }
    let mut ring = vec![0usize; m];
    let mut seen = 0usize;
    let mut k = 0usize;
    for (i, &c) in b.iter().enumerate() {
        if skip[i] {
            continue;
        }
        ring[seen % m] = i;
        seen += 1;
        while k > 0 && c != n.pat[k] {
            k = n.fail[k - 1];
        }
        if c == n.pat[k] {
            k += 1;
        }
        if k == m {
            k = n.fail[m - 1];
        }
    }
    (k > 0).then(|| ring[(seen - k) % m])
}

/// `s[..end]` с заменой слитых находок `spans` (все внутри `..end`) метками.
fn render(s: &str, spans: &[Span<'_>], end: usize) -> String {
    let mut out = String::with_capacity(end);
    let mut pos = 0;
    for sp in spans {
        out.push_str(&s[pos..sp.start]);
        out.push_str(MASK_PREFIX);
        out.push_str(sp.label);
        out.push(']');
        pos = sp.end;
    }
    out.push_str(&s[pos..end]);
    out
}

impl fmt::Debug for SecretMasker {
    /// Только имена известных значений: сами значения не печатаются.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut labels: Vec<&str> = self.known.iter().map(|n| n.label.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        f.debug_struct("SecretMasker")
            .field("known", &labels)
            .finish()
    }
}

/// Форма значения внутри строки JSON (как пишет `serde_json`).
fn json_escaped(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}

/// Пропускаемые при поиске известных значений байты: перевод строки (`\r`, `\n`) и
/// его форма в тексте JSON (`\n`, `\r` после нечётного числа обратных косых черт).
/// Возвращает по байтам текста: `true` — байт пропускается.
fn skip_mask(b: &[u8]) -> Vec<bool> {
    let mut skip = vec![false; b.len()];
    let mut slashes = 0usize;
    for (i, &c) in b.iter().enumerate() {
        if c == b'\r' || c == b'\n' {
            skip[i] = true;
        } else if (c == b'n' || c == b'r') && slashes % 2 == 1 {
            skip[i] = true;
            skip[i - 1] = true;
        }
        slashes = if c == b'\\' { slashes + 1 } else { 0 };
    }
    skip
}

/// Байты без пропускаемых [`skip_mask`].
fn normalized(b: &[u8]) -> Vec<u8> {
    let skip = skip_mask(b);
    b.iter()
        .zip(skip)
        .filter_map(|(c, s)| (!s).then_some(*c))
        .collect()
}

/// Таблица префикс-функции КМП.
fn kmp_table(p: &[u8]) -> Vec<usize> {
    let mut fail = vec![0usize; p.len()];
    let mut k = 0;
    for i in 1..p.len() {
        while k > 0 && p[i] != p[k] {
            k = fail[k - 1];
        }
        if p[i] == p[k] {
            k += 1;
        }
        fail[i] = k;
    }
    fail
}

/// Все вхождения значения `n` в `b` (и перекрывающиеся, и разрезанные переносом строки)
/// — один проход КМП по тексту без пропускаемых байт.
fn known_spans<'m>(
    b: &[u8],
    skip: &[bool],
    n: &'m Needle,
    out: &mut Vec<Span<'m>>,
    steps: &mut u64,
) {
    let m = n.pat.len();
    if m == 0 {
        return;
    }
    *steps += b.len() as u64;
    // Исходные позиции последних `m` учтённых байт: начало совпадения.
    let mut ring = vec![0usize; m];
    let mut seen = 0usize;
    let mut k = 0usize;
    for (i, &c) in b.iter().enumerate() {
        if skip[i] {
            continue;
        }
        ring[seen % m] = i;
        seen += 1;
        while k > 0 && c != n.pat[k] {
            k = n.fail[k - 1];
            *steps += 1;
        }
        if c == n.pat[k] {
            k += 1;
        }
        if k == m {
            let start = ring[(seen - m) % m];
            out.push(Span {
                start,
                end: i + 1,
                label: &n.label,
            });
            k = n.fail[k - 1];
        }
    }
}

/// Класс символов тела шаблона.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Alnum,
    Word,
    UpperDigit,
    AlnumDash,
    B64url,
    /// Токен после `Bearer `: `[A-Za-z0-9._~+/=-]`.
    Opaque,
}

const CLASSES: usize = 6;

impl Class {
    fn index(self) -> usize {
        self as usize
    }

    fn has(self, c: u8) -> bool {
        match self {
            Self::Alnum => c.is_ascii_alphanumeric(),
            Self::Word => is_word(c),
            Self::UpperDigit => c.is_ascii_uppercase() || c.is_ascii_digit(),
            Self::AlnumDash => c.is_ascii_alphanumeric() || c == b'-',
            Self::B64url => c.is_ascii_alphanumeric() || c == b'-' || c == b'_',
            Self::Opaque => {
                c.is_ascii_alphanumeric()
                    || matches!(c, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-')
            }
        }
    }
}

/// Шаблон токена: префиксы, вид, класс символов тела, длина тела и доп. проверка.
struct Rule {
    prefixes: &'static [&'static str],
    kind: &'static str,
    body: Class,
    min: usize,
    max: usize,
    /// После тела не должно идти символа слова (`AKIA…` ровно из 20 символов).
    right_boundary: bool,
    /// Проверка начала тела (не больше [`CHECK_WINDOW`] байт); `None` — не нужна. Должна
    /// быть монотонной: не прошло тело — не пройдёт и его хвост.
    check: Option<fn(&[u8]) -> bool>,
}

/// Шаблоны ключей и токенов D90 и распространённые форматы провайдеров. Каждый требует
/// границы слова слева: `myghp_value` и `task-runner` не находятся.
const RULES: &[Rule] = &[
    Rule {
        // OpenAI, Anthropic (`sk-ant-`), OpenRouter (`sk-or-`), DeepSeek и др.
        prefixes: &["sk-"],
        kind: "api_key",
        body: Class::B64url,
        min: 20,
        max: usize::MAX,
        right_boundary: false,
        check: Some(random_looking),
    },
    Rule {
        prefixes: &["sk_live_", "sk_test_", "rk_live_", "rk_test_"],
        kind: "stripe_key",
        body: Class::Alnum,
        min: 16,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        prefixes: &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"],
        kind: "github_token",
        body: Class::Alnum,
        min: 36,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        prefixes: &["github_pat_"],
        kind: "github_token",
        body: Class::Word,
        min: 22,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        prefixes: &["AKIA", "ASIA"],
        kind: "aws_access_key",
        body: Class::UpperDigit,
        min: 16,
        max: 16,
        right_boundary: true,
        check: None,
    },
    Rule {
        prefixes: &["xoxa-", "xoxb-", "xoxp-", "xoxo-", "xoxs-", "xoxr-"],
        kind: "slack_token",
        body: Class::AlnumDash,
        min: 10,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        prefixes: &["AIza"],
        kind: "google_api_key",
        body: Class::B64url,
        min: 35,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        prefixes: &["glpat-"],
        kind: "gitlab_token",
        body: Class::B64url,
        min: 20,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        prefixes: &["hf_"],
        kind: "huggingface_token",
        body: Class::Alnum,
        min: 30,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        prefixes: &["npm_"],
        kind: "npm_token",
        body: Class::Alnum,
        min: 36,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        // Groq.
        prefixes: &["gsk_"],
        kind: "groq_key",
        body: Class::Alnum,
        min: 40,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        // xAI.
        prefixes: &["xai-"],
        kind: "xai_key",
        body: Class::Alnum,
        min: 40,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
    Rule {
        // Макарон PyPI: `pypi-` и base64 заголовка `pypi.org`.
        prefixes: &["pypi-AgEIcHlwaS5vcmc"],
        kind: "pypi_token",
        body: Class::B64url,
        min: 50,
        max: usize::MAX,
        right_boundary: false,
        check: None,
    },
];

/// Индексы «мёртвых до» для особых правил после [`RULES`].
const JWT: usize = RULES.len();
const BEARER: usize = RULES.len() + 1;
const DEAD: usize = RULES.len() + 2;

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Тело `sk-` похоже на случайное (по первым [`CHECK_WINDOW`] байтам): есть цифра, не
/// меньше 32 букв и цифр и участок из 16 букв и цифр подряд. Имена вида
/// `sk-video-processing-toolkit` и `sk-video-0123456789abcdefgh` не маскируются;
/// ключи OpenAI (48), DeepSeek (32 hex), OpenRouter (64 hex), Anthropic (~90) — да.
fn random_looking(body: &[u8]) -> bool {
    let digit = body.iter().any(u8::is_ascii_digit);
    let alnum = body.iter().filter(|c| c.is_ascii_alphanumeric()).count();
    let run = body
        .split(|c| !c.is_ascii_alphanumeric())
        .map(<[u8]>::len)
        .max()
        .unwrap_or(0);
    digit && alnum >= 32 && run >= 16
}

/// Токен после `Bearer `: есть цифра (не слово прозы).
fn opaque_token(body: &[u8]) -> bool {
    body.iter().any(u8::is_ascii_digit)
}

/// Нечётное ли число обратных косых черт кончается в позиции `j` (включительно).
fn odd_slashes_ending_at(b: &[u8], j: usize) -> bool {
    let n = b[..=j].iter().rev().take_while(|c| **c == b'\\').count();
    n % 2 == 1
}

/// Перед `i` стоит экранирующая последовательность JSON: `\n`, `\t`, `\r`, `\b`, `\f`
/// или `\uXXXX` (I2).
fn escape_before(b: &[u8], i: usize) -> bool {
    if i >= 2 && matches!(b[i - 1], b'n' | b't' | b'r' | b'b' | b'f') && b[i - 2] == b'\\' {
        return odd_slashes_ending_at(b, i - 2);
    }
    i >= 6
        && b[i - 6] == b'\\'
        && b[i - 5] == b'u'
        && b[i - 4..i].iter().all(u8::is_ascii_hexdigit)
        && odd_slashes_ending_at(b, i - 6)
}

/// Граница слова слева от `i`: начало текста, не `[A-Za-z0-9_]` (не-ASCII — граница)
/// или экранирующая последовательность JSON.
fn boundary(b: &[u8], i: usize) -> bool {
    i == 0 || !is_word(b[i - 1]) || escape_before(b, i)
}

/// Длина перевода строки в `i`: `\n`, `\r\n`, а в тексте JSON — `\n`, `\r\n` экранированные.
fn line_break(b: &[u8], i: usize) -> usize {
    let r = &b[i..];
    if r.starts_with(b"\r\n") {
        2
    } else if r.starts_with(b"\n") {
        1
    } else if r.starts_with(b"\\r\\n") {
        4
    } else if r.starts_with(b"\\n") {
        2
    } else {
        0
    }
}

/// Проход шаблонов по тексту.
struct Scanner<'s, 'c> {
    s: &'s str,
    b: &'s [u8],
    steps: &'c mut u64,
    /// По классам: последняя посчитанная цепочка `start..end`.
    runs: [(usize, usize); CLASSES],
    /// По правилам: правило не подходит до этой позиции (его цепочка уже проверена).
    dead: [usize; DEAD],
}

impl<'s, 'c> Scanner<'s, 'c> {
    fn new(s: &'s str, steps: &'c mut u64) -> Self {
        Self {
            s,
            b: s.as_bytes(),
            steps,
            runs: [(0, 0); CLASSES],
            dead: [0; DEAD],
        }
    }

    /// Конец цепочки символов `class`, идущей с `from`: один подсчёт на цепочку.
    fn run_end(&mut self, class: Class, from: usize) -> usize {
        let (start, end) = self.runs[class.index()];
        if start <= from && from < end {
            return end;
        }
        let n = self.b[from..].iter().take_while(|c| class.has(**c)).count();
        *self.steps += n as u64 + 1;
        let end = from + n;
        if n > 0 {
            self.runs[class.index()] = (from, end);
        }
        end
    }

    fn run(mut self, out: &mut Vec<Span<'_>>) {
        let len = self.b.len();
        let mut i = 0;
        while i < len {
            *self.steps += 1;
            let c = self.b[i];
            if c == b':'
                && let Some((start, end)) = url_password(self.b, i)
            {
                out.push(Span {
                    start,
                    end,
                    label: "url_password",
                });
            }
            if c == b'-'
                && let Some(end) = pem_block(self.s, i)
            {
                *self.steps += (end - i) as u64;
                out.push(Span {
                    start: i,
                    end,
                    label: PRIVATE_KEY,
                });
                i = end;
                continue;
            }
            if c.is_ascii_alphabetic() && boundary(self.b, i) {
                if let Some((end, kind)) = self.token(i) {
                    out.push(Span {
                        start: i,
                        end,
                        label: kind,
                    });
                    i = end;
                    continue;
                }
                if let Some((start, end, kind)) = self.bearer(i) {
                    out.push(Span {
                        start,
                        end,
                        label: kind,
                    });
                    i = end;
                    continue;
                }
                if let Some((start, end)) = connection_password(self.b, i) {
                    *self.steps += (end - i) as u64;
                    out.push(Span {
                        start,
                        end,
                        label: "connection_password",
                    });
                    i = end;
                    continue;
                }
            }
            i += 1;
        }
    }

    /// Токен по [`RULES`] или JWT, начинающийся в `i`: (конец, вид).
    fn token(&mut self, i: usize) -> Option<(usize, &'static str)> {
        for (idx, r) in RULES.iter().enumerate() {
            if self.dead[idx] > i {
                continue;
            }
            let rest = &self.b[i..];
            let Some(p) = r.prefixes.iter().find(|p| rest.starts_with(p.as_bytes())) else {
                continue;
            };
            let from = i + p.len();
            let end = self.run_end(r.body, from);
            let len = end - from;
            let ok = len >= r.min
                && len <= r.max
                && !(r.right_boundary && self.b.get(end).is_some_and(|c| is_word(*c)))
                && r.check.is_none_or(|f| {
                    let w = &self.b[from..end.min(from + CHECK_WINDOW)];
                    *self.steps += w.len() as u64;
                    f(w)
                });
            if ok {
                return Some((self.wrapped_tail(r.body, end), r.kind));
            }
            // Хвост цепочки этому правилу тоже не подходит (проверки монотонны).
            self.dead[idx] = end;
        }
        self.jwt(i).map(|end| (end, "jwt"))
    }

    /// Хвост токена после переноса строки (m1): цепочка того же класса от
    /// [`MIN_WRAP_TAIL`] символов с буквой и цифрой, за которой конец текста, пробел,
    /// кавычка, `\` или запятая.
    fn wrapped_tail(&mut self, class: Class, mut end: usize) -> usize {
        for _ in 0..MAX_WRAPS {
            if end >= self.b.len() {
                break;
            }
            let br = line_break(self.b, end);
            if br == 0 {
                break;
            }
            let from = end + br;
            if from >= self.b.len() {
                break;
            }
            let tail_end = self.run_end(class, from);
            let tail = &self.b[from..tail_end];
            let after_ok = self.b.get(tail_end).is_none_or(|c| {
                c.is_ascii_whitespace() || matches!(c, b'"' | b'\'' | b'\\' | b',')
            });
            if tail.len() >= MIN_WRAP_TAIL
                && tail.iter().any(u8::is_ascii_digit)
                && tail.iter().any(u8::is_ascii_alphabetic)
                && after_ok
            {
                end = tail_end;
            } else {
                break;
            }
        }
        end
    }

    /// JWT: `eyJ…` (заголовок), `.`, `eyJ…` (данные), `.`, подпись (может быть пустой).
    fn jwt(&mut self, i: usize) -> Option<usize> {
        if self.dead[JWT] > i || !self.b[i..].starts_with(b"eyJ") {
            return None;
        }
        let h = self.run_end(Class::B64url, i);
        let ok_head =
            h - i >= 10 && self.b.get(h) == Some(&b'.') && self.b[h + 1..].starts_with(b"eyJ");
        if !ok_head {
            self.dead[JWT] = h;
            return None;
        }
        let p = self.run_end(Class::B64url, h + 1);
        if p - (h + 1) < 10 || self.b.get(p) != Some(&b'.') {
            self.dead[JWT] = h;
            return None;
        }
        Some(self.run_end(Class::B64url, p + 1))
    }

    /// Токен после `Bearer ` (без учёта регистра): от 20 символов `[A-Za-z0-9._~+/=-]`
    /// с цифрой. Участок — только токен; токен известного формата получает свой вид.
    fn bearer(&mut self, i: usize) -> Option<(usize, usize, &'static str)> {
        const KEY: &[u8] = b"bearer ";
        if self.b.len() - i < KEY.len() || !self.b[i..i + KEY.len()].eq_ignore_ascii_case(KEY) {
            return None;
        }
        let from = i + KEY.len();
        if from < self.b.len()
            && self.b[from].is_ascii_alphabetic()
            && let Some((end, kind)) = self.token(from)
        {
            return Some((from, end, kind));
        }
        if self.dead[BEARER] > from {
            return None;
        }
        let end = self.run_end(Class::Opaque, from);
        let w = &self.b[from..end.min(from + CHECK_WINDOW)];
        *self.steps += w.len() as u64;
        if end - from >= 20 && opaque_token(w) {
            Some((from, end, "bearer_token"))
        } else {
            self.dead[BEARER] = end;
            None
        }
    }
}

/// Блок приватного ключа PEM с `-----BEGIN … PRIVATE KEY-----` в `i`: конец блока —
/// после `-----END …-----`, без `END` — конец текста. Метка ищется не дальше 100 байт.
fn pem_block(s: &str, i: usize) -> Option<usize> {
    const BEGIN: &str = PEM_BEGIN;
    const END: &str = PEM_END;
    const DASHES: &str = PEM_DASHES;
    const LABEL_MAX: usize = PEM_LABEL_MAX;
    let rest = s[i..].strip_prefix(BEGIN)?;
    let head = &rest.as_bytes()[..rest.len().min(LABEL_MAX)];
    let h = head
        .windows(DASHES.len())
        .position(|w| w == DASHES.as_bytes())?;
    let label = &rest[..h];
    if label.contains('\n') || !label.contains("PRIVATE KEY") {
        return None;
    }
    let body = i + BEGIN.len() + h + DASHES.len();
    let Some(e) = s[body..].find(END) else {
        return Some(s.len());
    };
    let after = body + e + END.len();
    Some(
        s[after..]
            .find(DASHES)
            .map_or(s.len(), |f| after + f + DASHES.len()),
    )
}

/// Пароль в URL: `i` — двоеточие `://`; участок пароля в `scheme://user:pass@host`.
/// Участок адреса не содержит `/`, поэтому соседние `://` не просматриваются повторно.
fn url_password(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if !b[i..].starts_with(b"://") || i == 0 || !b[i - 1].is_ascii_alphanumeric() {
        return None;
    }
    let a = i + 3;
    let stop = |c: &u8| {
        matches!(
            c,
            b'/' | b'?' | b'#' | b'"' | b'\'' | b'<' | b'>' | b'`' | b'\\'
        ) || c.is_ascii_whitespace()
    };
    let e = a + b[a..].iter().position(stop).unwrap_or(b.len() - a);
    let at = a + b[a..e].iter().rposition(|c| *c == b'@')?;
    let colon = a + b[a..at].iter().position(|c| *c == b':')?;
    (colon + 1 < at).then_some((colon + 1, at))
}

/// Пароль строки подключения: `Password=…` или `Pwd=…` (без учёта регистра) после
/// начала текста, `;`, `?`, `&`, кавычки, пробела или экранирования JSON; значение — до
/// `;`, `&`, кавычки, `\` или пробела. Пустое значение и подстановки (`$X`, `%X%`, `{x}`,
/// `<x>`) не трогаются.
fn connection_password(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if i > 0
        && !matches!(b[i - 1], b';' | b'?' | b'&' | b'"' | b'\'')
        && !b[i - 1].is_ascii_whitespace()
        && !escape_before(b, i)
    {
        return None;
    }
    let key = [&b"password="[..], &b"pwd="[..]]
        .into_iter()
        .find(|k| b.len() - i >= k.len() && b[i..i + k.len()].eq_ignore_ascii_case(k))?;
    let start = i + key.len();
    // Заглушка или пустое значение — до просмотра значения: отказ стоит O(1) (I1).
    if b.get(start).is_none_or(|c| {
        matches!(
            c,
            b'$' | b'%' | b'{' | b'<' | b';' | b'&' | b'"' | b'\'' | b'\\'
        ) || c.is_ascii_whitespace()
    }) || b[start..].starts_with(MASK_PREFIX.as_bytes())
    {
        return None;
    }
    let end = start
        + b[start..]
            .iter()
            .position(|c| {
                matches!(c, b';' | b'&' | b'"' | b'\'' | b'\\') || c.is_ascii_whitespace()
            })
            .unwrap_or(b.len() - start);
    Some((start, end))
}
