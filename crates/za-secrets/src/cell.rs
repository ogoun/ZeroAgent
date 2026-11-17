//! Маска процесса, пересобираемая после каждого раскрытия (R1-M D160).
//!
//! [`MaskCell`] держит текущий [`SecretMasker`] и список известных значений. Значения
//! процесса появляются не сразу: токен памяти известен на старте команды, ключи из Vault —
//! после раскрытия. Каждое [`MaskCell::extend`] пересобирает маскировщик, и
//! [`MaskCell::current`] отдаёт уже новый снимок. Прежний снимок (`Arc`) не меняется: кто
//! взял его до раскрытия, маскирует по старому набору. Поэтому потребители маски берут
//! `current()` после фазы раскрытия (`VaultKeys::reveal` кладёт значения в ячейку до того,
//! как вернуть их вызывающему).

use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::scan::SecretMasker;

/// Ячейка маски процесса: известные значения и маскировщик по ним (D160).
pub struct MaskCell {
    inner: RwLock<Inner>,
}

struct Inner {
    /// Известные пары (имя, значение) без повторов, в порядке добавления.
    known: Vec<(String, String)>,
    current: Arc<SecretMasker>,
}

impl MaskCell {
    /// Ячейка с известными парами (имя, значение) — например, значением токена памяти.
    pub fn new(known: Vec<(String, String)>) -> Self {
        let mut list = Vec::new();
        for pair in known {
            if !list.contains(&pair) {
                list.push(pair);
            }
        }
        let current = Arc::new(SecretMasker::new(list.clone()));
        Self {
            inner: RwLock::new(Inner {
                known: list,
                current,
            }),
        }
    }

    /// Добавляет пары и пересобирает маскировщик. Пары, которые ячейка уже знает, не
    /// добавляются; если нового нет, снимок прежний.
    pub fn extend(&self, more: Vec<(String, String)>) {
        let mut g = self.write();
        let before = g.known.len();
        for pair in more {
            if !g.known.contains(&pair) {
                g.known.push(pair);
            }
        }
        if g.known.len() != before {
            g.current = Arc::new(SecretMasker::new(g.known.clone()));
        }
    }

    /// Текущий маскировщик: известные значения на этот момент и шаблоны D90.
    pub fn current(&self) -> Arc<SecretMasker> {
        Arc::clone(&self.read().current)
    }

    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for MaskCell {
    /// Ячейка без известных значений: только шаблоны.
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl fmt::Debug for MaskCell {
    /// Только имена известных значений: сами значения не печатаются.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let g = self.read();
        let mut names: Vec<&str> = g.known.iter().map(|(n, _)| n.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        f.debug_struct("MaskCell").field("known", &names).finish()
    }
}
