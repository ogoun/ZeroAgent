//! Набор доверенных файлов и ссылки (ревью задачи 4 R1-C1, I1, I2; D83).
//!
//! Ссылка на файл внутри проекта хэшируется по содержимому цели; ссылка наружу, на
//! сетевой путь и каталог-ссылка (junction) — метка `external-link`, обход в них не
//! заходит и к цели не обращается. Помощники ссылок и пропуска — как в
//! `za-core/tests/paths.rs`: на Windows ссылка на файл требует прав, без них проверка
//! пропускается с сообщением, а `ZA_REQUIRE_LINK_TESTS=1` превращает пропуск в падение.
//!
//! Доверие в `run` (R1-C1 задача 13, D83): недоверенный проект не читает `AGENTS.md`,
//! `CLAUDE.md` и `.zero-agent/config.toml`; `zero-agent trust` даёт доверие заранее;
//! правка доверенного файла снимает доверие с перечнем изменений; первая команда в
//! недоверенном проекте — прерывание `trust-…`, ответ пишет `trust.granted` в аудит.

use std::path::{Path, PathBuf};

use axum::response::Response;
use serde_json::Value;
use zero_agent::trust::{EXTERNAL_LINK, LINK_PREFIX, TrustDiff, TrustSet, trust_set};

mod support;
use support::fake_llama::{Fake, call_reply, llama_config, text_reply};
use support::process::{Env, env_in, interrupt_id, run_id, unique_dir};

/// Каталог проекта и внешний каталог теста.
fn fixture(name: &str) -> (PathBuf, PathBuf) {
    let root = unique_dir("zero-agent-trust", name);
    let project = root.join("project");
    let outside = root.join("outside");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    (project, outside)
}

fn hex(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// Ссылка на каталог: junction на Windows (`mklink /J` прав не требует), символическая
/// ссылка на Unix.
fn dir_link(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
        // `mklink` понимает только `\`: путь собирается заново по компонентам.
        let native = |p: &Path| p.components().collect::<PathBuf>();
        let out = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(native(link))
            .arg(native(target))
            .output()
            .expect("cmd не запустился");
        assert!(
            out.status.success(),
            "mklink /J: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// Ссылка на файл. На Windows требует прав администратора или режима разработчика:
/// без них — `None`, и тест явно пропускает проверку с сообщением.
#[cfg(unix)]
fn file_link(link: &Path, target: &Path) -> Option<()> {
    std::os::unix::fs::symlink(target, link).unwrap();
    Some(())
}

/// См. Unix-вариант выше.
#[cfg(windows)]
fn file_link(link: &Path, target: &Path) -> Option<()> {
    match std::os::windows::fs::symlink_file(target, link) {
        Ok(()) => Some(()),
        Err(e) => {
            skip(&format!(
                "ссылка на файл не создана ({e}); нужен режим разработчика или права администратора"
            ));
            None
        }
    }
}

/// Символическая ссылка на каталог (на Windows нужен режим разработчика или права).
#[cfg(windows)]
fn dir_symlink(link: &Path, target: &Path) -> Option<()> {
    match std::os::windows::fs::symlink_dir(target, link) {
        Ok(()) => Some(()),
        Err(e) => {
            skip(&format!(
                "символическая ссылка не создана ({e}); нужен режим разработчика или права администратора"
            ));
            None
        }
    }
}

/// Пропуск проверки с сообщением; `ZA_REQUIRE_LINK_TESTS=1` (CI) — пропуск — падение.
/// Пропуски бывают только на Windows (на Unix ссылки создаются без прав).
#[cfg(windows)]
fn skip(why: &str) {
    if std::env::var("ZA_REQUIRE_LINK_TESTS").as_deref() == Ok("1") {
        panic!("ZA_REQUIRE_LINK_TESTS=1, а проверка пропущена: {why}");
    }
    eprintln!("ПРОПУСК: {why}");
}

/// I1: `AGENTS.md` — ссылка на файл внутри проекта. В наборе — `link:` и хэш содержимого
/// цели; подмена содержимого цели меняет набор (нужно новое доверие).
#[test]
fn link_inside_is_hashed_by_target_content() {
    let (p, _) = fixture("link-inside");
    std::fs::create_dir_all(p.join("docs")).unwrap();
    std::fs::write(p.join("docs/rules.md"), "версия 1").unwrap();
    if file_link(&p.join("AGENTS.md"), &p.join("docs/rules.md")).is_none() {
        return;
    }
    let first = trust_set(&p).unwrap();
    assert_eq!(
        first.files["AGENTS.md"],
        format!("{LINK_PREFIX}{}", hex("версия 1".as_bytes()))
    );
    std::fs::write(p.join("docs/rules.md"), "версия 2").unwrap();
    let second = trust_set(&p).unwrap();
    assert_ne!(first, second, "подмена цели не изменила набор");
    assert_eq!(
        second.files["AGENTS.md"],
        format!("{LINK_PREFIX}{}", hex("версия 2".as_bytes()))
    );
    // Тот же текст обычным файлом — другое значение (домен хэша).
    std::fs::remove_file(p.join("AGENTS.md")).unwrap();
    std::fs::write(p.join("AGENTS.md"), "версия 2").unwrap();
    assert_ne!(trust_set(&p).unwrap(), second);
}

/// I1: ссылка на файл вне проекта и висячая ссылка — метка, содержимое цели не входит.
#[test]
fn link_outside_or_dangling_is_marked() {
    let (p, out) = fixture("link-outside");
    std::fs::write(out.join("evil.md"), "чужое").unwrap();
    if file_link(&p.join("CLAUDE.md"), &out.join("evil.md")).is_none() {
        return;
    }
    if file_link(&p.join("AGENTS.md"), &p.join("нет-такого.md")).is_none() {
        return;
    }
    let first = trust_set(&p).unwrap();
    assert_eq!(first.files["CLAUDE.md"], EXTERNAL_LINK);
    assert_eq!(first.files["AGENTS.md"], EXTERNAL_LINK);
    std::fs::write(out.join("evil.md"), "другое чужое").unwrap();
    assert_eq!(trust_set(&p).unwrap(), first);
}

/// I2: `.claude` — junction (ссылка) на внешний каталог со скиллами: обход в него не
/// заходит, дерево — метка; изменение внешних файлов набор не меняет. То же для
/// `.zero-agent` с `config.toml` и для каталога-ссылки внутри дерева.
#[test]
fn directory_links_are_not_walked() {
    let (p, out) = fixture("dir-links");
    std::fs::create_dir_all(out.join("skills/x")).unwrap();
    std::fs::write(out.join("skills/x/SKILL.md"), "s").unwrap();
    std::fs::write(out.join("config.toml"), "[policy]").unwrap();
    dir_link(&p.join(".claude"), &out);
    dir_link(&p.join(".zero-agent"), &out);
    std::fs::create_dir_all(p.join(".agents/skills/own")).unwrap();
    std::fs::write(p.join(".agents/skills/own/SKILL.md"), "своё").unwrap();
    dir_link(&p.join(".agents/skills/foreign"), &out.join("skills"));
    let first = trust_set(&p).unwrap();
    assert_eq!(first.files[".claude/skills"], EXTERNAL_LINK);
    assert_eq!(first.files[".zero-agent/config.toml"], EXTERNAL_LINK);
    assert_eq!(first.files[".zero-agent/skills"], EXTERNAL_LINK);
    let own_tree = first.files[".agents/skills"].clone();
    std::fs::write(out.join("skills/x/SKILL.md"), "s2").unwrap();
    std::fs::write(out.join("config.toml"), "[policy] changed").unwrap();
    let second = trust_set(&p).unwrap();
    assert_eq!(second, first, "внешние файлы попали в набор");
    // Своё содержимое дерева по-прежнему учитывается.
    std::fs::write(p.join(".agents/skills/own/SKILL.md"), "своё 2").unwrap();
    assert_ne!(trust_set(&p).unwrap().files[".agents/skills"], own_tree);
}

/// I2, Ruling 7.4: `.claude` и `AGENTS.md` — ссылки на `\\host\share`. Набор строится без
/// обращения к цели: число вызовов `canonicalize` то же, что у проекта без ссылок (только
/// корень проекта), значения — метка.
#[cfg(windows)]
#[test]
fn unc_links_are_not_touched() {
    use za_core::paths::canonicalize_calls;
    let (p, _) = fixture("unc-links");
    let before = canonicalize_calls();
    assert!(trust_set(&p).unwrap().files.is_empty());
    let baseline = canonicalize_calls() - before;
    if dir_symlink(&p.join(".claude"), Path::new(r"\\192.0.2.1\share")).is_none() {
        return;
    }
    if file_link(
        &p.join("AGENTS.md"),
        Path::new(r"\\192.0.2.1\share\AGENTS.md"),
    )
    .is_none()
    {
        return;
    }
    let before = canonicalize_calls();
    let s = trust_set(&p).unwrap();
    assert_eq!(
        canonicalize_calls() - before,
        baseline,
        "цель ссылки канонизирована"
    );
    assert_eq!(s.files[".claude/skills"], EXTERNAL_LINK);
    assert_eq!(s.files["AGENTS.md"], EXTERNAL_LINK);
}

// ---- Доверие в run (задача 13) ----

/// Окно `/props` сценариев доверия.
const N_CTX: u32 = 32768;

/// Метка текста `AGENTS.md` / `CLAUDE.md`: по ней видно, попал ли файл в system.
const MARK: &str = "PROJECT-INSTRUCTIONS-MARK";

fn paris(_: &[Value]) -> Response {
    text_reply(&["Париж"])
}

/// Сервер и окружение теста с конфигурацией модели.
fn run_env(name: &str, handler: support::fake_llama::ChatHandler) -> (Fake, Env) {
    let srv = Fake::start(N_CTX, true, handler);
    let e = env_in("zero-agent-trust-run", name, &llama_config(&srv.base));
    (srv, e)
}

/// Текст system-сообщения тела запроса.
fn system_of(body: &Value) -> String {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "system")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// Правила политики снимка run.
fn rules_of(snap: &Value) -> usize {
    snap["policy"]["rules"].as_array().map_or(0, Vec::len)
}

/// Проектная конфигурация с правилом `deny` для `bash`.
const DENY_BASH: &str = "[[policy.rules]]\naction = \"deny\"\ntool = \"bash\"\n";

#[test]
fn untrusted_project_skips_agents_md_and_config() {
    let (srv, e) = run_env("untrusted", paris);
    std::fs::write(e.project.join("AGENTS.md"), MARK).unwrap();
    std::fs::create_dir_all(e.project.join(".zero-agent")).unwrap();
    std::fs::write(e.project.join(".zero-agent/config.toml"), DENY_BASH).unwrap();
    let o = e.run_cmd(&["Столица Франции?"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(o.stderr.contains("проект не доверен"), "{o:?}");
    let body = &srv.bodies()[0];
    assert!(
        !system_of(body).contains(MARK),
        "AGENTS.md недоверенного проекта прочитан"
    );
    let snap = e.snapshot(run_id(&o));
    assert_eq!(snap["trusted"], false, "{snap}");
    assert_eq!(
        rules_of(&snap),
        0,
        "проектное правило недоверенного проекта действует"
    );
    assert!(
        snap["taint"]
            .as_array()
            .unwrap()
            .contains(&Value::from("untrusted_project"))
    );
    // Тот же проект после доверия: AGENTS.md в system, правило проекта в снимке.
    e.trust();
    let o = e.run_cmd(&["Столица Франции?"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(!o.stderr.contains("проект не доверен"), "{o:?}");
    assert!(system_of(&srv.bodies()[1]).contains(MARK));
    let snap = e.snapshot(run_id(&o));
    assert_eq!(snap["trusted"], true, "{snap}");
    assert_eq!(rules_of(&snap), 1, "{snap}");
    assert_eq!(snap["policy"]["rules"][0][0], "project", "{snap}");
    // policy.loaded — на каждую смену хэша политики, не на каждый run.
    let loaded = e.last_audit("policy.").unwrap();
    assert_eq!(loaded.payload["project_rules"], 1, "{loaded:?}");
    let o = e.run_cmd(&["Ещё раз"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert_eq!(e.last_audit("policy.").unwrap().seq, loaded.seq);
}

#[test]
fn trust_then_change_detected() {
    let (srv, e) = run_env("change", paris);
    std::fs::write(e.project.join("AGENTS.md"), format!("{MARK} v1")).unwrap();
    let t = e.trust();
    assert!(t.stderr.contains("AGENTS.md"), "{t:?}");
    let o = e.run_cmd(&["Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(system_of(&srv.bodies()[0]).contains(&format!("{MARK} v1")));
    // Правка доверенного файла снимает доверие; stderr называет файл.
    std::fs::write(e.project.join("AGENTS.md"), format!("{MARK} v2")).unwrap();
    let o = e.run_cmd(&["Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(o.stderr.contains("проект не доверен"), "{o:?}");
    assert!(o.stderr.contains("изменены: AGENTS.md"), "{o:?}");
    assert!(!system_of(&srv.bodies()[1]).contains(MARK));
    assert_eq!(e.snapshot(run_id(&o))["trusted"], false);
    // Отзыв доверия: следующий run не доверен без перечня изменений.
    e.trust();
    let r = e.run(&["trust", "--project", e.project_arg(), "--revoke"]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(e.last_audit("trust.").unwrap().action, "trust.revoked");
    let o = e.run_cmd(&["Вопрос"]);
    assert!(o.stderr.contains("проект не доверен"), "{o:?}");
    assert!(!o.stderr.contains("изменены"), "{o:?}");
}

/// Запрос 1 — вызов `bash` (`echo trusted-ok`), далее — «готово».
fn bash_then_done(seen: &[Value]) -> Response {
    if seen.len() > 1 {
        return text_reply(&["готово"]);
    }
    call_reply("call_1", "bash", "{\"cmd\":\"echo trusted-ok\"}")
}

#[test]
fn trust_interrupt_answer_records_audit() {
    let (srv, e) = run_env("interrupt", bash_then_done);
    std::fs::write(e.project.join("AGENTS.md"), MARK).unwrap();
    let o = e.run_cmd(&["--mode", "auto", "Запусти echo"]);
    assert_eq!(o.code(), Some(3), "{o:?}");
    assert!(o.stderr.contains("needs_trust"), "{o:?}");
    let int = interrupt_id(&o);
    assert!(int.starts_with("trust-"), "{int}: {o:?}");
    assert!(o.stderr.contains("--approve | --deny"), "{o:?}");
    let a = e.run(&[
        "answer",
        "--project",
        e.project_arg(),
        "--interrupt",
        &int,
        "--approve",
    ]);
    assert_eq!(a.code(), Some(0), "{a:?}");
    let audit = e.last_audit("trust.").unwrap();
    assert_eq!(audit.action, "trust.granted", "{audit:?}");
    let files: TrustSet = serde_json::from_value(audit.payload.clone()).unwrap();
    assert!(files.files.contains_key("AGENTS.md"), "{audit:?}");
    // Продолжение: доверие снимает метку, команда исполняется, run завершается.
    let id = run_id(&o).to_string();
    let r = e.run_cmd(&["--resume", &id]);
    assert_eq!(r.code(), Some(0), "{r:?}");
    assert_eq!(r.stdout, "готово\n", "{r:?}");
    let bodies = srv.bodies();
    let result = serde_json::to_string(&bodies[1]["messages"]).unwrap();
    assert!(result.contains("trusted-ok"), "{result}");
    let snap = e.snapshot(run_id(&o));
    assert_eq!(snap["trusted"], true, "{snap}");
    // Якорь аудита в снимке — голова цепочки на момент продолжения.
    assert!(
        snap["audit_head"]["seq"].as_u64().unwrap() >= audit.seq,
        "{snap}"
    );
}

#[test]
fn claude_md_is_fallback() {
    let (srv, e) = run_env("claude-md", paris);
    std::fs::write(e.project.join("CLAUDE.md"), format!("{MARK} claude")).unwrap();
    e.trust();
    let o = e.run_cmd(&["Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    assert!(system_of(&srv.bodies()[0]).contains(&format!("{MARK} claude")));
    // Есть оба — читается AGENTS.md.
    std::fs::write(e.project.join("AGENTS.md"), format!("{MARK} agents")).unwrap();
    e.trust();
    let o = e.run_cmd(&["Вопрос"]);
    assert_eq!(o.code(), Some(0), "{o:?}");
    let system = system_of(&srv.bodies()[1]);
    assert!(system.contains(&format!("{MARK} agents")), "{system}");
    assert!(!system.contains(&format!("{MARK} claude")), "{system}");
}

#[test]
fn agents_md_flag_gives_hint() {
    let e = env_in("zero-agent-trust-run", "agents-md-flag", "");
    let o = e.run_cmd(&["--agents-md", "Вопрос"]);
    assert_eq!(o.code(), Some(2), "{o:?}");
    assert!(o.stderr.contains("zero-agent trust"), "{o:?}");
}

/// Изменённый журнал аудита: запись, сохранённая в снимке run, исчезла — resume
/// отказывает с подсказкой `audit --verify` (D91, m1 ревью задачи 4).
#[test]
fn resume_detects_removed_audit_tail() {
    let (_srv, e) = run_env("anchor", bash_then_done);
    e.trust();
    let o = e.run_cmd(&["--mode", "ask", "Запусти echo"]);
    assert_eq!(o.code(), Some(3), "{o:?}");
    let head = e.snapshot(run_id(&o))["audit_head"]["seq"]
        .as_u64()
        .unwrap();
    // Удаление хвоста цепочки мимо хранилища (как в тесте `audit --verify`).
    let db = e.journal_dir().join("journal.db");
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute("DELETE FROM audit WHERE seq >= ?1", [head as i64])
        .unwrap();
    drop(c);
    let r = e.run_cmd(&["--resume", &run_id(&o).to_string()]);
    assert_eq!(r.code(), Some(1), "{r:?}");
    assert!(r.stderr.contains("audit --verify"), "{r:?}");
}

#[test]
fn trust_diff_names_changed_added_removed() {
    let set = |pairs: &[(&str, &str)]| TrustSet {
        files: pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    };
    let d = TrustDiff::between(
        &set(&[("AGENTS.md", "1"), ("CLAUDE.md", "2")]),
        &set(&[("AGENTS.md", "9"), (".claude/skills", "3")]),
    );
    assert_eq!(d.changed, ["AGENTS.md"]);
    assert_eq!(d.added, [".claude/skills"]);
    assert_eq!(d.removed, ["CLAUDE.md"]);
    assert_eq!(
        d.describe(),
        "изменены: AGENTS.md; новые: .claude/skills; удалены: CLAUDE.md"
    );
    assert!(TrustDiff::between(&set(&[]), &set(&[])).is_empty());
}
