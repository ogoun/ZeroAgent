//! Стабильный префикс промпта: описания инструментов и единственное system-сообщение.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use za_protocol::canonical_json;

/// Описание инструмента для модели.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Блоки, из которых собирается префикс.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PromptBlocks {
    pub tools: Vec<ToolSchema>,
    pub core: String,
    pub adapter: String,
    pub overlay: String,
    pub agents_md: String,
}

/// Версия формата префикса; входит в хэш, чтобы смена формата не совпала со старым.
const FORMAT_TAG: &[u8] = b"za-prefix-v1\0";

/// Единственное system-сообщение (CTX-004): блоки core, adapter, overlay, agents_md
/// в этом порядке через пустую строку (`"\n\n"`). Блок из одних пробельных символов
/// пропускается, содержимое остальных не меняется, кроме замены CRLF на LF.
pub fn render_system(p: &PromptBlocks) -> String {
    [&p.core, &p.adapter, &p.overlay, &p.agents_md]
        .into_iter()
        .map(|b| normalize_newlines(b))
        .filter(|b| !b.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Описания инструментов (CTX-001): массив, отсортированный по `name`, каждый
/// инструмент — объект `{description, name, parameters}` в каноническом JSON.
/// Во всех строках, включая ключи и значения `parameters`, CRLF заменяется на LF.
pub fn render_tools(p: &PromptBlocks) -> String {
    let mut tools: Vec<&ToolSchema> = p.tools.iter().collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let arr = tools
        .into_iter()
        .map(|t| {
            normalize_value(&serde_json::json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            }))
        })
        .collect();
    canonical_json(&Value::Array(arr))
}

/// BLAKE3 (hex, 64 символа) от инструментов и system. Части разделены длинами,
/// поэтому перенос текста из одной части в другую меняет хэш.
pub fn prefix_hash(p: &PromptBlocks) -> String {
    let tools = render_tools(p);
    let system = render_system(p);
    let mut h = blake3::Hasher::new();
    h.update(FORMAT_TAG);
    for part in [&tools, &system] {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    h.finalize().to_hex().to_string()
}

fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn normalize_value(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(normalize_newlines(s)),
        Value::Array(a) => Value::Array(a.iter().map(normalize_value).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (normalize_newlines(k), normalize_value(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, description: &str, parameters: Value) -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    /// Короткий, но реалистичный набор блоков. Любая правка меняет золотой хэш.
    fn fixture() -> PromptBlocks {
        PromptBlocks {
            tools: vec![
                tool(
                    "read_file",
                    "Прочитать текстовый файл рабочего каталога.",
                    json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Путь относительно корня"},
                            "offset": {"type": "integer", "minimum": 0}
                        },
                        "required": ["path"]
                    }),
                ),
                tool(
                    "apply_patch",
                    "Применить unified diff к файлам рабочего каталога.",
                    json!({
                        "type": "object",
                        "properties": {"patch": {"type": "string"}},
                        "required": ["patch"]
                    }),
                ),
                tool(
                    "run_command",
                    "Выполнить команду оболочки в песочнице.",
                    json!({
                        "type": "object",
                        "properties": {
                            "command": {"type": "string"},
                            "timeout_ms": {"type": "integer", "minimum": 1}
                        },
                        "required": ["command"]
                    }),
                ),
            ],
            core: "Ты zero-agent, агент для работы с кодом.\nДействуй через инструменты.".into(),
            adapter: "Отвечай кратко.".into(),
            overlay: String::new(),
            agents_md: "# AGENTS.md\n\nТесты: `cargo test --workspace`.".into(),
        }
    }

    #[test]
    fn tool_order_does_not_change_hash() {
        let a = fixture();
        let mut b = fixture();
        b.tools.reverse();
        assert_eq!(render_tools(&a), render_tools(&b));
        assert_eq!(prefix_hash(&a), prefix_hash(&b));
    }

    #[test]
    fn key_order_in_schema_does_not_change_hash() {
        let mut a = fixture();
        let mut b = fixture();
        let params_a = r#"{"type":"object","required":["x"],"properties":{"x":{"type":"string","description":"d"}}}"#;
        let params_b = r#"{"properties":{"x":{"description":"d","type":"string"}},"required":["x"],"type":"object"}"#;
        a.tools[0].parameters = serde_json::from_str(params_a).unwrap();
        b.tools[0].parameters = serde_json::from_str(params_b).unwrap();
        assert_eq!(prefix_hash(&a), prefix_hash(&b));
    }

    #[test]
    fn any_block_change_changes_hash() {
        let base = prefix_hash(&fixture());
        let edits: [fn(&mut PromptBlocks); 6] = [
            |p| p.core.push('.'),
            |p| p.adapter.push('.'),
            |p| p.overlay = "Режим плана.".into(),
            |p| p.agents_md.push('.'),
            |p| p.tools[0].description.push('.'),
            |p| {
                p.tools.pop();
            },
        ];
        for (i, edit) in edits.iter().enumerate() {
            let mut p = fixture();
            edit(&mut p);
            assert_ne!(prefix_hash(&p), base, "правка №{i} не изменила хэш");
        }
    }

    #[test]
    fn crlf_does_not_change_hash() {
        let a = fixture();
        let mut b = fixture();
        b.core = b.core.replace('\n', "\r\n");
        b.agents_md = b.agents_md.replace('\n', "\r\n");
        b.tools[1].description.push_str("\r\nВторая строка.");
        let mut a2 = a.clone();
        a2.tools[1].description.push_str("\nВторая строка.");
        assert_eq!(render_system(&a), render_system(&b));
        assert_eq!(prefix_hash(&a2), prefix_hash(&b));
    }

    #[test]
    fn system_joins_non_empty_blocks() {
        let s = render_system(&fixture());
        assert_eq!(
            s,
            "Ты zero-agent, агент для работы с кодом.\nДействуй через инструменты.\n\n\
             Отвечай кратко.\n\n\
             # AGENTS.md\n\nТесты: `cargo test --workspace`."
        );
    }

    #[test]
    fn tools_are_sorted_canonical_json() {
        let p = PromptBlocks {
            tools: vec![
                tool("b", "B", json!({"z": 1, "a": 2})),
                tool("a", "A", json!({})),
            ],
            ..Default::default()
        };
        assert_eq!(
            render_tools(&p),
            r#"[{"description":"A","name":"a","parameters":{}},{"description":"B","name":"b","parameters":{"a":2,"z":1}}]"#
        );
    }

    #[test]
    fn hash_separates_tools_and_system() {
        // Перенос текста между инструментами и system не должен давать тот же хэш.
        let a = PromptBlocks {
            core: "x".into(),
            ..Default::default()
        };
        let b = PromptBlocks::default();
        assert_ne!(prefix_hash(&a), prefix_hash(&b));
        assert_eq!(prefix_hash(&b).len(), 64);
    }

    #[test]
    fn tool_schema_serializes() {
        let t = fixture().tools[0].clone();
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["name"], "read_file");
        let back: ToolSchema = serde_json::from_value(v).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn golden_hash_is_stable() {
        let actual = prefix_hash(&fixture());
        let golden = include_str!("../tests/golden/prefix.hash").trim();
        assert_eq!(
            actual, golden,
            "префикс изменился; если намеренно, запишите в crates/za-llm/tests/golden/prefix.hash: {actual}"
        );
    }
}
