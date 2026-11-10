//! Правила игнорирования обхода `search` и `glob` (D87; ревью задачи 6: C1, I1, N1–N3, N6).
//!
//! Крейт `ignore` сам открывает `.gitignore`, `.ignore` и `.git/info/exclude` через
//! `File::open` и идёт по ссылкам. Ссылка `.gitignore` на `/dev/zero`, FIFO или
//! `\\host\share` дала бы OOM, зависание потока или SMB-запрос с утечкой NTLM до всякого
//! решения политики. Поэтому чтение правил у `ignore` выключено. Файлы правил агент читает
//! сам через [`open_checked`]: только обычный файл, не ссылка, не больше
//! [`RULES_MAX_BYTES`]. Иначе файл пропускается с `warn`. Текст разбирается
//! [`GitignoreBuilder::add_line`].
//!
//! **Цепочка.** Правила собираются от корня проекта вниз до каталога поиска (I1): корневой
//! `.gitignore` действует и для `search(path="sub")`. Выше корня проекта правил нет
//! (Ruling 5.2). Глобальные правила git не читаются. `.git/info/exclude` берётся только у
//! каталога, где `.git` — настоящий каталог, а не ссылка и не файл. Для внешнего каталога
//! поиска (по гранту) правила начинаются с него самого.
//!
//! **Память (N1).** Обход однопоточный и в глубину, поэтому хранится только стек правил
//! предков текущей записи. Каталог, из которого обход вышел, освобождается. Текст правил в
//! стеке в сумме не больше [`RULES_BUDGET_BYTES`]. Файл сверх бюджета не загружается: в
//! лог идёт `warn`, а вызов получает пометку ([`Rules::over_budget`]). Скомпилированные
//! правила занимают примерно в 20 раз больше текста.
//!
//! **Время (N2).** Перед каждым файлом правил проверяются дедлайн и отмена вызова. После
//! остановки правила не загружаются, обход останавливается на следующей записи.
//!
//! **Регистр (N3).** На Windows имена `.gitignore`, `.ignore`, `.git`, `info`, `exclude`
//! ищутся в списке каталога без учёта ASCII-регистра (`.GITIGNORE`) и открываются по
//! фактическому имени. Иначе сверка канонического пути в `open_checked` отвергла бы
//! открытие как подмену.
//!
//! **Порядок — как у `ignore` (N6).** Вид файла сильнее глубины: сначала `.ignore` по всей
//! цепочке от глубокого каталога к корню, затем так же `.gitignore`, затем `exclude`. Первое
//! совпадение решает, `!` открывает путь.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use za_core::paths::{ResolvedPath, open_checked, stat_checked, within};

use crate::fs::WalkLimits;

/// Предел размера одного файла правил.
pub(crate) const RULES_MAX_BYTES: u64 = 1 << 20;
/// Предел суммарного текста правил в стеке предков (N1).
pub(crate) const RULES_BUDGET_BYTES: usize = 8 << 20;
/// Виды файлов правил по убыванию силы; `exclude` — третий вид, лежит в `.git/info`.
const RULE_FILES: [&str; 2] = [".ignore", ".gitignore"];
/// Число видов правил.
const KINDS: usize = 3;

/// Правила одного каталога по видам ([`RULE_FILES`], затем `exclude`).
struct Level {
    dir: PathBuf,
    kinds: [Option<Gitignore>; KINDS],
    /// Байт текста правил этого каталога в бюджете.
    bytes: usize,
}

struct State {
    /// Каталоги от корня проекта (или от `base`) вниз до текущего; первые `fixed` — над
    /// обходом и не снимаются.
    stack: Vec<Level>,
    held: usize,
    peak: usize,
    over_budget: bool,
}

/// Правила обхода одного вызова. `Mutex` нужен только потому, что `filter_entry` — `Sync`;
/// обход однопоточный.
pub(crate) struct Rules {
    base: PathBuf,
    inside: bool,
    fixed: usize,
    limits: WalkLimits,
    state: Mutex<State>,
}

fn resolved(path: PathBuf, inside: bool) -> ResolvedPath {
    ResolvedPath {
        path,
        inside,
        protected: false,
    }
}

/// Имена записей каталога для поиска без учёта регистра (Windows, N3); на Unix — `None`:
/// имена сравниваются точно и список не нужен.
fn listing(dir: &Path, inside: bool) -> Option<Vec<OsString>> {
    if cfg!(windows) {
        Some(
            za_core::paths::read_dir_checked(&resolved(dir.to_path_buf(), inside))
                .unwrap_or_default(),
        )
    } else {
        None
    }
}

/// Путь записи `name` в `dir`: на Windows — фактическое имя из `names` без учёта
/// ASCII-регистра, нет такой записи — `None`.
fn entry(dir: &Path, names: Option<&[OsString]>, name: &str) -> Option<PathBuf> {
    match names {
        None => Some(dir.join(name)),
        Some(list) => list
            .iter()
            .find(|n| n.to_str().is_some_and(|s| s.eq_ignore_ascii_case(name)))
            .map(|n| dir.join(n)),
    }
}

impl Rules {
    /// Правила для обхода `base` (канонический каталог); `root` — канонический корень
    /// проекта. Блокирующая: загружает правила каталогов над обходом.
    pub(crate) fn new(root: &Path, base: &Path, inside: bool, limits: &WalkLimits) -> Self {
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
            fixed: above.len(),
            limits: limits.clone(),
            state: Mutex::new(State {
                stack: Vec::new(),
                held: 0,
                peak: 0,
                over_budget: false,
            }),
        };
        {
            let mut st = rules.lock();
            for d in above {
                rules.push(&mut st, d);
            }
        }
        rules
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Загружает правила каталога `dir` и кладёт его на стек.
    fn push(&self, st: &mut State, dir: PathBuf) {
        let names = listing(&dir, self.inside);
        let mut level = Level {
            kinds: [None, None, None],
            bytes: 0,
            dir,
        };
        let exclude = self.exclude_path(&level.dir, names.as_deref());
        for (i, file) in RULE_FILES
            .iter()
            .map(|n| entry(&level.dir, names.as_deref(), n))
            .chain([exclude])
            .enumerate()
        {
            let Some(file) = file else { continue };
            if let Some((g, len)) = self.read_rules(st, &level.dir, &file) {
                level.kinds[i] = Some(g);
                level.bytes += len;
            }
        }
        st.held += level.bytes;
        st.peak = st.peak.max(st.held);
        st.stack.push(level);
    }

    /// `<dir>/.git/info/exclude`, если `.git` и `.git/info` — настоящие каталоги.
    fn exclude_path(&self, dir: &Path, names: Option<&[OsString]>) -> Option<PathBuf> {
        let git = entry(dir, names, ".git")?;
        let is_dir = |p: &Path| {
            stat_checked(&resolved(p.to_path_buf(), self.inside)).is_ok_and(|m| m.is_dir())
        };
        if !is_dir(&git) {
            return None;
        }
        let info = entry(&git, listing(&git, self.inside).as_deref(), "info")?;
        if !is_dir(&info) {
            return None;
        }
        entry(&info, listing(&info, self.inside).as_deref(), "exclude")
    }

    /// Матчер из файла правил `file` с корнем `dir` и размер его текста. Файла нет —
    /// `None` молча; файл небезопасен, слишком велик, сверх бюджета или вызов остановлен —
    /// `None` (с `warn`, кроме остановки).
    fn read_rules(&self, st: &mut State, dir: &Path, file: &Path) -> Option<(Gitignore, usize)> {
        if self.limits.stopped() {
            return None;
        }
        let f = match open_checked(&resolved(file.to_path_buf(), self.inside), false) {
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
        if st.held + bytes.len() > RULES_BUDGET_BYTES {
            tracing::warn!(path = %file.display(), budget = RULES_BUDGET_BYTES, "правила игнорирования сверх бюджета памяти, файл не загружен");
            st.over_budget = true;
            return None;
        }
        let mut b = GitignoreBuilder::new(dir);
        for line in String::from_utf8_lossy(&bytes).lines() {
            if self.limits.stopped() {
                return None;
            }
            if let Err(e) = b.add_line(None, line) {
                tracing::debug!(path = %file.display(), error = %e, "строка правил пропущена");
            }
        }
        match b.build() {
            Ok(g) if !g.is_empty() => Some((g, bytes.len())),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(path = %file.display(), error = %e, "правила игнорирования не собраны");
                None
            }
        }
    }

    /// Игнорируется ли запись обхода `path` (внутри `base`, в порядке обхода в глубину).
    pub(crate) fn ignored(&self, path: &Path, is_dir: bool) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        if !parent.starts_with(&self.base) {
            return false;
        }
        let mut st = self.lock();
        // Снять каталоги, из которых обход вышел.
        while st.stack.len() > self.fixed
            && st.stack.last().is_some_and(|l| !parent.starts_with(&l.dir))
        {
            if let Some(l) = st.stack.pop() {
                st.held -= l.bytes;
            }
        }
        // Положить недостающие каталоги до родителя (в глубину — обычно один).
        let Some(top) = st.stack.last().map(|l| l.dir.clone()) else {
            return false;
        };
        if let Ok(rest) = parent.strip_prefix(&top) {
            let mut d = top;
            for c in rest.components() {
                d.push(c.as_os_str());
                self.push(&mut st, d.clone());
            }
        }
        (0..KINDS)
            .find_map(|k| {
                st.stack.iter().rev().find_map(|l| {
                    // Только правила предков записи: `Gitignore` чужого каталога
                    // сопоставил бы шаблон без `/` с любым путём.
                    if !path.starts_with(&l.dir) {
                        return None;
                    }
                    l.kinds[k]
                        .as_ref()
                        .and_then(|g| match g.matched(path, is_dir) {
                            Match::None => None,
                            Match::Ignore(_) => Some(true),
                            Match::Whitelist(_) => Some(false),
                        })
                })
            })
            .unwrap_or(false)
    }

    /// Какие-то файлы правил не загружены из-за бюджета памяти.
    pub(crate) fn over_budget(&self) -> bool {
        self.lock().over_budget
    }

    /// Наибольший суммарный текст правил в стеке за обход (для тестов, N1).
    #[cfg(test)]
    pub(crate) fn peak(&self) -> usize {
        self.lock().peak
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::{RULES_BUDGET_BYTES, Rules};
    use crate::fs::WalkLimits;

    fn dir(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.tmp/za-tools-unit");
        let d = crate::unique_dir::unique_dir_in(&root, name);
        za_core::paths::Roots::canonical(&d, &d).unwrap().project
    }

    fn limits() -> WalkLimits {
        WalkLimits::new(Instant::now() + Duration::from_secs(3600))
    }

    /// Файл правил ~`size` байт: первая строка — шаблон, дальше комментарии.
    fn rules_file(path: &Path, first: &str, size: usize) {
        let mut text = format!("{first}\n");
        while text.len() < size {
            text.push_str("# padding padding padding padding padding padding\n");
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn siblings_are_released_and_budget_caps_depth() {
        // 20 соседних каталогов по ~900 КиБ правил: в стеке одновременно один.
        let root = dir("rules_siblings");
        for i in 0..20 {
            rules_file(
                &root.join(format!("d{i:02}/.gitignore")),
                &format!("x{i}.tmp"),
                900 << 10,
            );
        }
        let r = Rules::new(&root, &root, true, &limits());
        for i in 0..20 {
            let d = root.join(format!("d{i:02}"));
            assert!(!r.ignored(&d, true));
            assert!(r.ignored(&d.join(format!("x{i}.tmp")), false), "d{i:02}");
            // Правила соседа сюда не действуют.
            assert!(
                !r.ignored(&d.join(format!("x{}.tmp", i + 1)), false),
                "d{i:02}"
            );
        }
        assert!(r.peak() < 1 << 20, "peak {}", r.peak());
        assert!(!r.over_budget());

        // Глубокая цепочка по ~900 КиБ: сверх 8 МиБ правила глубже не загружаются.
        let root = dir("rules_deep");
        let mut d = root.clone();
        let mut chain = Vec::new();
        for i in 0..12 {
            d.push(format!("l{i}"));
            rules_file(&d.join(".gitignore"), &format!("deep{i}.tmp"), 900 << 10);
            chain.push(d.clone());
        }
        let r = Rules::new(&root, &root, true, &limits());
        for c in &chain {
            r.ignored(c, true);
        }
        let last = chain.last().unwrap();
        assert!(r.ignored(&last.join("deep0.tmp"), false));
        assert!(!r.ignored(&last.join("deep11.tmp"), false));
        assert!(r.over_budget());
        assert!(r.peak() <= RULES_BUDGET_BYTES, "peak {}", r.peak());
    }

    #[test]
    fn stopped_call_loads_no_rules() {
        let root = dir("rules_stopped");
        rules_file(&root.join("sub/.gitignore"), "x.tmp", 10);
        let l = limits();
        let r = Rules::new(&root, &root, true, &l);
        l.cancel();
        assert!(!r.ignored(&root.join("sub/x.tmp"), false));
        assert_eq!(r.peak(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn rule_file_names_ignore_ascii_case_on_windows() {
        let root = dir("rules_case");
        rules_file(&root.join("sub/.GITIGNORE"), "x.tmp", 10);
        rules_file(&root.join(".Git/Info/EXCLUDE"), "y.tmp", 10);
        let r = Rules::new(&root, &root, true, &limits());
        assert!(r.ignored(&root.join("sub/x.tmp"), false));
        assert!(r.ignored(&root.join("y.tmp"), false));
    }
}
