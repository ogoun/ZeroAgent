//! Бюджет run (CORE-113): необязательные пределы по видам расхода.

use serde::{Deserialize, Serialize};

/// Пределы расхода run; `None` — без предела.
///
/// Незнакомое поле — ошибка разбора: опечатка в имени предела (`step` вместо
/// `steps`) иначе молча снимала бы ограничение.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Токены: `input + output` всех вызовов модели (`input` включает кэш).
    pub tokens: Option<u64>,
    /// Время GPU, мс.
    pub gpu_ms: Option<u64>,
    /// Стоимость, доллары США.
    pub usd: Option<f64>,
    /// Число шагов run.
    pub steps: Option<u32>,
    /// Длительность run по настенным часам, мс.
    pub duration_ms: Option<u64>,
}

/// Вид бюджета: поле `kind` события `budget.exceeded`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetKind {
    Tokens,
    GpuMs,
    Usd,
    Steps,
    Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_default_is_unlimited() {
        assert_eq!(Budget::default().steps, None);
        assert_eq!(serde_json::to_value(BudgetKind::GpuMs).unwrap(), "gpu_ms");
    }

    #[test]
    fn budget_rejects_unknown_fields() {
        use serde_json::json;
        assert!(serde_json::from_value::<Budget>(json!({"foo": 1})).is_err());
        assert_eq!(
            serde_json::from_value::<Budget>(json!({"steps": 2}))
                .unwrap()
                .steps,
            Some(2)
        );
    }
}
