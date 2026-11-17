//! Эффекты run по событиям журнала в интеграционных тестах `za-agent` (R1-M, fix-3
//! задачи 3): позиция ключа и имена намерений по порядку.

use za_protocol::{Event, EventKind};

/// `(шаг, номер)` ключа эффекта `{run}:{step}:{seq}` из намерения или квитанции.
pub fn key_pos(e: &Event) -> (u64, u64) {
    let key = e.payload["key"].as_str().unwrap();
    let mut it = key.rsplit(':');
    let seq = it.next().unwrap().parse().unwrap();
    let step = it.next().unwrap().parse().unwrap();
    (step, seq)
}

/// Все эффекты run как `(шаг, номер, имя)` по порядку намерений.
pub fn effect_names(evs: &[Event]) -> Vec<(u64, u64, String)> {
    evs.iter()
        .filter(|e| e.kind == EventKind::EffectIntent)
        .map(|e| {
            let (s, q) = key_pos(e);
            (s, q, e.payload["name"].as_str().unwrap().to_owned())
        })
        .collect()
}
