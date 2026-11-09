//! Повторы стрима, предохранитель эндпоинта, запрет идентичного повтора после ошибки
//! разбора (задача 10, D19–D22, D44). Время — `tokio::time::pause`, сокетов нет.

use std::cell::Cell;
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::Instant;
use za_llm::resilience::{
    BreakerOptions, BreakerState, CircuitBreaker, ParseGuard, RetryDecision, RetryPolicy,
    drive_with_retry, jitter_unit, request_fingerprint,
};
use za_llm::{
    ChatRequest, EndpointId, ErrorClass, FinishReason, Message, ModelError, ModelStream, Part,
    Role, StreamEvent, ToolCall, ToolSchema, Usage,
};
use za_protocol::RunId;

type Ev = Result<StreamEvent, ModelError>;

fn err(class: ErrorClass) -> ModelError {
    ModelError::new(class, "тест")
}

fn rate_limited(after: Duration) -> ModelError {
    ModelError {
        retry_after: Some(after),
        ..err(ErrorClass::RateLimited)
    }
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

#[test]
fn backoff_sequence_without_jitter() {
    let p = RetryPolicy::default();
    let e = err(ErrorClass::Server);
    for (n, want) in [1, 2, 4, 8, 16].into_iter().enumerate() {
        assert_eq!(
            p.decide(n as u32, &e, 0.5),
            RetryDecision::Retry(secs(want)),
            "retries_done {n}"
        );
    }
    assert_eq!(p.decide(5, &e, 0.5), RetryDecision::GiveUp);
}

#[test]
fn backoff_is_capped_and_jitter_bounded() {
    let p = RetryPolicy {
        max_retries: 20,
        ..RetryPolicy::default()
    };
    let e = err(ErrorClass::Timeout);
    // 2^5 = 32 с > потолка 30 с.
    assert_eq!(p.decide(5, &e, 0.5), RetryDecision::Retry(secs(30)));
    // Огромная степень не переполняется.
    assert_eq!(p.decide(19, &e, 0.5), RetryDecision::Retry(secs(30)));
    // Джиттер ±20 %: 0.0 → 0.8 × 1 с, около 1.0 → почти 1.2 × 1 с.
    assert_eq!(
        p.decide(0, &e, 0.0),
        RetryDecision::Retry(Duration::from_millis(800))
    );
    match p.decide(0, &e, 0.999_999) {
        RetryDecision::Retry(d) => assert!(d <= Duration::from_millis(1200) && d > secs(1)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn retry_after_respected_and_capped() {
    let p = RetryPolicy::default();
    assert_eq!(
        p.decide(0, &rate_limited(secs(7)), 0.5),
        RetryDecision::Retry(secs(7))
    );
    // Пауза провайдера не подвергается джиттеру.
    assert_eq!(
        p.decide(0, &rate_limited(secs(7)), 0.0),
        RetryDecision::Retry(secs(7))
    );
    assert_eq!(
        p.decide(0, &rate_limited(secs(300)), 0.5),
        RetryDecision::GiveUp
    );
    // Лимит повторов действует и для `Retry-After`.
    assert_eq!(
        p.decide(5, &rate_limited(secs(7)), 0.5),
        RetryDecision::GiveUp
    );
}

#[test]
fn non_retryable_gives_up() {
    let p = RetryPolicy::default();
    for c in [
        ErrorClass::Auth,
        ErrorClass::BadRequest,
        ErrorClass::ContextOverflow,
        ErrorClass::Malformed,
        ErrorClass::Cancelled,
        ErrorClass::RepeatedAfterParseError,
    ] {
        assert_eq!(p.decide(0, &err(c), 0.5), RetryDecision::GiveUp, "{c:?}");
    }
}

#[test]
fn jitter_is_deterministic_and_bounded() {
    let a = jitter_unit("fp", 1);
    assert_eq!(a, jitter_unit("fp", 1));
    assert_ne!(a, jitter_unit("fp", 2));
    assert_ne!(a, jitter_unit("fq", 1));
    for i in 0..10_000 {
        let u = jitter_unit("seed", i);
        assert!((0.0..1.0).contains(&u), "{u}");
    }
}

/// Канал попытки, заполненный заранее; отправитель закрыт.
fn attempt_rx(events: Vec<Ev>) -> mpsc::Receiver<Ev> {
    let (tx, rx) = mpsc::channel(events.len().max(1));
    for e in events {
        tx.try_send(e).unwrap();
    }
    rx
}

fn tool_finished(args: serde_json::Value) -> StreamEvent {
    StreamEvent::Finished {
        message: Message {
            role: Role::Assistant,
            parts: vec![Part::ToolCall(ToolCall {
                id: "c1".into(),
                name: "read".into(),
                raw_arguments: Some(args.to_string()),
                arguments: args,
            })],
        },
        finish: FinishReason::ToolCalls,
        usage: Usage::default(),
    }
}

fn tool_start() -> StreamEvent {
    StreamEvent::ToolCallStart {
        index: 0,
        id: "c1".into(),
        name: "read".into(),
    }
}

fn tool_args(f: &str) -> StreamEvent {
    StreamEvent::ToolCallArgs {
        index: 0,
        fragment: f.into(),
    }
}

/// Все события из выходного канала (канал закрыт после `drive_with_retry`).
fn drain(rx: &mut mpsc::Receiver<Ev>) -> Vec<Ev> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

#[tokio::test(start_paused = true)]
async fn cut_after_deltas_emits_restart_and_clean_finish() {
    let (tx, mut rx) = mpsc::channel(64);
    let res = drive_with_retry(
        &RetryPolicy::default(),
        "fp",
        &tx,
        |attempt| async move {
            Ok(match attempt {
                1 => attempt_rx(vec![
                    Ok(tool_start()),
                    Ok(tool_args("{\"pa")),
                    Err(err(ErrorClass::StreamCut)),
                ]),
                _ => attempt_rx(vec![
                    Ok(tool_start()),
                    Ok(tool_args("{\"path\":")),
                    Ok(tool_args("\"a.txt\"}")),
                    Ok(tool_finished(json!({"path": "a.txt"}))),
                ]),
            })
        },
        |_| {},
    )
    .await;
    assert_eq!(res, Ok(()));
    drop(tx);
    let events = drain(&mut rx);
    assert_eq!(events[0], Ok(tool_start()));
    assert_eq!(events[1], Ok(tool_args("{\"pa")));
    match &events[2] {
        Ok(StreamEvent::Restart { attempt: 2, reason }) => {
            assert!(reason.contains("StreamCut"), "{reason}")
        }
        other => panic!("ожидался Restart, получено {other:?}"),
    }
    assert!(matches!(
        events.last(),
        Some(Ok(StreamEvent::Finished { .. }))
    ));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Ok(StreamEvent::Restart { .. })))
            .count(),
        1
    );

    // Потребитель собирает итог только из последней попытки.
    let (tx2, rx2) = mpsc::channel(events.len());
    for e in events {
        tx2.try_send(e).unwrap();
    }
    drop(tx2);
    let resp = ModelStream::from_channel(rx2, Box::new(()))
        .collect()
        .await
        .unwrap();
    let calls: Vec<&ToolCall> = resp
        .message
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::ToolCall(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].arguments, json!({"path": "a.txt"}));
}

#[tokio::test(start_paused = true)]
async fn failed_open_after_restart_does_not_repeat_restart() {
    // Попытка 1 отдала дельту и оборвалась, попытка 2 не открылась, попытка 3 успешна:
    // один Restart (перед попыткой 2), перед попыткой 3 нового нет.
    let (tx, mut rx) = mpsc::channel(64);
    let res = drive_with_retry(
        &RetryPolicy::default(),
        "fp",
        &tx,
        |attempt| async move {
            match attempt {
                1 => Ok(attempt_rx(vec![Ok(StreamEvent::TextDelta("об".into()))])),
                2 => Err(err(ErrorClass::Transport)),
                _ => Ok(attempt_rx(vec![Ok(tool_finished(json!({})))])),
            }
        },
        |_| {},
    )
    .await;
    assert_eq!(res, Ok(()));
    drop(tx);
    let events = drain(&mut rx);
    let restarts: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Ok(StreamEvent::Restart { attempt, .. }) => Some(*attempt),
            _ => None,
        })
        .collect();
    assert_eq!(restarts, vec![2]);
}

#[tokio::test(start_paused = true)]
async fn five_retries_then_error() {
    let (tx, mut rx) = mpsc::channel(64);
    let calls = Cell::new(Vec::new());
    let t0 = Instant::now();
    let res = drive_with_retry(
        &RetryPolicy::default(),
        "fp",
        &tx,
        |attempt| {
            let mut v = calls.take();
            v.push(attempt);
            calls.set(v);
            async { Err(err(ErrorClass::StreamCut)) }
        },
        |_| {},
    )
    .await;
    assert_eq!(calls.take(), vec![1, 2, 3, 4, 5, 6]);
    let e = res.unwrap_err();
    assert_eq!(e.class, ErrorClass::StreamCut);
    // Паузы 1 + 2 + 4 + 8 + 16 с ± 20 %.
    let waited = t0.elapsed();
    assert!(
        waited >= Duration::from_secs_f32(31.0 * 0.8)
            && waited <= Duration::from_secs_f32(31.0 * 1.2),
        "{waited:?}"
    );
    drop(tx);
    let events = drain(&mut rx);
    assert_eq!(events.last(), Some(&Err(e)));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Ok(StreamEvent::Restart { .. }))),
        "попытки ничего не отдали — Restart не нужен"
    );
}

#[tokio::test(start_paused = true)]
async fn on_attempt_error_sees_every_failure() {
    let (tx, mut rx) = mpsc::channel(64);
    let seen = Cell::new(0u32);
    let t0 = Instant::now();
    let res = drive_with_retry(
        &RetryPolicy::default(),
        "fp",
        &tx,
        |attempt| async move {
            if attempt <= 2 {
                Err(rate_limited(secs(1)))
            } else {
                Ok(attempt_rx(vec![Ok(tool_finished(json!({})))]))
            }
        },
        |e| {
            assert_eq!(e.class, ErrorClass::RateLimited);
            seen.set(seen.get() + 1);
        },
    )
    .await;
    assert_eq!(res, Ok(()));
    assert_eq!(seen.get(), 2);
    // Дважды ровно по `Retry-After`.
    assert_eq!(t0.elapsed(), secs(2));
    drop(tx);
    assert!(matches!(
        drain(&mut rx).last(),
        Some(Ok(StreamEvent::Finished { .. }))
    ));
}

#[tokio::test(start_paused = true)]
async fn non_retryable_error_is_final_at_once() {
    let (tx, mut rx) = mpsc::channel(8);
    let opens = Cell::new(0u32);
    let res = drive_with_retry(
        &RetryPolicy::default(),
        "fp",
        &tx,
        |_| {
            opens.set(opens.get() + 1);
            async { Ok(attempt_rx(vec![Err(err(ErrorClass::Malformed))])) }
        },
        |_| {},
    )
    .await;
    assert_eq!(opens.get(), 1);
    assert_eq!(res.as_ref().unwrap_err().class, ErrorClass::Malformed);
    drop(tx);
    assert_eq!(drain(&mut rx), vec![Err(res.unwrap_err())]);
}

#[tokio::test(start_paused = true)]
async fn closed_attempt_channel_without_finished_is_retried() {
    let (tx, _rx) = mpsc::channel(8);
    let opens = Cell::new(0u32);
    let res = drive_with_retry(
        &RetryPolicy::default(),
        "fp",
        &tx,
        |attempt| {
            opens.set(attempt);
            async move {
                Ok(if attempt == 1 {
                    attempt_rx(vec![])
                } else {
                    attempt_rx(vec![Ok(tool_finished(json!({})))])
                })
            }
        },
        |e| assert_eq!(e.class, ErrorClass::StreamCut),
    )
    .await;
    assert_eq!(res, Ok(()));
    assert_eq!(opens.get(), 2);
}

#[tokio::test(start_paused = true)]
async fn consumer_drop_stops_retries() {
    let (tx, rx) = mpsc::channel::<Ev>(8);
    drop(rx);
    let opens = Cell::new(0u32);
    let res = drive_with_retry(
        &RetryPolicy::default(),
        "fp",
        &tx,
        |_| {
            opens.set(opens.get() + 1);
            async { Err(err(ErrorClass::Server)) }
        },
        |_| {},
    )
    .await;
    assert_eq!(res.unwrap_err().class, ErrorClass::Cancelled);
    assert!(opens.get() <= 1, "после ухода потребителя повторов нет");
}

#[test]
fn breaker_opens_after_five_and_half_opens() {
    let mut b = CircuitBreaker::new(BreakerOptions::default());
    let t0 = Instant::now();
    for i in 0..4 {
        b.on_failure(&err(ErrorClass::Server), t0);
        assert_eq!(b.state(), BreakerState::Closed, "после {} отказов", i + 1);
    }
    b.on_failure(&err(ErrorClass::Server), t0);
    assert_eq!(
        b.state(),
        BreakerState::Open {
            until: t0 + secs(30)
        }
    );
    let e = b.allow(t0 + secs(29)).unwrap_err();
    assert_eq!(e.class, ErrorClass::CircuitOpen);
    assert_eq!(e.retry_after, Some(secs(1)));

    assert_eq!(b.allow(t0 + secs(30)), Ok(()));
    assert_eq!(b.state(), BreakerState::HalfOpen);
    // Пробная заявка одна: вторая отвергается, пока проба не закончилась.
    assert_eq!(
        b.allow(t0 + secs(30)).unwrap_err().class,
        ErrorClass::CircuitOpen
    );

    let t1 = t0 + secs(31);
    b.on_failure(&err(ErrorClass::Timeout), t1);
    assert_eq!(
        b.state(),
        BreakerState::Open {
            until: t1 + secs(60)
        }
    );
    assert_eq!(
        b.allow(t1 + secs(59)).unwrap_err().class,
        ErrorClass::CircuitOpen
    );
    assert_eq!(b.allow(t1 + secs(60)), Ok(()));
    b.on_success();
    assert_eq!(b.state(), BreakerState::Closed);
    assert_eq!(b.allow(t1 + secs(60)), Ok(()));

    // После успеха пауза снова базовая.
    let t2 = t1 + secs(100);
    for _ in 0..5 {
        b.on_failure(&err(ErrorClass::Overloaded), t2);
    }
    assert_eq!(
        b.state(),
        BreakerState::Open {
            until: t2 + secs(30)
        }
    );
}

#[test]
fn breaker_cooldown_doubles_up_to_cap() {
    let mut b = CircuitBreaker::new(BreakerOptions::default());
    let mut t = Instant::now();
    for _ in 0..5 {
        b.on_failure(&err(ErrorClass::Transport), t);
    }
    let mut expected = [60, 120, 240, 300, 300].into_iter();
    loop {
        let BreakerState::Open { until } = b.state() else {
            panic!("{:?}", b.state())
        };
        t = until;
        assert_eq!(b.allow(t), Ok(()));
        b.on_failure(&err(ErrorClass::StreamCut), t);
        let Some(want) = expected.next() else { break };
        assert_eq!(
            b.state(),
            BreakerState::Open {
                until: t + secs(want)
            }
        );
    }
}

#[test]
fn success_resets_failure_streak() {
    let mut b = CircuitBreaker::new(BreakerOptions::default());
    let t = Instant::now();
    for _ in 0..4 {
        b.on_failure(&err(ErrorClass::Server), t);
    }
    b.on_success();
    for _ in 0..4 {
        b.on_failure(&err(ErrorClass::Server), t);
    }
    assert_eq!(b.state(), BreakerState::Closed);
}

#[test]
fn half_open_probe_released_by_neutral_outcome() {
    // Проба, закончившаяся классом вне предохранителя (отмена, 429), не держит
    // эндпоинт закрытым: следующая заявка снова получает пробу.
    let mut b = CircuitBreaker::new(BreakerOptions::default());
    let t = Instant::now();
    for _ in 0..5 {
        b.on_failure(&err(ErrorClass::Server), t);
    }
    assert_eq!(b.allow(t + secs(30)), Ok(()));
    b.on_failure(&err(ErrorClass::Cancelled), t + secs(31));
    assert_eq!(b.state(), BreakerState::HalfOpen);
    assert_eq!(b.allow(t + secs(31)), Ok(()));
}

#[test]
fn rate_limited_does_not_trip_breaker() {
    let mut b = CircuitBreaker::new(BreakerOptions::default());
    let t = Instant::now();
    for _ in 0..10 {
        b.on_failure(&rate_limited(secs(1)), t);
    }
    assert_eq!(b.state(), BreakerState::Closed);
    assert_eq!(b.allow(t), Ok(()));
}

#[test]
fn auth_disables_until_reset() {
    let mut b = CircuitBreaker::new(BreakerOptions::default());
    let t = Instant::now();
    b.on_failure(&ModelError::new(ErrorClass::Auth, "ключ отвергнут"), t);
    assert!(
        matches!(b.state(), BreakerState::Disabled { ref reason } if reason.contains("ключ отвергнут")),
        "{:?}",
        b.state()
    );
    assert_eq!(b.allow(t + secs(3600)).unwrap_err().class, ErrorClass::Auth);
    b.on_success();
    assert!(matches!(b.state(), BreakerState::Disabled { .. }));
    b.reset();
    assert_eq!(b.state(), BreakerState::Closed);
    assert_eq!(b.allow(t + secs(3600)), Ok(()));
}

#[test]
fn identical_request_after_parse_error_is_refused() {
    let mut g = ParseGuard::new();
    let run = RunId::new();
    let other_run = RunId::new();
    g.record(run, "fp1".into());
    assert_eq!(
        g.check(run, "fp1").unwrap_err().class,
        ErrorClass::RepeatedAfterParseError
    );
    assert_eq!(g.check(run, "fp2"), Ok(()));
    assert_eq!(g.check(other_run, "fp1"), Ok(()));
    g.clear(run);
    assert_eq!(g.check(run, "fp1"), Ok(()));
}

fn tool(name: &str) -> ToolSchema {
    ToolSchema {
        name: name.into(),
        description: format!("инструмент {name}"),
        parameters: json!({"type": "object"}),
    }
}

#[test]
fn fingerprint_ignores_tool_order_but_not_temperature() {
    let ep = EndpointId("local".into());
    let base = ChatRequest {
        system: "s".into(),
        tools: vec![tool("a"), tool("b")],
        messages: vec![Message {
            role: Role::User,
            parts: vec![Part::Text("привет".into())],
        }],
        ..ChatRequest::default()
    };
    let swapped = ChatRequest {
        tools: vec![tool("b"), tool("a")],
        ..base.clone()
    };
    let fp = request_fingerprint(&ep, &base);
    assert_eq!(fp, request_fingerprint(&ep, &base));
    assert_eq!(fp, request_fingerprint(&ep, &swapped));

    let mut t02 = base.clone();
    t02.sampling.temperature = Some(0.2);
    let mut t03 = base.clone();
    t03.sampling.temperature = Some(0.3);
    assert_ne!(
        request_fingerprint(&ep, &t02),
        request_fingerprint(&ep, &t03)
    );
    assert_ne!(fp, request_fingerprint(&ep, &t02));

    assert_ne!(fp, request_fingerprint(&EndpointId("cloud".into()), &base));

    let mut longer = base.clone();
    longer.messages.push(Message {
        role: Role::User,
        parts: vec![Part::Text("ещё".into())],
    });
    assert_ne!(fp, request_fingerprint(&ep, &longer));
}
