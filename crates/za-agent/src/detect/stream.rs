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
//! N-грамма срабатывает только на однообразном хвосте (Ruling I-1, раунд 2): в последних
//! [`MONOTONY_BYTES`] байтах доля разных n-грамм меньше [`MONOTONY_SHARE`], и последнее
//! вхождение кончается в последних [`RECENT_BYTES`] байтах — лог с меняющимся номером и
//! список с разными элементами разнообразны. Внутри открытого блока кода (нечётное число
//! ```` ``` ````) n-граммы не действуют, а правилу единицы нужно [`CODE_MIN_COPIES`] копий и
//! [`CODE_MIN_REPEAT_BYTES`] байт: повторяющиеся данные (JSON, `INSERT`, лог) не петля.
//!
//! Бесконечное рассуждение (D59, Ruling I-2): символы рассуждения / 4 больше бюджета × 1,5
//! (основной предел; символы, а не байты — m4 финального ревью R1-B2) или рассуждение дольше дедлайна без текста и вызовов. Дедлайн —
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
/// Доля разных n-грамм в последних [`MONOTONY_BYTES`] байтах, ниже которой хвост
/// однообразен (Ruling I-1, раунд 2).
pub const MONOTONY_SHARE: f64 = 0.25;
/// Окно однообразия n-грамм — последние байты текста (Ruling I-1, раунд 2).
pub const MONOTONY_BYTES: usize = 2048;
/// Последнее вхождение повторённой n-граммы кончается в стольких последних байтах
/// (Ruling I-1, раунд 2).
pub const RECENT_BYTES: usize = 256;
/// Копий единицы подряд внутри открытого блока кода, не меньше (Ruling I-1, раунд 2).
pub const CODE_MIN_COPIES: usize = 32;
/// Повторённая часть внутри открытого блока кода, байты, не меньше (Ruling I-1, раунд 2).
pub const CODE_MIN_REPEAT_BYTES: usize = 8 * 1024;
/// Самая длинная единица повтора, символы: предел работы одной проверки. Единица длиннее
/// не ловится (принятый риск, Ruling I-1 раунда 2): n-грамме нужно 8 вхождений в
/// [`NGRAM_WINDOW`] словах, она видит единицы не длиннее ≈ 75 слов.
const MAX_UNIT_CHARS: usize = 2048;
/// Ограждение блока кода Markdown.
const FENCE: &str = "```";
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
    /// Символов рассуждения попытки.
    reasoning_chars: u64,
    /// Начало рассуждения попытки (первая дельта рассуждения).
    reasoning_since: Option<Instant>,
    /// В попытке был текст или вызов: тайм-аут рассуждения снят.
    acted: bool,
    /// Предел символов рассуждения: бюджет × 1,5 × 4 (D59; символы, а не байты — m4
    /// финального ревью R1-B2: у кириллицы 2 байта на символ, байтовый предел рвал бы
    /// законное рассуждение на русском около 0,9–1,2 бюджета); 0 — без предела.
    reasoning_limit: u64,
    /// Дедлайн рассуждения от его начала (D59).
    timeout: Duration,
    /// Ограждений блока кода в отброшенном начале текста (`trim`): чётность — открыт ли
    /// блок кода в конце текста.
    dropped_fences: usize,
}

impl StreamWatch {
    /// Бюджет рассуждения — [`thinking_budget`] режима хода (D40, D59). У режима без
    /// бюджета (`Off`) предела символов нет, остаётся тайм-аут: модель, которая не умеет
    /// выключать мышление, иначе обрывалась бы на первой дельте и в повторе. Дедлайн —
    /// [`THINKING_TIMEOUT`]; по бюджету и скорости — [`StreamWatch::with_timeout`].
    pub fn new(reasoning: &ReasoningMode) -> StreamWatch {
        StreamWatch {
            text: String::new(),
            text_chars: 0,
            since_check: 0,
            reasoning_chars: 0,
            reasoning_since: None,
            acted: false,
            reasoning_limit: u64::from(thinking_budget(reasoning)) * 6,
            timeout: THINKING_TIMEOUT,
            dropped_fences: 0,
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
        let in_code = (self.dropped_fences + self.text.matches(FENCE).count()) % 2 == 1;
        let hit = if in_code {
            tail_repeated(&self.text, CODE_MIN_COPIES, CODE_MIN_REPEAT_BYTES)
        } else {
            // N-граммы — только по тексту после закрытого блока кода: его содержимое
            // (данные, логи) — не петля и после закрытия.
            let prose = self
                .text
                .rfind(FENCE)
                .map_or(self.text.as_str(), |i| &self.text[i + FENCE.len()..]);
            tail_repeated(&self.text, MIN_COPIES, MIN_REPEAT_BYTES) || ngram_repeated(prose)
        };
        hit.then_some(Degenerate::Repetition)
    }

    /// Дельта рассуждения в момент `now` (D59): предел символов или тайм-аут без текста и
    /// вызовов.
    pub fn on_reasoning(&mut self, delta: &str, now: Instant) -> Option<Degenerate> {
        if delta.is_empty() {
            return None;
        }
        self.reasoning_since.get_or_insert(now);
        self.reasoning_chars += delta.chars().count() as u64;
        if self.reasoning_limit > 0 && self.reasoning_chars > self.reasoning_limit {
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
        self.reasoning_chars = 0;
        self.reasoning_since = None;
        self.acted = false;
        self.dropped_fences = 0;
    }

    /// Хвост текста не растёт без предела: окна проверки много меньше `KEEP_BYTES`.
    fn trim(&mut self) {
        if self.text.len() <= 2 * KEEP_BYTES {
            return;
        }
        let mut cut = self.text.len() - KEEP_BYTES;
        // Разрез не попадает внутрь ограждения: серия обратных кавычек уходит целиком.
        while !self.text.is_char_boundary(cut) || self.text.as_bytes()[cut] == b'`' {
            cut += 1;
        }
        self.dropped_fences += self.text[..cut].matches(FENCE).count();
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

/// D58, Ruling I-1: хвост текста — не меньше `min_copies` копий одной годной единицы
/// подряд, повторённая часть не меньше `min_bytes` байт ([`MIN_COPIES`] и
/// [`MIN_REPEAT_BYTES`]; в блоке кода — [`CODE_MIN_COPIES`] и [`CODE_MIN_REPEAT_BYTES`]).
fn tail_repeated(text: &str, min_copies: usize, min_bytes: usize) -> bool {
    // r — хвост текста задом наперёд: суффикс текста длины l — префикс r длины l.
    let r: Vec<char> = text
        .chars()
        .rev()
        .take(min_copies * MAX_UNIT_CHARS)
        .collect();
    let n = r.len();
    // Префикс-функция r (KMP): у префикса длины l наименьший период l − pi[l − 1].
    let mut pi = vec![0usize; n];
    for i in 1..n {
        let mut k = pi[i - 1];
        while k > 0 && r[i] != r[k] {
            k = pi[k - 1];
        }
        if r[i] == r[k] {
            k += 1;
        }
        pi[i] = k;
    }
    // Байты, буквы и непробельные символы последних l символов текста.
    let prefix = |f: &dyn Fn(char) -> usize| {
        let mut v = Vec::with_capacity(n + 1);
        v.push(0usize);
        for &c in &r {
            let last = v.last().copied().unwrap_or(0);
            v.push(last + f(c));
        }
        v
    };
    let bytes = prefix(&|c| c.len_utf8());
    let letters = prefix(&|c| usize::from(c.is_alphabetic()));
    let solid = prefix(&|c| usize::from(!c.is_whitespace()));
    let mut words_ok: HashMap<usize, bool> = HashMap::new();
    let max = (n / min_copies).min(MAX_UNIT_CHARS);
    (1..=max).any(|p| {
        if letters[p] < MIN_UNIT_WORDS || solid[p] < MIN_UNIT_CHARS {
            return false;
        }
        // Нужная длина повторённой части: кратна p, не меньше min_copies копий и min_bytes.
        let mut need = min_copies * p;
        while need <= n && bytes[need] < min_bytes {
            need += p;
        }
        if need > n {
            return false;
        }
        // Период p у суффикса длины need ⇔ p кратно наименьшему периоду q (Файн — Вильф:
        // p + q ≤ need). Тогда единица — степень корня длины q, слова у них по кругу те же.
        let q = need - pi[need - 1];
        if !p.is_multiple_of(q) {
            return false;
        }
        *words_ok.entry(q).or_insert_with(|| {
            let root: Vec<char> = r[..q].iter().rev().copied().collect();
            cyclic_words_ok(&root)
        })
    })
}

/// Слова единицы периода (Ruling I-1): конец текста режет её в произвольном месте
/// (`ln!("--");\n    print`), поэтому слова считаются по кругу — в двух копиях подряд без
/// обрезков слов на краях. Иначе обрезки дают лишние «слова» (`ln`, `print`).
fn cyclic_words_ok(unit: &[char]) -> bool {
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
    distinct.len() >= MIN_UNIT_WORDS
}

/// D58, Ruling I-1: годная [`NGRAM_WORDS`]-словная n-грамма встречается не меньше
/// [`MIN_COPIES`] раз без перекрытия в последних [`NGRAM_WINDOW`] словах, от первого
/// вхождения до конца текста — не меньше [`MIN_REPEAT_BYTES`] байт. Только на
/// однообразном хвосте (Ruling I-1, раунд 2): доля разных n-грамм в последних
/// [`MONOTONY_BYTES`] байтах меньше [`MONOTONY_SHARE`], последнее вхождение кончается в
/// последних [`RECENT_BYTES`] байтах.
fn ngram_repeated(text: &str) -> bool {
    let mut words: Vec<&str> = text.split_whitespace().rev().take(NGRAM_WINDOW).collect();
    words.reverse();
    // Начало и конец слова в байтах текста: слово — подстрока `text`.
    let base = text.as_ptr() as usize;
    let start = |w: &str| w.as_ptr() as usize - base;
    let end = |w: &str| start(w) + w.len();
    let len = text.len();
    if !monotonous(&words, len, start) {
        return false;
    }
    struct Seen {
        good: bool,
        count: usize,
        /// Первый индекс слова, с которого вхождение не перекрывает прошлое.
        free: usize,
        first: usize,
        /// Конец последнего засчитанного вхождения, байты.
        last_end: usize,
    }
    let mut seen: HashMap<&[&str], Seen> = HashMap::new();
    for (i, g) in words.windows(NGRAM_WORDS).enumerate() {
        let s = seen.entry(g).or_insert_with(|| Seen {
            good: good_unit(g.iter().copied()),
            count: 0,
            free: 0,
            first: start(g[0]),
            last_end: 0,
        });
        if !s.good || i < s.free {
            continue;
        }
        s.count += 1;
        s.free = i + NGRAM_WORDS;
        s.last_end = end(g[NGRAM_WORDS - 1]);
    }
    seen.values().any(|s| {
        s.count >= MIN_COPIES
            && len - s.first >= MIN_REPEAT_BYTES
            && len - s.last_end <= RECENT_BYTES
    })
}

/// Хвост однообразен (Ruling I-1, раунд 2): среди n-грамм, начинающихся в последних
/// [`MONOTONY_BYTES`] байтах, разных меньше [`MONOTONY_SHARE`]. `words` — слова окна,
/// `start` — их смещение в тексте длины `len`.
fn monotonous(words: &[&str], len: usize, start: impl Fn(&str) -> usize) -> bool {
    let from = words.partition_point(|w| start(w) + MONOTONY_BYTES < len);
    let tail = &words[from..];
    if tail.len() < NGRAM_WORDS {
        return false;
    }
    let grams: Vec<&[&str]> = tail.windows(NGRAM_WORDS).collect();
    let distinct: HashSet<&[&str]> = grams.iter().copied().collect();
    (distinct.len() as f64) < MONOTONY_SHARE * grams.len() as f64
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
        assert!(!cyclic_words_ok(&cut), "обрезки слова — не слова");
        let rotated: Vec<char> = "config.toml ещё раз\nПроверяю файл ".chars().collect();
        assert!(cyclic_words_ok(&rotated));
    }

    #[test]
    fn tail_needs_copies_and_bytes() {
        let line = "Проверяю файл config.toml ещё раз\n"; // 52 байта
        assert!(
            !tail_repeated(&line.repeat(39), MIN_COPIES, MIN_REPEAT_BYTES),
            "39 × 52 < 2048"
        );
        assert!(tail_repeated(
            &line.repeat(40),
            MIN_COPIES,
            MIN_REPEAT_BYTES
        ));
        // 8 копий длинной строки: 8 × 300 > 2048.
        let long = format!("{}\n", "word другое слово ".repeat(12));
        assert!(!tail_repeated(
            &long.repeat(7),
            MIN_COPIES,
            MIN_REPEAT_BYTES
        ));
        assert!(tail_repeated(&long.repeat(8), MIN_COPIES, MIN_REPEAT_BYTES));
        assert!(!tail_repeated(
            &"=".repeat(10_000),
            MIN_COPIES,
            MIN_REPEAT_BYTES
        ));
    }

    /// Ruling I-1, раунд 2: n-грамма — только на однообразном хвосте, с повтором у конца.
    #[test]
    fn ngram_needs_monotonous_recent_tail() {
        // ABAB: однообразно, повтор до конца.
        let abab = "Проверяю файл.\nФайл в порядке.\n".repeat(120);
        assert!(ngram_repeated(&abab));
        // Лог с меняющимся номером: каждая строка — свои n-граммы.
        let log: String = (0..80)
            .map(|i| {
                format!(
                    "[INFO] za_agent::driver: model turn finished successfully, waiting for the next boundary of the run step={i}\n"
                )
            })
            .collect();
        assert!(!ngram_repeated(&log));
        // Петля, за которой идёт другой текст: последнее вхождение не у конца.
        let after: String = (0..60).map(|i| format!("w{i} x{i} y{i} z{i} ")).collect();
        assert!(!ngram_repeated(&format!("{abab}{after}")));
    }

    #[test]
    fn code_block_raises_unit_threshold() {
        let line = "INSERT INTO users (name, role) VALUES ('guest', 'viewer');\n";
        let mut w = StreamWatch::new(&ReasoningMode::Default);
        assert_eq!(w.on_text("```sql\n"), None);
        // 40 копий (≈ 2,4 КиБ) в блоке кода — не петля; 32 копии и 8 КиБ — петля.
        let hit = (1..=200).find_map(|i| w.on_text(line).map(|d| (i, d)));
        let (i, d) = hit.expect("петля в коде ловится");
        assert_eq!(d, Degenerate::Repetition);
        assert!(
            i * line.len() >= CODE_MIN_REPEAT_BYTES && i >= CODE_MIN_COPIES,
            "{i}"
        );
        // Вне блока кода та же строка — петля с 2 КиБ.
        let mut w = StreamWatch::new(&ReasoningMode::Default);
        let i = (1..=200)
            .find(|_| w.on_text(line).is_some())
            .expect("петля");
        assert!(i * line.len() < 3 * 1024, "{i}");
    }

    #[test]
    fn trim_keeps_fence_parity() {
        let mut w = StreamWatch::new(&ReasoningMode::Default);
        w.on_text("```\n");
        let filler: String = (0..800).map(|i| format!("w{i} ")).collect();
        for _ in 0..(3 * KEEP_BYTES / filler.len()) {
            w.on_text(&filler);
        }
        assert!(!w.text.contains(FENCE), "ограждение отброшено");
        assert_eq!(w.dropped_fences, 1);
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
