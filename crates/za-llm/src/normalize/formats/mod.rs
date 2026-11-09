//! Текстовые форматы вызовов инструментов (MDL-005, шаг 2).
//!
//! Каждый формат — функция `text → Extraction`: найденные вызовы и текст без их
//! разметки. Разбор без regex: поиск подстрок и ручной сканер. Битая или обрезанная
//! разметка не роняет разбор: она вырезается из текста и возвращается вызовом с
//! пустым `name` и `RawArgs::Text` с исходной разметкой — конвейер превращает его
//! в `CallRejection::Unparseable`.
//!
//! P0 (задача 7): Hermes, Qwen XML, Harmony. P1 (`Dsml`, `Gemma4`,
//! `MistralToolCalls`, `Pythonic`, `Llama3Json`) — задача 8; до неё `extract`
//! возвращает текст без изменений.

mod harmony;
mod hermes;
mod qwen_xml;

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

/// accept_name учитывается только форматами без маркеров (Pythonic, Llama3Json): при false
/// вызов не извлекается, а remaining == text без изменений (D28, D39). Прочие форматы его игнорируют.
pub(crate) fn extract(
    format: TextFormat,
    text: &str,
    accept_name: &dyn Fn(&str) -> bool,
) -> Extraction {
    match format {
        TextFormat::Hermes => hermes::extract(text),
        TextFormat::QwenXml => qwen_xml::extract(text),
        TextFormat::Harmony => harmony::extract(text),
        // P1 — задача 8.
        TextFormat::Dsml
        | TextFormat::Gemma4
        | TextFormat::MistralToolCalls
        | TextFormat::Pythonic
        | TextFormat::Llama3Json => {
            let _ = accept_name;
            Extraction::unchanged(text)
        }
    }
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
