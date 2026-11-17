//! Область памяти run — `ScopeGuard` (R1-M D164; MEM-013; П10 изменений по ZAM 0.0.12-m6.5).
//!
//! - **Разрешено:** текущее решение (GUID без учёта регистра, пути `<guid>:/…`) — чтение и
//!   запись; `knowledge` — чтение и запись; `rules` — только чтение. Без решения — `knowledge`
//!   (запись — только с явным `scope: "global"`, D135) и `rules` (чтение). Недоверенный проект
//!   (`RunBinding.writable == false`) — только чтение `knowledge` и `rules`: решения у него нет
//!   (D135), запись — отказ [`READ_ONLY`].
//! - **Проверяются все параметры, задающие пространство** (у любого инструмента — набор
//!   проверок общий): `space`; `scope` объектом (`solution`, `all_solutions: true` — отказ,
//!   `spaces[]`, `include_human: true` — отказ, `path_prefix` с пространством) и строкой
//!   (`global` — Knowledge, `solution` — решение); `path` и `id` вида `<пространство>:/<путь>`;
//!   `items[].id` (`memory_read`); `sources[].id` (`memory_cache_put`, всегда чтение). У
//!   разбора идеи (`action: "review"`) `space` — только пространство идеи, то есть решение run:
//!   идеи Knowledge сервер не разбирает (П10).
//! - **Неизвестная форма пространства** (не строка, не `knowledge`/`rules`/`human`, не GUID;
//!   26-символьный id пространства клиент сопоставить не может) — отказ.
//! - **Чужое пространство — отказ без запроса** (`memory scope: <space> is outside this project;
//!   allowed: <list>`): канал сервера пуст. Гранты памяти — R1-D (D164, «Переносы»).
//! - **Не проверяются** голые id и ссылки `n:`, `i:`, `k:` …: пространство по ним не видно,
//!   сервер отдаёт только доступное пользователю (принятый риск, ADR 0007).
//!
//! Тексты отказов уходят модели: английские, ASCII-пунктуация (D94).

use serde_json::Value;

use crate::session::RunBinding;

/// Личное пространство знаний пользователя.
pub const KNOWLEDGE: &str = "knowledge";
/// Правила пользователя: модель только читает.
pub const RULES: &str = "rules";
/// Пространство человека: токену не видно (ZAM — 404).
pub const HUMAN: &str = "human";
/// Отказ записи в недоверенном проекте (D135).
pub const READ_ONLY: &str = "memory is read-only in an untrusted project";
/// Предел значения пространства в тексте отказа, символов.
const SHOWN_MAX: usize = 80;

/// Доступ инструмента к пространству.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeAccess {
    Read,
    Write,
}

/// Разобранное пространство.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Space {
    Knowledge,
    Rules,
    Human,
    /// GUID решения строчными.
    Solution(String),
    Unknown,
}

/// Пространство по строке ZAM (`SpaceRef::parse`, `api/scope.rs@v0.0.12-m6.5`): `knowledge`,
/// `rules`, `human` или GUID решения; иное (в том числе id пространства из 26 символов, которое
/// клиент сопоставить не может) — неизвестная форма.
fn parse(raw: &str) -> Space {
    let s = raw.trim();
    match s {
        KNOWLEDGE => Space::Knowledge,
        RULES => Space::Rules,
        HUMAN => Space::Human,
        _ if is_guid(s) => Space::Solution(s.to_ascii_lowercase()),
        _ => Space::Unknown,
    }
}

/// GUID вида `8-4-4-4-12` из шестнадцатеричных цифр.
pub fn is_guid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Пространство пути `<пространство>:/<путь>` (как `split_path` MCP ZAM): `None` — путь без
/// пространства.
pub fn path_space(path: &str) -> Option<&str> {
    match path.split_once(":/") {
        Some((sp, _)) if !sp.is_empty() && !sp.contains('/') => Some(sp),
        _ => None,
    }
}

/// Пространство и путь `[<пространство>:/]<путь>` (путь без `/` по краям).
pub fn split_path(raw: &str) -> (Option<&str>, &str) {
    match raw.split_once(":/") {
        Some((sp, p)) if !sp.is_empty() && !sp.contains('/') => (Some(sp), p.trim_matches('/')),
        _ => (None, raw.trim_matches('/')),
    }
}

/// Значение пространства для текста отказа: одной строкой, не длиннее [`SHOWN_MAX`].
fn shown(raw: &str) -> String {
    let one: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(SHOWN_MAX)
        .collect();
    one.trim().to_owned()
}

/// Страж области run (D164).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeGuard {
    /// GUID решения строчными; у недоверенного проекта решения нет (D135).
    solution: Option<String>,
    writable: bool,
}

impl ScopeGuard {
    /// Страж привязки run.
    pub fn new(b: &RunBinding) -> Self {
        Self {
            solution: b
                .solution
                .as_deref()
                .map(str::trim)
                .filter(|s| is_guid(s))
                .map(str::to_ascii_lowercase),
            writable: b.writable,
        }
    }

    /// GUID решения run строчными.
    pub fn solution(&self) -> Option<&str> {
        self.solution.as_deref()
    }

    /// Может ли run писать в память (доверенный проект).
    pub fn writable(&self) -> bool {
        self.writable
    }

    /// Разрешённые пространства для доступа `a` — для текста отказа.
    fn allowed(&self, a: ScopeAccess) -> String {
        let mut out: Vec<&str> = Vec::new();
        if a == ScopeAccess::Write && !self.writable {
            return "none (read-only)".to_owned();
        }
        if let Some(s) = &self.solution {
            out.push(s);
        }
        out.push(KNOWLEDGE);
        if a == ScopeAccess::Read {
            out.push(RULES);
        }
        out.join(", ")
    }

    fn outside(&self, raw: &str, a: ScopeAccess) -> String {
        format!(
            "memory scope: {} is outside this project; allowed: {}",
            shown(raw),
            self.allowed(a)
        )
    }

    /// Проверка пространства `raw` для доступа `a`.
    pub fn check_space(&self, raw: &str, a: ScopeAccess) -> Result<(), String> {
        if a == ScopeAccess::Write && !self.writable {
            return Err(READ_ONLY.to_owned());
        }
        match parse(raw) {
            Space::Knowledge => Ok(()),
            Space::Rules if a == ScopeAccess::Read => Ok(()),
            Space::Rules => Err(format!(
                "memory scope: {RULES} is read-only; allowed: {}",
                self.allowed(a)
            )),
            Space::Solution(g) if self.solution.as_deref() == Some(g.as_str()) => Ok(()),
            Space::Solution(_) | Space::Human => Err(self.outside(raw, a)),
            Space::Unknown => Err(format!(
                "memory scope: unknown space form {:?}; allowed: {}",
                shown(raw),
                self.allowed(a)
            )),
        }
    }

    /// Пространство разбора идеи (П10): только решение run.
    fn check_review_space(&self, raw: &str) -> Result<(), String> {
        self.check_space(raw, ScopeAccess::Write)?;
        match (parse(raw), &self.solution) {
            (Space::Solution(g), Some(s)) if &g == s => Ok(()),
            _ => Err(format!(
                "memory scope: review stays in the idea's space ({})",
                self.solution.as_deref().unwrap_or("the project solution")
            )),
        }
    }

    /// Пространство пути или id вида `<пространство>:/<путь>`; голый путь и голый id — без
    /// проверки.
    fn check_path(&self, raw: &str, a: ScopeAccess) -> Result<(), String> {
        match path_space(raw) {
            Some(sp) => self.check_space(sp, a),
            None => Ok(()),
        }
    }

    /// Строка-значение параметра `name` или отказ «неизвестная форма» (не строка).
    fn string_arg<'a>(&self, v: &'a Value, name: &str, a: ScopeAccess) -> Result<&'a str, String> {
        v.as_str().ok_or_else(|| {
            format!(
                "memory scope: {name} must be a string; allowed: {}",
                self.allowed(a)
            )
        })
    }

    /// Проверка аргументов инструмента `tool` с доступом `a` по всем параметрам, задающим
    /// пространство (D164). Отказ — текст модели; сеть не трогается.
    pub fn check_args(&self, tool: &str, args: &Value, a: ScopeAccess) -> Result<(), String> {
        let Some(m) = args.as_object() else {
            return Ok(());
        };
        let present = |k: &str| m.get(k).filter(|v| !v.is_null());
        let review = m.get("action").and_then(Value::as_str) == Some("review");
        if let Some(v) = present("space") {
            let s = self.string_arg(v, "space", a)?;
            if review {
                self.check_review_space(s)?;
            } else {
                self.check_space(s, a)?;
            }
        }
        match present("scope") {
            None => {}
            Some(Value::Object(sc)) => self.check_scope_object(sc, a)?,
            Some(Value::String(s)) => match s.trim() {
                "global" => self.check_space(KNOWLEDGE, a)?,
                "solution" => match &self.solution {
                    Some(g) => self.check_space(g, a)?,
                    None => {
                        return Err(format!(
                            "memory scope: this project has no memory solution; allowed: {}",
                            self.allowed(a)
                        ));
                    }
                },
                other => return Err(self.outside(other, a)),
            },
            Some(_) => {
                return Err(format!(
                    "memory scope: scope must be an object or \"global\"; allowed: {}",
                    self.allowed(a)
                ));
            }
        }
        for k in ["path", "id"] {
            if let Some(v) = present(k) {
                let s = self.string_arg(v, k, a)?;
                self.check_path(s, a)?;
            }
        }
        // Пачка `memory_read` и источники выжимки: источники только читаются (выжимка пишется в
        // пространство run по `X-Zam-Solution`).
        for (k, access) in [
            ("items", a),
            (
                "sources",
                if tool == "memory_cache_put" {
                    ScopeAccess::Read
                } else {
                    a
                },
            ),
        ] {
            let Some(v) = present(k) else { continue };
            let Some(list) = v.as_array() else {
                return Err(format!(
                    "memory scope: {k} must be an array; allowed: {}",
                    self.allowed(access)
                ));
            };
            for item in list {
                if let Some(id) = item.get("id").filter(|v| !v.is_null()) {
                    let s = self.string_arg(id, "id", access)?;
                    self.check_path(s, access)?;
                }
            }
        }
        Ok(())
    }

    /// `scope` объектом (`memory_search`, `memory_cache_put`).
    fn check_scope_object(
        &self,
        sc: &serde_json::Map<String, Value>,
        a: ScopeAccess,
    ) -> Result<(), String> {
        let present = |k: &str| sc.get(k).filter(|v| !v.is_null());
        if let Some(v) = present("solution") {
            let s = self.string_arg(v, "scope.solution", a)?;
            self.check_space(s, a)?;
        }
        if present("all_solutions").and_then(Value::as_bool) == Some(true) {
            return Err(self.outside("all_solutions", a));
        }
        if present("include_human").and_then(Value::as_bool) == Some(true) {
            return Err(self.outside(HUMAN, a));
        }
        if let Some(v) = present("spaces") {
            let Some(list) = v.as_array() else {
                return Err(format!(
                    "memory scope: scope.spaces must be an array; allowed: {}",
                    self.allowed(a)
                ));
            };
            for s in list {
                let s = self.string_arg(s, "scope.spaces[]", a)?;
                self.check_space(s, a)?;
            }
        }
        if let Some(v) = present("path_prefix") {
            let s = self.string_arg(v, "scope.path_prefix", a)?;
            self.check_path(s, a)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_and_paths() {
        assert!(is_guid("6f1c2b3a-0d4e-4f5a-8b6c-7d8e9f0a1b2c"));
        assert!(is_guid("6F1C2B3A-0D4E-4F5A-8B6C-7D8E9F0A1B2C"));
        assert!(!is_guid("6f1c2b3a0d4e4f5a8b6c7d8e9f0a1b2c"));
        assert!(!is_guid("knowledge"));
        assert_eq!(path_space("knowledge:/a/b"), Some("knowledge"));
        assert_eq!(path_space("n:01ABC"), None);
        assert_eq!(path_space("a/b:/c"), None);
        assert_eq!(split_path("rules:/x/"), (Some("rules"), "x"));
        assert_eq!(split_path("/a/b/"), (None, "a/b"));
    }
}
