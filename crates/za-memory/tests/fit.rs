//! Укладка ответа `/recall` в бюджет на клиенте (R1-M D141, П3 изменений по ZAM 0.0.12-m6.5):
//! чистая функция `fit_digest` на синтетических дайджестах `tests/fixtures/digest/*.md`
//! (формы заголовков и строк — по коду ZAM `api/recall.rs@v0.0.12-m6.5`; записей рабочей
//! памяти нет).

use std::collections::BTreeSet;

use za_memory::fit::{
    NOTICES_NAME, OMITTED_HEAD, OMITTED_TAIL, PRIORITY_ENVIRONMENT, PRIORITY_IDEAS_REVIEW,
    PRIORITY_OTHER, PRIORITY_POINTERS, PRIORITY_RULES, PRIORITY_SNAPSHOT, PRIORITY_TASKS,
    PRIORITY_TRAPS, PRIORITY_UNKNOWN, approx_tokens, header_priority,
};
use za_memory::{Fitted, IdeasMode, fetch_budget, fit_digest};

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/digest/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .replace("\r\n", "\n")
}

fn none() -> BTreeSet<String> {
    BTreeSet::new()
}

fn fit(text: &str, budget: u32, mode: IdeasMode) -> Fitted {
    fit_digest(text, budget, None, mode, &none())
}

/// Строки раздела `header` (до пустой строки или следующего заголовка).
fn section<'a>(text: &'a str, header: &str) -> Option<Vec<&'a str>> {
    let mut lines = text.lines();
    lines.by_ref().find(|l| l.starts_with(header))?;
    Some(
        lines
            .take_while(|l| !l.is_empty() && !l.starts_with("### "))
            .collect(),
    )
}

fn omitted_line(f: &Fitted) -> Option<&str> {
    f.text
        .lines()
        .last()
        .filter(|l| l.starts_with(OMITTED_HEAD) && l.ends_with(OMITTED_TAIL))
}

/// Общие свойства итога: счёт не больше бюджета, оценка D22 не больше счёта, снятое названо
/// последней строкой тогда и только тогда, когда снят раздел с именем; строки без заголовка
/// (уведомления, текст подсказки) в строке снятого не называются (Minor 2 ревью задачи 4).
fn check(f: &Fitted, budget: u32) {
    assert!(f.tokens <= budget, "{} > {budget}:\n{}", f.tokens, f.text);
    assert!(approx_tokens(&f.text) <= f.tokens.max(1) || f.text.is_empty());
    assert_eq!(
        omitted_line(f).is_some(),
        f.omitted.iter().any(|n| n != NOTICES_NAME),
        "{:?}\n{}",
        f.omitted,
        f.text
    );
    if let Some(l) = omitted_line(f) {
        assert!(!l.contains(NOTICES_NAME), "{l}");
    }
}

#[test]
fn fit_8k_keeps_tasks_drops_rules_to_refs() {
    let text = fixture("rules_heavy.md");
    // Резерв Rules фикстуры — около 1312 токенов оценки D22.
    let rules: String = section(&text, "### Правила").unwrap().join("\n");
    let rules_tokens = approx_tokens(&rules);
    assert!((1250..=1350).contains(&rules_tokens), "{rules_tokens}");

    let f = fit(&text, 200, IdeasMode::Wait);
    check(&f, 200);
    assert!(f.text.starts_with("### Задачи"), "{}", f.text);
    assert!(f.text.contains("далее: write test"), "{}", f.text);
    assert!(
        f.text.contains("k:01M8SYNTH00000000000000K02"),
        "{}",
        f.text
    );
    // Правила — строками `<путь> n:<id>` или сняты: текста правил нет.
    assert!(!f.text.contains("Синтетическое правило"), "{}", f.text);
    for l in section(&f.text, "### Правила").unwrap_or_default() {
        let (path, id) = l.split_once(' ').unwrap();
        assert!(path.starts_with("synthetic/rule-"), "{l}");
        assert!(id.starts_with("n:01M8SYNTH") && id.len() == 28, "{l}");
    }
    let last = omitted_line(&f).unwrap_or_else(|| panic!("нет строки снятого:\n{}", f.text));
    assert!(last.contains("Правила"), "{last}");
    assert!(f.omitted.iter().any(|s| s == "Правила"), "{:?}", f.omitted);
}

/// Образцы разделов по таблице D141 (режим `Review`, чтобы все строки таблицы были в деле):
/// приоритет, текст и метка, по которой раздел виден в итоге.
fn samples() -> Vec<(u8, String, String)> {
    let filler = "синтетический текст раздела для проверки приоритета укладки дайджеста, \
                  достаточно длинный, чтобы два раздела не помещались в тесный бюджет вместе";
    let id = |n: usize| format!("i:01M8SYNTH00000000000000P{n:02}");
    let mut out = Vec::new();
    let mut add = |p: u8, head: Option<&str>, line: &dyn Fn(&str, &str) -> String| {
        let n = out.len();
        let mark = format!("MARK{n:02}");
        let body = line(&mark, &id(n));
        let text = match head {
            Some(h) => format!("{h}\n{body}\n"),
            None => format!("{body}\n"),
        };
        out.push((p, text, mark));
    };
    let plain = |m: &str, r: &str| format!("ZeroAIMemory: {m} {filler} {r}");
    add(PRIORITY_TASKS, Some("### Задачи"), &|m, r| {
        format!("ZeroAIMemory: [in_progress] {m} {filler} — далее: шаг — {r}")
    });
    add(
        PRIORITY_SNAPSHOT,
        Some(
            "### Последнее известное состояние (снимок 2026-11-09 10:00, извлечено автоматически)",
        ),
        &|m, r| format!("Из журнала агента: {m} {filler} {r}"),
    );
    add(3, None, &plain);
    add(4, Some("### Ответы человека"), &plain);
    add(
        PRIORITY_IDEAS_REVIEW,
        Some("### Идеи на разбор"),
        &|m, _| format!("Непросмотренных идей: 1. {m} {filler}"),
    );
    add(PRIORITY_IDEAS_REVIEW, None, &|m, r| {
        format!("План из идей: 2 открытых, следующая — {r} «{m} {filler}» [high]")
    });
    add(PRIORITY_ENVIRONMENT, Some("### Окружение"), &plain);
    add(PRIORITY_RULES, Some("### Правила"), &|m, r| {
        let r = r.replace("i:", "n:");
        format!("synthetic/rules/{m}-sample [th] {r}\n  {filler}")
    });
    add(PRIORITY_TRAPS, Some("### Ловушки и сбои"), &plain);
    add(PRIORITY_TRAPS, Some("### Ловушки"), &plain);
    for h in [
        "### Решения",
        "### Решение",
        "### Открытые вопросы",
        "### Идеи",
        "### Пересмотреть (revisit)",
        "### Последние изменения",
        "### Термины",
        "### Файлы",
        "### Код",
    ] {
        add(PRIORITY_OTHER, Some(h), &plain);
    }
    add(PRIORITY_POINTERS, None, &|m, _| {
        format!("Словарь: 4 терминов {m} {filler} — memory_search kinds=[term]")
    });
    add(PRIORITY_POINTERS, None, &|m, _| {
        format!("Проверенные команды: synthetic/{m} {filler} (проверено 2026-11-10)")
    });
    add(PRIORITY_POINTERS, None, &|m, _| {
        format!("ZeroAIMemory: карта кода: 12 файлов, {m} {filler} — `memory_code`/`zam code map`")
    });
    add(
        PRIORITY_POINTERS,
        Some("### Карта кода"),
        &|m, _| format!("crates/synthetic/src/lib.rs:1 fn {m} {filler}"),
    );
    add(PRIORITY_UNKNOWN, Some("### Новое"), &plain);
    out
}

#[test]
fn fit_priority_table() {
    let s = samples();
    let mut pairs = 0;
    for (i, (pi, senior, mark_i)) in s.iter().enumerate() {
        for (j, (pj, junior, mark_j)) in s.iter().enumerate() {
            if pi >= pj {
                continue;
            }
            // Строки до первого заголовка — уведомления: такой раздел идёт в тексте первым;
            // иначе младший — первым, чтобы проверить и порядок вывода.
            let text = if senior.starts_with("### ") {
                format!("{junior}\n{senior}")
            } else {
                format!("{senior}\n{junior}")
            };
            // Запас — на строку снятого; младший в самой короткой форме (правило — `<путь>
            // n:<id>` с заголовком) в него не влезает.
            let alone = fit(senior, 100_000, IdeasMode::Review).tokens;
            let budget = alone + 30;
            let f = fit(&text, budget, IdeasMode::Review);
            check(&f, budget);
            assert!(
                f.text.contains(mark_i.as_str()),
                "старший {i} (p{pi}) снят ради {j} (p{pj}):\n{}",
                f.text
            );
            assert!(
                !f.text.contains(mark_j.as_str()),
                "младший {j} (p{pj}) остался рядом с {i} (p{pi}):\n{}",
                f.text
            );
            pairs += 1;
        }
    }
    assert!(pairs > 200, "{pairs}");
}

#[test]
fn fit_section_header_by_prefix() {
    let head =
        "### Последнее известное состояние (снимок 2026-11-09 10:00, извлечено автоматически)";
    assert_eq!(header_priority(head), PRIORITY_SNAPSHOT);
    assert_eq!(
        header_priority(
            "### Последнее известное состояние (снимок 2026-01-01 00:00, извлечено автоматически)"
        ),
        PRIORITY_SNAPSHOT
    );
    // Снимок важнее уведомлений: при тесном бюджете уведомление снимается первым.
    let text = fixture("interrupted_session.md");
    let tasks_and_snapshot = {
        let t = section(&text, "### Задачи").unwrap().join("\n");
        let s = section(&text, head).unwrap().join("\n");
        format!("### Задачи\n{t}\n\n{head}\n{s}\n")
    };
    // Запас — на строку снятого (в ней названы все снятые разделы фикстуры).
    let budget = fit(&tasks_and_snapshot, 100_000, IdeasMode::Wait).tokens + 70;
    let f = fit(&text, budget, IdeasMode::Wait);
    check(&f, budget);
    assert!(f.text.contains(head), "{}", f.text);
    assert!(
        f.text.contains("не обновлялась с 2026-11-09 09:40"),
        "{}",
        f.text
    );
    assert!(!f.text.contains("запросы доступа"), "{}", f.text);
    // Порядок вывода — по приоритету: задачи, затем снимок.
    let t = f.text.find("### Задачи").unwrap();
    let s = f.text.find(head).unwrap();
    assert!(t < s, "{}", f.text);
}

#[test]
fn fit_unknown_section_lowest() {
    assert_eq!(header_priority("### Новое"), PRIORITY_UNKNOWN);
    // Префикс — по границе слова: «### Кодекс» — не «### Код».
    assert_eq!(header_priority("### Кодекс"), PRIORITY_UNKNOWN);
    assert_eq!(header_priority("### Код"), PRIORITY_OTHER);
    let text = "### Задачи\n\
                ZeroAIMemory: [in_progress] Синтетическая задача — далее: шаг — k:01M8SYNTH00000000000000K07\n\
                \n\
                ### Новое\n\
                ZeroAIMemory: синтетический раздел, которого нет в таблице укладки, с длинным текстом строки\n\
                \n\
                ### Последние изменения\n\
                c:01M8SYNTH00000000000000C02 «Синтетическое изменение» · 40t\n\
                Словарь: 3 терминов — memory_search kinds=[term]\n";
    let full = fit(text, 100_000, IdeasMode::Wait);
    assert!(full.omitted.is_empty(), "{:?}", full.omitted);
    let budget = full.tokens - 1;
    let f = fit(text, budget, IdeasMode::Wait);
    check(&f, budget);
    assert_eq!(f.omitted, vec!["Новое".to_owned()]);
    assert!(!f.text.contains("### Новое"), "{}", f.text);
    assert!(f.text.contains("### Последние изменения"), "{}", f.text);
    assert!(f.text.contains("Словарь: 3 терминов"), "{}", f.text);
}

#[test]
fn fit_environment_kept_for_all_models() {
    let text = fixture("interrupted_session.md");
    let env = section(&text, "### Окружение").unwrap().join("\n");
    let env = format!("### Окружение\n{env}\n");
    let rules = section(&text, "### Правила").unwrap();
    let rules: Vec<&str> = rules
        .into_iter()
        .filter(|l| !l.starts_with("Словарь:") && !l.starts_with("ZeroAIMemory: карта кода:"))
        .collect();
    // Правила в тексте раньше окружения: вывод — по приоритету.
    let both = format!("### Правила\n{}\n\n{env}", rules.join("\n"));
    let budget = fit(&env, 100_000, IdeasMode::Wait).tokens + 30;
    // Укладка не знает модели: окружение остаётся в обоих режимах идей (решение владельца 2).
    for mode in [IdeasMode::Wait, IdeasMode::Review] {
        let f = fit(&both, budget, mode);
        check(&f, budget);
        assert!(f.text.starts_with("### Окружение"), "{}", f.text);
        assert!(f.text.contains("synthetic-host"), "{}", f.text);
        assert!(!f.text.contains("Синтетическое правило"), "{}", f.text);
        assert!(f.omitted.iter().any(|s| s == "Правила"), "{:?}", f.omitted);
    }
}

#[test]
fn fit_scales_by_server_tokens() {
    let text = fixture("rules_heavy.md");
    let est = approx_tokens(&text);
    let budget = 400;
    let ours = fit_digest(&text, budget, None, IdeasMode::Wait, &none());
    let server = fit_digest(&text, budget, Some(2 * est), IdeasMode::Wait, &none());
    check(&ours, budget);
    check(&server, budget);
    // k ≥ 2: счёт итога не меньше двух оценок D22.
    assert!(
        server.tokens >= 2 * approx_tokens(&server.text),
        "{}",
        server.tokens
    );
    // По счёту сервера (вдвое больше оценки) итог в бюджете.
    assert!(2 * approx_tokens(&server.text) <= budget);
    assert!(server.text.len() < ours.text.len());
    // Сервер считает меньше оценки — k не меньше 1,1: запас 10 % остаётся.
    let small = fit_digest(&text, budget, Some(est / 2), IdeasMode::Wait, &none());
    assert_eq!(small.text, ours.text);
}

#[test]
fn fit_ideas_only_in_review() {
    let text = fixture("ideas_review_m65.md");
    let wait = fit(&text, 100_000, IdeasMode::Wait);
    check(&wait, 100_000);
    for gone in [
        "### Идеи на разбор",
        "Непросмотренных идей",
        "План из идей",
        "w:01M8SYNTH00000000000000W01",
    ] {
        assert!(!wait.text.contains(gone), "{gone}:\n{}", wait.text);
    }
    // Снятое режимом не называется: модели его не прочитать нечем.
    assert!(wait.omitted.is_empty(), "{:?}", wait.omitted);
    assert!(wait.text.contains("### Окружение"), "{}", wait.text);

    let review = fit(&text, 100_000, IdeasMode::Review);
    check(&review, 100_000);
    let pos = |s: &str| {
        review
            .text
            .find(s)
            .unwrap_or_else(|| panic!("{s}:\n{}", review.text))
    };
    // Пятый по приоритету: после ответов человека, раньше окружения.
    assert!(pos("### Ответы человека") < pos("### Идеи на разбор"));
    assert!(pos("### Идеи на разбор") < pos("### Окружение"));
    assert!(pos("Непросмотренных идей") > pos("### Идеи на разбор"));
    // В тесном бюджете раздел идей держится дольше окружения, правил и ловушек.
    let tight = fit(&text, review.tokens / 2 + 40, IdeasMode::Review);
    check(&tight, review.tokens / 2 + 40);
    assert!(tight.text.contains("### Идеи на разбор"), "{}", tight.text);
    assert!(!tight.text.contains("### Ловушки и сбои"), "{}", tight.text);
}

#[test]
fn fit_header_longest_prefix() {
    assert_eq!(header_priority("### Идеи на разбор"), PRIORITY_IDEAS_REVIEW);
    assert_eq!(header_priority("### Идеи"), PRIORITY_OTHER);
    assert_eq!(header_priority("### Ловушки и сбои"), PRIORITY_TRAPS);
    assert_eq!(header_priority("### Ловушки"), PRIORITY_TRAPS);
    assert_eq!(header_priority("### Решение"), PRIORITY_OTHER);
    assert_eq!(header_priority("### Решения"), PRIORITY_OTHER);
    // «Идеи на разбор» не принимается за «Идеи»: в `Wait` снимается только он.
    let text = "### Идеи на разбор\n\
                Непросмотренных идей: 1. Разбери, когда закончишь текущий шаг.\n\
                ZeroAIMemory: идея «Синтетическая А» [normal] — w:01M8SYNTH00000000000000W11\n\
                \n\
                ### Идеи\n\
                Предложи человеку одну: memory_update_entry action=propose.\n\
                ZeroAIMemory: идея «Синтетическая Б» [normal] — w:01M8SYNTH00000000000000W12\n";
    let f = fit(text, 100_000, IdeasMode::Wait);
    check(&f, 100_000);
    assert!(!f.text.contains("Синтетическая А"), "{}", f.text);
    assert!(f.text.starts_with("### Идеи\n"), "{}", f.text);
    assert!(f.text.contains("Синтетическая Б"), "{}", f.text);
}

#[test]
fn fit_plan_line_by_prefix() {
    let text = fixture("ideas_review_m65.md");
    let plan = "План из идей: 2 открытых";
    // Строка плана — без заголовка: в `Review` она пятая по приоритету, а в выводе стоит там,
    // где её ставит сервер, — сразу за задачами (Minor 1 ревью задачи 4). После `### Ответы
    // человека` она читалась бы частью ответов: раздел в Markdown длится до заголовка.
    let review = fit(&text, 100_000, IdeasMode::Review);
    check(&review, 100_000);
    let tasks = section(&review.text, "### Задачи").unwrap();
    assert!(tasks.last().unwrap().starts_with(plan), "{tasks:?}");
    let answers = section(&review.text, "### Ответы человека").unwrap();
    assert!(answers.iter().all(|l| !l.starts_with(plan)), "{answers:?}");
    // В `Wait` строка снимается, где бы ни стояла: и внутри другого раздела.
    let inside = "### Правила\n\
                  synthetic/rule-x [th] n:01M8SYNTH00000000000000R41\n\
                  План из идей: 1 открытых\n";
    for t in [text.as_str(), inside] {
        let f = fit(t, 100_000, IdeasMode::Wait);
        assert!(!f.text.contains("План из идей"), "{}", f.text);
    }
    // Задач нет: строка плана (5) — под заголовком `### Идеи на разбор`, раньше правил (7).
    let f = fit(inside, 100_000, IdeasMode::Review);
    check(&f, 100_000);
    assert!(
        f.text
            .starts_with("### Идеи на разбор\nПлан из идей: 1 открытых\n\n### Правила"),
        "{}",
        f.text
    );
    // Задач нет, раздел разбора есть: строка плана — в нём, а не после ответов человека.
    let no_tasks = text.replacen(
        "### Задачи\nZeroAIMemory: [in_progress] Синтетическая задача режима идей — далее: write test — k:01M8SYNTH00000000000000K05\n",
        "",
        1,
    );
    assert_ne!(no_tasks, text);
    let f = fit(&no_tasks, 100_000, IdeasMode::Review);
    check(&f, 100_000);
    let ideas = section(&f.text, "### Идеи на разбор").unwrap();
    assert!(ideas.iter().any(|l| l.starts_with(plan)), "{}", f.text);
    let answers = section(&f.text, "### Ответы человека").unwrap();
    assert!(answers.iter().all(|l| !l.starts_with(plan)), "{}", f.text);
    assert_eq!(
        f.text.matches("### Идеи на разбор").count(),
        1,
        "{}",
        f.text
    );
}

/// Minor 2 ревью задачи 4: у подсказки (`prompt`, `post_tool`) весь текст — строки без
/// заголовка; снятые бюджетом строки в строке снятого не называются — слово `notices` модели
/// ничего не говорит, а сама строка заняла бы бюджет подсказки. Данные события имя знают.
#[test]
fn fit_hint_cut_without_omitted_line() {
    let hint: Vec<String> = (1..=6)
        .map(|n| {
            format!(
                "ZeroAIMemory: синтетическая подсказка {n} с текстом для проверки укладки подсказки [th] i:01M8SYNTH00000000000000H{n:02}"
            )
        })
        .collect();
    let hint = hint.join("\n");
    let full = fit(&hint, 100_000, IdeasMode::Wait).tokens;
    let budget = full / 2;
    let f = fit(&hint, budget, IdeasMode::Wait);
    check(&f, budget);
    assert!(omitted_line(&f).is_none(), "{}", f.text);
    assert!(!f.text.contains(OMITTED_HEAD), "{}", f.text);
    assert!(f.text.contains("H01"), "{}", f.text);
    assert!(!f.text.contains("H06"), "{}", f.text);
    assert_eq!(f.omitted, vec![NOTICES_NAME.to_owned()]);
    // Рядом со снятым разделом уведомление по-прежнему не называется, раздел — называется.
    let digest = format!("{hint}\n### Ловушки и сбои\n{}\n", hint.replace("H0", "T0"));
    let f = fit(&digest, budget, IdeasMode::Wait);
    check(&f, budget);
    let line = omitted_line(&f).unwrap_or_else(|| panic!("{}", f.text));
    assert!(line.contains("Ловушки и сбои"), "{line}");
}

/// Сверка по ZAM `v0.0.12-m6.5` (`arrange`, `Arranged::notices`, `api/recall.rs`): в пакете
/// восстановления уведомления идут без заголовка сразу за задачами, ответами и решениями.
/// Клиент узнаёт их по началу строки, а не относит к предыдущему разделу: в выводе они не стоят
/// под `### Решения`, а их приоритет (3) — выше ответов (4) и решений (9).
#[test]
fn fit_recovery_notice_after_decisions() {
    let text = fixture("recovery_m65.md");
    let notice = "ZeroAIMemory: запросы доступа к вашим решениям ждут ответа";
    let f = fit(&text, 100_000, IdeasMode::Review);
    check(&f, 100_000);
    assert!(f.text.contains(notice), "{}", f.text);
    let decisions = section(&f.text, "### Решения").unwrap();
    assert!(
        decisions.iter().all(|l| !l.starts_with(notice)),
        "{decisions:?}"
    );
    // Тесный бюджет: задачи, снимок и уведомление; ответы человека и решения сняты.
    let head = "### Последнее известное состояние";
    let kept = format!(
        "### Задачи\n{}\n\n{head}{}\n{}",
        section(&text, "### Задачи").unwrap()[0],
        text.split(head)
            .nth(1)
            .unwrap()
            .split("\n\n")
            .next()
            .unwrap(),
        text.lines().find(|l| l.starts_with(notice)).unwrap(),
    );
    let budget = fit(&kept, 100_000, IdeasMode::Wait).tokens + 50;
    let f = fit(&text, budget, IdeasMode::Wait);
    check(&f, budget);
    assert!(f.text.contains(notice), "{}", f.text);
    assert!(f.text.contains(head), "{}", f.text);
    for gone in ["### Ответы человека", "### Решения"] {
        assert!(!f.text.contains(gone), "{gone}:\n{}", f.text);
    }
    assert!(
        !f.omitted.iter().any(|n| n == NOTICES_NAME),
        "{:?}",
        f.omitted
    );
}

#[test]
fn fit_wait_drops_idea_review_refs() {
    let idea = "w:01M8SYNTH00000000000000W01";
    let hint = format!(
        "ZeroAIMemory: к задаче подходит идея на разбор {idea} «Синтетическая идея» — разбери, когда закончишь текущий шаг (memory_recall event=idea_review)\n\
         ZeroAIMemory: синтетическая ловушка укладки [th] i:01M8SYNTH00000000000000I01\n"
    );
    let drop: BTreeSet<String> = [idea.to_owned()].into();
    let wait = fit_digest(&hint, 100_000, None, IdeasMode::Wait, &drop);
    check(&wait, 100_000);
    assert!(!wait.text.contains(idea), "{}", wait.text);
    assert!(!wait.text.contains("memory_recall"), "{}", wait.text);
    assert!(wait.text.contains("синтетическая ловушка"), "{}", wait.text);
    assert!(wait.omitted.is_empty(), "{:?}", wait.omitted);
    // `Review` — подсказка разбора остаётся.
    let review = fit_digest(&hint, 100_000, None, IdeasMode::Review, &drop);
    assert!(review.text.contains(idea), "{}", review.text);
    // Строка с такой ссылкой снимается и в разделе дайджеста.
    let digest = format!(
        "### Идеи\nZeroAIMemory: идея «Синтетическая» [normal] — {idea}\n\
         ZeroAIMemory: идея «Другая» [low] — w:01M8SYNTH00000000000000W03\n"
    );
    let f = fit_digest(&digest, 100_000, None, IdeasMode::Wait, &drop);
    assert!(!f.text.contains(idea), "{}", f.text);
    assert!(
        f.text.contains("w:01M8SYNTH00000000000000W03"),
        "{}",
        f.text
    );
}

#[test]
fn fit_untouched_when_budget_allows() {
    let text = fixture("interrupted_session.md");
    let f = fit(&text, 100_000, IdeasMode::Wait);
    check(&f, 100_000);
    assert!(f.omitted.is_empty());
    // Ни одна строка не потеряна (порядок — по приоритету).
    for l in text.lines().filter(|l| !l.is_empty()) {
        assert!(f.text.lines().any(|x| x == l), "{l}:\n{}", f.text);
    }
    assert!(f.text.starts_with("### Задачи"), "{}", f.text);
    assert!(f.refs.iter().any(|r| r == "k:01M8SYNTH00000000000000K03"));
    assert!(f.refs.iter().any(|r| r == "v:01M8SYNTH00000000000000V01"));
    assert!(f.refs.iter().any(|r| r == "n:01M8SYNTH00000000000000R21"));
}

#[test]
fn fit_rules_ladder_first_line_then_refs() {
    let text = fixture("rules_heavy.md");
    let rules = section(&text, "### Правила").unwrap();
    let firsts: Vec<&str> = rules
        .windows(2)
        .filter(|w| !w[0].starts_with(' ') && w[1].starts_with("  "))
        .map(|w| w[1])
        .collect();
    // Бюджет на правила по первой строке (L0): вторых строк нет, первые — все.
    let l0: String = rules
        .iter()
        .enumerate()
        .filter(|(i, l)| !l.starts_with(' ') || !rules[*i - 1].starts_with(' '))
        .map(|(_, l)| *l)
        .collect::<Vec<_>>()
        .join("\n");
    let tasks = section(&text, "### Задачи").unwrap().join("\n");
    let budget = fit(
        &format!("### Задачи\n{tasks}\n\n### Правила\n{l0}\n"),
        100_000,
        IdeasMode::Wait,
    )
    .tokens
        + 40;
    let f = fit(&text, budget, IdeasMode::Wait);
    check(&f, budget);
    for first in &firsts {
        assert!(f.text.contains(first), "{first}:\n{}", f.text);
    }
    assert!(!f.text.contains("Вторая строка правила 01"), "{}", f.text);
    assert!(
        f.text
            .contains("synthetic/rule-01 [ax] n:01M8SYNTH00000000000000R01")
    );
}

#[test]
fn fit_tiny_budget_is_empty() {
    let text = fixture("rules_heavy.md");
    for budget in [0, 1, 5] {
        let f = fit(&text, budget, IdeasMode::Wait);
        assert!(f.text.is_empty(), "{budget}: {}", f.text);
        assert_eq!(f.tokens, 0);
        assert!(f.refs.is_empty());
    }
    let empty = fit("", 200, IdeasMode::Wait);
    assert!(empty.text.is_empty() && empty.omitted.is_empty());
}

#[test]
fn fetch_budget_has_reserve_and_ceiling() {
    assert_eq!(fetch_budget(200), 800);
    assert_eq!(fetch_budget(400), 1600);
    assert_eq!(fetch_budget(1500), 6000);
    assert_eq!(fetch_budget(2000), 6000);
    assert_eq!(fetch_budget(0), 0);
}
