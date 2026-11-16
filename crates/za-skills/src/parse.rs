//! Разбор `SKILL.md` (SKL-003, D115): frontmatter своим подмножеством YAML, проверки
//! полей, тело. Строгий writer, лояльный reader: нарушения формата, не мешающие понять
//! скилл, — предупреждения; без описания, без frontmatter или с YAML вне подмножества —
//! отказ, скилл пропускается.

use std::collections::BTreeMap;

use za_core::text::{Clean, clean_invisible};

use crate::yaml::{self, Value, YamlError};

/// Предел файла `SKILL.md`, байты.
pub const MAX_FILE_BYTES: usize = 256 << 10;
/// Предел frontmatter, байты.
pub const MAX_FRONTMATTER_BYTES: usize = 64 << 10;
/// Длина описания, символы: длиннее — предупреждение.
pub const DESCRIPTION_MAX_CHARS: usize = 1024;
/// Длина имени, символы: длиннее — предупреждение.
pub const NAME_MAX_CHARS: usize = 64;
/// Длина `compatibility`, символы: длиннее — предупреждение.
pub const COMPATIBILITY_MAX_CHARS: usize = 500;
/// Поля спецификации Agent Skills.
const KNOWN_FIELDS: &[&str] = &[
    "name",
    "description",
    "license",
    "compatibility",
    "metadata",
    "allowed-tools",
];
/// Исполняемые расширения верхнего уровня (SKL-006): ничего не исполняется.
const EXECUTABLE_FIELDS: &[&str] = &["hooks", "commands", "scripts"];

/// Разобранный `SKILL.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// Имя скилла: `name` frontmatter, а при недопустимом — имя каталога; `None` — оба
    /// недопустимы (скилл пропускается).
    pub name: Option<String>,
    /// Описание после очистки невидимых символов уровня `Untrusted` (D102): это текст для
    /// модели.
    pub description: String,
    /// Тело после строки закрытия frontmatter, переводы строк — `\n`.
    pub body: String,
    /// Все поля frontmatter, включая незнакомые, — как в файле, без очистки (очищенное
    /// описание — в [`Parsed::description`]).
    pub fields: BTreeMap<String, Value>,
    /// Предупреждения (английский: уходят в журнал и `skills list`).
    pub warnings: Vec<String>,
}

/// Отказ разбора: скилл пропускается с этим текстом причины.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("no frontmatter")]
    NoFrontmatter,
    /// Корректный YAML вне подмножества («неразбираемый для нас», а не битый файл).
    #[error("unsupported YAML feature: {0}")]
    Unsupported(String),
    #[error("unparseable frontmatter: {0}")]
    Unparseable(String),
    #[error("empty description")]
    EmptyDescription,
    #[error("too large (SKILL.md over 256 KiB or frontmatter over 64 KiB)")]
    TooLarge,
}

/// Разбор текста `SKILL.md` из каталога `dir_name` (D115). Чистая функция, время линейно
/// по длине текста.
pub fn parse_skill_md(text: &str, dir_name: &str) -> Result<Parsed, ParseError> {
    if text.len() > MAX_FILE_BYTES {
        return Err(ParseError::TooLarge);
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (fm, body) = split_frontmatter(text)?;
    let fields = yaml::parse(fm, 2).map_err(|e| match e {
        YamlError::Unsupported(w) => ParseError::Unsupported(w),
        YamlError::Unparseable(w) => ParseError::Unparseable(w),
    })?;
    let mut warnings = Vec::new();
    let description = match fields.get("description") {
        None => return Err(ParseError::EmptyDescription),
        Some(Value::Str(s)) => clean_invisible(s, Clean::Untrusted).0.into_owned(),
        Some(_) => {
            return Err(ParseError::Unparseable(
                "description is not a string".into(),
            ));
        }
    };
    if description.trim().is_empty() {
        return Err(ParseError::EmptyDescription);
    }
    if description.chars().count() > DESCRIPTION_MAX_CHARS {
        warnings.push(format!(
            "description longer than {DESCRIPTION_MAX_CHARS} characters"
        ));
    }
    let name = resolve_name(fields.get("name"), dir_name, &mut warnings);
    match fields.get("compatibility") {
        Some(Value::Str(s)) if s.chars().count() > COMPATIBILITY_MAX_CHARS => warnings.push(
            format!("compatibility longer than {COMPATIBILITY_MAX_CHARS} characters"),
        ),
        Some(Value::Str(_)) | None => {}
        Some(_) => warnings.push("compatibility is not a string".into()),
    }
    if matches!(fields.get("license"), Some(Value::List(_) | Value::Map(_))) {
        warnings.push("license is not a string".into());
    }
    if matches!(fields.get("metadata"), Some(Value::Str(_) | Value::List(_))) {
        warnings.push("metadata is not a mapping".into());
    }
    if matches!(fields.get("allowed-tools"), Some(Value::Map(_))) {
        warnings.push("allowed-tools is not a list".into());
    }
    for key in fields.keys() {
        if EXECUTABLE_FIELDS.contains(&key.as_str()) {
            warnings.push(format!("executable extension ignored: {key}"));
        } else if !KNOWN_FIELDS.contains(&key.as_str()) {
            let plain = key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
            let key = if plain { key.clone() } else { shown(key) };
            warnings.push(format!("unknown field {key}"));
        }
    }
    Ok(Parsed {
        name,
        description,
        body: body.replace("\r\n", "\n"),
        fields,
        warnings,
    })
}

/// Инструменты `allowed-tools` (SKL-007: прав не меняют, только хранятся). Список — как
/// есть; строка — через запятые и пробелы вне скобок (`Bash(git add:*), Read`).
pub fn allowed_tools(fields: &BTreeMap<String, Value>) -> Vec<String> {
    match fields.get("allowed-tools") {
        Some(Value::List(v)) => v
            .iter()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect(),
        Some(Value::Str(s)) => {
            let mut out = Vec::new();
            let mut cur = String::new();
            let mut depth = 0usize;
            for c in s.chars() {
                match c {
                    '(' => depth += 1,
                    ')' => depth = depth.saturating_sub(1),
                    _ => {}
                }
                if depth == 0 && (c == ',' || c.is_whitespace()) {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                } else {
                    cur.push(c);
                }
            }
            if !cur.is_empty() {
                out.push(cur);
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Допустимое имя: `[a-z0-9-]`, без `-` в начале, в конце и `--` (длина проверяется
/// отдельно — длиннее 64 только предупреждение).
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
}

/// Frontmatter и тело. Первая строка — `---` (после BOM; CRLF и пробелы в конце
/// допустимы), закрытие — следующая строка `---`.
fn split_frontmatter(text: &str) -> Result<(&str, &str), ParseError> {
    let (first, rest) = match text.find('\n') {
        Some(i) => (&text[..i], &text[i + 1..]),
        None => (text, ""),
    };
    if first.trim_end_matches([' ', '\t', '\r']) != "---" {
        return Err(ParseError::NoFrontmatter);
    }
    let mut off = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r', ' ', '\t']) == "---" {
            return Ok((&rest[..off], &rest[off + line.len()..]));
        }
        off += line.len();
        if off > MAX_FRONTMATTER_BYTES {
            return Err(ParseError::TooLarge);
        }
    }
    Err(ParseError::Unparseable("frontmatter is not closed".into()))
}

/// Имя скилла по полю `name` и имени каталога (D115).
fn resolve_name(field: Option<&Value>, dir: &str, warnings: &mut Vec<String>) -> Option<String> {
    if let Some(Value::Str(n)) = field
        && valid_name(n)
    {
        if n.chars().count() > NAME_MAX_CHARS {
            warnings.push(format!("name longer than {NAME_MAX_CHARS} characters"));
        }
        if n != dir {
            warnings.push(format!(
                "name {:?} does not match directory {}",
                n,
                shown(dir)
            ));
        }
        return Some(n.clone());
    }
    let what = match field {
        Some(Value::Str(n)) => format!("invalid name {}", shown(n)),
        Some(_) => "name is not a string".to_owned(),
        None => "name missing".to_owned(),
    };
    if !valid_name(dir) {
        warnings.push(format!(
            "{what}; directory name {} is not valid either",
            shown(dir)
        ));
        return None;
    }
    warnings.push(format!("{what}; using directory name {}", shown(dir)));
    if dir.chars().count() > NAME_MAX_CHARS {
        warnings.push(format!("name longer than {NAME_MAX_CHARS} characters"));
    }
    Some(dir.to_owned())
}

/// Текст из файла для предупреждения: в кавычках, с экранированием управляющих символов,
/// не длиннее 64 символов.
fn shown(s: &str) -> String {
    let cut: String = s.chars().take(64).collect();
    let more = if cut.len() < s.len() { "..." } else { "" };
    format!("\"{}\"{more}", cut.escape_debug())
}
