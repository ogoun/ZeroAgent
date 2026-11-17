//! Порт памяти `ZamMemory` (R1-M D140, D141, D142, D168, D169; П3, Δ11 изменений по ZAM
//! 0.0.12-m6.5): запросы `/recall` без `output_limit`, итоги `Recall`, укладка в бюджет,
//! подсказки с маской ввода, признак режима идей, уровень и повторная проба — на поддельном
//! ZAM (`test-support/fake_zam.rs`, D154), только `127.0.0.1`.

#[path = "../../../test-support/fake_zam.rs"]
mod fake_zam;
mod support;

use std::sync::Arc;

use fake_zam::{
    FakeZam, Health, M65_IDEA_NEW, M65_TASK, RecallAnswer, WAIT, digest_m65_review,
    prompt_m65_review, recall_item, within,
};
use serde_json::{Value, json};
use za_core::memory::{IdeasSignal, MemoryLevel, MemoryPort, Recall};
use za_memory::fit::{OMITTED_HEAD, approx_tokens};
use za_memory::{TokioClock, session_id};
use za_protocol::RunId;
use za_secrets::{MASK_PREFIX, MaskCell};

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
    let mem = port_at(&fake.url()).await;
    fake.seen();
    let run = RunId::new();
    let r = within(WAIT, "snapshot", mem.snapshot(run, 200, None)).await;
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
    let mem = port_at(&fake.url()).await;
    let r = within(WAIT, "snapshot", mem.snapshot(RunId::new(), 200, None)).await;
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
    let mem = port_at(&fake.url()).await;
    let r = within(WAIT, "snapshot", mem.snapshot(RunId::new(), 200, None)).await;
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
    let mem = port_at(&fake.url()).await;
    let r = within(WAIT, "snapshot", mem.snapshot(RunId::new(), 200, None)).await;
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
    let mem = port_at(&fake.url()).await;
    let run = RunId::new();
    within(WAIT, "snapshot", mem.snapshot(run, 200, None)).await;
    let reports = mem.drain_reports();
    assert_eq!(reports[0].event, "session_start");
    assert_eq!(reports[0].hooks_off, vec!["prompt".to_owned()]);
    assert_eq!(reports[0].to_payload()["hooks_off"], json!(["prompt"]));
    // Подсказка выключенного типа — пустой текст; событие называет причину.
    let r = within(WAIT, "prompt", mem.prompt_hint(run, "ввод", 100, None)).await;
    assert_eq!(r.recall, Recall::Empty);
    let reports = mem.drain_reports();
    assert_eq!((reports[0].event, reports[0].state), ("prompt", "empty"));
    assert_eq!(reports[0].hooks_off, vec!["prompt".to_owned()]);
}

#[tokio::test]
async fn no_recall_timeout_flag_omits_field() {
    let mut fake = FakeZam::start().await;
    fake.set_health(Health::NoFeatures);
    let mem = port_at(&fake.url()).await;
    assert_eq!(mem.level(), MemoryLevel::L1);
    within(WAIT, "snapshot", mem.snapshot(RunId::new(), 200, None)).await;
    let rec = last_recall(&mut fake);
    assert_eq!(rec.body["event"], "session_start");
    assert!(rec.body.get("timeout_ms").is_none(), "{}", rec.body);
    assert_eq!(rec.body["budget_tokens"], 800);
}

#[tokio::test]
async fn recovery_request_shape() {
    let mut fake = FakeZam::start().await;
    let mem = port_at(&fake.url()).await;
    let run = RunId::new();
    within(WAIT, "recovery", mem.after_compaction(run, 400, None)).await;
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
    let masker = Arc::new(MaskCell::new(vec![("TEST_KEY".to_owned(), secret())]));
    let client =
        za_memory::ZamClient::new(&za_memory::MemoryConfig::new(&fake.url()), None).unwrap();
    let d = za_memory::discover(&client).await;
    let mem = port(&fake.url(), d, Arc::new(TokioClock), masker);
    let prompt = format!("{} {} {}", "a".repeat(100), secret(), "б".repeat(1900));
    assert!(prompt.chars().count() > 2000);
    let run = RunId::new();
    within(WAIT, "prompt", mem.prompt_hint(run, &prompt, 100, None)).await;
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

/// Minor 3 ревью задачи 4: секрет пересекает 512-й символ ввода. Маска раньше обрезки — в
/// поле `prompt` нет и головы секрета; при обратном порядке ушла бы она (тело `sk-` короче
/// 20 символов шаблон не ловит, известное значение целиком не совпадает).
#[tokio::test]
async fn prompt_mask_before_cut_at_limit() {
    let mut fake = FakeZam::start().await;
    let masker = Arc::new(MaskCell::new(vec![("TEST_KEY".to_owned(), secret())]));
    let client =
        za_memory::ZamClient::new(&za_memory::MemoryConfig::new(&fake.url()), None).unwrap();
    let d = za_memory::discover(&client).await;
    let mem = port(&fake.url(), d, Arc::new(TokioClock), masker);
    let prompt = format!("{} {}{}", "a".repeat(499), secret(), " хвост".repeat(40));
    let head: String = prompt.chars().take(512).collect();
    assert!(
        head.contains("sk-synth") && !head.contains(&secret()),
        "{head}"
    );
    within(
        WAIT,
        "prompt",
        mem.prompt_hint(RunId::new(), &prompt, 100, None),
    )
    .await;
    let rec = last_recall(&mut fake);
    let sent = rec.body["prompt"].as_str().unwrap();
    assert_eq!(sent.chars().count(), 512, "{sent}");
    assert!(sent.starts_with(&"a".repeat(499)), "{sent}");
    assert!(!sent.contains("sk-"), "{sent}");
}

/// D160: порт берёт маску процесса из ячейки при каждом запросе — значение, раскрытое после
/// сборки порта (ключ из Vault), в ввод подсказки уже не уходит.
#[tokio::test]
async fn prompt_hint_sees_values_revealed_after_build() {
    let mut fake = FakeZam::start().await;
    let cell = Arc::new(MaskCell::default());
    let client =
        za_memory::ZamClient::new(&za_memory::MemoryConfig::new(&fake.url()), None).unwrap();
    let d = za_memory::discover(&client).await;
    let mem = port(&fake.url(), d, Arc::new(TokioClock), cell.clone());
    let late = ["late", "-revealed-", "value-77"].concat();
    cell.extend(vec![("vault-key".to_owned(), late.clone())]);
    within(
        WAIT,
        "prompt",
        mem.prompt_hint(RunId::new(), &format!("ключ {late} в тексте"), 100, None),
    )
    .await;
    let rec = last_recall(&mut fake);
    let sent = rec.body["prompt"].as_str().unwrap();
    assert!(!rec.body.to_string().contains(&late), "{sent}");
    assert!(sent.contains("[secret masked: vault-key]"), "{sent}");
}

#[tokio::test]
async fn tool_error_hint_masks_and_truncates() {
    let mut fake = FakeZam::start().await;
    let masker = Arc::new(MaskCell::new(vec![("TEST_KEY".to_owned(), secret())]));
    let client =
        za_memory::ZamClient::new(&za_memory::MemoryConfig::new(&fake.url()), None).unwrap();
    let d = za_memory::discover(&client).await;
    let mem = port(&fake.url(), d, Arc::new(TokioClock), masker);
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
    let mem = port_at(&fake.url()).await;
    fake.seen();
    let run = RunId::new();
    let r = within(WAIT, "prompt", mem.prompt_hint(run, "ввод", 0, None)).await;
    assert_eq!(r.recall, Recall::Capped);
    let r = within(WAIT, "retry", mem.retry_digest(run, 0, None)).await;
    assert_eq!(r.recall, Recall::Capped);
    assert!(fake.seen().is_empty());
}

#[tokio::test]
async fn recall_http_timeout_is_timeout() {
    let mut fake = FakeZam::start().await;
    let mem = port_at(&fake.url()).await;
    let run = RunId::new();
    within(WAIT, "snapshot", mem.snapshot(run, 200, None)).await;
    fake.seen();
    // Ответ задержан воротами: клиент упирается в свой срок (`timeout_ms` + 1 с, D137).
    fake.hold_recall();
    let r = within(WAIT, "prompt", mem.prompt_hint(run, "ввод", 100, None)).await;
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
    let mem = port_at(&fake.url()).await;
    let run = RunId::new();
    let r = within(WAIT, "snapshot", mem.snapshot(run, 1500, None)).await;
    assert_eq!(r.ideas, Some(IdeasSignal::Review));
    // D169, I3 ревью задачи 4: режим укладки — по признаку того же ответа (флаг `idea_review`
    // и `items[].reason: idea_review`), а не по прежнему режиму run: раздел разбора и строка
    // плана остаются.
    let text = r.recall.text().unwrap();
    assert!(text.contains("далее: write test"), "{text}");
    for kept in ["### Идеи на разбор", "План из идей", M65_IDEA_NEW] {
        assert!(text.contains(kept), "{kept}:\n{text}");
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
    let r = within(WAIT, "prompt", mem.prompt_hint(run, "ввод", 100, None)).await;
    assert_eq!(r.ideas, None);
    assert!(r.recall.text().unwrap().contains("W07"));
}

/// D169, I3: ответ без элементов `reason: idea_review` укладывается по признаку run, который
/// передал драйвер (`MemoryState.ideas`). Строка плана приходит и при пустой очереди разбора
/// (Δ10): с признаком `review` она остаётся, без признака и с `propose` — снята.
#[tokio::test]
async fn review_signal_of_run_keeps_plan_line() {
    let fake = FakeZam::start().await;
    fake.set_health(Health::M65);
    let plan = "План из идей: 1 открытых";
    let text = format!(
        "### Задачи\nZeroAIMemory: [in_progress] Синтетическая задача — далее: write test — {M65_TASK}\n{plan}\n"
    );
    let answer = RecallAnswer::text(&text).items(vec![recall_item(M65_TASK, "task", "task")]);
    fake.answer_recall("session_start", answer.clone());
    fake.answer_recall("recovery", answer);
    let mem = port_at(&fake.url()).await;
    let run = RunId::new();
    for (ideas, kept) in [
        (Some(IdeasSignal::Review), true),
        (None, false),
        (Some(IdeasSignal::Propose), false),
    ] {
        let r = within(WAIT, "snapshot", mem.snapshot(run, 1500, ideas)).await;
        // Признака в ответе нет: он не выдумывается из признака run.
        assert_eq!(r.ideas, None);
        let t = r.recall.text().unwrap();
        assert_eq!(t.contains(plan), kept, "{ideas:?}:\n{t}");
        // `recovery` при переданном признаке `review` сохраняет строку плана.
        let r = within(WAIT, "recovery", mem.after_compaction(run, 400, ideas)).await;
        let t = r.text().unwrap();
        assert_eq!(t.contains(plan), kept, "recovery {ideas:?}:\n{t}");
        assert!(t.contains(M65_TASK), "{t}");
    }
}

/// D169: без флага `idea_review` у сервера (форма `0.0.11-m6`) — всегда `Wait`: и при
/// элементах `reason: idea_review` в том же ответе, и при признаке run `review`.
#[tokio::test]
async fn review_mode_needs_server_flag() {
    let fake = FakeZam::start().await;
    fake.answer_recall("session_start", digest_m65_review());
    let mem = port_at(&fake.url()).await;
    for ideas in [None, Some(IdeasSignal::Review)] {
        let r = within(WAIT, "snapshot", mem.snapshot(RunId::new(), 1500, ideas)).await;
        let text = r.recall.text().unwrap();
        for gone in ["### Идеи на разбор", "План из идей", M65_IDEA_NEW] {
            assert!(!text.contains(gone), "{ideas:?} {gone}:\n{text}");
        }
        assert!(text.contains(M65_TASK), "{text}");
    }
}

/// Δ11, D169: подсказка разбора к `prompt` (`reason: idea_review`). С флагом `idea_review` тот
/// же ответ даёт режим `Review` — подсказка остаётся; без флага (`Wait`) она снята целиком:
/// велит звать инструмент, которого у модели в `Wait` нет. Признак режима — в обоих случаях.
#[tokio::test]
async fn idea_review_prompt_hint_by_server_flag() {
    let fake = FakeZam::start().await;
    fake.set_health(Health::M65);
    fake.answer_recall("prompt", prompt_m65_review());
    let mem = port_at(&fake.url()).await;
    let r = within(
        WAIT,
        "prompt review",
        mem.prompt_hint(RunId::new(), "ввод", 150, None),
    )
    .await;
    assert_eq!(r.ideas, Some(IdeasSignal::Review));
    assert!(r.recall.text().unwrap().contains(M65_IDEA_NEW));
    fake.set_health(Health::M6);
    let wait = port_at(&fake.url()).await;
    let r = within(
        WAIT,
        "prompt wait",
        wait.prompt_hint(RunId::new(), "ввод", 150, Some(IdeasSignal::Review)),
    )
    .await;
    assert_eq!(r.ideas, Some(IdeasSignal::Review));
    let text = r.recall.text().unwrap();
    assert!(!text.contains(M65_IDEA_NEW), "{text}");
    assert!(!text.contains("memory_recall"), "{text}");
    assert!(text.contains("синтетическая ловушка"), "{text}");
}

/// Minor 6 ревью задачи 4: повтор дайджеста — тот же `session_start`, его признак режима идей
/// отдаётся (`RecallReply`), а укладка — по признаку того же ответа.
#[tokio::test]
async fn retry_digest_gives_review_signal() {
    let fake = FakeZam::start().await;
    fake.set_health(Health::M65);
    fake.answer_recall("session_start", digest_m65_review());
    let mem = port_at(&fake.url()).await;
    let r = within(WAIT, "retry", mem.retry_digest(RunId::new(), 1500, None)).await;
    assert_eq!(r.ideas, Some(IdeasSignal::Review));
    let text = r.recall.text().unwrap();
    for kept in ["### Идеи на разбор", "План из идей", M65_IDEA_NEW] {
        assert!(text.contains(kept), "{kept}:\n{text}");
    }
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
    let mem = port_at(&fake.url()).await;
    let run = RunId::new();
    let r = within(WAIT, "snapshot", mem.snapshot(run, 400, None)).await;
    assert!(matches!(r.recall, Recall::Partial(_)), "{:?}", r.recall);
    // Повтор приносит полный дайджест: показанное в первом сообщении не повторяется (D140).
    fake.answer_recall(
        "session_start",
        RecallAnswer::text(&format!("### Задачи\n{}\n{}\n", task(1), task(2))),
    );
    let r = within(WAIT, "retry", mem.retry_digest(run, 400, None)).await;
    let text = r.recall.text().unwrap().to_owned();
    assert!(text.contains("K02"), "{text}");
    assert!(!text.contains("K01"), "{text}");
    // Подсказка не повторяет ссылки эпохи.
    fake.answer_recall("prompt", RecallAnswer::text(&task(2)));
    let r = within(WAIT, "prompt", mem.prompt_hint(run, "ввод", 150, None)).await;
    assert_eq!(r.recall, Recall::Empty);
    // После компакции контекст потерян: пакет восстановления показывает задачу снова.
    fake.answer_recall(
        "recovery",
        RecallAnswer::text(&format!("### Задачи\n{}\n", task(1))),
    );
    let r = within(WAIT, "recovery", mem.after_compaction(run, 400, None)).await;
    assert!(r.text().unwrap().contains("K01"), "{r:?}");
    // Дайджест шага 0 другого run — новый контекст: показанное прежним run не снимается.
    fake.answer_recall(
        "session_start",
        RecallAnswer::text(&format!("### Задачи\n{}\n", task(1))),
    );
    let r = within(WAIT, "snapshot 2", mem.snapshot(RunId::new(), 400, None)).await;
    assert!(r.recall.text().unwrap().contains("K01"), "{:?}", r.recall);
}

#[tokio::test]
async fn retry_request_is_digest_with_longer_timeout() {
    let mut fake = FakeZam::start().await;
    let mem = port_at(&fake.url()).await;
    let run = RunId::new();
    within(WAIT, "retry", mem.retry_digest(run, 200, None)).await;
    let rec = last_recall(&mut fake);
    assert_eq!(rec.body["event"], "session_start");
    assert_eq!(rec.body["timeout_ms"], 2000);
    assert_eq!(rec.body["budget_tokens"], 800);
}

#[tokio::test]
async fn server_error_is_unavailable_without_level_drop() {
    let fake = FakeZam::start().await;
    let mem = port_at(&fake.url()).await;
    fake.fail("/api/v1/recall", 500, "about:blank", None);
    let r = within(WAIT, "snapshot", mem.snapshot(RunId::new(), 200, None)).await;
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

    // Сервер «недоступен»: порт за ним, соединение закрывается до чтения запроса (Minor 9 ревью
    // задачи 4 — без гонки за освобождённый порт).
    let mut fake = FakeZam::start().await;
    fake.refuse_connections(true);
    let clock = Arc::new(ManualClock::new());
    let mem = port(
        &fake.url(),
        discovery_m6(),
        clock.clone(),
        Arc::new(MaskCell::default()),
    );
    let mut level = mem.level_watch();
    assert_eq!(*level.borrow_and_update(), Level::L2);
    let run = RunId::new();
    let r = within(WAIT, "snapshot", mem.snapshot(run, 200, None)).await;
    assert_eq!(r.recall, Recall::Unavailable);
    assert!(level.has_changed().unwrap());
    assert_eq!(*level.borrow_and_update(), Level::L0);
    assert_eq!(mem.level(), MemoryLevel::L0);
    assert!(
        fake.seen().is_empty(),
        "запрос дошёл до «недоступного» сервера"
    );
    // Сервер поднялся; через 10 с пробы нет — запроса нет.
    fake.refuse_connections(false);
    clock.advance(Duration::from_secs(10));
    let r = within(WAIT, "prompt 10 s", mem.prompt_hint(run, "ввод", 100, None)).await;
    assert_eq!(r.recall, Recall::Unavailable);
    assert!(fake.seen().is_empty(), "проба раньше 30 с");
    // Через 30 с — проба `/api/v1/health`, уровень вернулся, запрос пошёл.
    clock.advance(Duration::from_secs(20));
    let r = within(WAIT, "prompt 30 s", mem.prompt_hint(run, "ввод", 100, None)).await;
    assert_eq!(r.recall, Recall::Empty);
    let routes: Vec<String> = fake.seen().iter().map(|s| s.route().to_owned()).collect();
    assert_eq!(
        routes,
        ["/api/v1/health", "/api/v1/sessions", "/api/v1/recall"]
    );
    assert_eq!(*level.borrow_and_update(), Level::L2);
    assert_eq!(mem.level(), MemoryLevel::L2);
}

/// Minor 7 ревью задачи 4 («мёртвый хост»): сервер, который принимает соединение и молчит
/// (фильтр отбрасывает пакеты), даёт `Timeout`, а не `Offline`. После трёх `Timeout` подряд
/// без ответа сервера порт переходит в L0, дальше — проба не чаще раза в 30 с (`Clock`). Любой
/// ответ сервера сбрасывает счёт.
#[cfg(feature = "test-hooks")]
#[tokio::test]
async fn dead_host_after_three_timeouts_goes_l0() {
    use std::time::Duration;

    use za_memory::{Level, ManualClock};

    let mut fake = FakeZam::start().await;
    let client =
        za_memory::ZamClient::new(&za_memory::MemoryConfig::new(&fake.url()), None).unwrap();
    let d = za_memory::discover(&client).await;
    let clock = Arc::new(ManualClock::new());
    let mem = port(&fake.url(), d, clock.clone(), Arc::new(MaskCell::default()));
    let mut level = mem.level_watch();
    let run = RunId::new();
    within(WAIT, "snapshot", mem.snapshot(run, 200, None)).await;
    let hint = || mem.prompt_hint(run, "ввод", 100, None);
    // Два `Timeout` подряд — уровень прежний.
    fake.hold_recall();
    let (a, b) = within(WAIT, "2 timeouts", async { tokio::join!(hint(), hint()) }).await;
    assert_eq!((a.recall, b.recall), (Recall::Timeout, Recall::Timeout));
    assert_eq!(mem.level(), MemoryLevel::L2);
    // Ответ сервера сбрасывает счёт.
    fake.release_recall();
    assert_eq!(within(WAIT, "answer", hint()).await.recall, Recall::Empty);
    fake.hold_recall();
    let (a, b) = within(WAIT, "2 more", async { tokio::join!(hint(), hint()) }).await;
    assert_eq!((a.recall, b.recall), (Recall::Timeout, Recall::Timeout));
    assert_eq!(mem.level(), MemoryLevel::L2, "счёт не сброшен ответом");
    // Третий подряд — L0.
    assert_eq!(within(WAIT, "third", hint()).await.recall, Recall::Timeout);
    assert_eq!(mem.level(), MemoryLevel::L0);
    assert_eq!(*level.borrow_and_update(), Level::L0);
    fake.release_recall();
    fake.seen();
    // В L0 через 10 с запроса нет.
    clock.advance(Duration::from_secs(10));
    assert_eq!(
        within(WAIT, "10 s", hint()).await.recall,
        Recall::Unavailable
    );
    assert!(fake.seen().is_empty(), "проба раньше 30 с");
    // Через 30 с — проба, уровень вернулся, запрос пошёл.
    clock.advance(Duration::from_secs(20));
    assert_eq!(within(WAIT, "30 s", hint()).await.recall, Recall::Empty);
    let routes: Vec<String> = fake.seen().iter().map(|s| s.route().to_owned()).collect();
    assert_eq!(routes, ["/api/v1/health", "/api/v1/recall"]);
    assert_eq!(mem.level(), MemoryLevel::L2);
}
