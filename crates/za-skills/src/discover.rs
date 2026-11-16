//! Обнаружение скиллов и каталог (SKL-001, SKL-002, D114, D116).
//!
//! Источники по старшинству: `user` (`ZA_HOME/skills`) > `bundled` (поставка) > `extra`
//! (`[skills] dirs` по порядку списка) > `project` (`.zero-agent/skills` >
//! `.agents/skills` > `.claude/skills` доверенного проекта). Младший скилл с тем же именем
//! пропускается с причиной `shadows <source> skill <name>`; повтор имени внутри источника
//! (в одном корне — второй по пути, порядок байтов; в разных корнях источника — младший
//! корень) — `duplicate skill <name>`.
//!
//! Проект читается только по списку доверенного обхода ([`ProjectSkills`], D116): каждый
//! прочитанный `SKILL.md` сверяется по BLAKE3 со значением списка (`open_checked`);
//! значения `external-link` и `too-large:<size>` не читаются. `user` и `extra` обходятся
//! тем же [`crate::tree_listing`] с корнем-каталогом, без перехода по ссылкам, со своими
//! пределами ([`LOCAL_TREE_LIMITS`]) и без каталогов [`SKIP_DIRS`].

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};

use za_core::paths::Roots;
use za_protocol::skill_source;

use crate::parse::{self, Parsed, parse_skill_md};
use crate::tree::{
    self, EXTERNAL_LINK, LINK_PREFIX, PROJECT_TREES, Place, ReadFail, TOO_LARGE_PREFIX,
    TRUST_LIMITS, TreeLimits, TreeListing,
};

/// Файл скилла.
pub const SKILL_FILE: &str = "SKILL.md";
/// Каталоги, в которые обнаружение не заходит (D116).
pub const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", "__pycache__"];
/// Пределы обхода `user` и `extra` (на хэш доверия проекта не влияют): 1 МиБ на файл, как
/// у доверия, и 20 000 записей на корень — с запасом над 2000 каталогами по 100 файлов
/// реального набора скиллов и без многосекундного обхода чужого огромного каталога.
pub const LOCAL_TREE_LIMITS: TreeLimits = TreeLimits {
    max_file: 1 << 20,
    max_entries: 20_000,
};

/// Пределы обнаружения (D116).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Глубина каталога скилла от корня источника, компоненты.
    pub depth: usize,
    /// Каталогов на источник (по путям файлов; пустые не в счёт).
    pub dirs: usize,
    /// Файлов-ресурсов на скилл (без `SKILL.md`).
    pub files_per_skill: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            depth: 6,
            dirs: 2000,
            files_per_skill: 100,
        }
    }
}

/// Скилл каталога.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    pub name: String,
    /// Описание (очищено уровнем `Untrusted`, D102).
    pub description: String,
    /// `za_protocol::skill_source`.
    pub source: &'static str,
    /// Канонический каталог скилла; `None` — поставка.
    pub dir: Option<PathBuf>,
    /// Путь для человека: у проекта — от корня проекта (`.claude/skills/x`), у `user` и
    /// `extra` — от заданного каталога, у поставки — `bundled:<name>`.
    pub display: String,
    /// BLAKE3 содержимого `SKILL.md` (hex).
    pub hash: String,
    /// Ресурсы: путь от каталога скилла → значение списка (hex или `link:<hex>`).
    pub files: BTreeMap<String, String>,
    pub warnings: Vec<String>,
    /// `allowed-tools` (SKL-007: прав не меняют).
    pub allowed_tools: Vec<String>,
}

/// Пропущенный скилл или источник.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

/// Каталог скиллов run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// По старшинству источника, затем по имени.
    pub skills: Vec<SkillMeta>,
    pub skipped: Vec<Skipped>,
}

impl Catalog {
    pub fn get(&self, name: &str) -> Option<&SkillMeta> {
        self.skills.iter().find(|s| s.name == name)
    }
}

/// Деревья скиллов доверенного проекта: только те, чей хэш сверен с доверенным набором.
#[derive(Debug, Clone)]
pub struct ProjectSkills {
    pub roots: Roots,
    pub trees: Vec<TreeListing>,
}

/// Вход обнаружения. `Default` — без источников (и без поставки).
#[derive(Debug, Clone, Default)]
pub struct DiscoverInput {
    /// `None` — проект не доверен (SKL-002) или проектные скиллы выключены.
    pub project: Option<ProjectSkills>,
    /// `ZA_HOME/skills`.
    pub user_dir: Option<PathBuf>,
    /// `[skills] dirs` глобальной конфигурации по порядку.
    pub extra_dirs: Vec<PathBuf>,
    /// Поставка: пары (имя каталога, текст `SKILL.md`). Продукт передаёт
    /// [`crate::bundled`], тест — свою заглушку.
    pub bundled: &'static [(&'static str, &'static str)],
    pub limits: Limits,
}

/// Каталог скиллов (D114, D116). Блокирующая: из асинхронного кода — `spawn_blocking`.
pub fn discover(input: &DiscoverInput) -> Catalog {
    let mut d = Discovery::default();
    if let Some(dir) = &input.user_dir {
        d.local(skill_source::USER, dir, &input.limits);
    }
    for (name, text) in input.bundled {
        d.bundled(name, text);
    }
    for dir in &input.extra_dirs {
        d.local(skill_source::EXTRA, dir, &input.limits);
    }
    if let Some(p) = &input.project {
        let mut trees: Vec<&TreeListing> = p.trees.iter().collect();
        // Старшинство деревьев проекта — по `PROJECT_TREES`, в каком бы порядке ни пришли.
        trees.sort_by_key(|t| {
            PROJECT_TREES
                .iter()
                .position(|r| *r == t.rel)
                .unwrap_or(usize::MAX)
        });
        for t in trees {
            let root = Root {
                source: skill_source::PROJECT,
                abs: join_rel(&p.roots.project, &t.rel),
                display: Display::Project(t.rel.clone()),
            };
            d.scan(&root, &t.files, &input.limits);
        }
    }
    d.cat
        .skills
        .sort_by(|a, b| (rank(a.source), &a.name).cmp(&(rank(b.source), &b.name)));
    d.cat
}

/// Ранг источника: меньше — старше.
fn rank(source: &str) -> usize {
    [
        skill_source::USER,
        skill_source::BUNDLED,
        skill_source::EXTRA,
        skill_source::PROJECT,
    ]
    .iter()
    .position(|s| *s == source)
    .unwrap_or(usize::MAX)
}

/// Как показывать пути корня.
enum Display {
    /// Дерево проекта: путь от корня проекта.
    Project(String),
    /// Каталог `user` или `extra`, как его задали.
    Local(PathBuf),
}

/// Корень источника.
struct Root {
    source: &'static str,
    /// Канонический путь корня (каталоги скиллов — от него).
    abs: PathBuf,
    display: Display,
}

impl Root {
    fn show(&self, rel: &str) -> String {
        match &self.display {
            Display::Project(tree) if rel.is_empty() => tree.clone(),
            Display::Project(tree) => format!("{tree}/{rel}"),
            Display::Local(dir) => join_rel(dir, rel).display().to_string(),
        }
    }
}

/// `base` и относительный путь через `/` — по компонентам (разделители ОС).
fn join_rel(base: &Path, rel: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    for part in rel.split('/').filter(|s| !s.is_empty()) {
        p.push(part);
    }
    p
}

#[derive(Default)]
struct Discovery {
    cat: Catalog,
    /// Имя → источник, давший скилл.
    taken: BTreeMap<String, &'static str>,
    /// Каталогов учтено по источнику.
    dirs_used: BTreeMap<&'static str, usize>,
    /// Источники, остановленные пределом.
    stopped: BTreeSet<&'static str>,
}

impl Discovery {
    fn skip(&mut self, path: String, reason: impl Into<String>) {
        self.cat.skipped.push(Skipped {
            path,
            reason: reason.into(),
        });
    }

    /// Каталог `user` или `extra`: обход и разбор.
    fn local(&mut self, source: &'static str, dir: &Path, limits: &Limits) {
        if self.stopped.contains(source) {
            return;
        }
        let shown = dir.display().to_string();
        let roots = match Roots::canonical(dir, dir) {
            Ok(r) => r,
            Err(e) if tree::is_missing(&e) => return,
            Err(e) => return self.skip(shown, format!("unreadable: {e}")),
        };
        let listing = match tree::listing_with(&roots, "", &LOCAL_TREE_LIMITS, SKIP_DIRS) {
            Ok((Place::Link, _)) => {
                self.stopped.insert(source);
                return self.skip(
                    shown,
                    format!(
                        "limit reached: more than {} entries",
                        LOCAL_TREE_LIMITS.max_entries
                    ),
                );
            }
            Ok((_, Some(l))) => l,
            Ok((_, None)) => return,
            Err(e) => return self.skip(shown, format!("unreadable: {e}")),
        };
        let root = Root {
            source,
            abs: roots.project,
            display: Display::Local(dir.to_path_buf()),
        };
        self.scan(&root, &listing.files, limits);
    }

    /// Скилл поставки.
    fn bundled(&mut self, dir_name: &str, text: &'static str) {
        let shown = format!("{}:{dir_name}", skill_source::BUNDLED);
        let parsed = match parse_skill_md(text, dir_name) {
            Ok(p) => p,
            Err(e) => return self.skip(shown, e.to_string()),
        };
        let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
        self.add(
            skill_source::BUNDLED,
            None,
            shown,
            hash,
            parsed,
            BTreeMap::new(),
        );
    }

    /// Скиллы корня по списку файлов `files` (пути от корня).
    fn scan(&mut self, root: &Root, files: &BTreeMap<String, String>, limits: &Limits) {
        if self.stopped.contains(root.source) {
            return;
        }
        let accepted = self.accept(root, files, limits);
        // Кандидаты — каталоги с `SKILL.md`; вложенные в другой кандидат — ресурсы.
        let candidates: BTreeSet<&str> = accepted
            .keys()
            .filter_map(|p| p.strip_suffix("/SKILL.md"))
            .collect();
        if accepted.contains_key(SKILL_FILE) {
            self.skip(
                root.show(""),
                "SKILL.md at the source root is not in a skill directory",
            );
        }
        for dir in &candidates {
            let nested = dir
                .match_indices('/')
                .any(|(i, _)| candidates.contains(&dir[..i]));
            if nested {
                continue;
            }
            if dir.split('/').count() > limits.depth {
                self.skip(
                    root.show(dir),
                    format!("limit reached: deeper than {} directories", limits.depth),
                );
                continue;
            }
            self.skill(root, dir, &accepted, limits);
        }
    }

    /// Файлы корня в пределах числа каталогов источника (пути по порядку байтов); каталоги
    /// пропуска не входят. Сверх предела — `limit reached` и остановка источника.
    fn accept<'f>(
        &mut self,
        root: &Root,
        files: &'f BTreeMap<String, String>,
        limits: &Limits,
    ) -> BTreeMap<&'f str, &'f str> {
        let used = self.dirs_used.get(root.source).copied().unwrap_or(0);
        let budget = limits.dirs.saturating_sub(used);
        let mut dirs: BTreeSet<&str> = BTreeSet::new();
        let mut accepted = BTreeMap::new();
        for (path, value) in files {
            let parts: Vec<&str> = path.split('/').collect();
            if parts[..parts.len() - 1]
                .iter()
                .any(|c| SKIP_DIRS.contains(c))
            {
                continue;
            }
            let fresh: Vec<&str> = path
                .match_indices('/')
                .map(|(i, _)| &path[..i])
                .filter(|d| !dirs.contains(d))
                .collect();
            if dirs.len() + fresh.len() > budget {
                self.stopped.insert(root.source);
                self.skip(
                    root.show(""),
                    format!("limit reached: more than {} directories", limits.dirs),
                );
                break;
            }
            dirs.extend(fresh);
            accepted.insert(path.as_str(), value.as_str());
        }
        *self.dirs_used.entry(root.source).or_insert(0) += dirs.len();
        accepted
    }

    /// Скилл в каталоге `dir` корня: чтение `SKILL.md` со сверкой хэша, разбор, ресурсы.
    fn skill(&mut self, root: &Root, dir: &str, accepted: &BTreeMap<&str, &str>, limits: &Limits) {
        let shown = root.show(dir);
        let value = accepted
            .get(format!("{dir}/{SKILL_FILE}").as_str())
            .copied()
            .unwrap_or(EXTERNAL_LINK);
        if value == EXTERNAL_LINK {
            return self.skip(shown, "external link");
        }
        if let Some(size) = value.strip_prefix(TOO_LARGE_PREFIX) {
            return self.skip(shown, format!("too large: {size} bytes"));
        }
        let expected = value.strip_prefix(LINK_PREFIX).unwrap_or(value);
        let changed = if root.source == skill_source::PROJECT {
            "changed since trust"
        } else {
            "changed while reading"
        };
        let abs = join_rel(&root.abs, dir);
        let bytes = match tree::read_checked(&abs, SKILL_FILE, TRUST_LIMITS.max_file) {
            Ok(b) => b,
            Err(ReadFail::Missing | ReadFail::TooLarge) => return self.skip(shown, changed),
            Err(ReadFail::Other(e)) => return self.skip(shown, format!("unreadable: {e}")),
        };
        if blake3::hash(&bytes).to_hex().as_str() != expected {
            return self.skip(shown, changed);
        }
        let dir_name = dir.rsplit('/').next().unwrap_or(dir);
        let mut parsed = match parse_skill_md(&String::from_utf8_lossy(&bytes), dir_name) {
            Ok(p) => p,
            Err(e) => return self.skip(shown, e.to_string()),
        };
        // Ресурсы: файлы каталога скилла, кроме его `SKILL.md`.
        let prefix = format!("{dir}/");
        let mut files = BTreeMap::new();
        let warnings = &mut parsed.warnings;
        let mut over = 0usize;
        let from = (Bound::Included(prefix.as_str()), Bound::Unbounded);
        for (path, v) in accepted.range::<str, _>(from) {
            let Some(inner) = path.strip_prefix(&prefix) else {
                break;
            };
            if inner == SKILL_FILE {
                continue;
            }
            if *v == EXTERNAL_LINK {
                warnings.push(format!(
                    "file {} is not listed: external link",
                    inner.escape_debug()
                ));
                continue;
            }
            if v.starts_with(TOO_LARGE_PREFIX) {
                warnings.push(format!(
                    "file {} is not listed: too large",
                    inner.escape_debug()
                ));
                continue;
            }
            if files.len() == limits.files_per_skill {
                over += 1;
                continue;
            }
            files.insert(inner.to_owned(), (*v).to_owned());
        }
        if over > 0 {
            warnings.push(format!(
                "limit reached: {over} files beyond {} are not listed",
                limits.files_per_skill
            ));
        }
        self.add(
            root.source,
            Some(abs),
            shown,
            expected.to_owned(),
            parsed,
            files,
        );
    }

    /// Скилл в каталог, если имя свободно; иначе — пропуск с причиной.
    fn add(
        &mut self,
        source: &'static str,
        dir: Option<PathBuf>,
        shown: String,
        hash: String,
        parsed: Parsed,
        files: BTreeMap<String, String>,
    ) {
        let Some(name) = parsed.name.clone() else {
            return self.skip(shown, "invalid skill name");
        };
        if let Some(owner) = self.taken.get(&name) {
            let reason = if *owner == source {
                format!("duplicate skill {name}")
            } else {
                format!("shadows {owner} skill {name}")
            };
            return self.skip(shown, reason);
        }
        self.taken.insert(name.clone(), source);
        self.cat.skills.push(SkillMeta {
            name,
            description: parsed.description,
            source,
            dir,
            display: shown,
            hash,
            allowed_tools: parse::allowed_tools(&parsed.fields),
            files,
            warnings: parsed.warnings,
        });
    }
}
