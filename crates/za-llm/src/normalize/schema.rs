//! Подмножественная проверка JSON Schema с приведением типов (D5, MDL-005 шаг 5).
//!
//! Поддерживаются: `type` (в том числе массив типов), `properties`, `required`,
//! `additionalProperties`, `items`, `enum`, `const`, `minimum`/`maximum`,
//! `minLength`/`maxLength`, `minItems`/`maxItems`, `anyOf`/`oneOf`, локальные `$ref`
//! на `#/$defs/…` и `#/definitions/…`. Прочие ключевые слова игнорируются с `debug`.
//!
//! Приведение (только при несовпадении типа): строка-число → `integer`/`number`,
//! `"true"`/`"false"` → `boolean`, строка с JSON → `object`/`array`, скаляр при
//! `type: array` → `[скаляр]`; `null` у необязательного свойства, схема которого
//! `null` не допускает, удаляется.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::Repair;

/// Тип JSON Schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonType {
    Null,
    Boolean,
    Integer,
    Number,
    String,
    Array,
    Object,
}

impl JsonType {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "null" => Self::Null,
            "boolean" => Self::Boolean,
            "integer" => Self::Integer,
            "number" => Self::Number,
            "string" => Self::String,
            "array" => Self::Array,
            "object" => Self::Object,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean => "boolean",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::String => "string",
            Self::Array => "array",
            Self::Object => "object",
        }
    }

    fn matches(self, v: &Value) -> bool {
        match self {
            Self::Null => v.is_null(),
            Self::Boolean => v.is_boolean(),
            Self::Integer => is_integer(v),
            Self::Number => v.is_number(),
            Self::String => v.is_string(),
            Self::Array => v.is_array(),
            Self::Object => v.is_object(),
        }
    }
}

/// Ошибка проверки: `path` — JSON Pointer значения (`""` — корень). `message` —
/// на английском: текст уходит модели в результате вызова (R1-B1 D23).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaError {
    pub path: String,
    pub message: String,
}

/// Предел вложенности схемы и цепочек `$ref` (защита от циклов).
const MAX_DEPTH: usize = 64;

/// Ключевые слова, которые проверка понимает или которые не влияют на валидность.
const KNOWN: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "anyOf",
    "oneOf",
    "$ref",
    "$defs",
    "definitions",
    "$schema",
    "$id",
    "$comment",
    "title",
    "description",
    "default",
    "examples",
];

/// Проверяет `args` по `schema` и приводит типы. При успехе `args` заменяется
/// приведённым значением и возвращаются сделанные исправления; при ошибке `args`
/// не меняется.
pub fn validate_and_coerce(
    schema: &Value,
    args: &mut Value,
) -> Result<Vec<Repair>, Vec<SchemaError>> {
    let mut work = args.clone();
    let mut cx = Cx::new(schema);
    cx.check(schema, &mut work, "", 0);
    if cx.errors.is_empty() {
        *args = work;
        Ok(cx.repairs)
    } else {
        Err(cx.errors)
    }
}

struct Cx<'a> {
    root: &'a Value,
    repairs: Vec<Repair>,
    errors: Vec<SchemaError>,
}

impl<'a> Cx<'a> {
    fn new(root: &'a Value) -> Self {
        Self {
            root,
            repairs: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn error(&mut self, path: &str, message: impl Into<String>) {
        self.errors.push(SchemaError {
            path: path.to_owned(),
            message: message.into(),
        });
    }

    /// Проверка ветки на копии значения: (ошибки, исправления, приведённое значение).
    fn trial(
        &self,
        schema: &Value,
        value: &Value,
        path: &str,
        depth: usize,
    ) -> (bool, Vec<Repair>, Value) {
        let mut sub = Cx::new(self.root);
        let mut v = value.clone();
        sub.check(schema, &mut v, path, depth);
        (sub.errors.is_empty(), sub.repairs, v)
    }

    fn check(&mut self, schema: &Value, value: &mut Value, path: &str, depth: usize) {
        if depth > MAX_DEPTH {
            self.error(path, "schema too deep or contains a $ref cycle");
            return;
        }
        let s = match schema {
            Value::Bool(true) => return,
            Value::Bool(false) => {
                self.error(path, "schema allows no value");
                return;
            }
            Value::Object(s) => s,
            _ => return,
        };
        for key in s.keys() {
            if !KNOWN.contains(&key.as_str()) {
                tracing::debug!(keyword = %key, path, "ключевое слово JSON Schema не поддерживается, пропущено");
            }
        }

        if let Some(r) = s.get("$ref").and_then(Value::as_str) {
            match resolve_ref(self.root, r) {
                Some(target) => self.check(target, value, path, depth + 1),
                None => tracing::debug!(
                    r#ref = r,
                    path,
                    "нелокальный или неразрешимый $ref пропущен"
                ),
            }
        }

        if let Some(t) = s.get("type") {
            let types = parse_types(t);
            if !types.is_empty() && !types.iter().any(|t| t.matches(value)) {
                match coerce(value, &types, path) {
                    Some((v, repair)) => {
                        *value = v;
                        self.repairs.push(repair);
                    }
                    None => {
                        let names: Vec<&str> = types.iter().map(|t| t.as_str()).collect();
                        self.error(
                            path,
                            format!(
                                "expected type {}, got {}",
                                names.join(" | "),
                                type_name(value)
                            ),
                        );
                        return;
                    }
                }
            }
        }

        if !s.contains_key("type") {
            self.coerce_to_constants(s, value, path);
        }

        if let Some(Value::Array(options)) = s.get("enum")
            && !options.iter().any(|o| json_eq(o, value))
        {
            self.error(
                path,
                format!("value is not one of enum {}", Value::Array(options.clone())),
            );
        }
        if let Some(c) = s.get("const")
            && !json_eq(c, value)
        {
            self.error(path, format!("expected constant {c}"));
        }

        self.check_number(s, value, path);
        self.check_string(s, value, path);
        self.check_array(s, value, path, depth);
        self.check_object(s, value, path, depth);

        if let Some(Value::Array(branches)) = s.get("anyOf") {
            self.check_branches(branches, value, path, depth, false);
        }
        if let Some(Value::Array(branches)) = s.get("oneOf") {
            self.check_branches(branches, value, path, depth, true);
        }
    }

    /// `enum`/`const` без `type`: строка приводится к типу значения-кандидата, если
    /// после приведения совпадает с ним (Qwen XML даёт `"1"` при `{"enum": [1, 2]}`).
    fn coerce_to_constants(&mut self, s: &Map<String, Value>, value: &mut Value, path: &str) {
        if !value.is_string() {
            return;
        }
        let mut candidates: Vec<&Value> = Vec::new();
        if let Some(Value::Array(options)) = s.get("enum") {
            candidates.extend(options);
        }
        candidates.extend(s.get("const"));
        if candidates.iter().any(|c| json_eq(c, value)) {
            return;
        }
        for c in candidates {
            let t = match c {
                Value::Bool(_) => JsonType::Boolean,
                Value::Number(_) if is_integer(c) => JsonType::Integer,
                Value::Number(_) => JsonType::Number,
                Value::Object(_) => JsonType::Object,
                Value::Array(_) => JsonType::Array,
                Value::Null | Value::String(_) => continue,
            };
            if let Some(v) = coerce_string(value, t)
                && json_eq(&v, c)
            {
                *value = v;
                self.repairs.push(Repair::TypeCoerced {
                    path: path.to_owned(),
                    to: t,
                });
                return;
            }
        }
    }

    fn check_number(&mut self, s: &Map<String, Value>, value: &Value, path: &str) {
        let Some(x) = value.as_f64() else { return };
        if let Some(min) = s.get("minimum").and_then(Value::as_f64)
            && x < min
        {
            self.error(path, format!("value is less than minimum {min}"));
        }
        if let Some(max) = s.get("maximum").and_then(Value::as_f64)
            && x > max
        {
            self.error(path, format!("value is greater than maximum {max}"));
        }
    }

    fn check_string(&mut self, s: &Map<String, Value>, value: &Value, path: &str) {
        let Some(text) = value.as_str() else { return };
        let len = text.chars().count() as u64;
        if let Some(min) = s.get("minLength").and_then(Value::as_u64)
            && len < min
        {
            self.error(path, format!("string is shorter than minLength {min}"));
        }
        if let Some(max) = s.get("maxLength").and_then(Value::as_u64)
            && len > max
        {
            self.error(path, format!("string is longer than maxLength {max}"));
        }
    }

    fn check_array(&mut self, s: &Map<String, Value>, value: &mut Value, path: &str, depth: usize) {
        let Value::Array(items) = value else { return };
        let len = items.len() as u64;
        if let Some(min) = s.get("minItems").and_then(Value::as_u64)
            && len < min
        {
            self.error(path, format!("fewer items than minItems {min}"));
        }
        if let Some(max) = s.get("maxItems").and_then(Value::as_u64)
            && len > max
        {
            self.error(path, format!("more items than maxItems {max}"));
        }
        match s.get("items") {
            Some(item_schema @ (Value::Object(_) | Value::Bool(_))) => {
                for (i, item) in items.iter_mut().enumerate() {
                    self.check(item_schema, item, &format!("{path}/{i}"), depth + 1);
                }
            }
            Some(_) => tracing::debug!(path, "items в форме кортежа не поддерживается, пропущено"),
            None => {}
        }
    }

    fn check_object(
        &mut self,
        s: &Map<String, Value>,
        value: &mut Value,
        path: &str,
        depth: usize,
    ) {
        let Value::Object(obj) = value else { return };
        let empty = Map::new();
        let props = s
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let required: Vec<&str> = s
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        // `null` у необязательного свойства, схема которого его не допускает, — удаляется.
        let nulls: Vec<String> = obj
            .iter()
            .filter(|(k, v)| v.is_null() && !required.contains(&k.as_str()))
            .filter_map(|(k, _)| props.get(k).map(|ps| (k, ps)))
            .filter(|(k, ps)| {
                let (ok, repairs, _) = self.trial(ps, &Value::Null, &child(path, k), depth + 1);
                !(ok && repairs.is_empty())
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in nulls {
            obj.remove(&k);
            self.repairs.push(Repair::NullOptionalRemoved {
                path: child(path, &k),
            });
        }

        for r in &required {
            if !obj.contains_key(*r) {
                self.error(&child(path, r), "missing required field");
            }
        }
        for (k, v) in obj.iter_mut() {
            let p = child(path, k);
            if let Some(ps) = props.get(k) {
                self.check(ps, v, &p, depth + 1);
            } else {
                match s.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        self.error(&p, "unexpected field, additionalProperties is false")
                    }
                    Some(extra @ Value::Object(_)) => self.check(extra, v, &p, depth + 1),
                    _ => {}
                }
            }
        }
    }

    /// `anyOf` (`one == false`) и `oneOf`: сначала ветки, подходящие без приведения;
    /// если таких нет — первая ветка, подходящая с приведением.
    fn check_branches(
        &mut self,
        branches: &[Value],
        value: &mut Value,
        path: &str,
        depth: usize,
        one: bool,
    ) {
        let kw = if one { "oneOf" } else { "anyOf" };
        let trials: Vec<(bool, Vec<Repair>, Value)> = branches
            .iter()
            .map(|b| self.trial(b, value, path, depth + 1))
            .collect();
        let strict = trials
            .iter()
            .filter(|(ok, r, _)| *ok && r.is_empty())
            .count();
        if strict > 0 {
            if one && strict > 1 {
                self.error(path, format!("value matches more than one variant of {kw}"));
            }
            return;
        }
        match trials.into_iter().find(|(ok, _, _)| *ok) {
            Some((_, repairs, v)) => {
                *value = v;
                self.repairs.extend(repairs);
            }
            None => self.error(path, format!("value matches no variant of {kw}")),
        }
    }
}

/// Путь к дочернему элементу (JSON Pointer, RFC 6901: `~` → `~0`, `/` → `~1`).
fn child(path: &str, key: &str) -> String {
    format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"))
}

/// Локальный `$ref` (`#`, `#/$defs/…`, `#/definitions/…` и любой указатель внутри схемы).
fn resolve_ref<'a>(root: &'a Value, r: &str) -> Option<&'a Value> {
    let pointer = r.strip_prefix('#')?;
    if pointer.is_empty() {
        return Some(root);
    }
    root.pointer(pointer)
}

fn parse_types(t: &Value) -> Vec<JsonType> {
    let names: Vec<&str> = match t {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    names
        .into_iter()
        .filter_map(|n| {
            let parsed = JsonType::parse(n);
            if parsed.is_none() {
                tracing::debug!(r#type = n, "неизвестный тип JSON Schema пропущен");
            }
            parsed
        })
        .collect()
}

/// Приведение значения к одному из типов: сначала разбор строки по порядку типов,
/// затем обёртка скаляра в массив. Возвращает значение и исправление по пути.
fn coerce(value: &Value, types: &[JsonType], path: &str) -> Option<(Value, Repair)> {
    for &t in types {
        if let Some(v) = coerce_string(value, t) {
            let repair = Repair::TypeCoerced {
                path: path.to_owned(),
                to: t,
            };
            return Some((v, repair));
        }
    }
    // Строка, похожая на массив, но не разобранная как JSON, — ошибка, а не `[строка]`.
    let broken_array = value
        .as_str()
        .is_some_and(|s| s.trim_start().starts_with('['));
    if types.contains(&JsonType::Array) && !value.is_array() && !broken_array {
        let repair = Repair::WrappedInArray {
            path: path.to_owned(),
        };
        return Some((Value::Array(vec![value.clone()]), repair));
    }
    None
}

fn coerce_string(value: &Value, t: JsonType) -> Option<Value> {
    let s = value.as_str()?.trim();
    match t {
        JsonType::Integer => {
            if let Ok(i) = s.parse::<i64>() {
                return Some(Value::from(i));
            }
            if let Ok(u) = s.parse::<u64>() {
                return Some(Value::from(u));
            }
            let f = s
                .parse::<f64>()
                .ok()
                .filter(|f| f.is_finite() && f.fract() == 0.0)?;
            // Целое без потери точности только до 2^53.
            (f.abs() <= 9_007_199_254_740_992.0).then(|| Value::from(f as i64))
        }
        JsonType::Number => {
            if let Ok(i) = s.parse::<i64>() {
                return Some(Value::from(i));
            }
            let f = s.parse::<f64>().ok().filter(|f| f.is_finite())?;
            serde_json::Number::from_f64(f).map(Value::Number)
        }
        JsonType::Boolean => {
            if s.eq_ignore_ascii_case("true") {
                Some(Value::Bool(true))
            } else if s.eq_ignore_ascii_case("false") {
                Some(Value::Bool(false))
            } else {
                None
            }
        }
        JsonType::Object => serde_json::from_str::<Value>(s)
            .ok()
            .filter(Value::is_object),
        JsonType::Array => serde_json::from_str::<Value>(s)
            .ok()
            .filter(Value::is_array),
        JsonType::Null | JsonType::String => None,
    }
}

fn is_integer(v: &Value) -> bool {
    match v {
        Value::Number(n) => {
            n.is_i64()
                || n.is_u64()
                || n.as_f64()
                    .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
        }
        _ => false,
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) if is_integer(v) => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Равенство JSON с числами по значению (`3` == `3.0`).
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if let (Some(x), Some(y)) = (x.as_i64(), y.as_i64()) {
                x == y
            } else if let (Some(x), Some(y)) = (x.as_u64(), y.as_u64()) {
                x == y
            } else {
                x.as_f64() == y.as_f64()
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_eq(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => a == b,
    }
}
