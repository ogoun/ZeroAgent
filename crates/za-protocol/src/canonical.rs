//! Канонический JSON: ключи объектов отсортированы рекурсивно, массивы сохраняют порядок.

use serde_json::Value;

/// Каноническая строка JSON (компактная, ключи отсортированы).
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, &mut out);
    out
}

fn write_value(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                // Строки и числа сериализует serde_json: детерминированно.
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write_value(&m[k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(x, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_sorts_nested_keys() {
        let v = json!({"b":1,"a":{"d":1,"c":2}});
        assert_eq!(canonical_json(&v), r#"{"a":{"c":2,"d":1},"b":1}"#);
    }

    #[test]
    fn canonical_keeps_array_order_and_sorts_inside() {
        let v = json!([{"z":1,"y":[3,1,2]},"s\"q",1.5,null,true]);
        assert_eq!(
            canonical_json(&v),
            r#"[{"y":[3,1,2],"z":1},"s\"q",1.5,null,true]"#
        );
    }

    #[test]
    fn canonical_is_independent_of_insertion_order() {
        let a: Value = serde_json::from_str(r#"{"x":1,"y":{"b":1,"a":2}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"y":{"a":2,"b":1},"x":1}"#).unwrap();
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }
}
