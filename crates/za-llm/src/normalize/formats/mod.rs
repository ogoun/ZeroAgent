//! Текстовые форматы вызовов инструментов (MDL-005, шаг 2).
//!
//! Каждый формат — функция `text → Extraction`: найденные вызовы и текст без их
//! разметки. Разбор без regex: поиск подстрок и ручной сканер.
//!
//! Общие правила форматов (ревью задачи 7, решения контроллера):
//! - **Линейность.** Каждый маркер ищется от текущей позиции не больше одного раза
//!   на вхождение ([`Finder`]): повтор маркера на мегабайты (зацикленная модель)
//!   разбирается за O(n).
//! - **Код Markdown** форматы не видят: конвейер заменяет его заглушками до
//!   разбора и возвращает после (`normalize::code`).
//! - **Признак вызова.** Маркер без валидного продолжения — обычный текст: не
//!   вырезается и не даёт отклонения. Битая или обрезанная разметка, начало которой
//!   похоже на настоящий вызов, вырезается и возвращается вызовом с пустым `name` и
//!   `RawArgs::Text` с исходной разметкой — конвейер превращает его в
//!   `CallRejection::Unparseable`.
//!
//! - **Форматы без маркеров** (`Pythonic`, `Llama3Json`) разбирают только весь
//!   текст целиком (как парсеры vLLM) и извлекают вызовы, только если хотя бы одно
//!   имя принято `accept_name` (D28, D39); иначе текст не меняется.
//!
//! P0 (задача 7): Hermes, Qwen XML, Harmony. P1 (задача 8): DSML, Gemma 4,
//! Mistral `[TOOL_CALLS]`, pythonic, llama3_json.

pub(crate) mod dsml;
pub(crate) mod gemma4;
mod harmony;
mod hermes;
mod llama3_json;
mod mistral;
mod pythonic;
mod qwen_xml;

pub(crate) use mistral::bodies as mistral_bodies;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::caps::TextFormat;

/// Аргументы вызова в том виде, в каком их дал формат.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawArgs {
    /// Уже разобранный JSON.
    Json(Value),
    /// Строка, которую надо разобрать как JSON.
    Text(String),
    /// Пары ключ — строковое значение (Qwen XML); типы приводит схема.
    Pairs(Vec<(String, String)>),
}

/// Вызов, найденный в тексте. Пустой `name` — нераспознанная разметка: в `args`
/// лежит `RawArgs::Text` с её исходным текстом.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawCall {
    pub name: String,
    pub args: RawArgs,
}

impl RawCall {
    /// Нераспознанная разметка вызова.
    pub(crate) fn broken(raw: &str) -> Self {
        Self {
            name: String::new(),
            args: RawArgs::Text(raw.to_owned()),
        }
    }
}

/// Итог разбора одного формата.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Extraction {
    pub calls: Vec<RawCall>,
    /// Текст без разметки вызовов; без находок — исходный текст без изменений.
    pub remaining: String,
}

impl Extraction {
    fn unchanged(text: &str) -> Self {
        Self {
            calls: Vec::new(),
            remaining: text.to_owned(),
        }
    }
}

/// accept_name учитывается форматами без маркеров (Pythonic, Llama3Json): при false
/// вызов не извлекается, а remaining == text без изменений (D28, D39).
///
/// Уточнение раунда исправлений 1 (I2): Qwen XML тоже требует принятого имени —
/// `<function=ИМЯ>` с неизвестным и неисправимым именем считается текстом; Harmony —
/// для обрезанного заголовка без `<|message|>`. DSML, Gemma 4 и Mistral (задача 8) —
/// для обрезанного вызова: без принятого имени он остаётся текстом.
pub(crate) fn extract(
    format: TextFormat,
    text: &str,
    accept_name: &dyn Fn(&str) -> bool,
) -> Extraction {
    match format {
        TextFormat::Hermes => hermes::extract(text),
        TextFormat::QwenXml => qwen_xml::extract(text, accept_name),
        TextFormat::Harmony => harmony::extract(text, accept_name),
        TextFormat::Dsml => dsml::extract(text, accept_name),
        TextFormat::Gemma4 => gemma4::extract(text, accept_name),
        TextFormat::MistralToolCalls => mistral::extract(text, accept_name),
        TextFormat::Pythonic => pythonic::extract(text, accept_name),
        TextFormat::Llama3Json => llama3_json::extract(text, accept_name),
    }
}

/// Ошибка разбора значения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParseError {
    /// Текст кончился посреди значения (обрыв).
    Eof,
    /// Синтаксическая ошибка в этой байтовой позиции.
    Syntax(usize),
}

/// Одно JSON-значение, начинающееся в `from` (пробелы перед ним допускаются):
/// (значение, конец значения). Текст после значения не читается.
pub(crate) fn json_at<T: DeserializeOwned>(
    text: &str,
    from: usize,
) -> Result<(T, usize), ParseError> {
    let slice = &text[from..];
    let mut stream = serde_json::Deserializer::from_str(slice).into_iter::<T>();
    match stream.next() {
        Some(Ok(v)) => Ok((v, from + stream.byte_offset())),
        Some(Err(e)) if e.is_eof() => Err(ParseError::Eof),
        Some(Err(e)) => Err(ParseError::Syntax(from + error_offset(slice, &e))),
        None => Err(ParseError::Eof),
    }
}

/// Байтовая позиция ошибки `serde_json` (строка и столбец в байтах) в `slice`.
fn error_offset(slice: &str, e: &serde_json::Error) -> usize {
    let line_start = if e.line() <= 1 {
        0
    } else {
        slice
            .match_indices('\n')
            .nth(e.line() - 2)
            .map_or(slice.len(), |(i, _)| i + 1)
    };
    let mut at = (line_start + e.column()).min(slice.len());
    while !slice.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Символ имени инструмента в форматах, где имя не ограничено кавычками.
pub(crate) fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// Длина имени инструмента (в байтах), начинающегося в `from`.
pub(crate) fn name_len(text: &str, from: usize) -> usize {
    let rest = &text[from..];
    rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len())
}

/// Позиция первого непробельного символа не раньше `from`.
pub(crate) fn skip_ws(text: &str, from: usize) -> usize {
    text.len() - text[from..].trim_start().len()
}

/// Текст без вырезанных диапазонов `spans` (отсортированы, не пересекаются).
///
/// Пробельные символы по краям выреза схлопываются: между оставшимися кусками
/// ставится `"\n\n"`, если хотя бы с одного края выреза стояло два и более перевода
/// строки, `"\n"` — если один, `" "` — если были только пробелы, иначе ничего. Края всего текста не трогаются,
/// кроме пробелов, примыкающих к вырезу. Без вырезов — исходный текст.
pub(crate) fn splice(text: &str, spans: &[(usize, usize)]) -> String {
    if spans.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    // Пробелы, снятые с краёв вырезов с момента последнего непустого куска.
    let mut gap: Vec<&str> = Vec::new();
    let mut pos = 0;
    for k in 0..=spans.len() {
        let end = spans.get(k).map_or(text.len(), |s| s.0);
        let mut seg = &text[pos..end];
        if k > 0 {
            let t = seg.trim_start();
            gap.push(&seg[..seg.len() - t.len()]);
            seg = t;
        }
        let mut trail = "";
        if k < spans.len() {
            let t = seg.trim_end();
            trail = &seg[t.len()..];
            seg = t;
        }
        if !seg.is_empty() {
            if !out.is_empty() {
                out.push_str(separator(&gap));
            }
            out.push_str(seg);
            gap.clear();
        }
        gap.push(trail);
        if let Some(s) = spans.get(k) {
            pos = s.1;
        }
    }
    out
}

fn separator(gap: &[&str]) -> &'static str {
    let newlines = gap
        .iter()
        .map(|g| g.matches('\n').count())
        .max()
        .unwrap_or(0);
    match newlines {
        0 if gap.iter().all(|g| g.is_empty()) => "",
        0 => " ",
        1 => "\n",
        _ => "\n\n",
    }
}

/// Позиция `needle` в `text` начиная с `from`.
pub(crate) fn find_from(text: &str, from: usize, needle: &str) -> Option<usize> {
    text.get(from..)?.find(needle).map(|i| from + i)
}

/// Поиск маркера с запоминанием: пока запрос не ушёл дальше найденного вхождения,
/// поиск не повторяется; «не найдено» запоминается до конца текста. При
/// неубывающих `from` суммарная работа — один проход по тексту.
pub(crate) struct Finder<'a> {
    text: &'a str,
    needle: &'static str,
    /// (откуда искали, что нашли).
    cached: Option<(usize, Option<usize>)>,
}

impl<'a> Finder<'a> {
    pub(crate) fn new(text: &'a str, needle: &'static str) -> Self {
        Self {
            text,
            needle,
            cached: None,
        }
    }

    pub(crate) fn needle(&self) -> &'static str {
        self.needle
    }

    /// Первое вхождение не раньше `from`.
    pub(crate) fn find(&mut self, from: usize) -> Option<usize> {
        if let Some((searched, found)) = self.cached
            && searched <= from
        {
            match found {
                None => return None,
                Some(p) if p >= from => return Some(p),
                Some(_) => {}
            }
        }
        let found = find_from(self.text, from, self.needle);
        self.cached = Some((from, found));
        found
    }
}

/// Ближайший из маркеров не раньше `from`: (позиция, маркер).
pub(crate) fn earliest(finders: &mut [Finder<'_>], from: usize) -> Option<(usize, &'static str)> {
    finders
        .iter_mut()
        .filter_map(|f| f.find(from).map(|p| (p, f.needle())))
        .min_by_key(|(p, _)| *p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_joins_with_whitespace_kind() {
        assert_eq!(splice("a\n[x]\nb", &[(2, 5)]), "a\nb");
        assert_eq!(splice("a [x] b", &[(2, 5)]), "a b");
        assert_eq!(splice("a[x]b", &[(1, 4)]), "ab");
        assert_eq!(splice("a\n\n[x]\n\nb", &[(3, 6)]), "a\n\nb");
        assert_eq!(splice("[x]", &[(0, 3)]), "");
        assert_eq!(splice("[x] [y] tail ", &[(0, 3), (4, 7)]), "tail ");
        assert_eq!(splice(" lead [x]", &[(6, 9)]), " lead");
        assert_eq!(splice("same", &[]), "same");
    }
}
