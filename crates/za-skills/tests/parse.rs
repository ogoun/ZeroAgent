//! Разбор `SKILL.md` (D115): корпус frontmatter, генератор случайных строк, значения полей.
//!
//! Корпус — по файлу на случай в `tests/fixtures/frontmatter/`, ожидание — в префиксе
//! имени (`ok`, `warn`, `skip`) и в таблице [`CASES`]. Случай `skip-10mib` собирается в
//! тесте: файл в 10 МиБ в репозитории не нужен.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use za_skills::{ParseError, Parsed, Value, parse_skill_md};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/frontmatter")
}

/// Множитель пределов времени (`ZA_TEST_TIME_SCALE`, целое 1–6).
fn time_scale() -> u32 {
    std::env::var("ZA_TEST_TIME_SCALE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .map_or(1, |v| v.clamp(1, 6))
}

/// Ожидание случая корпуса.
#[derive(Debug, Clone, Copy)]
enum Want {
    Ok,
    /// Предупреждение с этой подстрокой.
    Warn(&'static str),
    /// `unsupported YAML feature: …` с этой подстрокой.
    Unsupported(&'static str),
    Unparseable,
    NoFrontmatter,
    EmptyDescription,
    TooLarge,
}

const CASES: &[(&str, Want)] = &[
    ("ok-plain", Want::Ok),
    ("ok-quoted", Want::Ok),
    ("ok-block-literal", Want::Ok),
    ("ok-folded", Want::Ok),
    ("ok-flow-list", Want::Ok),
    ("ok-metadata", Want::Ok),
    ("ok-crlf-bom", Want::Ok),
    ("warn-name-mismatch", Want::Warn("does not match directory")),
    ("warn-name-long", Want::Warn("longer than 64")),
    ("warn-desc-1025", Want::Warn("description longer than 1024")),
    (
        "warn-hooks",
        Want::Warn("executable extension ignored: hooks"),
    ),
    ("warn-unknown-field", Want::Warn("unknown field model")),
    ("skip-no-fence", Want::NoFrontmatter),
    ("skip-empty-desc", Want::EmptyDescription),
    ("skip-anchor", Want::Unsupported("anchor")),
    ("skip-alias", Want::Unsupported("alias")),
    ("skip-tag", Want::Unsupported("tag")),
    ("skip-flow-map", Want::Unsupported("flow mapping")),
    ("skip-tab-indent", Want::Unparseable),
    ("skip-duplicate-key", Want::Unparseable),
    ("skip-second-document", Want::Unsupported("second document")),
    ("skip-10mib", Want::TooLarge),
];

/// Случаи, которые собираются в тесте, а не лежат файлом.
const GENERATED: &[&str] = &["skip-10mib"];

fn case_text(name: &str) -> String {
    if name == "skip-10mib" {
        let mut s = String::from("---\nname: skip-10mib\ndescription: big\n---\n");
        s.push_str(&"a".repeat(10 << 20));
        return s;
    }
    let bytes = std::fs::read(fixtures().join(format!("{name}.md"))).unwrap();
    String::from_utf8(bytes).unwrap()
}

fn parsed(name: &str) -> Parsed {
    parse_skill_md(&case_text(name), name).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn s(v: &str) -> Value {
    Value::Str(v.to_owned())
}

fn list(v: &[&str]) -> Value {
    Value::List(v.iter().map(|x| (*x).to_owned()).collect())
}

#[test]
fn frontmatter_corpus() {
    // Каждый файл корпуса есть в таблице, и каждый случай таблицы есть файлом.
    let files: BTreeSet<String> = std::fs::read_dir(fixtures())
        .unwrap()
        .map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".md").unwrap().to_owned()
        })
        .collect();
    let table: BTreeSet<String> = CASES
        .iter()
        .map(|(n, _)| (*n).to_owned())
        .filter(|n| !GENERATED.contains(&n.as_str()))
        .collect();
    assert_eq!(files, table, "корпус и таблица случаев расходятся");
    for (name, want) in CASES {
        let prefix = name.split('-').next().unwrap();
        let got = parse_skill_md(&case_text(name), name);
        match (want, &got) {
            (Want::Ok, Ok(p)) => {
                assert_eq!(prefix, "ok", "{name}");
                assert!(p.warnings.is_empty(), "{name}: {:?}", p.warnings);
                assert_eq!(p.name.as_deref(), Some(*name), "{name}");
                assert!(!p.description.is_empty(), "{name}");
            }
            (Want::Warn(text), Ok(p)) => {
                assert_eq!(prefix, "warn", "{name}");
                assert!(
                    p.warnings.iter().any(|w| w.contains(text)),
                    "{name}: нет предупреждения {text:?} в {:?}",
                    p.warnings
                );
            }
            (Want::Unsupported(what), Err(e @ ParseError::Unsupported(_))) => {
                assert_eq!(prefix, "skip", "{name}");
                let shown = e.to_string();
                assert!(
                    shown.starts_with("unsupported YAML feature: ") && shown.contains(what),
                    "{name}: {shown}"
                );
            }
            (Want::Unparseable, Err(e @ ParseError::Unparseable(_))) => {
                assert_eq!(prefix, "skip", "{name}");
                assert!(
                    e.to_string().starts_with("unparseable frontmatter: "),
                    "{name}: {e}"
                );
            }
            (Want::NoFrontmatter, Err(ParseError::NoFrontmatter))
            | (Want::EmptyDescription, Err(ParseError::EmptyDescription))
            | (Want::TooLarge, Err(ParseError::TooLarge)) => {
                assert_eq!(prefix, "skip", "{name}");
            }
            (want, got) => panic!("{name}: ожидалось {want:?}, получено {got:?}"),
        }
    }
}

/// Значения полей корпуса: кавычки и экранирования, блоки `|` и `>` с индикаторами,
/// списки, вложенная карта, CRLF и BOM.
#[test]
fn corpus_values() {
    let p = parsed("ok-plain");
    assert_eq!(
        p.description,
        "Plain description of the skill. Use when: a colon inside is fine."
    );
    assert_eq!(p.fields["license"], s("MIT"));
    assert_eq!(p.body, "# Body\n\nPlain body text.\n");

    let p = parsed("ok-quoted");
    assert_eq!(
        p.description,
        "Quoted \"text\" with \\ backslash,\ttab, \u{e9} and\nnew line"
    );
    assert_eq!(p.fields["license"], s("it's MIT"));

    let p = parsed("ok-block-literal");
    assert_eq!(p.description, "Line one\n  indented line\n\nLine four\n");
    assert_eq!(p.fields["compatibility"], s("needs git"));

    let p = parsed("ok-folded");
    assert_eq!(p.description, "Folded text continues here.\nNew paragraph.");
    assert_eq!(p.fields["license"], s("MIT\n"));

    let p = parsed("ok-flow-list");
    assert_eq!(
        p.fields["allowed-tools"],
        list(&["Read", "Grep", "Glob", "Bash(git:*)"])
    );

    let p = parsed("ok-metadata");
    assert_eq!(
        p.description,
        "Nested metadata map and a block list, with the description on the next lines."
    );
    let meta = BTreeMap::from([
        ("author".to_owned(), s("someone")),
        ("version".to_owned(), s("1.0")),
        ("tags".to_owned(), list(&["a", "b"])),
    ]);
    assert_eq!(p.fields["metadata"], Value::Map(meta));
    assert_eq!(p.fields["allowed-tools"], list(&["Read", "Bash"]));

    let p = parsed("ok-crlf-bom");
    assert_eq!(p.description, "Windows line endings and a byte order mark.");
    assert_eq!(p.body, "Body line one.\nBody line two.\n");
}

/// Сюрпризы YAML 1.1 не действуют: всё — строки.
#[test]
fn yes_no_stay_strings() {
    assert_eq!(
        parse_skill_md("---\nname: x\ndescription: no\n---\n", "x")
            .unwrap()
            .description,
        "no"
    );
    let p = parse_skill_md(
        "---\nname: x\ndescription: yes\nlicense: ~\ncompatibility: 0x1F\nmetadata:\n  a: true\n  b: 1e3\n  c: null\n---\n",
        "x",
    )
    .unwrap();
    assert_eq!(p.description, "yes");
    assert_eq!(p.fields["license"], s("~"));
    assert_eq!(p.fields["compatibility"], s("0x1F"));
    let Value::Map(m) = &p.fields["metadata"] else {
        panic!("metadata не карта: {:?}", p.fields["metadata"]);
    };
    assert_eq!(m["a"], s("true"));
    assert_eq!(m["b"], s("1e3"));
    assert_eq!(m["c"], s("null"));
}

#[test]
fn bad_name_falls_back_to_dir() {
    let p = parse_skill_md("---\nname: \"../X\"\ndescription: d\n---\n", "tdd-x").unwrap();
    assert_eq!(p.name.as_deref(), Some("tdd-x"));
    assert!(
        p.warnings.iter().any(|w| w.contains("invalid name")),
        "{:?}",
        p.warnings
    );
    // Заглавные, `--`, `-` в начале — тоже недопустимы.
    for bad in ["TDD", "a--b", "-a", "a-", "a b", "a_b"] {
        let text = format!("---\nname: '{bad}'\ndescription: d\n---\n");
        let p = parse_skill_md(&text, "tdd-x").unwrap();
        assert_eq!(p.name.as_deref(), Some("tdd-x"), "{bad}");
    }
    // Имени нет — имя каталога с предупреждением.
    let p = parse_skill_md("---\ndescription: d\n---\n", "tdd-x").unwrap();
    assert_eq!(p.name.as_deref(), Some("tdd-x"));
    assert!(!p.warnings.is_empty());
    // Недопустимы и имя, и каталог — имени нет (скилл пропускается).
    let p = parse_skill_md("---\nname: ../X\ndescription: d\n---\n", "Bad Dir").unwrap();
    assert_eq!(p.name, None);
}

/// Невидимые символы в описании удаляются уровнем `Untrusted` (D102): это текст для модели.
#[test]
fn description_is_cleaned() {
    let p = parse_skill_md(
        "---\nname: x\ndescription: \"a\\u200Bb\\u202Ec\\U000E0041d\"\n---\n",
        "x",
    )
    .unwrap();
    assert_eq!(p.description, "abcd");
    // Описание только из невидимых символов — пустое.
    let e = parse_skill_md("---\nname: x\ndescription: \"\\u200B \"\n---\n", "x");
    assert_eq!(e.unwrap_err(), ParseError::EmptyDescription);
}

/// Детерминированный генератор xorshift64 (без зависимостей).
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// D115: 10⁴ случайных строк из алфавита YAML-индикаторов и букв (плюс пробел и обратная
/// косая — иначе не складываются `key: value` и экранирования). Разбор не паникует, и
/// каждый вызов укладывается в предел времени.
#[test]
fn random_frontmatter_never_panics() {
    const ALPHABET: &[char] = &[
        '-', ':', '\'', '"', '|', '>', '#', '[', ']', '{', '}', '&', '*', '!', '\t', '\n', 'a',
        'b', 'n', 'u', 'x', 'D', ' ', ' ', ' ', '\\', '0',
    ];
    let limit = Duration::from_millis(250) * time_scale();
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let mut slowest = Duration::ZERO;
    for i in 0..10_000 {
        let len = rng.below(240);
        let body: String = (0..len)
            .map(|_| ALPHABET[rng.below(ALPHABET.len())])
            .collect();
        // Чаще всего — внутри ограждения, чтобы работал разбор YAML, а не только поиск
        // frontmatter.
        let text = match i % 4 {
            0 => body,
            1 => format!("---\nname: x\n{body}\n---\nbody"),
            _ => format!("---\n{body}\n---\n"),
        };
        let started = Instant::now();
        let r = std::panic::catch_unwind(|| parse_skill_md(&text, "x"));
        let spent = started.elapsed();
        assert!(r.is_ok(), "паника на входе {text:?}");
        assert!(spent < limit, "вход {text:?}: {spent:?} дольше {limit:?}");
        slowest = slowest.max(spent);
    }
    eprintln!("самый медленный вызов: {slowest:?}");
}

/// Худшие входы у предела frontmatter (64 КиБ): разбор линейный, а не квадратичный.
#[test]
fn large_inputs_stay_fast() {
    let limit = Duration::from_secs(1) * time_scale();
    let fm = |inner: String| format!("---\n{inner}\n---\nbody");
    let cases = [
        // Длинный многострочный текст в кавычках с экранированием конца строки.
        fm(format!(
            "name: x\ndescription: \"{}end\"",
            "a \\\n ".repeat(10_000)
        )),
        // Длинный многострочный простой скаляр.
        fm(format!(
            "name: x\ndescription: a\n{}",
            "  more text\n".repeat(5_000)
        )),
        // Длинный блочный список.
        fm(format!(
            "name: x\ndescription: d\nallowed-tools:\n{}",
            "  - Read\n".repeat(7_000)
        )),
        // Длинный блок `>` со сменой отступов.
        fm(format!(
            "name: x\ndescription: >\n{}",
            "  line\n    more\n\n".repeat(3_500)
        )),
        // Длинная строка flow-списка.
        fm(format!(
            "name: x\ndescription: d\nallowed-tools: [{}a]",
            "a, ".repeat(20_000)
        )),
        // Много ключей.
        fm((0..5_000).map(|i| format!("k{i}: v\n")).collect()),
        // Незакрытая кавычка через много строк.
        fm(format!("name: x\ndescription: \"{}", " a\n".repeat(20_000))),
    ];
    for (i, text) in cases.iter().enumerate() {
        let started = Instant::now();
        let r = parse_skill_md(text, "x");
        let spent = started.elapsed();
        assert!(
            spent < limit,
            "случай {i}: {spent:?} дольше {limit:?} ({:?})",
            r.map(|p| p.warnings.len())
        );
    }
}

/// Пределы D115: frontmatter ≤ 64 КиБ, файл ≤ 256 КиБ.
#[test]
fn size_limits() {
    let big_fm = format!(
        "---\nname: x\ndescription: d\nlicense: {}\n---\n",
        "a".repeat(64 << 10)
    );
    assert_eq!(
        parse_skill_md(&big_fm, "x").unwrap_err(),
        ParseError::TooLarge
    );
    let head = "---\nname: x\ndescription: d\n---\n";
    let at_limit = format!("{head}{}", "b".repeat((256 << 10) - head.len()));
    assert_eq!(at_limit.len(), 256 << 10);
    assert!(parse_skill_md(&at_limit, "x").is_ok());
    let over = format!("{at_limit}b");
    assert_eq!(
        parse_skill_md(&over, "x").unwrap_err(),
        ParseError::TooLarge
    );
}
