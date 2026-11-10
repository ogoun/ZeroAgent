//! Выгрузка большого вывода инструмента (CTX-006, R1-B2 D44; SEC-004, ARC-021).
//!
//! Вывод длиннее лимита [`spill_limit_bytes`] уходит в контекст головой (60 % лимита)
//! и хвостом (40 %) по целым строкам, а между ними — строка-указатель
//! `… omitted N lines; search .zero-agent/work/spill/<id>.txt`. Если первая строка
//! длиннее доли головы (или последняя — доли хвоста), срез идёт по границе символа
//! UTF-8, и указатель — `… omitted N bytes; search …`. Лимит сверяется с текстом
//! **после** [`crate::escape_untrusted`]: экранирование растит `<` вчетверо, и лимит до
//! него превышался бы до 4 раз. В файл пишется исходный вывод.
//!
//! Файлы лежат только в `<project>/.zero-agent/work/spill/`; каталог
//! `.zero-agent/work/` получает `.gitignore` со строкой `*`. Запись — через временный
//! файл и переименование: оборванная запись не оставляет полуфайла под именем spill.

use std::io;
use std::path::{Path, PathBuf};

use za_protocol::{EffectKey, RunId};

/// Каталог выгрузки относительно корня проекта (D44, SEC-004).
pub const SPILL_DIR: &str = ".zero-agent/work/spill";
/// Каталог рабочих файлов агента: в нём `.gitignore` (ARC-021).
const WORK_DIR: &str = ".zero-agent/work";
/// Содержимое `.zero-agent/work/.gitignore`.
const WORK_GITIGNORE: &str = "*\n";
/// Доля головы в лимите, проценты (D44); хвост — остаток.
const HEAD_PERCENT: usize = 60;
/// Начало строки-указателя пропуска.
const OMITTED: &str = "… omitted ";
/// Указатель на файл выгрузки: `search <путь>`; перед ним в строке пропуска — `; `.
const SEARCH: &str = "search ";
/// Указатель, если файл выгрузки записать не удалось.
pub(crate) const NOT_SAVED: &str = "full output not saved";

/// Итог выгрузки.
#[derive(Debug, Clone, PartialEq)]
pub struct SpillResult {
    /// Текст для контекста: весь экранированный вывод или голова, указатель и хвост.
    pub content: String,
    /// Путь файла выгрузки относительно корня проекта, через `/`; `None` — вывод в лимите.
    pub path: Option<String>,
    /// Пропущено целых строк (0 при разрезе внутри строки).
    pub omitted_lines: usize,
    /// Пропущено байт экранированного текста.
    pub omitted_bytes: usize,
}

/// Параметры выгрузки run (`ToolEnv::spill`): окно модели и `spill_share` (D44, D65).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpillCfg {
    pub n_ctx: u32,
    pub share: f32,
}

/// Лимит вывода в байтах (D44): `⌊spill_share × n_ctx × 4⌋`; `ToolSpec.output_limit`
/// уменьшает его, но не увеличивает.
pub fn spill_limit_bytes(n_ctx: u32, share: f32, tool_limit: Option<usize>) -> usize {
    let bytes = (f64::from(share) * f64::from(n_ctx) * 4.0).floor();
    // `share` проверен конфигурацией (D65); NaN или отрицательное — лимит 0.
    let bytes = if bytes.is_finite() && bytes > 0.0 {
        bytes as usize
    } else {
        0
    };
    tool_limit.map_or(bytes, |t| t.min(bytes))
}

/// Имя файла выгрузки вывода инструмента: ключ эффекта с `:` → `-` (D44).
pub fn spill_name(key: &EffectKey) -> String {
    safe_name(&key.0.replace(':', "-"))
}

/// Имя файла выгрузки результата при удалении (D45): `{run}-{call_id}`. `call_id`
/// уникален только в одном ответе: детерминированные id нормализации повторяются между
/// ходами, поэтому второе и следующие появления того же id в истории получают
/// суффикс `-{n}` (порядковый номер появления), иначе разные результаты делили бы файл.
pub(crate) fn prune_name(run: RunId, call_id: &str, occurrence: usize) -> String {
    let base = safe_name(&format!("{run}-{}", call_id.replace(':', "-")));
    if occurrence > 1 {
        format!("{base}-{occurrence}")
    } else {
        base
    }
}

/// Путь файла выгрузки относительно корня проекта, через `/`.
pub(crate) fn spill_path(name: &str) -> String {
    format!("{SPILL_DIR}/{name}.txt")
}

/// Выгрузка вывода (D44). `escaped` — вывод после `escape_untrusted` (по нему лимит,
/// голова и хвост), `raw` — исходный (в файл). Вывод в лимите возвращается как есть,
/// файл не пишется. Каталог создаётся, `.zero-agent/work/.gitignore` = `"*\n"`.
pub fn spill(
    project_dir: &Path,
    name: &str,
    escaped: &str,
    raw: &str,
    limit_bytes: usize,
) -> io::Result<SpillResult> {
    if escaped.len() <= limit_bytes {
        return Ok(SpillResult {
            content: escaped.to_owned(),
            path: None,
            omitted_lines: 0,
            omitted_bytes: 0,
        });
    }
    write_spill(project_dir, name, raw.as_bytes())?;
    let path = spill_path(name);
    let mut r = cut(escaped, limit_bytes, &format!("{SEARCH}{path}"));
    r.path = Some(path);
    Ok(r)
}

/// Голова, строка пропуска и хвост текста длиннее `limit` (D44). Строка пропуска —
/// `… omitted N lines; {pointer}` или `… omitted N bytes; {pointer}`; `path` итога — `None`.
pub(crate) fn cut(s: &str, limit: usize, pointer: &str) -> SpillResult {
    let head_budget = limit * HEAD_PERCENT / 100;
    let tail_budget = limit - head_budget;
    let mut by_bytes = false;

    // Голова — целые строки, пока влезают; первая длиннее доли — срез по символу.
    let mut head_end = 0;
    for (i, _) in s.match_indices('\n') {
        if i < head_budget {
            head_end = i + 1;
        } else {
            break;
        }
    }
    if head_end == 0 {
        head_end = floor_boundary(s, head_budget);
        by_bytes = true;
    }

    // Хвост — с первого начала строки не раньше `len − tail_budget`; последняя строка
    // длиннее доли — срез по символу.
    let tail_min = s.len().saturating_sub(tail_budget).max(head_end);
    let line_start = if tail_min == 0 {
        Some(0)
    } else {
        // Поиск по байтам: `tail_min − 1` может стоять внутри символа.
        let from = tail_min - 1;
        s.as_bytes()[from..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| from + i + 1)
    };
    let tail_start = match line_start {
        Some(p) if p < s.len() => p,
        _ => {
            by_bytes = true;
            ceil_boundary(s, tail_min)
        }
    };

    let (head, middle, tail) = (&s[..head_end], &s[head_end..tail_start], &s[tail_start..]);
    let omitted_bytes = middle.len();
    let (omitted_lines, marker) = if by_bytes {
        let lead = if head.is_empty() || head.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        (
            0,
            format!("{lead}{OMITTED}{omitted_bytes} bytes; {pointer}\n"),
        )
    } else {
        // `tail_start` — начало строки: каждая пропущенная строка кончается `\n`.
        let n = middle.bytes().filter(|&b| b == b'\n').count();
        (n, format!("{OMITTED}{n} lines; {pointer}\n"))
    };
    let mut content = String::with_capacity(head.len() + marker.len() + tail.len());
    content.push_str(head);
    content.push_str(&marker);
    content.push_str(tail);
    SpillResult {
        content,
        path: None,
        omitted_lines,
        omitted_bytes,
    }
}

/// Путь выгрузки, на который указывает результат, если файл этого run существует
/// (D45: выгруженный результат при удалении ссылается на свой файл). Вывод
/// недоверенный и может содержать поддельный указатель, поэтому имя проверяется:
/// префикс `{run}-`, допустимые символы, файл на диске.
pub(crate) fn spilled_path(project_dir: &Path, run: RunId, content: &str) -> Option<String> {
    let needle = format!("; {SEARCH}{SPILL_DIR}/");
    let prefix = format!("{run}-");
    for (i, _) in content.match_indices(&needle) {
        let rest = &content[i + needle.len()..];
        let Some(end) = rest.find(".txt\n") else {
            continue;
        };
        let name = &rest[..end];
        if name.starts_with(&prefix)
            && safe_name(name) == name
            && file_of(project_dir, name).is_file()
        {
            return Some(spill_path(name));
        }
    }
    None
}

/// Записать файл выгрузки `name` (каталоги и `.gitignore` создаются). Совпадающий файл
/// не переписывается: запись идемпотентна (исключение D30 для удаления).
pub(crate) fn write_spill(project_dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    if safe_name(name) != name || name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("недопустимое имя файла выгрузки: {name}"),
        ));
    }
    let dir = project_dir.join(SPILL_DIR);
    std::fs::create_dir_all(&dir)?;
    let ignore = project_dir.join(WORK_DIR).join(".gitignore");
    if std::fs::read(&ignore).ok().as_deref() != Some(WORK_GITIGNORE.as_bytes()) {
        atomic_write(&ignore, WORK_GITIGNORE.as_bytes())?;
    }
    let file = file_of(project_dir, name);
    if std::fs::metadata(&file).is_ok_and(|m| m.len() == bytes.len() as u64)
        && std::fs::read(&file).is_ok_and(|b| b == bytes)
    {
        return Ok(());
    }
    atomic_write(&file, bytes)?;
    tracing::debug!(file = %file.display(), bytes = bytes.len(), "вывод выгружен в файл");
    Ok(())
}

fn file_of(project_dir: &Path, name: &str) -> PathBuf {
    project_dir.join(SPILL_DIR).join(format!("{name}.txt"))
}

/// Запись через временный файл рядом и переименование.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Имя файла без разделителей пути: всё, кроме ASCII-букв, цифр, `-`, `_` и `.`, —
/// `_`; ведущие точки — `_` (нет `..` и скрытых файлов).
fn safe_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    while out.starts_with('.') {
        out.replace_range(..1, "_");
    }
    out
}

/// Наибольшая граница символа не дальше `i`.
fn floor_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Наименьшая граница символа не ближе `i`.
fn ceil_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_bytes_values() {
        assert_eq!(spill_limit_bytes(8192, 0.15, None), 4915);
        assert_eq!(spill_limit_bytes(8192, 0.15, Some(1000)), 1000);
        assert_eq!(spill_limit_bytes(8192, 0.15, Some(1 << 20)), 4915);
        assert_eq!(spill_limit_bytes(8192, f32::NAN, None), 0);
    }

    #[test]
    fn names_are_safe() {
        assert_eq!(spill_name(&EffectKey("r:3:1".into())), "r-3-1");
        let run = RunId::new();
        assert_eq!(
            prune_name(run, "functions.read:0", 1),
            format!("{run}-functions.read-0")
        );
        assert_eq!(prune_name(run, "../x/y", 2), format!("{run}-.._x_y-2"));
        assert_eq!(safe_name("..a"), "_.a");
        assert_eq!(safe_name(r"a/b\c"), "a_b_c");
    }

    /// Голова и хвост `content` вокруг строки пропуска.
    fn split(content: &str) -> (&str, &str) {
        let at = content.find(OMITTED).unwrap();
        let head = content[..at].strip_suffix('\n').unwrap_or(&content[..at]);
        let after = at + content[at..].find('\n').unwrap() + 1;
        (head, &content[after..])
    }

    #[test]
    fn last_line_longer_than_tail_is_cut_by_bytes() {
        // Короткие строки и одна длинная последняя: хвост режется внутри строки.
        let mut s = "ab\n".repeat(10);
        s.push_str(&"я".repeat(500));
        let r = cut(&s, 100, "search p");
        assert_eq!(r.omitted_lines, 0);
        assert!(r.content.contains(" bytes; search p\n"), "{}", r.content);
        assert!(r.content.starts_with("ab\n"));
        assert!(r.content.ends_with('я'));
        let (head, tail) = split(&r.content);
        // Голова по целым строкам кончается `\n`; `split` его отрезает.
        assert!(head.ends_with("ab"));
        assert_eq!(r.omitted_bytes, s.len() - (head.len() + 1) - tail.len());
        assert!(tail.len() <= 40 && tail.len() % 2 == 0);
    }

    #[test]
    fn tiny_limit_does_not_panic() {
        for limit in 0..8 {
            let r = cut("яяя\nяя\nя", limit, "search p");
            assert!(r.content.contains(OMITTED));
        }
    }
}
