//! Область памяти run — `ScopeGuard` (R1-M D164; MEM-013; П10 изменений по ZAM 0.0.12-m6.5):
//! все параметры, задающие пространство; чужое — отказ без запроса.

use serde_json::{Value, json};
use za_memory::scope::READ_ONLY;
use za_memory::{RunBinding, ScopeAccess, ScopeGuard};

/// Решение run (G).
const G: &str = "6f1c2b3a-0d4e-4f5a-8b6c-7d8e9f0a1b2c";
/// Чужое решение пользователя.
const FOREIGN: &str = "0a0b0c0d-1111-4222-8333-444455556666";

fn guard(solution: Option<&str>, writable: bool) -> ScopeGuard {
    ScopeGuard::new(&RunBinding {
        project_id: "0123456789abcdef".into(),
        solution: solution.map(str::to_owned),
        area: None,
        writable,
    })
}

#[test]
fn scope_table() {
    use ScopeAccess::{Read, Write};
    let g = guard(Some(G), true);
    // (инструмент, аргументы, доступ, отказ: подстрока или None — можно)
    let rows: Vec<(&str, Value, ScopeAccess, Option<&str>)> = vec![
        (
            "memory_search",
            json!({"query": "q", "scope": {"all_solutions": true}}),
            Read,
            Some("all_solutions is outside"),
        ),
        (
            "memory_search",
            json!({"query": "q", "scope": {"solution": FOREIGN}}),
            Read,
            Some("is outside this project"),
        ),
        (
            "memory_search",
            json!({"query": "q", "scope": {"spaces": ["knowledge", FOREIGN]}}),
            Read,
            Some(FOREIGN),
        ),
        (
            "memory_search",
            json!({"query": "q", "scope": {"include_human": true}}),
            Read,
            Some("human is outside"),
        ),
        (
            "memory_search",
            json!({"query": "q", "scope": {"path_prefix": format!("{FOREIGN}:/docs")}}),
            Read,
            Some("is outside"),
        ),
        (
            "memory_tree",
            json!({"space": FOREIGN}),
            Read,
            Some("is outside this project"),
        ),
        (
            "memory_read",
            json!({"items": [{"id": format!("{FOREIGN}:/a")}]}),
            Read,
            Some("is outside"),
        ),
        (
            "memory_read",
            json!({"id": format!("{FOREIGN}:/a")}),
            Read,
            Some("is outside"),
        ),
        (
            "memory_write",
            json!({"path": "x", "space": "rules"}),
            Write,
            Some("rules is read-only"),
        ),
        (
            "memory_write",
            json!({"path": format!("{FOREIGN}:/x")}),
            Write,
            Some("is outside"),
        ),
        (
            "memory_log",
            json!({"kind": "insight", "space": FOREIGN}),
            Write,
            Some("is outside"),
        ),
        (
            "memory_log",
            json!({"kind": "insight", "space": "human"}),
            Write,
            Some("is outside"),
        ),
        (
            "memory_update_entry",
            json!({"id": "w:01A", "action": "propose", "space": FOREIGN}),
            Write,
            Some("is outside"),
        ),
        (
            "memory_update_entry",
            json!({"id": "w:01A", "action": "decide", "space": FOREIGN}),
            Write,
            Some("is outside"),
        ),
        // П10: разбор — только пространство идеи (решение run).
        (
            "memory_update_entry",
            json!({"id": "w:01A", "action": "review", "space": "knowledge"}),
            Write,
            Some("review stays in the idea's space"),
        ),
        (
            "memory_update_entry",
            json!({"id": "w:01A", "action": "review", "space": G}),
            Write,
            None,
        ),
        (
            "memory_cache_put",
            json!({"query": "q", "summary": "s", "sources": [{"id": format!("{FOREIGN}:/a")}]}),
            Write,
            Some("is outside"),
        ),
        (
            "memory_cache_put",
            json!({"query": "q", "summary": "s", "sources": [{"id": "rules:/a"}, {"id": "n:01A"}]}),
            Write,
            None,
        ),
        // Можно.
        (
            "memory_search",
            json!({"query": "q", "scope": {"spaces": ["knowledge", G]}}),
            Read,
            None,
        ),
        (
            "memory_search",
            json!({"query": "q", "scope": {"solution": G.to_uppercase(), "include_rules": true}}),
            Read,
            None,
        ),
        (
            "memory_read",
            json!({"items": [{"id": "knowledge:/x"}, {"id": "n:01M8SYNNODE00000000000001"}]}),
            Read,
            None,
        ),
        ("memory_tree", json!({"space": "rules"}), Read, None),
        (
            "memory_log",
            json!({"kind": "insight", "scope": "global"}),
            Write,
            None,
        ),
        (
            "memory_write",
            json!({"path": format!("{G}:/notes/a")}),
            Write,
            None,
        ),
        // Голые id не проверяются (принятый риск D164).
        (
            "memory_update_entry",
            json!({"id": "i:01M8SYNENTRY0000000000001", "action": "append"}),
            Write,
            None,
        ),
        // Неизвестная форма — отказ.
        (
            "memory_tree",
            json!({"space": "elsewhere"}),
            Read,
            Some("unknown space form"),
        ),
        (
            "memory_tree",
            json!({"space": 7}),
            Read,
            Some("space must be a string"),
        ),
        (
            "memory_search",
            json!({"query": "q", "scope": {"spaces": [1]}}),
            Read,
            Some("must be a string"),
        ),
        (
            "memory_search",
            json!({"query": "q", "scope": {"spaces": "knowledge"}}),
            Read,
            Some("must be an array"),
        ),
        (
            "memory_tree",
            json!({"space": "01M8SYNSPACE0000000000001"}),
            Read,
            Some("unknown space form"),
        ),
    ];
    for (tool, args, a, want) in rows {
        let got = g.check_args(tool, &args, a);
        match want {
            Some(w) => {
                let e = got.expect_err(&format!("{tool} {args}"));
                assert!(e.contains(w), "{tool} {args}: «{e}» без «{w}»");
                assert!(e.starts_with("memory scope: "), "{e}");
            }
            None => assert!(got.is_ok(), "{tool} {args}: {got:?}"),
        }
    }
}

#[test]
fn refusal_names_allowed_spaces() {
    let e = guard(Some(G), true)
        .check_space(FOREIGN, ScopeAccess::Read)
        .unwrap_err();
    assert_eq!(
        e,
        format!("memory scope: {FOREIGN} is outside this project; allowed: {G}, knowledge, rules")
    );
    let e = guard(None, true)
        .check_space(FOREIGN, ScopeAccess::Write)
        .unwrap_err();
    assert!(e.ends_with("allowed: knowledge"), "{e}");
}

#[test]
fn untrusted_project_reads_knowledge_and_rules_only() {
    let g = guard(None, false);
    assert!(g.check_space("knowledge", ScopeAccess::Read).is_ok());
    assert!(g.check_space("rules", ScopeAccess::Read).is_ok());
    assert!(g.check_space(G, ScopeAccess::Read).is_err());
    assert_eq!(
        g.check_space("knowledge", ScopeAccess::Write).unwrap_err(),
        READ_ONLY
    );
}
