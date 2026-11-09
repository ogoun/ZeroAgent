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
//!
//! Договор с вызывающим (`Gateway`):
//! - ключ сессии составной — `"{bundle_key}:{session}"`: файл одной связки не попадает
//!   в слот другой модели после смены модели на эндпоинте; [`slot_file`] хэширует ключ
//!   целиком, двоеточие и прочие символы в имя не попадают;
//! - при смене `bundle_key` или `total_slots` и при перезапуске llama-server (по зонду)
//!   менеджер сбрасывается [`SlotManager::reset`]: менеджер не видит сервер и иначе
//!   считал бы, что сессия ещё живёт в слоте (без restore), а при вытеснении затёр бы
//!   хороший файл пустым или чужим содержимым слота. `total_slots` берётся из свежего
//!   зонда: сервер берёт `id_slot` по модулю, и лишние слоты менеджера легли бы на
//!   чужие слоты сервера. Сброс сохраняет занятость идущих аренд (без двойного
//!   занятия слота), drop аренды освобождает только свой слот. Зонд без изменений
//!   менеджер не трогает: карта «сессия → слот» сохраняется;
//! - [`SlotLease`] объявляется полем раньше `Permit` очереди (поля дропаются в порядке
//!   объявления): иначе очередь выдаст место до освобождения слота, и два запроса
//!   получат один слот;
//! - действия идут с тем же `Authorization: Bearer`, что зонд и запрос модели
//!   ([`LlamaSlots::with_secret`]); 401/403 на `/slots` — «не поддержано»;
//! - порядок: `save_evicted`, затем `restore`, затем запрос. Предел каждого действия —
//!   [`SLOT_ACTION_TIMEOUT`], суммарно save и restore держат место очереди до 60 с;
//!   при `slot_save == Some(false)` оба действия пропускаются, привязка к слоту остаётся;
//! - владение передаётся в `assign` сразу, до save: если вытесненная сессия вернётся,
//!   пока идёт её save, она получит другой слот и restore; задачи сервера идут
//!   последовательно, поэтому restore прочтёт старый или полный новый файл, не половину.
//!
//! Известные ограничения:
//! - файлы `za-*.bin` в `--slot-save-path` не удаляются: у llama-server нет API
//!   удаления (`action=erase` очищает только слот), рост каталога — забота настройки
//!   сервера;
//! - RAM-кэш промптов llama-server (`--cache-ram`) уже держит состояния слотов в памяти
//!   при смене промпта и частично перекрывает выигрыш save/restore; файлы остаются
//!   полезны после перезапуска сервера и сверх предела RAM.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::json;

use crate::error::{ErrorClass, ModelError};
use crate::http::{HeaderMap, HttpTransport, Secret};
use reqwest::header::AUTHORIZATION;

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
    /// Номер аренды, занявшей слот (0 — не занят арендой): drop аренды освобождает
    /// слот, только если он всё ещё её.
    lease: u64,
}

/// Соответствие «сессия → слот» (D29). Сессия владеет не больше чем одним слотом.
#[derive(Debug)]
pub struct SlotManager {
    slots: Vec<SlotState>,
    /// Логические часы LRU: растут при каждой выдаче и освобождении.
    clock: u64,
    /// Счётчик номеров аренд.
    next_lease: u64,
    /// Действующих слотов (`total_slots` сервера). Слоты с номером `>= active`
    /// выведены сбросом с уменьшением числа слотов: держатся, только пока заняты
    /// старой арендой, и не выдаются.
    active: usize,
}

impl SlotManager {
    /// `total_slots` — `total_slots` из `/props`; ноль трактуется как один слот.
    pub fn new(total_slots: u32) -> Self {
        let n = total_slots.max(1) as usize;
        Self {
            slots: vec![SlotState::default(); n],
            clock: 0,
            next_lease: 0,
            active: n,
        }
    }

    /// Сервер сменил связку или число слотов либо перезапустился: владельцы забываются
    /// (KV-кэша в слотах сервера больше нет), число слотов — `total_slots`. Занятость
    /// идущих аренд сохраняется: новые заявки не получат их слоты, пока аренды живы, а
    /// drop аренды освобождает только свой слот (по номеру аренды), даже если слоты
    /// между тем исчезали и появлялись снова.
    pub fn reset(&mut self, total_slots: u32) {
        let n = total_slots.max(1) as usize;
        if self.slots.len() < n {
            self.slots.resize_with(n, SlotState::default);
        }
        self.active = n;
        for st in &mut self.slots {
            st.owner = None;
        }
        self.trim();
    }

    /// Убирает выведенные слоты в конце, которые больше никто не занимает. Занятый
    /// выведенный слот остаётся занятым и при новом росте числа слотов: второй аренде
    /// он не достанется, пока жива первая.
    fn trim(&mut self) {
        while self.slots.len() > self.active && self.slots.last().is_some_and(|s| !s.busy) {
            self.slots.pop();
        }
    }

    /// Число действующих слотов.
    pub fn len(&self) -> usize {
        self.active
    }

    /// Слотов нет (не бывает: минимум один).
    pub fn is_empty(&self) -> bool {
        self.active == 0
    }

    /// Занятые слоты по возрастанию номера.
    pub fn busy_slots(&self) -> Vec<u32> {
        (0..self.slots.len())
            .filter(|i| self.slots[*i].busy)
            .map(|i| i as u32)
            .collect()
    }

    /// Слот, где живёт сессия.
    pub fn slot_of(&self, session: &str) -> Option<u32> {
        self.home_of(session).map(|i| i as u32)
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
        self.next_lease += 1;
        st.lease = self.next_lease;
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
        st.lease = 0;
        st.last_used = self.clock;
        self.trim();
    }

    /// Освобождает слот аренды `id`; слот, занятый уже другой арендой, не трогается.
    fn release_lease(&mut self, slot: u32, id: u64) {
        if self
            .slots
            .get(slot as usize)
            .is_some_and(|st| st.busy && st.lease == id)
        {
            self.release(slot);
        }
    }

    /// Выдача слота под замком с освобождением при drop [`SlotLease`]: отмена заявки
    /// (drop будущего во время save, restore или стрима) не оставляет слот занятым.
    pub fn lease(this: &Arc<Mutex<Self>>, session: Option<&str>) -> (SlotLease, SlotPlan) {
        let mut m = lock(this);
        let plan = m.assign(session);
        let id = m.slots[plan.slot as usize].lease;
        drop(m);
        let lease = SlotLease {
            manager: this.clone(),
            slot: plan.slot,
            id,
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
            self.slots[..self.active]
                .iter()
                .enumerate()
                .filter(|(_, st)| busy_ok || !st.busy)
                .min_by_key(|(_, st)| st.last_used)
                .map(|(i, _)| i)
        };
        if let Some(i) = self.slots[..self.active]
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
    id: u64,
}

impl SlotLease {
    pub fn slot(&self) -> u32 {
        self.slot
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        lock(&self.manager).release_lease(self.slot, self.id);
    }
}

/// Замок без паники: состояние менеджера после паники держателя остаётся согласованным
/// (каждая операция меняет его целиком под замком), поэтому отравление снимается.
fn lock(m: &Mutex<SlotManager>) -> MutexGuard<'_, SlotManager> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Имя файла сохранения сессии: `za-<blake3(session)[..16]>.bin`, где `session` —
/// составной ключ `"{bundle_key}:{session}"` от `Gateway`. Только hex от хэша, поэтому
/// ни путь, ни недопустимые символы от пользователя в имя не попадают;
/// детерминировано — восстановление работает после перезапуска daemon.
pub fn slot_file(session: &str) -> String {
    let hash = blake3::hash(session.as_bytes()).to_hex();
    format!("za-{}.bin", &hash[..16])
}

/// Ошибка save означает «сохранение не поддержано» (D29): 501 (нет `--slot-save-path`
/// или состояние модели не сериализуется), `NotFound` (нет маршрута) или `Auth`
/// (401/403: сервер с `--api-key` не принял ключ эндпоинта на `/slots`). Вызывающий
/// ставит `slot_save = Some(false)`, предупреждает один раз и больше не сохраняет.
/// Прочие ошибки (400 — неверный запрос, таймаут, сбой соединения, 500 «Unable to save
/// slot») — разовые.
pub fn save_unsupported(e: &ModelError) -> bool {
    e.http_status == Some(501) || matches!(e.class, ErrorClass::NotFound | ErrorClass::Auth)
}

/// Ошибка restore означает «сохранение не поддержано»: 501, `NotFound` или `Auth`.
/// Прочие 4xx — нет файла или он не подходит (400 «Unable to restore slot»): обычный
/// случай для новой сессии, вызывающий пишет `debug` и продолжает.
pub fn restore_unsupported(e: &ModelError) -> bool {
    save_unsupported(e)
}

/// Вызовы `/slots` llama-server. `base_url` — корень сервера (D37).
#[derive(Debug, Clone)]
pub struct LlamaSlots {
    base_url: String,
    http: HttpTransport,
    timeout: Duration,
    secret: Option<Secret>,
}

impl LlamaSlots {
    pub fn new(base_url: String, http: HttpTransport) -> Self {
        Self {
            base_url,
            http,
            timeout: SLOT_ACTION_TIMEOUT,
            secret: None,
        }
    }

    /// Ключ эндпоинта: `Authorization: Bearer <ключ>`, как у зонда и запроса модели
    /// (llama-server с `--api-key` проверяет ключ на `/slots`). Текст ошибки очищается
    /// от ключа.
    pub fn with_secret(mut self, secret: Option<Secret>) -> Self {
        self.secret = secret;
        self
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
        let mut headers = HeaderMap::new();
        if let Some(s) = &self.secret {
            headers.insert(AUTHORIZATION, s.header_value("Bearer ")?);
        }
        let redact: Vec<&Secret> = self.secret.iter().collect();
        let fut = self.http.post_json(&url, headers, &body, &redact);
        match tokio::time::timeout(self.timeout, fut).await {
            Err(_) => {
                tracing::debug!(slot, action, "действие со слотом: таймаут");
                Err(ModelError::new(
                    ErrorClass::Timeout,
                    format!(
                        "слот {slot}: {action} не завершён за {} мс",
                        self.timeout.as_millis()
                    ),
                ))
            }
            Ok(Err(e)) => {
                tracing::debug!(slot, action, class = ?e.class, "действие со слотом не выполнено");
                Err(e)
            }
            Ok(Ok(_)) => Ok(()),
        }
    }
}
