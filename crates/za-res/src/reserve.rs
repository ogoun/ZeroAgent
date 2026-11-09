//! Резервирование места на диске перед большой записью (RES-017).
//!
//! Заявка проходит, если на томе свободно `bytes + disk_margin` сверх уже
//! зарезервированного. Иначе проситель ждёт нового снимка или освобождения чужой
//! заявки; уведомление `DiskWait` — один раз на заявку. Фоновые (`Job`, `Arena`) вдобавок
//! ждут, пока на томе журнала мало места (RES-018).
//!
//! Тома нет среди замеренных — заявка проходит без учёта (мерить нечем). Замер томов
//! устарел или его не было ([`crate::VOLUMES_STALE_AFTER`]) — фоновые ждут, остальные
//! проходят без проверки места; заявка учитывается в резерве, если том известен.
//!
//! **Правило учёта.** Резерв — обещание места под запись. Свободное место заявки
//! считается как `available − зарезервировано − ожидающее`. Байты, снятые с резерва
//! ([`Reservation::consume`] или Drop), становятся *ожидающими* с монотонной отметкой
//! снятия и вычитаются, пока не придёт замер тома, начатый позже снятия
//! (`volumes_sampled_mono` > отметки): только такой замер уже отражает запись. Без
//! монотонной отметки замера (снимок из сериализации) ожидающие байты не снимаются.
//! Пока резерв держится, уже записанные байты учитываются дважды (в замере и в резерве) —
//! консервативно; поэтому владелец отпускает резерв сразу после завершения записи
//! (fsync или rename), а при долгой записи сообщает о записанном через `consume`.
//!
//! Освобождение будит всех ждущих; порядка между ними нет — маленькие заявки могут
//! обгонять большую (справедливость очереди — задача очереди, не резерва).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;
use tokio::time::Instant;
use za_protocol::Priority;

use crate::admission::{
    Admission, MonitorStopped, ResourceNotice, VOLUMES_STALE_AFTER, Verdict, is_fresh,
};
use crate::volumes::volume_for;

const MIB: u64 = 1 << 20;

/// Учёт одного тома.
#[derive(Debug, Default)]
struct Book {
    /// Действующие резервы, байты.
    reserved: u64,
    /// Снятые байты, ещё не отражённые замером тома: (момент снятия, байты).
    pending: Vec<(Instant, u64)>,
}

impl Book {
    fn pending_sum(&self) -> u64 {
        self.pending
            .iter()
            .fold(0u64, |acc, (_, b)| acc.saturating_add(*b))
    }

    /// Снимает ожидающие байты, которые уже отражены замером, начатым в `sampled`.
    fn settle(&mut self, sampled: Option<Instant>) {
        if let Some(t) = sampled {
            self.pending.retain(|(released, _)| *released >= t);
        }
    }

    fn is_empty(&self) -> bool {
        self.reserved == 0 && self.pending.is_empty()
    }
}

type Books = HashMap<PathBuf, Book>;

struct Inner {
    adm: Admission,
    /// Учёт по точке монтирования тома.
    reserved: Mutex<Books>,
    /// Счётчик освобождений: будит ждущих при Drop чужой заявки.
    released: watch::Sender<u64>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Books> {
        // Карта согласована и после паники держателя: правка — одна операция.
        self.reserved.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Резервы места на томах. Clone дешёвый, клоны видят общие резервы.
#[derive(Clone)]
pub struct DiskReservations {
    inner: Arc<Inner>,
}

/// Выданный резерв. Drop возвращает объём и будит ждущих.
pub struct Reservation {
    inner: Arc<Inner>,
    volume: Option<PathBuf>,
    bytes: u64,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("volume", &self.volume)
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl Reservation {
    /// Том, на котором учтён резерв; `None` — том не найден, резерв не учитывается.
    pub fn volume(&self) -> Option<&Path> {
        self.volume.as_deref()
    }

    /// Ещё не записанный объём резерва, байты.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Записано `written` байт: резерв уменьшается на них (не ниже нуля), байты становятся
    /// ожидающими до замера тома, начатого позже этого вызова; ждущие пробуждаются.
    pub fn consume(&mut self, written: u64) {
        let n = written.min(self.bytes);
        if n == 0 {
            return;
        }
        self.bytes -= n;
        if let Some(v) = &self.volume {
            release(&mut self.inner.lock(), v, n);
        }
        self.inner.released.send_modify(|c| *c = c.wrapping_add(1));
    }
}

/// Переводит `n` байт резерва тома `v` в ожидающие.
fn release(map: &mut Books, v: &Path, n: u64) {
    if n == 0 {
        return;
    }
    if let Some(book) = map.get_mut(v) {
        book.reserved = book.reserved.saturating_sub(n);
        book.pending.push((Instant::now(), n));
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(v) = &self.volume {
            release(&mut self.inner.lock(), v, self.bytes);
        }
        self.inner.released.send_modify(|n| *n = n.wrapping_add(1));
    }
}

impl DiskReservations {
    pub fn new(adm: Admission) -> Self {
        let (released, _) = watch::channel(0);
        Self {
            inner: Arc::new(Inner {
                adm,
                reserved: Mutex::new(HashMap::new()),
                released,
            }),
        }
    }

    /// Резервирует `bytes` на томе пути `path`; ждёт, пока места не хватает (RES-017).
    pub async fn reserve(
        &self,
        path: &Path,
        bytes: u64,
        p: Priority,
    ) -> Result<Reservation, MonitorStopped> {
        let mut snaps = self.inner.adm.changed();
        let mut released = self.inner.released.subscribe();
        let mut notified = false;
        loop {
            snaps.borrow_and_update();
            released.borrow_and_update();
            match self.try_reserve(path, bytes, p) {
                Ok(r) => return Ok(r),
                Err(Some(notice)) if !notified => {
                    notified = true;
                    self.inner.adm.notify(notice);
                }
                Err(_) => {}
            }
            tokio::select! {
                r = snaps.changed() => r.map_err(|_| MonitorStopped)?,
                // Отправитель живёт в `Inner`, которым мы владеем: ошибки не бывает.
                _ = released.changed() => {}
            }
        }
    }

    /// Действующие резервы на томе (точка монтирования), байты.
    pub fn reserved_on(&self, volume: &Path) -> u64 {
        self.inner.lock().get(volume).map_or(0, |b| b.reserved)
    }

    /// Снятые с резерва байты тома, ещё не отражённые замером (по последнему учёту), байты.
    pub fn pending_on(&self, volume: &Path) -> u64 {
        self.inner.lock().get(volume).map_or(0, Book::pending_sum)
    }

    /// Одна попытка. `Err(Some)` — места нет (уведомление), `Err(None)` — ждать без
    /// уведомления о месте (том журнала, устаревший замер).
    fn try_reserve(
        &self,
        path: &Path,
        bytes: u64,
        p: Priority,
    ) -> Result<Reservation, Option<ResourceNotice>> {
        let adm = &self.inner.adm;
        if p.is_background() && adm.journal_verdict(p) != Verdict::Admit {
            return Err(None);
        }
        let snap = adm.snapshot();
        let fresh = is_fresh(
            snap.volumes_sampled_mono,
            snap.volumes_sampled_at,
            VOLUMES_STALE_AFTER,
        );
        if !fresh && p.is_background() {
            return Err(None);
        }
        let mounts: Vec<PathBuf> = snap.volumes.iter().map(|v| v.mount.clone()).collect();
        let Some(vol) = volume_for(path, &mounts).cloned() else {
            tracing::debug!(path = %path.display(), "том не найден, резерв места не учитывается");
            return Ok(self.grant(None, bytes));
        };
        let mut map = self.inner.lock();
        let book = map.entry(vol.clone()).or_default();
        book.settle(snap.volumes_sampled_mono);
        let held = book.reserved.saturating_add(book.pending_sum());
        if fresh {
            let available = snap
                .volumes
                .iter()
                .find(|v| v.mount == vol)
                .map_or(0, |v| v.available);
            let free = available.saturating_sub(held);
            let need = bytes.saturating_add(adm.config().disk_margin_mib.saturating_mul(MIB));
            if free < need {
                let mut caches: Vec<_> = snap
                    .categories
                    .iter()
                    .filter(|c| volume_for(&c.path, &mounts) == Some(&vol))
                    .cloned()
                    .collect();
                caches.sort_by_key(|c| std::cmp::Reverse(c.bytes));
                if book.is_empty() {
                    map.remove(&vol);
                }
                drop(map);
                return Err(Some(ResourceNotice::DiskWait {
                    volume: vol,
                    need,
                    free,
                    caches,
                }));
            }
        }
        book.reserved = book.reserved.saturating_add(bytes);
        drop(map);
        Ok(self.grant(Some(vol), bytes))
    }

    fn grant(&self, volume: Option<PathBuf>, bytes: u64) -> Reservation {
        Reservation {
            inner: self.inner.clone(),
            volume,
            bytes,
        }
    }
}
