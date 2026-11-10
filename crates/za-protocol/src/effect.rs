//! Классы и ключи эффектов.

use crate::ids::RunId;
use serde::{Deserialize, Serialize};

/// Класс эффекта по идемпотентности.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Pure,
    ReadOnly,
    Idempotent,
    NonIdempotent,
}

/// Ключ эффекта: `"{run}:{step}:{effect_seq}"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EffectKey(pub String);

impl EffectKey {
    /// Детерминированно строит ключ по run, шагу и номеру эффекта в шаге.
    pub fn derive(run: RunId, step: u64, effect_seq: u64) -> Self {
        Self(format!("{run}:{step}:{effect_seq}"))
    }

    /// Основа имени файла по ключу (D76, Ruling 1.11): `:` → `-`. На Windows `:` в
    /// имени файла — это альтернативный поток данных (ADS), поэтому маркеры, файлы
    /// выгрузки, `procs` и id чекпоинтов строятся только от этой формы ключа.
    pub fn file_stem(&self) -> String {
        self.0.replace(':', "-")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn key_is_stable() {
        let r = RunId(Uuid::parse_str("0190b6a0-0000-7000-8000-000000000001").unwrap());
        assert_eq!(EffectKey::derive(r, 2, 3), EffectKey::derive(r, 2, 3));
        assert_ne!(EffectKey::derive(r, 2, 3), EffectKey::derive(r, 2, 4));
        assert_eq!(
            EffectKey::derive(r, 2, 3).0,
            "0190b6a0-0000-7000-8000-000000000001:2:3"
        );
    }

    #[test]
    fn file_stem_has_no_colon() {
        let r = RunId(Uuid::parse_str("0190b6a0-0000-7000-8000-000000000001").unwrap());
        let stem = EffectKey::derive(r, 2, 3).file_stem();
        assert!(!stem.contains(':'));
        assert_eq!(stem, "0190b6a0-0000-7000-8000-000000000001-2-3");
    }

    #[test]
    fn class_serde_snake_case() {
        let s = serde_json::to_string(&EffectClass::NonIdempotent).unwrap();
        assert_eq!(s, r#""non_idempotent""#);
    }
}
