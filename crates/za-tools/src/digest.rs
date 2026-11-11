//! Сжатие вывода `bash` по типу (R1-C1 D86, CTX-007; D51 R1-B2, Carry 4).
//!
//! Только для одной простой команды ([`za_core::shell::is_simple`]: без конвейера,
//! подстановок и перенаправлений, кроме `2>&1`). Тип узнаётся по имени команды и
//! подкоманде ([`command_args`]), JSON — по самому выводу. Результат кладётся в
//! `ToolOutput.digest`; пакет берёт его вместо головы и хвоста, когда вывод длиннее
//! лимита (D44), а полный текст всё равно уходит в выгрузку.
//!
//! - Тесты: строки с признаками ошибки по 2 строки вокруг (не больше 60 % лимита), затем
//!   последние 40 строк. Строки с ошибками не теряются: при нехватке места режутся
//!   длинные строки, а не совпадения.
//! - `git diff`, `git show`: таблица `файл +N -M`, затем хунки до лимита.
//! - `ls`, `dir`, `Get-ChildItem`, `tree`, `find`: первые 200 строк.
//! - JSON (весь stdout): скелет до глубины 3, ключи по алфавиту (`serde_json` без
//!   `preserve_order`).
//!
//! Неизвестный тип — `None`: пакет режет вывод как обычно.

use serde_json::Value;
use za_core::shell::{Segment, command_args, is_simple};

/// Признаки строки с ошибкой в логе тестов (D86).
const FAIL_MARKS: &[&str] = &["FAILED", "FAIL", "panicked", "Error", "error:", "assert"];
/// Строк контекста вокруг строки с ошибкой.
const CONTEXT: usize = 2;
/// Доля лимита под строки с ошибками, в процентах.
const MATCH_SHARE: usize = 60;
/// Строк хвоста лога тестов.
const TAIL: usize = 40;
/// Строк начала списка файлов.
const LIST_HEAD: usize = 200;
/// Глубина скелета JSON.
const JSON_DEPTH: usize = 3;
/// Ключей объекта в скелете, дальше — `... N more keys`.
const JSON_KEYS: usize = 50;
/// Скелет JSON короче этого не режется лимитом: он и так мал (глубина и число ключей
/// ограничены), а пакет берёт digest только при выводе длиннее лимита.
const JSON_MIN: usize = 1024;
/// Самая длинная строка в сжатом тексте, байт.
const LINE_MAX: usize = 500;
/// Хвост обрезанной строки.
const LINE_CUT: &str = " ... [line cut]";

/// Подкоманды тестов у менеджеров пакетов и сборщиков.
const TEST_SUB: &[(&str, &str)] = &[
    ("cargo", "test"),
    ("cargo", "nextest"),
    ("go", "test"),
    ("npm", "test"),
    ("pnpm", "test"),
    ("yarn", "test"),
    ("dotnet", "test"),
    ("mvn", "test"),
    ("gradle", "test"),
    ("gradlew", "test"),
];
/// Команды-раннеры тестов без подкоманды.
const TEST_CMD: &[&str] = &["pytest", "jest", "vitest"];
/// Команды списка файлов.
const LIST_CMD: &[&str] = &["ls", "dir", "get-childitem", "gci", "tree", "find"];
/// Глобальные опции `git` со значением отдельным словом.
const GIT_VALUE_OPTS: &[&str] = &["-C", "-c", "--git-dir", "--work-tree", "--namespace"];

// Тексты для модели (D23, D94): английский, только ASCII-пунктуация.

/// Заголовок сжатого лога тестов.
pub(crate) fn tests_header(lines: usize, matched: usize) -> String {
    format!("[test output digest: {lines} lines, {matched} matching; last {TAIL} lines below]")
}

/// Пропуск между участками.
pub(crate) const GAP: &str = "...";
/// Совпадения не поместились.
pub(crate) fn more_matches(n: usize) -> String {
    format!("... {n} more matching lines omitted")
}
/// Начало хвоста лога.
pub(crate) const TAIL_HEADER: &str = "--- last lines ---";
/// Заголовок таблицы diff.
pub(crate) fn diff_header(files: usize, add: usize, del: usize) -> String {
    format!("[git diff digest: {files} files, +{add} -{del}]")
}
/// Начало хунков.
pub(crate) const HUNKS_HEADER: &str = "--- hunks ---";
/// Хунки не поместились.
pub(crate) fn more_lines(n: usize) -> String {
    format!("... {n} more lines")
}
/// Строки списка не показаны.
pub(crate) fn more_entries(n: usize) -> String {
    format!("... {n} more entries")
}
/// Ключи объекта JSON не показаны.
pub(crate) fn more_keys(n: usize) -> String {
    format!("... {n} more keys")
}
/// Начало stderr после сжатого stdout.
pub(crate) const STDERR_HEADER: &str = "--- stderr ---";

/// Сжатое представление вывода по типу команды (D86); `None` — тип не узнан или команда
/// не простая. `limit` — предел длины результата в байтах (у скелета JSON — не меньше
/// [`JSON_MIN`]).
pub fn digest(segs: &[Segment], stdout: &str, stderr: &str, limit: usize) -> Option<String> {
    if !is_simple(segs) {
        return None;
    }
    let (name, args) = command_args(segs.first()?)?;
    let sub = args.first().map(String::as_str);
    let is_test = TEST_CMD.contains(&name.as_str())
        || TEST_SUB.iter().any(|(n, s)| *n == name && sub == Some(*s))
        || (matches!(name.as_str(), "npm" | "pnpm" | "yarn")
            && sub == Some("run")
            && args.get(1).map(String::as_str) == Some("test"))
        || (matches!(name.as_str(), "python" | "python3" | "py")
            && args.windows(2).any(|w| w[0] == "-m" && w[1] == "pytest"));
    if is_test {
        return Some(tests(stdout, stderr, limit));
    }
    if name == "git" && matches!(git_sub(&args), Some("diff" | "show")) {
        return git_diff(stdout, limit).map(|d| with_stderr(d, stderr, limit));
    }
    if LIST_CMD.contains(&name.as_str()) {
        return Some(with_stderr(listing(stdout, limit), stderr, limit));
    }
    json_skeleton(stdout).map(|s| cut(&s, limit.max(JSON_MIN)))
}

/// Подкоманда `git` после глобальных опций.
fn git_sub(args: &[String]) -> Option<&str> {
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if GIT_VALUE_OPTS.contains(&a.as_str()) {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            return Some(a.as_str());
        }
    }
    None
}

/// Строка не длиннее [`LINE_MAX`] байт (по границе символа).
fn short(line: &str) -> String {
    if line.len() <= LINE_MAX {
        return line.to_owned();
    }
    let mut end = LINE_MAX - LINE_CUT.len();
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{LINE_CUT}", &line[..end])
}

/// Текст не длиннее `limit` байт (по границе символа).
fn cut(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_owned();
    }
    let mut end = limit;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// Набор строк с бюджетом в байтах: строка добавляется, только если помещается целиком.
struct Budget {
    out: Vec<String>,
    used: usize,
    limit: usize,
}

impl Budget {
    fn new(limit: usize) -> Self {
        Self {
            out: Vec::new(),
            used: 0,
            limit,
        }
    }

    /// Остаток бюджета.
    fn left(&self) -> usize {
        self.limit.saturating_sub(self.used)
    }

    /// Добавляет строку (с переводом строки); не помещается — `false`.
    fn push(&mut self, line: String) -> bool {
        let need = line.len() + usize::from(!self.out.is_empty());
        if need > self.left() {
            return false;
        }
        self.used += need;
        self.out.push(line);
        true
    }

    fn text(self) -> String {
        self.out.join("\n")
    }
}

/// Лог тестов: заголовок, участки со строками ошибок, хвост.
fn tests(stdout: &str, stderr: &str, limit: usize) -> String {
    let lines: Vec<&str> = stdout.lines().chain(stderr.lines()).collect();
    let tail_start = lines.len().saturating_sub(TAIL);
    let hits: Vec<usize> = (0..tail_start)
        .filter(|&i| FAIL_MARKS.iter().any(|m| lines[i].contains(m)))
        .collect();
    // В заголовке — все совпадения, и в хвосте тоже (m8 ревью).
    let total_hits = hits.len()
        + lines[tail_start..]
            .iter()
            .filter(|l| FAIL_MARKS.iter().any(|m| l.contains(m)))
            .count();
    let mut b = Budget::new(limit);
    b.push(tests_header(lines.len(), total_hits));
    // Участки: совпадение ± CONTEXT, без захода в хвост.
    let match_limit = limit * MATCH_SHARE / 100;
    let mut shown_to = 0usize; // первая строка после последней показанной
    let mut shown_hits = 0usize;
    for &h in &hits {
        let from = h.saturating_sub(CONTEXT).max(shown_to);
        let to = (h + CONTEXT + 1).min(tail_start);
        if from >= to {
            shown_hits += 1;
            continue;
        }
        let region: Vec<String> = (from..to).map(|i| short(lines[i])).collect();
        let gap = from > shown_to;
        let need: usize =
            region.iter().map(|l| l.len() + 1).sum::<usize>() + if gap { GAP.len() + 1 } else { 0 };
        if b.used + need > match_limit {
            // Без контекста — только сама строка ошибки, если помещается.
            let line = short(lines[h]);
            let gap = h > shown_to;
            let need = line.len() + 1 + if gap { GAP.len() + 1 } else { 0 };
            if b.used + need > match_limit {
                break;
            }
            if gap {
                b.push(GAP.to_owned());
            }
            b.push(line);
            shown_to = h + 1;
            shown_hits += 1;
            continue;
        }
        if gap {
            b.push(GAP.to_owned());
        }
        for l in region {
            b.push(l);
        }
        shown_to = to;
        shown_hits += 1;
    }
    if shown_hits < hits.len() {
        b.push(more_matches(hits.len() - shown_hits));
    }
    // Хвост: последние строки, что помещаются в остаток.
    if tail_start < lines.len() {
        let header_need = TAIL_HEADER.len() + 1;
        let mut tail: Vec<String> = Vec::new();
        let mut used = header_need;
        for l in lines[tail_start..].iter().rev() {
            let s = short(l);
            if used + s.len() + 1 > b.left() {
                break;
            }
            used += s.len() + 1;
            tail.push(s);
        }
        if !tail.is_empty() {
            b.push(TAIL_HEADER.to_owned());
            for l in tail.into_iter().rev() {
                b.push(l);
            }
        }
    }
    b.text()
}

/// Файл diff и счёт строк.
struct FileStat {
    name: String,
    add: usize,
    del: usize,
}

/// Таблица `файл +N -M` и хунки; без `diff --git` — `None`.
fn git_diff(stdout: &str, limit: usize) -> Option<String> {
    let lines: Vec<&str> = stdout.lines().collect();
    let first = lines.iter().position(|l| l.starts_with("diff --git "))?;
    let mut files: Vec<FileStat> = Vec::new();
    let mut in_hunk = false;
    for l in &lines[first..] {
        if let Some(rest) = l.strip_prefix("diff --git ") {
            // `a/x b/x`: имя — после ` b/`; `+++ b/x` уточнит его ниже.
            let name = rest.rsplit_once(" b/").map_or(rest, |(_, b)| b).to_owned();
            files.push(FileStat {
                name,
                add: 0,
                del: 0,
            });
            in_hunk = false;
            continue;
        }
        let Some(f) = files.last_mut() else { continue };
        if !in_hunk {
            if let Some(n) = l.strip_prefix("+++ b/") {
                f.name = n.to_owned();
            }
            if l.starts_with("@@") {
                in_hunk = true;
            }
            continue;
        }
        if l.starts_with('+') {
            f.add += 1;
        } else if l.starts_with('-') {
            f.del += 1;
        } else if l.starts_with("@@") {
            in_hunk = true;
        }
    }
    let (add, del) = files
        .iter()
        .fold((0, 0), |(a, d), f| (a + f.add, d + f.del));
    let mut b = Budget::new(limit);
    b.push(diff_header(files.len(), add, del));
    // Шапка `git show` (коммит, автор, сообщение) — до таблицы, сколько поместится в
    // половину лимита вместе с таблицей.
    let half = limit / 2;
    for l in lines[..first].iter().take(20) {
        if b.used + l.len() + 1 > half / 2 {
            break;
        }
        b.push(short(l));
    }
    let mut shown = 0;
    for f in &files {
        let row = short(&format!("{} +{} -{}", f.name, f.add, f.del));
        if b.used + row.len() + 1 > half || !b.push(row) {
            break;
        }
        shown += 1;
    }
    if shown < files.len() {
        b.push(more_entries(files.len() - shown));
    }
    if !b.push(HUNKS_HEADER.to_owned()) {
        return Some(b.text());
    }
    let body = &lines[first..];
    let reserve = more_lines(body.len()).len() + 1;
    let mut i = 0;
    while i < body.len() {
        let l = short(body[i]);
        if b.used + l.len() + 1 + reserve > limit {
            break;
        }
        b.push(l);
        i += 1;
    }
    if i < body.len() {
        b.push(more_lines(body.len() - i));
    }
    Some(b.text())
}

/// Начало списка файлов.
fn listing(stdout: &str, limit: usize) -> String {
    let lines: Vec<&str> = stdout.lines().collect();
    let reserve = more_entries(lines.len()).len() + 1;
    let mut b = Budget::new(limit);
    let mut shown = 0;
    for l in lines.iter().take(LIST_HEAD) {
        let s = short(l);
        if b.used + s.len() + 1 + reserve > limit {
            break;
        }
        b.push(s);
        shown += 1;
    }
    if shown < lines.len() {
        b.push(more_entries(lines.len() - shown));
    }
    b.text()
}

/// Дописывает начало stderr в остаток лимита.
fn with_stderr(body: String, stderr: &str, limit: usize) -> String {
    if stderr.trim().is_empty() {
        return body;
    }
    let mut b = Budget::new(limit);
    b.used = body.len();
    b.out.push(body);
    if b.push(STDERR_HEADER.to_owned()) {
        for l in stderr.lines() {
            if !b.push(short(l)) {
                break;
            }
        }
    }
    b.text()
}

/// Скелет JSON, если весь stdout — объект или массив JSON.
fn json_skeleton(stdout: &str) -> Option<String> {
    let t = stdout.trim();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    let v: Value = serde_json::from_str(t).ok()?;
    let mut out = String::new();
    skeleton(&v, 1, &mut out);
    Some(out)
}

fn skeleton(v: &Value, depth: usize, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(_) => out.push_str("bool"),
        Value::Number(_) => out.push_str("number"),
        Value::String(_) => out.push_str("string"),
        Value::Array(a) => {
            out.push_str(&format!("array[{}]", a.len()));
            if let Some(first @ (Value::Object(_) | Value::Array(_))) = a.first()
                && depth < JSON_DEPTH
            {
                out.push_str(" of ");
                skeleton(first, depth + 1, out);
            }
        }
        Value::Object(m) if depth > JSON_DEPTH => {
            out.push_str(if m.is_empty() { "{}" } else { "{...}" });
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, val)) in m.iter().take(JSON_KEYS).enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(k);
                out.push_str(": ");
                skeleton(val, depth + 1, out);
            }
            if m.len() > JSON_KEYS {
                out.push_str(", ");
                out.push_str(&more_keys(m.len() - JSON_KEYS));
            }
            out.push('}');
        }
    }
}
