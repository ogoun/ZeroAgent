//! Доверие к проекту (SEC-030, R1-C1 D83; решение заказчика (б)): набор доверенных файлов
//! проекта с хэшами их содержимого.
//!
//! Набор пишется в запись аудита `trust.granted` в момент ответа на прерывание доверия
//! (`answer --interrupt trust-… --approve`, D91) или командой `zero-agent trust`
//! ([`cmd_trust`]) и сверяется с набором из payload прерывания. На старте `run` и при
//! resume набор пересчитывается и сравнивается с доверенным ([`trust_state`]): отличие —
//! проект не доверен, отличия ([`TrustDiff`]) печатаются. Доверенный проект читает
//! `AGENTS.md`, а без него `CLAUDE.md` (CAP-084), и `.zero-agent/config.toml` — только
//! через [`read_trusted`]: тем же разрешением пути, что набор, и со сверкой хэша
//! прочитанного с доверенным значением.
//!
//! Хэш описывает ровно то, что потом прочтёт рантайм (ревью задачи 4, I1, I2):
//! - обычный файл — BLAKE3 содержимого (потоково, блоками), значение — hex;
//! - файл-ссылка (symlink, junction), цель которой — обычный файл внутри проекта, —
//!   `link:` и BLAKE3 содержимого цели: подмена содержимого цели меняет набор, а замена
//!   файла ссылкой на такое же содержимое — тоже (другой домен хэша);
//! - ссылка наружу, на сетевой или device-путь, висячая ссылка, ссылка в промежуточном
//!   каталоге (`.claude` — junction и т. п.), имя, которое не проходит
//!   `za_core::paths::classify`, — значение [`EXTERNAL_LINK`]. Загрузчик такой файл не
//!   читает, как отсутствующий; само появление метки меняет набор.
//!
//! Обход не заходит в каталоги-ссылки и junction ни на каком уровне: у промежуточных
//! компонентов читаются только локальные метаданные самой записи (`symlink_metadata`),
//! без перехода по ссылке. Цель файла-ссылки разрешается `za_core::paths::resolve`:
//! цепочка ссылок проверяется лексически до любой канонизации, сетевая цель — отказ без
//! SMB-запроса (Ruling 7.4, C1 задачи 2). Файл открывается `open_checked` (без следования
//! ссылке и со сверкой дескриптора). Компонента нет или он не каталог (`ENOTDIR`) —
//! файла нет, а не ошибка.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use std::io::Read;
use std::process::ExitCode;

use serde::{Deserialize, Serialize};
use serde_json::json;
use za_core::Access;
use za_core::paths::{self, Roots};
use za_protocol::{audit_action, canonical_json};
use za_store::{AuditNew, ProjectStore, StoreError};

use crate::run::{RunError, canonical_project, display_path};

/// Доверенные файлы в корне проекта.
pub const TRUSTED_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".zero-agent/config.toml"];
/// Доверенные деревья (скиллы).
pub const TRUSTED_TREES: &[&str] = &[".zero-agent/skills", ".agents/skills", ".claude/skills"];
/// Значение набора у файла, который рантайм не читает: ссылка наружу, на сетевой или
/// device-путь, висячая ссылка, ссылка в пути, неразбираемое имя.
pub const EXTERNAL_LINK: &str = "external-link";
/// Префикс значения у ссылки на файл внутри проекта.
pub const LINK_PREFIX: &str = "link:";

/// Набор доверенных файлов: путь относительно проекта (через `/`) → значение (hex BLAKE3
/// содержимого, `link:<hex>` или [`EXTERNAL_LINK`]). У дерева один элемент — BLAKE3
/// канонического JSON отсортированного списка пар (путь в дереве, значение). Отсутствующие
/// файлы и пустые деревья в набор не входят.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustSet {
    pub files: BTreeMap<String, String>,
}

impl TrustSet {
    /// Хэш набора — BLAKE3 канонического JSON (поле `set_hash` payload прерывания `Trust`).
    pub fn hash(&self) -> String {
        let v = serde_json::json!({ "files": self.files });
        blake3::hash(canonical_json(&v).as_bytes())
            .to_hex()
            .to_string()
    }
}

/// Набор доверенных файлов проекта `project` сейчас. Блокирующий ввод-вывод: из
/// асинхронного кода — через `spawn_blocking`. Проект за сетевой ссылкой или на сетевом
/// диске — ошибка (`Roots::canonical`).
pub fn trust_set(project: &Path) -> io::Result<TrustSet> {
    // `ZA_HOME` нужен `Roots` только для защиты записи; набор только читает.
    let roots = Roots::canonical(project, project)?;
    let mut files = BTreeMap::new();
    for rel in TRUSTED_FILES {
        if let Some(v) = file_value(&roots, rel)? {
            files.insert((*rel).to_owned(), v);
        }
    }
    for rel in TRUSTED_TREES {
        match locate(&roots.project, rel)? {
            Place::Missing => {}
            Place::Link => {
                files.insert((*rel).to_owned(), EXTERNAL_LINK.to_owned());
            }
            Place::Found(dir, meta) => {
                if !meta.is_dir() {
                    continue;
                }
                let mut list = Vec::new();
                walk(&roots, &dir, rel, "", &mut list)?;
                if !list.is_empty() {
                    list.sort();
                    let text = canonical_json(&serde_json::json!(list));
                    files.insert(
                        (*rel).to_owned(),
                        blake3::hash(text.as_bytes()).to_hex().to_string(),
                    );
                }
            }
        }
    }
    Ok(TrustSet { files })
}

/// Отличие нынешнего набора от доверенного: пути изменённых, новых и удалённых файлов
/// (деревья — одним путём), по возрастанию.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustDiff {
    pub changed: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

impl TrustDiff {
    /// Отличие `now` от доверенного `granted`.
    pub fn between(granted: &TrustSet, now: &TrustSet) -> TrustDiff {
        let mut d = TrustDiff {
            changed: Vec::new(),
            added: Vec::new(),
            removed: Vec::new(),
        };
        for (path, v) in &now.files {
            match granted.files.get(path) {
                Some(old) if old == v => {}
                Some(_) => d.changed.push(path.clone()),
                None => d.added.push(path.clone()),
            }
        }
        for path in granted.files.keys() {
            if !now.files.contains_key(path) {
                d.removed.push(path.clone());
            }
        }
        d
    }

    /// Отличий нет.
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }

    /// Текст для stderr: «изменены: AGENTS.md; новые: …; удалены: …».
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        for (name, list) in [
            ("изменены", &self.changed),
            ("новые", &self.added),
            ("удалены", &self.removed),
        ] {
            if !list.is_empty() {
                parts.push(format!("{name}: {}", list.join(", ")));
            }
        }
        parts.join("; ")
    }
}

/// Итог проверки доверия: доверен ли проект, отличия от доверенного набора, набор сейчас
/// и признаки отзыва и разрыва цепочки аудита.
#[derive(Debug, Clone)]
pub struct TrustCheck {
    pub trusted: bool,
    pub diff: Option<TrustDiff>,
    pub set: TrustSet,
    /// Последнее решение — `trust.revoked` (доверие отозвано явно).
    pub revoked: bool,
    /// Цепочка аудита не сходится (`StoreError::Corrupt` при чтении): доверие не действует.
    pub chain_broken: bool,
}

/// Доверен ли проект сейчас (D83): последняя запись аудита доверия — `trust.granted`, и
/// её набор совпадает с нынешним. Иначе — не доверен; если доверие было дано, а набор
/// изменился, второе значение называет отличия. Последняя запись `trust.revoked` или
/// записей нет — не доверен без отличий.
pub async fn trust_state(
    store: &ProjectStore,
    project: &Path,
) -> Result<(bool, Option<TrustDiff>), RunError> {
    let c = trust_check(store, project).await?;
    Ok((c.trusted, c.diff))
}

/// [`trust_state`] с набором и признаками (набор считается один раз на команду). Разрыв
/// цепочки аудита — не ошибка команды, а «не доверен» с признаком: доверию из
/// испорченной цепочки верить нельзя.
pub async fn trust_check(store: &ProjectStore, project: &Path) -> Result<TrustCheck, RunError> {
    let mut c = TrustCheck {
        trusted: false,
        diff: None,
        set: current_set(project).await?,
        revoked: false,
        chain_broken: false,
    };
    let last = match store.last_audit("trust.").await {
        Ok(l) => l,
        Err(StoreError::Corrupt(e)) => {
            tracing::warn!(error = %e, "цепочка аудита не сходится: доверие не действует");
            c.chain_broken = true;
            return Ok(c);
        }
        Err(e) => return Err(e.into()),
    };
    let Some(last) = last else { return Ok(c) };
    if last.action != audit_action::TRUST_GRANTED {
        c.revoked = last.action == audit_action::TRUST_REVOKED;
        return Ok(c);
    }
    // Запись не того формата — как пустой доверенный набор (все файлы — новые).
    let granted: TrustSet = serde_json::from_value(last.payload).unwrap_or_default();
    if granted == c.set {
        c.trusted = true;
    } else {
        c.diff = Some(TrustDiff::between(&granted, &c.set));
    }
    Ok(c)
}

/// Набор доверенных файлов проекта сейчас (`spawn_blocking`: обход ФС).
pub async fn current_set(project: &Path) -> Result<TrustSet, RunError> {
    let dir = project.to_path_buf();
    let err = |source| RunError::TrustSet {
        path: project.to_path_buf(),
        source,
    };
    tokio::task::spawn_blocking(move || trust_set(&dir))
        .await
        .map_err(|e| err(io::Error::other(e.to_string())))?
        .map_err(err)
}

/// Предел чтения доверенного файла, байты: больше — файл не читается (`warn`).
pub const TRUSTED_READ_LIMIT: u64 = 4 << 20;

/// Текст доверенного файла `rel` (из [`TRUSTED_FILES`]) по доверенному набору `set`: нет
/// в наборе или значение [`EXTERNAL_LINK`] — `None`, как отсутствующий. Файл разрешается
/// и открывается так же, как при расчёте набора (без следования ссылке, со сверкой
/// дескриптора), а хэш прочитанного сверяется со значением набора: изменился после
/// проверки доверия — `None` с `warn`. Больше [`TRUSTED_READ_LIMIT`] — `None` с `warn`.
/// Не UTF-8 — с заменой символов. Блокирующая.
pub fn read_trusted(project: &Path, set: &TrustSet, rel: &str) -> io::Result<Option<String>> {
    let Some(value) = set.files.get(rel) else {
        return Ok(None);
    };
    if value == EXTERNAL_LINK {
        return Ok(None);
    }
    let expected = value.strip_prefix(LINK_PREFIX).unwrap_or(value);
    let roots = Roots::canonical(project, project)?;
    let Ok(lex) = paths::classify(&roots.project, rel) else {
        return Ok(None);
    };
    let Ok(resolved) = paths::resolve(&roots, &lex, Access::Read) else {
        return Ok(None);
    };
    if !resolved.inside {
        return Ok(None);
    }
    let file = match paths::open_checked(&resolved, false) {
        Ok(f) => f,
        Err(e) if is_missing(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.take(TRUSTED_READ_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > TRUSTED_READ_LIMIT {
        tracing::warn!(
            file = rel,
            limit = TRUSTED_READ_LIMIT,
            "доверенный файл больше предела и не читается"
        );
        return Ok(None);
    }
    if blake3::hash(&bytes).to_hex().as_str() != expected {
        tracing::warn!(
            file = rel,
            "доверенный файл изменился после проверки доверия и не читается"
        );
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Файлы инструкций проекта по порядку (CAP-084): `AGENTS.md`, без него — `CLAUDE.md`.
pub const INSTRUCTION_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Инструкции доверенного проекта (CAP-084, D83): первый из [`INSTRUCTION_FILES`], что
/// есть в наборе и читается; имя файла и текст. Блокирующая.
pub fn project_instructions(
    project: &Path,
    set: &TrustSet,
) -> io::Result<Option<(&'static str, String)>> {
    for name in INSTRUCTION_FILES {
        if let Some(text) = read_trusted(project, set, name)? {
            return Ok(Some((name, text)));
        }
    }
    Ok(None)
}

/// Ошибка команды `trust`.
#[derive(Debug, thiserror::Error)]
pub enum TrustCmdError {
    /// Журнал держит запущенный run (D91).
    #[error("журнал проекта занят запущенным run; повторите после его остановки")]
    Busy,
    #[error(transparent)]
    Run(RunError),
    #[error(transparent)]
    Store(StoreError),
}

impl From<StoreError> for TrustCmdError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Locked => Self::Busy,
            e => Self::Store(e),
        }
    }
}

impl From<RunError> for TrustCmdError {
    fn from(e: RunError) -> Self {
        match e {
            RunError::Store(s) => s.into(),
            e => Self::Run(e),
        }
    }
}

/// Actor записей аудита команды `trust`: решение пользователя (как у `answer`).
const TRUST_ACTOR: &str = "user";

/// `zero-agent trust --project P [--revoke]` (D83, D91): доверие проекту заранее — запись
/// `trust.granted` с набором доверенных файлов сейчас; `--revoke` — запись
/// `trust.revoked`. Журнал проекта создаётся, если run в проекте ещё не было. Уже
/// доверен с тем же набором (или уже не доверен при `--revoke`) — новой записи нет.
pub async fn cmd_trust(
    za_home: &Path,
    project: &Path,
    revoke: bool,
) -> Result<ExitCode, TrustCmdError> {
    let project = canonical_project(project)?;
    let dir = crate::home::journal_dir(za_home, &project).map_err(|source| RunError::Io {
        what: "каталог проекта",
        path: project.clone(),
        source,
    })?;
    let store = ProjectStore::open(&dir)?;
    let check = trust_check(&store, &project).await?;
    let shown = display_path(&project);
    if revoke {
        if !check.trusted && check.diff.is_none() && !check.chain_broken {
            eprintln!("zero-agent: проект {shown} и так не доверен");
            return Ok(ExitCode::SUCCESS);
        }
        store
            .append_audit(AuditNew {
                actor: TRUST_ACTOR.to_owned(),
                action: audit_action::TRUST_REVOKED.to_owned(),
                target: shown.clone(),
                payload: json!({}),
            })
            .await?;
        eprintln!("zero-agent: доверие к проекту {shown} отозвано");
        return Ok(ExitCode::SUCCESS);
    }
    if check.trusted {
        eprintln!("zero-agent: проект {shown} уже доверен, доверенные файлы не менялись");
        return Ok(ExitCode::SUCCESS);
    }
    let payload = serde_json::to_value(&check.set)
        .map_err(|e| StoreError::Serde(format!("набор доверенных файлов: {e}")))?;
    store
        .append_audit(AuditNew {
            actor: TRUST_ACTOR.to_owned(),
            action: audit_action::TRUST_GRANTED.to_owned(),
            target: shown.clone(),
            payload,
        })
        .await?;
    let files: Vec<&str> = check.set.files.keys().map(String::as_str).collect();
    eprintln!(
        "zero-agent: проект {shown} доверен; доверенные файлы: {}",
        if files.is_empty() {
            "нет".to_owned()
        } else {
            files.join(", ")
        }
    );
    Ok(ExitCode::SUCCESS)
}

/// Где лежит доверенный путь `rel`.
enum Place {
    /// Компонента нет или промежуточный компонент не каталог.
    Missing,
    /// Промежуточный компонент — ссылка или junction: дальше не идём.
    Link,
    /// Путь и локальные метаданные последнего компонента (без перехода по ссылке).
    Found(PathBuf, std::fs::Metadata),
}

/// Проходит `rel` по компонентам от корня проекта, читая только метаданные самих записей.
fn locate(project: &Path, rel: &str) -> io::Result<Place> {
    let parts: Vec<&str> = rel.split('/').collect();
    let mut p = project.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        p.push(part);
        let meta = match std::fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(e) if is_missing(&e) => return Ok(Place::Missing),
            Err(e) => return Err(e),
        };
        if i + 1 == parts.len() {
            return Ok(Place::Found(p, meta));
        }
        if meta.file_type().is_symlink() {
            return Ok(Place::Link);
        }
        if !meta.is_dir() {
            return Ok(Place::Missing);
        }
    }
    Ok(Place::Missing)
}

/// Значение доверенного файла `rel`; `None` — файла нет (или это каталог).
fn file_value(roots: &Roots, rel: &str) -> io::Result<Option<String>> {
    match locate(&roots.project, rel)? {
        Place::Missing => Ok(None),
        Place::Link => Ok(Some(EXTERNAL_LINK.to_owned())),
        Place::Found(_, meta) => entry_value(roots, rel, &meta),
    }
}

/// Значение записи с относительным путём `rel` и локальными метаданными `meta`.
fn entry_value(roots: &Roots, rel: &str, meta: &std::fs::Metadata) -> io::Result<Option<String>> {
    let link = meta.file_type().is_symlink();
    if !link && !meta.is_file() {
        return Ok(None);
    }
    let Ok(lex) = paths::classify(&roots.project, rel) else {
        return Ok(Some(EXTERNAL_LINK.to_owned()));
    };
    // Цепочка ссылок проверяется лексически до канонизации: сетевая цель — отказ без SMB.
    let Ok(resolved) = paths::resolve(roots, &lex, Access::Read) else {
        return Ok(Some(EXTERNAL_LINK.to_owned()));
    };
    if !resolved.inside {
        return Ok(Some(EXTERNAL_LINK.to_owned()));
    }
    let file = match paths::open_checked(&resolved, false) {
        Ok(f) => f,
        // Висячая ссылка — метка; исчезнувший обычный файл — нет файла.
        Err(e) if is_missing(&e) => {
            return Ok(link.then(|| EXTERNAL_LINK.to_owned()));
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Err(e),
        // Не обычный файл, ссылка, подмена между разрешением и открытием: не читается.
        Err(_) => {
            return Ok(link.then(|| EXTERNAL_LINK.to_owned()));
        }
    };
    let mut h = blake3::Hasher::new();
    h.update_reader(file)?;
    let hex = h.finalize().to_hex().to_string();
    Ok(Some(if link {
        format!("{LINK_PREFIX}{hex}")
    } else {
        hex
    }))
}

/// Пары (путь в дереве, значение) записей каталога `dir` (путь `rel` от проекта).
/// Каталоги-ссылки и junction не обходятся: у них значение [`EXTERNAL_LINK`].
fn walk(
    roots: &Roots,
    dir: &Path,
    rel: &str,
    prefix: &str,
    out: &mut Vec<(String, String)>,
) -> io::Result<()> {
    let items = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) if is_missing(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    for item in items {
        let item = item?;
        let name = item.file_name();
        let Some(name) = name.to_str().map(str::to_owned) else {
            // Имя не в UTF-8: рантайм его не прочтёт.
            out.push((
                join(prefix, &item.file_name().to_string_lossy()),
                EXTERNAL_LINK.to_owned(),
            ));
            continue;
        };
        let in_tree = join(prefix, &name);
        let path = item.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if is_missing(&e) => continue,
            Err(e) => return Err(e),
        };
        if meta.file_type().is_symlink() {
            // Ссылка на файл внутри проекта хэшируется по цели; на каталог — не обходится.
            let v = entry_value(roots, &format!("{rel}/{in_tree}"), &meta)?
                .unwrap_or_else(|| EXTERNAL_LINK.to_owned());
            out.push((in_tree, v));
        } else if meta.is_dir() {
            walk(roots, &path, rel, &in_tree, out)?;
        } else if let Some(v) = entry_value(roots, &format!("{rel}/{in_tree}"), &meta)? {
            out.push((in_tree, v));
        }
    }
    Ok(())
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

/// Компонента нет: `NotFound` или «не каталог» (`ENOTDIR`).
fn is_missing(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Свой каталог на процесс и вызов `.tmp/zero-agent-unit/<name>-<pid>-<n>`.
    fn dir(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.tmp/zero-agent-unit");
        crate::test_unique_dir::unique_dir_in(&root, name)
    }

    #[test]
    fn trust_set_hashes_trusted_files_and_trees() {
        let p = dir("trust-set");
        assert_eq!(trust_set(&p).unwrap(), TrustSet::default());
        std::fs::write(p.join("AGENTS.md"), "a").unwrap();
        std::fs::write(p.join("README.md"), "не входит").unwrap();
        std::fs::create_dir_all(p.join(".claude/skills/x")).unwrap();
        std::fs::write(p.join(".claude/skills/x/SKILL.md"), "s").unwrap();
        let first = trust_set(&p).unwrap();
        let keys: Vec<&str> = first.files.keys().map(String::as_str).collect();
        assert_eq!(keys, [".claude/skills", "AGENTS.md"]);
        assert_eq!(
            first.files["AGENTS.md"],
            blake3::hash(b"a").to_hex().to_string()
        );
        // Изменение файла дерева меняет хэш дерева.
        std::fs::write(p.join(".claude/skills/x/SKILL.md"), "s2").unwrap();
        let second = trust_set(&p).unwrap();
        assert_ne!(
            first.files[".claude/skills"],
            second.files[".claude/skills"]
        );
        assert_eq!(first.files["AGENTS.md"], second.files["AGENTS.md"]);
        assert_ne!(first.hash(), second.hash());
    }

    /// `ENOTDIR`: `.zero-agent` — файл, а не каталог; это «нет файла», не ошибка.
    #[test]
    fn file_in_place_of_directory_is_missing() {
        let p = dir("trust-enotdir");
        std::fs::write(p.join(".zero-agent"), "файл").unwrap();
        std::fs::write(p.join(".claude"), "файл").unwrap();
        assert_eq!(trust_set(&p).unwrap(), TrustSet::default());
    }

    /// Большой файл хэшируется потоково и совпадает с хэшем содержимого.
    #[test]
    fn large_file_is_hashed_by_content() {
        let p = dir("trust-large");
        let data = vec![7u8; 3 * 1024 * 1024 + 11];
        std::fs::write(p.join("CLAUDE.md"), &data).unwrap();
        let s = trust_set(&p).unwrap();
        assert_eq!(
            s.files["CLAUDE.md"],
            blake3::hash(&data).to_hex().to_string()
        );
    }
}
