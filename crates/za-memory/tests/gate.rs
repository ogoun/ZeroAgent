//! Одна функция решения шлюза записи (R1-M D163, D145; D170 и П6 изменений по ZAM 0.0.12-m6.5):
//! таблица `gate_table` — строка на правило, чистая функция, без сети.

use serde_json::{Value, json};
use za_memory::gate::{
    CACHE_TAINTED, FRONTMATTER_PATCH, HUMAN_ONLY, NEEDS_SOLUTION, NO_SOLUTION, NOTE_EVIDENCE,
    NOTE_FRONTMATTER, NOTE_PROVENANCE, NOTE_TAINTED_HYPOTHESIS, NOTE_TAINTED_RELATIONS,
    NOTE_TAINTED_TRIGGERS, PROMOTE, RULES_READ_ONLY, TAINTED, TargetTrust, Verdict, WriteCx,
    WriteOp, decide, text_samples,
};
use za_memory::scope::READ_ONLY;
use za_memory::{RunBinding, ScopeGuard};
use za_protocol::{EffectKey, RunId, taint_origin};

const SOLUTION: &str = "6f1c2b3a-0d4e-4f5a-8b6c-7d8e9f0a1b2c";
const FOREIGN: &str = "0a0b0c0d-1111-4222-8333-444455556666";
const ID: &str = "01M8SYNENTRY0000000000001";

fn cx(untrusted: bool) -> WriteCx {
    let run = RunId::new();
    WriteCx {
        run,
        session: format!("za-0123456789abcdef-{run}"),
        key: EffectKey::derive(run, 3, 1),
        untrusted,
        origins: if untrusted {
            vec![taint_origin::WEB.to_owned()]
        } else {
            Vec::new()
        },
        area: None,
        deadline: None,
        scope_global: false,
    }
}

fn guard(solution: bool, writable: bool) -> ScopeGuard {
    ScopeGuard::new(&RunBinding {
        project_id: "0123456789abcdef".into(),
        solution: solution.then(|| SOLUTION.to_owned()),
        area: None,
        writable,
    })
}

fn trusted() -> TargetTrust {
    TargetTrust {
        certainty: Some("theorem".into()),
        kind: Some("insight".into()),
        ..TargetTrust::default()
    }
}

/// Ожидание строки таблицы.
enum Expect {
    Refuse(&'static str),
    /// Разрешено без пометок; проверка тела.
    Allow(fn(&Value)),
    /// Разрешено с пометкой; проверка тела.
    Downgrade(&'static str, fn(&Value)),
}

struct Row {
    name: &'static str,
    op: WriteOp,
    target: Option<TargetTrust>,
    untrusted: bool,
    solution: bool,
    writable: bool,
    evidence_ok: bool,
    expect: Expect,
}

fn row(name: &'static str, op: WriteOp, expect: Expect) -> Row {
    Row {
        name,
        op,
        target: None,
        untrusted: false,
        solution: true,
        writable: true,
        evidence_ok: false,
        expect,
    }
}

fn tainted(mut r: Row) -> Row {
    r.untrusted = true;
    r
}

fn on(mut r: Row, t: TargetTrust) -> Row {
    r.target = Some(t);
    r
}

fn insight(extra: Value) -> Value {
    let mut v =
        json!({"kind": "insight", "title": "Synthetic insight", "data": {"kind": "discovery"}});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

fn theorem() -> Value {
    insight(json!({
        "certainty": "theorem",
        "data": {"kind": "discovery", "evidence": {"checked": "call:c1", "method": "cargo test", "result": "green"}},
    }))
}

#[test]
fn gate_table() {
    let rows = vec![
        // Правило 1: cache_put.
        tainted(row(
            "cache_put из tainted",
            WriteOp::CachePut(json!({"query": "q", "summary": "s", "sources": [{"id": "n:01A"}]})),
            Expect::Refuse(CACHE_TAINTED),
        )),
        row(
            "cache_put с promote",
            WriteOp::CachePut(
                json!({"query": "q", "summary": "s", "sources": [{"id": "n:01A"}], "promote": {"as": "note"}}),
            ),
            Expect::Refuse(PROMOTE),
        ),
        row(
            "cache_put с чужим источником",
            WriteOp::CachePut(
                json!({"query": "q", "summary": "s", "sources": [{"id": format!("{FOREIGN}:/a")}]}),
            ),
            Expect::Refuse("is outside this project"),
        ),
        // Правило 2: frontmatter заметок.
        row(
            "frontmatter certainty: axiom и verified_at в заметке",
            WriteOp::Note {
                path: "notes/a".into(),
                body: json!({"content": "---\ncertainty: axiom\nverified_at: 2026-11-01\ntags: [x]\n---\n# A\n"}),
            },
            Expect::Downgrade(NOTE_FRONTMATTER, |b| {
                let c = b["content"].as_str().unwrap();
                assert!(!c.contains("axiom") && !c.contains("verified_at"), "{c}");
                assert!(c.contains("tags: [x]"), "{c}");
            }),
        ),
        row(
            "PATCH ops по frontmatter",
            WriteOp::NodePatch {
                id: "01M8SYNNODE00000000000001".into(),
                rev: 2,
                ops: json!([{"op": "prepend", "content": "---\ncertainty: axiom\n---\n"}]),
            },
            Expect::Refuse(FRONTMATTER_PATCH),
        ),
        tainted(row(
            "relations во frontmatter из tainted",
            WriteOp::Note {
                path: "notes/a".into(),
                body: json!({"content": "---\nrelations:\n- links_to [[B]]\n---\n# A\n"}),
            },
            Expect::Downgrade(NOTE_TAINTED_RELATIONS, |b| {
                let c = b["content"].as_str().unwrap();
                assert!(!c.contains("relations") && !c.contains("links_to"), "{c}");
                assert!(c.contains("certainty: hypothesis"), "{c}");
            }),
        )),
        row(
            "certainty заметки ставит шлюз из аргументов",
            WriteOp::Note {
                path: "knowledge:/notes/a".into(),
                body: json!({"content": "# A\n", "certainty": "hypothesis"}),
            },
            Expect::Allow(|b| {
                assert_eq!(b["content"], "---\ncertainty: hypothesis\n---\n# A\n");
                assert_eq!(
                    (b["space"].as_str(), b["path"].as_str()),
                    (Some("knowledge"), Some("notes/a"))
                );
            }),
        ),
        // Правило 3: доверенные цели из tainted-контекста.
        on(
            tainted(row(
                "append цели theorem из tainted",
                WriteOp::Patch {
                    id: ID.into(),
                    rev: 3,
                    body: json!({"append_body": "more"}),
                },
                Expect::Refuse(TAINTED),
            )),
            trusted(),
        ),
        on(
            tainted(row(
                "PATCH status цели theorem из tainted",
                WriteOp::Patch {
                    id: ID.into(),
                    rev: 3,
                    body: json!({"status": "revisit"}),
                },
                Expect::Refuse(TAINTED),
            )),
            trusted(),
        ),
        on(
            tainted(row(
                "supersede цели theorem из tainted",
                WriteOp::Supersede {
                    id: ID.into(),
                    body: insight(json!({"reason": "the old check was wrong"})),
                },
                Expect::Refuse(TAINTED),
            )),
            trusted(),
        ),
        on(
            tainted(row(
                "retract цели theorem из tainted",
                WriteOp::Retract {
                    id: ID.into(),
                    reason: "the old check was wrong".into(),
                },
                Expect::Refuse(TAINTED),
            )),
            trusted(),
        ),
        on(
            tainted(row(
                "PUT заметки theorem из tainted",
                WriteOp::Note {
                    path: "notes/a".into(),
                    body: json!({"content": "x", "base_rev": 2}),
                },
                Expect::Refuse(TAINTED),
            )),
            trusted(),
        ),
        on(
            tainted(row(
                "повышение certainty из tainted",
                WriteOp::Patch {
                    id: ID.into(),
                    rev: 1,
                    body: json!({"certainty": "theorem", "reason": "checked"}),
                },
                Expect::Refuse(TAINTED),
            )),
            TargetTrust {
                certainty: Some("hypothesis".into()),
                ..TargetTrust::default()
            },
        ),
        tainted(row(
            "разбор идеи из tainted (D170)",
            WriteOp::Review {
                id: ID.into(),
                body: json!({"outcome": "decline", "summary": "s"}),
            },
            Expect::Refuse(TAINTED),
        )),
        // Правило 4: подделка человека.
        row(
            "idea origin: human",
            WriteOp::Log(
                json!({"kind": "idea", "title": "Synthetic idea", "data": {"origin": "human"}}),
            ),
            Expect::Refuse(HUMAN_ONLY),
        ),
        row(
            "answer by: human",
            WriteOp::Answer {
                id: ID.into(),
                body: json!({"option": "a", "by": "human"}),
            },
            Expect::Refuse(HUMAN_ONLY),
        ),
        row(
            "decide by: human",
            WriteOp::Decide {
                id: ID.into(),
                body: json!({"decision": "accept", "by": "human"}),
            },
            Expect::Refuse(HUMAN_ONLY),
        ),
        row(
            "append идеи by: human (П6)",
            WriteOp::Patch {
                id: ID.into(),
                rev: 2,
                body: json!({"append_body": "words", "by": "human"}),
            },
            Expect::Refuse(HUMAN_ONLY),
        ),
        row(
            "certainty: axiom",
            WriteOp::Log(insight(json!({"certainty": "axiom"}))),
            Expect::Refuse(HUMAN_ONLY),
        ),
        row(
            "data.decision.decided_by: human",
            WriteOp::Log(insight(
                json!({"data": {"kind": "decision", "decision": {"chosen": "a", "decided_by": "human"}}}),
            )),
            Expect::Refuse(HUMAN_ONLY),
        ),
        row(
            "answer без by получает by: agent",
            WriteOp::Answer {
                id: ID.into(),
                body: json!({"option": "a"}),
            },
            Expect::Allow(|b| assert_eq!(b["by"], "agent")),
        ),
        row(
            "правка by: agent — поле не шлётся (П6)",
            WriteOp::Patch {
                id: ID.into(),
                rev: 2,
                body: json!({"append_body": "x", "by": "agent"}),
            },
            Expect::Allow(|b| assert!(b.get("by").is_none(), "{b}")),
        ),
        // Правило 5: теги происхождения.
        row(
            "теги src:x и tainted от модели сняты",
            WriteOp::Log(insight(json!({"tags": ["src:x", "tainted", "b", "a"]}))),
            Expect::Downgrade(NOTE_PROVENANCE, |b| {
                assert_eq!(b["tags"], json!(["a", "b"]));
                assert_eq!(b["data"]["origin"]["source_kind"], "agent");
                assert_eq!(b["data"]["origin"]["tainted"], false);
            }),
        ),
        row(
            "задача: тег src:agent и message",
            WriteOp::Log(json!({"kind": "task", "title": "Synthetic task", "tags": ["z"]})),
            Expect::Allow(|b| {
                assert_eq!(b["tags"], json!(["src:agent", "z"]));
                assert!(
                    b["message"]
                        .as_str()
                        .unwrap()
                        .starts_with("zero-agent: run "),
                    "{b}"
                );
                assert!(
                    b.get("data").is_none_or(|d| d.get("origin").is_none()),
                    "{b}"
                );
            }),
        ),
        // Правило 6: theorem.
        row(
            "theorem с evidence.checked call:c1 без tool.finished",
            WriteOp::Log(theorem()),
            Expect::Downgrade(NOTE_EVIDENCE, |b| assert_eq!(b["certainty"], "hypothesis")),
        ),
        {
            let mut r = row(
                "theorem с проверенным вызовом",
                WriteOp::Log(theorem()),
                Expect::Allow(|b| assert_eq!(b["certainty"], "theorem")),
            );
            r.evidence_ok = true;
            r
        },
        // Правило 7: область.
        row(
            "space rules",
            WriteOp::Log(insight(json!({"space": "rules"}))),
            Expect::Refuse(RULES_READ_ONLY),
        ),
        row(
            "чужое решение",
            WriteOp::Log(insight(json!({"space": FOREIGN}))),
            Expect::Refuse("is outside this project"),
        ),
        {
            let mut r = row(
                "changelog без решения",
                WriteOp::Log(
                    json!({"kind": "changelog", "title": "Synthetic change", "data": {"kind": "fix"}}),
                ),
                Expect::Refuse(NEEDS_SOLUTION),
            );
            r.solution = false;
            r
        },
        {
            let mut r = row(
                "report без решения со scope global",
                WriteOp::Log(
                    json!({"kind": "report", "title": "Synthetic report", "body": "b", "scope": "global"}),
                ),
                Expect::Refuse("needs a memory solution"),
            );
            r.solution = false;
            r
        },
        {
            let mut r = row(
                "insight без решения и без scope global",
                WriteOp::Log(insight(json!({}))),
                Expect::Refuse(NO_SOLUTION),
            );
            r.solution = false;
            r
        },
        {
            let mut r = row(
                "недоверенный проект",
                WriteOp::Log(insight(json!({}))),
                Expect::Refuse(READ_ONLY),
            );
            r.writable = false;
            r.solution = false;
            r
        },
        row(
            "разбор в чужом пространстве (П10)",
            WriteOp::Review {
                id: ID.into(),
                body: json!({"outcome": "decline", "summary": "s", "space": "knowledge"}),
            },
            Expect::Refuse("review stays in the idea's space"),
        ),
        row(
            "запись в решение run по умолчанию",
            WriteOp::Log(insight(json!({}))),
            Expect::Allow(|b| {
                assert_eq!(b["space"], SOLUTION);
                assert_eq!(b["certainty"], "hypothesis");
            }),
        ),
    ];
    for r in rows {
        let cx = cx(r.untrusted);
        let g = guard(r.solution, r.writable);
        let v = decide(&r.op, r.target.as_ref(), &cx, &g, r.evidence_ok);
        match (&r.expect, &v) {
            (Expect::Refuse(want), Verdict::Refuse(got)) => {
                assert!(got.contains(want), "{}: «{got}» без «{want}»", r.name);
                assert!(got.is_ascii(), "{}: не ASCII: {got}", r.name);
            }
            (Expect::Allow(check), Verdict::Allow(body)) => check(body),
            (Expect::Downgrade(note, check), Verdict::Downgrade { body, notes }) => {
                assert!(notes.iter().any(|n| n == note), "{}: {notes:?}", r.name);
                check(body);
            }
            _ => panic!("{}: неожиданный итог {v:?}", r.name),
        }
    }
    // Тексты шлюза — модели: ASCII, все в образцах хэша промптов.
    for (k, t) in text_samples() {
        assert!(t.is_ascii(), "{k}: {t}");
    }
}

#[test]
fn tainted_forced_to_hypothesis() {
    let op = WriteOp::Log(insight(json!({
        "certainty": "theorem",
        "data": {"kind": "gotcha", "evidence": {"checked": "call:c1", "method": "m", "result": "r"}},
        "triggers": [{"type": "file_glob", "pattern": "**/*.rs"}],
        "tags": ["a"],
    })));
    let cx = cx(true);
    // Даже с проверенным вызовом: в tainted-контексте — только hypothesis (MEM-077).
    let Verdict::Downgrade { body, notes } = decide(&op, None, &cx, &guard(true, true), true)
    else {
        panic!("ожидалось понижение");
    };
    assert_eq!(body["certainty"], "hypothesis");
    assert!(body.get("triggers").is_none(), "{body}");
    assert_eq!(body["tags"], json!(["a", "tainted"]));
    assert_eq!(body["data"]["origin"]["tainted"], true);
    assert_eq!(body["data"]["origin"]["source_kind"], "web");
    assert!(
        notes.iter().any(|n| n == NOTE_TAINTED_HYPOTHESIS),
        "{notes:?}"
    );
    assert!(
        notes.iter().any(|n| n == NOTE_TAINTED_TRIGGERS),
        "{notes:?}"
    );
}

#[test]
fn body_is_deterministic() {
    // Повтор того же вызова даёт то же тело байт в байт (D145): происхождение без времени,
    // теги отсортированы.
    let op = WriteOp::Log(json!({"kind": "task", "title": "T", "tags": ["b", "a"]}));
    let cx = cx(false);
    let g = guard(true, true);
    let a = decide(&op, None, &cx, &g, false);
    let b = decide(&op, None, &cx, &g, false);
    assert_eq!(
        serde_json::to_string(&a.into_parts().unwrap().0).unwrap(),
        serde_json::to_string(&b.into_parts().unwrap().0).unwrap()
    );
}

#[test]
fn supersede_reason_goes_to_message() {
    let op = WriteOp::Supersede {
        id: ID.into(),
        body: insight(json!({"reason": "the old check was wrong", "scope": "global"})),
    };
    let target = TargetTrust {
        kind: Some("insight".into()),
        ..TargetTrust::default()
    };
    let (b, _) = decide(&op, Some(&target), &cx(false), &guard(true, true), false)
        .into_parts()
        .unwrap();
    assert_eq!(b["message"], "the old check was wrong");
    assert!(
        b.get("reason").is_none() && b.get("scope").is_none() && b.get("kind").is_none(),
        "{b}"
    );
    assert!(b["data"]["origin"].is_object(), "{b}");
}

#[test]
fn patch_tags_keep_target_provenance() {
    let op = WriteOp::Patch {
        id: ID.into(),
        rev: 2,
        body: json!({"tags": ["x", "src:web"]}),
    };
    let target = TargetTrust {
        tags: vec!["src:web".into(), "tainted".into(), "old".into()],
        ..TargetTrust::default()
    };
    let Verdict::Downgrade { body, notes } =
        decide(&op, Some(&target), &cx(false), &guard(true, true), false)
    else {
        panic!("ожидалась пометка");
    };
    // Теги происхождения цели не теряются при замене набора тегов (отмывание метки tainted).
    assert_eq!(body["tags"], json!(["src:web", "tainted", "x"]));
    assert!(notes.iter().any(|n| n == NOTE_PROVENANCE), "{notes:?}");
}

#[test]
fn tainted_term_certainty_lowered_and_no_default_status() {
    let term = |extra: Value| {
        let mut v = json!({"kind": "term", "title": "Synthetic term", "data": {"definition": "d"}});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        WriteOp::Log(v)
    };
    let Verdict::Downgrade { body, notes } = decide(
        &term(json!({"certainty": "theorem"})),
        None,
        &cx(true),
        &guard(true, true),
        true,
    ) else {
        panic!("ожидалось понижение");
    };
    assert_eq!(body["certainty"], "hypothesis");
    assert!(
        notes.iter().any(|n| n == NOTE_TAINTED_HYPOTHESIS),
        "{notes:?}"
    );
    // У термина нет `data.origin` (строгая `data`): происхождение — тегами.
    assert_eq!(body["tags"], json!(["src:web", "tainted"]));
    assert!(body["data"].get("origin").is_none(), "{body}");
    // Без `certainty` статус термину не ставится.
    let (body, _) = decide(
        &term(json!({})),
        None,
        &cx(false),
        &guard(true, true),
        false,
    )
    .into_parts()
    .unwrap();
    assert!(body.get("certainty").is_none(), "{body}");
}
