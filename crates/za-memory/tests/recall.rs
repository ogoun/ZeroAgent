//! Порт памяти `ZamMemory` (R1-M D140, D141, D142, D168, D169; П3, Δ11 изменений по ZAM
//! 0.0.12-m6.5): запросы `/recall` без `output_limit`, итоги `Recall`, укладка в бюджет,
//! подсказки с маской ввода, признак режима идей, уровень и повторная проба — на поддельном
//! ZAM (`test-support/fake_zam.rs`, D154), только `127.0.0.1`.

#[path = "../../../test-support/fake_zam.rs"]
mod fake_zam;
mod support;

use std::sync::Arc;

use fake_zam::{
    FakeZam, Health, M65_IDEA_NEW, RecallAnswer, WAIT, digest_m65_review, prompt_m65_review,
    recall_item, within,
};
use serde_json::{Value, json};
use za_core::memory::{IdeasSignal, MemoryLevel, MemoryPort, Recall};
use za_memory::fit::{OMITTED_HEAD, approx_tokens};
use za_memory::{IdeasMode, TokioClock, session_id};
use za_protocol::RunId;
use za_secrets::{MASK_PREFIX, SecretMasker};

use support::{AREA, PROJECT, SOLUTION, port, port_at};

const RULES_HEAVY: &str = include_str!("fixtures/digest/rules_heavy.md");

/// Последний запрос `/recall` из канала поддельного сервера.
fn last_recall(fake: &mut FakeZam) -> fake_zam::Seen {
    fake.seen()
        .into_iter()
        .rev()
        .find(|s| s.route() == "/api/v1/recall")
        .expect("запроса /recall не было")
}

/// Фиктивный секрет — из частей во время выполнения (GitHub push protection).
fn secret() -> String {
    ["sk", "-", "synthetic", "Q7w2E9r4T1y6U3i8"].concat()
}

#[tokio::test]
async fn snapshot_request_shape() {
    let mut fake = FakeZam::start().await;
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    fake.seen();
    let run = RunId::new();
    let r = within(WAIT, "snapshot", mem.snapshot(run, 200)).await;
    let seen = fake.seen();
    assert_eq!(seen.len(), 2, "{seen:?}");
    // Регистрация сессии — раньше `/recall` (D139).
    assert_eq!(seen[0].route(), "/api/v1/sessions");
    let rec = &seen[1];
    assert_eq!(
        (rec.method.as_str(), rec.route()),
        ("POST", "/api/v1/recall")
    );
    let id = session_id(PROJECT, run);
    // Ровно эти поля: без `output_limit` (D141), `source`, `code_map`, `agent_block_version`
    // (D140); `budget_tokens` — `fetch_budget(200)`.
    assert_eq!(
        rec.body,
        json!({
            "event": "session_start",
            "session_id": id,
            "solution": SOLUTION,
            "area": AREA,
            "budget_tokens": 800,
            "timeout_ms": 1500,
        })
    );
    for absent in ["output_limit", "source", "code_map", "agent_block_version"] {
        assert!(rec.body.get(absent).is_none(), "{absent}");
    }
    assert_eq!(
        rec.header_str("x-zam-session").as_deref(),
        Some(id.as_str())
    );
    assert_eq!(rec.header_str("x-zam-solution").as_deref(), Some(SOLUTION));
    // Пустой ответ — `Empty`, признака режима нет.
    assert_eq!(r.recall, Recall::Empty);
    assert_eq!(r.ideas, None);
    assert_eq!(mem.level(), MemoryLevel::L2);
}

#[tokio::test]
async fn recall_timeout_warning_is_timeout() {
    let fake = FakeZam::start().await;
    fake.answer_recall(
        "session_start",
        RecallAnswer::text("").warning("recall timeout"),
    );
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    let r = within(WAIT, "snapshot", mem.snapshot(RunId::new(), 200)).await;
    assert_eq!(r.recall, Recall::Timeout);
    // Медленный сбор — не недоступность: уровень прежний.
    assert_eq!(mem.level(), MemoryLevel::L2);
}

#[tokio::test]
async fn recall_timeout_with_text_is_partial() {
    let fake = FakeZam::start().await;
    fake.answer_recall(
        "session_start",
        RecallAnswer::text(RULES_HEAVY).warning("recall timeout"),
    );
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    let r = within(WAIT, "snapshot", mem.snapshot(RunId::new(), 200)).await;
    let Recall::Partial(text) = &r.recall else {
        panic!("{:?}", r.recall)
    };
    // Текст уложен в бюджет (D141): задачи первыми, правила сокращены, снятое названо.
    assert!(text.starts_with("### Задачи"), "{text}");
    assert!(text.contains("далее: write test"), "{text}");
    assert!(approx_tokens(text) <= 200, "{text}");
    assert!(
        text.lines().last().unwrap().starts_with(OMITTED_HEAD),
        "{text}"
    );
}

#[tokio::test]
async fn snapshot_fits_rules_reserve_into_budget() {
    let fake = FakeZam::start().await;
    fake.answer_recall("session_start", RecallAnswer::text(RULES_HEAVY));
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    let r = within(WAIT, "snapshot", mem.snapshot(RunId::new(), 200)).await;
    let Recall::Hits(text) = &r.recall else {
        panic!("{:?}", r.recall)
    };
    assert!(text.starts_with("### Задачи"), "{text}");
    assert!(approx_tokens(text) <= 200);
    let reports = mem.drain_reports();
    assert_eq!(reports.len(), 1, "{reports:?}");
    let rep = &reports[0];
    assert_eq!((rep.event, rep.state), ("session_start", "hits"));
    assert!(rep.tokens <= 200, "{rep:?}");
    assert!(rep.omitted.iter().any(|s| s == "Правила"), "{rep:?}");
    // Данные события — без текста памяти (D153).
    let payload = rep.to_payload();
    assert!(
        !payload.to_string().contains("Синтетическое правило"),
        "{payload}"
    );
    for k in [
        "event",
        "state",
        "tokens",
        "rules_tokens",
        "omitted",
        "ms",
        "warnings",
        "hooks_off",
    ] {
        assert!(payload.get(k).is_some(), "{k}: {payload}");
    }
}

#[tokio::test]
async fn hooks_off_reported() {
    let fake = FakeZam::start().await;
    fake.hook_off("prompt");
    fake.answer_recall(
        "prompt",
        RecallAnswer::text("ZeroAIMemory: подсказка i:01M8SYNTH00000000000000I05"),
    );
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    let run = RunId::new();
    within(WAIT, "snapshot", mem.snapshot(run, 200)).await;
    let reports = mem.drain_reports();
    assert_eq!(reports[0].event, "session_start");
    assert_eq!(reports[0].hooks_off, vec!["prompt".to_owned()]);
    assert_eq!(reports[0].to_payload()["hooks_off"], json!(["prompt"]));
    // Подсказка выключенного типа — пустой текст; событие называет причину.
    let r = within(WAIT, "prompt", mem.prompt_hint(run, "ввод", 100)).await;
    assert_eq!(r.recall, Recall::Empty);
    let reports = mem.drain_reports();
    assert_eq!((reports[0].event, reports[0].state), ("prompt", "empty"));
    assert_eq!(reports[0].hooks_off, vec!["prompt".to_owned()]);
}

#[tokio::test]
async fn no_recall_timeout_flag_omits_field() {
    let mut fake = FakeZam::start().await;
    fake.set_health(Health::NoFeatures);
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    assert_eq!(mem.level(), MemoryLevel::L1);
    within(WAIT, "snapshot", mem.snapshot(RunId::new(), 200)).await;
    let rec = last_recall(&mut fake);
    assert_eq!(rec.body["event"], "session_start");
    assert!(rec.body.get("timeout_ms").is_none(), "{}", rec.body);
    assert_eq!(rec.body["budget_tokens"], 800);
}

#[tokio::test]
async fn recovery_request_shape() {
    let mut fake = FakeZam::start().await;
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    let run = RunId::new();
    within(WAIT, "recovery", mem.after_compaction(run, 400)).await;
    let rec = last_recall(&mut fake);
    // Эпоху начинает сам `recovery`: `new_epoch` не нужен; без `output_limit` (D140, D141).
    assert_eq!(
        rec.body,
        json!({
            "event": "recovery",
            "session_id": session_id(PROJECT, run),
            "solution": SOLUTION,
            "area": AREA,
            "budget_tokens": 1600,
            "timeout_ms": 1500,
        })
    );
}

#[tokio::test]
async fn prompt_hint_masks_and_truncates() {
    let mut fake = FakeZam::start().await;
    let masker = SecretMasker::new(vec![("TEST_KEY".to_owned(), secret())]);
    let client =
        za_memory::ZamClient::new(&za_memory::MemoryConfig::new(&fake.url()), None).unwrap();
    let d = za_memory::discover(&client).await;
    let mem = port(
        &fake.url(),
        d,
        Arc::new(TokioClock),
        masker,
        IdeasMode::Wait,
    );
    let prompt = format!("{} {} {}", "a".repeat(100), secret(), "б".repeat(1900));
    assert!(prompt.chars().count() > 2000);
    let run = RunId::new();
    within(WAIT, "prompt", mem.prompt_hint(run, &prompt, 100)).await;
    let rec = last_recall(&mut fake);
    let sent = rec.body["prompt"].as_str().unwrap();
    assert!(sent.chars().count() <= 512, "{}", sent.chars().count());
    assert!(!sent.contains(&secret()), "{sent}");
    assert!(!rec.body.to_string().contains(&secret()));
    assert!(sent.contains(MASK_PREFIX), "{sent}");
    assert_eq!(rec.body["event"], "prompt");
    assert_eq!(rec.body["timeout_ms"], 800);
    assert_eq!(rec.body["budget_tokens"], 400);
    assert_eq!(rec.body["session_id"], json!(session_id(PROJECT, run)));
}

#[tokio::test]
async fn tool_error_hint_masks_and_truncates() {
    let mut fake = FakeZam::start().await;
    let masker = SecretMasker::new(vec![("TEST_KEY".to_owned(), secret())]);
    let client =
        za_memory::ZamClient::new(&za_memory::MemoryConfig::new(&fake.url()), None).unwrap();
    let d = za_memory::discover(&client).await;
    let mem = port(
        &fake.url(),
        d,
        Arc::new(TokioClock),
        masker,
        IdeasMode::Wait,
    );
    let error = format!("error: {} {}", secret(), "ошибка ".repeat(2000));
    within(
        WAIT,
        "tool_error",
        mem.tool_error_hint(RunId::new(), "bash", &error, 150),
    )
    .await;
    let rec = last_recall(&mut fake);
    let sent = rec.body["error"].as_str().unwrap();
    assert!(sent.len() <= 4096, "{}", sent.len());
    assert!(!rec.body.to_string().contains(&secret()));
    assert!(sent.contains(MASK_PREFIX), "{sent}");
    assert_eq!(rec.body["event"], "post_tool");
    assert_eq!(rec.body["tool"], "bash");
    assert_eq!(rec.body["timeout_ms"], 1000);
    assert_eq!(rec.body["budget_tokens"], 600);
}

#[tokio::test]
async fn zero_budget_is_capped_without_request() {
    let mut fake = FakeZam::start().await;
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    fake.seen();
    let run = RunId::new();
    let r = within(WAIT, "prompt", mem.prompt_hint(run, "ввод", 0)).await;
    assert_eq!(r.recall, Recall::Capped);
    let r = within(WAIT, "retry", mem.retry_digest(run, 0)).await;
    assert_eq!(r, Recall::Capped);
    assert!(fake.seen().is_empty());
}

#[tokio::test]
async fn recall_http_timeout_is_timeout() {
    let mut fake = FakeZam::start().await;
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    let run = RunId::new();
    within(WAIT, "snapshot", mem.snapshot(run, 200)).await;
    fake.seen();
    // Ответ задержан воротами: клиент упирается в свой срок (`timeout_ms` + 1 с, D137).
    fake.hold_recall();
    let r = within(WAIT, "prompt", mem.prompt_hint(run, "ввод", 100)).await;
    fake.release_recall();
    assert_eq!(r.recall, Recall::Timeout);
    assert_eq!(last_recall(&mut fake).body["event"], "prompt");
    // Срок вышел — сервер жив: уровень прежний.
    assert_eq!(mem.level(), MemoryLevel::L2);
    assert_eq!(mem.drain_reports().last().unwrap().state, "timeout");
}

#[tokio::test]
async fn idea_review_reason_gives_review_signal() {
    let fake = FakeZam::start().await;
    fake.set_health(Health::M65);
    fake.answer_recall("session_start", digest_m65_review());
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    let run = RunId::new();
    let r = within(WAIT, "snapshot", mem.snapshot(run, 1500)).await;
    assert_eq!(r.ideas, Some(IdeasSignal::Review));
    // Режим порта — `Wait`: раздел разбора и строка плана сняты, задачи на месте (D141, П3).
    let text = r.recall.text().unwrap();
    assert!(text.contains("далее: write test"), "{text}");
    for gone in ["### Идеи на разбор", "План из идей", M65_IDEA_NEW] {
        assert!(!text.contains(gone), "{gone}:\n{text}");
    }
    // Ответ без `reason: idea_review` признака не даёт.
    fake.answer_recall(
        "prompt",
        RecallAnswer::text(
            "ZeroAIMemory: идея «Синтетическая» [normal] — w:01M8SYNTH00000000000000W07",
        )
        .items(vec![recall_item(
            "w:01M8SYNTH00000000000000W07",
            "idea",
            "idea",
        )]),
    );
    let r = within(WAIT, "prompt", mem.prompt_hint(run, "ввод", 100)).await;
    assert_eq!(r.ideas, None);
    assert!(r.recall.text().unwrap().contains("W07"));
}

#[tokio::test]
async fn review_mode_keeps_idea_review_digest() {
    let fake = FakeZam::start().await;
    fake.set_health(Health::M65);
    fake.answer_recall("session_start", digest_m65_review());
    let mem = port_at(&fake.url(), IdeasMode::Review).await;
    let r = within(WAIT, "snapshot", mem.snapshot(RunId::new(), 1500)).await;
    assert_eq!(r.ideas, Some(IdeasSignal::Review));
    let text = r.recall.text().unwrap();
    for kept in ["### Идеи на разбор", "План из идей", M65_IDEA_NEW] {
        assert!(text.contains(kept), "{kept}:\n{text}");
    }
}

#[tokio::test]
async fn review_mode_needs_server_flag() {
    // Сервер без флага `idea_review` (форма `0.0.11-m6`): режим порта — всегда `Wait`.
    let fake = FakeZam::start().await;
    fake.answer_recall("session_start", digest_m65_review());
    let mem = port_at(&fake.url(), IdeasMode::Review).await;
    let r = within(WAIT, "snapshot", mem.snapshot(RunId::new(), 1500)).await;
    let text = r.recall.text().unwrap();
    assert!(!text.contains("### Идеи на разбор"), "{text}");
}

#[tokio::test]
async fn wait_drops_idea_review_prompt_hint() {
    let fake = FakeZam::start().await;
    fake.set_health(Health::M65);
    fake.answer_recall("prompt", prompt_m65_review());
    let wait = port_at(&fake.url(), IdeasMode::Wait).await;
    let run = RunId::new();
    let r = within(WAIT, "prompt", wait.prompt_hint(run, "ввод", 150)).await;
    // Подсказка разбора велит звать инструмент, которого у модели в `Wait` нет (Δ11): снята
    // целиком; признак режима — есть.
    assert_eq!(r.ideas, Some(IdeasSignal::Review));
    let text = r.recall.text().unwrap();
    assert!(!text.contains(M65_IDEA_NEW), "{text}");
    assert!(!text.contains("memory_recall"), "{text}");
    assert!(text.contains("синтетическая ловушка"), "{text}");
    let review = port_at(&fake.url(), IdeasMode::Review).await;
    let r = within(
        WAIT,
        "prompt",
        review.prompt_hint(RunId::new(), "ввод", 150),
    )
    .await;
    assert!(r.recall.text().unwrap().contains(M65_IDEA_NEW));
}

#[tokio::test]
async fn retry_skips_refs_shown_in_digest() {
    let task = |n: u8| {
        format!(
            "ZeroAIMemory: [in_progress] Синтетическая задача {n} — далее: шаг {n} — k:01M8SYNTH00000000000000K{n:02}"
        )
    };
    let fake = FakeZam::start().await;
    fake.answer_recall(
        "session_start",
        RecallAnswer::text(&format!("### Задачи\n{}\n", task(1))).warning("recall timeout"),
    );
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    let run = RunId::new();
    let r = within(WAIT, "snapshot", mem.snapshot(run, 400)).await;
    assert!(matches!(r.recall, Recall::Partial(_)), "{:?}", r.recall);
    // Повтор приносит полный дайджест: показанное в первом сообщении не повторяется (D140).
    fake.answer_recall(
        "session_start",
        RecallAnswer::text(&format!("### Задачи\n{}\n{}\n", task(1), task(2))),
    );
    let r = within(WAIT, "retry", mem.retry_digest(run, 400)).await;
    let text = r.text().unwrap().to_owned();
    assert!(text.contains("K02"), "{text}");
    assert!(!text.contains("K01"), "{text}");
    // Подсказка не повторяет ссылки эпохи.
    fake.answer_recall("prompt", RecallAnswer::text(&task(2)));
    let r = within(WAIT, "prompt", mem.prompt_hint(run, "ввод", 150)).await;
    assert_eq!(r.recall, Recall::Empty);
    // После компакции контекст потерян: пакет восстановления показывает задачу снова.
    fake.answer_recall(
        "recovery",
        RecallAnswer::text(&format!("### Задачи\n{}\n", task(1))),
    );
    let r = within(WAIT, "recovery", mem.after_compaction(run, 400)).await;
    assert!(r.text().unwrap().contains("K01"), "{r:?}");
    // Дайджест шага 0 другого run — новый контекст: показанное прежним run не снимается.
    fake.answer_recall(
        "session_start",
        RecallAnswer::text(&format!("### Задачи\n{}\n", task(1))),
    );
    let r = within(WAIT, "snapshot 2", mem.snapshot(RunId::new(), 400)).await;
    assert!(r.recall.text().unwrap().contains("K01"), "{:?}", r.recall);
}

#[tokio::test]
async fn retry_request_is_digest_with_longer_timeout() {
    let mut fake = FakeZam::start().await;
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    let run = RunId::new();
    within(WAIT, "retry", mem.retry_digest(run, 200)).await;
    let rec = last_recall(&mut fake);
    assert_eq!(rec.body["event"], "session_start");
    assert_eq!(rec.body["timeout_ms"], 2000);
    assert_eq!(rec.body["budget_tokens"], 800);
}

#[tokio::test]
async fn server_error_is_unavailable_without_level_drop() {
    let fake = FakeZam::start().await;
    let mem = port_at(&fake.url(), IdeasMode::Wait).await;
    fake.fail("/api/v1/recall", 500, "about:blank", None);
    let r = within(WAIT, "snapshot", mem.snapshot(RunId::new(), 200)).await;
    assert_eq!(r.recall, Recall::Unavailable);
    assert_eq!(mem.level(), MemoryLevel::L2);
    let rep = mem.drain_reports();
    assert_eq!(rep.last().unwrap().state, "unavailable");
    let _: Value = rep.last().unwrap().to_payload();
}

#[cfg(feature = "test-hooks")]
#[tokio::test]
async fn offline_is_unavailable_and_level_drops() {
    use std::time::Duration;

    use support::discovery_m6;
    use za_memory::{Level, ManualClock};

    let url = fake_zam::closed_port_url();
    let clock = Arc::new(ManualClock::new());
    let mem = port(
        &url,
        discovery_m6(),
        clock.clone(),
        SecretMasker::default(),
        IdeasMode::Wait,
    );
    let mut level = mem.level_watch();
    assert_eq!(*level.borrow_and_update(), Level::L2);
    let run = RunId::new();
    let r = within(WAIT, "snapshot", mem.snapshot(run, 200)).await;
    assert_eq!(r.recall, Recall::Unavailable);
    assert!(level.has_changed().unwrap());
    assert_eq!(*level.borrow_and_update(), Level::L0);
    assert_eq!(mem.level(), MemoryLevel::L0);
    // Сервер поднялся на том же порту; через 10 с пробы нет — запроса нет.
    let addr = url.trim_start_matches("http://").parse().unwrap();
    let mut fake = FakeZam::start_at(addr).await;
    clock.advance(Duration::from_secs(10));
    let r = within(WAIT, "prompt 10 s", mem.prompt_hint(run, "ввод", 100)).await;
    assert_eq!(r.recall, Recall::Unavailable);
    assert!(fake.seen().is_empty(), "проба раньше 30 с");
    // Через 30 с — проба `/api/v1/health`, уровень вернулся, запрос пошёл.
    clock.advance(Duration::from_secs(20));
    let r = within(WAIT, "prompt 30 s", mem.prompt_hint(run, "ввод", 100)).await;
    assert_eq!(r.recall, Recall::Empty);
    let routes: Vec<String> = fake.seen().iter().map(|s| s.route().to_owned()).collect();
    assert_eq!(
        routes,
        ["/api/v1/health", "/api/v1/sessions", "/api/v1/recall"]
    );
    assert_eq!(*level.borrow_and_update(), Level::L2);
    assert_eq!(mem.level(), MemoryLevel::L2);
}
