//! Детекторы отказов таблицы 3.4 части 02 (R1-B2 D54–D62).
//!
//! [`DetectorState`] — состояние детекторов run; оно входит в снимок (D66), поэтому
//! после сбоя и resume решения детекторов те же: реплей шага начинается с состояния
//! снимка `Model` и получает из квитанций те же вызовы и результаты. Внешних чтений у
//! детекторов нет (D30): вход — вызовы ответа модели и хэши содержимого результатов.
//!
//! Детектор отвечает [`Reaction`]; применяет её драйвер: до пакета вызовов (повтор
//! A,A,A — повторное использование прошлого результата, отказ, блокировка) и после
//! результатов хода (цикл ABAB, застой). Повтор, цикл и застой — [`repeat`]. Повтор
//! текста и бесконечное рассуждение в стриме хода (D58, D59) — [`stream`]: их состояние
//! живёт одну попытку хода и в снимок не входит. Обещание без действия, цикл ошибок
//! инструмента, порча JSON и падение кэша (D57, D60–D62) — [`turn`]: по итогу
//! принятого хода и его пакета.

mod repeat;
mod stream;
mod turn;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use za_core::ToolKind;
use za_llm::{ReasoningMode, Sampling, Usage};
use za_protocol::{BlobHash, EffectClass, RunRole, canonical_json};

use repeat::RepeatState;
pub use stream::{Degenerate, StreamWatch, THINKING_TIMEOUT, retry_params, thinking_timeout};
use turn::TurnState;
pub use turn::{PROMISE_NUDGE, promise_without_action};

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
    /// Повтор хода с другими параметрами: обрыв стрима детектором (D58, D59), применяет
    /// `model_node` драйвера.
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
    /// Обещание, ошибки инструмента, порча JSON, падение кэша (D57, D60–D62).
    turn: TurnState,
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
        // Ход с вызовами — не обещание: следующее обещание снова получит nudge (D60).
        self.turn.reset_promise();
        self.repeat.on_calls(turn, calls)
    }

    /// Ввод пользователя (очередь, прерывание, поправка) сбрасывает окно повтора
    /// (Ruling I-3 ревью задачи 6), серию обещаний (D60) и счёт ошибок инструмента
    /// (D57, Ruling I-1 ревью задачи 8).
    pub fn on_user_input(&mut self) {
        self.repeat.on_user_input();
        self.turn.reset_promise();
        self.turn.reset_tool_errors();
    }

    /// D60: ход без вызовов с `finish = Stop` и оборотом обещания
    /// ([`promise_without_action`]). Первый — `Nudge(PROMISE_NUDGE)`; второй подряд —
    /// `None`: драйвер принимает ход как обычно (top — ответ, worker и orchestrator —
    /// пустой ход D18).
    pub fn on_promise(&mut self, role: RunRole) -> Reaction {
        self.turn.on_promise(role)
    }

    /// Ход — не обещание (вызовы, ответ, пустой ход): серия обещаний прервана (D60).
    pub(crate) fn reset_promise(&mut self) {
        self.turn.reset_promise();
    }

    /// D57: результаты исполненных вызовов хода — (класс инструмента, `is_error`).
    /// Счёт — по ходам (Ruling I-1 ревью задачи 8): класс получает +1 за ход, в котором
    /// у него есть ошибка и нет успеха; успех класса обнуляет его счёт; успешный Edit
    /// или Exec обнуляет счёт всех классов. Ходы подряд с ошибкой класса: третий —
    /// `Sleep(1 s)`, четвёртый — `Sleep(2 s)`, пятый — `Sleep(4 s)`, шестой —
    /// `Pause("tool_errors")` (счёт класса начинается заново). Реакция хода —
    /// сильнейшая по классам: пауза run, затем самый длинный `Sleep`.
    ///
    /// Передаются только исполненные вызовы (`ToolsOutcome::executed`): повтор прошлого
    /// результата (D54), отказ детектора, гейта или пользователя — не исполнение и
    /// счёт не двигают и не сбрасывают (Carry задачи 6).
    pub fn on_tool_errors(&mut self, results: &[(ToolKind, bool)]) -> Reaction {
        self.turn.on_tool_errors(results)
    }

    /// D61: `parse_errors` принятого хода. Третий ход подряд с `n > 0` — `Compact`
    /// (аварийная компакция на ближайшей границе, [`DetectorState::compaction_due`]);
    /// после неё ещё два таких хода подряд — `Pause("model_loop")`. `n == 0` сбрасывает
    /// серию.
    pub fn on_parse_errors(&mut self, n: u32) -> Reaction {
        self.turn.on_parse_errors(n)
    }

    /// Компакция по порче JSON (D61) назначена и ещё не выполнена.
    pub fn compaction_due(&self) -> bool {
        self.turn.compaction_due()
    }

    /// Компакция выполнена (любая: история уже новая эпоха) — назначенная снята.
    pub(crate) fn compaction_done(&mut self) {
        self.turn.compaction_done();
    }

    /// Запоминает хэши сообщений запроса текущего хода и возвращает хэши прошлого
    /// (для `prev_refs` в [`DetectorState::on_usage`]).
    pub(crate) fn remember_request_refs(&mut self, refs: &[BlobHash]) -> Vec<BlobHash> {
        self.turn.remember_request_refs(refs)
    }

    /// Resume: метка кэша прошлого хода сброшена (D62) — кэш сервера мог остыть за
    /// паузу, эндпоинт мог смениться; первый ход после resume не сравнивается.
    pub(crate) fn reset_cache(&mut self) {
        self.turn.reset_cache();
    }

    /// D62: расход принятого хода. `Some` — payload `context.alert {kind: "cache_drop",
    /// prev_share, share, first_diff}`: у эндпоинта с полем кэша (`cached_supported`)
    /// доля `cached / input` прошлого хода ≥ 0,5, текущего — меньше половины прошлой,
    /// токенов из кэша — меньше половины прошлых, и `epoch` (эпоха истории запроса) не
    /// изменился. `first_diff` — индекс первого сообщения прошлого запроса (`prev_refs`),
    /// хэш которого в текущем (`refs`) другой; `null` — история та же.
    pub fn on_usage(
        &mut self,
        usage: &Usage,
        cached_supported: bool,
        epoch: u32,
        prev_refs: &[BlobHash],
        refs: &[BlobHash],
    ) -> Option<Value> {
        self.turn
            .on_usage(usage, cached_supported, epoch, prev_refs, refs)
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
