//! Скиллы в бинарнике (R1-C2 D114, D116, D122): каталог run и команда `skills list`.
//!
//! **Каталог** ([`catalog`]) — `za_skills::discover` вне потоков tokio по источникам
//! `user` (`ZA_HOME/skills`), `bundled` (core pack, `za_skills::bundled`), `extra`
//! (`[skills] dirs`) и `project` — только у доверенного проекта и только по деревьям, чей
//! нынешний обход совпал с доверенным набором ([`project_skills`], D116): `discover` читает
//! лишь файлы из этого списка и сверяет BLAKE3 каждого прочитанного. Дерево, изменённое
//! между проверкой доверия и обходом, скиллов не даёт: пропуск с причиной
//! [`TREE_CHANGED`] (защита от гонки; основной путь — недоверие проекта целиком, D83).
//!
//! **Предел записей** `[skills] max_entries` (перенос задачи 12) действует на каждый корень
//! `user` и `extra` отдельно: переполненный корень пропускается целиком с причиной
//! `limit reached: more than <N> entries`, следующие корни обходятся. `skills list` и
//! `doctor` называют такой корень и предел ([`limit_lines`]).

use std::io;
use std::path::Path;
use std::process::ExitCode;

use za_core::paths::Roots;
use za_protocol::skill_source;
use za_skills::{
    Catalog, DiscoverInput, EXTERNAL_LINK, Limits, Place, ProjectSkills, Skipped, TRUST_LIMITS,
    discover, tree_hash, tree_listing,
};

use crate::config::{SkillsSettings, load_config};
use crate::models::CommandError;
use crate::run::{RunError, canonical_project, display_path, open_existing};
use crate::trust::{TRUSTED_TREES, TrustCheck, TrustSet, current_set, trust_check};

/// Каталог пользовательских скиллов в `ZA_HOME`.
pub const USER_SKILLS_DIR: &str = "skills";
/// Причина пропуска дерева проекта, изменённого после проверки доверия (D116).
pub const TREE_CHANGED: &str = "tree changed since trust";
/// Причина пропуска дерева проекта, которое набор доверия не читает: ссылка в пути или
/// больше 10 000 записей (`za_skills::TRUST_LIMITS`, D116).
pub const TREE_NOT_READ: &str = "not read: link in the path or more than 10000 entries";
/// Начало причины пропуска переполненного корня `user` или `extra` (`za_skills`).
const LIMIT_ENTRIES: &str = "limit reached: more than ";

/// Деревья скиллов доверенного проекта из сверенного обхода (D116): дерево набора доверия
/// обходится заново тем же `tree_listing`, и его хэш сверяется со значением `set`;
/// совпало — дерево входит в [`ProjectSkills`], нет — пропуск [`TREE_CHANGED`]. Дерево со
/// значением `external-link` не читается ([`TREE_NOT_READ`]). Блокирующая.
pub fn project_skills(
    project: &Path,
    set: &TrustSet,
) -> io::Result<(Option<ProjectSkills>, Vec<Skipped>)> {
    // Корни — как у набора доверия (`trust_set`): значения обхода те же.
    let roots = Roots::canonical(project, project)?;
    let mut trees = Vec::new();
    let mut skipped = Vec::new();
    for rel in TRUSTED_TREES {
        let Some(value) = set.files.get(*rel) else {
            continue;
        };
        let skip = |reason: &str| Skipped {
            path: (*rel).to_owned(),
            reason: reason.to_owned(),
        };
        if value == EXTERNAL_LINK {
            skipped.push(skip(TREE_NOT_READ));
            continue;
        }
        match tree_listing(&roots, rel, &TRUST_LIMITS)? {
            (Place::Found, Some(l)) if tree_hash(&l) == *value => trees.push(l),
            _ => {
                tracing::warn!(
                    tree = *rel,
                    "дерево скиллов изменилось после проверки доверия: скиллы этого дерева не читаются"
                );
                skipped.push(skip(TREE_CHANGED));
            }
        }
    }
    let project = (!trees.is_empty()).then_some(ProjectSkills { roots, trees });
    Ok((project, skipped))
}

/// Вход обнаружения: `user` — `ZA_HOME/skills`, поставка, `extra` по `[skills] dirs`,
/// предел записей корня — `[skills] max_entries`.
fn discover_input(
    home: &Path,
    s: &SkillsSettings,
    project: Option<ProjectSkills>,
) -> DiscoverInput {
    DiscoverInput {
        project,
        user_dir: Some(home.join(USER_SKILLS_DIR)),
        extra_dirs: s.dirs.clone(),
        bundled: za_skills::bundled(),
        limits: Limits {
            entries: s.max_entries,
            ..Limits::default()
        },
    }
}

/// Каталог скиллов (D114, D116): обнаружение в `spawn_blocking`; `project` — доверенный
/// проект и его доверенный набор (недоверенный проект — `None`: проектных скиллов нет,
/// SKL-002). Пропуски деревьев проекта ([`project_skills`]) — в `skipped` каталога.
pub async fn catalog(
    home: &Path,
    s: &SkillsSettings,
    project: Option<(&Path, &TrustSet)>,
) -> io::Result<Catalog> {
    let home = home.to_path_buf();
    let s = s.clone();
    let project = project.map(|(p, set)| (p.to_path_buf(), set.clone()));
    tokio::task::spawn_blocking(move || {
        let (project, skipped) = match &project {
            Some((p, set)) => project_skills(p, set)?,
            None => (None, Vec::new()),
        };
        let mut cat = discover(&discover_input(&home, &s, project));
        cat.skipped.extend(skipped);
        Ok(cat)
    })
    .await
    .map_err(|e| io::Error::other(e.to_string()))?
}

/// Строки о корнях `user` и `extra`, пропущенных пределом записей: какой корень и почему
/// (перенос задачи 12).
pub fn limit_lines(cat: &Catalog, s: &SkillsSettings) -> Vec<String> {
    cat.skipped
        .iter()
        .filter(|k| k.reason.starts_with(LIMIT_ENTRIES) && k.reason.ends_with(" entries"))
        .map(|k| {
            format!(
                "корень {} не прочитан: больше {} записей (предел [skills] max_entries = {}); поднимите предел, разнесите скиллы по нескольким [skills] dirs или уберите из каталога лишнее",
                k.path, s.max_entries, s.max_entries
            )
        })
        .collect()
}

/// Число скиллов источника.
pub fn count_of(cat: &Catalog, source: &str) -> usize {
    cat.skills.iter().filter(|s| s.source == source).count()
}

/// Сводка каталога: `user N, bundled N, extra N, project N`.
pub fn summary(cat: &Catalog) -> String {
    [
        skill_source::USER,
        skill_source::BUNDLED,
        skill_source::EXTRA,
        skill_source::PROJECT,
    ]
    .iter()
    .map(|s| format!("{s} {}", count_of(cat, s)))
    .collect::<Vec<_>>()
    .join(", ")
}

/// Ошибка команды `skills list`.
#[derive(Debug, thiserror::Error)]
pub enum SkillsCmdError {
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error("каталог скиллов: {0}")]
    Io(#[from] io::Error),
}

/// Доверие к проекту для `skills list`: журнала нет — проект ни разу не доверяли (без
/// отличий); журнал не создаётся.
async fn project_trust(home: &Path, project: &Path) -> Result<TrustCheck, RunError> {
    match open_existing(home, project) {
        Ok(store) => trust_check(&store, project).await,
        Err(RunError::NoJournal(_)) => Ok(TrustCheck {
            trusted: false,
            diff: None,
            set: current_set(project).await?,
            revoked: false,
            chain_broken: false,
        }),
        Err(e) => Err(e),
    }
}

/// `zero-agent skills list [--project P]` (D122): таблица `name  source  путь` с
/// предупреждениями скилла, затем пропущенные с причинами и корни, не прочитанные пределом
/// записей. Проектные скиллы — только у доверенного проекта; иначе строка «проект не
/// доверен» и отличие доверия (`TrustDiff`), если оно есть.
pub async fn cmd_skills_list(
    home: &Path,
    project: Option<&Path>,
) -> Result<ExitCode, SkillsCmdError> {
    let cfg = load_config(home).map_err(CommandError::from)?;
    let s = &cfg.skills;
    if !s.enabled {
        println!("скиллы выключены: [skills] enabled = false");
        return Ok(ExitCode::SUCCESS);
    }
    let mut notes = Vec::new();
    let checked = match project {
        Some(p) => {
            let p = canonical_project(p)?;
            let check = project_trust(home, &p).await?;
            Some((p, check))
        }
        None => None,
    };
    let trusted = checked.as_ref().filter(|(_, c)| c.trusted);
    if let Some((p, c)) = checked.as_ref().filter(|(_, c)| !c.trusted) {
        notes.push(format!(
            "проект не доверен: скиллы проекта не читаются (доверие: zero-agent trust --project \"{}\")",
            display_path(p)
        ));
        if c.chain_broken {
            notes.push("цепочка журнала аудита проекта не сходится (zero-agent audit --verify): прежнее доверие не действует".to_owned());
        }
        if let Some(d) = c.diff.as_ref().filter(|d| !d.is_empty()) {
            notes.push(format!(
                "доверенные файлы проекта изменились после доверия: {}",
                d.describe()
            ));
        }
    }
    let cat = catalog(home, s, trusted.map(|(p, c)| (p.as_path(), &c.set))).await?;
    print!("{}", render_list(&cat, s, &notes));
    Ok(ExitCode::SUCCESS)
}

/// Текст `skills list`: строки `name  source  путь` (по старшинству источника, затем
/// имени) с предупреждениями, затем заметки о проекте, пропущенные и корни, не прочитанные
/// пределом записей.
pub fn render_list(cat: &Catalog, s: &SkillsSettings, notes: &[String]) -> String {
    let mut out = String::new();
    for k in &cat.skills {
        out.push_str(&format!("{}  {}  {}", k.name, k.source, k.display));
        if !k.warnings.is_empty() {
            out.push_str(&format!("  предупреждения: {}", k.warnings.join("; ")));
        }
        out.push('\n');
    }
    if cat.skills.is_empty() {
        out.push_str("скиллов нет\n");
    }
    for n in notes {
        out.push_str(&format!("{n}\n"));
    }
    if !cat.skipped.is_empty() {
        out.push_str("пропущены:\n");
        for k in &cat.skipped {
            out.push_str(&format!("  {}: {}\n", k.path, k.reason));
        }
    }
    for l in limit_lines(cat, s) {
        out.push_str(&format!("предупреждение: {l}\n"));
    }
    out
}
