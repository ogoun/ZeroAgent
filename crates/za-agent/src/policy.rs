//! Движок политики R1-C1 (D77–D83; SEC-001…005, SEC-030…036): правила `allow`, `ask`,
//! `deny` по инструменту, пути и разобранной команде, гранты доступа вне проекта,
//! защищённые пути, режим `bypass`, доверие к проекту и метка недоверенного содержимого.
//!
//! [`decide`] — чистая функция от предметов вызова ([`Subjects`]), их разрешённых путей,
//! режима, доверия, грантов, меток и снимка политики ([`PolicySnapshot`]). Чтений вне
//! эффектов в ней нет (D30): при реплее то же состояние даёт то же решение (D92).
//!
//! Порядок (D80): ранний выход только у `Deny`, у прерывания доверия и у `Allow`.
//! 1. Защищённый путь — `Deny("protected path")`.
//! 2. Любое правило `deny` — `Deny("policy rule")`.
//! 3. Запрет режима ([`gate`], D14) — `Deny`.
//! 4. `Exec` в недоверенном проекте — [`Decision::Trust`], после отказа —
//!    `Deny("untrusted project")` (D83).
//! 5. Причины `Ask` собираются все: `irreversible` (не снимается ничем), `outside_project`
//!    (снимают только грант или глобальное `allow` с `path`), `rule`, `mode` и `tainted`
//!    (последние два снимают глобальное `allow` и `bypass`).
//!
//! Классификатор необратимых команд — дополнительный слой (D81): основная защита —
//! граница проекта, чекпоинты, метка `tainted` и режимы с подтверждением.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use za_core::paths::{self, ResolvedPath, Roots};
use za_core::shell::{self, Segment};
use za_core::{Access, Approval, Gate, RawPath, RiskClass, Subjects, ToolKind, ToolSpec, gate};
use za_protocol::{RunMode, approval_reason, canonical_json, deny_reason};

use crate::prompt::escape_untrusted;

/// Действие правила `[[policy.rules]]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    Allow,
    Ask,
    Deny,
}

/// Правило политики (D80): срабатывает, если совпали все заданные поля.
///
/// - `tool` — имя инструмента (шаблон glob, с учётом регистра);
/// - `path` — путь вызова: внутри проекта относительный через `/`, абсолютный шаблон —
///   с абсолютным каноническим путём (glob по компонентам, на Windows без учёта регистра);
/// - `command` — разобранная команда (D81): `deny` и `ask` — `argv0` или текст любого
///   сегмента, `allow` — текст каждого сегмента без подстановок и перенаправлений.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub action: RuleAction,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
}

/// Грант из глобальной конфигурации `[[policy.grants]]` (D78).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub mode: GrantMode,
    #[serde(default)]
    pub reason: String,
}

/// Раздел `[policy]` конфигурации: глобальной (`ZA_HOME/config.toml`) или проектной
/// (`.zero-agent/config.toml`, только у доверенного проекта). У проекта действуют только
/// правила `ask` и `deny` (ARC-041).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    pub allow_bypass: bool,
    pub rules: Vec<Rule>,
    pub grants: Vec<GrantConfig>,
}

/// Источник правила снимка.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSource {
    Global,
    Project,
}

/// Снимок политики run (D80, D92): входит в снимок run и не меняется до границы шага.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicySnapshot {
    /// `[policy] allow_bypass` глобальной конфигурации.
    pub allow_bypass: bool,
    /// Действующие правила: глобальные, затем проектные `ask` и `deny`.
    pub rules: Vec<(RuleSource, Rule)>,
    /// BLAKE3 hex канонического JSON `{allow_bypass, rules, grants}`.
    pub hash: String,
    /// Отброшенные проектные `allow` (для `policy.loaded`).
    pub ignored: Vec<Rule>,
}

/// Ошибка политики. Тексты — для человека (CLI, конфигурация), на русском.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("policy.rules[{0}]: нужно хотя бы одно поле tool, path или command")]
    Empty(usize),
    #[error("policy.rules[{0}].{1}: неверный шаблон: {2}")]
    Glob(usize, &'static str, String),
    #[error(
        "режим bypass выключен: задайте [policy] allow_bypass = true в глобальной конфигурации"
    )]
    BypassDisabled,
}

impl PolicySnapshot {
    /// Снимок из глобальной и проектной конфигурации (D80, ARC-041). Правило без полей
    /// и неверный шаблон — ошибка; проектные `allow` отбрасываются с `warn` и попадают в
    /// [`PolicySnapshot::ignored`]; `allow_bypass` и гранты проекта не действуют.
    pub fn build(
        global: &PolicyConfig,
        project: Option<&PolicyConfig>,
    ) -> Result<Self, PolicyError> {
        let mut rules = Vec::new();
        for (i, r) in global.rules.iter().enumerate() {
            check_rule(i, r)?;
            rules.push((RuleSource::Global, r.clone()));
        }
        let mut ignored = Vec::new();
        if let Some(p) = project {
            for (i, r) in p.rules.iter().enumerate() {
                check_rule(i, r)?;
                if r.action == RuleAction::Allow {
                    tracing::warn!(index = i, "проектное правило allow не действует (ARC-041)");
                    ignored.push(r.clone());
                } else {
                    rules.push((RuleSource::Project, r.clone()));
                }
            }
            if p.allow_bypass {
                tracing::warn!("allow_bypass проектной конфигурации не действует (ARC-041)");
            }
            if !p.grants.is_empty() {
                tracing::warn!(
                    count = p.grants.len(),
                    "гранты проектной конфигурации не действуют: только глобальные (ARC-041)"
                );
            }
        }
        let hash = policy_hash(global.allow_bypass, &rules, &global.grants);
        Ok(Self {
            allow_bypass: global.allow_bypass,
            rules,
            hash,
            ignored,
        })
    }

    /// Режим run допустим политикой: `bypass` — только при `allow_bypass` (D80).
    pub fn check_mode(&self, mode: RunMode) -> Result<(), PolicyError> {
        if mode == RunMode::Bypass && !self.allow_bypass {
            return Err(PolicyError::BypassDisabled);
        }
        Ok(())
    }
}

/// Хэш политики: BLAKE3 канонического JSON (ключи по порядку). Пути грантов — строкой
/// с потерями (`to_string_lossy`): хэш определён и для путей не в UTF-8.
fn policy_hash(allow_bypass: bool, rules: &[(RuleSource, Rule)], grants: &[GrantConfig]) -> String {
    let rules: Vec<Value> = rules
        .iter()
        .map(|(src, r)| json!({"source": src, "rule": r}))
        .collect();
    let grants: Vec<Value> = grants
        .iter()
        .map(|g| json!({"path": g.path.to_string_lossy(), "mode": g.mode, "reason": g.reason}))
        .collect();
    let v = json!({"allow_bypass": allow_bypass, "rules": rules, "grants": grants});
    blake3::hash(canonical_json(&v).as_bytes())
        .to_hex()
        .to_string()
}

/// Проверка правила при сборке: хотя бы одно поле, шаблоны собираются, в `path` нет `..`.
fn check_rule(i: usize, r: &Rule) -> Result<(), PolicyError> {
    if r.tool.is_none() && r.path.is_none() && r.command.is_none() {
        return Err(PolicyError::Empty(i));
    }
    let bad = |field: &'static str, why: String| PolicyError::Glob(i, field, why);
    for (field, pat) in [
        ("tool", &r.tool),
        ("path", &r.path),
        ("command", &r.command),
    ] {
        let Some(pat) = pat else { continue };
        if pat.is_empty() {
            return Err(bad(field, "пустой шаблон".to_owned()));
        }
        let built = match field {
            "path" => path_glob(pat).map(|_| ()),
            _ => text_glob(pat, false).map(|_| ()),
        };
        built.map_err(|e| bad(field, e.to_string()))?;
        if field == "command" {
            text_glob(&argv0_pattern(pat), true).map_err(|e| bad(field, e.to_string()))?;
        }
        if field == "path" && slashed(pat).split('/').any(|c| c == "..") {
            return Err(bad(
                field,
                "компонент `..` недопустим: путь сверяется канонически".to_owned(),
            ));
        }
    }
    Ok(())
}

// ---- Шаблоны ----

/// Шаблон пути через `/`: на Windows `\` — разделитель.
fn slashed(s: &str) -> String {
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s.to_owned()
    }
}

/// Шаблон или путь через `/` абсолютный: `/…` или `X:/…`.
fn is_absolute_pattern(s: &str) -> bool {
    let b = s.as_bytes();
    s.starts_with('/')
        || (cfg!(windows) && b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// Шаблон `path`: `*` не пересекает `/`, `**` — пересекает; на Windows без учёта регистра.
/// Второе значение — шаблон абсолютный.
fn path_glob(pat: &str) -> Result<(GlobMatcher, bool), globset::Error> {
    let norm = slashed(pat);
    let m = GlobBuilder::new(&norm)
        .literal_separator(true)
        .case_insensitive(cfg!(windows))
        .backslash_escape(false)
        .build()?
        .compile_matcher();
    Ok((m, is_absolute_pattern(&norm)))
}

/// Шаблон текста (имя инструмента, сегмент команды): `*` пересекает любые символы,
/// `\` — обычный символ на всех ОС (пути Windows в командах, m6 ревью).
fn text_glob(pat: &str, case_insensitive: bool) -> Result<GlobMatcher, globset::Error> {
    Ok(GlobBuilder::new(pat)
        .literal_separator(false)
        .case_insensitive(case_insensitive)
        .backslash_escape(false)
        .build()?
        .compile_matcher())
}

/// Шаблон `command` для сравнения с `argv0` (D80): без регистра и `.exe`, как имя
/// команды у [`shell::argv0`].
fn argv0_pattern(pat: &str) -> String {
    let p = pat.trim().to_lowercase();
    p.strip_suffix(".exe").map(str::to_owned).unwrap_or(p)
}

/// Шаблон `command` для `deny` и `ask` по нормализованной команде (I2 ревью):
/// имя — как у [`shell::command_args`] (без обёрток, пути, регистра и `.exe`), слова
/// шаблона после имени — по порядку среди аргументов, между ними допускаются другие
/// (опции git, флаги).
struct CommandWords {
    name: GlobMatcher,
    rest: Vec<GlobMatcher>,
}

impl CommandWords {
    fn new(pat: &str) -> Option<Self> {
        let mut words = pat.split_whitespace();
        let name = text_glob(&argv0_pattern(words.next()?), true).ok()?;
        let rest = words
            .map(|w| text_glob(w, false).ok())
            .collect::<Option<Vec<_>>>()?;
        Some(Self { name, rest })
    }

    fn matches(&self, seg: &Segment) -> bool {
        let Some((name, args)) = shell::command_args(seg) else {
            return false;
        };
        if !self.name.is_match(&name) {
            return false;
        }
        let mut args = args.iter();
        self.rest
            .iter()
            .all(|w| args.by_ref().any(|a| w.is_match(a)))
    }
}

/// Шаблон `command` правила: текст сегмента, имя команды и нормализованная команда.
struct CommandMatch {
    text: GlobMatcher,
    argv0: GlobMatcher,
    words: CommandWords,
}

/// Скомпилированное правило. Неверный шаблон (снимок из журнала, собранный не через
/// [`PolicySnapshot::build`]) — в безопасную сторону: `deny` и `ask` срабатывают,
/// `allow` — нет.
struct Compiled<'a> {
    source: RuleSource,
    rule: &'a Rule,
    tool: Option<Option<GlobMatcher>>,
    path: Option<Option<(GlobMatcher, bool)>>,
    command: Option<Option<CommandMatch>>,
}

impl<'a> Compiled<'a> {
    fn new(source: RuleSource, rule: &'a Rule) -> Self {
        let has_dotdot = |p: &str| slashed(p).split('/').any(|c| c == "..");
        Self {
            source,
            rule,
            // Имена инструментов — в нижнем регистре: шаблон без учёта регистра (m2).
            tool: rule.tool.as_deref().map(|p| text_glob(p, true).ok()),
            path: rule.path.as_deref().map(|p| {
                path_glob(p)
                    .ok()
                    .filter(|_| !p.is_empty() && !has_dotdot(p))
            }),
            command: rule.command.as_deref().map(|p| {
                Some(CommandMatch {
                    text: text_glob(p, false).ok()?,
                    argv0: text_glob(&argv0_pattern(p), true).ok()?,
                    words: CommandWords::new(p)?,
                })
            }),
        }
    }

    fn broken(&self) -> bool {
        matches!(self.tool, Some(None))
            || matches!(self.path, Some(None))
            || matches!(self.command, Some(None))
    }

    /// Условия `tool` и `command`; `strict` — семантика `allow` для команды.
    fn tool_and_command(&self, call: &Call<'_>, strict: bool) -> bool {
        let tool_ok = match &self.tool {
            None => true,
            Some(m) => m.as_ref().is_some_and(|m| m.is_match(call.tool)),
        };
        let cmd_ok = match &self.command {
            None => true,
            Some(None) => false,
            Some(Some(m)) => {
                if !call.has_command || call.segments.is_empty() {
                    false
                } else if strict {
                    call.segments
                        .iter()
                        .all(|s| allow_segment(s) && m.text.is_match(&s.text))
                } else {
                    call.segments.iter().any(|s| {
                        m.text.is_match(&s.text)
                            || shell::argv0(s).is_some_and(|a| m.argv0.is_match(&a))
                            || m.words.matches(s)
                    })
                }
            }
        };
        tool_ok && cmd_ok
    }

    /// Шаблон `path` совпал с путём вызова.
    fn path_matches(&self, p: &CallPath) -> bool {
        match &self.path {
            None | Some(None) => false,
            Some(Some((m, absolute))) => {
                if *absolute {
                    m.is_match(&p.abs)
                } else {
                    p.rel.as_ref().is_some_and(|r| m.is_match(r))
                }
            }
        }
    }

    /// `deny` или `ask` сработало: все заданные поля совпали, `path` — с любым путём.
    /// Сломанный шаблон — срабатывает.
    fn fires(&self, call: &Call<'_>) -> bool {
        if self.broken() {
            return true;
        }
        let path_ok = match &self.path {
            None => true,
            Some(_) => {
                call.paths.iter().any(|p| self.path_matches(p))
                    || (self.rule.action == RuleAction::Deny
                        && call.words.iter().any(|p| self.path_matches(p)))
            }
        };
        path_ok && self.tool_and_command(call, false)
    }

    /// Глобальное `allow` применимо к вызову: правило глобальное, шаблоны целы, а у
    /// команды (`Exec`) задано поле `command` (C1 ревью: правило по `path` или `tool` не
    /// разрешает произвольную команду).
    fn allow_applies(&self, call: &Call<'_>) -> bool {
        !self.broken()
            && self.source == RuleSource::Global
            && self.rule.action == RuleAction::Allow
            && (!call.exec || self.rule.command.is_some())
    }

    /// Глобальное `allow` совпало с вызовом целиком: `path` — с каждым путём (и пути есть).
    fn allows(&self, call: &Call<'_>) -> bool {
        if !self.allow_applies(call) {
            return false;
        }
        let path_ok = match &self.path {
            None => true,
            Some(_) => !call.paths.is_empty() && call.paths.iter().all(|p| self.path_matches(p)),
        };
        path_ok && self.tool_and_command(call, true)
    }

    /// Глобальное `allow` с `path` покрывает внешний путь (D77): явное указание пользователя.
    fn allows_path(&self, call: &Call<'_>, p: &CallPath) -> bool {
        self.allow_applies(call) && self.path_matches(p) && self.tool_and_command(call, true)
    }
}

/// Сегмент допускает `allow` (D80, Ruling 7.16): без подстановок, перенаправлений и
/// неразобранных конструкций.
fn allow_segment(s: &Segment) -> bool {
    !s.has_subst && !s.unparsed && s.redirects.is_empty()
}

// ---- Вызов ----

/// Путь вызова для правил и границы.
struct CallPath {
    /// Канонический путь.
    path: PathBuf,
    /// Доступ для покрытия грантом: у команды — запись (I1 ревью).
    cover: Access,
    /// Абсолютный путь через `/`.
    abs: String,
    /// Внутри проекта — относительный через `/`.
    rel: Option<String>,
    /// Вне проекта, `~` команды или `ZA_HOME`.
    outside: bool,
    /// Вне проекта так, что грант и `allow` не снимают: `~` команды (путь неизвестен до
    /// оболочки) или чтение `ZA_HOME` (защищённое место, D79).
    pinned: bool,
    protected: bool,
}

struct Call<'a> {
    tool: &'a str,
    /// Вызов запускает команду (`ToolKind::Exec`).
    exec: bool,
    has_command: bool,
    segments: &'a [Segment],
    paths: Vec<CallPath>,
    /// Все слова-аргументы и цели перенаправлений сегментов команды, разобранные
    /// лексически: по ним сверяются правила `deny` с `path` (Ruling: слова bash).
    words: Vec<CallPath>,
    /// Разрешение не соответствует [`subject_paths`]: граница неизвестна.
    unresolved: bool,
}

/// Пути вызова, которые пакет разрешает перед решением (D77, D98) и передаёт в
/// [`decide`] в том же порядке: [`Subjects::paths`], затем `cwd` команды с доступом
/// `Exec` (команда в чужом каталоге может в нём писать).
pub fn subject_paths(s: &Subjects) -> Vec<RawPath> {
    let mut out = s.paths.clone();
    if let Some(cwd) = &s.cwd {
        out.push(RawPath {
            arg: cwd.clone(),
            access: Access::Exec,
        });
    }
    out
}

fn call_of<'a>(
    cx: &PolicyCx<'_>,
    spec: &'a ToolSpec,
    s: &'a Subjects,
    resolved: &[ResolvedPath],
) -> Call<'a> {
    let raw = subject_paths(s);
    let exec = spec.kind == ToolKind::Exec;
    let project = &cx.roots.project;
    let paths = raw
        .iter()
        .zip(resolved)
        .map(|(r, p)| {
            let inside = p.inside && paths::within(project, &p.path);
            // Оболочка раскрывает `~` сама; нативные инструменты — нет (D76).
            let tilde = exec && r.arg.starts_with('~');
            let home = paths::in_za_home(cx.roots, &p.path);
            CallPath {
                abs: slashed(&p.path.to_string_lossy()),
                rel: inside.then(|| paths::display(project, &p.path)),
                outside: !inside || tilde || home,
                pinned: tilde || home,
                protected: p.protected
                    || (r.access == Access::Write && paths::protected_place(cx.roots, &p.path)),
                // Команда с путём может в него писать (`cp`, `tee`, `sed -i`): грант `ro`
                // её не покрывает (I1 ревью, решение заказчика (а)).
                cover: if exec { Access::Write } else { r.access },
                path: p.path.clone(),
            }
        })
        .collect();
    Call {
        tool: &spec.schema.name,
        exec,
        has_command: s.command.is_some(),
        segments: &s.segments,
        paths,
        words: if exec {
            command_words(cx, s)
        } else {
            Vec::new()
        },
        unresolved: raw.len() != resolved.len(),
    }
}

/// Слова сегментов команды как пути (лексически, без ввода-вывода): относительные — от
/// `cwd`, если он задан. Слово, которое [`paths::classify`] отвергает, пропускается.
/// Только для сверки `deny` с `path`: граница считается по [`subject_paths`].
fn command_words(cx: &PolicyCx<'_>, s: &Subjects) -> Vec<CallPath> {
    let project = &cx.roots.project;
    let base = s
        .cwd
        .as_deref()
        .and_then(|c| paths::classify(project, c).ok())
        .map_or_else(|| project.clone(), |l| l.as_path().to_path_buf());
    let mut out: Vec<CallPath> = Vec::new();
    for seg in &s.segments {
        let redirects = seg
            .redirects
            .iter()
            .map(|r| r.trim_start_matches(|c: char| c.is_ascii_digit() || "<>&|".contains(c)));
        for w in seg.argv.iter().map(String::as_str).chain(redirects) {
            if w.is_empty() {
                continue;
            }
            let Ok(lex) = paths::classify(&base, w) else {
                continue;
            };
            let path = lex.as_path().to_path_buf();
            let inside = paths::within(project, &path);
            let abs = slashed(&path.to_string_lossy());
            if out.iter().any(|p| p.abs == abs) {
                continue;
            }
            out.push(CallPath {
                abs,
                rel: inside.then(|| paths::display(project, &path)),
                outside: !inside,
                pinned: false,
                protected: false,
                cover: Access::Write,
                path,
            });
        }
    }
    out
}

// ---- Гранты ----

/// Режим гранта: `ro` — чтение, `rw` — чтение, запись и запуск команд в каталоге.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantMode {
    #[default]
    Ro,
    Rw,
}

impl GrantMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ro => "ro",
            Self::Rw => "rw",
        }
    }
}

/// Откуда грант (D78).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantSource {
    /// Флаг `run --grant`.
    Cli,
    /// Одобренный `request_grant`.
    Request,
    /// `[[policy.grants]]` глобальной конфигурации.
    Config,
}

/// Грант доступа вне проекта до конца run (D78); `path` — канонический.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub path: PathBuf,
    pub mode: GrantMode,
    pub source: GrantSource,
    pub reason: String,
}

/// Грант покрывает канонический путь: по компонентам, ординально ([`paths::within`]:
/// `/data` не покрывает `/data2`, Ruling 7.22, I1). `ro` — только чтение; `rw` — и
/// запись, и рабочий каталог команды.
pub fn covers(grants: &[Grant], path: &Path, access: Access) -> bool {
    grants.iter().any(|g| {
        paths::within(&g.path, path)
            && match access {
                Access::Read => true,
                Access::Write | Access::Exec => g.mode == GrantMode::Rw,
            }
    })
}

/// Значение флага `run --grant <путь>[=ro|=rw]` (D78): `=`, потому что `:` — часть пути
/// Windows; без режима — `ro`. Путь с `=` пишется с явным режимом (`/a=b=ro`).
pub fn parse_grant_flag(s: &str) -> Result<(PathBuf, GrantMode), String> {
    let (path, mode) = match s.rsplit_once('=') {
        Some((p, "ro")) => (p, GrantMode::Ro),
        Some((p, "rw")) => (p, GrantMode::Rw),
        Some((_, m)) => {
            return Err(format!(
                "--grant {s}: неизвестный режим `{m}`, ожидается ro или rw (<путь>[=ro|=rw])"
            ));
        }
        None => (s, GrantMode::Ro),
    };
    if path.trim().is_empty() {
        return Err(format!("--grant {s}: пустой путь (<путь>[=ro|=rw])"));
    }
    Ok((PathBuf::from(path), mode))
}

/// Текст блока `<grants>` (D78, без тегов): строки `path (ro|rw)`, пути через
/// [`escape_untrusted`]; без грантов — `none`.
pub fn grants_text(grants: &[Grant], root: &Path) -> String {
    if grants.is_empty() {
        return GRANTS_NONE.to_owned();
    }
    grants
        .iter()
        .map(|g| {
            format!(
                "{} ({})",
                escape_controls(&escape_untrusted(&paths::display(root, &g.path))),
                g.mode.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Управляющие символы пути — видимой записью `\u{..}`: перевод строки в имени не
/// дорисует ложную строку блока `<grants>` (m4 ревью).
fn escape_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// `grants_text` без грантов.
const GRANTS_NONE: &str = "none";

// ---- Решение ----

/// Контекст решения: снимок политики и состояние run (D92).
#[derive(Debug, Clone)]
pub struct PolicyCx<'a> {
    pub policy: &'a PolicySnapshot,
    pub mode: RunMode,
    /// Канонические корни run (D76).
    pub roots: &'a Roots,
    /// Проект доверен (D83).
    pub trusted: bool,
    /// Пользователь отказал проекту в доверии.
    pub trust_refused: bool,
    /// Происхождения недоверенного содержимого run (D82).
    pub taint: &'a BTreeSet<String>,
    pub grants: &'a [Grant],
}

/// Решение о вызове.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Нужно одобрение; причины — константы `approval_reason` в их порядке.
    Ask {
        reasons: Vec<&'static str>,
    },
    /// Нужно доверие к проекту (прерывание `Trust`, D83).
    Trust,
    /// Отказ; строка — причина для `denied: <причина>`.
    Deny(&'static str),
}

/// Решение о вызове (D80). `paths` — разрешение [`subject_paths`] в том же порядке;
/// несоответствие длины — граница неизвестна, `Ask` с `outside_project`.
pub fn decide(
    cx: &PolicyCx<'_>,
    spec: &ToolSpec,
    s: &Subjects,
    paths: &[ResolvedPath],
) -> Decision {
    let call = call_of(cx, spec, s, paths);
    let rules: Vec<Compiled<'_>> = cx
        .policy
        .rules
        .iter()
        .map(|(src, r)| Compiled::new(*src, r))
        .collect();

    // 1. Защищённый путь (D79).
    if call.paths.iter().any(|p| p.protected) {
        return Decision::Deny(deny_reason::PROTECTED_PATH);
    }
    // 2. Правила deny.
    if rules
        .iter()
        .any(|r| r.rule.action == RuleAction::Deny && r.fires(&call))
    {
        return Decision::Deny(deny_reason::POLICY_RULE);
    }
    // 3. Запрет режима. `bypass` без разрешения глобальной конфигурации (снимок не
    // прошёл `check_mode`) послаблений не даёт и решается как `ask`.
    let bypass = cx.mode == RunMode::Bypass && cx.policy.allow_bypass;
    let mode = if cx.mode == RunMode::Bypass && !bypass {
        RunMode::Ask
    } else {
        cx.mode
    };
    let by_mode = gate(mode, spec);
    if let Gate::Deny(reason) = by_mode {
        return Decision::Deny(reason);
    }
    // 4. Команды недоверенного проекта (D83).
    if spec.kind == ToolKind::Exec && !cx.trusted {
        return if cx.trust_refused {
            Decision::Deny(deny_reason::UNTRUSTED_PROJECT)
        } else {
            Decision::Trust
        };
    }
    // 5. Причины Ask.
    let allowed = rules.iter().any(|r| r.allows(&call));
    let risk = effective_risk(spec, s);
    // `Approval::Always` — поправка инструмента: её не снимают ни `bypass`, ни `allow`
    // (I3 ревью, контракт `gate`).
    let always = spec.approval == Approval::Always;
    let want_mode = by_mode == Gate::Ask && (always || (!allowed && !bypass));
    let want_rule = rules
        .iter()
        .any(|r| r.rule.action == RuleAction::Ask && r.fires(&call));
    let want_outside = call.unresolved
        || call.paths.iter().any(|p| {
            p.outside
                && (p.pinned
                    || !(covers(cx.grants, &p.path, p.cover)
                        || rules.iter().any(|r| r.allows_path(&call, p))))
        });
    // Метку run снимает только `bypass`; `allow` снимает лишь вопрос режима (C1 ревью).
    let want_tainted = !cx.taint.is_empty() && risk >= RiskClass::Exec && !bypass;
    let want_irreversible = !s.irreversible.is_empty()
        || s.segments.iter().any(|seg| seg.unparsed)
        || spec.risk >= RiskClass::Irreversible;
    let reasons: Vec<&'static str> = [
        (want_mode, approval_reason::MODE),
        (want_rule, approval_reason::RULE),
        (want_outside, approval_reason::OUTSIDE_PROJECT),
        (want_tainted, approval_reason::TAINTED),
        (want_irreversible, approval_reason::IRREVERSIBLE),
    ]
    .into_iter()
    .filter_map(|(on, r)| on.then_some(r))
    .collect();
    if reasons.is_empty() {
        Decision::Allow
    } else {
        Decision::Ask { reasons }
    }
}

/// Риск вызова: класс инструмента, повышенный разбором команды (D81): метки
/// необратимости — `Irreversible`, сеть — `Network`, запуск команды — не ниже `Exec`.
fn effective_risk(spec: &ToolSpec, s: &Subjects) -> RiskClass {
    let mut risk = spec.risk;
    if spec.kind == ToolKind::Exec {
        risk = risk.max(RiskClass::Exec);
    }
    if s.network {
        risk = risk.max(RiskClass::Network);
    }
    if !s.irreversible.is_empty() {
        risk = risk.max(RiskClass::Irreversible);
    }
    risk
}

// ---- Тексты для модели ----

/// Образцы текстов политики, уходящих модели (PRM-010, D94): отказы и блок `<grants>`.
pub(crate) fn text_samples() -> Vec<(&'static str, String)> {
    let sample = [
        Grant {
            path: PathBuf::from("/data"),
            mode: GrantMode::Ro,
            source: GrantSource::Cli,
            reason: String::new(),
        },
        Grant {
            path: PathBuf::from("/srv"),
            mode: GrantMode::Rw,
            source: GrantSource::Request,
            reason: String::new(),
        },
    ];
    let root = Path::new("/project");
    let denied = crate::tools::denied_text;
    vec![
        (
            "runtime/policy.denied_policy_rule",
            denied(deny_reason::POLICY_RULE),
        ),
        (
            "runtime/policy.denied_protected_path",
            denied(deny_reason::PROTECTED_PATH),
        ),
        (
            "runtime/policy.denied_untrusted_project",
            denied(deny_reason::UNTRUSTED_PROJECT),
        ),
        ("runtime/policy.grants", grants_text(&sample, root)),
        ("runtime/policy.grants_none", grants_text(&[], root)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotdot_patterns_never_match() {
        // Снимок из журнала с `..` в шаблоне (мимо `build`): allow не срабатывает,
        // deny — срабатывает.
        let allow = Rule {
            action: RuleAction::Allow,
            tool: None,
            path: Some("/data/../**".into()),
            command: None,
        };
        let c = Compiled::new(RuleSource::Global, &allow);
        assert!(c.broken());
    }

    #[test]
    fn argv0_pattern_normalized() {
        assert_eq!(argv0_pattern("RM.EXE"), "rm");
        assert_eq!(argv0_pattern("git push*"), "git push*");
    }
}
