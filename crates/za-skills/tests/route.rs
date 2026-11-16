//! Роутер карточек tier small (R1-C2 D119): кандидат — при двух и более общих словах с
//! наибольшим весом idf; одно слово, ничья и пустой ввод — без кандидата; стоп-слова и
//! слова короче трёх символов не считаются.

use za_skills::{Catalog, DiscoverInput, ROUTE_MIN_WORDS, discover, route};

/// Поставка-заглушка: тексты `SKILL.md` по именам каталогов.
static STUBS: &[(&str, &str)] = &[
    (
        "tdd",
        "---\nname: tdd\ndescription: Write a failing test first, then make it pass.\n---\nRed, green, refactor.\n",
    ),
    (
        "review",
        "---\nname: review\ndescription: Review a diff or a pull request before merge.\n---\nRead the diff.\n",
    ),
    (
        "deploy",
        "---\nname: deploy\ndescription: Deploy the service to production servers.\n---\nShip it.\n",
    ),
    (
        "release",
        "---\nname: release\ndescription: Deploy the service release to production.\n---\nTag it.\n",
    ),
    (
        "chatter",
        "---\nname: chatter\ndescription: This is about that, and with these there they were.\n---\nNoise.\n",
    ),
];

fn catalog() -> Catalog {
    let c = discover(&DiscoverInput {
        bundled: STUBS,
        ..DiscoverInput::default()
    });
    assert_eq!(c.skills.len(), STUBS.len(), "{:?}", c.skipped);
    c
}

fn routed(c: &Catalog, text: &str) -> Option<String> {
    route(c, text).map(|s| s.name.clone())
}

#[test]
fn route_table() {
    assert_eq!(ROUTE_MIN_WORDS, 2);
    let c = catalog();
    let table: &[(&str, Option<&str>)] = &[
        // Четыре общих слова (write, failing, test, first): кандидат.
        ("write a failing test first for the parser", Some("tdd")),
        // Два слова: порог достигнут.
        ("review this diff", Some("review")),
        // Одно общее слово (имя скилла) — мало.
        ("please review", None),
        ("tdd please", None),
        // Ничья: у deploy и release общие deploy и production с одинаковым весом.
        ("deploy production", None),
        // Третье слово (servers) только у deploy — ничьей нет.
        ("deploy production servers", Some("deploy")),
        // Стоп-слова не считаются: иначе выиграл бы chatter (this, that, with, these).
        (
            "this that with these deploy production servers",
            Some("deploy"),
        ),
        // Только стоп-слова и короткие слова — пустой запрос.
        ("what should they have been there", None),
        ("a an to of in on", None),
        ("", None),
    ];
    for (text, want) in table {
        assert_eq!(routed(&c, text).as_deref(), *want, "{text:?}");
    }
}

#[test]
fn route_weighs_rare_words_higher() {
    // У review и tdd по два общих слова, но «merge» есть только у review, а «test» — у
    // tdd и у дополнительного скилла: вес idf решает в пользу review.
    static MORE: &[(&str, &str)] = &[
        (
            "tdd",
            "---\nname: tdd\ndescription: Write a test and code.\n---\nb\n",
        ),
        (
            "review",
            "---\nname: review\ndescription: Code review before merge.\n---\nb\n",
        ),
        (
            "bench",
            "---\nname: bench\ndescription: Benchmark test of code speed.\n---\nb\n",
        ),
    ];
    let c = discover(&DiscoverInput {
        bundled: MORE,
        ..DiscoverInput::default()
    });
    assert_eq!(c.skills.len(), 3);
    // code — у всех трёх, test — у двух, merge — у одного. У каждого по два общих слова,
    // но редкое merge весит больше test: review впереди tdd и bench (они равны между
    // собой, но ниже лучшего).
    assert_eq!(routed(&c, "test merge code").as_deref(), Some("review"));
    assert_eq!(
        routed(&c, "test code").as_deref(),
        None,
        "ничья tdd и bench"
    );
    // Пустой каталог — без кандидата.
    assert_eq!(
        routed(&Catalog::default(), "write a failing test first"),
        None
    );
}
