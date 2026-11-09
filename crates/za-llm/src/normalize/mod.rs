//! Нормализация вызовов инструментов (MDL-005, MDL-006; D5, D27, D28, D39).
//!
//! Чистая функция над итоговым сообщением модели: без ввода-вывода и времени.
//! Порядок MDL-005:
//! 1. Нативные `Part::ToolCall`.
//! 2. Всегда (D39) — скан `Text`, затем `Reasoning` по текстовым форматам; найденная
//!    разметка вырезается из `text` и `reasoning`. Извлечённый вызов с тем же именем и
//!    теми же каноническими аргументами, что у нативного, отбрасывается как дубль;
//!    вызов из рассуждения, совпавший с принятым из текста, — тоже. Блоки кода
//!    Markdown не сканируются; извлекается не больше [`MAX_EXTRACTED_CALLS`] вызовов.
//! 3. Аргументы к канону: строка разбирается как JSON, пары — объект строк.
//! 4. Имя (D28 с порогом по длине): единственный кандидат с расстоянием
//!    ≤ min(2, длина / 4) при длине имени ≥ 5 исправляет имя.
//! 5. Схема и приведение типов (D5, [`validate_and_coerce`]).
//! 6. Проверки политики — R1-C.

mod code;
mod formats;
mod names;
mod schema;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use za_protocol::{EffectClass, canonical_json};

use crate::caps::TextFormat;
use crate::prefix::ToolSchema;
use crate::types::{Message, Part, ToolCall};

use formats::{RawArgs, RawCall};
use names::{NameMatch, is_known_or_fixable, resolve_name};

pub use names::damerau_levenshtein;
pub use schema::{JsonType, SchemaError, validate_and_coerce};

/// Откуда взят вызов.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallSource {
    /// Структурный вызов провайдера.
    Native,
    /// Извлечён из текста ответа.
    Content,
    /// Извлечён из рассуждения.
    Reasoning,
}

/// Исправление, сделанное нормализацией.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repair {
    NameCorrected {
        from: String,
        to: String,
    },
    /// Аргументы пришли строкой и разобраны как JSON.
    ArgsParsedFromString,
    TypeCoerced {
        path: String,
        to: JsonType,
    },
    WrappedInArray {
        path: String,
    },
    NullOptionalRemoved {
        path: String,
    },
    ExtractedFromText {
        format: TextFormat,
    },
}

/// Причина отказа принять вызов.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallRejection {
    /// Имя не найдено и не исправимо однозначно; `nearest` — до трёх ближайших.
    UnknownTool { name: String, nearest: Vec<String> },
    InvalidArgs {
        name: String,
        errors: Vec<SchemaError>,
    },
    /// Разметка или аргументы не разбираются; `raw` — исходный текст.
    Unparseable { raw: String },
    /// Извлечено больше [`MAX_EXTRACTED_CALLS`] вызовов из текста и рассуждения:
    /// разобраны первые `limit`, `skipped` отброшено (зацикленная модель). Добавлено
    /// в раунде исправлений 1 (решение контроллера).
    TooManyCalls { limit: u32, skipped: u32 },
}

/// Предел извлечённых из текста и рассуждения вызовов на сообщение; сверх него —
/// одно отклонение [`CallRejection::TooManyCalls`].
pub const MAX_EXTRACTED_CALLS: u32 = 64;

/// Принятый вызов.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedCall {
    pub call: ToolCall,
    pub source: CallSource,
    pub repairs: Vec<Repair>,
}

/// Итог нормализации сообщения.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NormalizeOutcome {
    /// Текст ответа без разметки вызовов.
    pub text: String,
    /// Рассуждение без разметки вызовов.
    pub reasoning: String,
    /// Принятые вызовы: нативные, затем из текста, затем из рассуждения.
    pub calls: Vec<NormalizedCall>,
    pub rejected: Vec<CallRejection>,
    /// Извлечённые из текста дубли нативных вызовов (D39), а также вызовы из
    /// рассуждения, совпавшие с принятыми из текста.
    pub dropped_duplicates: u32,
}

/// Все текстовые форматы в порядке разбора: с маркерами, затем без маркеров.
pub fn all_formats() -> &'static [TextFormat] {
    &[
        TextFormat::Hermes,
        TextFormat::QwenXml,
        TextFormat::Harmony,
        TextFormat::Dsml,
        TextFormat::Gemma4,
        TextFormat::MistralToolCalls,
        TextFormat::Pythonic,
        TextFormat::Llama3Json,
    ]
}

/// Исполнять ли вызов нельзя без подтверждения (D27, MDL-006): извлечённый из
/// текста или рассуждения вызов инструмента, меняющего состояние. Отказ и nudge
/// делает цикл R1-B по классу эффекта из реестра инструментов.
pub fn extracted_mutation_blocked(source: CallSource, class: EffectClass) -> bool {
    source != CallSource::Native && !matches!(class, EffectClass::Pure | EffectClass::ReadOnly)
}

/// Нормализует сообщение модели. `formats` — текстовые форматы для скана; пустой
/// срез — [`all_formats`] (решение контроллера: скан по D39 выключить нельзя).
pub fn normalize(msg: &Message, tools: &[ToolSchema], formats: &[TextFormat]) -> NormalizeOutcome {
    let formats = if formats.is_empty() {
        all_formats()
    } else {
        formats
    };
    let known: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut natives = Vec::new();
    for part in &msg.parts {
        match part {
            Part::Text(t) => join_part(&mut text, t),
            Part::Reasoning { text: r, .. } => join_part(&mut reasoning, r),
            Part::ToolCall(c) => natives.push(c),
            Part::ToolResult { .. } => {}
        }
    }

    let mut out = NormalizeOutcome::default();
    let mut ids = Ids::default();
    // Ключи нативных вызовов: исходные и после нормализации (D39).
    let mut native_keys: HashSet<String> = HashSet::new();

    // 1. Нативные вызовы.
    for c in natives {
        if !c.arguments.is_null() {
            native_keys.insert(call_key(&c.name, &c.arguments));
        }
        let mut repairs = Vec::new();
        let args = match native_args(c, &mut repairs) {
            Ok(a) => a,
            Err(rejection) => {
                reject(&mut out, rejection);
                continue;
            }
        };
        native_keys.insert(call_key(&c.name, &args));
        match finish(&c.name, args, repairs, tools, &known) {
            Ok((name, args, repairs)) => {
                native_keys.insert(call_key(&name, &args));
                let id = if c.id.is_empty() {
                    ids.derive(&name, &args)
                } else {
                    ids.reserve(&c.id);
                    c.id.clone()
                };
                out.calls.push(NormalizedCall {
                    call: ToolCall {
                        id,
                        name,
                        arguments: args,
                        raw_arguments: c.raw_arguments.clone(),
                    },
                    source: CallSource::Native,
                    repairs,
                });
            }
            Err(rejection) => reject(&mut out, rejection),
        }
    }

    // 2. Скан текста и рассуждения (D39).
    // Кэш по имени в пределах одного вызова: повтор одного имени на мегабайты не
    // пересчитывает расстояния (Minor A раунда 2).
    let accepted: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
    let accept_name = |n: &str| {
        if let Some(&hit) = accepted.borrow().get(n) {
            return hit;
        }
        let ok = is_known_or_fixable(n, &known);
        accepted.borrow_mut().insert(n.to_owned(), ok);
        ok
    };
    let (text, from_text) = scan(&text, formats, &accept_name);
    let (reasoning, from_reasoning) = scan(&reasoning, formats, &accept_name);
    out.text = text;
    out.reasoning = reasoning;

    // Ключи вызовов, принятых из текста: тот же вызов в рассуждении — дубль (I3).
    let mut content_keys: HashSet<String> = HashSet::new();
    let mut processed = 0u32;
    let mut skipped = 0u32;
    for (source, found) in [
        (CallSource::Content, from_text),
        (CallSource::Reasoning, from_reasoning),
    ] {
        for (format, raw) in found {
            if processed >= MAX_EXTRACTED_CALLS {
                skipped += 1;
                continue;
            }
            processed += 1;
            let is_duplicate = |key: &str| {
                native_keys.contains(key)
                    || (source == CallSource::Reasoning && content_keys.contains(key))
            };
            let raw_key = match &raw.args {
                RawArgs::Json(v) => Some(call_key(&raw.name, v)),
                _ => None,
            };
            if raw_key.as_deref().is_some_and(is_duplicate) {
                out.dropped_duplicates += 1;
                continue;
            }
            let mut repairs = vec![Repair::ExtractedFromText { format }];
            let raw_arguments = match &raw.args {
                RawArgs::Text(s) => Some(s.clone()),
                _ => None,
            };
            let args = match extracted_args(&raw, &mut repairs) {
                Ok(a) => a,
                Err(rejection) => {
                    reject(&mut out, rejection);
                    continue;
                }
            };
            match finish(&raw.name, args, repairs, tools, &known) {
                Ok((name, args, repairs)) => {
                    let key = call_key(&name, &args);
                    if is_duplicate(&key) {
                        out.dropped_duplicates += 1;
                        continue;
                    }
                    if source == CallSource::Content {
                        content_keys.insert(key);
                        content_keys.extend(raw_key);
                    }
                    let id = ids.derive(&name, &args);
                    out.calls.push(NormalizedCall {
                        call: ToolCall {
                            id,
                            name,
                            arguments: args,
                            raw_arguments,
                        },
                        source,
                        repairs,
                    });
                }
                Err(rejection) => reject(&mut out, rejection),
            }
        }
    }
    if skipped > 0 {
        reject(
            &mut out,
            CallRejection::TooManyCalls {
                limit: MAX_EXTRACTED_CALLS,
                skipped,
            },
        );
    }
    out
}

/// Склейка частей одного вида: между непустыми частями без пробела на стыке —
/// перевод строки, чтобы слова соседних блоков не слиплись.
fn join_part(acc: &mut String, part: &str) {
    if !acc.is_empty()
        && !part.is_empty()
        && !acc.ends_with(char::is_whitespace)
        && !part.starts_with(char::is_whitespace)
    {
        acc.push('\n');
    }
    acc.push_str(part);
}

fn reject(out: &mut NormalizeOutcome, rejection: CallRejection) {
    tracing::debug!(?rejection, "вызов инструмента отклонён нормализацией");
    out.rejected.push(rejection);
}

/// Последовательный разбор текста форматами: каждый следующий видит текст без
/// разметки предыдущих. Код Markdown заменён заглушками до разбора ([`code`]) и
/// возвращается в оставшийся текст и аргументы вызовов после.
fn scan(
    text: &str,
    formats: &[TextFormat],
    accept_name: &dyn Fn(&str) -> bool,
) -> (String, Vec<(TextFormat, RawCall)>) {
    let masked = code::Masked::new(text);
    let mut remaining = masked.text.clone();
    let mut found = Vec::new();
    for &format in formats {
        if remaining.is_empty() {
            break;
        }
        let e = formats::extract(format, &remaining, accept_name);
        found.extend(e.calls.into_iter().map(|c| (format, c)));
        remaining = e.remaining;
    }
    let found = found
        .into_iter()
        .map(|(f, c)| (f, masked.restore_call(c)))
        .collect();
    (masked.restore(&remaining), found)
}

/// Шаг 3 для нативного вызова: `Null` с исходной строкой — попытка разбора;
/// строка вместо объекта (двойное кодирование) — разбор строки.
fn native_args(c: &ToolCall, repairs: &mut Vec<Repair>) -> Result<Value, CallRejection> {
    match (&c.arguments, &c.raw_arguments) {
        (Value::Null, Some(raw)) => parse_args_text(raw, repairs),
        (Value::Null, None) => Ok(Value::Object(Map::new())),
        (Value::String(s), _) => parse_args_text(s, repairs),
        (v, _) => Ok(v.clone()),
    }
}

/// Шаг 3 для извлечённого вызова.
fn extracted_args(raw: &RawCall, repairs: &mut Vec<Repair>) -> Result<Value, CallRejection> {
    match &raw.args {
        // Пустое имя — нераспознанная разметка (formats).
        RawArgs::Text(s) if raw.name.is_empty() => {
            Err(CallRejection::Unparseable { raw: s.clone() })
        }
        RawArgs::Text(s) => parse_args_text(s, repairs),
        RawArgs::Json(_) | RawArgs::Pairs(_) if raw.name.is_empty() => {
            Err(CallRejection::Unparseable {
                raw: serde_json::to_string(&raw.args).unwrap_or_default(),
            })
        }
        RawArgs::Json(v) => Ok(v.clone()),
        RawArgs::Pairs(pairs) => Ok(Value::Object(
            pairs
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        )),
    }
}

/// Строка аргументов как JSON (`ArgsParsedFromString`). Пустая строка — `{}`;
/// обёртка Markdown-блока кода снимается.
fn parse_args_text(raw: &str, repairs: &mut Vec<Repair>) -> Result<Value, CallRejection> {
    let s = strip_code_fence(raw.trim());
    if s.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    match serde_json::from_str::<Value>(s) {
        Ok(v) => {
            repairs.push(Repair::ArgsParsedFromString);
            Ok(v)
        }
        Err(_) => Err(CallRejection::Unparseable {
            raw: raw.to_owned(),
        }),
    }
}

fn strip_code_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    let Some(inner) = rest.strip_suffix("```") else {
        return s;
    };
    // Язык блока (```json) — до конца первой строки.
    let inner = match inner.find('\n') {
        Some(nl) if inner[..nl].chars().all(|c| c.is_ascii_alphanumeric()) => &inner[nl + 1..],
        _ => inner,
    };
    inner.trim()
}

/// Шаги 4 и 5: имя (D28) и схема с приведением (D5).
fn finish(
    name: &str,
    mut args: Value,
    mut repairs: Vec<Repair>,
    tools: &[ToolSchema],
    known: &[&str],
) -> Result<(String, Value, Vec<Repair>), CallRejection> {
    let name = match resolve_name(name, known) {
        NameMatch::Exact => name.to_owned(),
        NameMatch::Corrected(to) => {
            repairs.push(Repair::NameCorrected {
                from: name.to_owned(),
                to: to.clone(),
            });
            to
        }
        NameMatch::Unknown(nearest) => {
            return Err(CallRejection::UnknownTool {
                name: name.to_owned(),
                nearest,
            });
        }
    };
    let Some(tool) = tools.iter().find(|t| t.name == name) else {
        return Err(CallRejection::UnknownTool {
            name,
            nearest: Vec::new(),
        });
    };
    match validate_and_coerce(&tool.parameters, &mut args) {
        Ok(more) => {
            repairs.extend(more);
            Ok((name, args, repairs))
        }
        Err(errors) => Err(CallRejection::InvalidArgs { name, errors }),
    }
}

/// Ключ сравнения вызовов: имя и канонический JSON аргументов.
fn call_key(name: &str, args: &Value) -> String {
    format!("{name}\0{}", canonical_json(args))
}

/// Детерминированные id вызовов без id (CORE-007): `call_<blake3(name + args)[..12]>`.
/// Совпадение с уже выданным id (тот же вызов дважды) разводится номером в хэше.
#[derive(Default)]
struct Ids {
    used: HashSet<String>,
}

impl Ids {
    fn reserve(&mut self, id: &str) {
        self.used.insert(id.to_owned());
    }

    fn derive(&mut self, name: &str, args: &Value) -> String {
        let canonical = canonical_json(args);
        let mut n = 0u32;
        loop {
            let mut h = blake3::Hasher::new();
            h.update(name.as_bytes());
            h.update(canonical.as_bytes());
            if n > 0 {
                h.update(format!("#{n}").as_bytes());
            }
            let hex = h.finalize().to_hex();
            let id = format!("call_{}", &hex.as_str()[..12]);
            if self.used.insert(id.clone()) {
                return id;
            }
            n += 1;
        }
    }
}
