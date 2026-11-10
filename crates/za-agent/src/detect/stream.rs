//! Детекторы стрима (R1-B2 D58, D59): повтор текста и бесконечное рассуждение.
//!
//! [`StreamWatch`] смотрит дельты одной попытки хода. Повтор текста (D58) проверяется,
//! когда с прошлой проверки накоплено [`CHECK_BYTES`] байт текста или дельта содержит
//! `\n` и текст попытки не короче [`CHECK_MIN_CHARS`] символов: срабатывание —
//! строка от [`MIN_LINE_CHARS`] символов повторена [`REPEATS`] раза подряд в конце
//! текста или [`NGRAM_WORDS`]-словная n-грамма встречается [`REPEATS`] раза в
//! последних [`NGRAM_WINDOW`] словах. Бесконечное рассуждение (D59): байты рассуждения
//! / 4 больше бюджета × 1,5 или рассуждение дольше [`THINKING_TIMEOUT`] без текста и
//! вызовов. Тайм-аут — дедлайн [`StreamWatch::deadline`] в `select!` стрима (`turn.rs`):
//! замолчавший стрим тоже обрывается. Время передаётся явно (`tokio::time::Instant`,
//! как у `DeltaBuffer`), поэтому детектор детерминирован.
//!
//! Реакция — обрыв стрима с квитанцией `interrupted:repetition|thinking` и повтор хода
//! в том же шаге с параметрами [`retry_params`]; второе срабатывание в шаге — пауза
//! `model_loop` (решает драйвер).

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;
use za_llm::{ReasoningMode, Sampling};
use za_protocol::detector;

use crate::context::thinking_budget;

/// Вид вырождения стрима (D58, D59).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Degenerate {
    /// Повтор текста (D58).
    Repetition,
    /// Бесконечное рассуждение (D59).
    Thinking,
}

impl Degenerate {
    /// Имя детектора: поле `detector` в `loop.detected` и `turn.finished` и хвост
    /// квитанции `interrupted:{name}`.
    pub fn as_str(self) -> &'static str {
        match self {
            Degenerate::Repetition => detector::REPETITION,
            Degenerate::Thinking => detector::THINKING,
        }
    }

    /// Обратно из имени ([`Degenerate::as_str`]).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            detector::REPETITION => Some(Degenerate::Repetition),
            detector::THINKING => Some(Degenerate::Thinking),
            _ => None,
        }
    }
}

/// Рассуждение дольше этого без текста и вызовов — обрыв (D59).
pub const THINKING_TIMEOUT: Duration = Duration::from_secs(300);
/// Проверка повтора — после стольких байт текста с прошлой проверки (D58).
pub const CHECK_BYTES: usize = 512;
/// Проверка по `\n` — когда текст попытки не короче стольких символов (4 × 20, D58).
pub const CHECK_MIN_CHARS: usize = 80;
/// Минимальная длина повторённой строки, символы (D58).
pub const MIN_LINE_CHARS: usize = 20;
/// Сколько раз подряд (строка) или в окне (n-грамма) — срабатывание (D58).
pub const REPEATS: usize = 4;
/// Длина n-граммы, слова (D58).
pub const NGRAM_WORDS: usize = 12;
/// Окно n-грамм — последние слова текста (D58).
pub const NGRAM_WINDOW: usize = 600;
/// Самая длинная строка, повтор которой ищется в конце текста, символы: предел
/// работы одной проверки. Длиннее — повтор ловит n-грамма.
const MAX_LINE_CHARS: usize = 2048;
/// Хвост текста попытки, который хранит детектор, байты: больше обоих окон проверки.
const KEEP_BYTES: usize = 64 * 1024;

/// Наблюдатель стрима одной попытки хода (D58, D59).
#[derive(Debug, Clone)]
pub struct StreamWatch {
    /// Хвост текста попытки (не больше `2 × KEEP_BYTES`).
    text: String,
    /// Символов текста попытки всего (порог проверки по `\n`).
    text_chars: usize,
    /// Байт текста с прошлой проверки повтора.
    since_check: usize,
    /// Байт рассуждения попытки.
    reasoning_bytes: u64,
    /// Начало рассуждения попытки (первая дельта рассуждения).
    reasoning_since: Option<Instant>,
    /// В попытке был текст или вызов: тайм-аут рассуждения снят.
    acted: bool,
    /// Предел байт рассуждения: бюджет × 1,5 × 4 (D59); 0 — без предела.
    reasoning_limit: u64,
}

impl StreamWatch {
    /// Бюджет рассуждения — [`thinking_budget`] режима хода (D40, D59). У режима без
    /// бюджета (`Off`) предела байт нет, остаётся тайм-аут: модель, которая не умеет
    /// выключать мышление, иначе обрывалась бы на первой дельте и в повторе.
    pub fn new(reasoning: &ReasoningMode) -> StreamWatch {
        StreamWatch {
            text: String::new(),
            text_chars: 0,
            since_check: 0,
            reasoning_bytes: 0,
            reasoning_since: None,
            acted: false,
            reasoning_limit: u64::from(thinking_budget(reasoning)) * 6,
        }
    }

    /// Дельта текста (D58): снимает тайм-аут рассуждения; проверка повтора — после
    /// [`CHECK_BYTES`] байт с прошлой проверки или по `\n` при тексте не короче
    /// [`CHECK_MIN_CHARS`] символов.
    pub fn on_text(&mut self, delta: &str) -> Option<Degenerate> {
        if delta.is_empty() {
            return None;
        }
        self.acted = true;
        self.text.push_str(delta);
        self.text_chars += delta.chars().count();
        self.since_check += delta.len();
        self.trim();
        let due = self.since_check >= CHECK_BYTES
            || (delta.contains('\n') && self.text_chars >= CHECK_MIN_CHARS);
        if !due {
            return None;
        }
        self.since_check = 0;
        (line_repeated(&self.text) || ngram_repeated(&self.text)).then_some(Degenerate::Repetition)
    }

    /// Дельта рассуждения в момент `now` (D59): предел байт или тайм-аут без текста и
    /// вызовов.
    pub fn on_reasoning(&mut self, delta: &str, now: Instant) -> Option<Degenerate> {
        if delta.is_empty() {
            return None;
        }
        self.reasoning_since.get_or_insert(now);
        self.reasoning_bytes += delta.len() as u64;
        if self.reasoning_limit > 0 && self.reasoning_bytes > self.reasoning_limit {
            return Some(Degenerate::Thinking);
        }
        match self.deadline() {
            Some(d) if now >= d => Some(Degenerate::Thinking),
            _ => None,
        }
    }

    /// Вызов инструмента в стриме снимает тайм-аут рассуждения.
    pub fn on_calls(&mut self) {
        self.acted = true;
    }

    /// Начало рассуждения + [`THINKING_TIMEOUT`], пока нет текста и вызовов; иначе None.
    pub fn deadline(&self) -> Option<Instant> {
        if self.acted {
            return None;
        }
        self.reasoning_since.map(|t| t + THINKING_TIMEOUT)
    }

    /// Перезапуск стрима (MDL-018): накопленное прежней попыткой отбрасывается.
    pub fn on_restart(&mut self) {
        let limit = self.reasoning_limit;
        *self = StreamWatch {
            text: String::new(),
            text_chars: 0,
            since_check: 0,
            reasoning_bytes: 0,
            reasoning_since: None,
            acted: false,
            reasoning_limit: limit,
        };
    }

    /// Хвост текста не растёт без предела: окна проверки много меньше `KEEP_BYTES`.
    fn trim(&mut self) {
        if self.text.len() <= 2 * KEEP_BYTES {
            return;
        }
        let mut cut = self.text.len() - KEEP_BYTES;
        while !self.text.is_char_boundary(cut) {
            cut += 1;
        }
        self.text.drain(..cut);
    }
}

/// D58: текст кончается [`REPEATS`] копиями строки от [`MIN_LINE_CHARS`] символов.
/// Строка — неделимая: строка из повторов более короткой (`====`, `|---`) повтором
/// длинной строки не считается.
fn line_repeated(text: &str) -> bool {
    let mut tail: Vec<char> = text.chars().rev().take(REPEATS * MAX_LINE_CHARS).collect();
    tail.reverse();
    let n = tail.len();
    let max = (n / REPEATS).min(MAX_LINE_CHARS);
    (MIN_LINE_CHARS..=max).any(|p| {
        let periodic = (0..p * (REPEATS - 1)).all(|k| tail[n - 1 - k] == tail[n - 1 - k - p]);
        periodic && root_len(&tail[n - p..]) >= MIN_LINE_CHARS
    })
}

/// Длина наименьшего корня строки: `u = r^k` с наибольшим `k`.
fn root_len(u: &[char]) -> usize {
    let p = u.len();
    (1..p)
        .filter(|d| p.is_multiple_of(*d))
        .find(|&d| (d..p).all(|i| u[i] == u[i - d]))
        .unwrap_or(p)
}

/// D58: [`NGRAM_WORDS`]-словная n-грамма встречается [`REPEATS`] раза в последних
/// [`NGRAM_WINDOW`] словах (вхождения могут перекрываться).
fn ngram_repeated(text: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    let window = &words[words.len().saturating_sub(NGRAM_WINDOW)..];
    let mut counts: HashMap<&[&str], usize> = HashMap::new();
    for g in window.windows(NGRAM_WORDS) {
        let c = counts.entry(g).or_insert(0);
        *c += 1;
        if *c >= REPEATS {
            return true;
        }
    }
    false
}

/// D58, D59: параметры повтора хода. Повтор текста — `temperature = max(t − 0,3, 0,1)`,
/// округлённая до сотых (без значения — 0,3), и `presence_penalty = 0,5`; рассуждение —
/// те же параметры выборки и `ReasoningMode::Off`.
pub fn retry_params(d: Degenerate, sampling: &Sampling) -> (Sampling, Option<ReasoningMode>) {
    match d {
        Degenerate::Repetition => {
            let t = sampling.temperature.map_or(0.3, |t| (t - 0.3).max(0.1));
            let t = (t * 100.0).round() / 100.0;
            let s = Sampling {
                temperature: Some(t),
                presence_penalty: Some(0.5),
                ..sampling.clone()
            };
            (s, None)
        }
        Degenerate::Thinking => (sampling.clone(), Some(ReasoningMode::Off)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_repeated_needs_primitive_long_line() {
        let line = "abcdefghij0123456789\n";
        assert!(line_repeated(&line.repeat(4)));
        assert!(!line_repeated(&line.repeat(3)));
        // Разделители из коротких повторов — не строка от 20 символов.
        assert!(!line_repeated(&"=".repeat(400)));
        assert!(!line_repeated(&"|---".repeat(100)));
        assert_eq!(root_len(&"abab".chars().collect::<Vec<_>>()), 2);
        assert_eq!(root_len(&"abc".chars().collect::<Vec<_>>()), 3);
    }

    #[test]
    fn restart_drops_attempt() {
        let mut w = StreamWatch::new(&ReasoningMode::Default);
        let t0 = Instant::now();
        assert_eq!(w.on_reasoning("x", t0), None);
        assert!(w.deadline().is_some());
        w.on_restart();
        assert_eq!(w.deadline(), None);
        assert_eq!(w.reasoning_limit, 16_384 * 6);
    }

    #[test]
    fn degenerate_names_round_trip() {
        for d in [Degenerate::Repetition, Degenerate::Thinking] {
            assert_eq!(Degenerate::parse(d.as_str()), Some(d));
        }
        assert_eq!(Degenerate::parse("pause"), None);
    }
}
