//! Правила игнорирования обхода `search` и `glob` (D87; ревью задачи 6, C1 и I1).
//!
//! Крейт `ignore` сам открывает `.gitignore`, `.ignore` и `.git/info/exclude` через
//! `File::open` и идёт по ссылкам. Ссылка `.gitignore` на `/dev/zero`, FIFO или
//! `\\host\share` дала бы OOM, зависание потока или SMB-запрос с утечкой NTLM до всякого
//! решения политики. Поэтому чтение правил у `ignore` выключено. Файлы правил агент читает
//! сам через [`open_checked`]: только обычный файл, не ссылка, не больше
//! [`RULES_MAX_BYTES`]. Иначе файл пропускается с `warn`. Текст разбирается
//! [`GitignoreBuilder::add_line`].
//!
//! Правила собираются от корня проекта вниз до каталога поиска (I1): корневой
//! `.gitignore` действует и для `search(path="sub")`. Выше корня проекта правил нет
//! (Ruling 5.2). Глобальные правила git не читаются. `.git/info/exclude` берётся только у
//! каталога, где `.git` — настоящий каталог, а не ссылка и не файл. Для внешнего каталога
//! поиска (по гранту) правила начинаются с него самого.
//!
//! Порядок, как у `ignore`: правила глубже сильнее; в одном каталоге `.ignore` сильнее
//! `.gitignore`, а тот — `.git/info/exclude`. Первое совпадение решает: `!` открывает путь.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use za_core::paths::{ResolvedPath, open_checked, stat_checked, within};

/// Предел размера файла правил.
pub(crate) const RULES_MAX_BYTES: u64 = 1 << 20;
/// Файлы правил каталога по убыванию силы.
const RULE_FILES: [&str; 2] = [".ignore", ".gitignore"];

/// Правила одного каталога по убыванию силы.
struct DirRules(Vec<Gitignore>);

impl DirRules {
    fn load(dir: &Path, inside: bool) -> Self {
        let mut out: Vec<Gitignore> = RULE_FILES
            .iter()
            .filter_map(|name| read_rules(dir, &dir.join(name), inside))
            .collect();
        if let Some(ex) = exclude_path(dir, inside) {
            out.extend(read_rules(dir, &ex, inside));
        }
        Self(out)
    }

    /// `Some(true)` — игнорируется, `Some(false)` — открыт `!`, `None` — правила молчат.
    fn decide(&self, path: &Path, is_dir: bool) -> Option<bool> {
        self.0.iter().find_map(|g| match g.matched(path, is_dir) {
            Match::None => None,
            Match::Ignore(_) => Some(true),
            Match::Whitelist(_) => Some(false),
        })
    }
}

fn resolved(path: PathBuf, inside: bool) -> ResolvedPath {
    ResolvedPath {
        path,
        inside,
        protected: false,
    }
}

/// `<dir>/.git/info/exclude`, если `.git` и `.git/info` — настоящие каталоги.
fn exclude_path(dir: &Path, inside: bool) -> Option<PathBuf> {
    let git = dir.join(".git");
    let info = git.join("info");
    for d in [&git, &info] {
        match stat_checked(&resolved(d.clone(), inside)) {
            Ok(m) if m.is_dir() => {}
            _ => return None,
        }
    }
    Some(info.join("exclude"))
}

/// Матчер из файла правил `file` с корнем `dir`; файла нет — `None` молча, файл небезопасен
/// или слишком велик — `None` с `warn`.
fn read_rules(dir: &Path, file: &Path, inside: bool) -> Option<Gitignore> {
    let f = match open_checked(&resolved(file.to_path_buf(), inside), false) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %file.display(), error = %e, "файл правил игнорирования пропущен");
            return None;
        }
    };
    let mut bytes = Vec::new();
    if let Err(e) = (&f).take(RULES_MAX_BYTES + 1).read_to_end(&mut bytes) {
        tracing::warn!(path = %file.display(), error = %e, "файл правил игнорирования не прочитан");
        return None;
    }
    if bytes.len() as u64 > RULES_MAX_BYTES {
        tracing::warn!(path = %file.display(), limit = RULES_MAX_BYTES, "файл правил игнорирования больше предела, пропущен");
        return None;
    }
    let mut b = GitignoreBuilder::new(dir);
    for line in String::from_utf8_lossy(&bytes).lines() {
        if let Err(e) = b.add_line(None, line) {
            tracing::debug!(path = %file.display(), error = %e, "строка правил пропущена");
        }
    }
    match b.build() {
        Ok(g) if !g.is_empty() => Some(g),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(path = %file.display(), error = %e, "правила игнорирования не собраны");
            None
        }
    }
}

/// Правила обхода одного вызова: цепочка каталогов над корнем поиска и кэш каталогов
/// внутри него. Обход однопоточный; `Mutex` нужен только потому, что `filter_entry` —
/// `Sync`.
pub(crate) struct Rules {
    base: PathBuf,
    inside: bool,
    /// Каталоги от корня проекта (или от `base`) до `base` включительно, сверху вниз.
    above: Vec<PathBuf>,
    cache: Mutex<HashMap<PathBuf, Arc<DirRules>>>,
}

impl Rules {
    /// Правила для обхода `base` (канонический каталог); `root` — канонический корень
    /// проекта. Блокирующая.
    pub(crate) fn new(root: &Path, base: &Path, inside: bool) -> Self {
        let mut above = Vec::new();
        if inside && within(root, base) {
            let mut d = root.to_path_buf();
            above.push(d.clone());
            for c in base.components().skip(root.components().count()) {
                d.push(c.as_os_str());
                above.push(d.clone());
            }
        } else {
            above.push(base.to_path_buf());
        }
        let rules = Self {
            base: base.to_path_buf(),
            inside,
            above,
            cache: Mutex::new(HashMap::new()),
        };
        for d in rules.above.clone() {
            rules.dir(&d);
        }
        rules
    }

    fn dir(&self, d: &Path) -> Arc<DirRules> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .entry(d.to_path_buf())
            .or_insert_with(|| Arc::new(DirRules::load(d, self.inside)))
            .clone()
    }

    /// Игнорируется ли запись обхода `path` (внутри `base`).
    pub(crate) fn ignored(&self, path: &Path, is_dir: bool) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        // Каталоги от родителя записи вверх до `base`, затем над ним.
        let mut chain: Vec<PathBuf> = Vec::new();
        let mut d = parent;
        while d != self.base && d.starts_with(&self.base) {
            chain.push(d.to_path_buf());
            match d.parent() {
                Some(p) => d = p,
                None => break,
            }
        }
        chain.extend(self.above.iter().rev().cloned());
        chain
            .iter()
            .find_map(|d| self.dir(d).decide(path, is_dir))
            .unwrap_or(false)
    }
}
