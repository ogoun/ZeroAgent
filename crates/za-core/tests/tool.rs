//! Инструменты, гейт режима (D14), реестр по роли и тиру, шина событий журнала (D34, D35).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use za_core::{
    AgentEvent, Approval, BoxFuture, DuplicateTool, EventBus, Gate, Interrupt, InterruptKind,
    Journal, Postcondition, RiskClass, Subjects, Tool, ToolCx, ToolKind, ToolOutput, ToolRegistry,
    ToolSpec, gate, pause_reason_for,
};
use za_llm::ToolSchema;
use za_protocol::{
    Actor, EffectClass, EffectKey, EventKind, NewEvent, RunId, RunMode, RunRole, RunStatus, Seq,
    Tier, approval_reason, audit_action, checkpoint_kind, deny_reason, irreversible, pause_reason,
    taint_origin,
};
use za_store::{NewRun, ProjectStore, UsageRow};

#[path = "../../../test-support/unique_dir.rs"]
mod unique_dir;
use unique_dir::unique_dir;

/// Чистый каталог теста внутри `CARGO_TARGET_TMPDIR`.
fn dir(name: &str) -> PathBuf {
    unique_dir("za-core-tool", name)
}

fn spec_full(
    name: &str,
    kind: ToolKind,
    approval: Approval,
    roles: Vec<RunRole>,
    min_tier: Tier,
) -> ToolSpec {
    ToolSpec {
        schema: ToolSchema {
            name: name.into(),
            description: format!("инструмент {name}"),
            parameters: json!({"type": "object"}),
        },
        class: EffectClass::ReadOnly,
        kind,
        approval,
        risk: RiskClass::Read,
        roles,
        min_tier,
        timeout: Duration::from_secs(5),
        output_limit: None,
        tainted: false,
    }
}

fn spec(kind: ToolKind, approval: Approval) -> ToolSpec {
    spec_full("t", kind, approval, vec![RunRole::Top], Tier::Small)
}

/// Инструмент, возвращающий имя и аргументы.
struct Echo(ToolSpec);

impl Tool for Echo {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }

    fn call(&self, args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let name = self.0.schema.name.clone();
        Box::pin(async move { ToolOutput::ok(format!("{name}:{args}")) })
    }
}

fn echo(name: &str, roles: Vec<RunRole>, min_tier: Tier) -> Arc<dyn Tool> {
    Arc::new(Echo(spec_full(
        name,
        ToolKind::Read,
        Approval::Mode,
        roles,
        min_tier,
    )))
}

fn names(v: &[Arc<dyn Tool>]) -> Vec<String> {
    v.iter().map(|t| t.spec().schema.name.clone()).collect()
}

#[test]
fn gate_table() {
    use RunMode::*;
    use ToolKind::*;
    let s = |k| spec(k, Approval::Mode);
    assert_eq!(gate(ReadOnly, &s(Read)), Gate::Allow);
    assert_eq!(gate(ReadOnly, &s(Edit)), Gate::Deny("read-only mode"));
    assert_eq!(gate(Plan, &s(Exec)), Gate::Deny("plan mode"));
    assert_eq!(gate(Ask, &s(Edit)), Gate::Ask);
    assert_eq!(gate(AcceptEdits, &s(Edit)), Gate::Allow);
    assert_eq!(gate(AcceptEdits, &s(Exec)), Gate::Ask);
    assert_eq!(gate(Auto, &s(Exec)), Gate::Allow);
    assert_eq!(gate(Auto, &spec(Exec, Approval::Always)), Gate::Ask);
    assert_eq!(gate(Ask, &spec(Edit, Approval::Never)), Gate::Allow);
    assert_eq!(
        gate(ReadOnly, &spec(Edit, Approval::Never)),
        Gate::Deny("read-only mode")
    );
    assert_eq!(gate(ReadOnly, &s(Control)), Gate::Allow);
}

#[test]
fn registry_filters_role_and_tier() {
    let mut r = ToolRegistry::default();
    r.register(echo(
        "b",
        vec![RunRole::Top, RunRole::Worker],
        Tier::Standard,
    ))
    .unwrap();
    r.register(echo("c", vec![RunRole::Worker], Tier::Small))
        .unwrap();
    r.register(echo("a", vec![RunRole::Top], Tier::Small))
        .unwrap();

    assert_eq!(names(&r.for_run(RunRole::Top, Tier::Small)), ["a"]);
    assert_eq!(names(&r.for_run(RunRole::Top, Tier::Full)), ["a", "b"]);
    assert_eq!(
        names(&r.for_run(RunRole::Worker, Tier::Standard)),
        ["b", "c"]
    );
    assert!(r.get("a").is_some());
    assert!(r.get("zzz").is_none());

    let err = r
        .register(echo("a", vec![RunRole::Worker], Tier::Full))
        .unwrap_err();
    assert_eq!(err.0, "a");
    assert_eq!(err.to_string(), DuplicateTool("a".into()).to_string());
    // Первая регистрация осталась.
    assert_eq!(r.get("a").unwrap().spec().roles, [RunRole::Top]);
}

#[test]
fn tool_output_round_trips() {
    let o = ToolOutput::error("x");
    let v = serde_json::to_value(&o).unwrap();
    assert_eq!(v, json!({"content": "x", "is_error": true}));
    assert_eq!(serde_json::from_value::<ToolOutput>(v).unwrap(), o);
}

#[test]
fn bypass_allows_edit_and_exec() {
    assert_eq!(
        gate(RunMode::Bypass, &spec(ToolKind::Exec, Approval::Mode)),
        Gate::Allow
    );
    assert_eq!(
        gate(RunMode::Bypass, &spec(ToolKind::Edit, Approval::Mode)),
        Gate::Allow
    );
    // Затем поправка инструмента, как прежде.
    assert_eq!(
        gate(RunMode::Bypass, &spec(ToolKind::Exec, Approval::Always)),
        Gate::Ask
    );
}

/// Квитанция R1-B (`{"content", "is_error"}`) читается; новый результат без признаков
/// пишется так же (D75).
#[test]
fn tool_output_reads_r1b_receipt() {
    let o: ToolOutput = serde_json::from_str(r#"{"content":"x","is_error":false}"#).unwrap();
    assert_eq!(o, ToolOutput::ok("x"));
    assert_eq!(
        serde_json::to_string(&ToolOutput::ok("x")).unwrap(),
        r#"{"content":"x","is_error":false}"#
    );
    let full = ToolOutput {
        taint: Some(taint_origin::NETWORK.into()),
        digest: Some("d".into()),
        post: vec![
            Postcondition::FileHash {
                path: "a.txt".into(),
                blake3: "ab".into(),
            },
            Postcondition::FileAbsent { path: "b".into() },
        ],
        ..ToolOutput::ok("x")
    };
    let v = serde_json::to_value(&full).unwrap();
    assert_eq!(
        v,
        json!({"content": "x", "is_error": false, "taint": "network", "digest": "d",
            "post": [{"kind": "file_hash", "path": "a.txt", "blake3": "ab"},
                     {"kind": "file_absent", "path": "b"}]})
    );
    assert_eq!(serde_json::from_value::<ToolOutput>(v).unwrap(), full);
}

/// Умолчания трейта: предметов и текстов нет; классы риска упорядочены и в snake_case.
#[test]
fn tool_defaults_and_risk() {
    let t = Echo(spec(ToolKind::Read, Approval::Mode));
    assert_eq!(t.subjects(&json!({})), Subjects::default());
    assert!(t.texts().is_empty());
    assert!(
        RiskClass::Read < RiskClass::LocalWrite && RiskClass::Irreversible < RiskClass::Publish
    );
    assert_eq!(
        serde_json::to_value(RiskClass::LocalWrite).unwrap(),
        json!("local_write")
    );
}

#[test]
fn protocol_strings_stable() {
    assert_eq!(EventKind::BoundaryOutside.as_str(), "boundary.outside");
    assert_eq!(EventKind::GrantAdded.as_str(), "grant.added");
    assert_eq!(EventKind::CheckpointCreated.as_str(), "checkpoint.created");
    assert_eq!(EventKind::FileChanged.as_str(), "file.changed");
    assert_eq!(
        serde_json::to_value(RunMode::Bypass).unwrap(),
        json!("bypass")
    );
    assert_eq!(pause_reason_for(InterruptKind::Trust), "needs_trust");
    assert_eq!(pause_reason::NEEDS_TRUST, "needs_trust");
    assert_eq!(InterruptKind::Trust.as_str(), "trust");
    let key = EffectKey::derive(RunId::new(), 1, 2);
    let i = Interrupt::for_effect(InterruptKind::Trust, &key, json!({}));
    assert_eq!(i.id, format!("trust-{}", key.0));
    assert_eq!(
        [
            approval_reason::MODE,
            approval_reason::RULE,
            approval_reason::OUTSIDE_PROJECT,
            approval_reason::TAINTED,
            approval_reason::IRREVERSIBLE,
            approval_reason::GRANT,
        ],
        [
            "mode",
            "rule",
            "outside_project",
            "tainted",
            "irreversible",
            "grant"
        ]
    );
    assert_eq!(
        [
            deny_reason::PROTECTED_PATH,
            deny_reason::POLICY_RULE,
            deny_reason::UNTRUSTED_PROJECT
        ],
        ["protected path", "policy rule", "untrusted project"]
    );
    assert_eq!(
        [
            irreversible::DELETE,
            irreversible::GIT_RESET,
            irreversible::GIT_FORCE_PUSH,
            irreversible::PUBLISH,
            irreversible::INFRA,
            irreversible::SQL_DROP,
            irreversible::OVERWRITE,
            irreversible::UNPARSED,
            irreversible::EXEC_UNTRUSTED,
            irreversible::GIT_CONFIG_EXEC,
        ],
        [
            "delete",
            "git_reset",
            "git_force_push",
            "publish",
            "infra",
            "sql_drop",
            "overwrite",
            "unparsed",
            "exec_untrusted",
            "git_config_exec"
        ]
    );
    assert_eq!(
        [
            taint_origin::OUTSIDE_PROJECT,
            taint_origin::UNTRUSTED_PROJECT,
            taint_origin::NETWORK,
            taint_origin::TOOL,
        ],
        ["outside_project", "untrusted_project", "network", "tool"]
    );
    assert_eq!(
        [
            checkpoint_kind::GIT,
            checkpoint_kind::FILES,
            checkpoint_kind::SKIPPED
        ],
        ["git", "files", "skipped"]
    );
    assert_eq!(
        [
            audit_action::APPROVAL_RESOLVED,
            audit_action::GRANT_ADDED,
            audit_action::TRUST_GRANTED,
            audit_action::TRUST_REVOKED,
            audit_action::POLICY_LOADED,
            audit_action::CHECKPOINT_RESTORED,
        ],
        [
            "approval.resolved",
            "grant.added",
            "trust.granted",
            "trust.revoked",
            "policy.loaded",
            "checkpoint.restored",
        ]
    );
}

async fn store_with_run(name: &str) -> (ProjectStore, RunId) {
    let s = ProjectStore::open(&dir(name)).unwrap();
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
    (s, run)
}

#[tokio::test]
async fn journal_publishes_after_commit() {
    let (s, run) = store_with_run("publish").await;
    let bus = EventBus::new(EventBus::DEFAULT_CAPACITY);
    let j = Journal::new(s.clone(), bus.clone());
    let mut rx = bus.subscribe();

    let e = NewEvent {
        run_id: run,
        parent_seq: None,
        actor: Actor::Runtime,
        kind: EventKind::TurnStarted,
        payload: json!({"turn": 1}),
        tainted: false,
    };
    let seq = j.append(e.clone()).await.unwrap();
    let got: AgentEvent = rx.recv().await.unwrap();
    assert_eq!(got.seq, Some(seq));
    assert_eq!(got.event, e);
    // Опубликованное событие уже в журнале.
    let stored = s.read_run(run, Seq(0), 100).await.unwrap();
    assert!(
        stored
            .iter()
            .any(|x| x.seq == seq && x.kind == EventKind::TurnStarted)
    );

    let generation = j
        .set_status(run, RunStatus::Running, 0, None)
        .await
        .unwrap();
    assert_eq!(generation, 1);
    let got = rx.recv().await.unwrap();
    assert_eq!(got.seq, None);
    assert_eq!(got.event.kind, EventKind::RunStatus);
    assert_eq!(got.event.run_id, run);
    assert_eq!(got.event.actor, Actor::Runtime);
    assert_eq!(
        got.event.payload,
        json!({"from": "queued", "to": "running", "generation": 1})
    );
    // Копия на шине совпадает с событием журнала.
    let last = s.read_run(run, seq, 100).await.unwrap();
    assert_eq!(last.len(), 1);
    assert_eq!(last[0].kind, EventKind::RunStatus);
    assert_eq!(last[0].actor, got.event.actor);
    assert_eq!(last[0].payload, got.event.payload);

    let row = UsageRow {
        model: "m".into(),
        input: 10,
        cached: 0,
        output: 2,
        reasoning: 0,
        gpu_ms: None,
        cost_usd: None,
    };
    let useq = j
        .record_usage(run, row, json!({"effect_key": "k"}))
        .await
        .unwrap();
    let got = rx.recv().await.unwrap();
    assert_eq!(got.seq, Some(useq));
    assert_eq!(got.event.kind, EventKind::ModelUsage);
    assert_eq!(got.event.actor, Actor::Model);
    assert_eq!(got.event.payload, json!({"effect_key": "k"}));
    assert_eq!(j.store().usage_totals(run).await.unwrap().input, 10);
}

#[tokio::test]
async fn journal_without_subscribers_and_failed_write_publish_nothing() {
    let (s, run) = store_with_run("nosub").await;
    let bus = EventBus::new(4);
    let j = Journal::new(s, bus.clone());
    // Без подписчиков отправка — не ошибка.
    j.set_status(run, RunStatus::Running, 0, None)
        .await
        .unwrap();

    let mut rx = bus.subscribe();
    // Устаревшее поколение: записи нет, публикации нет.
    assert!(
        j.set_status(run, RunStatus::Paused, 0, Some("command"))
            .await
            .is_err()
    );
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    j.set_status(run, RunStatus::Pausing, 1, Some("command"))
        .await
        .unwrap();
    let got = rx.recv().await.unwrap();
    assert_eq!(
        got.event.payload,
        json!({"from": "running", "to": "pausing", "generation": 2, "reason": "command"})
    );
}

#[tokio::test]
async fn zero_capacity_bus_holds_one_event() {
    let (s, run) = store_with_run("cap0").await;
    let bus = EventBus::new(0);
    let j = Journal::new(s, bus.clone());
    let mut rx = bus.subscribe();
    let seq = j.append(turn(run, 1)).await.unwrap();
    let got = rx.recv().await.unwrap();
    assert_eq!(got.seq, Some(seq));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

fn turn(run: RunId, n: u32) -> NewEvent {
    NewEvent {
        run_id: run,
        parent_seq: None,
        actor: Actor::Runtime,
        kind: EventKind::TurnStarted,
        payload: json!({"turn": n}),
        tainted: false,
    }
}

/// Подписчик по правилу отсева из doc `bus.rs`: события с `seq` — по курсору,
/// `run.status` — по `generation`.
struct Follower {
    cursor: Seq,
    status_generation: u64,
    /// (seq, kind, payload) принятых событий по порядку.
    seen: Vec<(Option<Seq>, EventKind, Value)>,
}

impl Follower {
    fn new() -> Self {
        Self {
            cursor: Seq(0),
            status_generation: 0,
            seen: Vec::new(),
        }
    }

    fn accept(&mut self, seq: Option<Seq>, kind: EventKind, payload: Value) {
        if kind == EventKind::RunStatus {
            let g = payload["generation"].as_u64().unwrap();
            if g <= self.status_generation {
                return;
            }
            self.status_generation = g;
        } else if let Some(sq) = seq
            && sq <= self.cursor
        {
            return;
        }
        if let Some(sq) = seq {
            self.cursor = self.cursor.max(sq);
        }
        self.seen.push((seq, kind, payload));
    }
}

#[tokio::test]
async fn lagged_subscriber_catches_up_without_duplicates() {
    let (s, run) = store_with_run("lagged").await;
    let bus = EventBus::new(4);
    let j = Journal::new(s.clone(), bus.clone());
    let mut rx = bus.subscribe();

    // 8 событий в шину ёмкостью 4: подписчик отстаёт.
    let mut generation = j
        .set_status(run, RunStatus::Running, 0, None)
        .await
        .unwrap();
    for n in 1..=3 {
        j.append(turn(run, n)).await.unwrap();
    }
    generation = j
        .set_status(run, RunStatus::Pausing, generation, Some("command"))
        .await
        .unwrap();
    generation = j
        .set_status(run, RunStatus::Running, generation, None)
        .await
        .unwrap();
    j.append(turn(run, 4)).await.unwrap();
    j.set_status(run, RunStatus::Done, generation, None)
        .await
        .unwrap();

    let mut f = Follower::new();
    assert!(matches!(rx.recv().await, Err(RecvError::Lagged(4))));
    // Догоняем из журнала: всё после run.created.
    for e in s.read_run(run, Seq(0), 100).await.unwrap() {
        if e.kind != EventKind::RunCreated {
            f.accept(Some(e.seq), e.kind, e.payload);
        }
    }
    let from_journal = f.seen.len();
    assert_eq!(from_journal, 8);
    // Остаток шины — уже прочитанные события: всё отсеивается.
    loop {
        match rx.try_recv() {
            Ok(ev) => f.accept(ev.seq, ev.event.kind, ev.event.payload),
            Err(TryRecvError::Empty) => break,
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!(f.seen.len(), from_journal);
    let statuses: Vec<u64> = f
        .seen
        .iter()
        .filter(|x| x.1 == EventKind::RunStatus)
        .map(|x| x.2["generation"].as_u64().unwrap())
        .collect();
    assert_eq!(statuses, [1, 2, 3, 4]);

    // Новое событие после догона принимается ровно один раз.
    let seq = j.append(turn(run, 5)).await.unwrap();
    let ev = rx.recv().await.unwrap();
    f.accept(ev.seq, ev.event.kind, ev.event.payload);
    assert_eq!(f.seen.len(), from_journal + 1);
    assert_eq!(f.seen.last().unwrap().0, Some(seq));
}
