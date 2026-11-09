//! Слоты llama.cpp (D29, MDL-011): соответствие «сессия → слот» с вытеснением по LRU,
//! сохранение и восстановление KV-кэша слота.
//!
//! [`SlotManager`] — чистая бухгалтерия без ввода-вывода: кому принадлежит слот, когда
//! им пользовались, занят ли он. Он только планирует ([`SlotPlan`]); запросы
//! `POST /slots/{id}?action=save|restore` выполняет [`LlamaSlots`], а порядок и реакцию
//! на ошибки определяет вызывающий (`Gateway`): сначала `save_evicted`, затем `restore`,
//! затем запрос модели с `id_slot`.
//!
//! Все сбои здесь влияют только на скорость, не на корректность: KV-кэш — кэш.
//! llama-server сам сравнивает промпт с содержимым слота (`cache_prompt`) и досчитывает
//! расхождение. Несохранённый или невосстановленный слот означает пересчёт промпта.
//!
//! Сверено с llama.cpp `tools/server` (master `a0ad695`, 2026-11-09):
//! - `POST /slots/:id_slot?action=save|restore|erase`, тело `{"filename": "..."}`; без
//!   `--slot-save-path` — 501 `not_supported_error` до разбора тела;
//! - имя проверяется `fs_validate_filename` (без каталогов), файл кладётся в
//!   `--slot-save-path`; неверное имя — 400;
//! - нет файла или он не подходит — 400 `invalid_request_error` «Unable to restore slot»,
//!   слот при этом очищается;
//! - save/restore занятого слота откладывается сервером до его освобождения;
//! - `id_slot` в теле `/completion` и `/v1/chat/completions` (общий обработчик), номер
//!   берётся по модулю числа слотов; занятый слот — задача ждёт его.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::json;

use crate::error::{ErrorClass, ModelError};
use crate::http::{HeaderMap, HttpTransport};

/// Предел одного запроса save или restore. Сохранение идёт, пока заявка держит
/// разрешение очереди, поэтому ждать дольше нельзя: при таймауте вызывающий
/// продолжает без сохранения. KV-кэш слота на десятки тысяч токенов — единицы ГБ,
/// запись на локальный диск занимает секунды.
pub const SLOT_ACTION_TIMEOUT: Duration = Duration::from_secs(30);

/// План для выданного слота.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotPlan {
    /// Слот для `id_slot` запроса.
    pub slot: u32,
    /// Сохранить слот перед использованием: (слот, файл прежнего владельца).
    pub save_evicted: Option<(u32, String)>,
    /// Восстановить в слот файл сессии (после `save_evicted`).
    pub restore: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct SlotState {
    /// Сессия, чей KV-кэш лежит в слоте.
    owner: Option<String>,
    /// Отметка последнего использования (выдача или освобождение).
    last_used: u64,
    busy: bool,
}

/// Соответствие «сессия → слот» (D29). Сессия владеет не больше чем одним слотом.
#[derive(Debug)]
pub struct SlotManager {
    slots: Vec<SlotState>,
    /// Логические часы LRU: растут при каждой выдаче и освобождении.
    clock: u64,
}

impl SlotManager {
    /// `total_slots` — `total_slots` из `/props`; ноль трактуется как один слот.
    pub fn new(total_slots: u32) -> Self {
        let n = total_slots.max(1) as usize;
        Self {
            slots: vec![SlotState::default(); n],
            clock: 0,
        }
    }

    /// Вызывается при выдаче `Permit`; свободный слот гарантирован очередью.
    ///
    /// С сессией (D29): слот, где сессия уже живёт, если он свободен; иначе свободный
    /// слот без владельца; иначе свободный слот, который дольше всех не использовался,
    /// его прежний владелец сохраняется (`save_evicted`). `restore` планируется, если
    /// сессия не живёт ни в одном слоте (в том числе неизвестна: её файл мог остаться
    /// от прошлого запуска daemon). Если домашний слот сессии занят, сессия переезжает
    /// в выбранный слот без `restore`: её состояние ещё дописывается в прежнем.
    ///
    /// `None` — запрос без сессии: свободный слот без владельца; если такого нет —
    /// LRU-слот, его прежний владелец сохраняется, владельцем запрос не становится.
    pub fn assign(&mut self, session: Option<&str>) -> SlotPlan {
        let home = session.and_then(|s| self.home_of(s));
        let slot = match home {
            Some(h) if !self.slots[h].busy => h,
            _ => self.pick_free(),
        };
        self.clock += 1;
        let st = &mut self.slots[slot];
        let evicted = match (&st.owner, session) {
            (Some(o), Some(s)) if o == s => None,
            (Some(_), _) => st.owner.take(),
            (None, _) => None,
        };
        let save_evicted = evicted.map(|o| (slot as u32, slot_file(&o)));
        let restore = match (session, home) {
            (Some(s), None) => Some(slot_file(s)),
            _ => None,
        };
        st.owner = session.map(str::to_owned);
        st.busy = true;
        st.last_used = self.clock;
        // Сессия переехала из занятого домашнего слота: тот остаётся без владельца.
        if let Some(h) = home
            && h != slot
        {
            self.slots[h].owner = None;
        }
        SlotPlan {
            slot: slot as u32,
            save_evicted,
            restore,
        }
    }

    /// Освобождает слот; номер вне диапазона игнорируется.
    pub fn release(&mut self, slot: u32) {
        let Some(st) = self.slots.get_mut(slot as usize) else {
            tracing::debug!(slot, "освобождение несуществующего слота");
            return;
        };
        self.clock += 1;
        st.busy = false;
        st.last_used = self.clock;
    }

    /// Выдача слота под замком с освобождением при drop [`SlotLease`]: отмена заявки
    /// (drop будущего во время save, restore или стрима) не оставляет слот занятым.
    pub fn lease(this: &Arc<Mutex<Self>>, session: Option<&str>) -> (SlotLease, SlotPlan) {
        let plan = lock(this).assign(session);
        let lease = SlotLease {
            manager: this.clone(),
            slot: plan.slot,
        };
        (lease, plan)
    }

    fn home_of(&self, session: &str) -> Option<usize> {
        self.slots
            .iter()
            .position(|st| st.owner.as_deref() == Some(session))
    }

    /// Свободный слот без владельца, иначе свободный LRU-слот. Свободных нет (нарушен
    /// договор с очередью) — LRU среди всех с предупреждением: сервер поставит запрос
    /// в очередь к занятому слоту, это медленно, но не ошибка.
    fn pick_free(&self) -> usize {
        let lru = |busy_ok: bool| {
            self.slots
                .iter()
                .enumerate()
                .filter(|(_, st)| busy_ok || !st.busy)
                .min_by_key(|(_, st)| st.last_used)
                .map(|(i, _)| i)
        };
        if let Some(i) = self
            .slots
            .iter()
            .position(|st| !st.busy && st.owner.is_none())
        {
            return i;
        }
        lru(false).unwrap_or_else(|| {
            tracing::warn!("все слоты llama.cpp заняты при выдаче разрешения");
            lru(true).unwrap_or(0)
        })
    }
}

/// Выданный слот; drop освобождает его в [`SlotManager`].
#[derive(Debug)]
pub struct SlotLease {
    manager: Arc<Mutex<SlotManager>>,
    slot: u32,
}

impl SlotLease {
    pub fn slot(&self) -> u32 {
        self.slot
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        lock(&self.manager).release(self.slot);
    }
}

/// Замок без паники: состояние менеджера после паники держателя остаётся согласованным
/// (каждая операция меняет его целиком под замком), поэтому отравление снимается.
fn lock(m: &Mutex<SlotManager>) -> MutexGuard<'_, SlotManager> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Имя файла сохранения сессии: `za-<blake3(session)[..16]>.bin`. Только hex от
/// хэша, поэтому ни путь, ни недопустимые символы от пользователя в имя не попадают;
/// детерминировано — восстановление работает после перезапуска daemon.
pub fn slot_file(session: &str) -> String {
    let hash = blake3::hash(session.as_bytes()).to_hex();
    format!("za-{}.bin", &hash[..16])
}

/// Ошибка save означает «сохранение не поддержано» (D29): `BadRequest` (501 без
/// `--slot-save-path`, 400) или `NotFound` (нет маршрута). Вызывающий ставит
/// `slot_save = Some(false)`, предупреждает один раз и больше не сохраняет. Прочие
/// классы (таймаут, сбой соединения, 500 «Unable to save slot») — разовые.
pub fn save_unsupported(e: &ModelError) -> bool {
    matches!(e.class, ErrorClass::BadRequest | ErrorClass::NotFound)
}

/// Ошибка restore означает «сохранение не поддержано»: 501 или `NotFound`. Прочие 4xx —
/// нет файла или он не подходит (400 «Unable to restore slot»): обычный случай для
/// новой сессии, вызывающий пишет `debug` и продолжает.
pub fn restore_unsupported(e: &ModelError) -> bool {
    e.http_status == Some(501) || e.class == ErrorClass::NotFound
}

/// Вызовы `/slots` llama-server. `base_url` — корень сервера (D37).
#[derive(Debug, Clone)]
pub struct LlamaSlots {
    base_url: String,
    http: HttpTransport,
    timeout: Duration,
}

impl LlamaSlots {
    pub fn new(base_url: String, http: HttpTransport) -> Self {
        Self {
            base_url,
            http,
            timeout: SLOT_ACTION_TIMEOUT,
        }
    }

    /// Другой предел запроса вместо [`SLOT_ACTION_TIMEOUT`].
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// `POST {base}/slots/{slot}?action=save` с `{"filename": file}`.
    pub async fn save(&self, slot: u32, file: &str) -> Result<(), ModelError> {
        self.action(slot, "save", file).await
    }

    /// `POST {base}/slots/{slot}?action=restore` с `{"filename": file}`.
    pub async fn restore(&self, slot: u32, file: &str) -> Result<(), ModelError> {
        self.action(slot, "restore", file).await
    }

    /// Drop будущего обрывает соединение; сервер доводит уже принятую задачу сам.
    async fn action(&self, slot: u32, action: &str, file: &str) -> Result<(), ModelError> {
        let url = format!(
            "{}/slots/{slot}?action={action}",
            self.base_url.trim_end_matches('/')
        );
        let body = json!({ "filename": file });
        let fut = self.http.post_json(&url, HeaderMap::new(), &body, &[]);
        match tokio::time::timeout(self.timeout, fut).await {
            Err(_) => Err(ModelError::new(
                ErrorClass::Timeout,
                format!(
                    "слот {slot}: {action} не завершён за {} мс",
                    self.timeout.as_millis()
                ),
            )),
            Ok(Err(e)) => {
                tracing::debug!(slot, action, class = ?e.class, "действие со слотом не выполнено");
                Err(e)
            }
            Ok(Ok(_)) => Ok(()),
        }
    }
}
