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
//! Файлы лежат только в `<project>/.zero-agent/work/spill/`. Каталог создаёт
//! [`za_core::paths::work_dir`] (R1-C1 D76): по компонентам, без ссылок, внутри корня
//! проекта, с `.zero-agent/work/.gitignore` = `*`. Запись — через временный файл и
//! переименование: оборванная запись не оставляет полуфайла под именем spill.
//!
//! Запись не идёт через ссылки (SEC-004; m5 финального ревью R1-B2): ни один компонент
//! пути под корнем проекта (`.zero-agent`, `work`, `spill`), ни файл выгрузки не могут
//! быть символической ссылкой или junction. Иначе — ошибка записи: вывод уходит в
//! контекст с пометкой `full output not saved`, удаление на границе — `prune_failed`.
//! Проверка и запись — не одна атомарная операция: подмена каталога между ними
//! (TOCTOU) — принятый риск до песочницы файловых инструментов (D98, SEC-100).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use za_core::paths::work_dir;
use za_protocol::{EffectKey, RunId};

/// Каталог выгрузки относительно корня проекта (D44, SEC-004).
pub const SPILL_DIR: &str = ".zero-agent/work/spill";
/// Имя подкаталога выгрузки у [`za_core::paths::work_dir`].
const SPILL_SUB: &str = "spill";
/// Доля головы в лимите, проценты (D44); хвост — остаток.
const HEAD_PERCENT: usize = 60;
/// Начало строки-указателя пропуска.
pub(crate) const OMITTED: &str = "… omitted ";
/// Указатель на файл выгрузки: `search <путь>`; перед ним в строке пропуска — `; `.
pub(crate) const SEARCH: &str = "search ";
/// Указатель, если файл выгрузки записать не удалось.
pub(crate) const NOT_SAVED: &str = "full output not saved";

/// Итог выгрузки.
#[derive(Debug, Clone, PartialEq)]
pub struct SpillResult {
    /// Текст для контекста: весь экранированный вывод или голова, указатель и хвост.
    pub content: String,
    /// Путь файла выгрузки относительно корня проекта, через `/`; `None` — вывод в лимите.
    pub path: Option<String>,
    /// Пропущено целых строк. 0 при разрезе внутри строки — и тогда, когда голова
    /// взята целыми строками, а хвост режет длинную последнюю строку: строка пропуска
    /// одна, `omitted N bytes` (Ruling «2/задача 2»).
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

/// Имя файла выгрузки вывода инструмента: [`EffectKey::file_stem`] (`:` → `-`, D44, D76).
pub fn spill_name(key: &EffectKey) -> String {
    safe_name(&key.file_stem())
}

/// Hex-символов BLAKE3 в имени файла выгрузки при удалении.
const CONTENT_HASH_HEX: usize = 16;

/// Имя файла выгрузки результата при удалении (D45, Ruling I-2 ревью задачи 2):
/// `{run}-{blake3(content)[..16]}` — по содержимому. `call_id` для имени не годится:
/// он недоверенный, уникален только в одном ответе (нативные id моделей вида `call_0` и
/// id `call_<blake3>` нормализации повторяются между ходами), не ограничен по длине и
/// после замены символов не инъективен. Имя по содержимому детерминировано при
/// реплее, фиксированной длины, разные тексты не делят файл, тот же текст — тот же файл.
pub(crate) fn content_name(run: RunId, content: &str) -> String {
    let hex = blake3::hash(content.as_bytes()).to_hex();
    format!("{run}-{}", &hex.as_str()[..CONTENT_HASH_HEX])
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

/// Записать файл выгрузки `name` (каталоги и `.gitignore` создаются). Совпадающий файл
/// не переписывается: запись идемпотентна (исключение D30 для удаления). Сравнение с
/// диском только решает, писать ли; на историю run оно не влияет.
pub(crate) fn write_spill(project_dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    if safe_name(name) != name || name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("недопустимое имя файла выгрузки: {name}"),
        ));
    }
    let file = work_dir(project_dir, SPILL_SUB)?.join(format!("{name}.txt"));
    not_link(&file)?;
    if std::fs::metadata(&file).is_ok_and(|m| m.len() == bytes.len() as u64)
        && std::fs::read(&file).is_ok_and(|b| b == bytes)
    {
        return Ok(());
    }
    atomic_write(&file, bytes)?;
    tracing::debug!(file = %file.display(), bytes = bytes.len(), "вывод выгружен в файл");
    Ok(())
}

/// Путь — не символическая ссылка и не junction (на Windows junction — тоже ссылка у
/// `symlink_metadata`); отсутствующий путь допустим.
fn not_link(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(io::Error::other(format!(
            "путь выгрузки — символическая ссылка или junction: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Порядковый номер временного файла в процессе: одновременные записи (первая
/// выгрузка двух чтений сразу, брошенный вызов и его повтор) не делят `.tmp`.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Запись через временный файл рядом (`<имя>.<pid>-<n>.tmp`) и переименование.
/// Временный файл создаётся заново (`create_new`): чужая ссылка на его месте — ошибка,
/// а не запись по ней. Любой сбой удаляет временный файл: остатка `.tmp` нет.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}-{n}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let mut created = false;
    let r = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| {
            created = true;
            f.write_all(bytes)
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if r.is_err() && created {
        let _ = std::fs::remove_file(&tmp);
    }
    r
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
        let name = content_name(run, "текст");
        assert_eq!(name.len(), run.to_string().len() + 1 + CONTENT_HASH_HEX);
        assert_eq!(safe_name(&name), name);
        assert_eq!(name, content_name(run, "текст"));
        assert_ne!(name, content_name(run, "текст2"));
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
