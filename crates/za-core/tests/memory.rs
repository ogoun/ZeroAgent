//! Порт памяти в `za-core` (R1-M задача 3, D138, D140, D168, D169): уровни, итоги чтения
//! и их значение в квитанции эффекта, признак режима идей, умолчания методов порта и
//! режим L0 [`NoMemory`].

use serde_json::json;
use za_core::memory::{
    CommandNote, IdeasSignal, MemoryLevel, MemoryPort, NoMemory, NoteOutcome, Recall, RecallReply,
    SessionNote, memory_budget,
};
use za_protocol::RunId;

#[test]
fn recall_value_roundtrip_and_legacy() {
    assert_eq!(
        Recall::from_value(&json!({"text": "x", "state": "timeout"})),
        Recall::Timeout
    );
    assert_eq!(
        Recall::from_value(&json!({"text": "x", "state": "partial"})),
        Recall::Partial("x".into())
    );
    // Квитанция R1-B: `{text}` без `state`.
    assert_eq!(
        Recall::from_value(&json!({"text": "abc"})),
        Recall::Hits("abc".into())
    );
    assert_eq!(
        Recall::from_value(&json!({"text": null})),
        Recall::Unavailable
    );
}

#[test]
fn recall_value_roundtrip_all_states() {
    let all = [
        Recall::Hits("### Задачи\n[in_progress] A — k:1".into()),
        Recall::Partial("### Задачи".into()),
        Recall::Empty,
        Recall::Unavailable,
        Recall::Timeout,
        Recall::Capped,
    ];
    let states: Vec<&str> = all.iter().map(Recall::state).collect();
    assert_eq!(
        states,
        [
            "hits",
            "partial",
            "empty",
            "unavailable",
            "timeout",
            "capped"
        ]
    );
    for r in &all {
        let v = r.to_value();
        assert_eq!(v["state"], r.state(), "{v}");
        assert_eq!(Recall::from_value(&v), *r, "{v}");
    }
    // Текста без текста нет: у итогов без текста `text` — null.
    assert_eq!(
        Recall::Timeout.to_value(),
        json!({"text": null, "state": "timeout"})
    );
    assert_eq!(
        Recall::Hits("t".into()).to_value(),
        json!({"text": "t", "state": "hits"})
    );
    // Незнакомое состояние — без текста (консервативно), не догадка.
    assert_eq!(
        Recall::from_value(&json!({"text": "x", "state": "zz"})),
        Recall::Unavailable
    );
    // Не объект — недоступна.
    assert_eq!(Recall::from_value(&json!(null)), Recall::Unavailable);
}

#[test]
fn recall_text_and_retry() {
    assert_eq!(Recall::Hits("fact".into()).text(), Some("fact"));
    assert_eq!(Recall::Partial("part".into()).text(), Some("part"));
    assert_eq!(Recall::Hits(String::new()).text(), None);
    assert_eq!(Recall::Partial("  ".into()).text(), None);
    for r in [
        Recall::Empty,
        Recall::Unavailable,
        Recall::Timeout,
        Recall::Capped,
    ] {
        assert_eq!(r.text(), None, "{r:?}");
    }
    // Отложенный повтор (D140, D168): только «не успели» и частичный ответ.
    assert!(Recall::Timeout.needs_retry());
    assert!(Recall::Partial("p".into()).needs_retry());
    for r in [
        Recall::Hits("h".into()),
        Recall::Empty,
        Recall::Unavailable,
        Recall::Capped,
    ] {
        assert!(!r.needs_retry(), "{r:?}");
    }
}

/// D169: признак режима идей — необязательное поле `ideas` значения эффекта; нет поля —
/// признака нет, незнакомое значение — тоже нет.
#[test]
fn reply_value_carries_ideas_signal() {
    let r = RecallReply {
        recall: Recall::Hits("x".into()),
        ideas: Some(IdeasSignal::Review),
    };
    let v = r.to_value();
    assert_eq!(v, json!({"text": "x", "state": "hits", "ideas": "review"}));
    assert_eq!(RecallReply::from_value(&v), r);
    let p = RecallReply {
        recall: Recall::Empty,
        ideas: Some(IdeasSignal::Propose),
    };
    assert_eq!(p.to_value()["ideas"], "propose");
    assert_eq!(RecallReply::from_value(&p.to_value()), p);
    // Без признака поля нет: квитанции прежней формы не меняются.
    let plain = RecallReply::from(Recall::Timeout);
    assert_eq!(plain.ideas, None);
    assert_eq!(plain.to_value(), json!({"text": null, "state": "timeout"}));
    assert_eq!(RecallReply::from_value(&json!({"text": "abc"})).ideas, None);
    assert_eq!(
        RecallReply::from_value(&json!({"text": "abc", "state": "hits", "ideas": "zz"})).ideas,
        None
    );
    assert_eq!(IdeasSignal::Review.as_str(), "review");
    assert_eq!(
        serde_json::to_value(IdeasSignal::Propose).unwrap(),
        "propose"
    );
}

#[test]
fn memory_levels_without_l3() {
    assert!(MemoryLevel::L0 < MemoryLevel::L1 && MemoryLevel::L1 < MemoryLevel::L2);
    assert_eq!(serde_json::to_value(MemoryLevel::L2).unwrap(), "L2");
    assert!(serde_json::from_value::<MemoryLevel>(json!("L3")).is_err());
}

#[test]
fn memory_budget_by_window() {
    assert_eq!(memory_budget(8192), 200);
    assert_eq!(memory_budget(16_384), 400);
    assert_eq!(memory_budget(32_768), 800);
    assert_eq!(memory_budget(131_072), 1500);
}

/// D138: новые методы порта — с умолчаниями «недоступна» или «ничего»; `NoMemory` их не
/// переопределяет и признака идей не даёт.
#[tokio::test]
async fn no_memory_defaults() {
    let run = RunId::new();
    let m = NoMemory;
    assert_eq!(m.level(), MemoryLevel::L0);
    assert_eq!(m.snapshot(run, 200, None).await, Recall::Unavailable.into());
    assert_eq!(m.snapshot(run, 200, None).await.ideas, None);
    assert_eq!(
        m.prompt_hint(run, "task", 100, None).await,
        Recall::Unavailable.into()
    );
    assert_eq!(
        m.tool_error_hint(run, "read", "no such file", 100).await,
        Recall::Unavailable
    );
    assert_eq!(
        m.retry_digest(run, 200, None).await,
        Recall::Unavailable.into()
    );
    assert_eq!(
        m.after_compaction(run, 200, None).await,
        Recall::Unavailable
    );
    m.before_compaction(run, "next").await;
    let note = SessionNote {
        trigger: "stop",
        status: "paused",
        summary: None,
        prompts: vec!["p".into()],
        files: Vec::new(),
        commands: vec![CommandNote {
            cmd: "cargo test".into(),
            failed: true,
        }],
        claimed: Vec::new(),
        next_step: None,
        taken_at: None,
    };
    assert_eq!(m.session_note(run, note).await, NoteOutcome::default());
    assert!(!NoteOutcome::default().sent);
    m.files_changed(run, vec!["src/a.rs".into()]).await;
    m.end_session(run, true).await;
    // Порт вызывается из `tokio::spawn`: future — `Send + 'static` при `Arc` порта.
    let shared: std::sync::Arc<dyn MemoryPort> = std::sync::Arc::new(NoMemory);
    let got = tokio::spawn(async move { shared.snapshot(run, 200, None).await.recall })
        .await
        .unwrap();
    assert_eq!(got, Recall::Unavailable);
}

#[test]
fn command_note_serde() {
    let c = CommandNote {
        cmd: "cargo test".into(),
        failed: false,
    };
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v, json!({"cmd": "cargo test", "failed": false}));
    assert_eq!(serde_json::from_value::<CommandNote>(v).unwrap(), c);
}
