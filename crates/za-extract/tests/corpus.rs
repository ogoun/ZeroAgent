//! Закреплённый корпус извлечения HTML (m4 финального ревью извлечения): вывод
//! [`html_to_markdown`] сверяется с хэшами `tests/corpus/expected.txt`, поэтому любое
//! изменение вывода видно в CI, а не только в отчёте прогона.
//!
//! - **Синтетические страницы** (десять, генераторы — `tests/corpus/pages.rs`) сверяются
//!   всегда: генераторы детерминированы.
//! - **Реальные страницы** (Docs.rs, GitHub, HN, MDN, WHATWG, две статьи Википедии) в
//!   репозиторий не входят: это чужое содержимое. Тест сверяет их, если каталог со снимками
//!   есть (`ZA_CORPUS_REAL`, по умолчанию `.tmp/review7-pages` в корне workspace) и входной
//!   хэш страницы совпал с записанным; иначе страница пропускается с пометкой.
//!
//! Запись `expected.txt`: `<имя> <BLAKE3 входа> <BLAKE3 записи> <байт текста> <simplified>`.
//! Запись страницы — `{title:?}\n{simplified}\n{text}`, тот же вид, что у дампов прежних
//! прогонов корпуса (`.tmp/fix10b`, `.tmp/task14/corpus-after`): хэш записи равен хэшу файла
//! дампа.
//!
//! - `ZA_CORPUS_DUMP=<метка>` пишет записи в `.tmp/corpus/<метка>/<имя>.md` (сверка `cmp`
//!   с другим прогоном);
//! - `ZA_UPDATE_GOLDEN=1` переписывает `expected.txt`. Изменение вывода — осознанное
//!   решение: причина пишется в журнал (`docs/journal.md`).

#[path = "corpus/pages.rs"]
mod pages;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use za_extract::html_to_markdown;

/// Страница корпуса: имя, вход и откуда он взят.
struct Page {
    name: String,
    html: String,
    real: bool,
}

/// Закреплённые значения страницы.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pinned {
    input: String,
    output: String,
    bytes: usize,
    simplified: bool,
}

impl Pinned {
    fn line(&self, name: &str) -> String {
        format!(
            "{name} {} {} {} {}",
            self.input, self.output, self.bytes, self.simplified
        )
    }
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn expected_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/expected.txt")
}

fn hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Десять синтетических страниц: `syn_<имя>`.
fn synthetic() -> Vec<Page> {
    let mut out: Vec<Page> = pages::realistic_pages()
        .into_iter()
        .map(|(n, html)| Page {
            name: format!("syn_{n}"),
            html,
            real: false,
        })
        .collect();
    for (n, html) in [
        ("wiki_like", pages::wiki_like()),
        ("mdn_like", pages::mdn_like()),
    ] {
        out.push(Page {
            name: format!("syn_{n}"),
            html,
            real: false,
        });
    }
    out
}

/// Реальные страницы из каталога снимков (`real_<имя файла>`), по имени; каталога нет —
/// пусто.
fn real() -> Vec<Page> {
    let dir = std::env::var_os("ZA_CORPUS_REAL")
        .map_or_else(|| workspace().join(".tmp/review7-pages"), PathBuf::from);
    let Ok(items) = std::fs::read_dir(&dir) else {
        println!("реальные страницы: нет каталога {}", dir.display());
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = items
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "html"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| {
            let bytes = std::fs::read(&p).ok()?;
            let stem = p.file_stem()?.to_string_lossy().into_owned();
            Some(Page {
                name: format!("real_{stem}"),
                html: String::from_utf8_lossy(&bytes).into_owned(),
                real: true,
            })
        })
        .collect()
}

/// Записанные значения `expected.txt`; строки `#` и пустые пропускаются.
fn read_expected() -> BTreeMap<String, Pinned> {
    let text = std::fs::read_to_string(expected_path()).unwrap_or_default();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            assert_eq!(f.len(), 5, "строка expected.txt: {l}");
            (
                f[0].to_owned(),
                Pinned {
                    input: f[1].to_owned(),
                    output: f[2].to_owned(),
                    bytes: f[3].parse().expect("байт текста"),
                    simplified: f[4].parse().expect("simplified"),
                },
            )
        })
        .collect()
}

fn write_expected(all: &BTreeMap<String, Pinned>) {
    let mut text = String::from(
        "# Корпус za-extract: <имя> <BLAKE3 входа> <BLAKE3 записи> <байт текста> <simplified>.\n\
         # Пишется командой ZA_UPDATE_GOLDEN=1 cargo test -p za-extract --test corpus;\n\
         # причину изменения вывода записать в docs/journal.md.\n",
    );
    for (name, p) in all {
        text.push_str(&p.line(name));
        text.push('\n');
    }
    std::fs::write(expected_path(), text).expect("запись expected.txt");
}

/// Вывод каждой страницы корпуса совпадает с закреплённым (см. документацию модуля).
#[test]
fn corpus_output_is_pinned() {
    let dump = std::env::var("ZA_CORPUS_DUMP")
        .ok()
        .filter(|l| !l.is_empty())
        .map(|l| workspace().join(".tmp/corpus").join(l));
    if let Some(d) = &dump {
        std::fs::create_dir_all(d).expect("каталог дампа");
    }
    let expected = read_expected();
    let mut seen = expected.clone();
    let mut wrong = Vec::new();
    let pages: Vec<Page> = synthetic().into_iter().chain(real()).collect();
    for page in &pages {
        let m = html_to_markdown(&page.html);
        let record = format!("{:?}\n{}\n{}", m.title, m.simplified, m.text);
        if let Some(d) = &dump {
            std::fs::write(d.join(format!("{}.md", page.name)), &record).expect("дамп");
        }
        let got = Pinned {
            input: hex(page.html.as_bytes()),
            output: hex(record.as_bytes()),
            bytes: m.text.len(),
            simplified: m.simplified,
        };
        println!("{}", got.line(&page.name));
        match expected.get(&page.name) {
            // Другой снимок реальной страницы: сверять не с чем.
            Some(e) if page.real && e.input != got.input => {
                println!("{}: другой снимок страницы, пропуск", page.name);
                continue;
            }
            Some(e) if *e == got => {}
            Some(e) => wrong.push(format!("{}: ждали {e:?}, получили {got:?}", page.name)),
            None => wrong.push(format!("{}: нет в expected.txt", page.name)),
        }
        seen.insert(page.name.clone(), got);
    }
    if std::env::var("ZA_UPDATE_GOLDEN").as_deref() == Ok("1") {
        write_expected(&seen);
        return;
    }
    assert!(
        wrong.is_empty(),
        "вывод корпуса изменился (обновить осознанно: ZA_UPDATE_GOLDEN=1, причина — в журнал):\n{}",
        wrong.join("\n")
    );
    let synthetic_pinned = expected.keys().filter(|n| n.starts_with("syn_")).count();
    assert_eq!(
        synthetic_pinned, 10,
        "в expected.txt не все синтетические страницы"
    );
}
