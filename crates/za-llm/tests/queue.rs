//! Очередь к моделям (задача 13): приоритеты и старение (D9), пулы (RES-001), допуск по
//! ресурсам (RES-012, RES-013), cooldown, дневной бюджет облака (D14), ожидание (D12).
//! Время — на паузе (`start_paused`), сокетов нет.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use za_llm::queue::{
    DEFAULT_CLOUD_PARALLEL, GroupId, GroupLimits, ModelQueue, Permit, QueueError, Ticket,
};
use za_llm::{PoolKind, WaitState};
use za_protocol::{Priority, RunId};
use za_res::{Admission, CpuSample, GpuStatus, MemSample, ResourceSnapshot, ResourcesConfig};

const GIB: u64 = 1 << 30;

/// Метка ожидаемых паник тестов: их сообщения не печатаются.
const EXPECTED_PANIC: &str = "ожидаемая паника теста";

/// Глушит вывод только ожидаемых паник; прочие идут в прежний обработчик.
fn quiet_expected_panics() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let msg = info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str));
            if msg.is_some_and(|m| m.contains(EXPECTED_PANIC)) {
                return;
            }
            prev(info);
        }));
    });
}

fn local(n: u32) -> GroupLimits {
    GroupLimits {
        kind: PoolKind::Local,
        max_parallel: n,
        resource_gated: false,
        daily_budget_usd: None,
    }
}

fn cloud(n: u32, budget: Option<f64>) -> GroupLimits {
    GroupLimits {
        kind: PoolKind::Cloud,
        max_parallel: n,
        resource_gated: false,
        daily_budget_usd: budget,
    }
}

fn gid(s: &str) -> GroupId {
    GroupId(s.to_owned())
}

/// Очередь с одной локальной группой `g`.
fn queue_with(n: u32) -> (ModelQueue, GroupId) {
    let q = ModelQueue::new(Handle::current(), None);
    let g = gid("local-a");
    q.configure(g.clone(), local(n));
    assert_eq!(q.snapshot().groups[0].max_parallel, n);
    (q, g)
}

fn enq(q: &ModelQueue, g: &GroupId, p: Priority) -> Ticket {
    q.enqueue(g, RunId::new(), p, None).unwrap()
}

/// Дать фоновым задачам (драйверу очереди, задачам тестов) поработать.
async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

fn active(q: &ModelQueue, g: &GroupId) -> u32 {
    q.snapshot()
        .groups
        .into_iter()
        .find(|v| &v.id == g)
        .unwrap()
        .active
}

fn waiting(q: &ModelQueue, g: &GroupId) -> usize {
    q.snapshot()
        .groups
        .into_iter()
        .find(|v| &v.id == g)
        .unwrap()
        .waiting
        .len()
}

/// Ждёт выдачи в фоне и записывает порядок по метке.
fn spawn_recorder(
    t: Ticket,
    label: &'static str,
    log: Arc<Mutex<Vec<&'static str>>>,
) -> JoinHandle<Permit> {
    tokio::spawn(async move {
        let p = t.granted().await.unwrap();
        log.lock().unwrap().push(label);
        p
    })
}

#[tokio::test(start_paused = true)]
async fn local_parallelism_never_exceeds_limit() {
    let (q, g) = queue_with(2);
    let mut tickets: Vec<Ticket> = (0..5).map(|_| enq(&q, &g, Priority::Worker)).collect();
    assert_eq!(active(&q, &g), 2);
    assert_eq!(waiting(&q, &g), 3);
    let rest = tickets.split_off(2);
    let mut permits = Vec::new();
    for t in tickets {
        permits.push(t.granted().await.unwrap());
    }
    let mut rest = rest.into_iter();
    let third = rest.next().unwrap();
    assert_eq!(third.status().borrow().state, WaitState::Queued);
    drop(permits.pop());
    assert_eq!(active(&q, &g), 2);
    assert_eq!(waiting(&q, &g), 2);
    assert_eq!(third.status().borrow().state, WaitState::Granted);
    let _p3 = third.granted().await.unwrap();
    assert_eq!(active(&q, &g), 2);
}

#[tokio::test(start_paused = true)]
async fn priority_order() {
    let (q, g) = queue_with(1);
    let holder = enq(&q, &g, Priority::Interactive).granted().await.unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut hs = Vec::new();
    for (p, l) in [
        (Priority::Arena, "arena"),
        (Priority::Worker, "worker"),
        (Priority::Interactive, "interactive"),
        (Priority::Top, "top"),
    ] {
        hs.push(spawn_recorder(enq(&q, &g, p), l, log.clone()));
    }
    settle().await;
    drop(holder);
    // Каждый получивший сразу освобождает место: выдача идёт по одному.
    for _ in 0..4 {
        settle().await;
        let next = hs.iter().position(|h| h.is_finished()).unwrap();
        drop(hs.remove(next).await.unwrap());
    }
    assert_eq!(
        *log.lock().unwrap(),
        ["interactive", "top", "worker", "arena"]
    );
}

#[tokio::test(start_paused = true)]
async fn aging_prevents_starvation() {
    let (q, g) = queue_with(1);
    let holder = enq(&q, &g, Priority::Interactive).granted().await.unwrap();
    let worker = enq(&q, &g, Priority::Worker);
    tokio::time::advance(Duration::from_secs(61)).await;
    let top = enq(&q, &g, Priority::Top);
    drop(holder);
    assert_eq!(worker.status().borrow().state, WaitState::Granted);
    assert_eq!(top.status().borrow().state, WaitState::Queued);
    let _w = worker.granted().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn background_never_overtakes_interactive() {
    let (q, g) = queue_with(1);
    let holder = enq(&q, &g, Priority::Top).granted().await.unwrap();
    let arena = enq(&q, &g, Priority::Arena);
    tokio::time::advance(Duration::from_secs(3600)).await;
    let inter = enq(&q, &g, Priority::Interactive);
    // Состарившийся фон стоит на ранге 1, интерактив — 0.
    assert_eq!(inter.status().borrow().position, 1);
    assert_eq!(arena.status().borrow().position, 2);
    drop(holder);
    assert_eq!(inter.status().borrow().state, WaitState::Granted);
    assert_eq!(arena.status().borrow().state, WaitState::Queued);
}

#[tokio::test(start_paused = true)]
async fn last_local_slot_is_kept_for_foreground() {
    let (q, g) = queue_with(2);
    let j1 = enq(&q, &g, Priority::Job);
    let j2 = enq(&q, &g, Priority::Job);
    assert_eq!(active(&q, &g), 1);
    assert_eq!(j1.status().borrow().state, WaitState::Granted);
    assert_eq!(j2.status().borrow().state, WaitState::Queued);
    let i = enq(&q, &g, Priority::Interactive);
    assert_eq!(i.status().borrow().state, WaitState::Granted);
    assert_eq!(active(&q, &g), 2);
    // Единственный слот (n = 1) фоновым доступен: max(1, n − 1) = 1.
    let (q1, g1) = queue_with(1);
    let j = enq(&q1, &g1, Priority::Arena);
    assert_eq!(j.status().borrow().state, WaitState::Granted);
}

#[tokio::test(start_paused = true)]
async fn background_limit_does_not_apply_to_cloud() {
    let q = ModelQueue::new(Handle::current(), None);
    let c = gid("cloud:openrouter:openrouter.ai");
    q.configure(c.clone(), cloud(2, None));
    let a = q.enqueue(&c, RunId::new(), Priority::Job, None).unwrap();
    let b = q.enqueue(&c, RunId::new(), Priority::Job, None).unwrap();
    assert_eq!(a.status().borrow().state, WaitState::Granted);
    assert_eq!(b.status().borrow().state, WaitState::Granted);
}

#[tokio::test(start_paused = true)]
async fn pools_do_not_block_each_other() {
    let (q, g) = queue_with(1);
    let c = gid("cloud:anthropic:api.anthropic.com");
    q.configure(c.clone(), cloud(DEFAULT_CLOUD_PARALLEL, None));
    let _l = enq(&q, &g, Priority::Worker).granted().await.unwrap();
    let blocked = enq(&q, &g, Priority::Interactive);
    assert_eq!(blocked.status().borrow().state, WaitState::Queued);
    let t = q.enqueue(&c, RunId::new(), Priority::Worker, None).unwrap();
    let p = t.granted().await.unwrap();
    assert_eq!(p.group(), &c);
}

#[tokio::test(start_paused = true)]
async fn cooldown_until_retry_after() {
    let q = ModelQueue::new(Handle::current(), None);
    let c = gid("cloud:openai:api.openai.com");
    q.configure(c.clone(), cloud(4, None));
    q.cooldown(&c, Instant::now() + Duration::from_secs(10));
    // Более ранний срок не сокращает текущий.
    q.cooldown(&c, Instant::now() + Duration::from_secs(1));
    let t = q
        .enqueue(&c, RunId::new(), Priority::Interactive, None)
        .unwrap();
    match t.status().borrow().state {
        WaitState::Cooldown { remaining_ms } => assert!((9_900..=10_000).contains(&remaining_ms)),
        ref s => panic!("ожидался cooldown, получено {s:?}"),
    }
    let view = q.snapshot().groups.into_iter().next().unwrap();
    assert_eq!(view.cooldown_ms, Some(10_000));
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    assert!(matches!(
        t.status().borrow().state,
        WaitState::Cooldown { .. }
    ));
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    assert_eq!(t.status().borrow().state, WaitState::Granted);
    let _p = t.granted().await.unwrap();
    assert_eq!(q.snapshot().groups[0].cooldown_ms, None);
}

#[tokio::test(start_paused = true)]
async fn budget_exhausted_fails_fast_and_resets_next_day() {
    let day = Arc::new(Mutex::new(jiff::civil::date(2026, 11, 9)));
    let d = day.clone();
    let q =
        ModelQueue::new(Handle::current(), None).with_today(Arc::new(move || *d.lock().unwrap()));
    let c = gid("cloud:openrouter:openrouter.ai");
    q.configure(c.clone(), cloud(4, Some(1.0)));
    q.record_spend(&c, 0.4);
    let ok = q.enqueue(&c, RunId::new(), Priority::Worker, None).unwrap();
    drop(ok.granted().await.unwrap());
    q.record_spend(&c, 0.6);
    assert_eq!(q.snapshot().groups[0].spent_today_usd, 1.0);
    let t = q
        .enqueue(&c, RunId::new(), Priority::Interactive, None)
        .unwrap();
    assert_eq!(
        t.granted().await.err(),
        Some(QueueError::BudgetExhausted { group: c.clone() })
    );
    assert_eq!(waiting(&q, &c), 0);
    *day.lock().unwrap() = jiff::civil::date(2026, 11, 10);
    assert_eq!(q.snapshot().groups[0].spent_today_usd, 0.0);
    let t = q
        .enqueue(&c, RunId::new(), Priority::Interactive, None)
        .unwrap();
    let _p = t.granted().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn budget_exhaustion_fails_waiting_requests() {
    let q = ModelQueue::new(Handle::current(), None);
    let c = gid("cloud:openrouter:openrouter.ai");
    q.configure(c.clone(), cloud(1, Some(2.0)));
    let _p = q
        .enqueue(&c, RunId::new(), Priority::Worker, None)
        .unwrap()
        .granted()
        .await
        .unwrap();
    let t = q.enqueue(&c, RunId::new(), Priority::Worker, None).unwrap();
    q.record_spend(&c, 2.5);
    assert_eq!(
        t.granted().await.err(),
        Some(QueueError::BudgetExhausted { group: c.clone() })
    );
}

fn snapshot_with_ram(available: u64) -> ResourceSnapshot {
    let now = jiff::Timestamp::now();
    ResourceSnapshot {
        sampled_at: now,
        sampled_mono: Some(Instant::now()),
        cpu: CpuSample {
            usage_percent: 5.0,
            cores: 16,
            busy_ms: 0,
        },
        ram: MemSample {
            total: 64 * GIB,
            available,
        },
        gpu_sampled_at: Some(now),
        gpu_sampled_mono: Some(Instant::now()),
        gpu: GpuStatus::Unavailable {
            reason: "тест".into(),
        },
        volumes_sampled_at: Some(now),
        volumes_sampled_mono: Some(Instant::now()),
        volumes: vec![],
        categories_sampled_at: None,
        categories_sampled_mono: None,
        categories: vec![],
    }
}

fn admission(s: ResourceSnapshot) -> (watch::Sender<ResourceSnapshot>, Admission) {
    let (tx, rx) = watch::channel(s);
    let cfg = ResourcesConfig {
        ram_reserve_mib: Some(2048),
        ..ResourcesConfig::default()
    };
    let home = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("za-llm-queue-home");
    (tx, Admission::new(rx, cfg, home))
}

fn gated(n: u32) -> GroupLimits {
    GroupLimits {
        resource_gated: true,
        ..local(n)
    }
}

#[tokio::test(start_paused = true)]
async fn resource_wait_and_auto_resume() {
    // Свободно 1 ГиБ: ниже запаса фоновых (2 ГиБ), выше пола интерактива (512 МиБ).
    let (tx, adm) = admission(snapshot_with_ram(GIB));
    let q = ModelQueue::new(Handle::current(), Some(adm));
    let g = gid("llama-local");
    q.configure(g.clone(), gated(4));
    let job = enq(&q, &g, Priority::Job);
    assert_eq!(
        job.status().borrow().state,
        WaitState::WaitingResource { kind: "ram".into() }
    );
    // Ждущий ресурса не блокирует остальных (RES-012), пользователь не ждёт фона (RES-013).
    let inter = enq(&q, &g, Priority::Interactive);
    let _pi = inter.granted().await.unwrap();
    let h = tokio::spawn(job.granted());
    settle().await;
    assert!(!h.is_finished());
    tx.send(snapshot_with_ram(32 * GIB)).unwrap();
    let _pj = h.await.unwrap().unwrap();
    assert_eq!(active(&q, &g), 2);
}

#[tokio::test(start_paused = true)]
async fn resource_wait_does_not_hold_the_only_slot() {
    // Review: одно место; ждущая ресурса заявка его не держит, и её состояние не
    // переключается на `Queued` из-за занятости слота.
    let (tx, adm) = admission(snapshot_with_ram(GIB));
    let q = ModelQueue::new(Handle::current(), Some(adm));
    let g = gid("llama-local");
    q.configure(g.clone(), gated(1));
    let job = enq(&q, &g, Priority::Job);
    let res = WaitState::WaitingResource { kind: "ram".into() };
    assert_eq!(job.status().borrow().state, res);
    assert_eq!(active(&q, &g), 0);
    let inter = enq(&q, &g, Priority::Interactive);
    assert_eq!(inter.status().borrow().state, WaitState::Granted);
    assert_eq!(active(&q, &g), 1);
    assert_eq!(job.status().borrow().state, res);
    let pi = inter.granted().await.unwrap();
    // Новый снимок без изменений ресурса: состояние прежнее.
    tx.send(snapshot_with_ram(GIB)).unwrap();
    settle().await;
    assert_eq!(job.status().borrow().state, res);
    // Ресурс есть, но место занято — теперь ждёт места.
    tx.send(snapshot_with_ram(32 * GIB)).unwrap();
    settle().await;
    assert_eq!(job.status().borrow().state, WaitState::Queued);
    drop(pi);
    assert_eq!(job.status().borrow().state, WaitState::Granted);
}

#[tokio::test(start_paused = true)]
async fn stopped_monitor_fails_resource_waiters() {
    let (tx, adm) = admission(snapshot_with_ram(GIB));
    let q = ModelQueue::new(Handle::current(), Some(adm));
    let g = gid("llama-local");
    q.configure(g.clone(), gated(4));
    let job = enq(&q, &g, Priority::Job);
    let h = tokio::spawn(job.granted());
    settle().await;
    drop(tx);
    assert_eq!(h.await.unwrap().err(), Some(QueueError::MonitorStopped));
    assert_eq!(waiting(&q, &g), 0);
}

#[tokio::test(start_paused = true)]
async fn cancel_run_removes_waiting() {
    let (q, g) = queue_with(1);
    let _holder = enq(&q, &g, Priority::Interactive).granted().await.unwrap();
    let run = RunId::new();
    let other = enq(&q, &g, Priority::Worker);
    let a = q.enqueue(&g, run, Priority::Worker, None).unwrap();
    let b = q.enqueue(&g, run, Priority::Job, None).unwrap();
    assert_eq!(q.cancel_run(run), 2);
    assert_eq!(a.granted().await.err(), Some(QueueError::Cancelled));
    assert_eq!(b.granted().await.err(), Some(QueueError::Cancelled));
    let view = q.snapshot().groups.into_iter().next().unwrap();
    assert!(view.waiting.iter().all(|w| w.run_id != run));
    assert_eq!(view.waiting.len(), 1);
    assert_eq!(other.status().borrow().position, 1);
    assert_eq!(q.cancel_run(run), 0);
}

#[tokio::test(start_paused = true)]
async fn cancel_run_while_awaiting_grant() {
    // Review Focus 5: run отменён, пока его задача ждёт в `granted()`.
    let (q, g) = queue_with(1);
    let holder = enq(&q, &g, Priority::Interactive).granted().await.unwrap();
    let run = RunId::new();
    let h = tokio::spawn(
        q.enqueue(&g, run, Priority::Worker, None)
            .unwrap()
            .granted(),
    );
    settle().await;
    assert_eq!(q.cancel_run(run), 1);
    assert_eq!(h.await.unwrap().err(), Some(QueueError::Cancelled));
    drop(holder);
    assert_eq!(active(&q, &g), 0);
    assert_eq!(waiting(&q, &g), 0);
}

#[tokio::test(start_paused = true)]
async fn dropped_ticket_leaves_queue() {
    let (q, g) = queue_with(1);
    let holder = enq(&q, &g, Priority::Interactive).granted().await.unwrap();
    let t = enq(&q, &g, Priority::Worker);
    assert_eq!(waiting(&q, &g), 1);
    drop(t);
    assert_eq!(waiting(&q, &g), 0);
    // Брошенный ожидающий `granted()` (стрим брошен до выдачи) — тоже.
    let h = tokio::spawn(enq(&q, &g, Priority::Worker).granted());
    settle().await;
    assert_eq!(waiting(&q, &g), 1);
    h.abort();
    let _ = h.await;
    assert_eq!(waiting(&q, &g), 0);
    drop(holder);
    assert_eq!(active(&q, &g), 0);
}

#[tokio::test(start_paused = true)]
async fn granted_but_unclaimed_ticket_releases_slot() {
    // Выдано, но `granted()` не вызван: Drop билета возвращает место следующему.
    let (q, g) = queue_with(1);
    let first = enq(&q, &g, Priority::Worker);
    let second = enq(&q, &g, Priority::Worker);
    assert_eq!(first.status().borrow().state, WaitState::Granted);
    drop(first);
    assert_eq!(second.status().borrow().state, WaitState::Granted);
    assert_eq!(active(&q, &g), 1);
    drop(second);
    assert_eq!(active(&q, &g), 0);
}

#[tokio::test(start_paused = true)]
async fn permit_is_released_on_panic() {
    quiet_expected_panics();
    let (q, g) = queue_with(1);
    let t = enq(&q, &g, Priority::Worker);
    let h = tokio::spawn(async move {
        let _p = t.granted().await.unwrap();
        panic!("{EXPECTED_PANIC}: задача упала с местом");
    });
    let err = h.await.unwrap_err();
    assert!(err.is_panic());
    assert_eq!(active(&q, &g), 0);
    let next = enq(&q, &g, Priority::Worker);
    assert_eq!(next.status().borrow().state, WaitState::Granted);
}

#[tokio::test(start_paused = true)]
async fn long_wait_never_fails_without_max_wait() {
    let (q, g) = queue_with(1);
    let _holder = enq(&q, &g, Priority::Interactive).granted().await.unwrap();
    let t = enq(&q, &g, Priority::Worker);
    let status = t.status();
    let h = tokio::spawn(t.granted());
    tokio::time::advance(Duration::from_secs(3600)).await;
    settle().await;
    assert_eq!(status.borrow().state, WaitState::Queued);
    assert!(!h.is_finished());
    assert_eq!(waiting(&q, &g), 1);
}

#[tokio::test(start_paused = true)]
async fn max_wait_gives_wait_timeout() {
    let (q, g) = queue_with(1);
    let _holder = enq(&q, &g, Priority::Interactive).granted().await.unwrap();
    let t = q
        .enqueue(
            &g,
            RunId::new(),
            Priority::Worker,
            Some(Duration::from_secs(5)),
        )
        .unwrap();
    let h = tokio::spawn(t.granted());
    tokio::time::advance(Duration::from_secs(4)).await;
    settle().await;
    assert!(!h.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(h.await.unwrap().err(), Some(QueueError::WaitTimeout));
    assert_eq!(waiting(&q, &g), 0);
}

#[tokio::test(start_paused = true)]
async fn failure_is_visible_in_status() {
    let (q, g) = queue_with(1);
    let _holder = enq(&q, &g, Priority::Interactive).granted().await.unwrap();
    let t = q
        .enqueue(
            &g,
            RunId::new(),
            Priority::Worker,
            Some(Duration::from_secs(5)),
        )
        .unwrap();
    let mut status = t.status();
    let h = tokio::spawn(t.granted());
    tokio::time::advance(Duration::from_secs(5)).await;
    status.changed().await.unwrap();
    let s = status.borrow().clone();
    assert_eq!(
        s.state,
        WaitState::Failed {
            reason: "wait_timeout".into()
        }
    );
    assert_eq!(s.position, 0);
    assert_eq!(h.await.unwrap().err(), Some(QueueError::WaitTimeout));
    let run = RunId::new();
    let c = q.enqueue(&g, run, Priority::Worker, None).unwrap();
    q.cancel_run(run);
    assert_eq!(
        c.status().borrow().state,
        WaitState::Failed {
            reason: "cancelled".into()
        }
    );
}

#[tokio::test(start_paused = true)]
async fn status_reports_position_and_wait() {
    let (q, g) = queue_with(1);
    let _holder = enq(&q, &g, Priority::Interactive).granted().await.unwrap();
    let ts: Vec<Ticket> = (0..3).map(|_| enq(&q, &g, Priority::Worker)).collect();
    for (i, t) in ts.iter().enumerate() {
        let s = t.status().borrow().clone();
        assert_eq!(s.position, i as u32 + 1);
        assert_eq!(s.state, WaitState::Queued);
    }
    tokio::time::advance(Duration::from_secs(2)).await;
    let view = q.snapshot().groups.into_iter().next().unwrap();
    let pos: Vec<u32> = view.waiting.iter().map(|w| w.position).collect();
    assert_eq!(pos, [1, 2, 3]);
    assert!(view.waiting.iter().all(|w| w.waited_ms >= 2000));
    assert!(view.waiting.iter().all(|w| w.priority == Priority::Worker));
    // Статус обновляется при событиях очереди: после снятия первой заявки.
    let mut ts = ts.into_iter();
    drop(ts.next());
    let rest: Vec<Ticket> = ts.collect();
    assert_eq!(rest[0].status().borrow().position, 1);
    assert_eq!(rest[1].status().borrow().position, 2);
    assert!(rest[0].status().borrow().waited_ms >= 2000);
    let view = q.snapshot().groups.into_iter().next().unwrap();
    assert_eq!(view.waiting.len(), 2);
    assert_eq!(view.waiting[0].position, 1);
}

#[tokio::test(start_paused = true)]
async fn status_position_follows_aging() {
    // Позиции пересчитываются на границах старения без внешних событий.
    let (q, g) = queue_with(1);
    let _holder = enq(&q, &g, Priority::Interactive).granted().await.unwrap();
    let arena = enq(&q, &g, Priority::Arena);
    tokio::time::advance(Duration::from_secs(50)).await;
    let worker = enq(&q, &g, Priority::Worker);
    // Arena: 5 − 1 = 4 > Worker 3.
    assert_eq!(arena.status().borrow().position, 2);
    assert_eq!(worker.status().borrow().position, 1);
    tokio::time::advance(Duration::from_secs(10)).await;
    settle().await;
    // Arena: 5 − 2 = 3 = Worker, FIFO → Arena первой.
    assert_eq!(arena.status().borrow().position, 1);
    assert_eq!(worker.status().borrow().position, 2);
    assert!(arena.status().borrow().waited_ms >= 60_000);
}

#[tokio::test(start_paused = true)]
async fn cloud_group_default_parallel_is_four() {
    let q = ModelQueue::new(Handle::current(), None);
    let c = gid("cloud:anthropic:api.anthropic.com");
    q.configure(c.clone(), cloud(DEFAULT_CLOUD_PARALLEL, None));
    let _ts: Vec<Ticket> = (0..6)
        .map(|_| q.enqueue(&c, RunId::new(), Priority::Worker, None).unwrap())
        .collect();
    assert_eq!(active(&q, &c), 4);
    assert_eq!(waiting(&q, &c), 2);
}

#[tokio::test(start_paused = true)]
async fn set_parallel_grants_more() {
    let (q, g) = queue_with(1);
    let _a = enq(&q, &g, Priority::Worker);
    let b = enq(&q, &g, Priority::Worker);
    assert_eq!(b.status().borrow().state, WaitState::Queued);
    q.set_parallel(&g, 2);
    assert_eq!(b.status().borrow().state, WaitState::Granted);
}

#[tokio::test(start_paused = true)]
async fn unknown_group_is_an_error() {
    let q = ModelQueue::new(Handle::current(), None);
    let g = gid("nope");
    assert_eq!(
        q.enqueue(&g, RunId::new(), Priority::Worker, None).err(),
        Some(QueueError::UnknownGroup(g))
    );
}

#[tokio::test(start_paused = true)]
async fn snapshot_serializes() {
    let (q, g) = queue_with(1);
    let _a = enq(&q, &g, Priority::Worker);
    let _b = enq(&q, &g, Priority::Job);
    let v = serde_json::to_value(q.snapshot()).unwrap();
    assert_eq!(v["groups"][0]["id"], "local-a");
    assert_eq!(v["groups"][0]["kind"], "local");
    assert_eq!(v["groups"][0]["waiting"][0]["priority"], "job");
    assert_eq!(v["groups"][0]["waiting"][0]["state"], "queued");
}

#[tokio::test(start_paused = true)]
async fn crashed_driver_is_restarted() {
    // Фоновая задача падает (паника в источнике даты при пересчёте по снимку); следующее
    // обращение к очереди перезапускает её, и таймеры работают.
    quiet_expected_panics();
    let boom = Arc::new(AtomicBool::new(false));
    let b = boom.clone();
    let today = Arc::new(move || {
        if b.swap(false, Ordering::SeqCst) {
            panic!("{EXPECTED_PANIC}: источник даты");
        }
        jiff::civil::date(2026, 11, 9)
    });
    let (tx, adm) = admission(snapshot_with_ram(32 * GIB));
    let q = ModelQueue::new(Handle::current(), Some(adm)).with_today(today);
    let g = gid("llama-local");
    q.configure(g.clone(), gated(1));
    let _holder = enq(&q, &g, Priority::Interactive).granted().await.unwrap();
    let _w = enq(&q, &g, Priority::Worker);
    settle().await;
    // Пересчёт по снимку делает только фоновая задача: паника — в ней.
    boom.store(true, Ordering::SeqCst);
    tx.send(snapshot_with_ram(32 * GIB)).unwrap();
    settle().await;
    assert!(
        !boom.load(Ordering::SeqCst),
        "фоновая задача не пересчитала"
    );
    let t = q
        .enqueue(
            &g,
            RunId::new(),
            Priority::Worker,
            Some(Duration::from_secs(5)),
        )
        .unwrap();
    let h = tokio::spawn(t.granted());
    tokio::time::advance(Duration::from_secs(5)).await;
    // Без перезапуска таймера нет: ожидание упрётся в предел теста, а не зависнет.
    let r = tokio::time::timeout(Duration::from_secs(60), h).await;
    assert_eq!(
        r.expect("фоновая задача не перезапущена").unwrap().err(),
        Some(QueueError::WaitTimeout)
    );
}

#[tokio::test(start_paused = true)]
async fn permit_drop_restarts_crashed_driver() {
    // Заявка уже ждёт в `granted()` (cooldown), фоновая задача падает; единственное
    // последующее событие — Drop `Permit`. Он перезапускает задачу, и cooldown кончается.
    quiet_expected_panics();
    let boom = Arc::new(AtomicBool::new(false));
    let b = boom.clone();
    let today = Arc::new(move || {
        if b.swap(false, Ordering::SeqCst) {
            panic!("{EXPECTED_PANIC}: источник даты");
        }
        jiff::civil::date(2026, 11, 9)
    });
    let (tx, adm) = admission(snapshot_with_ram(32 * GIB));
    let q = ModelQueue::new(Handle::current(), Some(adm)).with_today(today);
    let g = gid("llama-local");
    q.configure(g.clone(), gated(1));
    let c = gid("cloud:openai:api.openai.com");
    q.configure(c.clone(), cloud(4, None));
    let holder = enq(&q, &g, Priority::Interactive).granted().await.unwrap();
    let _w = enq(&q, &g, Priority::Worker);
    q.cooldown(&c, Instant::now() + Duration::from_secs(10));
    let cooled = q.enqueue(&c, RunId::new(), Priority::Worker, None).unwrap();
    let h = tokio::spawn(cooled.granted());
    settle().await;
    boom.store(true, Ordering::SeqCst);
    tx.send(snapshot_with_ram(32 * GIB)).unwrap();
    settle().await;
    assert!(
        !boom.load(Ordering::SeqCst),
        "фоновая задача не пересчитала"
    );
    drop(holder);
    tokio::time::advance(Duration::from_secs(10)).await;
    let r = tokio::time::timeout(Duration::from_secs(60), h).await;
    let p = r.expect("фоновая задача не перезапущена").unwrap().unwrap();
    assert_eq!(p.group(), &c);
}

#[test]
fn stopped_runtime_fails_timed_requests() {
    // Runtime очереди остановлен: заявки, которым нужен таймер, получают явную ошибку.
    let first = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let q = ModelQueue::new(first.handle().clone(), None);
    let g = gid("local-a");
    q.configure(g.clone(), local(1));
    drop(first);
    let second = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    second.block_on(async {
        let holder = enq(&q, &g, Priority::Interactive).granted().await.unwrap();
        // Без таймера заявка ждёт и выдаётся синхронно.
        let plain = enq(&q, &g, Priority::Worker);
        assert_eq!(plain.status().borrow().state, WaitState::Queued);
        let timed = q
            .enqueue(
                &g,
                RunId::new(),
                Priority::Worker,
                Some(Duration::from_secs(5)),
            )
            .unwrap();
        let r = tokio::time::timeout(Duration::from_secs(5), timed.granted()).await;
        assert_eq!(
            r.expect("вечное ожидание").err(),
            Some(QueueError::RuntimeStopped)
        );
        let c = gid("cloud:openai:api.openai.com");
        q.configure(c.clone(), cloud(4, None));
        q.cooldown(&c, Instant::now() + Duration::from_secs(10));
        let cooled = q.enqueue(&c, RunId::new(), Priority::Worker, None).unwrap();
        let r = tokio::time::timeout(Duration::from_secs(5), cooled.granted()).await;
        assert_eq!(
            r.expect("вечное ожидание").err(),
            Some(QueueError::RuntimeStopped)
        );
        drop(holder);
        let _p = plain.granted().await.unwrap();
    });
}

#[tokio::test(start_paused = true)]
async fn held_tag_keeps_order_and_aging_without_places() {
    // Задача 15, раунд 3 (I6): приостановленные заявки стоят в очереди с порядком и
    // временем постановки, места не получают; снятие приостановки выдаёт по D9.
    let (q, g) = queue_with(1);
    q.hold("ep");
    let job = q
        .enqueue_tagged(&g, RunId::new(), Priority::Job, None, Some("ep".into()))
        .unwrap();
    tokio::time::advance(Duration::from_secs(70)).await;
    let inter = q
        .enqueue_tagged(
            &g,
            RunId::new(),
            Priority::Interactive,
            None,
            Some("ep".into()),
        )
        .unwrap();
    // Заявка без метки той же группы приостановкой не затронута.
    let free = enq(&q, &g, Priority::Worker);
    settle().await;
    assert_eq!(active(&q, &g), 1, "заявка без метки получила место");
    let snap = q.snapshot();
    let view = &snap.groups[0].waiting;
    assert_eq!(view.len(), 2, "приостановленные видны в снимке");
    let job_view = view.iter().find(|w| w.priority == Priority::Job).unwrap();
    assert!(
        job_view.waited_ms >= 70_000,
        "старение потеряно: {}",
        job_view.waited_ms
    );
    assert_eq!(job_view.state, WaitState::Queued);

    let log = Arc::new(Mutex::new(Vec::new()));
    let job_h = spawn_recorder(job, "job", log.clone());
    let inter_h = spawn_recorder(inter, "interactive", log.clone());
    settle().await;
    assert!(
        log.lock().unwrap().is_empty(),
        "приостановленная заявка получила место"
    );
    let p_free = free.granted().await.unwrap();
    q.release("ep");
    drop(p_free);
    settle().await;
    let p_inter = inter_h.await.unwrap();
    assert_eq!(*log.lock().unwrap(), vec!["interactive"]);
    drop(p_inter);
    let _p_job = job_h.await.unwrap();
    assert_eq!(*log.lock().unwrap(), vec!["interactive", "job"]);
}
