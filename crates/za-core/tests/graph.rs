//! Граф узлов с переходами `Goto`, `Suspend`, `Done` (D2) и прерывания run
//! с детерминированными id (CORE-007, D16).

use std::path::PathBuf;

use serde_json::json;
use za_core::interrupt::{self, InterruptState};
use za_core::{
    BoxFuture, Exit, Graph, GraphError, Interrupt, InterruptKind, Node, NodeId, Transition,
    pause_reason_for,
};
use za_protocol::{EffectKey, RunId, RunRole, pause_reason};
use za_store::{NewRun, ProjectStore};

/// Чистый каталог теста внутри `CARGO_TARGET_TMPDIR`.
fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("za-core-graph")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Состояние тестового графа.
#[derive(Default)]
struct St {
    count: u32,
}

/// Узел с переходом, вычисляемым из состояния.
struct Fn_ {
    id: NodeId,
    step: fn(&mut St) -> Transition,
}

impl Node<St, String> for Fn_ {
    fn id(&self) -> NodeId {
        self.id
    }

    fn run<'a>(&'a self, st: &'a mut St) -> BoxFuture<'a, Result<Transition, String>> {
        Box::pin(async move { Ok((self.step)(st)) })
    }
}

fn node(id: NodeId, step: fn(&mut St) -> Transition) -> Box<dyn Node<St, String>> {
    Box::new(Fn_ { id, step })
}

fn a_counts(st: &mut St) -> Transition {
    st.count += 1;
    if st.count < 3 {
        Transition::Goto("b")
    } else {
        Transition::Done
    }
}

fn to_a(_: &mut St) -> Transition {
    Transition::Goto("a")
}

fn to_b(_: &mut St) -> Transition {
    Transition::Goto("b")
}

fn done(_: &mut St) -> Transition {
    Transition::Done
}

fn ask(_: &mut St) -> Transition {
    Transition::Suspend(sample_interrupt())
}

fn sample_interrupt() -> Interrupt {
    Interrupt::for_effect(
        InterruptKind::Input,
        &EffectKey("r:1:0".into()),
        json!({"question": "?"}),
    )
}

#[tokio::test]
async fn graph_runs_until_done() {
    let g = Graph::new("a")
        .node(node("a", a_counts))
        .node(node("b", to_a))
        .edge("a", "b")
        .edge("b", "a");
    let mut st = St::default();
    let exit = g.run_from("a", &mut st).await;
    assert!(matches!(exit, Ok(Exit::Done)), "{exit:?}");
    assert_eq!(st.count, 3);
}

#[tokio::test]
async fn undeclared_edge_is_error() {
    let g = Graph::new("a").node(node("a", to_b)).node(node("b", done));
    let mut st = St::default();
    let exit = g.run_from("a", &mut st).await;
    assert!(
        matches!(exit, Err(GraphError::UndeclaredEdge { from: "a", to: "b" })),
        "{exit:?}"
    );
}

#[tokio::test]
async fn unknown_node_is_error() {
    let g: Graph<St, String> = Graph::new("a").node(node("a", done));
    let mut st = St::default();
    let exit = g.run_from("x", &mut st).await;
    assert!(
        matches!(exit, Err(GraphError::UnknownNode("x"))),
        "{exit:?}"
    );
    // Ребро объявлено, а узла нет.
    let g = Graph::new("a").node(node("a", to_b)).edge("a", "b");
    let exit = g.run_from("a", &mut st).await;
    assert!(
        matches!(exit, Err(GraphError::UnknownNode("b"))),
        "{exit:?}"
    );
}

#[tokio::test]
async fn node_error_is_wrapped() {
    struct Fail;
    impl Node<St, String> for Fail {
        fn id(&self) -> NodeId {
            "f"
        }
        fn run<'a>(&'a self, _: &'a mut St) -> BoxFuture<'a, Result<Transition, String>> {
            Box::pin(async { Err("сломался".to_owned()) })
        }
    }
    let g = Graph::new("f").node(Box::new(Fail));
    let exit = g.run_from("f", &mut St::default()).await;
    match exit {
        Err(GraphError::Node(e)) => assert_eq!(e, "сломался"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn suspend_returns_interrupt() {
    let g = Graph::new("a").node(node("a", ask));
    let mut st = St::default();
    match g.run_from("a", &mut st).await {
        Ok(Exit::Suspended(i)) => assert_eq!(i, sample_interrupt()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn graph_hash_depends_on_edges() {
    let build = || {
        Graph::<St, String>::new("a")
            .node(node("a", done))
            .node(node("b", done))
            .edge("a", "b")
    };
    let h1 = build().graph_hash();
    let h2 = build().graph_hash();
    assert_eq!(h1, h2);
    assert_eq!(h1.len(), 64);
    let more = build().edge("b", "a").graph_hash();
    assert_ne!(h1, more);
    let reordered = Graph::<St, String>::new("a")
        .edge("a", "b")
        .node(node("b", done))
        .node(node("a", done))
        .graph_hash();
    assert_eq!(h1, reordered);
    // Вход тоже входит в хэш.
    let other_entry = Graph::<St, String>::new("b")
        .node(node("a", done))
        .node(node("b", done))
        .edge("a", "b")
        .graph_hash();
    assert_ne!(h1, other_entry);
}

#[test]
fn interrupt_ids_are_deterministic() {
    let k = EffectKey("r:1:2".into());
    assert_eq!(
        Interrupt::for_effect(InterruptKind::Approval, &k, json!({})).id,
        "appr-r:1:2"
    );
    assert_eq!(
        Interrupt::for_effect(InterruptKind::UnknownOutcome, &k, json!({})).id,
        "unk-r:1:2"
    );
    assert_eq!(pause_reason_for(InterruptKind::Timer), "waiting");
}

#[test]
fn interrupt_kinds_map_to_ids_and_reasons() {
    let k = EffectKey("r:0:1".into());
    let cases = [
        (
            InterruptKind::Approval,
            "approval",
            "appr-r:0:1",
            pause_reason::NEEDS_APPROVAL,
        ),
        (
            InterruptKind::Input,
            "input",
            "input-r:0:1",
            pause_reason::NEEDS_INPUT,
        ),
        (
            InterruptKind::External,
            "external",
            "ext-r:0:1",
            pause_reason::WAITING,
        ),
        (
            InterruptKind::Timer,
            "timer",
            "timer-r:0:1",
            pause_reason::WAITING,
        ),
        (
            InterruptKind::UnknownOutcome,
            "unknown_outcome",
            "unk-r:0:1",
            pause_reason::NEEDS_DECISION,
        ),
    ];
    for (kind, name, id, reason) in cases {
        assert_eq!(kind.as_str(), name);
        assert_eq!(Interrupt::for_effect(kind, &k, json!({})).id, id);
        assert_eq!(pause_reason_for(kind), reason);
    }
}

#[tokio::test]
async fn interrupt_state_reads_resolution() {
    let s = ProjectStore::open(&dir("state")).unwrap();
    let run = RunId::new();
    s.create_run(NewRun {
        id: run,
        project_id: "p".into(),
        parent_id: None,
        role: RunRole::Top,
        model_profile: None,
    })
    .await
    .unwrap();
    let i = Interrupt::for_effect(
        InterruptKind::Input,
        &EffectKey(format!("{run}:1:0")),
        json!({"question": "сколько?"}),
    );
    assert_eq!(interrupt::state(&s, &i.id).await.unwrap(), None);
    interrupt::open(&s, run, &i).await.unwrap();
    assert_eq!(
        interrupt::state(&s, &i.id).await.unwrap(),
        Some(InterruptState::Open)
    );
    let row = s.interrupt(&i.id).await.unwrap().unwrap();
    assert_eq!(row.kind, "input");
    assert_eq!(row.payload, Some(json!({"question": "сколько?"})));
    assert!(
        s.resolve_interrupt(&i.id, json!({"answer": "42"}))
            .await
            .unwrap()
    );
    assert_eq!(
        interrupt::state(&s, &i.id).await.unwrap(),
        Some(InterruptState::Resolved(json!({"answer": "42"})))
    );
}
