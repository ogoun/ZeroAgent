//! Конвейер нормализации вызовов инструментов (MDL-005, MDL-006) и форматы P0:
//! Hermes, Qwen XML, Harmony (задача 7); форматы P1: DSML, Gemma 4, Mistral,
//! pythonic, llama3_json (задача 8).

mod support;

use serde_json::{Value, json};
use za_llm::normalize::{
    CallRejection, CallSource, JsonType, MAX_EXTRACTED_CALLS, NormalizeOutcome, Repair,
    all_formats, damerau_levenshtein, extracted_mutation_blocked, normalize, validate_and_coerce,
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

// ---- Раунд исправлений 1 ----

/// Порог тестов скорости на ~1,2 МБ (раунд исправлений 1 задачи 8, решение: запас
/// ×5 в debug). Норма — 1 с в release; в debug, где тесты гоняются обычно, порог 5 с,
/// чтобы медленный раннер CI не давал ложных падений. Квадратичный разбор на таком
/// входе занимает десятки секунд и в release, так что порог его ловит. Худший
/// замер ревью в release — 0,076 с.
fn time_limit() -> std::time::Duration {
    support::scaled(std::time::Duration::from_secs(if cfg!(debug_assertions) {
        5
    } else {
        1
    }))
}

/// I1: разбор линейный — ~1,2 МБ повторов каждого формата укладываются в
/// [`time_limit`].
#[test]
fn megabyte_of_repeated_markers_is_linear() {
    let call_h = hermes("read_file", json!({"path": "a"}));
    let call_q = "<function=count><parameter=n>1</parameter></function>\n".to_owned();
    let call_x = "<|channel|>commentary to=functions.read_file <|constrain|>json<|message|>{\"path\":\"a\"}<|call|>".to_owned();
    let units: Vec<String> = vec![
        "to=functions.x ".into(),
        "<tool_call> ".into(),
        "<tool_call>{".into(),
        "<function=a> ".into(),
        "<function=read_file> ".into(),
        "<function=count><parameter=n>".into(),
        "<|channel|>commentary to=functions.read_file ".into(),
        "```\n<tool_call>\n".into(),
        call_h,
        call_q,
        call_x,
    ];
    let tools = tools();
    for unit in units {
        let src = unit.repeat(1_200_000 / unit.len());
        let started = std::time::Instant::now();
        let out = normalize(&text(&src), &tools, all_formats());
        let took = started.elapsed();
        assert!(took < time_limit(), "{unit:?}: {took:?}");
        assert!(
            out.calls.len() + out.rejected.len() <= MAX_EXTRACTED_CALLS as usize + 1,
            "{unit:?}"
        );
    }
}

#[test]
fn too_many_extracted_calls_are_capped() {
    let src = hermes("read_file", json!({"path": "a"})).repeat(70);
    let out = normalize(&text(&src), &tools(), all_formats());
    assert_eq!(out.calls.len(), 64);
    assert_eq!(
        out.rejected,
        [CallRejection::TooManyCalls {
            limit: 64,
            skipped: 6
        }]
    );
}

#[test]
fn tag_mention_in_prose_is_plain_text() {
    for src in [
        "Добавил разбор `<tool_call>` в парсер. Дальше важный текст.",
        "Шаблон Qwen: `<function=NAME>` и параметры. Дальше важный текст.",
        "Используй `<function=read_file>` для чтения. Дальше важный текст.",
        "Заголовок `<|channel|>commentary to=functions.read_file` и\nдальше важный текст.",
    ] {
        let out = normalize(&text(src), &tools(), all_formats());
        assert!(out.calls.is_empty() && out.rejected.is_empty(), "{src}");
        assert_eq!(out.text, src);
    }
}

#[test]
fn examples_in_code_blocks_are_not_calls() {
    let call = hermes("read_file", json!({"path": "x"}));
    let closed = format!("Пример:\n```xml\n{call}\n```\nКонец.");
    let out = normalize(&text(&closed), &tools(), all_formats());
    assert!(out.calls.is_empty() && out.rejected.is_empty());
    assert_eq!(out.text, closed);

    let unclosed = format!("Пример:\n```\n{call}");
    let out = normalize(&text(&unclosed), &tools(), all_formats());
    assert!(out.calls.is_empty() && out.rejected.is_empty());
    assert_eq!(out.text, unclosed);

    // Вызов после закрытого блока извлекается.
    let after = format!("```\n{call}\n```\n{call}");
    let out = normalize(&text(&after), &tools(), all_formats());
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.text, format!("```\n{call}\n```"));
}

#[test]
fn unclosed_real_call_at_end_is_rejected() {
    for src in [
        "Текст.\n<tool_call>\n{\"name\": \"read_file\", \"argu",
        "Текст.\n<function=read_file>\n<parameter=path>\na.t",
        "Текст.\n<function=read_file>\n<param",
        "Текст.\n<|channel|>commentary to=functions.read_file <|constrain|>js",
    ] {
        let out = normalize(&text(src), &tools(), all_formats());
        assert!(out.calls.is_empty(), "{src}");
        assert!(
            matches!(&out.rejected[..], [CallRejection::Unparseable { .. }]),
            "{src}: {:?}",
            out.rejected
        );
        assert_eq!(out.text, "Текст.");
    }
}

#[test]
fn same_call_in_reasoning_and_content_runs_once() {
    let call = hermes("read_file", json!({"path": "a"}));
    let msg = assistant(vec![
        Part::Reasoning {
            text: call.clone(),
            replay: None,
        },
        Part::Text(call),
    ]);
    let out = normalize(&msg, &tools(), all_formats());
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].source, CallSource::Content);
    assert_eq!(out.dropped_duplicates, 1);
}

#[test]
fn short_names_are_not_corrected() {
    let tools = names(&["read_file", "set"]);
    let out = normalize(
        &assistant(vec![native("c1", "get", json!({}))]),
        &tools,
        all_formats(),
    );
    assert!(out.calls.is_empty());
    assert!(
        matches!(&out.rejected[..], [CallRejection::UnknownTool { name, nearest }] if name == "get" && nearest[0] == "set")
    );
    // len 6 → порог 1: «read_x» с двумя ошибками от «read_ab» не исправляется.
    let tools = names(&["read_ab"]);
    let out = normalize(
        &assistant(vec![native("c1", "rexd_b", json!({}))]),
        &tools,
        all_formats(),
    );
    assert!(out.calls.is_empty());
}

#[test]
fn close_tag_inside_hermes_string_argument() {
    let src = hermes(
        "write_file",
        json!({"path": "a", "content": "x</tool_call>y"}),
    );
    let out = normalize(&text(&src), &tools(), all_formats());
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(out.calls[0].call.arguments["content"], "x</tool_call>y");
    assert_eq!(out.text, "");
}

#[test]
fn enum_without_type_is_coerced() {
    let schema = json!({"properties": {"k": {"enum": [1, 2]}, "c": {"const": true}}});
    let mut args = json!({"k": "1", "c": "true"});
    let repairs = validate_and_coerce(&schema, &mut args).unwrap();
    assert_eq!(args, json!({"k": 1, "c": true}));
    assert_eq!(repairs.len(), 2);
}

#[test]
fn broken_json_array_string_is_not_wrapped() {
    let schema = json!({"type": "array", "items": {"type": "string"}});
    let mut args = json!("[1, 2");
    assert!(validate_and_coerce(&schema, &mut args).is_err());
}

#[test]
fn text_parts_do_not_stick_together() {
    let msg = assistant(vec![
        Part::Text("A".into()),
        native("c1", "read_file", json!({"path": "a"})),
        Part::Text("B".into()),
    ]);
    assert_eq!(normalize(&msg, &tools(), all_formats()).text, "A\nB");
}

// ---- Раунд исправлений 2 ----

/// R1: полный вызов Qwen XML с неизвестным именем — `UnknownTool` с ближайшими (D28),
/// как у Hermes; обрезанный вызов с неизвестным именем — текст.
#[test]
fn complete_qwen_call_with_unknown_name_is_rejected_with_nearest() {
    let src = "<tool_call>\n<function=delete_everything>\n<parameter=path>\n/\n</parameter>\n</function>\n</tool_call>";
    let out = normalize(&text(src), &tools(), all_formats());
    assert!(out.calls.is_empty());
    assert!(
        matches!(&out.rejected[..], [CallRejection::UnknownTool { name, nearest }] if name == "delete_everything" && nearest.len() == 3),
        "{:?}",
        out.rejected
    );
    assert_eq!(out.text, "");

    let truncated = "Текст <function=delete_everything>\n<parameter=path>\n/";
    let out = normalize(&text(truncated), &tools(), all_formats());
    assert!(out.calls.is_empty() && out.rejected.is_empty());
    assert_eq!(out.text, truncated);
}

/// R2: `<tool_call>{...}` в прозе без `</tool_call>` — текст; обрезанный концом
/// текста JSON — отклонение.
#[test]
fn hermes_syntax_error_without_close_is_plain_text() {
    let src = "Модель пишет <tool_call>{...} в ответ. Дальше важный текст.";
    let out = normalize(&text(src), &tools(), all_formats());
    assert!(out.calls.is_empty() && out.rejected.is_empty());
    assert_eq!(out.text, src);
    // Упоминание в прозе не захватывает настоящий вызов после него.
    let second = normalize(
        &text(
            "Формат: <tool_call>{\"name\": ...} и так далее.\n<tool_call>{\"name\": \"read_file\", \"arguments\": {\"path\": \"a\"}}</tool_call>",
        ),
        &tools(),
        all_formats(),
    );
    assert_eq!(second.calls.len(), 1);
    assert_eq!(
        second.text,
        "Формат: <tool_call>{\"name\": ...} и так далее."
    );
    // Синтаксическая ошибка, но блок закрыт — битый вызов.
    let closed = normalize(
        &text("A <tool_call>{\"name\": oops}</tool_call> B"),
        &tools(),
        all_formats(),
    );
    assert!(matches!(
        &closed.rejected[..],
        [CallRejection::Unparseable { .. }]
    ));
    assert_eq!(closed.text, "A B");
}

/// Minor A: путь `accept_name` с 33 инструментами, опечатками и длинными именами —
/// линейный и быстрый (кэш по имени, отсев по разнице длин).
#[test]
fn megabyte_with_many_tools_and_long_names() {
    let mut tools = tools();
    tools.extend((0..30).map(|i| {
        tool(
            &format!("long_tool_name_number_{i:02}_for_testing_purposes"),
            json!({"type": "object"}),
        )
    }));
    let long = "x".repeat(250);
    let units = [
        "<function=read_filx><parameter=path>a</parameter></function>\n".to_owned(),
        "<function=frobnicate_tool><parameter=path>a</parameter></function>\n".to_owned(),
        "<function=read_filx><parameter=".to_owned(),
        format!("<|channel|>commentary to=functions.{long} "),
        format!("<function={}><parameter=", "y".repeat(200)),
        format!("<function=long_tool_name_number_0{}><parameter=", "z"),
    ];
    for unit in units {
        let src = unit.repeat(1_200_000 / unit.len());
        let started = std::time::Instant::now();
        let out = normalize(&text(&src), &tools, all_formats());
        let took = started.elapsed();
        assert!(
            took < time_limit(),
            "{}: {took:?}",
            &unit[..unit.len().min(40)]
        );
        assert!(out.calls.len() + out.rejected.len() <= MAX_EXTRACTED_CALLS as usize + 1);
    }
}

/// Minor B: вызов в инлайн-коде не исполняется, текст не меняется.
#[test]
fn call_in_inline_code_is_not_extracted() {
    let src = format!(
        "Пример: `{}` — так выглядит вызов.",
        hermes("read_file", json!({"path": "/etc/passwd"})).replace('\n', "")
    );
    let out = normalize(&text(&src), &tools(), all_formats());
    assert!(out.calls.is_empty() && out.rejected.is_empty());
    assert_eq!(out.text, src);
    // Обратные кавычки внутри аргумента настоящего вызова — не код.
    let real = hermes(
        "write_file",
        json!({"path": "a", "content": "use `x` and `y`"}),
    );
    let out = normalize(&text(&real), &tools(), all_formats());
    assert_eq!(out.calls[0].call.arguments["content"], "use `x` and `y`");
}

/// Minor C: области кода — по исходному тексту, вырезание вызовов их не сдвигает;
/// забор внутри значения вызова не спаривается с забором примера после него.
#[test]
fn code_regions_are_not_shifted_by_cut_calls() {
    let example = "```\n<|channel|>commentary to=functions.read_file<|message|>{\"path\":\"EXAMPLE\"}<|call|>\n```";
    // Значение Qwen XML с нечётным числом заборов (кусок Markdown).
    let src = format!(
        "<function=write_file>\n<parameter=path>\nREADME.md\n</parameter>\n<parameter=content>\nначало\n```\n</parameter>\n</function>\nПример:\n{example}"
    );
    let out = normalize(&text(&src), &tools(), all_formats());
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].call.name, "write_file");
    assert_eq!(out.calls[0].call.arguments["content"], "начало\n```");
    assert_eq!(out.text, format!("Пример:\n{example}"));

    // Тот же формат в примере: раньше забор значения спаривался с открывающим
    // забором примера, и пример исполнялся.
    let qwen_example =
        "```\n<function=read_file>\n<parameter=path>\nEXAMPLE\n</parameter>\n</function>\n```";
    let src = format!(
        "<function=write_file>\n<parameter=content>\nначало\n```\n</parameter>\n</function>\nПример:\n{qwen_example}"
    );
    let out = normalize(&text(&src), &tools(), all_formats());
    assert_eq!(out.calls.len(), 1, "{:?}", out.calls);
    assert_eq!(out.calls[0].call.name, "write_file");
    assert_eq!(out.text, format!("Пример:\n{qwen_example}"));

    // Полный блок кода в значении: восстанавливается в аргументе, пример после
    // вызова защищён, и все форматы видят одни и те же области.
    let src = format!(
        "<function=write_file>\n<parameter=content>\n```rs\nlet a = `b`;\n```\n</parameter>\n</function>\n{example}\n```\n{}\n```",
        hermes("read_file", json!({"path": "EXAMPLE2"}))
    );
    let out = normalize(&text(&src), &tools(), all_formats());
    assert_eq!(out.calls.len(), 1, "{:?}", out.calls);
    assert_eq!(
        out.calls[0].call.arguments["content"],
        "```rs\nlet a = `b`;\n```"
    );
}

// ---- Раунд исправлений 3 ----

/// I-H1: сообщение обычного канала Harmony (`final`, `analysis`) — не тело вызова:
/// код Markdown внутри него скрыт, примеры вызовов не исполняются.
#[test]
fn code_inside_harmony_channels_stays_hidden() {
    let hermes_example = hermes("read_file", json!({"path": "EX"}));
    let qwen_example =
        "<function=read_file>\n<parameter=path>\nEX\n</parameter>\n</function>".to_owned();
    for src in [
        format!(
            "<|channel|>final<|message|>Пример:\n```\n{hermes_example}\n```\nГотово.<|return|>"
        ),
        format!("<|channel|>analysis<|message|>Пример:\n```\n{qwen_example}\n```\n<|end|>"),
        format!(
            "<|channel|>final<|message|>Инлайн: `{}` — так.<|return|>",
            hermes_example.replace('\n', "")
        ),
    ] {
        let out = normalize(&text(&src), &tools(), all_formats());
        assert!(
            out.calls.is_empty() && out.rejected.is_empty(),
            "{src}: {:?} {:?}",
            out.calls,
            out.rejected
        );
        assert_eq!(out.text, src);
    }
    // Настоящий вызов Harmony с кодом в аргументе по-прежнему тело вызова.
    let call = "<|channel|>commentary to=functions.write_file <|constrain|>json<|message|>{\"path\":\"a\",\"content\":\"`x`\"}<|call|>";
    let out = normalize(&text(call), &tools(), all_formats());
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].call.arguments["content"], "`x`");
}

// ---- Форматы P1 (задача 8): DSML, Gemma 4, Mistral, pythonic, llama3_json ----

/// Нормализация одним форматом.
fn only(src: &str, format: TextFormat) -> NormalizeOutcome {
    normalize(&text(src), &tools(), &[format])
}

/// Похожий на вызов текст остаётся текстом и в одиночном формате, и во всех.
fn assert_plain(src: &str, format: TextFormat) {
    for out in [
        only(src, format),
        normalize(&text(src), &tools(), all_formats()),
    ] {
        assert!(
            out.calls.is_empty() && out.rejected.is_empty(),
            "{src}: {:?} {:?}",
            out.calls,
            out.rejected
        );
        assert_eq!(out.text, src);
    }
}

fn dsml_call(name: &str, params: &[(&str, &str, &str)]) -> String {
    let params: String = params
        .iter()
        .map(|(k, s, v)| {
            format!("<｜DSML｜parameter name=\"{k}\" string=\"{s}\">{v}</｜DSML｜parameter>\n")
        })
        .collect();
    format!(
        "<｜DSML｜function_calls>\n<｜DSML｜invoke name=\"{name}\">\n{params}</｜DSML｜invoke>\n</｜DSML｜function_calls>"
    )
}

#[test]
fn dsml_string_false_is_json() {
    let out = only(
        &dsml_call("count", &[("n", "false", "5")]),
        TextFormat::Dsml,
    );
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].call.arguments, json!({"n": 5}));
    assert!(
        !out.calls[0]
            .repairs
            .iter()
            .any(|r| matches!(r, Repair::TypeCoerced { .. }))
    );
    assert_eq!(out.text, "");

    let out = only(&dsml_call("count", &[("n", "true", "5")]), TextFormat::Dsml);
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].call.arguments, json!({"n": 5}));
    assert!(out.calls[0].repairs.contains(&Repair::TypeCoerced {
        path: "/n".into(),
        to: JsonType::Integer
    }));
    assert!(out.calls[0].repairs.contains(&Repair::ExtractedFromText {
        format: TextFormat::Dsml
    }));
}

#[test]
fn dsml_lookalike_is_text() {
    assert_plain(
        "пример тега <｜DSML｜invoke> без закрытия",
        TextFormat::Dsml,
    );
    assert_plain(
        "Вызов пишется как <｜DSML｜invoke name=\"read_file\"> и дальше параметры.",
        TextFormat::Dsml,
    );
}

#[test]
fn dsml_two_invokes_v4_wrapper_and_text_around() {
    let src = "Читаю.\n<｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"read_file\">\n<｜DSML｜parameter name=\"path\" string=\"true\">a.txt</｜DSML｜parameter>\n</｜DSML｜invoke>\n<｜DSML｜invoke name=\"count\">\n<｜DSML｜parameter name=\"n\" string=\"false\">2</｜DSML｜parameter>\n<｜DSML｜parameter name=\"tags\" string=\"false\">[\"x\"]</｜DSML｜parameter>\n</｜DSML｜invoke>\n</｜DSML｜tool_calls>\nГотово.";
    let out = only(src, TextFormat::Dsml);
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    let got: Vec<(&str, &Value)> = out
        .calls
        .iter()
        .map(|c| (c.call.name.as_str(), &c.call.arguments))
        .collect();
    assert_eq!(
        got,
        [
            ("read_file", &json!({"path": "a.txt"})),
            ("count", &json!({"n": 2, "tags": ["x"]}))
        ]
    );
    assert_eq!(out.text, "Читаю.\nГотово.");
}

#[test]
fn dsml_truncated_known_is_unparseable() {
    let src = "<｜DSML｜function_calls>\n<｜DSML｜invoke name=\"read_file\">\n<｜DSML｜parameter name=\"path\" string=\"true\">a.t";
    let out = only(src, TextFormat::Dsml);
    assert!(out.calls.is_empty());
    assert!(matches!(
        out.rejected[..],
        [CallRejection::Unparseable { .. }]
    ));
    assert_eq!(out.text, "");
}

#[test]
fn gemma4_quoted_strings_and_turn_close() {
    let src = "<|tool_call>call:read_file{path:<|\"|>a.txt<|\"|>}<turn|>";
    let out = only(src, TextFormat::Gemma4);
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].call.name, "read_file");
    assert_eq!(out.calls[0].call.arguments, json!({"path": "a.txt"}));
    assert_eq!(out.text, "");
}

#[test]
fn gemma4_lookalike_is_text() {
    assert_plain("call:read_file{path:a.txt}", TextFormat::Gemma4);
    assert_plain(
        "Токен <|tool_call> открывает вызов, а <tool_call|> закрывает.",
        TextFormat::Gemma4,
    );
}

#[test]
fn gemma4_nested_values_and_several_calls() {
    let src = "Сейчас.\n<|tool_call>call:count{f:true,n:3,tags:[<|\"|>a,b<|\"|>,<|\"|>}c<|\"|>]}<tool_call|><|tool_call>call:write_file{content:<|\"|>x <tool_call|> y<|\"|>,path:<|\"|>p<|\"|>}<tool_call|>";
    let out = only(src, TextFormat::Gemma4);
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(out.calls.len(), 2);
    assert_eq!(
        out.calls[0].call.arguments,
        json!({"f": true, "n": 3, "tags": ["a,b", "}c"]})
    );
    assert_eq!(
        out.calls[1].call.arguments,
        json!({"content": "x <tool_call|> y", "path": "p"})
    );
    assert_eq!(out.text, "Сейчас.");
}

#[test]
fn gemma4_truncated_and_broken() {
    let out = only(
        "<|tool_call>call:read_file{path:<|\"|>a.t",
        TextFormat::Gemma4,
    );
    assert!(out.calls.is_empty());
    assert!(matches!(
        out.rejected[..],
        [CallRejection::Unparseable { .. }]
    ));
    assert_eq!(out.text, "");
    // Синтаксическая ошибка до закрывающего тега — отклонение до него.
    let out = only(
        "<|tool_call>call:read_file{path:[}<tool_call|>\nдальше",
        TextFormat::Gemma4,
    );
    assert!(matches!(
        out.rejected[..],
        [CallRejection::Unparseable { .. }]
    ));
    assert_eq!(out.text, "дальше");
    // Обрыв с неизвестным именем — текст.
    assert_plain("<|tool_call>call:frobnicate{x:<|\"|>a", TextFormat::Gemma4);
}

#[test]
fn mistral_new_and_old_forms() {
    for src in [
        "[TOOL_CALLS]read_file[ARGS]{\"path\":\"a.txt\"}",
        "[TOOL_CALLS][{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.txt\"}}]",
        "[TOOL_CALLS]read_file[CALL_ID]a1B2c3D4e[ARGS]{\"path\":\"a.txt\"}",
        "[TOOL_CALLS]read_file{\"path\":\"a.txt\"}",
    ] {
        let out = only(src, TextFormat::MistralToolCalls);
        assert!(out.rejected.is_empty(), "{src}: {:?}", out.rejected);
        assert_eq!(out.calls.len(), 1, "{src}");
        assert_eq!(out.calls[0].call.name, "read_file");
        assert_eq!(out.calls[0].call.arguments, json!({"path": "a.txt"}));
        assert!(out.calls[0].repairs.contains(&Repair::ExtractedFromText {
            format: TextFormat::MistralToolCalls
        }));
        assert_eq!(out.text, "");
    }
}

#[test]
fn mistral_lookalike_is_text() {
    assert_plain(
        "в логе было [TOOL_CALLS] без вызова",
        TextFormat::MistralToolCalls,
    );
    assert_plain(
        "[TOOL_CALLS][1, 2] — не вызов",
        TextFormat::MistralToolCalls,
    );
}

#[test]
fn mistral_parallel_calls_and_trailing_text() {
    let src =
        "Делаю.[TOOL_CALLS]read_file[ARGS]{\"path\":\"a\"}[TOOL_CALLS]count[ARGS]{\"n\":1} и всё";
    let out = only(src, TextFormat::MistralToolCalls);
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    let names: Vec<&str> = out.calls.iter().map(|c| c.call.name.as_str()).collect();
    assert_eq!(names, ["read_file", "count"]);
    assert_eq!(out.text, "Делаю. и всё");

    let out = only(
        "[TOOL_CALLS]read_file[ARGS]{\"path\":\"a",
        TextFormat::MistralToolCalls,
    );
    assert!(matches!(
        out.rejected[..],
        [CallRejection::Unparseable { .. }]
    ));
    assert_eq!(out.text, "");
}

#[test]
fn pythonic_literals() {
    let src = "[count(n=5, tags=[\"a\",\"b\"], f=True)]";
    let out = only(src, TextFormat::Pythonic);
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(out.calls.len(), 1);
    assert_eq!(
        out.calls[0].call.arguments,
        json!({"n": 5, "tags": ["a", "b"], "f": true})
    );
    assert_eq!(out.text, "");
}

#[test]
fn pythonic_unknown_name_is_text() {
    assert_plain("[print(x)]", TextFormat::Pythonic);
    assert_plain(
        "[read_file(path=\"a\")] — так выглядит вызов",
        TextFormat::Pythonic,
    );
    assert_plain("[1, 2, 3]", TextFormat::Pythonic);
}

#[test]
fn pythonic_strings_numbers_and_tokens() {
    let src = "<|python_tag|>[write_file(path='a\\'b', content=\"\"\"line1\nline2 \\u00e9\"\"\"), count(n=1_000, tags=(), f=False,)]<|eot_id|>";
    let out = only(src, TextFormat::Pythonic);
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(out.calls.len(), 2);
    assert_eq!(
        out.calls[0].call.arguments,
        json!({"path": "a'b", "content": "line1\nline2 é"})
    );
    assert_eq!(
        out.calls[1].call.arguments,
        json!({"n": 1000, "tags": [], "f": false})
    );
    assert_eq!(out.text, "");
    // Обрыв с известным именем — отклонение.
    let out = only("[read_file(path=\"a", TextFormat::Pythonic);
    assert!(matches!(
        out.rejected[..],
        [CallRejection::Unparseable { .. }]
    ));
    assert_eq!(out.text, "");
}

#[test]
fn llama3_json_whole_content_only() {
    let out = only(
        "<|python_tag|>{\"name\":\"read_file\",\"parameters\":{\"path\":\"a.txt\"}}",
        TextFormat::Llama3Json,
    );
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].call.arguments, json!({"path": "a.txt"}));
    assert_eq!(out.text, "");
    assert_plain(
        "Ответ: {\"name\":\"read_file\",\"parameters\":{}} — пример",
        TextFormat::Llama3Json,
    );
}

#[test]
fn llama3_json_unknown_name_is_text() {
    assert_plain(
        "{\"name\":\"Вася\",\"parameters\":{}}",
        TextFormat::Llama3Json,
    );
    assert_plain("{\"name\":\"read_file\"}", TextFormat::Llama3Json);
}

#[test]
fn llama3_json_several_and_truncated() {
    let src = "{\"name\":\"read_file\",\"parameters\":{\"path\":\"a\"}}; {\"name\":\"count\",\"arguments\":{\"n\":2}}<|eom_id|>";
    let out = only(src, TextFormat::Llama3Json);
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    let names: Vec<&str> = out.calls.iter().map(|c| c.call.name.as_str()).collect();
    assert_eq!(names, ["read_file", "count"]);
    assert_eq!(out.text, "");

    let out = only(
        "{\"name\": \"read_file\", \"parameters\": {\"path\": \"a",
        TextFormat::Llama3Json,
    );
    assert!(matches!(
        out.rejected[..],
        [CallRejection::Unparseable { .. }]
    ));
    assert_eq!(out.text, "");
    assert_plain(
        "{\"name\": \"frobnicate\", \"parameters\": {",
        TextFormat::Llama3Json,
    );
}

/// Заборы и кавычки внутри тел вызовов P1 не считаются Markdown, а примеры в коде
/// не исполняются.
#[test]
fn p1_call_bodies_and_code_examples() {
    let gemma = "<|tool_call>call:write_file{content:<|\"|>```\nfn x() {}\n<|\"|>,path:<|\"|>a<|\"|>}<tool_call|>\n```\nпример\n```";
    let out = normalize(&text(gemma), &tools(), all_formats());
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].call.arguments["content"], "```\nfn x() {}\n");
    assert_eq!(out.text, "```\nпример\n```");

    let mistral = "[TOOL_CALLS]write_file[ARGS]{\"path\":\"a\",\"content\":\"`\"}[TOOL_CALLS]write_file[ARGS]{\"path\":\"b\",\"content\":\"`\"}";
    let out = normalize(&text(mistral), &tools(), all_formats());
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(out.calls.len(), 2);

    let dsml = format!(
        "{}\n```\nпример\n```",
        dsml_call(
            "write_file",
            &[("path", "true", "a"), ("content", "true", "```")]
        )
    );
    let out = normalize(&text(&dsml), &tools(), all_formats());
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].call.arguments["content"], "```");
    assert_eq!(out.text, "```\nпример\n```");

    for example in [
        "Пример:\n```\n[TOOL_CALLS]read_file[ARGS]{\"path\":\"x\"}\n```".to_owned(),
        "Пример: `<|tool_call>call:read_file{path:<|\"|>x<|\"|>}<tool_call|>`".to_owned(),
        format!(
            "Пример:\n```\n{}\n```",
            dsml_call("read_file", &[("path", "true", "x")])
        ),
    ] {
        let out = normalize(&text(&example), &tools(), all_formats());
        assert!(out.calls.is_empty() && out.rejected.is_empty(), "{example}");
        assert_eq!(out.text, example);
    }
}

/// Линейность форматов P1: ~1,2 МБ повторов укладываются в [`time_limit`].
#[test]
fn megabyte_of_repeated_p1_markers_is_linear() {
    let units: Vec<String> = vec![
        // DSML
        "<｜DSML｜invoke name=\"x\">".into(),
        "<｜DSML｜invoke name=\"read_file\">".into(),
        "<｜DSML｜invoke name=\"count\">\n<｜DSML｜parameter name=\"n\" string=\"false\">".into(),
        "<｜DSML｜function_calls>".into(),
        dsml_call("count", &[("n", "false", "1")]),
        // Gemma 4
        "<|tool_call>".into(),
        "<|tool_call>call:x{".into(),
        "<|tool_call>call:read_file{path:<|\"|>".into(),
        "<|tool_call>call:count{n:[".into(),
        "<|tool_call>call:count{n:1}<tool_call|>".into(),
        // Mistral
        "[TOOL_CALLS]".into(),
        "[TOOL_CALLS][{\"name\":\"".into(),
        "[TOOL_CALLS]x[ARGS]{".into(),
        "[TOOL_CALLS]read_file[ARGS]{\"path\":\"a\"}".into(),
        "[TOOL_CALLS]read_file[ARGS]{\"path\":[".into(),
        // pythonic
        "[read_file(path=\"a\"), ".into(),
        "[".into(),
        "[count(tags=[".into(),
        "[count(n=\"".into(),
        // llama3_json
        "{\"name\":\"read_file\",\"parameters\":{\"path\":\"a\"}};".into(),
        "{".into(),
        "<|python_tag|>".into(),
    ];
    let tools = tools();
    for unit in units {
        let src = unit.repeat(1_200_000 / unit.len());
        let started = std::time::Instant::now();
        let out = normalize(&text(&src), &tools, all_formats());
        let took = started.elapsed();
        assert!(took < time_limit(), "{unit:?}: {took:?}");
        assert!(
            out.calls.len() + out.rejected.len() <= MAX_EXTRACTED_CALLS as usize + 1,
            "{unit:?}"
        );
    }
}

/// Любой префикс ответа P1 (обрыв стрима) разбирается без паники; принятые
/// вызовы проходят схему.
#[test]
fn every_truncation_point_is_safe_p1() {
    let tools = tools();
    for src in [
        format!(
            "Т ✓.\n{}",
            dsml_call(
                "count",
                &[("n", "false", "5"), ("tags", "false", "[\"я\"]")]
            )
        ),
        "Т ✓.\n<|tool_call>call:count{n:5,tags:[<|\"|>я<|\"|>]}<tool_call|>".to_owned(),
        "Т ✓.[TOOL_CALLS]count[ARGS]{\"n\":5,\"tags\":[\"я\"]}".to_owned(),
        "[count(n=5, tags=[\"я\"])]".to_owned(),
        "<|python_tag|>{\"name\":\"count\",\"parameters\":{\"n\":5,\"tags\":[\"я\"]}}".to_owned(),
    ] {
        for (i, _) in src.char_indices() {
            let out = normalize(&text(&src[..i]), &tools, all_formats());
            assert!(out.calls.len() + out.rejected.len() <= 1, "{src} / {i}");
            for c in &out.calls {
                let schema = &tools
                    .iter()
                    .find(|t| t.name == c.call.name)
                    .unwrap()
                    .parameters;
                let mut args = c.call.arguments.clone();
                assert_eq!(validate_and_coerce(schema, &mut args), Ok(vec![]), "{i}");
            }
        }
        let full = normalize(&text(&src), &tools, all_formats());
        assert_eq!(full.calls.len(), 1, "{src}");
        assert!(full.rejected.is_empty(), "{src}");
    }
}

// ---- Задача 8, раунд исправлений 1 ----

/// Important 1: описание инструмента (OpenAI или сериализованный `ToolSchema`) —
/// не вызов llama3_json, даже если имя известно, а схема пропустила бы аргументы.
#[test]
fn llama3_json_tool_description_is_not_a_call() {
    let schema = serde_json::to_string(&tools()[1]).unwrap();
    for src in [
        schema,
        "{\"name\":\"write_file\",\"description\":\"Пишет файл\",\"parameters\":{\"type\":\"object\",\"properties\":{}}}".to_owned(),
        "{\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"parameters\":{}}}".to_owned(),
        "{\"name\":\"count\",\"parameters\":{\"n\":1},\"description\":\"x\"}".to_owned(),
        // Оборванное описание — тоже не вызов.
        "{\"name\":\"read_file\",\"description\":\"Чита".to_owned(),
    ] {
        assert_plain(&src, TextFormat::Llama3Json);
    }
}

/// Important 2: маркеры в инлайн-коде не делают тело вызова — пример в блоке кода
/// между ними остаётся скрытым и не исполняется.
#[test]
fn marker_mentions_do_not_unmask_code_examples() {
    let example = hermes("read_file", json!({"path": "EX"}));
    for src in [
        format!(
            "Токен `<|tool_call>` открывает вызов:\n```\n{example}\n```\nа `<tool_call|>` закрывает."
        ),
        format!(
            "Тег `<｜DSML｜invoke name=\"x\">` и пример:\n```\n{example}\n```\nзакрытие `</｜DSML｜invoke>`."
        ),
        format!("Тег `<function=x>`:\n```\n{example}\n```\nзакрытие `</function>`."),
        "Обёртка `<tool_call>`:\n```\n<function=read_file>\n<parameter=path>\nEX\n</parameter>\n</function>\n```\nи `</tool_call>`."
            .to_owned(),
    ] {
        let out = normalize(&text(&src), &tools(), all_formats());
        assert!(
            out.calls.is_empty() && out.rejected.is_empty(),
            "{src}: {:?} {:?}",
            out.calls,
            out.rejected
        );
        assert_eq!(out.text, src);
    }
}

/// Minor 1, 2: синтаксическая ошибка без обрыва концом текста — обычный текст
/// (правило R2), `Unparseable` — только для обрыва.
#[test]
fn syntax_errors_without_truncation_are_text() {
    for (src, format) in [
        ("[read_file(path=p) for p in paths]", TextFormat::Pythonic),
        ("[count(n=len(xs)), count(n=2)]", TextFormat::Pythonic),
        ("[read_file(\"a.txt\")]", TextFormat::Pythonic),
        (
            "{\"name\": \"read_file\", \"parameters\": {path: \"a\"}}",
            TextFormat::Llama3Json,
        ),
        (
            "Формат Mistral: [TOOL_CALLS]read_file[ARGS]{...} — далее текст\nвторая строка",
            TextFormat::MistralToolCalls,
        ),
        (
            "Модель пишет [TOOL_CALLS] read_file",
            TextFormat::MistralToolCalls,
        ),
    ] {
        assert_plain(src, format);
    }
}

/// Minor 3: обратные кавычки в двух аргументах pythonic не искажают аргументы.
#[test]
fn pythonic_backticks_in_two_arguments() {
    let out = normalize(
        &text("[write_file(path=\"`a\", content=\"b`\")]"),
        &tools(),
        all_formats(),
    );
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(out.calls.len(), 1);
    assert_eq!(
        out.calls[0].call.arguments,
        json!({"path": "`a", "content": "b`"})
    );
    let out = normalize(
        &text("{\"name\":\"write_file\",\"parameters\":{\"path\":\"`a\",\"content\":\"b`\"}}"),
        &tools(),
        all_formats(),
    );
    assert_eq!(out.calls.len(), 1);
    assert_eq!(
        out.calls[0].call.arguments,
        json!({"path": "`a", "content": "b`"})
    );
}

/// Minor 4: расхождения с vLLM — значение Gemma с `<`, варианты обёртки DSML V4,
/// префиксы строк и восьмеричное экранирование pythonic.
#[test]
fn p1_vllm_compat_details() {
    let out = only(
        "<|tool_call>call:write_file{content:a<b,path:x}<tool_call|>",
        TextFormat::Gemma4,
    );
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(
        out.calls[0].call.arguments,
        json!({"content": "a<b", "path": "x"})
    );

    let src = "<｜DSML｜toolcalls>\n<｜DSML｜invoke name=\"read_file\">\n<｜DSML｜parameter name=\"path\" string=\"true\">a</｜DSML｜parameter>\n</｜DSML｜invoke>\n</｜DSML｜tool_calls>";
    let out = only(src, TextFormat::Dsml);
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.text, "");

    let out = only(
        "[write_file(path=r\"C:\\new\\\"q\", content=b'\\101\\0x' u\"\\x41\")]",
        TextFormat::Pythonic,
    );
    // Неявная склейка строк не поддерживается — ошибка синтаксиса, текст.
    assert!(out.calls.is_empty() && out.rejected.is_empty());
    let out = only(
        "[write_file(path=r\"C:\\new\\\"q\", content=b'\\101\\0x')]",
        TextFormat::Pythonic,
    );
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    assert_eq!(
        out.calls[0].call.arguments,
        json!({"path": "C:\\new\\\"q", "content": "A\u{0}x"})
    );
}

/// Раунд 2: тег Gemma внутри значения без кавычек — не часть значения: `set` не
/// исполняется с мусором, следующий настоящий вызов извлекается.
#[test]
fn gemma4_tag_inside_bare_value_is_not_value() {
    let mut tools = tools();
    tools.push(tool("set", json!({"type": "object"})));
    let src = "<|tool_call>call:set{a:x<tool_call|> текст <|tool_call>call:read_file{path:<|\"|>a<|\"|>}<tool_call|>";
    let out = normalize(&text(src), &tools, &[TextFormat::Gemma4]);
    let names: Vec<&str> = out.calls.iter().map(|c| c.call.name.as_str()).collect();
    assert_eq!(names, ["read_file"]);
    assert!(matches!(
        out.rejected[..],
        [CallRejection::Unparseable { .. }]
    ));
    assert_eq!(out.text, "текст");
    for tag in ["<turn|>", "<|tool_call>"] {
        let src = format!("<|tool_call>call:set{{a:x{tag}}}");
        let out = normalize(&text(&src), &tools, &[TextFormat::Gemma4]);
        assert!(out.calls.is_empty(), "{src}: {:?}", out.calls);
    }
}
