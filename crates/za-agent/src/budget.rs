//! Бюджеты run (CORE-113, D19).
//!
//! Расход run ([`Spent`]) собирается на границе шага из данных эффекта `boundary`
//! (`UsageTotals`) и счётчиков цикла; [`check_budget`] сравнивает его с пределами
//! [`Budget`] до начала хода и до приёма ввода (D32). Превышение драйвер оформляет
//! чистой паузой (D5): `budget.exceeded`, снимок, `blocked_on_budget`.

use serde::{Deserialize, Serialize};
use za_protocol::{Budget, BudgetKind};
use za_store::UsageTotals;

/// Расход run на границе шага.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Spent {
    /// `input + output` всех вызовов модели (`input` уже включает кэш).
    pub tokens: u64,
    pub gpu_ms: u64,
    pub usd: f64,
    /// Начатые ходы модели (`Counters.steps`).
    pub steps: u32,
    /// Активное время run (`Counters.active_ms`).
    pub duration_ms: u64,
}

impl Spent {
    /// Расход из итогов таблицы `usage`, числа ходов и активного времени.
    pub fn from_totals(t: &UsageTotals, steps: u32, duration_ms: u64) -> Spent {
        Spent {
            tokens: t.input.saturating_add(t.output),
            gpu_ms: t.gpu_ms,
            usd: t.cost_usd,
            steps,
            duration_ms,
        }
    }
}

/// D19, на границе до начала хода. Первое превышенное поле в порядке Tokens, GpuMs,
/// Usd, Steps, Duration; `(kind, used, limit)`. Steps — блок при `used >= limit`
/// (следующий ход не начинается); остальные — при `used > limit` (равенство — не
/// превышение).
pub fn check_budget(b: &Budget, s: &Spent) -> Option<(BudgetKind, f64, f64)> {
    // u64 → f64: значения расхода далеко ниже 2^53, точность не теряется.
    let over = |used: u64, limit: Option<u64>| limit.filter(|l| used > *l);
    if let Some(l) = over(s.tokens, b.tokens) {
        return Some((BudgetKind::Tokens, s.tokens as f64, l as f64));
    }
    if let Some(l) = over(s.gpu_ms, b.gpu_ms) {
        return Some((BudgetKind::GpuMs, s.gpu_ms as f64, l as f64));
    }
    if let Some(l) = b.usd.filter(|l| s.usd > *l) {
        return Some((BudgetKind::Usd, s.usd, l));
    }
    if let Some(l) = b.steps.filter(|l| s.steps >= *l) {
        return Some((BudgetKind::Steps, f64::from(s.steps), f64::from(l)));
    }
    if let Some(l) = over(s.duration_ms, b.duration_ms) {
        return Some((BudgetKind::Duration, s.duration_ms as f64, l as f64));
    }
    None
}

/// Число для payload `budget.exceeded {kind, used, limit}`: целое для всех видов,
/// кроме `usd` (`{"kind": "steps", "used": 2, "limit": 2}`).
pub(crate) fn budget_number(kind: BudgetKind, v: f64) -> serde_json::Value {
    match kind {
        BudgetKind::Usd => serde_json::json!(v),
        // Целые виды пришли из u64/u32 через f64 без потери точности (см. check_budget).
        _ => serde_json::json!(v as u64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_number_is_integer_except_usd() {
        assert_eq!(budget_number(BudgetKind::Steps, 2.0), serde_json::json!(2));
        assert_eq!(budget_number(BudgetKind::Usd, 0.5), serde_json::json!(0.5));
    }
}
