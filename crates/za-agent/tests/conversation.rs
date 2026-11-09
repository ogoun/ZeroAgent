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

// ---------- Исправления, раунд 1: повторы id, пустые сообщения, части по ролям ----------

/// Сохраняет сообщения блобами как есть (в обход API) и возвращает хэши.
async fn raw_refs(store: &ProjectStore, msgs: Vec<Message>) -> Vec<BlobHash> {
    let mut refs = Vec::new();
    for m in msgs {
        let v = serde_json::to_value(&m).unwrap();
        let b = za_protocol::canonical_json(&v);
        refs.push(store.put_blob(b.as_bytes()).await.unwrap());
    }
    refs
}

async fn load_err(name: &str, msgs: Vec<Message>) -> HistoryError {
    let store = ProjectStore::open(&dir(name)).unwrap();
    let refs = raw_refs(&store, msgs).await;
    match Conversation::load(&store, &refs).await {
        Err(LoadError::History(e)) => e,
        other => panic!("ожидалась ошибка истории, получено {other:?}"),
    }
}

#[test]
fn duplicate_call_ids_are_rejected() {
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    let before = c.clone();
    assert_eq!(
        c.commit_turn(vec![text(Role::System, "x")], calls(&["c1", "c1"])),
        Err(HistoryError::DuplicateCallId("c1".into()))
    );
    assert_eq!(c, before);
    assert!(c.pending_calls().is_empty());
}

#[test]
fn empty_user_message_is_rejected() {
    let mut c = Conversation::default();
    assert_eq!(
        c.push_user(String::new()),
        Err(HistoryError::EmptyMessage(Role::User))
    );
    assert!(c.messages().is_empty());
}

#[test]
fn empty_answer_drops_service_of_its_turn() {
    for empty in [
        Message {
            role: Role::Assistant,
            parts: vec![],
        },
        text(Role::Assistant, ""),
        Message {
            role: Role::Assistant,
            parts: vec![Part::Reasoning {
                text: "thinking".into(),
                replay: None,
            }],
        },
    ] {
        let mut c = Conversation::default();
        c.push_user("q".into()).unwrap();
        c.commit_turn(vec![text(Role::System, "x")], empty.clone())
            .unwrap();
        assert_eq!(roles(&c), vec![Role::User, Role::Assistant]);
        assert_eq!(c.messages()[1], empty);
        c.push_user("q2".into()).unwrap();
        assert_eq!(c.validate(), Ok(()));
    }
}

#[test]
fn empty_answer_without_service_is_kept() {
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    c.commit_turn(vec![], text(Role::Assistant, "")).unwrap();
    assert_eq!(roles(&c), vec![Role::User, Role::Assistant]);
    assert_eq!(c.validate(), Ok(()));
}

#[test]
fn parts_must_fit_role() {
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    let before = c.clone();
    assert_eq!(
        c.commit_turn(
            vec![Message {
                role: Role::System,
                parts: vec![call("c1")],
            }],
            text(Role::Assistant, "a")
        ),
        Err(HistoryError::PartNotAllowed {
            role: Role::System,
            part: "tool_call"
        })
    );
    assert_eq!(
        c.commit_turn(
            vec![],
            Message {
                role: Role::Assistant,
                parts: vec![result("c1")],
            }
        ),
        Err(HistoryError::PartNotAllowed {
            role: Role::Assistant,
            part: "tool_result"
        })
    );
    assert_eq!(c, before);
}

#[tokio::test]
async fn load_rejects_forms_the_api_never_builds() {
    let q = || text(Role::User, "q");
    // Повторные id: close_interrupted не создаст такую историю, load её не примет.
    assert_eq!(
        load_err("dup_ids", vec![q(), calls(&["c1", "c1"])]).await,
        HistoryError::DuplicateCallId("c1".into())
    );
    // System перед пустым ответом: на проводе system оказался бы перед user.
    assert_eq!(
        load_err(
            "sys_empty",
            vec![q(), text(Role::System, "x"), text(Role::Assistant, ""), q()]
        )
        .await,
        HistoryError::SystemNotBeforeAssistant
    );
    assert_eq!(
        load_err("empty_user", vec![text(Role::User, "")]).await,
        HistoryError::EmptyMessage(Role::User)
    );
    assert_eq!(
        load_err(
            "empty_tool",
            vec![
                q(),
                calls(&["c1"]),
                Message {
                    role: Role::Tool,
                    parts: vec![],
                },
            ]
        )
        .await,
        HistoryError::EmptyMessage(Role::Tool)
    );
    assert_eq!(
        load_err(
            "user_call",
            vec![Message {
                role: Role::User,
                parts: vec![Part::Text("q".into()), call("c1")],
            }]
        )
        .await,
        HistoryError::PartNotAllowed {
            role: Role::User,
            part: "tool_call"
        }
    );
}

// ---------- Формы на проводе Anthropic (Carry 400 (а)) ----------

mod wire {
    use serde_json::Value;
    use za_llm::__test::{AnthropicWire, WireCtx};
    use za_llm::{BackendKind, ChatRequest, default_caps};

    use super::Conversation;

    /// `messages` тела Anthropic Messages для модели с mid-conversation system.
    pub fn messages(c: &Conversation) -> Vec<Value> {
        let caps = default_caps(BackendKind::Anthropic);
        let ctx = WireCtx {
            caps: &caps,
            slot: None,
            secret: None,
            thinking: None,
            max_output_tokens: None,
        };
        let w = AnthropicWire {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "claude-opus-5-5".into(),
        };
        let req = ChatRequest {
            system: "s".into(),
            messages: c.messages().to_vec(),
            ..ChatRequest::default()
        };
        let body = w.build_body(&req, &ctx).unwrap();
        body["messages"].as_array().unwrap().clone()
    }

    fn role(m: &Value) -> &str {
        m["role"].as_str().unwrap()
    }

    fn ids<'a>(m: &'a Value, kind: &str, field: &str) -> Vec<&'a Value> {
        m["content"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|b| b["type"] == kind)
                    .map(|b| &b[field])
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Формы, запрещённые документацией Anthropic (Messages, «Mid-conversation system
    /// messages», tool use): первое сообщение не `user`; пустое содержимое или пустой
    /// текстовый блок; две соседние реплики одной роли; system не сразу после `user`
    /// (цепочка system считается вместе) или перед `user`; `tool_result` без `tool_use`
    /// предыдущего ответа; `tool_use` без `tool_result` в следующем сообщении.
    pub fn assert_allowed(ms: &[Value]) {
        assert_eq!(role(&ms[0]), "user", "первое сообщение: {ms:?}");
        for (i, m) in ms.iter().enumerate() {
            if role(m) == "system" {
                let prev = ms[..i].iter().rev().find(|p| role(p) != "system");
                assert_eq!(prev.map(role), Some("user"), "system не после user: {ms:?}");
                if let Some(next) = ms[i + 1..].iter().find(|n| role(n) != "system") {
                    assert_eq!(role(next), "assistant", "system перед user: {ms:?}");
                }
                continue;
            }
            let content = m["content"].as_array().unwrap();
            assert!(!content.is_empty(), "пустое сообщение: {ms:?}");
            for b in content {
                if b["type"] == "text" {
                    assert_ne!(b["text"], "", "пустой текст: {ms:?}");
                }
            }
        }
        let turns: Vec<&Value> = ms.iter().filter(|m| role(m) != "system").collect();
        assert!(
            ids(turns[0], "tool_result", "tool_use_id").is_empty(),
            "результат в начале: {ms:?}"
        );
        for w in turns.windows(2) {
            assert_ne!(
                role(w[0]),
                role(w[1]),
                "соседние реплики одной роли: {ms:?}"
            );
            let uses = ids(w[0], "tool_use", "id");
            let results = ids(w[1], "tool_result", "tool_use_id");
            for id in &uses {
                assert!(results.contains(id), "tool_use {id} без результата: {ms:?}");
            }
            for id in &results {
                assert!(uses.contains(id), "tool_result {id} без tool_use: {ms:?}");
            }
        }
    }
}

#[test]
fn empty_answer_with_service_renders_allowed_anthropic_body() {
    for parts in [vec![], vec![Part::Text(String::new())]] {
        let mut c = Conversation::default();
        c.push_user("q".into()).unwrap();
        c.commit_turn(
            vec![text(Role::System, "Answer with a tool call.")],
            Message {
                role: Role::Assistant,
                parts,
            },
        )
        .unwrap();
        c.push_user("q2".into()).unwrap();
        let ms = wire::messages(&c);
        wire::assert_allowed(&ms);
        assert!(ms.iter().all(|m| m["role"] != "system"));
    }
}

#[test]
fn full_history_renders_allowed_anthropic_body() {
    let mut c = Conversation::default();
    c.push_user("q".into()).unwrap();
    c.commit_turn(vec![text(Role::System, "x")], calls(&["c1", "c2"]))
        .unwrap();
    c.push_results(vec![result("c1")]).unwrap();
    assert_eq!(c.close_interrupted(), 1);
    c.commit_turn(
        vec![text(Role::System, "y"), text(Role::System, "z")],
        text(Role::Assistant, "done"),
    )
    .unwrap();
    c.push_user("again".into()).unwrap();
    // Пустой ответ на эмуляцию: system этого хода не фиксируется.
    c.commit_turn(vec![text(Role::System, "w")], text(Role::Assistant, ""))
        .unwrap();
    c.push_user("more".into()).unwrap();
    c.commit_turn(vec![], text(Role::Assistant, "ok")).unwrap();
    assert_eq!(c.validate(), Ok(()));
    let ms = wire::messages(&c);
    wire::assert_allowed(&ms);
    let systems: Vec<&serde_json::Value> = ms
        .iter()
        .filter(|m| m["role"] == "system")
        .map(|m| &m["content"])
        .collect();
    assert_eq!(systems, vec!["x", "y", "z"]);
}
