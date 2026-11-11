//! Маска строк значения JSON (R1-C1 D90, D100): общий обход для бинарника и `za-agent`.
//!
//! Сам маскировщик (`za_secrets::SecretMasker`) сюда не входит: `za-protocol` не зависит
//! от крейтов workspace, поэтому обход получает функцию маски текста.

use serde_json::Value;

/// Значение JSON, в котором замаскированы все строки и ключи объектов: аргументы вызовов,
/// payload событий, записи аудита. Маскируются декодированные значения строк; при выводе
/// `serde_json` кодирует их обратно. Строка, которая сама — текст JSON (`raw_arguments`),
/// обрабатывается [`mask_str_with`].
pub fn mask_json_with(mask: &dyn Fn(&str) -> String, v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(mask_str_with(mask, s)),
        Value::Array(a) => Value::Array(a.iter().map(|x| mask_json_with(mask, x)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (mask(k), mask_json_with(mask, x)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Строка, которая может быть текстом JSON (ревью задачи 10, I2). Сначала маска сырого
/// текста: экранирование JSON сканер считает границей слова, байты текста сохраняются.
/// Если строка — объект или массив JSON, проверяются и её декодированные строки: секрет,
/// скрытый экранированием (`ghp_…`), маскируется, и текст кодируется заново из
/// маскированного значения.
pub fn mask_str_with(mask: &dyn Fn(&str) -> String, s: &str) -> String {
    let raw = mask(s);
    let t = s.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return raw;
    }
    let Ok(decoded) = serde_json::from_str::<Value>(s) else {
        return raw;
    };
    let reencode = |v: &Value| serde_json::to_string(v).unwrap_or_else(|_| raw.clone());
    match serde_json::from_str::<Value>(&raw) {
        Ok(parsed) => {
            let again = mask_json_with(mask, &parsed);
            if again == parsed {
                raw
            } else {
                reencode(&again)
            }
        }
        // Маска сломала текст JSON (блок PEM без END до конца строки): из декодированного.
        Err(_) => reencode(&mask_json_with(mask, &decoded)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn masks_strings_keys_and_nested_json_text() {
        let m = |s: &str| s.replace("SECRETVALUE", "[m]");
        let v = json!({"SECRETVALUEk": ["a SECRETVALUE", 1], "raw": "{\"x\": \"SECRETVALUE\"}"});
        let out = mask_json_with(&m, &v);
        assert_eq!(
            out,
            json!({"[m]k": ["a [m]", 1], "raw": "{\"x\": \"[m]\"}"})
        );
    }
}
