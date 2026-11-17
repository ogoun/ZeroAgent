//! Шлюз записи `WriteGateway` (R1-M D145, D163–D165; D169–D171 изменений по ZAM 0.0.12-m6.5):
//! `Idempotency-Key` из ключа эффекта, детерминированное тело, маска до отправки, сверка правок
//! без ключа, конфликты, разбор идеи мимо очереди, дедлайн и очередь — на поддельном ZAM
//! (`test-support/fake_zam.rs`, D154), только `127.0.0.1`.

#[path = "../../../test-support/fake_zam.rs"]
mod fake_zam;
#[path = "../../../test-support/unique_dir.rs"]
mod unique_dir;

use std::sync::Arc;

use fake_zam::{FakeZam, Health, Seen, WAIT, closed_port_url, within};
use serde_json::{Value, json};
use za_memory::discovery::from_health;
use za_memory::gate::NOTE_EVIDENCE;
use za_memory::gateway::is_idea_mode;
use za_memory::scope::READ_ONLY;
use za_memory::{
    Clock, DirectQueue, ErrorKind, GatewayParts, Link, MemoryConfig, Method, RunBinding,
    TokioClock, WriteCx, WriteGateway, WriteOp, WriteOutcome, WriteQueue, ZamClient, discover,
};
use za_protocol::{EffectKey, RunId};
use za_secrets::{MASK_PREFIX, MaskCell};

/// Решение проекта.
const SOLUTION: &str = "6f1c2b3a-0d4e-4f5a-8b6c-7d8e9f0a1b2c";
const FOREIGN: &str = "0a0b0c0d-1111-4222-8333-444455556666";
const PROJECT: &str = "0123456789abcdef";

fn binding() -> RunBinding {
    RunBinding {
        project_id: PROJECT.to_owned(),
        solution: Some(SOLUTION.to_owned()),
        area: Some("модуль-ядро".to_owned()),
        writable: true,
    }
}

fn cx() -> WriteCx {
    let run = RunId::new();
    WriteCx {
        run,
        session: format!("za-{PROJECT}-{run}"),
        key: EffectKey::derive(run, 4, 2),
        untrusted: false,
        origins: Vec::new(),
        area: None,
        deadline: None,
        scope_global: false,
    }
}

fn insight() -> Value {
    json!({"kind": "insight", "title": "Synthetic insight", "body": "text", "data": {"kind": "discovery"}})
}

/// Поддельный ZAM и шлюз к нему.
struct Rig {
    fake: FakeZam,
    gw: Arc<WriteGateway>,
    link: Arc<Link>,
}

/// Сборка очереди теста по связи шлюза.
type MakeQueue = Box<dyn FnOnce(Arc<Link>) -> Arc<dyn WriteQueue>>;

struct Opts {
    health: Health,
    binding: RunBinding,
    clock: Arc<dyn Clock>,
    queue: Option<MakeQueue>,
    mask: Arc<MaskCell>,
    store: Option<za_store::ProjectStore>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            health: Health::M65,
            binding: binding(),
            clock: Arc::new(TokioClock),
            queue: None,
            mask: Arc::new(MaskCell::default()),
            store: None,
        }
    }
}

async fn rig(o: Opts) -> Rig {
    let mut fake = FakeZam::start().await;
    fake.set_health(o.health);
    let client = Arc::new(ZamClient::new(&MemoryConfig::new(&fake.url()), None).unwrap());
    let d = discover(&client).await;
    let link = Arc::new(Link::new(client, &d, o.clock));
    let queue = match o.queue {
        Some(make) => make(link.clone()),
        None => Arc::new(DirectQueue::new(link.clone())),
    };
    let gw = WriteGateway::new(GatewayParts {
        link: link.clone(),
        binding: o.binding,
        masker: o.mask,
        queue,
        store: o.store,
    });
    fake.seen();
    Rig {
        fake,
        gw: Arc::new(gw),
        link,
    }
}

/// Запросы записей (без `GET /api/v1/health`).
fn writes(seen: &[Seen]) -> Vec<&Seen> {
    seen.iter()
        .filter(|s| s.route() != "/api/v1/health")
        .collect()
}

async fn write(r: &Rig, op: WriteOp, cx: &WriteCx) -> WriteOutcome {
    within(WAIT, "write", r.gw.write(op, cx)).await.unwrap()
}

#[tokio::test]
async fn foreign_scope_refused_without_request() {
    let mut r = rig(Opts::default()).await;
    let mut v = insight();
    v["space"] = json!(FOREIGN);
    let out = write(&r, WriteOp::Log(v), &cx()).await;
    let WriteOutcome::Refused(t) = out else {
        panic!("{out:?}")
    };
    assert!(t.contains("is outside this project"), "{t}");
    let path = json!({"content": "x"});
    let out = write(
        &r,
        WriteOp::Note {
            path: format!("{FOREIGN}:/notes/a"),
            body: path,
        },
        &cx(),
    )
    .await;
    assert!(matches!(out, WriteOutcome::Refused(_)), "{out:?}");
    // Отказ без запроса: канал сервера пуст.
    assert!(r.fake.seen().is_empty());
}

#[tokio::test]
async fn untrusted_binding_is_read_only() {
    let mut r = rig(Opts {
        binding: RunBinding {
            solution: None,
            area: None,
            writable: false,
            ..binding()
        },
        ..Opts::default()
    })
    .await;
    for op in [
        WriteOp::Log(insight()),
        WriteOp::Claim {
            id: "01M8SYNTASK00000000000001".into(),
            ttl_min: None,
        },
        WriteOp::CachePut(json!({"query": "q", "summary": "s", "sources": [{"id": "n:01A"}]})),
    ] {
        let out = write(&r, op, &cx()).await;
        assert_eq!(out, WriteOutcome::Refused(READ_ONLY.to_owned()));
    }
    assert!(r.fake.seen().is_empty());
}

#[tokio::test]
async fn no_solution_needs_explicit_global() {
    let mut r = rig(Opts {
        binding: RunBinding {
            solution: None,
            ..binding()
        },
        ..Opts::default()
    })
    .await;
    let out = write(&r, WriteOp::Log(insight()), &cx()).await;
    let WriteOutcome::Refused(t) = out else {
        panic!("{out:?}")
    };
    assert!(
        t.contains("no memory solution") && t.contains("global"),
        "{t}"
    );
    assert!(r.fake.seen().is_empty());
    let mut c = cx();
    c.scope_global = true;
    let out = write(&r, WriteOp::Log(insight()), &c).await;
    assert!(matches!(out, WriteOutcome::Written { .. }), "{out:?}");
    let seen = r.fake.seen();
    let w = writes(&seen);
    assert_eq!(w.len(), 1, "{seen:?}");
    assert_eq!(
        (w[0].method.as_str(), w[0].route()),
        ("POST", "/api/v1/spaces/knowledge/entries")
    );
    assert!(w[0].body.get("scope").is_none() && w[0].body.get("space").is_none());
    // `changelog` без решения — отказ и со scope global (D135).
    let out = write(
        &r,
        WriteOp::Log(
            json!({"kind": "changelog", "title": "Synthetic change", "data": {"kind": "fix"}}),
        ),
        &c,
    )
    .await;
    let WriteOutcome::Refused(t) = out else {
        panic!("{out:?}")
    };
    assert!(t.starts_with("needs a memory solution"), "{t}");
    assert!(r.fake.seen().is_empty());
}

#[tokio::test]
async fn idempotency_key_is_effect_key() {
    let mut r = rig(Opts::default()).await;
    let c = cx();
    let out = write(&r, WriteOp::Log(insight()), &c).await;
    let WriteOutcome::Written { reference, rev, .. } = out else {
        panic!("{out:?}")
    };
    assert!(reference.starts_with("i:"), "{reference}");
    assert_eq!(rev, 1);
    let seen = r.fake.seen();
    let w = writes(&seen);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].route(), format!("/api/v1/spaces/{SOLUTION}/entries"));
    assert_eq!(
        w[0].header_str("idempotency-key").as_deref(),
        Some(c.key.0.as_str())
    );
    assert_eq!(w[0].header_count("idempotency-key"), 1);
    assert_eq!(
        w[0].header_str("x-zam-session").as_deref(),
        Some(c.session.as_str())
    );
    assert_eq!(w[0].header_str("x-zam-solution").as_deref(), Some(SOLUTION));
    assert_eq!(w[0].header("x-zam-area"), Some("модуль-ядро".as_bytes()));
    // Происхождение в `data.origin` без времени (MEM-076, D145).
    assert_eq!(
        w[0].body["data"]["origin"],
        json!({"session": c.session, "run": c.run.to_string(), "source_kind": "agent", "tainted": false})
    );
    assert_eq!(w[0].body["certainty"], "hypothesis");
}

#[tokio::test]
async fn retry_same_key_single_entry_same_body() {
    let mut r = rig(Opts::default()).await;
    let c = cx();
    let op = WriteOp::Log(json!({
        "kind": "task", "title": "Synthetic task", "tags": ["b", "a"], "data": {"next_step": "write"}
    }));
    let first = write(&r, op.clone(), &c).await;
    let second = write(&r, op, &c).await;
    let WriteOutcome::Written { reference, .. } = first else {
        panic!("{first:?}")
    };
    // Сервер ответил 201, затем 200 — повтор по ключу: запись та же.
    assert_eq!(
        second,
        WriteOutcome::Unchanged {
            reference: reference.clone(),
            warnings: Vec::new()
        }
    );
    assert!(reference.starts_with("k:"), "{reference}");
    let seen = r.fake.seen();
    let w = writes(&seen);
    assert_eq!(w.len(), 2);
    // Тела побайтно равны: повтор не задваивает запись и не даёт 422.
    assert_eq!(w[0].body.to_string(), w[1].body.to_string());
    assert_eq!(
        w[0].header_str("idempotency-key"),
        w[1].header_str("idempotency-key")
    );
    assert_eq!(r.fake.entries().len(), 1);
    assert_eq!(w[0].body["tags"], json!(["a", "b", "src:agent"]));
}

#[tokio::test]
async fn answer_gets_by_agent() {
    let mut r = rig(Opts::default()).await;
    let q = r.fake.add_entry(
        SOLUTION,
        json!({"kind": "question", "title": "Synthetic question"}),
    );
    let out = write(
        &r,
        WriteOp::Answer {
            id: q.clone(),
            body: json!({"option": "a"}),
        },
        &cx(),
    )
    .await;
    let WriteOutcome::Written { reference, .. } = out else {
        panic!("{out:?}")
    };
    assert_eq!(reference, format!("q:{q}"));
    let seen = r.fake.seen();
    let w = writes(&seen);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].route(), format!("/api/v1/entries/{q}/answer"));
    // Без `by` поддельный сервер, как ZAM, ответил бы 400 `by-required`.
    assert_eq!(w[0].body["by"], "agent");
    assert!(w[0].header("idempotency-key").is_none());
}

#[tokio::test]
async fn masked_before_send() {
    let secret = ["sk", "-", "synthetic", "M4sk3dV4lu3Xy9"].concat();
    let mask = Arc::new(MaskCell::default());
    let mut r = rig(Opts {
        mask: mask.clone(),
        ..Opts::default()
    })
    .await;
    // Значение раскрыто после сборки шлюза (D160): маска берётся в момент записи.
    mask.extend(vec![("model_key".to_owned(), secret.clone())]);
    let mut v = insight();
    v["body"] = json!(format!("the key {secret} leaked"));
    v["data"]["note"] = json!([format!("x{secret}")]);
    write(&r, WriteOp::Log(v), &cx()).await;
    let seen = r.fake.seen();
    let w = writes(&seen);
    let raw = w[0].body.to_string();
    assert!(!raw.contains(&secret), "{raw}");
    assert!(raw.contains(MASK_PREFIX), "{raw}");
}

#[tokio::test]
async fn patch_conflict_after_own_success_is_success() {
    let mut r = rig(Opts::default()).await;
    // Подделка рвёт соединение после записи: у клиента — `Interrupted`, исход неизвестен.
    let probe = r
        .fake
        .add_entry(SOLUTION, json!({"kind": "task", "title": "Probe"}));
    let route = format!("/api/v1/entries/{probe}");
    r.fake.cut_after_write(&route);
    let e = r
        .link
        .client()
        .send_json(
            Method::PATCH,
            &route,
            &json!({"status": "in_progress"}),
            &za_memory::Ctx::default(),
            None,
            Some(1),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Interrupted, "{e}");
    r.fake.seen();

    let task = r
        .fake
        .add_entry(SOLUTION, json!({"kind": "task", "title": "Synthetic task"}));
    r.fake.cut_after_write(&format!("/api/v1/entries/{task}"));
    let out = write(
        &r,
        WriteOp::Patch {
            id: task.clone(),
            rev: 1,
            body: json!({"status": "in_progress"}),
        },
        &cx(),
    )
    .await;
    assert_eq!(
        out,
        WriteOutcome::Written {
            reference: format!("k:{task}"),
            rev: 2,
            warnings: Vec::new()
        }
    );
    let seen = r.fake.seen();
    let w: Vec<(&str, &str)> = writes(&seen)
        .iter()
        .map(|s| (s.method.as_str(), s.path.as_str()))
        .collect();
    let path = format!("/api/v1/entries/{task}");
    // Первый PATCH дошёл (ответ потерян), повтор с тем же `If-Match` — 409, перечитывание:
    // статус уже `in_progress` — успех без третьей правки.
    assert_eq!(
        w,
        vec![
            ("PATCH", path.as_str()),
            ("PATCH", path.as_str()),
            ("GET", path.as_str())
        ]
    );
    assert_eq!(r.fake.entry(&task).unwrap()["rev"], 2);
}

#[tokio::test]
async fn append_conflict_retried_once_with_current_rev() {
    let mut r = rig(Opts::default()).await;
    let id = r.fake.add_entry(
        SOLUTION,
        json!({"kind": "insight", "title": "Synthetic", "body": "a", "rev": 2}),
    );
    // Ревизия устарела (2, а не 1): `append` не теряет чужих правок — один повтор с текущей
    // ревизией из 409, без сверки по вхождению текста.
    let out = write(
        &r,
        WriteOp::Patch {
            id: id.clone(),
            rev: 1,
            body: json!({"append_body": "b"}),
        },
        &cx(),
    )
    .await;
    assert!(
        matches!(out, WriteOutcome::Written { rev: 3, .. }),
        "{out:?}"
    );
    let seen = r.fake.seen();
    let w = writes(&seen);
    let shape: Vec<&str> = w.iter().map(|s| s.method.as_str()).collect();
    assert_eq!(shape, ["PATCH", "PATCH"]);
    assert_eq!(w[1].header_str("if-match").as_deref(), Some("2"));
    assert_eq!(r.fake.entry(&id).unwrap()["body"], "a\n\nb");
}

#[tokio::test]
async fn note_path_taken_is_error_not_success() {
    let r = rig(Opts {
        health: Health::NoFeatures,
        ..Opts::default()
    })
    .await;
    let c = cx();
    // Заметку по этому пути раньше писала та же сессия: 409 «путь занят» без неизвестного
    // исхода — ошибка модели, а не успех по авторству ревизии.
    r.fake
        .add_node(SOLUTION, "notes/a", "# old\n", Some(&c.session));
    let e = within(
        WAIT,
        "note",
        r.gw.write(
            WriteOp::Note {
                path: "notes/a".into(),
                body: json!({"content": "# new\n"}),
            },
            &c,
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Conflict, "{e}");
    assert_eq!(r.fake.node_count(), 1);
}

#[tokio::test]
async fn possible_duplicate_returned_to_model() {
    let mut r = rig(Opts::default()).await;
    let dup = "01M8SYNDUPLICATE000000001";
    r.fake.duplicate_title("Synthetic insight", dup);
    let out = write(&r, WriteOp::Log(insight()), &cx()).await;
    assert_eq!(
        out,
        WriteOutcome::Refused(format!(
            "possible duplicate of i:{dup}; append, supersede or set force"
        ))
    );
    // `force` модель задаёт сама: запись проходит.
    let mut v = insight();
    v["force"] = json!(true);
    let out = write(&r, WriteOp::Log(v), &cx()).await;
    assert!(matches!(out, WriteOutcome::Written { .. }), "{out:?}");
    r.fake.seen();
}

#[tokio::test]
async fn precondition_required_rereads_once() {
    let mut r = rig(Opts::default()).await;
    let task = r
        .fake
        .add_entry(SOLUTION, json!({"kind": "task", "title": "Synthetic task"}));
    let path = format!("/api/v1/entries/{task}");
    r.fake
        .fail_once(&path, 428, "/problems/precondition-required", None);
    let out = write(
        &r,
        WriteOp::Patch {
            id: task.clone(),
            rev: 1,
            body: json!({"next_step": "write the test"}),
        },
        &cx(),
    )
    .await;
    assert!(
        matches!(out, WriteOutcome::Written { rev: 2, .. }),
        "{out:?}"
    );
    let seen = r.fake.seen();
    let w = writes(&seen);
    let shape: Vec<&str> = w.iter().map(|s| s.method.as_str()).collect();
    assert_eq!(shape, ["PATCH", "GET", "PATCH"]);
    assert_eq!(w[2].header_str("if-match").as_deref(), Some("1"));
    assert_eq!(
        r.fake.entry(&task).unwrap()["data"]["next_step"],
        "write the test"
    );
}

#[cfg(feature = "test-hooks")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_serialized_per_solution() {
    let mut r = rig(Opts::default()).await;
    let a = r
        .fake
        .add_entry(SOLUTION, json!({"kind": "task", "title": "A"}));
    let b = r
        .fake
        .add_entry(SOLUTION, json!({"kind": "task", "title": "B"}));
    r.fake.hold_writes();
    let spawn = |id: String| {
        let gw = r.gw.clone();
        tokio::spawn(async move {
            gw.write(
                WriteOp::Claim {
                    id,
                    ttl_min: Some(30),
                },
                &cx(),
            )
            .await
            .unwrap()
        })
    };
    let first = spawn(a.clone());
    // Первая запись дошла до сервера (она в канале) и ждёт ответа.
    let got = within(WAIT, "первая запись", async {
        loop {
            if let Some(s) = r.fake.rx.recv().await
                && s.route().ends_with("/claim")
            {
                return s;
            }
        }
    })
    .await;
    assert_eq!(got.route(), format!("/api/v1/entries/{a}/claim"));
    let second = spawn(b.clone());
    // Вторая ждёт мьютекса решения, а не сервера.
    within(
        WAIT,
        "вторая запись ждёт мьютекса",
        async {
            while r.gw.lane_waiters() < 1 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        },
    )
    .await;
    assert!(
        r.fake.seen().iter().all(|s| !s.route().ends_with("/claim")),
        "вторая запись ушла до ответа первой"
    );
    r.fake.release_writes();
    let o1 = within(WAIT, "первая", first).await.unwrap();
    let o2 = within(WAIT, "вторая", second).await.unwrap();
    assert!(matches!(o1, WriteOutcome::Written { .. }), "{o1:?}");
    assert!(matches!(o2, WriteOutcome::Written { .. }), "{o2:?}");
    let seen = r.fake.seen();
    let claims: Vec<String> = seen
        .iter()
        .filter(|s| s.route().ends_with("/claim"))
        .map(|s| s.route().to_owned())
        .collect();
    assert_eq!(claims, [format!("/api/v1/entries/{b}/claim")]);
}

#[tokio::test]
async fn warnings_preserved() {
    let r = rig(Opts::default()).await;
    r.fake.warn_next(&["secrets_detected", "term_alias"]);
    let out = write(&r, WriteOp::Log(insight()), &cx()).await;
    let WriteOutcome::Written { warnings, .. } = out else {
        panic!("{out:?}")
    };
    assert_eq!(warnings, ["secrets_detected", "term_alias"]);
}

#[tokio::test]
async fn nodes_without_flag_use_base_rev() {
    // L1 (`0.0.10-m5.7`): флага `idempotent_nodes` нет.
    let mut r = rig(Opts {
        health: Health::NoFeatures,
        ..Opts::default()
    })
    .await;
    let c = cx();
    let out = write(
        &r,
        WriteOp::Note {
            path: "notes/a".into(),
            body: json!({"content": "# A\n"}),
        },
        &c,
    )
    .await;
    let WriteOutcome::Written { reference, rev, .. } = out else {
        panic!("{out:?}")
    };
    assert_eq!(rev, 1);
    let id = reference.strip_prefix("n:").unwrap().to_owned();
    let seen = r.fake.seen();
    let w = writes(&seen);
    assert_eq!(
        (w[0].method.as_str(), w[0].route()),
        ("POST", "/api/v1/nodes")
    );
    assert!(w[0].header("idempotency-key").is_none());
    assert_eq!(w[0].body["space"], SOLUTION);
    assert_eq!(w[0].body["path"], "notes/a");
    assert_eq!(w[0].body["kind"], "note");

    // PATCH с `base_rev`; ответ потерян — сверка по ревизиям: ревизия 2 этой сессии — успех
    // без второй правки.
    r.fake.cut_after_write(&format!("/api/v1/nodes/{id}"));
    let out = write(
        &r,
        WriteOp::NodePatch {
            id: id.clone(),
            rev: 1,
            ops: json!([{"op": "append", "content": "more\n"}]),
        },
        &c,
    )
    .await;
    assert_eq!(
        out,
        WriteOutcome::Written {
            reference: format!("n:{id}"),
            rev: 2,
            warnings: Vec::new()
        }
    );
    let seen = r.fake.seen();
    let w = writes(&seen);
    let shape: Vec<(&str, String)> = w
        .iter()
        .map(|s| (s.method.as_str(), s.route().to_owned()))
        .collect();
    assert_eq!(
        shape,
        [
            ("PATCH", format!("/api/v1/nodes/{id}")),
            ("GET", format!("/api/v1/nodes/{id}/revisions")),
        ]
    );
    assert_eq!(w[0].body["base_rev"], 1);
    assert!(w[0].header("idempotency-key").is_none());
    assert_eq!(r.fake.node(&id).unwrap(), ("# A\nmore\n".to_owned(), 2));
}

#[tokio::test]
async fn nodes_with_flag_send_effect_key() {
    let mut r = rig(Opts::default()).await;
    let c = cx();
    let out = write(
        &r,
        WriteOp::Note {
            path: "notes/a".into(),
            body: json!({"content": "# A\n"}),
        },
        &c,
    )
    .await;
    assert!(matches!(out, WriteOutcome::Written { .. }), "{out:?}");
    let seen = r.fake.seen();
    let w = writes(&seen);
    assert_eq!(
        w[0].header_str("idempotency-key").as_deref(),
        Some(c.key.0.as_str())
    );
}

#[tokio::test]
async fn review_not_queued_when_offline() {
    let idea = "01M8SYNIDEA00000000000001";
    let review = || WriteOp::Review {
        id: idea.into(),
        body: json!({"outcome": "decline", "summary": "s".repeat(200)}),
    };
    // Известный L0: без попытки.
    let url = closed_port_url();
    let client = Arc::new(ZamClient::new(&MemoryConfig::new(&url), None).unwrap());
    let d = discover(&client).await;
    let link = Arc::new(Link::new(client.clone(), &d, Arc::new(TokioClock)));
    let gw = WriteGateway::new(GatewayParts {
        link: link.clone(),
        binding: binding(),
        masker: Arc::new(MaskCell::default()),
        queue: Arc::new(DirectQueue::new(link.clone())),
        store: None,
    });
    let not_queued = WriteOutcome::Refused("memory unavailable: review is not queued".into());
    assert_eq!(
        within(WAIT, "review L0", gw.write(review(), &cx()))
            .await
            .unwrap(),
        not_queued
    );
    // L2 по обнаружению, но сервера нет: нет соединения — тот же отказ, уровень — L0.
    let d = from_health(
        &json!({"status": "ok", "version": "0.0.12-m6.5", "features": ["m6", "idea_review"]}),
    );
    let link = Arc::new(Link::new(client, &d, Arc::new(TokioClock)));
    let gw = WriteGateway::new(GatewayParts {
        link: link.clone(),
        binding: binding(),
        masker: Arc::new(MaskCell::default()),
        queue: Arc::new(DirectQueue::new(link.clone())),
        store: None,
    });
    assert_eq!(
        within(WAIT, "review offline", gw.write(review(), &cx()))
            .await
            .unwrap(),
        not_queued
    );
    assert_eq!(link.level(), za_memory::Level::L0);
    // Запись без ключа при L0 тоже не встаёт в очередь (D165).
    let out = within(
        WAIT,
        "claim L0",
        gw.write(
            WriteOp::Claim {
                id: idea.into(),
                ttl_min: None,
            },
            &cx(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        WriteOutcome::Refused("memory unavailable: claim is not queued".into())
    );
    // Запись с ключом без outbox (`DirectQueue`) — ошибка `memory unavailable`.
    let e = within(WAIT, "log L0", gw.write(WriteOp::Log(insight()), &cx()))
        .await
        .unwrap_err();
    assert_eq!(e.title, "memory unavailable");
}

#[tokio::test]
async fn review_key_found_after_mismatch_is_unchanged() {
    let mut r = rig(Opts::default()).await;
    let idea = r.fake.add_entry(
        SOLUTION,
        json!({"kind": "idea", "title": "Synthetic idea", "data": {"origin": "human"}}),
    );
    let route = format!("/api/v1/entries/{idea}/review");
    // Разбор записан, ответ потерян; сразу после этого человек правит идею: повтор с тем же
    // ключом — 422 `idempotency-mismatch`.
    r.fake.cut_after_write(&route);
    r.fake.touch_after_cut(&idea);
    let c = cx();
    let out = write(
        &r,
        WriteOp::Review {
            id: idea.clone(),
            body: json!({"outcome": "decline", "summary": "s".repeat(200)}),
        },
        &c,
    )
    .await;
    assert_eq!(
        out,
        WriteOutcome::Unchanged {
            reference: format!("w:{idea}"),
            warnings: Vec::new()
        }
    );
    let seen = r.fake.seen();
    let w = writes(&seen);
    let shape: Vec<(&str, String)> = w
        .iter()
        .map(|s| (s.method.as_str(), s.route().to_owned()))
        .collect();
    assert_eq!(
        shape,
        [
            ("POST", route.clone()),
            ("POST", route.clone()),
            ("GET", format!("/api/v1/entries/{idea}")),
        ]
    );
    for s in &w[..2] {
        assert_eq!(
            s.header_str("idempotency-key").as_deref(),
            Some(c.key.0.as_str())
        );
    }
}

#[tokio::test]
async fn review_replay_is_unchanged_and_idea_mode_returned() {
    let r = rig(Opts::default()).await;
    let idea = r.fake.add_entry(
        SOLUTION,
        json!({"kind": "idea", "title": "Synthetic idea", "data": {"origin": "human"}}),
    );
    let op = WriteOp::Review {
        id: idea.clone(),
        body: json!({"outcome": "decline", "summary": "s".repeat(200)}),
    };
    let c = cx();
    let first = write(&r, op.clone(), &c).await;
    assert!(matches!(first, WriteOutcome::Written { .. }), "{first:?}");
    // Повтор с тем же ключом до правки идеи — `replay: true`.
    assert!(matches!(
        write(&r, op, &c).await,
        WriteOutcome::Unchanged { .. }
    ));
    // Режим `propose` (D169): 409 `idea-mode` возвращается вызывающему как признак режима.
    r.fake.set_idea_mode("propose");
    let other = r.fake.add_entry(
        SOLUTION,
        json!({"kind": "idea", "title": "Other idea", "data": {"origin": "human"}}),
    );
    let e = within(
        WAIT,
        "review propose",
        r.gw.write(
            WriteOp::Review {
                id: other,
                body: json!({"outcome": "decline", "summary": "s".repeat(200)}),
            },
            &cx(),
        ),
    )
    .await
    .unwrap_err();
    assert!(is_idea_mode(&e), "{e}");
}

#[tokio::test]
async fn tainted_edit_of_trusted_target_refused_after_one_read() {
    let mut r = rig(Opts::default()).await;
    let id = r.fake.add_entry(
        SOLUTION,
        json!({"kind": "insight", "title": "Trusted", "certainty": "theorem"}),
    );
    let mut c = cx();
    c.untrusted = true;
    c.origins = vec!["web".into()];
    let out = write(
        &r,
        WriteOp::Patch {
            id: id.clone(),
            rev: 1,
            body: json!({"append_body": "rewritten by a hostile page"}),
        },
        &c,
    )
    .await;
    assert_eq!(
        out,
        WriteOutcome::Refused("needs human confirmation (tainted context)".into())
    );
    let seen = r.fake.seen();
    let w = writes(&seen);
    // Один запрос доверия цели, правки нет.
    assert_eq!(w.len(), 1, "{seen:?}");
    assert_eq!(
        (w[0].method.as_str(), w[0].route()),
        ("GET", format!("/api/v1/entries/{id}").as_str())
    );
}

#[tokio::test]
async fn theorem_kept_only_with_finished_call() {
    let dir = unique_dir::unique_dir("za-memory-gateway", "evidence");
    let store = za_store::ProjectStore::open(&dir).unwrap();
    let c = cx();
    store
        .append(za_protocol::NewEvent {
            run_id: c.run,
            parent_seq: None,
            actor: za_protocol::Actor::Tool,
            kind: za_protocol::EventKind::ToolFinished,
            payload: json!({"call_id": "c1", "is_error": false, "duration_ms": 5, "tainted": false}),
            tainted: false,
        })
        .await
        .unwrap();
    store
        .append(za_protocol::NewEvent {
            run_id: c.run,
            parent_seq: None,
            actor: za_protocol::Actor::Tool,
            kind: za_protocol::EventKind::ToolFinished,
            payload: json!({"call_id": "c2", "is_error": true, "duration_ms": 5, "tainted": false}),
            tainted: false,
        })
        .await
        .unwrap();
    let mut r = rig(Opts {
        store: Some(store),
        ..Opts::default()
    })
    .await;
    let theorem = |call: &str| {
        WriteOp::Log(json!({
            "kind": "insight", "title": format!("Synthetic theorem {call}"), "certainty": "theorem",
            "data": {"kind": "discovery", "evidence": {"checked": format!("call:{call}"), "method": "cargo test", "result": "green"}},
        }))
    };
    // Вызовы того же run: ключи эффектов разные.
    let at = |seq: u64| WriteCx {
        key: EffectKey::derive(c.run, 5, seq),
        ..c.clone()
    };
    let out = write(&r, theorem("c1"), &at(1)).await;
    assert!(
        matches!(&out, WriteOutcome::Written { warnings, .. } if warnings.is_empty()),
        "{out:?}"
    );
    // Вызов с ошибкой и вызов, которого нет, — понижение с пометкой (D163 правило 6).
    for (i, call) in ["c2", "c9"].into_iter().enumerate() {
        let out = write(&r, theorem(call), &at(2 + i as u64)).await;
        let WriteOutcome::Written { warnings, .. } = out else {
            panic!("{out:?}")
        };
        assert_eq!(warnings, [NOTE_EVIDENCE]);
    }
    let seen = r.fake.seen();
    let certs: Vec<&str> = writes(&seen)
        .iter()
        .map(|s| s.body["certainty"].as_str().unwrap())
        .collect();
    assert_eq!(certs, ["theorem", "hypothesis", "hypothesis"]);
}

/// Очередь, которая строку только хранит: итог — `Queued` (outbox при известном L0, D165).
struct Keep(std::sync::Mutex<Vec<za_memory::QueuedWrite>>);

impl WriteQueue for Keep {
    fn submit(
        &self,
        row: za_memory::QueuedWrite,
        _deadline: Option<std::time::Instant>,
    ) -> za_core::BoxFuture<'_, za_memory::Submit> {
        self.0.lock().unwrap().push(row);
        Box::pin(async { za_memory::Submit::Queued { outbox: 3 } })
    }
}

#[tokio::test]
async fn supersede_at_l0_goes_to_queue_only_in_clean_context() {
    let url = closed_port_url();
    let client = Arc::new(ZamClient::new(&MemoryConfig::new(&url), None).unwrap());
    let d = discover(&client).await;
    let link = Arc::new(Link::new(client, &d, Arc::new(TokioClock)));
    let keep = Arc::new(Keep(std::sync::Mutex::new(Vec::new())));
    let gw = WriteGateway::new(GatewayParts {
        link,
        binding: binding(),
        masker: Arc::new(MaskCell::default()),
        queue: keep.clone(),
        store: None,
    });
    let op = || WriteOp::Supersede {
        id: "01M8SYNENTRY0000000000001".into(),
        body: json!({"title": "Newer", "reason": "the old check was wrong", "data": {"kind": "discovery"}}),
    };
    // Доверие цели не прочитать: в чистом контексте запись с ключом встаёт в очередь.
    let out = within(WAIT, "supersede L0", gw.write(op(), &cx()))
        .await
        .unwrap();
    assert_eq!(
        out,
        WriteOutcome::Queued {
            outbox: 3,
            warnings: Vec::new()
        }
    );
    let rows = keep.0.lock().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].path,
        "/api/v1/entries/01M8SYNENTRY0000000000001/supersede"
    );
    assert_eq!(rows[0].body["message"], "the old check was wrong");
    // Из tainted-контекста без доверия цели — отказ (D163 правило 3).
    let mut c = cx();
    c.untrusted = true;
    let out = within(WAIT, "supersede tainted L0", gw.write(op(), &c))
        .await
        .unwrap();
    assert_eq!(
        out,
        WriteOutcome::Refused("memory unavailable: supersede is not queued".into())
    );
    assert_eq!(keep.0.lock().unwrap().len(), 1);
}

#[cfg(feature = "test-hooks")]
#[tokio::test]
async fn busy_retried_on_clock_before_deadline() {
    use za_memory::ManualClock;
    let clock = Arc::new(ManualClock::new());
    let mut r = rig(Opts {
        clock: clock.clone(),
        ..Opts::default()
    })
    .await;
    let task = r
        .fake
        .add_entry(SOLUTION, json!({"kind": "task", "title": "T"}));
    let route = format!("/api/v1/entries/{task}/claim");
    r.fake.fail_once(&route, 503, "/problems/busy", Some(1));
    let gw = r.gw.clone();
    let w = tokio::spawn(async move {
        gw.write(
            WriteOp::Claim {
                id: task,
                ttl_min: None,
            },
            &cx(),
        )
        .await
        .unwrap()
    });
    // Пауза повтора — по часам шлюза (D165), без настоящего ожидания.
    within(WAIT, "пауза повтора", clock.wait_sleepers(1)).await;
    clock.advance(std::time::Duration::from_secs(2));
    let out = within(WAIT, "повтор", w).await.unwrap();
    assert!(matches!(out, WriteOutcome::Written { .. }), "{out:?}");
    let n = r.fake.seen().iter().filter(|s| s.route() == route).count();
    assert_eq!(n, 2);
}

#[cfg(feature = "test-hooks")]
mod deadline {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use za_core::BoxFuture;
    use za_memory::{ManualClock, QueuedWrite, Submit};

    use super::*;

    /// Очередь теста: строку отправляет сразу (сервер молчит), дедлайн — по ручным часам; не
    /// успела — строка «остаётся в очереди» (`Queued`), как в outbox задачи 7.
    struct Recording {
        link: Arc<Link>,
        clock: Arc<ManualClock>,
        rows: Mutex<Vec<(QueuedWrite, Option<Instant>)>>,
    }

    impl WriteQueue for Recording {
        fn submit(&self, row: QueuedWrite, deadline: Option<Instant>) -> BoxFuture<'_, Submit> {
            Box::pin(async move {
                self.rows.lock().unwrap().push((row.clone(), deadline));
                let cx = za_memory::Ctx {
                    session: Some(&row.session),
                    solution: row.solution.as_deref(),
                    area: row.area.as_deref(),
                };
                let send = self.link.client().send_json(
                    Method::POST,
                    &row.path,
                    &row.body,
                    &cx,
                    Some(&row.key),
                    None,
                    Duration::from_secs(60),
                );
                let wait = deadline
                    .unwrap()
                    .saturating_duration_since(Clock::now(&*self.clock));
                tokio::select! {
                    r = send => Submit::Sent(r),
                    () = self.clock.sleep(wait) => Submit::Queued { outbox: 7 },
                }
            })
        }
    }

    #[tokio::test]
    async fn deadline_hands_over_to_queue() {
        let clock = Arc::new(ManualClock::new());
        let rec: Arc<Mutex<Option<Arc<Recording>>>> = Arc::new(Mutex::new(None));
        let hold = rec.clone();
        let mc = clock.clone();
        let mut r = rig(Opts {
            clock: clock.clone(),
            queue: Some(Box::new(move |link| {
                let q = Arc::new(Recording {
                    link,
                    clock: mc,
                    rows: Mutex::new(Vec::new()),
                });
                *hold.lock().unwrap() = Some(q.clone());
                q as Arc<dyn WriteQueue>
            })),
            ..Opts::default()
        })
        .await;
        r.fake.hold_writes();
        let mut c = cx();
        let call_deadline = Clock::now(&*clock) + Duration::from_secs(10);
        c.deadline = Some(call_deadline);
        let gw = r.gw.clone();
        let cc = c.clone();
        let w = tokio::spawn(async move { gw.write(WriteOp::Log(insight()), &cc).await.unwrap() });
        // Запрос дошёл до сервера, сервер молчит; очередь ждёт дедлайна по часам.
        within(WAIT, "запрос записи", async {
            loop {
                if let Some(s) = r.fake.rx.recv().await
                    && s.route().ends_with("/entries")
                {
                    return;
                }
            }
        })
        .await;
        within(WAIT, "ожидание дедлайна", clock.wait_sleepers(1)).await;
        clock.advance(Duration::from_secs(8));
        let out = within(WAIT, "итог записи", w).await.unwrap();
        assert_eq!(
            out,
            WriteOutcome::Queued {
                outbox: 7,
                warnings: Vec::new()
            }
        );
        let q = rec.lock().unwrap().clone().unwrap();
        let rows = q.rows.lock().unwrap();
        assert_eq!(rows.len(), 1);
        let (row, deadline) = &rows[0];
        // Дедлайн вызова минус 2 с (D165); ключ — ключ эффекта; тело — после шлюза.
        assert_eq!(*deadline, Some(call_deadline - Duration::from_secs(2)));
        assert_eq!(row.key, c.key.0);
        assert_eq!(row.op, "log");
        assert_eq!(row.kind.as_deref(), Some("insight"));
        assert_eq!(row.path, format!("/api/v1/spaces/{SOLUTION}/entries"));
        assert_eq!(row.session, c.session);
        assert!(row.body["data"]["origin"].is_object());
        r.fake.release_writes();
    }

    #[tokio::test]
    async fn deadline_without_call_deadline_is_tool_timeout() {
        let clock = Arc::new(ManualClock::new());
        let rec: Arc<Mutex<Option<Arc<Recording>>>> = Arc::new(Mutex::new(None));
        let hold = rec.clone();
        let mc = clock.clone();
        let r = rig(Opts {
            clock: clock.clone(),
            queue: Some(Box::new(move |link| {
                let q = Arc::new(Recording {
                    link,
                    clock: mc,
                    rows: Mutex::new(Vec::new()),
                });
                *hold.lock().unwrap() = Some(q.clone());
                q as Arc<dyn WriteQueue>
            })),
            ..Opts::default()
        })
        .await;
        let start = Clock::now(&*clock);
        let out = write(&r, WriteOp::Log(insight()), &cx()).await;
        assert!(matches!(out, WriteOutcome::Written { .. }), "{out:?}");
        let q = rec.lock().unwrap().clone().unwrap();
        let rows = q.rows.lock().unwrap();
        // Без дедлайна вызова — тайм-аут инструмента (40 с) минус 2 с.
        assert_eq!(rows[0].1, Some(start + Duration::from_secs(38)));
    }
}
