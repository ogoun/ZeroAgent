//! Повтор A,A,A (D54), цикл ABAB и дрейф ABCABC (D55), застой (D56).
//!
//! Окна считаются по номерам ходов модели. Ход входит в окна повтора и цикла, только
//! если в нём были вызовы, допущенные разбором ответа; застой считает ходы, после
//! которых драйвер зовёт [`RepeatState::on_results`] (ходы с вызовами).
//!
//! Повтор — по ходам, а не по вызовам: отпечаток хода — множество отпечатков его
//! вызовов, и одинаковые вызовы одного хода получают одну реакцию (параллельный
//! пакет из двух одинаковых чтений — не повтор).
//!
//! «Новый факт» — хэш содержимого результата, которого нет среди результатов последних
//! [`SEEN_WINDOW`] ходов. Цикл ABAB — повтор пары ходов, вторая половина которого не
//! дала новых фактов (в первой половине факты новые по определению: это первый раз).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use za_core::ToolKind;
use za_protocol::{BlobHash, detector};

use super::{CYCLE_NUDGE, REPEAT_NUDGE, REPEAT_REJECT, Reaction, call_fingerprint};

/// Окно повтора A,A,A, ходы (D54).
const REPEAT_WINDOW: u32 = 10;
/// Окно новизны хэшей содержимого, ходы: не меньше окна застоя и цикла.
const SEEN_WINDOW: u32 = 10;
/// Повторное срабатывание цикла в пределах стольких ходов после первого — `blocked` (D55).
const CYCLE_SPAN: u32 = 6;
/// Ходов подряд без прогресса до `blocked` (D56).
const STALL_TURNS: u32 = 8;
/// Длина окна отпечатков ходов цикла: `X,Y,Z,X,Y,Z`.
const CYCLE_WINDOW: usize = 6;

/// Состояние повтора, цикла и застоя.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct RepeatState {
    /// Окно повтора: отпечатки вызовов ходов (множество на ход), по возрастанию хода.
    calls: Vec<TurnCalls>,
    /// Прошлый результат по отпечатку вызова — для повторного показа (D54).
    results: BTreeMap<String, Prev>,
    /// Вызовы хода между `on_calls` и `on_results`: `call_id` → отпечаток.
    pending: BTreeMap<String, String>,
    /// Отпечаток хода из `on_calls` (D55: `on_results` берёт его отсюда).
    current: Option<Current>,
    /// Окно цикла: последние отпечатки ходов (не больше [`CYCLE_WINDOW`]).
    turns: Vec<TurnMark>,
    /// Ход первого срабатывания цикла (nudge), пока не истёк [`CYCLE_SPAN`].
    cycle_at: Option<u32>,
    /// Хэши содержимого результатов последних ходов (окно новизны).
    seen: Vec<SeenTurn>,
    /// Ходов подряд без прогресса (D56).
    stall: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct TurnCalls {
    turn: u32,
    /// Отсортированные отпечатки вызовов хода без повторов.
    calls: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Prev {
    call_id: String,
    result: BlobHash,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Current {
    turn: u32,
    fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct TurnMark {
    fingerprint: String,
    /// Ход дал новый факт.
    fresh: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SeenTurn {
    turn: u32,
    hashes: Vec<BlobHash>,
}

/// Ход `t` ещё в окне длины `window`, считая от хода `now` включительно.
fn in_window(t: u32, now: u32, window: u32) -> bool {
    t < now && now - t < window
}

impl RepeatState {
    pub(super) fn on_calls(
        &mut self,
        turn: u32,
        calls: &[(String, String, Value)],
    ) -> Vec<Reaction> {
        // Повторный вход в тот же ход (реплей в том же процессе) не считается дважды.
        self.calls
            .retain(|c| in_window(c.turn, turn, REPEAT_WINDOW));
        let calls_in_window = &self.calls;
        self.results
            .retain(|fp, _| calls_in_window.iter().any(|c| c.calls.contains(fp)));

        let fps: Vec<String> = calls
            .iter()
            .map(|(_, name, args)| call_fingerprint(name, args))
            .collect();
        let mut set = fps.clone();
        set.sort();
        set.dedup();

        let mut nudge = false;
        let mut reactions: Vec<Reaction> = fps
            .iter()
            .map(|fp| {
                let seen = self.calls.iter().filter(|c| c.calls.contains(fp)).count();
                match seen + 1 {
                    1 => Reaction::None,
                    2 => match self.results.get(fp) {
                        Some(p) => Reaction::ReusePrevious {
                            call_id: p.call_id.clone(),
                            result: p.result.clone(),
                        },
                        // Прошлого результата нет: отказ того же текста, что у третьего.
                        None => Reaction::Reject(REPEAT_REJECT.to_owned()),
                    },
                    3 => Reaction::Reject(REPEAT_REJECT.to_owned()),
                    4 => {
                        nudge = true;
                        Reaction::Reject(REPEAT_REJECT.to_owned())
                    }
                    _ => Reaction::Block(detector::REPEAT),
                }
            })
            .collect();
        if nudge {
            reactions.push(Reaction::Nudge(REPEAT_NUDGE.to_owned()));
        }

        self.pending = calls
            .iter()
            .zip(&fps)
            .map(|((id, _, _), fp)| (id.clone(), fp.clone()))
            .collect();
        self.current = (!set.is_empty()).then(|| Current {
            turn,
            fingerprint: set.join(","),
        });
        if !set.is_empty() {
            self.calls.push(TurnCalls { turn, calls: set });
        }
        reactions
    }

    pub(super) fn on_results(
        &mut self,
        turn: u32,
        results: &[(String, ToolKind, bool, BlobHash)],
        answered: bool,
    ) -> Reaction {
        let current = self.current.take().filter(|c| c.turn == turn);
        let pending = std::mem::take(&mut self.pending);

        // Мутация (успешный вызов `Edit` или `Exec`) сбрасывает счётчик повторов (D54).
        let mutation = results
            .iter()
            .any(|(_, kind, err, _)| matches!(kind, ToolKind::Edit | ToolKind::Exec) && !err);
        if mutation {
            self.calls.clear();
            self.results.clear();
        } else {
            for (id, _, _, hash) in results {
                if let Some(fp) = pending.get(id) {
                    self.results.entry(fp.clone()).or_insert_with(|| Prev {
                        call_id: id.clone(),
                        result: hash.clone(),
                    });
                }
            }
        }

        // Новые факты: хэш, которого нет в окне новизны.
        self.seen.retain(|s| in_window(s.turn, turn, SEEN_WINDOW));
        let fresh = results
            .iter()
            .any(|(_, _, _, h)| !self.seen.iter().any(|s| s.hashes.contains(h)));
        let mut hashes: Vec<BlobHash> = results.iter().map(|(_, _, _, h)| h.clone()).collect();
        hashes.sort_by(|a, b| a.0.cmp(&b.0));
        hashes.dedup();
        self.seen.push(SeenTurn { turn, hashes });

        // Застой (D56).
        if fresh || mutation || answered {
            self.stall = 0;
        } else {
            self.stall += 1;
        }

        // Цикл (D55).
        let mut cycle = None;
        if let Some(c) = current {
            self.turns.push(TurnMark {
                fingerprint: c.fingerprint,
                fresh,
            });
            let extra = self.turns.len().saturating_sub(CYCLE_WINDOW);
            self.turns.drain(..extra);
            if self.cycle_at.is_some_and(|t| turn - t > CYCLE_SPAN) {
                self.cycle_at = None;
            }
            if is_cycle(&self.turns) {
                cycle = Some(if self.cycle_at.is_some() {
                    Reaction::Block(detector::CYCLE)
                } else {
                    // Без очистки скользящее окно сработало бы на следующем ходу (`Y,X,Y,X`).
                    self.cycle_at = Some(turn);
                    self.turns.clear();
                    Reaction::Nudge(CYCLE_NUDGE.to_owned())
                });
            }
        }

        match cycle {
            Some(r @ Reaction::Block(_)) => r,
            _ if self.stall >= STALL_TURNS => Reaction::Block(detector::STALL),
            Some(r) => r,
            None => Reaction::None,
        }
    }
}

/// `X,Y,X,Y` (X ≠ Y) или `X,Y,Z,X,Y,Z` (попарно разные) в конце окна, и повторная
/// половина не дала новых фактов.
fn is_cycle(t: &[TurnMark]) -> bool {
    fn fp(m: &TurnMark) -> &str {
        &m.fingerprint
    }
    let n = t.len();
    if n >= 4 {
        let w = &t[n - 4..];
        if fp(&w[0]) == fp(&w[2])
            && fp(&w[1]) == fp(&w[3])
            && fp(&w[0]) != fp(&w[1])
            && w[2..].iter().all(|m| !m.fresh)
        {
            return true;
        }
    }
    if n >= 6 {
        let w = &t[n - 6..];
        let distinct = fp(&w[0]) != fp(&w[1]) && fp(&w[1]) != fp(&w[2]) && fp(&w[0]) != fp(&w[2]);
        if distinct && (0..3).all(|i| fp(&w[i]) == fp(&w[i + 3])) && w[3..].iter().all(|m| !m.fresh)
        {
            return true;
        }
    }
    false
}
