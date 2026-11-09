//! Слоты llama.cpp (задача 14, D29): соответствие «сессия → слот» с LRU, сохранение
//! и восстановление KV-кэша слота (`POST /slots/{id}?action=save|restore`).

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use support::{FakeServer, Reply};
use za_llm::ErrorClass;
use za_llm::http::{HttpTransport, Timeouts};
use za_llm::slots::{
    LlamaSlots, SlotManager, SlotPlan, restore_unsupported, save_unsupported, slot_file,
};

const SLOT1: &str = "POST /slots/1";

fn transport() -> HttpTransport {
    HttpTransport::local(Timeouts {
        connect: Duration::from_secs(2),
        first_byte: Duration::from_secs(5),
        idle: Duration::from_secs(5),
    })
}

#[test]
fn same_session_gets_same_slot() {
    let mut m = SlotManager::new(2);
    let a = m.assign(Some("a"));
    let s = a.slot;
    m.release(s);
    let b = m.assign(Some("b"));
    assert_ne!(b.slot, s);
    m.release(b.slot);
    assert_eq!(
        m.assign(Some("a")),
        SlotPlan {
            slot: s,
            save_evicted: None,
            restore: None
        }
    );
}

#[test]
fn eviction_saves_previous_owner() {
    let mut m = SlotManager::new(2);
    let a = m.assign(Some("a"));
    m.release(a.slot);
    let b = m.assign(Some("b"));
    m.release(b.slot);
    let c = m.assign(Some("c"));
    assert_eq!(c.slot, a.slot, "дольше всех не использовался слот a");
    assert_eq!(c.save_evicted, Some((a.slot, slot_file("a"))));
    assert_eq!(c.restore, Some(slot_file("c")));
}

#[test]
fn returning_session_is_restored() {
    let mut m = SlotManager::new(2);
    for s in ["a", "b", "c"] {
        let p = m.assign(Some(s));
        m.release(p.slot);
    }
    // a вытеснена сессией c; теперь дольше всех не использовался слот b.
    let p = m.assign(Some("a"));
    assert_eq!(p.restore, Some(slot_file("a")));
    assert_eq!(p.save_evicted, Some((p.slot, slot_file("b"))));
}

#[test]
fn new_session_tries_restore() {
    // Сессия, сохранённая до перезапуска daemon, неизвестна менеджеру: restore пробуется.
    let mut m = SlotManager::new(2);
    let p = m.assign(Some("a"));
    assert_eq!(p.restore, Some(slot_file("a")));
    assert_eq!(p.save_evicted, None);
}

#[test]
fn sessionless_request_takes_free_unowned_slot() {
    let mut m = SlotManager::new(2);
    let a = m.assign(Some("a"));
    assert_eq!(a.slot, 0);
    m.release(0);
    assert_eq!(
        m.assign(None),
        SlotPlan {
            slot: 1,
            save_evicted: None,
            restore: None
        }
    );
    m.release(1);
    assert_eq!(
        m.assign(Some("a")),
        SlotPlan {
            slot: 0,
            save_evicted: None,
            restore: None
        }
    );
}

#[test]
fn sessionless_request_evicts_lru_owner_without_taking_ownership() {
    let mut m = SlotManager::new(2);
    for s in ["a", "b"] {
        let p = m.assign(Some(s));
        m.release(p.slot);
    }
    let p = m.assign(None);
    assert_eq!(p.slot, 0);
    assert_eq!(p.save_evicted, Some((0, slot_file("a"))));
    assert_eq!(p.restore, None);
    m.release(0);
    // Слот 0 теперь без владельца: следующий запрос без сессии берёт его без сохранения.
    let q = m.assign(None);
    assert_eq!(
        q,
        SlotPlan {
            slot: 0,
            save_evicted: None,
            restore: None
        }
    );
}

#[test]
fn busy_home_slot_moves_session_without_restore() {
    // Сессия a занята в слоте 0 (параллельный запрос той же сессии): второй запрос
    // получает другой слот, restore не планируется (состояние a ещё пишется в слоте 0).
    let mut m = SlotManager::new(2);
    let first = m.assign(Some("a"));
    let second = m.assign(Some("a"));
    assert_ne!(first.slot, second.slot);
    assert_eq!(second.restore, None);
    assert_eq!(second.save_evicted, None);
}

#[test]
fn release_of_unknown_slot_is_ignored() {
    let mut m = SlotManager::new(1);
    m.release(7);
    assert_eq!(m.assign(None).slot, 0);
}

#[test]
fn lease_releases_slot_on_drop() {
    // Отмена во время save (drop будущего) не оставляет слот занятым: слот b после
    // drop свободен, и a, чей домашний слот 0 занят, получает слот 1 с сохранением b.
    // Без освобождения свободных слотов нет — запасной путь дал бы слот 0 без save.
    let m = Arc::new(Mutex::new(SlotManager::new(2)));
    let (lease_a, plan_a) = SlotManager::lease(&m, Some("a"));
    assert_eq!((lease_a.slot(), plan_a.slot), (0, 0));
    let (lease_b, plan_b) = SlotManager::lease(&m, Some("b"));
    assert_eq!((lease_b.slot(), plan_b.slot), (1, 1));
    drop(lease_b);
    let (_again, plan) = SlotManager::lease(&m, Some("a"));
    assert_eq!(
        plan,
        SlotPlan {
            slot: 1,
            save_evicted: Some((1, slot_file("b"))),
            restore: None
        }
    );
    drop(lease_a);
}

#[tokio::test]
async fn save_bad_request_is_not_unsupported() {
    // 400 у save — ошибка запроса (неверное имя, слот), а не «сохранение не поддержано».
    let srv = FakeServer::start(vec![(
        SLOT1,
        vec![Reply::Json(
            400,
            json!({"error": {"code": 400, "type": "invalid_request_error",
                "message": "Invalid filename"}}),
        )],
    )])
    .await;
    let slots = LlamaSlots::new(srv.base_url.clone(), transport());
    let err = slots.save(1, &slot_file("a")).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::BadRequest);
    assert!(!save_unsupported(&err));
}

#[tokio::test]
async fn missing_slots_route_is_unsupported() {
    // Нет маршрута /slots (404) — сохранение не поддержано и для save, и для restore.
    let srv = FakeServer::start(vec![]).await;
    let slots = LlamaSlots::new(srv.base_url.clone(), transport());
    let err = slots.save(1, &slot_file("a")).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::NotFound);
    assert!(save_unsupported(&err) && restore_unsupported(&err));
}

#[test]
fn slot_file_is_deterministic() {
    let f = slot_file("s1");
    assert_eq!(f, slot_file("s1"));
    assert_ne!(f, slot_file("s2"));
    assert!(f.starts_with("za-") && f.ends_with(".bin"));
    assert_eq!(f.len(), 3 + 16 + 4);
    assert!(f[3..19].chars().all(|c| c.is_ascii_hexdigit()));
    // Путь от пользователя в имя не попадает.
    assert!(!slot_file("../../etc/passwd").contains('/'));
}

#[tokio::test]
async fn save_unsupported_degrades() {
    let srv = FakeServer::start(vec![(
        SLOT1,
        vec![Reply::Json(
            501,
            json!({"error": {"code": 501, "type": "not_supported_error",
                "message": "This server does not support slots action. Start it with `--slot-save-path`"}}),
        )],
    )])
    .await;
    let slots = LlamaSlots::new(srv.base_url.clone(), transport());
    let err = slots.save(1, &slot_file("a")).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::BadRequest);
    assert!(!err.class.retryable());
    assert!(save_unsupported(&err));
    let err = slots.restore(1, &slot_file("a")).await.unwrap_err();
    assert!(restore_unsupported(&err));
}

#[tokio::test]
async fn restore_of_missing_file_is_not_unsupported() {
    // llama.cpp: нет файла → 400 invalid_request_error «Unable to restore slot».
    let srv = FakeServer::start(vec![(
        SLOT1,
        vec![Reply::Json(
            400,
            json!({"error": {"code": 400, "type": "invalid_request_error",
                "message": "Unable to restore slot: No available space in KV cache or invalid slot save file"}}),
        )],
    )])
    .await;
    let slots = LlamaSlots::new(srv.base_url.clone(), transport());
    let err = slots.restore(1, &slot_file("a")).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::BadRequest);
    assert!(!restore_unsupported(&err));
}

#[tokio::test]
async fn save_and_restore_requests_shape() {
    let srv = FakeServer::start(vec![(
        SLOT1,
        vec![Reply::Json(
            200,
            json!({"id_slot": 1, "filename": slot_file("a"), "n_saved": 10, "n_written": 100,
                "timings": {"save_ms": 1.0}}),
        )],
    )])
    .await;
    // Завершающий `/` в base_url допустим.
    let slots = LlamaSlots::new(format!("{}/", srv.base_url), transport());
    slots.save(1, &slot_file("a")).await.unwrap();
    slots.restore(1, &slot_file("a")).await.unwrap();
    let reqs = srv.requests(SLOT1);
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].path_and_query, "/slots/1?action=save");
    assert_eq!(reqs[0].json(), json!({"filename": slot_file("a")}));
    assert_eq!(reqs[1].path_and_query, "/slots/1?action=restore");
    assert_eq!(reqs[1].json(), json!({"filename": slot_file("a")}));
}

#[tokio::test]
async fn hanging_save_times_out() {
    // Сохранение не задерживает очередь дольше своего таймаута.
    let srv = FakeServer::start(vec![(SLOT1, vec![Reply::Hang])]).await;
    let slots =
        LlamaSlots::new(srv.base_url.clone(), transport()).with_timeout(Duration::from_millis(200));
    let started = std::time::Instant::now();
    let err = slots.save(1, &slot_file("a")).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Timeout);
    assert!(!save_unsupported(&err));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn reset_keeps_busy_slots_and_old_lease_frees_only_its_own() {
    // Раунд исправлений 1 задачи 15 (I1): сброс после перезапуска сервера забывает
    // владельцев, но не отдаёт слот идущей аренды; поздний drop старой аренды не
    // освобождает слот, выданный уже другой аренде.
    let m = Arc::new(Mutex::new(SlotManager::new(2)));
    let (l0, p0) = SlotManager::lease(&m, Some("a"));
    let (l1, p1) = SlotManager::lease(&m, Some("b"));
    assert_eq!((p0.slot, p1.slot), (0, 1));

    m.lock().unwrap().reset(2);
    {
        let g = m.lock().unwrap();
        assert_eq!(g.busy_slots(), vec![0, 1]);
        assert_eq!(g.slot_of("a"), None, "владельцы забыты");
    }
    // Слот 1 исчезает и появляется снова (сервер с -np 1, затем -np 2).
    m.lock().unwrap().reset(1);
    m.lock().unwrap().reset(2);
    assert_eq!(m.lock().unwrap().busy_slots(), vec![0]);
    let (l2, p2) = SlotManager::lease(&m, Some("c"));
    assert_eq!(p2.slot, 1);
    drop(l1);
    assert_eq!(
        m.lock().unwrap().busy_slots(),
        vec![0, 1],
        "старая аренда освободила чужой слот"
    );
    drop(l2);
    drop(l0);
    assert!(m.lock().unwrap().busy_slots().is_empty());
}
