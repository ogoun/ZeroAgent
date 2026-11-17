//! Экспозиция Rule of Two (SEC-052, R1-C2 D106, ADR 0006): три независимых свойства run,
//! отметка вызова с внешней связью, исполненного при всех трёх, и строка SEC-052 один раз
//! на run.
//!
//! Вызовов не прерывает — решение заказчика 2026-11-11 «Не спрашивать»: экспозиция
//! пишется в журнал (`exposure.changed`) и аудит (`rule_of_two`); `decide` и `PolicyCx`
//! её не получают.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use za_protocol::{EventKind, taint_origin};

/// Поле payload `tool.started` (запись аудита до вызова) и `tool.finished` (запись после
/// вызова, когда третье свойство дала метка результата): `true` — вызов с внешней связью
/// исполняется при всех трёх свойствах, запись аудита `rule_of_two` о нём есть.
pub(crate) const RULE_OF_TWO_MARK: &str = "rule_of_two";

/// Три независимых свойства run (D106).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exposure {
    /// В контексте недоверенное содержимое: метка любого происхождения, кроме
    /// `outside_project`.
    pub untrusted: bool,
    /// Исполнен нативный вызов с путём вне проекта, в том числе по гранту.
    pub private: bool,
    /// Исполнен нативный сетевой вызов (`risk = Network`).
    pub external: bool,
}

impl Exposure {
    /// `untrusted` по меткам: любое происхождение, кроме `outside_project` — чтение вне
    /// проекта даёт свойство `private`, а не недоверенный ввод (D106, preflight 1.9).
    pub fn untrusted_from(taint: &BTreeSet<String>) -> bool {
        untrusted_origin(taint)
    }

    /// Флаги, установленные хотя бы в одном из двух.
    pub fn union(self, o: Self) -> Self {
        Self {
            untrusted: self.untrusted || o.untrusted,
            private: self.private || o.private,
            external: self.external || o.external,
        }
    }

    /// Установлены все три свойства. Вызов исполняется при всех трёх, если
    /// `snapshot.union(call).all()`: снимок начала пакета плюс собственный вклад вызова,
    /// независимо от того, сколько свойств добавил сам вызов и сколько было в снимке
    /// (Ruling I1 ревью задачи 5). Запись `rule_of_two` получает только такой вызов с
    /// внешней связью (Ruling сомнения 1 прохода fix-5, `ToolJob::external_link`).
    pub fn all(self) -> bool {
        self.untrusted && self.private && self.external
    }

    /// Вызов с флагами `call` ставит флаг, которого в `self` нет.
    pub(crate) fn adds(self, call: Self) -> bool {
        self.union(call) != self
    }
}

/// Среди происхождений есть недоверенное (не `outside_project`).
pub(crate) fn untrusted_origin<'a>(origins: impl IntoIterator<Item = &'a String>) -> bool {
    origins
        .into_iter()
        .any(|o| o != taint_origin::OUTSIDE_PROJECT)
}

/// Событие вызова с внешней связью, исполненного при всех трёх свойствах: `tool.started`
/// или `tool.finished` с отметкой `rule_of_two` (о вызове есть запись аудита).
pub fn rule_of_two_marked(kind: &EventKind, payload: &Value) -> bool {
    matches!(kind, EventKind::ToolStarted | EventKind::ToolFinished)
        && payload.get(RULE_OF_TWO_MARK).and_then(Value::as_bool) == Some(true)
}

/// Строка SEC-052 один раз на run (D106, Ruling I1 ревью задачи 5): по первому событию
/// вызова с внешней связью, исполненного при всех трёх свойствах ([`rule_of_two_marked`]). Run, у которого
/// запись аудита `rule_of_two` уже была до процесса (resume), строку получил раньше. Если
/// событие не дошло (подписчик шины отстал), строку даёт проверка аудита в конце процесса.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuleOfTwoNotice {
    shown: bool,
}

impl RuleOfTwoNotice {
    /// `recorded` — у run уже есть запись аудита `rule_of_two` (строка была раньше).
    pub fn new(recorded: bool) -> Self {
        Self { shown: recorded }
    }

    /// Событие журнала run: `true` — напечатать строку сейчас.
    pub fn on_event(&mut self, kind: &EventKind, payload: &Value) -> bool {
        let first = !self.shown && rule_of_two_marked(kind, payload);
        self.shown |= first;
        first
    }

    /// Строки ещё не было: в конце процесса стоит проверить аудит run.
    pub fn pending(&self) -> bool {
        !self.shown
    }

    /// Конец процесса: `recorded` — у run есть запись аудита `rule_of_two`; `true` —
    /// строки не было, напечатать её сейчас.
    pub fn at_end(&mut self, recorded: bool) -> bool {
        let late = !self.shown && recorded;
        self.shown |= late;
        late
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_three_from_snapshot_and_call() {
        let e = |u, p, x| Exposure {
            untrusted: u,
            private: p,
            external: x,
        };
        // Переход сразу на два свойства — тоже «при всех трёх».
        assert!(e(false, true, false).union(e(true, false, true)).all());
        // Три в снимке — любой вызов, в том числе без своего вклада.
        assert!(e(true, true, true).union(Exposure::default()).all());
        assert!(!e(true, true, false).union(e(true, false, false)).all());
        assert!(e(true, true, false).adds(e(false, false, true)));
        assert!(!e(true, true, false).adds(e(true, false, false)));
    }

    #[test]
    fn notice_once_per_run() {
        let marked = json!({"call_id": "w1", "rule_of_two": true});
        let mut n = RuleOfTwoNotice::default();
        // Событие флагов и вызов без отметки строки не дают.
        assert!(!n.on_event(&EventKind::ExposureChanged, &json!({"rule_of_two": ["w1"]})));
        assert!(!n.on_event(&EventKind::ToolStarted, &json!({"call_id": "r1"})));
        assert!(!n.on_event(
            &EventKind::ToolStarted,
            &json!({"call_id": "r1", "rule_of_two": false})
        ));
        assert!(n.pending());
        assert!(n.on_event(&EventKind::ToolStarted, &marked));
        assert!(!n.on_event(&EventKind::ToolStarted, &marked));
        assert!(!n.on_event(&EventKind::ToolFinished, &marked));
        assert!(!n.pending());
        assert!(!n.at_end(true));
        // Отметка в `tool.finished` (третье свойство дала метка результата).
        let mut n = RuleOfTwoNotice::default();
        assert!(n.on_event(&EventKind::ToolFinished, &marked));
        // Resume run, у которого запись уже была, — без строки.
        let mut n = RuleOfTwoNotice::new(true);
        assert!(!n.on_event(&EventKind::ToolStarted, &marked));
        assert!(!n.at_end(true));
        // Событие потеряно (отставание шины) — строка в конце по аудиту, один раз.
        let mut n = RuleOfTwoNotice::default();
        assert!(!n.at_end(false));
        assert!(n.at_end(true));
        assert!(!n.at_end(true));
    }
}
