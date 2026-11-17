//! Таксономия записи (R1-M D145; часть 04, §9.1; MEM-078; П8 и D172 изменений по ZAM
//! 0.0.12-m6.5): таблица синтетических фикстур `fixtures/taxonomy/*.json` — строка на правило.

use std::path::Path;

use serde_json::{Value, json};
use za_memory::gate::WriteOp;
use za_memory::taxonomy::{self, Flags, IDEA_SERVER_KEYS};

/// Операция из фикстуры.
fn op_of(case: &Value) -> WriteOp {
    let a = &case["args"];
    let s = |k: &str| a[k].as_str().unwrap_or_default().to_owned();
    match case["op"].as_str().unwrap() {
        "log" => WriteOp::Log(a.clone()),
        "supersede" => WriteOp::Supersede {
            id: s("id"),
            body: a["body"].clone(),
        },
        "retract" => WriteOp::Retract {
            id: s("id"),
            reason: s("reason"),
        },
        "review" => WriteOp::Review {
            id: s("id"),
            body: a["body"].clone(),
        },
        "next_step" => WriteOp::NextStep {
            id: s("id"),
            text: s("text"),
        },
        other => panic!("неизвестная операция фикстуры: {other}"),
    }
}

#[test]
fn taxonomy_table() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/taxonomy");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(files.len() >= 25, "фикстур мало: {}", files.len());
    for f in files {
        let case: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        let name = case["case"].as_str().unwrap_or("?");
        let flags = Flags {
            idea_review: case["flags"]["idea_review"].as_bool().unwrap_or(false),
        };
        let got: Result<(), String> = if case["op"] == "filters" {
            taxonomy::check_filters(&case["args"], flags).map_err(|e| e.0)
        } else {
            let mut op = op_of(&case);
            taxonomy::check(&mut op, flags).map(|_| ()).map_err(|e| e.0)
        };
        match case["error"].as_str() {
            Some(want) => {
                let e = got.expect_err(name);
                assert!(e.contains(want), "{name}: «{e}» без «{want}»");
                assert!(e.is_ascii(), "{name}: текст модели не ASCII: {e}");
            }
            None => assert!(got.is_ok(), "{name}: {got:?}"),
        }
    }
}

#[test]
fn idea_server_fields_refused_before_network() {
    for k in IDEA_SERVER_KEYS {
        let mut op = WriteOp::Log(json!({
            "kind": "idea", "title": "Synthetic idea", "data": {"origin": "agent", k: 1}
        }));
        let e = taxonomy::check(&mut op, Flags::default()).unwrap_err();
        assert!(e.0.contains(&format!("data.{k}")), "{k}: {e}");
        let mut op = WriteOp::Patch {
            id: "01M8SYNIDEA00000000000001".into(),
            rev: 2,
            body: json!({"data": {k: null}}),
        };
        let e = taxonomy::check(&mut op, Flags::default()).unwrap_err();
        assert!(e.0.contains(&format!("data.{k}")), "patch {k}: {e}");
    }
}

#[test]
fn tags_sorted_and_idea_origin_defaulted() {
    let mut op = WriteOp::Log(json!({
        "kind": "idea", "title": "Synthetic idea", "tags": ["b", " a ", "b", ""], "data": {}
    }));
    taxonomy::check(&mut op, Flags::default()).unwrap();
    let WriteOp::Log(v) = op else { unreachable!() };
    assert_eq!(v["tags"], json!(["a", "b"]));
    // Своя мысль модели — `origin: agent` (D148); `human` закрывает функция решения.
    assert_eq!(v["data"]["origin"], "agent");
}

#[test]
fn certainty_only_on_insight() {
    let mut op = WriteOp::Log(json!({"kind": "task", "title": "T", "certainty": "theorem"}));
    let e = taxonomy::check(&mut op, Flags::default()).unwrap_err();
    assert!(e.0.contains("only an insight has certainty"), "{e}");
}
