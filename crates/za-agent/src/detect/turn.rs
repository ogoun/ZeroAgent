//! Детекторы хода (R1-B2 D57, D60–D62): обещание без действия, цикл ошибок
//! инструмента, порча JSON подряд, падение кэша.
//!
//! Вход каждого детектора — данные принятого хода (текст ответа, результаты пакета,
//! число неразобранных вызовов, расход и хэши сообщений запроса). Внешних чтений нет
//! (D30), состояние — [`TurnState`] в снимке run (D66): реплей шага по квитанциям даёт
//! те же решения.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use za_core::ToolKind;
use za_llm::Usage;
use za_protocol::{BlobHash, RunRole, alert_kind, pause_reason};

use super::Reaction;

/// Nudge обещания без действия (D60).
pub const PROMISE_NUDGE: &str = "You said you would act but made no tool call. Make the call now.";

/// Сколько последних символов текста смотрит детектор обещания (D60).
const PROMISE_TAIL_CHARS: usize = 200;
/// Обороты обещания действия (D60; Ruling I-2 ревью задачи 8): будущее 1-го лица и
/// приглашения, в нижнем регистре, пробелы одиночные. Совпадение — с границами слова
/// Unicode с обеих сторон («now i see», «сейчас ясно», «приступают» — не обещания).
const PROMISE_PATTERNS: [&str; 15] = [
    "i will",
    "i'll",
    "i am going to",
    "i'm going to",
    "let me",
    "let's",
    "let us",
    "next, i",
    "next i",
    "now i'll",
    "now i will",
    "now, i'll",
    "now, i will",
    "давайте",
    "далее я",
];
/// Русские глаголы будущего времени 1-го лица (D60, Ruling I-2): «Сейчас проверю»,
/// «Запущу тесты», «Теперь я исправлю» — обещание действия в любом месте последней
/// строки.
const PROMISE_VERBS_RU: [&str; 30] = [
    "проверю",
    "посмотрю",
    "запущу",
    "исправлю",
    "сделаю",
    "открою",
    "прочитаю",
    "прочту",
    "начну",
    "попробую",
    "выполню",
    "изучу",
    "поищу",
    "добавлю",
    "напишу",
    "перепишу",
    "обновлю",
    "поправлю",
    "соберу",
    "займусь",
    "приступаю",
    "проверим",
    "посмотрим",
    "запустим",
    "исправим",
    "сделаем",
    "начнём",
    "начнем",
    "попробуем",
    "займёмся",
];
/// Вежливые концовки ответа, содержащие оборот обещания, но не обещающие действия:
/// «Let me know if…», «I'll be happy to…». Вырезаются до поиска оборотов.
const PROMISE_CLOSINGS: [&str; 8] = [
    "let me know",
    "let us know",
    "i'll let you know",
    "i will let you know",
    "i'll be happy",
    "i'll be glad",
    "i will be happy",
    "i will be glad",
];
/// Условные обороты: оборот обещания после них в том же предложении — предложение
/// пользователю («If you want, I'll add tests», «Если нужно, проверю ещё раз»), а не
/// обещание действия (m2 финального ревью R1-B2). Нижний регистр, границы слова.
const PROMISE_CONDITIONALS: [&str; 22] = [
    "if you",
    "if needed",
    "if necessary",
    "if desired",
    "if required",
    "if preferred",
    "if it helps",
    "if that helps",
    "should you",
    "если хотите",
    "если хочешь",
    "если желаете",
    "если нужно",
    "если надо",
    "если потребуется",
    "если понадобится",
    "если необходимо",
    "если вам",
    "если тебе",
    "при желании",
    "при необходимости",
    "по желанию",
];

/// Ошибок подряд в классе инструмента до паузы run (D57): шестая.
const TOOL_ERRORS_PAUSE: u32 = 6;
/// Ход подряд с `parse_errors > 0`, на котором — аварийная компакция (D61).
const PARSE_COMPACT_AT: u32 = 3;
/// Ходов подряд с `parse_errors > 0` после компакции до паузы `model_loop` (D61).
const PARSE_PAUSE_AFTER: u32 = 2;
/// Доля `cached / input` прошлого хода, от которой падение кэша замечается (D62).
const CACHE_SHARE_MIN: f64 = 0.5;

/// D60: в последних 200 символах последней строки текста (без учёта регистра) есть
/// оборот обещания действия ([`PROMISE_PATTERNS`], [`PROMISE_VERBS_RU`]) с границами
/// слова Unicode с обеих сторон. Вежливые концовки («Let me know…», «I'll be
/// happy…») не считаются; текст, кончающийся вопросом, — вопрос пользователю, а не
/// обещание; оборот после условного оборота ([`PROMISE_CONDITIONALS`]) в том же
/// предложении — предложение, а не обещание. Последняя строка: «Let me summarize:» со списком после него — уже ответ;
/// если текст кончается блоком кода, смотрится строка перед ним («Let me run:» и
/// команда в блоке — обещание).
///
/// Вызывающий проверяет остальное условие D60: в ходе нет вызовов и `finish = Stop`.
pub fn promise_without_action(text: &str) -> bool {
    let trimmed = text.trim_end();
    if trimmed.ends_with('?') {
        return false;
    }
    let trimmed = before_trailing_code(trimmed);
    if trimmed.is_empty() || trimmed.ends_with('?') {
        return false;
    }
    // Последняя строка: обещание стоит в конце, а текст после оборота — это сам ответ.
    let last = trimmed.rsplit('\n').next().unwrap_or(trimmed);
    let chars: Vec<char> = last.chars().collect();
    let start = chars.len().saturating_sub(PROMISE_TAIL_CHARS);
    // Типографский апостроф — как обычный: «I’ll» тоже обещание; пробелы одиночные.
    let lower = chars[start..]
        .iter()
        .collect::<String>()
        .to_lowercase()
        .replace('\u{2019}', "'");
    let mut tail = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    for c in PROMISE_CLOSINGS {
        tail = tail.replace(c, " ");
    }
    // Предложения — до `.`, `!`, `;`: условие одного предложения не снимает обещание
    // другого («If you want more, ask. Let me run the tests»).
    tail.split(['.', '!', ';']).any(|sentence| {
        let cond = PROMISE_CONDITIONALS
            .iter()
            .filter_map(|c| word_positions(sentence, c).next())
            .min()
            .unwrap_or(usize::MAX);
        PROMISE_PATTERNS
            .iter()
            .chain(PROMISE_VERBS_RU.iter())
            .any(|p| word_positions(sentence, p).any(|i| i < cond))
    })
}

/// Текст до завершающего блока кода Markdown (```` ``` ````), если текст им кончается.
fn before_trailing_code(t: &str) -> &str {
    let Some(body) = t.strip_suffix("```") else {
        return t;
    };
    match body.rfind("```") {
        Some(i) => t[..i].trim_end(),
        None => t,
    }
}

/// `needle` входит в `hay` целыми словами: до и после него нет буквы или цифры
/// (Unicode: кириллица тоже).
#[cfg(test)]
fn has_word(hay: &str, needle: &str) -> bool {
    word_positions(hay, needle).next().is_some()
}

/// Байтовые позиции вхождений `needle` в `hay` целыми словами: до и после вхождения
/// нет буквы или цифры (Unicode: кириллица тоже).
fn word_positions<'a>(hay: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    hay.match_indices(needle).filter_map(move |(i, m)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + m.len()..].chars().next();
        (before.is_none_or(|c| !c.is_alphanumeric()) && after.is_none_or(|c| !c.is_alphanumeric()))
            .then_some(i)
    })
}

/// Счётчики ошибок подряд по классу инструмента (D57).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct ToolErrors {
    read: u32,
    edit: u32,
    exec: u32,
    control: u32,
}

impl ToolErrors {
    fn of(&mut self, kind: ToolKind) -> &mut u32 {
        match kind {
            ToolKind::Read => &mut self.read,
            ToolKind::Edit => &mut self.edit,
            ToolKind::Exec => &mut self.exec,
            ToolKind::Control => &mut self.control,
        }
    }
}

/// Доля кэша прошлого хода (D62).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CacheMark {
    /// Эпоха истории запроса хода.
    epoch: u32,
    cached: u64,
    share: f64,
}

/// Состояние детекторов хода (в [`super::DetectorState`], снимок D66).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct TurnState {
    /// Прошлый ход — обещание без действия, получившее nudge (D60).
    promised: bool,
    /// Ошибки подряд по классу инструмента (D57).
    tool_errors: ToolErrors,
    /// Ходов подряд с `parse_errors > 0` (D61).
    parse_streak: u32,
    /// Компакция по порче JSON уже назначена в этой серии (D61).
    parse_compacted: bool,
    /// Компакция по порче JSON назначена и ещё не выполнена: граница шага её делает.
    compact_due: bool,
    /// Доля кэша прошлого хода (D62).
    cache: Option<CacheMark>,
    /// Хэши сообщений запроса прошлого хода (D62, `first_diff`).
    request_refs: Vec<BlobHash>,
}

impl TurnState {
    pub(super) fn on_promise(&mut self, role: RunRole) -> Reaction {
        if self.promised {
            tracing::debug!(?role, "второе обещание без действия подряд: без nudge");
            return Reaction::None;
        }
        self.promised = true;
        Reaction::Nudge(PROMISE_NUDGE.to_owned())
    }

    pub(super) fn reset_promise(&mut self) {
        self.promised = false;
    }

    /// D57 (Ruling I-1 ревью задачи 8): счёт — по ходам. Успешный исполненный Edit
    /// или Exec обнуляет счёт всех классов (как сброс окна D54: правка — прогресс).
    /// Иначе класс с успехом в ходе обнуляется, класс с ошибкой и без успеха получает
    /// +1 за ход (параллельный пакет из шести ошибок — одна).
    pub(super) fn on_tool_errors(&mut self, results: &[(ToolKind, bool)]) -> Reaction {
        let has = |kind: ToolKind, err: bool| results.iter().any(|&(k, e)| k == kind && e == err);
        if has(ToolKind::Edit, false) || has(ToolKind::Exec, false) {
            self.tool_errors = ToolErrors::default();
            return Reaction::None;
        }
        let mut worst = Reaction::None;
        for kind in [
            ToolKind::Read,
            ToolKind::Edit,
            ToolKind::Exec,
            ToolKind::Control,
        ] {
            let n = self.tool_errors.of(kind);
            if has(kind, false) {
                *n = 0;
                continue;
            }
            if !has(kind, true) {
                continue;
            }
            *n += 1;
            let r = match *n {
                TOOL_ERRORS_PAUSE.. => {
                    // Пауза — конец серии: после resume счёт с начала (снова паузы
                    // 1, 2, 4 с, затем `paused`), без бесконечных повторов.
                    *n = 0;
                    Reaction::Pause(pause_reason::TOOL_ERRORS)
                }
                k @ 3.. => Reaction::Sleep(Duration::from_secs(1 << (k - 3))),
                _ => Reaction::None,
            };
            worst = stronger(worst, r);
        }
        worst
    }

    /// Ввод пользователя обнуляет счёт ошибок (Ruling I-1 ревью задачи 8).
    pub(super) fn reset_tool_errors(&mut self) {
        self.tool_errors = ToolErrors::default();
    }

    /// Метка кэша прошлого хода сброшена: resume (кэш сервера мог остыть, эндпоинт мог
    /// смениться) — первый ход после него не сравнивается (Ruling minor ревью задачи 8).
    pub(super) fn reset_cache(&mut self) {
        self.cache = None;
    }

    pub(super) fn on_parse_errors(&mut self, n: u32) -> Reaction {
        if n == 0 {
            self.parse_streak = 0;
            self.parse_compacted = false;
            return Reaction::None;
        }
        self.parse_streak += 1;
        if !self.parse_compacted && self.parse_streak >= PARSE_COMPACT_AT {
            self.parse_compacted = true;
            self.parse_streak = 0;
            self.compact_due = true;
            return Reaction::Compact;
        }
        if self.parse_compacted && self.parse_streak >= PARSE_PAUSE_AFTER {
            self.parse_streak = 0;
            self.parse_compacted = false;
            return Reaction::Pause(pause_reason::MODEL_LOOP);
        }
        Reaction::None
    }

    pub(super) fn compaction_due(&self) -> bool {
        self.compact_due
    }

    pub(super) fn compaction_done(&mut self) {
        self.compact_due = false;
    }

    pub(super) fn remember_request_refs(&mut self, refs: &[BlobHash]) -> Vec<BlobHash> {
        std::mem::replace(&mut self.request_refs, refs.to_vec())
    }

    pub(super) fn on_usage(
        &mut self,
        usage: &Usage,
        cached_supported: bool,
        epoch: u32,
        prev_refs: &[BlobHash],
        refs: &[BlobHash],
    ) -> Option<Value> {
        if !cached_supported || usage.input == 0 {
            // Доля неизвестна: сравнивать следующий ход не с чем.
            self.cache = None;
            return None;
        }
        let share = usage.cached as f64 / usage.input as f64;
        let prev = self.cache.replace(CacheMark {
            epoch,
            cached: usage.cached,
            share,
        })?;
        // Кэш упал, если доля стала меньше половины прошлой и сами токены из кэша —
        // меньше половины прошлых: большой новый результат инструмента снижает долю,
        // но не число токенов из кэша, и падением не считается.
        let dropped = prev.share >= CACHE_SHARE_MIN
            && share < prev.share / 2.0
            && usage.cached.saturating_mul(2) < prev.cached;
        if prev.epoch != epoch || !dropped {
            return None;
        }
        Some(json!({
            "kind": alert_kind::CACHE_DROP,
            "prev_share": prev.share,
            "share": share,
            "first_diff": first_diff(prev_refs, refs),
        }))
    }
}

/// Сильнейшая из двух реакций детектора ошибок: пауза run, затем самая длинная пауза
/// перед вызовом модели.
fn stronger(a: Reaction, b: Reaction) -> Reaction {
    match (&a, &b) {
        (Reaction::Pause(_), _) => a,
        (_, Reaction::Pause(_)) => b,
        (Reaction::Sleep(x), Reaction::Sleep(y)) if x >= y => a,
        (_, Reaction::Sleep(_)) => b,
        _ => a,
    }
}

/// Индекс первого сообщения прошлого запроса, хэш которого в текущем другой (или
/// которого в текущем нет); `null` — прошлый запрос — префикс текущего.
fn first_diff(prev: &[BlobHash], cur: &[BlobHash]) -> Value {
    prev.iter()
        .enumerate()
        .find(|(i, h)| cur.get(*i) != Some(*h))
        .map_or(Value::Null, |(i, _)| json!(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_words_only() {
        assert!(has_word("ok. let me run", "let me"));
        assert!(has_word("let me", "let me"));
        assert!(!has_word("the api will return", "i will"));
        assert!(!has_word("сейчас ясно", "сейчас я"));
        assert!(!has_word("они приступают", "приступаю"));
        assert!(has_word("приступаю.", "приступаю"));
    }

    #[test]
    fn trailing_code_block() {
        assert_eq!(before_trailing_code("Run:\n```\nx\n```"), "Run:");
        assert_eq!(before_trailing_code("no code"), "no code");
        assert_eq!(before_trailing_code("```"), "```");
    }

    #[test]
    fn stronger_prefers_pause_then_longer_sleep() {
        let s1 = Reaction::Sleep(Duration::from_secs(1));
        let s2 = Reaction::Sleep(Duration::from_secs(2));
        let p = Reaction::Pause(pause_reason::TOOL_ERRORS);
        assert_eq!(stronger(s1.clone(), s2.clone()), s2);
        assert_eq!(stronger(s2.clone(), s1.clone()), s2);
        assert_eq!(stronger(Reaction::None, s1.clone()), s1);
        assert_eq!(stronger(s2, p.clone()), p);
        assert_eq!(stronger(p.clone(), Reaction::None), p);
    }

    #[test]
    fn first_diff_index() {
        let h = |s: &str| BlobHash(s.into());
        let a = [h("a"), h("b")];
        assert_eq!(first_diff(&a, &[h("a"), h("b"), h("c")]), Value::Null);
        assert_eq!(first_diff(&a, &[h("a"), h("x")]), json!(1));
        assert_eq!(first_diff(&a, &[h("a")]), json!(1));
        assert_eq!(first_diff(&[], &[h("a")]), Value::Null);
    }
}
