//! Детекторы стрима (R1-B2 D58, D59): повтор текста и бесконечное рассуждение.
//!
//! [`StreamWatch`] смотрит дельты одной попытки хода. Повтор текста (D58, Ruling I-1
//! ревью задачи 7) проверяется, когда с прошлой проверки накоплено [`CHECK_BYTES`] байт
//! текста или дельта содержит `\n` и текст попытки не короче [`CHECK_MIN_CHARS`]
//! символов. Вырождение — только когда хвост текста непрерывно состоит из повторов одной
//! единицы: единица содержит не меньше [`MIN_UNIT_WORDS`] разных слов (буквенно-цифровых
//! токенов с буквой) и [`MIN_UNIT_CHARS`] непробельных символов, повторена не меньше
//! [`MIN_COPIES`] раз подряд без перекрытия, а повторённая часть — не меньше
//! [`MIN_REPEAT_BYTES`] байт. Строка из одних пробелов, пунктуации и цифр (матрица нулей,
//! рамка, разделитель, пустая строка таблицы) единицей не считается. Второе правило —
//! [`NGRAM_WORDS`]-словная n-грамма с теми же требованиями к единице встречается не меньше
//! [`MIN_COPIES`] раз без перекрытия в последних [`NGRAM_WINDOW`] словах, и от первого
//! вхождения до конца текста не меньше [`MIN_REPEAT_BYTES`] байт: петля с вариациями.
//!
//! Бесконечное рассуждение (D59, Ruling I-2): байты рассуждения / 4 больше бюджета × 1,5
//! (основной предел) или рассуждение дольше дедлайна без текста и вызовов. Дедлайн —
//! [`thinking_timeout`]: `max(300 с, бюджет / max(tps, 10) × 1,5)`. Он стоит в `select!`
//! стрима (`turn.rs`), поэтому замолчавший стрим тоже обрывается. Время передаётся явно
//! (`tokio::time::Instant`, как у `DeltaBuffer`), детектор детерминирован.
//!
//! Реакция — обрыв стрима с квитанцией `interrupted:repetition|thinking` и повтор хода
//! в том же шаге с параметрами [`retry_params`]; второе срабатывание в шаге — пауза
//! `model_loop` (решает драйвер).

use std::collections::{HashMap, HashSet};
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

/// Нижняя граница дедлайна рассуждения без текста и вызовов (D59).
pub const THINKING_TIMEOUT: Duration = Duration::from_secs(300);
/// Скорость вывода, если наблюдённой нет и она ниже этой, ток/с (Ruling I-2).
pub const MIN_TPS: f64 = 10.0;
/// Запас дедлайна над временем вывода бюджета (Ruling I-2).
const TIMEOUT_SLACK: f64 = 1.5;
/// Проверка повтора — после стольких байт текста с прошлой проверки (D58).
pub const CHECK_BYTES: usize = 512;
/// Проверка по `\n` — когда текст попытки не короче стольких символов (D58).
pub const CHECK_MIN_CHARS: usize = 80;
/// Разных слов в единице повтора, не меньше (Ruling I-1).
pub const MIN_UNIT_WORDS: usize = 3;
/// Непробельных символов в единице повтора, не меньше (Ruling I-1).
pub const MIN_UNIT_CHARS: usize = 20;
/// Копий единицы подряд (или вхождений n-граммы) без перекрытия, не меньше (Ruling I-1).
pub const MIN_COPIES: usize = 8;
/// Повторённая часть текста, байты, не меньше (Ruling I-1).
pub const MIN_REPEAT_BYTES: usize = 2048;
/// Длина n-граммы, слова (D58).
pub const NGRAM_WORDS: usize = 12;
/// Окно n-грамм — последние слова текста (D58).
pub const NGRAM_WINDOW: usize = 600;
/// Самая длинная единица повтора, символы: предел работы одной проверки. Длиннее —
/// повтор ловит n-грамма.
const MAX_UNIT_CHARS: usize = 2048;
/// Хвост текста попытки, который хранит детектор, байты: больше обоих окон проверки.
const KEEP_BYTES: usize = 64 * 1024;

/// D59, Ruling I-2: дедлайн рассуждения `max(300 с, budget_tokens / max(tps, 10) × 1,5)`.
/// `tps` — наблюдённая скорость вывода run (`None` или не число — 10 ток/с).
pub fn thinking_timeout(budget_tokens: u32, tps: Option<f64>) -> Duration {
    let tps = tps
        .filter(|t| t.is_finite())
        .unwrap_or(MIN_TPS)
        .max(MIN_TPS);
    let secs = f64::from(budget_tokens) / tps * TIMEOUT_SLACK;
    THINKING_TIMEOUT.max(Duration::from_secs_f64(secs))
}

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
    /// Дедлайн рассуждения от его начала (D59).
    timeout: Duration,
}

impl StreamWatch {
    /// Бюджет рассуждения — [`thinking_budget`] режима хода (D40, D59). У режима без
    /// бюджета (`Off`) предела байт нет, остаётся тайм-аут: модель, которая не умеет
    /// выключать мышление, иначе обрывалась бы на первой дельте и в повторе. Дедлайн —
    /// [`THINKING_TIMEOUT`]; по бюджету и скорости — [`StreamWatch::with_timeout`].
    pub fn new(reasoning: &ReasoningMode) -> StreamWatch {
        StreamWatch {
            text: String::new(),
            text_chars: 0,
            since_check: 0,
            reasoning_bytes: 0,
            reasoning_since: None,
            acted: false,
            reasoning_limit: u64::from(thinking_budget(reasoning)) * 6,
            timeout: THINKING_TIMEOUT,
        }
    }

    /// Дедлайн рассуждения от его начала ([`thinking_timeout`]).
    pub fn with_timeout(mut self, timeout: Duration) -> StreamWatch {
        self.timeout = timeout;
        self
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
        if !due || self.text.len() < MIN_REPEAT_BYTES {
            return None;
        }
        self.since_check = 0;
        (tail_repeated(&self.text) || ngram_repeated(&self.text)).then_some(Degenerate::Repetition)
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

    /// Начало рассуждения + дедлайн, пока нет текста и вызовов; иначе None.
    pub fn deadline(&self) -> Option<Instant> {
        if self.acted {
            return None;
        }
        self.reasoning_since.map(|t| t + self.timeout)
    }

    /// Перезапуск стрима (MDL-018): накопленное прежней попыткой отбрасывается.
    pub fn on_restart(&mut self) {
        self.text.clear();
        self.text_chars = 0;
        self.since_check = 0;
        self.reasoning_bytes = 0;
        self.reasoning_since = None;
        self.acted = false;
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

/// Единица повтора годится (Ruling I-1): не меньше [`MIN_UNIT_WORDS`] разных слов и
/// [`MIN_UNIT_CHARS`] непробельных символов. Слово — буквенно-цифровой токен с буквой:
/// строка из пробелов, пунктуации и цифр слов не содержит.
fn good_unit<'a>(parts: impl IntoIterator<Item = &'a str>) -> bool {
    let mut words: HashSet<&str> = HashSet::new();
    let mut chars = 0usize;
    for part in parts {
        chars += part.chars().filter(|c| !c.is_whitespace()).count();
        words.extend(
            part.split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.chars().any(char::is_alphabetic)),
        );
    }
    words.len() >= MIN_UNIT_WORDS && chars >= MIN_UNIT_CHARS
}

/// D58, Ruling I-1: хвост текста — не меньше [`MIN_COPIES`] копий одной годной
/// единицы подряд, повторённая часть не меньше [`MIN_REPEAT_BYTES`] байт.
fn tail_repeated(text: &str) -> bool {
    let mut tail: Vec<char> = text
        .chars()
        .rev()
        .take(MIN_COPIES * MAX_UNIT_CHARS)
        .collect();
    tail.reverse();
    let n = tail.len();
    // suffix_bytes[l] — байты последних l символов.
    let mut suffix_bytes = Vec::with_capacity(n + 1);
    suffix_bytes.push(0usize);
    for c in tail.iter().rev() {
        let last = suffix_bytes.last().copied().unwrap_or(0);
        suffix_bytes.push(last + c.len_utf8());
    }
    // suffix_letters[l] — букв среди последних l символов: дешёвый отсев единиц без слов.
    let mut suffix_letters = Vec::with_capacity(n + 1);
    suffix_letters.push(0usize);
    for c in tail.iter().rev() {
        let last = suffix_letters.last().copied().unwrap_or(0);
        suffix_letters.push(last + usize::from(c.is_alphabetic()));
    }
    let max = (n / MIN_COPIES).min(MAX_UNIT_CHARS);
    (1..=max).any(|p| {
        if suffix_letters[p] < MIN_UNIT_WORDS {
            return false;
        }
        // Нужная длина повторённой части: кратна p, не меньше 8 копий и 2 КиБ.
        let mut need = MIN_COPIES * p;
        while need <= n && suffix_bytes[need] < MIN_REPEAT_BYTES {
            need += p;
        }
        if need > n {
            return false;
        }
        let periodic = (0..need - p).all(|k| tail[n - 1 - k] == tail[n - 1 - k - p]);
        periodic && good_cyclic_unit(&tail[n - p..])
    })
}

/// [`good_unit`] для единицы периода: конец текста режет её в произвольном месте
/// (`ln!("--");\n    print`), поэтому слова считаются по кругу — в двух копиях подряд без
/// обрезков слов на краях. Иначе обрезки дают лишние «слова» (`ln`, `print`).
fn good_cyclic_unit(unit: &[char]) -> bool {
    let doubled: String = unit.iter().chain(unit).collect();
    let mut words: Vec<&str> = doubled
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    if unit.last().is_some_and(|c| c.is_alphanumeric()) {
        words.pop();
    }
    if unit.first().is_some_and(|c| c.is_alphanumeric()) && !words.is_empty() {
        words.remove(0);
    }
    let distinct: HashSet<&str> = words
        .into_iter()
        .filter(|w| w.chars().any(char::is_alphabetic))
        .collect();
    let chars = unit.iter().filter(|c| !c.is_whitespace()).count();
    distinct.len() >= MIN_UNIT_WORDS && chars >= MIN_UNIT_CHARS
}

/// D58, Ruling I-1: годная [`NGRAM_WORDS`]-словная n-грамма встречается не меньше
/// [`MIN_COPIES`] раз без перекрытия в последних [`NGRAM_WINDOW`] словах, от первого
/// вхождения до конца текста — не меньше [`MIN_REPEAT_BYTES`] байт.
fn ngram_repeated(text: &str) -> bool {
    let mut words: Vec<&str> = text.split_whitespace().rev().take(NGRAM_WINDOW).collect();
    words.reverse();
    // Начало слова в байтах текста: слово — подстрока `text`.
    let base = text.as_ptr() as usize;
    let start = |w: &str| w.as_ptr() as usize - base;
    struct Seen {
        good: bool,
        count: usize,
        /// Первый индекс слова, с которого вхождение не перекрывает прошлое.
        free: usize,
        first: usize,
    }
    let mut seen: HashMap<&[&str], Seen> = HashMap::new();
    for (i, g) in words.windows(NGRAM_WORDS).enumerate() {
        let s = seen.entry(g).or_insert_with(|| Seen {
            good: good_unit(g.iter().copied()),
            count: 0,
            free: 0,
            first: start(g[0]),
        });
        if !s.good || i < s.free {
            continue;
        }
        s.count += 1;
        s.free = i + NGRAM_WORDS;
        if s.count >= MIN_COPIES && text.len() - s.first >= MIN_REPEAT_BYTES {
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
    fn unit_needs_words_and_chars() {
        assert!(good_unit(["Проверяю файл config.toml ещё раз\n"]));
        assert!(!good_unit(["    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],\n"]));
        assert!(!good_unit(["|                              |\n"]));
        assert!(!good_unit(["    println!(\"--------\");\n"]), "одно слово");
        assert!(!good_unit(["a b c"]), "меньше 20 непробельных символов");
        // Единица периода, разрезанная концом текста: слова по кругу.
        let cut: Vec<char> = "ln!(\"--------\");\n    print".chars().collect();
        assert!(!good_cyclic_unit(&cut), "обрезки слова — не слова");
        let rotated: Vec<char> = "config.toml ещё раз\nПроверяю файл ".chars().collect();
        assert!(good_cyclic_unit(&rotated));
    }

    #[test]
    fn tail_needs_copies_and_bytes() {
        let line = "Проверяю файл config.toml ещё раз\n"; // 52 байта
        assert!(!tail_repeated(&line.repeat(39)), "39 × 52 < 2048");
        assert!(tail_repeated(&line.repeat(40)));
        // 8 копий длинной строки: 8 × 300 > 2048.
        let long = format!("{}\n", "word другое слово ".repeat(12));
        assert!(!tail_repeated(&long.repeat(7)));
        assert!(tail_repeated(&long.repeat(8)));
        assert!(!tail_repeated(&"=".repeat(10_000)));
    }

    #[test]
    fn timeout_by_budget_and_speed() {
        assert_eq!(thinking_timeout(1024, None), THINKING_TIMEOUT);
        // 2048 / 10 × 1,5 = 307,2 с: больше нижней границы.
        assert_eq!(thinking_timeout(2048, None), Duration::from_secs_f64(307.2));
        // 16384 / 10 × 1,5 = 2457,6 с.
        assert_eq!(
            thinking_timeout(16_384, None),
            Duration::from_secs_f64(2457.6)
        );
        // 16384 / 32 × 1,5 = 768 с; скорость ниже 10 считается как 10.
        assert_eq!(
            thinking_timeout(16_384, Some(32.0)),
            Duration::from_secs(768)
        );
        assert_eq!(
            thinking_timeout(16_384, Some(2.0)),
            thinking_timeout(16_384, None)
        );
        assert_eq!(
            thinking_timeout(16_384, Some(f64::NAN)),
            thinking_timeout(16_384, None)
        );
        assert_eq!(thinking_timeout(16_384, Some(1000.0)), THINKING_TIMEOUT);
    }

    #[test]
    fn restart_drops_attempt() {
        let t0 = Instant::now();
        let mut w =
            StreamWatch::new(&ReasoningMode::Default).with_timeout(Duration::from_secs(900));
        assert_eq!(w.on_reasoning("x", t0), None);
        assert_eq!(w.deadline(), Some(t0 + Duration::from_secs(900)));
        w.on_restart();
        assert_eq!(w.deadline(), None);
        assert_eq!(w.reasoning_limit, 16_384 * 6);
        assert_eq!(
            w.timeout,
            Duration::from_secs(900),
            "дедлайн попытки сохраняется"
        );
    }

    #[test]
    fn degenerate_names_round_trip() {
        for d in [Degenerate::Repetition, Degenerate::Thinking] {
            assert_eq!(Degenerate::parse(d.as_str()), Some(d));
        }
        assert_eq!(Degenerate::parse("pause"), None);
    }
}
