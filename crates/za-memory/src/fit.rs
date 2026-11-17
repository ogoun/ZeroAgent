//! Укладка ответа `/recall` в бюджет — только на клиенте (R1-M D141; MEM-030, MEM-061;
//! П3 изменений по ZAM 0.0.12-m6.5).
//!
//! `/recall` идёт без `output_limit`: под пределом сервер снимает «Последнее известное
//! состояние» и «Окружение» раньше правил. Клиент кладёт ответ в бюджет сам —
//! [`fit_digest`], чистая функция.
//!
//! - **Разделы** — по строкам `### …`; заголовок узнаётся по самому длинному известному
//!   префиксу на границе слова ([`header_priority`]): `### Идеи на разбор` раньше `### Идеи`,
//!   `### Ловушки и сбои` раньше `### Ловушки`, `### Последнее известное состояние (снимок …,
//!   извлечено автоматически)` — по началу. Строки до первого раздела — уведомления.
//! - **Строки без заголовка** узнаются по началу в любом разделе: `План из идей:` (сервер
//!   ставит её сразу за задачами без пустой строки), `Словарь:`, `Проверенные команды:`,
//!   строка карты кода `ZeroAIMemory: карта кода:`. Строка с отступом продолжает предыдущую
//!   (блок правила: `<путь> [надёжность] n:<id>` и текст с отступом в две позиции).
//! - **Приоритет** (меньше — важнее, константы `PRIORITY_*`): задачи; последнее известное
//!   состояние; уведомления; ответы человека; `Идеи на разбор` и строка плана — только в
//!   режиме `Review` (D148); окружение (решение владельца 2); правила; ловушки; решения,
//!   открытые вопросы, идеи, revisit, изменения, термины, файлы, код; строки `Словарь:`,
//!   `Проверенные команды:`, карта кода; раздел, которого нет в таблице, — слабее всех.
//! - **Режим `Wait`** снимает раздел разбора и строку плана, а также строки со ссылками
//!   `drop_refs` (элементы `items[]` с `reason: idea_review`: очередь разбора и подсказка к
//!   `prompt`, Δ11) — они велят звать инструмент, которого у модели в `Wait` нет.
//! - **Нехватка бюджета** снимает с самого слабого раздела (среди равных — с последнего):
//!   правила — ступенями (полный текст → первая строка каждого правила → `<путь> n:<id>` →
//!   снятие с конца), прочие — по блокам с конца. Снятое называет одна строка
//!   `[memory: omitted <разделы>; use memory_search]` (первые пять имён по приоритету и `+N`);
//!   она входит в бюджет. Снятое режимом и уже показанное не называется: искать его модели
//!   незачем.
//! - **Счёт** — `ceil(байты / 4)` × `k`, `k = max(1, server_tokens / ceil(байты всего текста /
//!   4)) × 1,1` (запас 10 %): сервер считает своим токенизатором (`/health.token_counter:
//!   "model"`), оценка D22 может быть ниже. Дроби — в целых.
//! - **Вывод** — по приоритету, задачи первыми; разделы через пустую строку.

use std::collections::BTreeSet;

use crate::ideas::IdeasMode;

/// `### Задачи`.
pub const PRIORITY_TASKS: u8 = 1;
/// `### Последнее известное состояние (снимок …, извлечено автоматически)`.
pub const PRIORITY_SNAPSHOT: u8 = 2;
/// Строки до первого раздела.
pub const PRIORITY_NOTICES: u8 = 3;
/// `### Ответы человека`.
pub const PRIORITY_ANSWERS: u8 = 4;
/// `### Идеи на разбор` и строка `План из идей:` — только в режиме `Review`.
pub const PRIORITY_IDEAS_REVIEW: u8 = 5;
/// `### Окружение`.
pub const PRIORITY_ENVIRONMENT: u8 = 6;
/// `### Правила`.
pub const PRIORITY_RULES: u8 = 7;
/// `### Ловушки и сбои`, `### Ловушки`.
pub const PRIORITY_TRAPS: u8 = 8;
/// Решения, открытые вопросы, идеи, revisit, изменения, термины, файлы, код.
pub const PRIORITY_OTHER: u8 = 9;
/// Строки-указатели после правил и карта кода.
pub const PRIORITY_POINTERS: u8 = 10;
/// Раздел, которого нет в таблице D141.
pub const PRIORITY_UNKNOWN: u8 = 11;

/// Начало строки снятого.
pub const OMITTED_HEAD: &str = "[memory: omitted ";
/// Конец строки снятого.
pub const OMITTED_TAIL: &str = "; use memory_search]";
/// Имя строк до первого раздела в строке снятого.
const NOTICES_NAME: &str = "notices";
/// Имя раздела, которого нет в таблице, в данных события `memory.recall` (D153: текста
/// памяти в событии нет).
pub const UNKNOWN_NAME: &str = "unknown";
/// Предел имени незнакомого раздела в строке снятого, символов.
const NAME_MAX_CHARS: usize = 60;
/// Имён в строке снятого, дальше — `+N`.
pub const OMITTED_NAMES_MAX: usize = 5;

const HEADER_PREFIX: &str = "### ";

/// Заголовки разделов ZAM и их приоритет (`crates/zam-server/src/api/recall.rs`,
/// `api/snapshot.rs:577`, `api/code.rs:904` по тегу `v0.0.12-m6.5`).
const HEADERS: &[(&str, u8)] = &[
    ("### Задачи", PRIORITY_TASKS),
    ("### Последнее известное состояние", PRIORITY_SNAPSHOT),
    ("### Ответы человека", PRIORITY_ANSWERS),
    ("### Идеи на разбор", PRIORITY_IDEAS_REVIEW),
    ("### Окружение", PRIORITY_ENVIRONMENT),
    ("### Правила", PRIORITY_RULES),
    ("### Ловушки и сбои", PRIORITY_TRAPS),
    ("### Ловушки", PRIORITY_TRAPS),
    ("### Решения", PRIORITY_OTHER),
    ("### Решение", PRIORITY_OTHER),
    ("### Открытые вопросы", PRIORITY_OTHER),
    ("### Идеи", PRIORITY_OTHER),
    ("### Пересмотреть (revisit)", PRIORITY_OTHER),
    ("### Последние изменения", PRIORITY_OTHER),
    ("### Термины", PRIORITY_OTHER),
    ("### Файлы", PRIORITY_OTHER),
    ("### Код", PRIORITY_OTHER),
    // Мини-карта кода — та же карта, что строка карты кода (D141, п. 10).
    ("### Карта кода", PRIORITY_POINTERS),
];

/// Строки без заголовка, которые узнаются по началу в любом разделе: начало, приоритет,
/// имя в строке снятого.
const LINES: &[(&str, u8, &str)] = &[
    ("План из идей:", PRIORITY_IDEAS_REVIEW, "План из идей"),
    ("Словарь:", PRIORITY_POINTERS, "Словарь"),
    (
        "Проверенные команды:",
        PRIORITY_POINTERS,
        "Проверенные команды",
    ),
    ("ZeroAIMemory: карта кода:", PRIORITY_POINTERS, "Карта кода"),
];

/// Буквы ссылок ZAM: записи журнала (`i c r t k q p w e`, `zam_store::ENTRY_REF_LETTERS`),
/// заметки (`n`) и Vault (`v`).
const REF_LETTERS: &[u8] = b"icrtkqpwenv";
/// Длина id ссылки (ULID).
const REF_ID_LEN: usize = 26;

/// Итог укладки.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fitted {
    /// Текст в бюджете (без завершающего перевода строки); пусто — нечего показать.
    pub text: String,
    /// Счёт текста ([`approx_tokens`] × `k`).
    pub tokens: u32,
    /// Разделы, снятые или сокращённые бюджетом, — в порядке приоритета.
    pub omitted: Vec<String>,
    /// Ссылки ZAM в тексте (`k:…`, `n:…`, …) в порядке появления, без повторов.
    pub refs: Vec<String>,
}

/// Оценка токенов `ceil(байты UTF-8 / 4)` — та же оценка, что `za_agent::approx_tokens`
/// (D22); `za-memory` от `za-agent` не зависит (D133), поэтому функция своя.
pub fn approx_tokens(s: &str) -> u32 {
    u32::try_from(s.len().div_ceil(4)).unwrap_or(u32::MAX)
}

/// Приоритет раздела по строке заголовка `### …` (D141): самый длинный известный префикс на
/// границе слова (дальше — конец, пробел или `(`); незнакомый — [`PRIORITY_UNKNOWN`].
pub fn header_priority(line: &str) -> u8 {
    known_header(line.trim_end()).map_or(PRIORITY_UNKNOWN, |(_, p)| p)
}

/// Известен ли раздел с таким именем (без `### `) или строка без заголовка таблицы D141.
pub fn is_known_name(name: &str) -> bool {
    name == NOTICES_NAME
        || HEADERS
            .iter()
            .any(|(h, _)| &h[HEADER_PREFIX.len()..] == name)
        || LINES.iter().any(|(_, _, n)| *n == name)
}

/// Укладывает текст ответа `/recall` в `budget` (D141, П3): `server_tokens` — поле `tokens`
/// ответа (счёт сервера), `mode` — режим идей, `drop_refs` — ссылки `items[]` с `reason:
/// idea_review` (снимаются в режиме `Wait`).
pub fn fit_digest(
    text: &str,
    budget: u32,
    server_tokens: Option<u32>,
    mode: IdeasMode,
    drop_refs: &BTreeSet<String>,
) -> Fitted {
    fit_with(
        text,
        budget,
        server_tokens,
        mode,
        drop_refs,
        &BTreeSet::new(),
    )
}

/// [`fit_digest`] без блоков, чьи ссылки все уже показаны в этой эпохе (`shown`; D140 «без
/// повторов ссылок»): порт памяти помнит показанное между дайджестом, подсказками и повтором.
pub(crate) fn fit_with(
    text: &str,
    budget: u32,
    server_tokens: Option<u32>,
    mode: IdeasMode,
    drop_refs: &BTreeSet<String>,
    shown: &BTreeSet<String>,
) -> Fitted {
    let scale = Scale::new(server_tokens, approx_tokens(text));
    let mut units = parse(text);
    filter(&mut units, mode, drop_refs, shown);
    loop {
        let omitted = omitted_names(&units);
        let out = render(&units, &omitted);
        let tokens = scale.count(&out);
        if tokens <= budget {
            let refs = if out.is_empty() {
                Vec::new()
            } else {
                kept_refs(&units)
            };
            return Fitted {
                text: out,
                tokens,
                omitted,
                refs,
            };
        }
        match units.iter().rposition(|u| u.kept > 0) {
            Some(i) => units[i].reduce(),
            None => {
                return Fitted {
                    omitted,
                    ..Fitted::default()
                };
            }
        }
    }
}

/// Строка снятого: `[memory: omitted <разделы через запятую>; use memory_search]` — первые
/// [`OMITTED_NAMES_MAX`] имён (они по приоритету) и `+N` остальных: строка входит в бюджет,
/// длинный список съел бы его у самих разделов.
pub fn omitted_line(names: &[String]) -> String {
    let shown = names.len().min(OMITTED_NAMES_MAX);
    let mut list = names[..shown].join(", ");
    if names.len() > shown {
        list.push_str(&format!(" +{}", names.len() - shown));
    }
    format!("{OMITTED_HEAD}{list}{OMITTED_TAIL}")
}

/// Тексты укладки, которые видит модель, — для хэша промптов (D155): строка снятого.
pub fn text_samples() -> Vec<(&'static str, String)> {
    vec![("memory/fit.omitted", omitted_line(&["SECTION".to_owned()]))]
}

/// Ссылки ZAM в строке: `<буква>:<ULID>` на границе слова.
pub fn refs_in(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 + REF_ID_LEN <= b.len() {
        let end = i + 2 + REF_ID_LEN;
        let starts = (i == 0 || !b[i - 1].is_ascii_alphanumeric())
            && REF_LETTERS.contains(&b[i])
            && b[i + 1] == b':';
        if starts
            && b[i + 2..end]
                .iter()
                .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
            && b.get(end).is_none_or(|c| !c.is_ascii_alphanumeric())
        {
            // Все байты `i..end` — ASCII: срез по границам символов.
            out.push(&s[i..end]);
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

/// Ступень сокращения правил.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Full,
    /// Строка правила и первая строка текста.
    FirstLine,
    /// `<путь> n:<id>`.
    Refs,
}

/// Раздел (или строка без заголовка) в укладке.
#[derive(Debug, Clone)]
struct Unit {
    prio: u8,
    /// Порядок в исходном тексте: среди равных по приоритету снимается последний.
    order: usize,
    /// Раздел `### Правила`: сокращается ступенями.
    rules: bool,
    /// Раздел разбора идей или строка плана: только в режиме `Review`.
    ideas: bool,
    title: Option<String>,
    name: String,
    /// Блоки: первая строка и строки с отступом за ней.
    blocks: Vec<Vec<String>>,
    /// Сколько блоков с начала выводится.
    kept: usize,
    stage: Stage,
    /// Бюджет что-то снял или сократил.
    reduced: bool,
}

impl Unit {
    fn new(prio: u8, order: usize, title: Option<String>, name: String) -> Self {
        Self {
            prio,
            order,
            rules: prio == PRIORITY_RULES,
            ideas: prio == PRIORITY_IDEAS_REVIEW,
            title,
            name,
            blocks: Vec::new(),
            kept: 0,
            stage: Stage::Full,
            reduced: false,
        }
    }

    fn notices(order: usize) -> Self {
        Self::new(PRIORITY_NOTICES, order, None, NOTICES_NAME.to_owned())
    }

    fn header(line: &str, order: usize) -> Self {
        match known_header(line) {
            Some((h, p)) => Self::new(
                p,
                order,
                Some(line.to_owned()),
                h[HEADER_PREFIX.len()..].to_owned(),
            ),
            None => {
                let name: String = line[HEADER_PREFIX.len()..]
                    .trim()
                    .chars()
                    .take(NAME_MAX_CHARS)
                    .collect();
                Self::new(PRIORITY_UNKNOWN, order, Some(line.to_owned()), name)
            }
        }
    }

    /// Строка без заголовка из [`LINES`].
    fn line(line: &str, order: usize) -> Option<Self> {
        let (_, p, name) = LINES.iter().find(|(pre, _, _)| line.starts_with(pre))?;
        let mut u = Self::new(*p, order, None, (*name).to_owned());
        u.blocks.push(vec![line.to_owned()]);
        Some(u)
    }

    /// Следующая ступень сокращения: правила — первая строка, затем `<путь> n:<id>`, затем
    /// снятие с конца; прочие — снятие последнего блока.
    fn reduce(&mut self) {
        self.reduced = true;
        if self.rules {
            while self.stage != Stage::Refs {
                let changed = match self.stage {
                    Stage::Full => {
                        self.stage = Stage::FirstLine;
                        self.shrink_to_first_lines()
                    }
                    _ => {
                        self.stage = Stage::Refs;
                        self.shrink_to_refs()
                    }
                };
                if changed {
                    return;
                }
            }
        }
        self.kept = self.kept.saturating_sub(1);
    }

    /// Блок правила — строка правила и первая строка текста; изменилось ли что-то.
    fn shrink_to_first_lines(&mut self) -> bool {
        let mut changed = false;
        for b in &mut self.blocks {
            if b.len() > 2 {
                b.truncate(2);
                changed = true;
            }
        }
        changed
    }

    /// Блок правила — `<путь> n:<id>`; блок без ссылки `n:` (строка о правилах) снимается.
    fn shrink_to_refs(&mut self) -> bool {
        let before = self.blocks.clone();
        self.blocks = before
            .iter()
            .filter_map(|b| rule_ref(&b[0]).map(|r| vec![r]))
            .collect();
        self.kept = self.blocks.len();
        self.blocks != before
    }
}

/// `<путь> n:<id>` из строки правила `<путь> [надёжность] n:<id>`.
fn rule_ref(head: &str) -> Option<String> {
    let path = head.split_whitespace().next()?;
    let r = refs_in(head).into_iter().find(|r| r.starts_with("n:"))?;
    (path != r).then(|| format!("{path} {r}"))
}

/// Известный заголовок: самый длинный префикс из [`HEADERS`] на границе слова.
fn known_header(line: &str) -> Option<(&'static str, u8)> {
    HEADERS
        .iter()
        .filter(|(h, _)| {
            line.strip_prefix(h)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '(']))
        })
        .max_by_key(|(h, _)| h.len())
        .copied()
}

/// Разбор текста на разделы, по приоритету (среди равных — в порядке текста).
fn parse(text: &str) -> Vec<Unit> {
    let mut done: Vec<Unit> = Vec::new();
    let mut order = 0;
    let mut cur = Unit::notices(order);
    // Последний блок — у строки без заголовка (индекс в `done`), а не у текущего раздела.
    let mut tail_line: Option<usize> = None;
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with(HEADER_PREFIX) {
            order += 1;
            let next = Unit::header(line, order);
            push_unit(&mut done, std::mem::replace(&mut cur, next));
            tail_line = None;
            continue;
        }
        let indented = line.starts_with([' ', '\t']);
        if !indented {
            order += 1;
            if let Some(u) = Unit::line(line, order) {
                done.push(u);
                tail_line = Some(done.len() - 1);
                continue;
            }
        }
        let target = match tail_line {
            Some(i) if indented => &mut done[i],
            _ => {
                tail_line = None;
                &mut cur
            }
        };
        match target.blocks.last_mut() {
            Some(b) if indented => b.push(line.to_owned()),
            _ => target.blocks.push(vec![line.to_owned()]),
        }
    }
    push_unit(&mut done, cur);
    for u in &mut done {
        u.kept = u.blocks.len();
    }
    done.sort_by_key(|u| (u.prio, u.order));
    done
}

fn push_unit(done: &mut Vec<Unit>, u: Unit) {
    if !u.blocks.is_empty() {
        done.push(u);
    }
}

/// Режим идей и показанное: `Wait` снимает раздел разбора, строку плана и блоки со ссылками
/// `drop_refs`; блок, чьи ссылки все в `shown`, снимается в любом режиме.
fn filter(
    units: &mut Vec<Unit>,
    mode: IdeasMode,
    drop_refs: &BTreeSet<String>,
    shown: &BTreeSet<String>,
) {
    let wait = mode == IdeasMode::Wait;
    units.retain(|u| !(wait && u.ideas));
    for u in units.iter_mut() {
        u.blocks.retain(|b| {
            let refs: Vec<&str> = b.iter().flat_map(|l| refs_in(l)).collect();
            let dropped = wait && refs.iter().any(|r| drop_refs.contains(*r));
            let seen = !refs.is_empty() && refs.iter().all(|r| shown.contains(*r));
            !dropped && !seen
        });
        u.kept = u.blocks.len();
    }
    units.retain(|u| !u.blocks.is_empty());
}

/// Имена разделов, которые бюджет снял или сократил, без повторов.
fn omitted_names(units: &[Unit]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for u in units.iter().filter(|u| u.reduced) {
        if !out.contains(&u.name) {
            out.push(u.name.clone());
        }
    }
    out
}

/// Текст выводимых блоков и строка снятого.
fn render(units: &[Unit], omitted: &[String]) -> String {
    let mut out = String::new();
    for u in units.iter().filter(|u| u.kept > 0) {
        if !out.is_empty() {
            out.push('\n');
        }
        if let Some(t) = &u.title {
            out.push_str(t);
            out.push('\n');
        }
        for l in u.blocks[..u.kept].iter().flatten() {
            out.push_str(l);
            out.push('\n');
        }
    }
    if !omitted.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&omitted_line(omitted));
    }
    out.truncate(out.trim_end().len());
    out
}

/// Ссылки выводимых блоков в порядке вывода, без повторов.
fn kept_refs(units: &[Unit]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for u in units.iter().filter(|u| u.kept > 0) {
        for r in u.blocks[..u.kept].iter().flatten().flat_map(|l| refs_in(l)) {
            if !out.iter().any(|x| x == r) {
                out.push(r.to_owned());
            }
        }
    }
    out
}

/// Множитель `k` счёта (D141) дробью `num / den`.
#[derive(Debug, Clone, Copy)]
struct Scale {
    num: u64,
    den: u64,
}

impl Scale {
    /// `k = max(1, server / est) × 1,1`.
    fn new(server_tokens: Option<u32>, est: u32) -> Self {
        match server_tokens {
            Some(s) if est > 0 && s > est => Self {
                num: u64::from(s) * 11,
                den: u64::from(est) * 10,
            },
            _ => Self { num: 11, den: 10 },
        }
    }

    /// `ceil(ceil(байты / 4) × k)`.
    fn count(&self, s: &str) -> u32 {
        let est = u64::from(approx_tokens(s));
        u32::try_from((est * self.num).div_ceil(self.den)).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_on_word_boundary_only() {
        let id = "01M8SYNTH00000000000000K01";
        let line = format!("— k:{id} → i:{id}; xk:{id} n:{id}Z w:01M8");
        assert_eq!(refs_in(&line), vec![format!("k:{id}"), format!("i:{id}")]);
        assert!(refs_in("").is_empty());
    }

    #[test]
    fn rule_ref_drops_certainty() {
        let id = "01M8SYNTH00000000000000R01";
        assert_eq!(
            rule_ref(&format!("proc/a [th] n:{id}")),
            Some(format!("proc/a n:{id}"))
        );
        assert_eq!(
            rule_ref("ZeroAIMemory: правила сокращены — memory_read n:<id>"),
            None
        );
    }

    #[test]
    fn scale_is_at_least_one_point_one() {
        assert_eq!(Scale::new(None, 100).count(&"a".repeat(400)), 110);
        assert_eq!(Scale::new(Some(50), 100).count(&"a".repeat(400)), 110);
        assert_eq!(Scale::new(Some(200), 100).count(&"a".repeat(400)), 220);
        assert_eq!(Scale::new(Some(200), 0).count(""), 0);
    }

    #[test]
    fn omitted_line_caps_names() {
        let names: Vec<String> = (1..=7).map(|i| format!("S{i}")).collect();
        assert_eq!(
            omitted_line(&names[..2]),
            "[memory: omitted S1, S2; use memory_search]"
        );
        assert_eq!(
            omitted_line(&names),
            "[memory: omitted S1, S2, S3, S4, S5 +2; use memory_search]"
        );
    }

    #[test]
    fn known_names_cover_table() {
        for n in [
            "Задачи",
            "Последнее известное состояние",
            "notices",
            "Словарь",
            "Карта кода",
        ] {
            assert!(is_known_name(n), "{n}");
        }
        assert!(!is_known_name("Новое"));
    }
}
