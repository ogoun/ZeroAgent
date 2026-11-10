//! Детекторы отказов таблицы 3.4 части 02 (R1-B2 D54–D61).
//!
//! [`DetectorState`] — состояние детекторов run; оно входит в снимок (D66), поэтому
//! после сбоя и resume решения детекторов те же: реплей шага начинается с состояния
//! снимка `Model` и получает из квитанций те же вызовы и результаты. Внешних чтений у
//! детекторов нет (D30): вход — вызовы ответа модели и хэши содержимого результатов.
//!
//! Детектор отвечает [`Reaction`]; применяет её драйвер: до пакета вызовов (повтор
//! A,A,A — повторное использование прошлого результата, отказ, блокировка) и после
//! результатов хода (цикл ABAB, застой). Повтор, цикл и застой — [`repeat`].

mod repeat;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use za_core::ToolKind;
use za_llm::{ReasoningMode, Sampling};
use za_protocol::{BlobHash, EffectClass, canonical_json};

use repeat::RepeatState;

/// Пометка результата, повторно показанного без исполнения (D54); после неё — `\n` и
/// прошлый результат.
pub const REPEAT_MARK: &str =
    "[repeated call: identical to an earlier call; previous result shown, not executed again]";
/// Результат третьего и четвёртого одинакового вызова (D54).
pub const REPEAT_REJECT: &str = "blocked: identical call made 2 times already; change approach";
/// Nudge четвёртого одинакового вызова (D54).
pub const REPEAT_NUDGE: &str = "You are repeating the same call. It was blocked twice: change the tool or its arguments, or finish with what you have.";
/// Nudge цикла ABAB и дрейфа ABCABC (D55); после него драйвер рецитирует задачу.
pub const CYCLE_NUDGE: &str = "You are cycling between the same calls without new results. State in one message what you learned and what you will do differently, then change approach.";

/// Реакция детектора; применяет её драйвер.
#[derive(Debug, Clone, PartialEq)]
pub enum Reaction {
    None,
    /// Вызов не исполняется: результат — прошлый результат того же вызова (`call_id`
    /// прошлого вызова, блоб его содержимого) с пометкой [`REPEAT_MARK`]; `is_error` —
    /// прошлый результат был ошибкой, повтор тоже ошибка (Ruling m-1 ревью задачи 6).
    ReusePrevious {
        call_id: String,
        result: BlobHash,
        is_error: bool,
    },
    /// Вызов не исполняется: результат-ошибка с этим текстом.
    Reject(String),
    /// Управляющее сообщение рантайма модели после результатов хода.
    Nudge(String),
    /// Run — `blocked`; аргумент — детектор (`za_protocol::detector`).
    Block(&'static str),
    /// Run — `paused`; аргумент — причина (`za_protocol::pause_reason`).
    Pause(&'static str),
    /// Пауза перед следующим вызовом модели.
    Sleep(Duration),
    /// Повтор хода с другими параметрами.
    RetryTurn {
        sampling: Sampling,
        reasoning: Option<ReasoningMode>,
    },
    /// Компакция до следующего хода.
    Compact,
}

/// Состояние детекторов run (в снимке, D66). Все поля — `serde(default)`: снимок без
/// них (R1-B1, ранние R1-B2) даёт пустое состояние.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectorState {
    /// Повтор A,A,A, цикл ABAB, застой (D54–D56).
    repeat: RepeatState,
}

/// Отпечаток вызова (D54): первые 16 hex BLAKE3 от `name ‖ 0x00 ‖ canonical_json(args)`.
pub fn call_fingerprint(name: &str, args: &Value) -> String {
    let mut h = blake3::Hasher::new();
    h.update(name.as_bytes());
    h.update(&[0]);
    h.update(canonical_json(args).as_bytes());
    let mut hex = h.finalize().to_hex().to_string();
    hex.truncate(16);
    hex
}

impl DetectorState {
    /// Реакции на вызовы хода по порядку вызовов (D54); запоминает отпечаток хода `turn`.
    ///
    /// Первые `calls.len()` элементов — по одному на вызов (`None`, `ReusePrevious`,
    /// `Reject` или `Block`); за ними — реакции хода (`Nudge`), не больше одной.
    /// `calls` — (call_id, имя, аргументы, класс эффекта) в порядке исполнения; после
    /// вызова, меняющего состояние, остальные вызовы получают `None` (Ruling I-2).
    pub fn on_calls(
        &mut self,
        turn: u32,
        calls: &[(String, String, Value, EffectClass)],
    ) -> Vec<Reaction> {
        self.repeat.on_calls(turn, calls)
    }

    /// Ввод пользователя (очередь, прерывание, поправка) сбрасывает окно повтора
    /// (Ruling I-3 ревью задачи 6).
    pub fn on_user_input(&mut self) {
        self.repeat.on_user_input();
    }

    /// После результатов хода: факты, мутации, ABAB (D55), застой (D56); отпечаток — из
    /// `on_calls` того же `turn`. `results` — (call_id, класс, is_error, хэш блоба
    /// содержимого) — только вызовов с настоящим результатом: исполненных и
    /// перехваченных детектором (Ruling I-1); оборванные, отклонённые гейтом или
    /// пользователем сюда не передаются. `answered` — у top ход дал ответ пользователю
    /// (прогресс D56).
    pub fn on_results(
        &mut self,
        turn: u32,
        results: &[(String, ToolKind, bool, BlobHash)],
        answered: bool,
    ) -> Reaction {
        self.repeat.on_results(turn, results, answered)
    }
}
