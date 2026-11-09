//! История диалога: атомарная фиксация хода (D10), пары вызов-результат, CORE-105,
//! хранение сообщений блобами (D4).

use std::path::PathBuf;

use serde_json::json;
use za_agent::{Conversation, HistoryError, LoadError};
use za_llm::{Message, Part, Role, ToolCall};
use za_protocol::BlobHash;
use za_store::ProjectStore;

/// Чистый каталог теста внутри `CARGO_TARGET_TMPDIR`.
fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("conversation")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn text(role: Role, t: &str) -> Message {
    Message {
        role,
        parts: vec![Part::Text(t.into())],
    }
}

fn call(id: &str) -> Part {
    Part::ToolCall(ToolCall {
        id: id.into(),
        name: "read".into(),
        arguments: json!({"path": "a.txt"}),
        raw_arguments: None,
    })
}

fn calls(ids: &[&str]) -> Message {
    Message {
        role: Role::Assistant,
        parts: ids.iter().map(|id| call(id)).collect(),
    }
}

fn result(id: &str) -> Part {
    Part::ToolResult {
        call_id: id.into(),
        content: format!("ok {id}"),
        is_error: false,
    }
}

fn roles(c: &Conversation) -> Vec<Role> {
    c.messages().iter().map(|m| m.role).collect()
}

#[test]
fn turn_commits_service_before_assistant() {
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    c.commit_turn(vec![text(Role::System, "x")], text(Role::Assistant, "a"))
        .unwrap();
    assert_eq!(roles(&c), vec![Role::User, Role::System, Role::Assistant]);
    assert_eq!(c.validate(), Ok(()));
}

#[test]
fn failed_turn_leaves_no_system() {
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    // Ход сорвался: commit_turn не вызван, служебные сообщения остались у хода.
    c.push_user("q2".into()).unwrap();
    assert_eq!(roles(&c), vec![Role::User, Role::User]);
    assert!(c.messages().iter().all(|m| m.role != Role::System));
}

#[test]
fn system_must_precede_assistant() {
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    let before = c.clone();
    assert_eq!(
        c.commit_turn(vec![text(Role::User, "u")], text(Role::Assistant, "a")),
        Err(HistoryError::SystemNotBeforeAssistant)
    );
    assert_eq!(c, before);
}

#[test]
fn results_pair_with_calls() {
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    c.commit_turn(vec![], calls(&["c1", "c2"])).unwrap();
    assert_eq!(
        c.push_results(vec![result("c3")]),
        Err(HistoryError::UnknownCallId("c3".into()))
    );
    c.push_results(vec![result("c1")]).unwrap();
    assert_eq!(c.pending_calls(), vec!["c2".to_string()]);
    assert_eq!(
        c.push_user("x".into()),
        Err(HistoryError::PendingCalls(vec!["c2".into()]))
    );
    assert_eq!(
        c.push_results(vec![result("c1")]),
        Err(HistoryError::DuplicateResult("c1".into()))
    );
}

#[test]
fn close_interrupted_fills_pending() {
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    c.commit_turn(vec![], calls(&["c1", "c2"])).unwrap();
    c.push_results(vec![result("c1")]).unwrap();
    assert_eq!(c.close_interrupted(), 1);
    let last = c.messages().last().unwrap();
    assert_eq!(last.role, Role::Tool);
    assert_eq!(
        last.parts.last().unwrap(),
        &Part::ToolResult {
            call_id: "c2".into(),
            content: "interrupted: call not executed".into(),
            is_error: true,
        }
    );
    assert_eq!(c.validate(), Ok(()));
}

#[tokio::test]
async fn persist_and_load_round_trip() {
    let store = ProjectStore::open(&dir("round_trip")).unwrap();
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    c.commit_turn(vec![text(Role::System, "x")], calls(&["c1"]))
        .unwrap();
    c.push_results(vec![result("c1")]).unwrap();
    assert_eq!(c.messages().len(), 4);

    let p1 = c.persist(&store).await.unwrap();
    assert_eq!(p1.written, 4);
    assert_eq!(p1.refs.len(), 4);

    c.push_user("e".into()).unwrap();
    let p2 = c.persist(&store).await.unwrap();
    assert_eq!(p2.written, 1);
    assert_eq!(p2.refs[..4], p1.refs[..]);
    assert_eq!(p2.refs.len(), 5);

    let p3 = c.persist(&store).await.unwrap();
    assert_eq!(p3.written, 0);
    assert_eq!(p3.refs, p2.refs);

    let loaded = Conversation::load(&store, &p2.refs).await.unwrap();
    assert_eq!(loaded.messages().len(), 5);
    assert_eq!(loaded, c);
}

// Дополнительные инварианты сверх брифа.

#[test]
fn first_message_must_be_user() {
    let mut c = Conversation::default();
    assert_eq!(
        c.commit_turn(vec![], text(Role::Assistant, "a")),
        Err(HistoryError::FirstNotUser)
    );
    assert!(c.messages().is_empty());
}

#[test]
fn turn_over_pending_calls_is_rejected() {
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    c.commit_turn(vec![], calls(&["c1"])).unwrap();
    assert_eq!(
        c.commit_turn(vec![], text(Role::Assistant, "a")),
        Err(HistoryError::PendingCalls(vec!["c1".into()]))
    );
    // После результатов следующий ход со служебным сообщением допустим.
    c.push_results(vec![result("c1")]).unwrap();
    c.commit_turn(vec![text(Role::System, "x")], text(Role::Assistant, "a"))
        .unwrap();
    assert_eq!(c.validate(), Ok(()));
}

#[test]
fn system_only_after_user_or_results() {
    // assistant, System, assistant — для Anthropic 400: system только после user.
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    c.commit_turn(vec![], text(Role::Assistant, "a")).unwrap();
    let before = c.clone();
    assert_eq!(
        c.commit_turn(vec![text(Role::System, "x")], text(Role::Assistant, "b")),
        Err(HistoryError::SystemNotAfterUser)
    );
    assert_eq!(c, before);
}

#[test]
fn turn_must_end_with_assistant() {
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    assert_eq!(
        c.commit_turn(vec![], text(Role::User, "u")),
        Err(HistoryError::TurnNotAssistant)
    );
    assert_eq!(
        c.commit_turn(vec![], text(Role::Tool, "t")),
        Err(HistoryError::TurnNotAssistant)
    );
}

#[test]
fn results_only_tool_results() {
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    c.commit_turn(vec![], calls(&["c1"])).unwrap();
    assert_eq!(
        c.push_results(vec![Part::Text("t".into())]),
        Err(HistoryError::NotToolResult)
    );
    // Повтор внутри одного пакета.
    assert_eq!(
        c.push_results(vec![result("c1"), result("c1")]),
        Err(HistoryError::DuplicateResult("c1".into()))
    );
    assert_eq!(c.pending_calls(), vec!["c1".to_string()]);
}

#[test]
fn results_do_not_pair_across_user() {
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    c.commit_turn(vec![], calls(&["c1"])).unwrap();
    c.push_results(vec![result("c1")]).unwrap();
    c.push_user("next".into()).unwrap();
    assert!(c.pending_calls().is_empty());
    assert_eq!(
        c.push_results(vec![result("c1")]),
        Err(HistoryError::UnknownCallId("c1".into()))
    );
}

#[test]
fn close_interrupted_without_pending_is_noop() {
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    assert_eq!(c.close_interrupted(), 0);
    assert_eq!(c.messages().len(), 1);
}

#[tokio::test]
async fn load_reports_missing_blob() {
    let store = ProjectStore::open(&dir("missing")).unwrap();
    let h = BlobHash("00".repeat(32));
    match Conversation::load(&store, std::slice::from_ref(&h)).await {
        Err(LoadError::History(HistoryError::MissingBlob(s))) => assert_eq!(s, h.0),
        other => panic!("ожидался MissingBlob, получено {other:?}"),
    }
}

#[tokio::test]
async fn load_validates_history() {
    // Блобы с историей `user, System, user` (сорвавшийся ход, Carry 400 (а)) не загружаются.
    let store = ProjectStore::open(&dir("invalid")).unwrap();
    let mut refs = Vec::new();
    for m in [
        text(Role::User, "q"),
        text(Role::System, "x"),
        text(Role::User, "q2"),
    ] {
        let v = serde_json::to_value(&m).unwrap();
        let b = za_protocol::canonical_json(&v);
        refs.push(store.put_blob(b.as_bytes()).await.unwrap());
    }
    match Conversation::load(&store, &refs).await {
        Err(LoadError::History(HistoryError::SystemNotBeforeAssistant)) => {}
        other => panic!("ожидался SystemNotBeforeAssistant, получено {other:?}"),
    }
}

#[tokio::test]
async fn persist_keeps_blob_canonical() {
    let store = ProjectStore::open(&dir("canonical")).unwrap();
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    let p = c.persist(&store).await.unwrap();
    let raw = store.get_blob(&p.refs[0]).await.unwrap().unwrap();
    let want = za_protocol::canonical_json(&serde_json::to_value(&c.messages()[0]).unwrap());
    assert_eq!(raw, want.into_bytes());
}
