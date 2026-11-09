//! Конвейер нормализации вызовов инструментов (MDL-005, MDL-006) и форматы P0:
//! Hermes, Qwen XML, Harmony (задача 7).

use serde_json::{Value, json};
use za_llm::normalize::{
    CallRejection, CallSource, JsonType, Repair, all_formats, damerau_levenshtein,
    extracted_mutation_blocked, normalize, validate_and_coerce,
};
use za_llm::{Message, Part, Role, TextFormat, ToolCall, ToolSchema};
use za_protocol::EffectClass;

fn tool(name: &str, parameters: Value) -> ToolSchema {
    ToolSchema {
        name: name.into(),
        description: String::new(),
        parameters,
    }
}

fn tools() -> Vec<ToolSchema> {
    vec![
        tool(
            "read_file",
            json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        ),
        tool(
            "write_file",
            json!({"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}}),
        ),
        tool(
            "count",
            json!({"type": "object", "properties": {
                "n": {"type": "integer"},
                "f": {"type": "boolean"},
                "tags": {"type": "array", "items": {"type": "string"}}
            }, "required": ["n"]}),
        ),
    ]
}

fn names(list: &[&str]) -> Vec<ToolSchema> {
    list.iter()
        .map(|n| tool(n, json!({"type": "object"})))
        .collect()
}

fn assistant(parts: Vec<Part>) -> Message {
    Message {
        role: Role::Assistant,
        parts,
    }
}

fn text(s: &str) -> Message {
    assistant(vec![Part::Text(s.into())])
}

fn native(id: &str, name: &str, args: Value) -> Part {
    Part::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        raw_arguments: Some(args.to_string()),
        arguments: args,
    })
}

fn hermes(name: &str, args: Value) -> String {
    format!(
        "<tool_call>\n{}\n</tool_call>",
        json!({"name": name, "arguments": args})
    )
}

#[test]
fn native_calls_pass_through() {
    let msg = assistant(vec![native("c1", "read_file", json!({"path": "a.txt"}))]);
    let out = normalize(&msg, &tools(), all_formats());
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].source, CallSource::Native);
    assert!(out.calls[0].repairs.is_empty());
    assert!(out.rejected.is_empty());
    assert_eq!(out.calls[0].call.id, "c1");
    assert_eq!(out.calls[0].call.arguments, json!({"path": "a.txt"}));
}

#[test]
fn text_call_next_to_native_is_kept_and_duplicate_dropped() {
    let content = format!(
        "{}\n{}",
        hermes("read_file", json!({"path": "a.txt"})),
        hermes("write_file", json!({"path": "b", "content": "x"}))
    );
    let msg = assistant(vec![
        Part::Text(content),
        native("c1", "read_file", json!({"path": "a.txt"})),
    ]);
    let out = normalize(&msg, &tools(), all_formats());
    let got: Vec<(&str, CallSource)> = out
        .calls
        .iter()
        .map(|c| (c.call.name.as_str(), c.source))
        .collect();
    assert_eq!(
        got,
        [
            ("read_file", CallSource::Native),
            ("write_file", CallSource::Content)
        ]
    );
    assert_eq!(out.dropped_duplicates, 1);
    assert_eq!(out.text, "");
}

#[test]
fn hermes_in_content_is_extracted() {
    let msg = text(
        "Сначала прочитаю.\n<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}\n</tool_call>",
    );
    let out = normalize(&msg, &tools(), all_formats());
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].call.name, "read_file");
    assert_eq!(out.calls[0].call.arguments, json!({"path": "a.txt"}));
    assert_eq!(out.calls[0].source, CallSource::Content);
    assert_eq!(out.text, "Сначала прочитаю.");
    assert!(out.calls[0].repairs.contains(&Repair::ExtractedFromText {
        format: TextFormat::Hermes
    }));
}

#[test]
fn qwen_xml_pairs_are_coerced_by_schema() {
    let msg = text(
        "<tool_call>\n<function=count>\n<parameter=n>\n5\n</parameter>\n</function>\n</tool_call>",
    );
    let out = normalize(&msg, &tools(), all_formats());
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].call.name, "count");
    assert_eq!(out.calls[0].call.arguments, json!({"n": 5}));
    assert!(out.calls[0].repairs.contains(&Repair::TypeCoerced {
        path: "/n".into(),
        to: JsonType::Integer
    }));
    assert_eq!(out.text, "");
}

#[test]
fn harmony_call_in_reasoning_has_reasoning_source() {
    let msg = assistant(vec![Part::Reasoning {
        text: "<|channel|>commentary to=functions.read_file <|constrain|>json<|message|>{\"path\":\"a.txt\"}<|call|>".into(),
        replay: None,
    }]);
    let out = normalize(&msg, &tools(), all_formats());
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].call.name, "read_file");
    assert_eq!(out.calls[0].source, CallSource::Reasoning);
    assert_eq!(out.reasoning, "");
}

#[test]
fn coercion_table() {
    let schema = &tools()[2].parameters;
    let mut args = json!({"n": "5", "f": "true", "tags": "x"});
    let repairs = validate_and_coerce(schema, &mut args).unwrap();
    assert_eq!(args, json!({"n": 5, "f": true, "tags": ["x"]}));
    assert_eq!(repairs.len(), 3);
    assert!(repairs.contains(&Repair::TypeCoerced {
        path: "/n".into(),
        to: JsonType::Integer
    }));
    assert!(repairs.contains(&Repair::TypeCoerced {
        path: "/f".into(),
        to: JsonType::Boolean
    }));
    assert!(repairs.contains(&Repair::WrappedInArray {
        path: "/tags".into()
    }));
}

#[test]
fn missing_required_is_invalid_args() {
    let msg = assistant(vec![native("c1", "count", json!({}))]);
    let out = normalize(&msg, &tools(), all_formats());
    assert!(out.calls.is_empty());
    assert_eq!(out.rejected.len(), 1);
    match &out.rejected[0] {
        CallRejection::InvalidArgs { name, errors } => {
            assert_eq!(name, "count");
            assert_eq!(errors[0].path, "/n");
        }
        other => panic!("ожидался InvalidArgs: {other:?}"),
    }
}

#[test]
fn local_ref_defs_are_resolved() {
    let schema =
        json!({"$defs": {"P": {"type": "integer"}}, "properties": {"n": {"$ref": "#/$defs/P"}}});
    let mut ok = json!({"n": "5"});
    assert!(validate_and_coerce(&schema, &mut ok).is_ok());
    assert_eq!(ok, json!({"n": 5}));
    let mut bad = json!({"n": "abc"});
    let errors = validate_and_coerce(&schema, &mut bad).unwrap_err();
    assert_eq!(errors[0].path, "/n");
    // При ошибке аргументы не меняются.
    assert_eq!(bad, json!({"n": "abc"}));
}

#[test]
fn name_one_typo_is_repaired() {
    let msg = assistant(vec![native("c1", "read_flie", json!({"path": "a"}))]);
    let out = normalize(&msg, &tools(), all_formats());
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].call.name, "read_file");
    assert!(out.calls[0].repairs.contains(&Repair::NameCorrected {
        from: "read_flie".into(),
        to: "read_file".into()
    }));
}

#[test]
fn ambiguous_name_is_rejected_with_nearest() {
    let msg = assistant(vec![native("c1", "read_c", json!({}))]);
    let out = normalize(&msg, &names(&["read_a", "read_b"]), all_formats());
    assert!(out.calls.is_empty());
    assert_eq!(
        out.rejected,
        [CallRejection::UnknownTool {
            name: "read_c".into(),
            nearest: vec!["read_a".into(), "read_b".into()]
        }]
    );
}

#[test]
fn unknown_name_lists_up_to_three_nearest() {
    let msg = assistant(vec![native("c1", "abcf", json!({}))]);
    let out = normalize(
        &msg,
        &names(&["abcd", "abce", "abzz", "xyzw", "qqqq"]),
        all_formats(),
    );
    assert_eq!(
        out.rejected,
        [CallRejection::UnknownTool {
            name: "abcf".into(),
            nearest: vec!["abcd".into(), "abce".into(), "abzz".into()]
        }]
    );
}

#[test]
fn plain_json_answer_is_not_a_call() {
    let src = "{\"name\":\"Вася\"}";
    let out = normalize(&text(src), &tools(), all_formats());
    assert!(out.calls.is_empty());
    assert!(out.rejected.is_empty());
    assert_eq!(out.text, src);
}

#[test]
fn deterministic_call_id() {
    let msg = text(&hermes("read_file", json!({"path": "a.txt"})));
    let a = normalize(&msg, &tools(), all_formats());
    let b = normalize(&msg, &tools(), all_formats());
    let id = &a.calls[0].call.id;
    assert_eq!(id, &b.calls[0].call.id);
    assert!(id.starts_with("call_"));
    assert_eq!(id.len(), 5 + 12);
}

#[test]
fn extracted_mutation_policy() {
    assert!(extracted_mutation_blocked(
        CallSource::Content,
        EffectClass::NonIdempotent
    ));
    assert!(extracted_mutation_blocked(
        CallSource::Reasoning,
        EffectClass::Idempotent
    ));
    assert!(!extracted_mutation_blocked(
        CallSource::Content,
        EffectClass::ReadOnly
    ));
    assert!(!extracted_mutation_blocked(
        CallSource::Native,
        EffectClass::NonIdempotent
    ));
}

// ---- Устойчивость и краевые случаи ----

#[test]
fn truncated_hermes_is_unparseable_not_panic() {
    let src = "Читаю.\n<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"pa";
    let out = normalize(&text(src), &tools(), all_formats());
    assert!(out.calls.is_empty());
    assert_eq!(out.rejected.len(), 1);
    assert!(matches!(&out.rejected[0], CallRejection::Unparseable { raw } if raw.contains("\"pa")));
    assert_eq!(out.text, "Читаю.");
}

#[test]
fn hermes_without_name_is_unparseable() {
    let out = normalize(
        &text("<tool_call>{\"arguments\": {}}</tool_call>"),
        &tools(),
        all_formats(),
    );
    assert!(out.calls.is_empty());
    assert!(matches!(
        &out.rejected[..],
        [CallRejection::Unparseable { .. }]
    ));
}

#[test]
fn hermes_string_arguments_are_parsed() {
    let src = r#"<tool_call>{"name": "read_file", "arguments": "{\"path\": \"a\"}"}</tool_call>"#;
    let out = normalize(&text(src), &tools(), all_formats());
    assert_eq!(out.calls[0].call.arguments, json!({"path": "a"}));
    assert!(out.calls[0].repairs.contains(&Repair::ArgsParsedFromString));
}

#[test]
fn several_hermes_calls_in_a_row() {
    let src = format!(
        "{}{}",
        hermes("read_file", json!({"path": "a"})),
        hermes("read_file", json!({"path": "b"}))
    );
    let out = normalize(&text(&src), &tools(), all_formats());
    assert_eq!(out.calls.len(), 2);
    assert_ne!(out.calls[0].call.id, out.calls[1].call.id);
}

#[test]
fn identical_extracted_calls_get_distinct_ids() {
    let one = hermes("read_file", json!({"path": "a"}));
    let out = normalize(&text(&format!("{one}{one}")), &tools(), all_formats());
    assert_eq!(out.calls.len(), 2);
    assert_ne!(out.calls[0].call.id, out.calls[1].call.id);
}

#[test]
fn qwen_xml_without_outer_and_multiline_value() {
    let src = "Пишу.\n<function=write_file>\n<parameter=path>\nb.txt\n</parameter>\n<parameter=content>\nстрока 1\nстрока 2\n\n</parameter>\n</function>\nГотово.";
    let out = normalize(&text(src), &tools(), all_formats());
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(
        out.calls[0].call.arguments,
        json!({"path": "b.txt", "content": "строка 1\nстрока 2\n"})
    );
    assert!(out.calls[0].repairs.contains(&Repair::ExtractedFromText {
        format: TextFormat::QwenXml
    }));
    assert_eq!(out.text, "Пишу.\nГотово.");
}

#[test]
fn qwen_xml_missing_parameter_close_is_tolerated() {
    let src = "<tool_call><function=count><parameter=n>7<parameter=f>false</function></tool_call>";
    let out = normalize(&text(src), &tools(), all_formats());
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(out.calls[0].call.arguments, json!({"n": 7, "f": false}));
}

#[test]
fn truncated_qwen_xml_is_unparseable() {
    let src = "<tool_call>\n<function=write_file>\n<parameter=content>\nполовина фай";
    let out = normalize(&text(src), &tools(), all_formats());
    assert!(out.calls.is_empty());
    assert!(
        matches!(&out.rejected[..], [CallRejection::Unparseable { raw }] if raw.contains("половина"))
    );
    assert_eq!(out.text, "");
}

#[test]
fn harmony_recipient_in_role_section_and_text_around() {
    let src = "До.<|start|>assistant to=functions.read_file<|channel|>commentary json<|message|>{\"path\":\"x\"}<|call|>После.";
    let out = normalize(&text(src), &tools(), all_formats());
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].call.arguments, json!({"path": "x"}));
    assert_eq!(out.text, "До.После.");
}

#[test]
fn harmony_until_end_of_text_and_end_token() {
    let src =
        "<|channel|>commentary to=functions.read_file <|constrain|>json<|message|>{\"path\":\"a\"}";
    let out = normalize(&text(src), &tools(), all_formats());
    assert_eq!(out.calls.len(), 1);
    let src2 = "<|channel|>commentary to=functions.read_file<|message|>{\"path\":\"a\"}<|end|>";
    let out2 = normalize(&text(src2), &tools(), all_formats());
    assert_eq!(out2.calls.len(), 1);
    assert_eq!(out2.text, "");
}

#[test]
fn harmony_broken_json_is_unparseable() {
    let src = "<|channel|>commentary to=functions.read_file <|constrain|>json<|message|>{\"path\":";
    let out = normalize(&text(src), &tools(), all_formats());
    assert!(out.calls.is_empty());
    assert!(matches!(
        &out.rejected[..],
        [CallRejection::Unparseable { .. }]
    ));
    assert_eq!(out.text, "");
}

#[test]
fn harmony_mention_in_prose_is_not_a_call() {
    let src = "Вызов пишется как to=functions.read_file в заголовке.";
    let out = normalize(&text(src), &tools(), all_formats());
    assert!(out.calls.is_empty() && out.rejected.is_empty());
    assert_eq!(out.text, src);
}

#[test]
fn duplicate_after_coercion_is_dropped() {
    // Нативный {"n":5} и тот же вызов в Qwen XML со строкой "5" — дубль (D39).
    let msg = assistant(vec![
        Part::Text("<function=count><parameter=n>5</parameter></function>".into()),
        native("c1", "count", json!({"n": 5})),
    ]);
    let out = normalize(&msg, &tools(), all_formats());
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.dropped_duplicates, 1);
}

#[test]
fn native_unparsed_raw_arguments() {
    let ok = assistant(vec![Part::ToolCall(ToolCall {
        id: String::new(),
        name: "read_file".into(),
        arguments: Value::Null,
        raw_arguments: Some("```json\n{\"path\": \"a\"}\n```".into()),
    })]);
    let out = normalize(&ok, &tools(), all_formats());
    assert_eq!(out.calls[0].call.arguments, json!({"path": "a"}));
    assert!(out.calls[0].repairs.contains(&Repair::ArgsParsedFromString));
    assert!(out.calls[0].call.id.starts_with("call_"));

    let bad = assistant(vec![Part::ToolCall(ToolCall {
        id: "c9".into(),
        name: "read_file".into(),
        arguments: Value::Null,
        raw_arguments: Some("{\"path\": ".into()),
    })]);
    let out = normalize(&bad, &tools(), all_formats());
    assert_eq!(
        out.rejected,
        [CallRejection::Unparseable {
            raw: "{\"path\": ".into()
        }]
    );
}

#[test]
fn null_optional_is_removed() {
    let schema = &tools()[2].parameters;
    let mut args = json!({"n": 1, "f": null});
    let repairs = validate_and_coerce(schema, &mut args).unwrap();
    assert_eq!(args, json!({"n": 1}));
    assert_eq!(repairs, [Repair::NullOptionalRemoved { path: "/f".into() }]);
}

#[test]
fn schema_keywords() {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "mode": {"enum": ["a", "b"]},
            "k": {"const": 3},
            "x": {"type": "number", "minimum": 0, "maximum": 10},
            "s": {"type": "string", "minLength": 2, "maxLength": 3},
            "l": {"type": "array", "minItems": 1, "maxItems": 2, "items": {"type": "integer"}},
            "u": {"anyOf": [{"type": "integer"}, {"type": "string"}]},
            "o": {"oneOf": [{"type": "integer"}, {"type": "boolean"}]},
            "t": {"type": ["integer", "null"]}
        }
    });
    let mut ok = json!({"mode": "a", "k": 3.0, "x": "2.5", "s": "ab", "l": ["1"], "u": "z", "o": "true", "t": null});
    let repairs = validate_and_coerce(&schema, &mut ok).unwrap();
    assert_eq!(
        ok,
        json!({"mode": "a", "k": 3.0, "x": 2.5, "s": "ab", "l": [1], "u": "z", "o": true, "t": null})
    );
    assert_eq!(repairs.len(), 3, "{repairs:?}");

    let mut bad = json!({"mode": "c", "k": 4, "x": 11, "s": "a", "l": [], "extra": 1});
    let errors = validate_and_coerce(&schema, &mut bad).unwrap_err();
    let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
    for p in ["/mode", "/k", "/x", "/s", "/l", "/extra"] {
        assert!(paths.contains(&p), "нет ошибки {p}: {errors:?}");
    }
}

#[test]
fn damerau_levenshtein_values() {
    assert_eq!(damerau_levenshtein("", ""), 0);
    assert_eq!(damerau_levenshtein("abc", ""), 3);
    assert_eq!(damerau_levenshtein("read_flie", "read_file"), 1);
    assert_eq!(damerau_levenshtein("ca", "abc"), 2);
    assert_eq!(damerau_levenshtein("kitten", "sitting"), 3);
    assert_eq!(damerau_levenshtein("чтение", "чтенеи"), 1);
}

#[test]
fn text_without_markup_is_untouched() {
    let src = "  Ответ без вызовов.\n\n";
    let out = normalize(&text(src), &tools(), &[]);
    assert_eq!(out.text, src);
}

#[test]
fn extracted_name_typo_is_corrected() {
    let out = normalize(
        &text(&hermes("raed_file", json!({"path": "a"}))),
        &tools(),
        all_formats(),
    );
    assert_eq!(out.calls[0].call.name, "read_file");
}

#[test]
fn harmony_prose_mention_does_not_swallow_real_call() {
    let src = "Пишу to=functions.x прозой.<|channel|>commentary to=functions.read_file<|message|>{\"path\":\"a\"}<|call|>";
    let out = normalize(&text(src), &tools(), all_formats());
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].call.name, "read_file");
    assert_eq!(out.text, "Пишу to=functions.x прозой.");
}

#[test]
fn mixed_formats_in_one_text() {
    let src = format!(
        "a\n{}\nb\n<tool_call>\n<function=read_file>\n<parameter=path>\nq\n</parameter>\n</function>\n</tool_call>\nc",
        hermes("read_file", json!({"path": "h"}))
    );
    let out = normalize(&text(&src), &tools(), all_formats());
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    let paths: Vec<&Value> = out
        .calls
        .iter()
        .map(|c| &c.call.arguments["path"])
        .collect();
    assert_eq!(paths, [&json!("h"), &json!("q")]);
    assert_eq!(out.text, "a\nb\nc");
}

/// Любой префикс сложного ответа (обрыв стрима на любом символе) разбирается без
/// паники, а принятые вызовы всегда проходят схему.
#[test]
fn every_truncation_point_is_safe() {
    let src = format!(
        "Текст ✓.\n{}\n<tool_call>\n<function=count>\n<parameter=n>\n5\n</parameter>\n</function>\n</tool_call>\n<|start|>assistant<|channel|>commentary to=functions.write_file <|constrain|>json<|message|>{{\"path\":\"п\",\"content\":\"ж\"}}<|call|>",
        hermes("read_file", json!({"path": "файл"}))
    );
    let tools = tools();
    for (i, _) in src.char_indices() {
        let out = normalize(&text(&src[..i]), &tools, all_formats());
        assert!(out.calls.len() + out.rejected.len() <= 3, "префикс {i}");
        for c in &out.calls {
            let schema = &tools
                .iter()
                .find(|t| t.name == c.call.name)
                .unwrap()
                .parameters;
            let mut args = c.call.arguments.clone();
            assert_eq!(
                validate_and_coerce(schema, &mut args),
                Ok(vec![]),
                "префикс {i}"
            );
        }
    }
    let full = normalize(&text(&src), &tools, all_formats());
    assert_eq!(full.calls.len(), 3);
    assert_eq!(full.text, "Текст ✓.");
}
