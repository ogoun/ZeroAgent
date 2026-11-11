//! Движок политики (R1-C1 D77–D83, задача 11): таблица решений, правила, гранты,
//! защищённые пути, `bypass`, доверие и метка `tainted`.
//!
//! Таблица строится без ввода-вывода: разрешённые пути собираются лексически
//! ([`classify`]) с признаками [`within`] и [`protected_place`] — так, как их дал бы
//! `paths::resolve` для существующих канонических путей. Обходы шаблона через `..` и
//! ссылку проверяются настоящим `resolve` во временном каталоге теста.

mod support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use za_agent::{
    Decision, Grant, GrantConfig, GrantMode, GrantSource, PolicyConfig, PolicyCx, PolicyError,
    PolicySnapshot, Rule, RuleAction, RuleSource, covers, decide, grants_text, parse_grant_flag,
    runtime_texts, subject_paths,
};
use za_core::paths::{ResolvedPath, Roots, classify, protected_place, resolve, within};
use za_core::shell::{self, Dialect};
use za_core::{Access, Approval, RawPath, RiskClass, Subjects, ToolKind, ToolSchema, ToolSpec};
use za_protocol::{EffectClass, RunMode, RunRole, Tier, approval_reason as ar, deny_reason};

// ---- Помощники ----

/// Абсолютный путь текущей ОС из записи через `/`: на Windows — на диске `C:`.
fn abs(s: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!("C:{}", s.replace('/', "\\")))
    } else {
        PathBuf::from(s)
    }
}

/// Абсолютный шаблон правила `path` текущей ОС (через `/`).
fn pat(s: &str) -> String {
    if cfg!(windows) {
        format!("C:{s}")
    } else {
        s.to_owned()
    }
}

fn roots() -> Roots {
    Roots {
        project: abs("/proj"),
        za_home: abs("/home/u/.zero-agent-home"),
    }
}

fn spec(name: &str, kind: ToolKind, risk: RiskClass) -> ToolSpec {
    ToolSpec {
        schema: ToolSchema {
            name: name.to_owned(),
            description: String::new(),
            parameters: json!({"type": "object"}),
        },
        class: match kind {
            ToolKind::Read => EffectClass::ReadOnly,
            ToolKind::Edit => EffectClass::Idempotent,
            ToolKind::Exec => EffectClass::NonIdempotent,
            ToolKind::Control => EffectClass::Pure,
        },
        kind,
        approval: Approval::Mode,
        risk,
        roles: vec![RunRole::Top],
        min_tier: Tier::Small,
        timeout: Duration::from_secs(30),
        output_limit: None,
        tainted: false,
    }
}

fn read_spec() -> ToolSpec {
    spec("read", ToolKind::Read, RiskClass::Read)
}
fn edit_spec() -> ToolSpec {
    spec("edit", ToolKind::Edit, RiskClass::LocalWrite)
}
fn bash_spec() -> ToolSpec {
    spec("bash", ToolKind::Exec, RiskClass::Exec)
}

/// Разрешённый путь без ввода-вывода: лексический разбор и признаки по корням.
fn rp(r: &Roots, arg: &str, access: Access) -> ResolvedPath {
    let path = classify(&r.project, arg).unwrap().as_path().to_path_buf();
    ResolvedPath {
        inside: within(&r.project, &path),
        protected: access == Access::Write && protected_place(r, &path),
        path,
    }
}

/// Предметы нативного инструмента с одним путём и их разрешение.
fn one(r: &Roots, arg: &str, access: Access) -> (Subjects, Vec<ResolvedPath>) {
    let s = Subjects {
        paths: vec![RawPath {
            arg: arg.to_owned(),
            access,
        }],
        ..Subjects::default()
    };
    let p = vec![rp(r, arg, access)];
    (s, p)
}

/// Предметы `bash` так, как их строит `za-tools` (D77, D81): сегменты, метки
/// необратимости, слова-пути, сеть; пути разрешены лексически.
fn bash(r: &Roots, cmd: &str) -> (Subjects, Vec<ResolvedPath>) {
    let d = Dialect::Bash;
    let root = r.project.clone();
    let segs = shell::segments(cmd, d);
    let outside = |w: &str| {
        w.starts_with('~') || classify(&root, w).map_or(true, |l| !within(&root, l.as_path()))
    };
    let mut irreversible = Vec::new();
    for s in &segs {
        if let Some(l) = shell::irreversible(s, d, &outside)
            && !irreversible.contains(&l)
        {
            irreversible.push(l);
        }
    }
    let mut paths: Vec<RawPath> = Vec::new();
    for s in &segs {
        for w in shell::path_words(s, d) {
            if !paths.iter().any(|p| p.arg == w) {
                paths.push(RawPath {
                    arg: w,
                    access: Access::Read,
                });
            }
        }
    }
    let s = Subjects {
        network: segs.iter().any(|s| shell::is_network(s, d)),
        command: Some(cmd.to_owned()),
        paths,
        irreversible,
        segments: segs,
        cwd: None,
    };
    // Слово, которое `classify` отвергает (на Windows `/etc/hosts` — `DriveRelative`),
    // не разрешается: список короче `subject_paths`, граница неизвестна — Ask.
    let resolved = subject_paths(&s)
        .iter()
        .filter(|p| classify(&r.project, &p.arg).is_ok())
        .map(|p| rp(r, &p.arg, p.access))
        .collect();
    (s, resolved)
}

fn rule(action: RuleAction) -> Rule {
    Rule {
        action,
        tool: None,
        path: None,
        command: None,
    }
}
fn tool_rule(action: RuleAction, tool: &str) -> Rule {
    Rule {
        tool: Some(tool.to_owned()),
        ..rule(action)
    }
}
fn path_rule(action: RuleAction, path: &str) -> Rule {
    Rule {
        path: Some(path.to_owned()),
        ..rule(action)
    }
}
fn cmd_rule(action: RuleAction, command: &str) -> Rule {
    Rule {
        command: Some(command.to_owned()),
        ..rule(action)
    }
}

fn cfg_with(rules: Vec<Rule>) -> PolicyConfig {
    PolicyConfig {
        allow_bypass: true,
        rules,
        grants: Vec::new(),
    }
}

fn snap(global: Vec<Rule>) -> PolicySnapshot {
    PolicySnapshot::build(&cfg_with(global), None).unwrap()
}

fn grant(path: &str, mode: GrantMode) -> Grant {
    Grant {
        path: abs(path),
        mode,
        source: GrantSource::Cli,
        reason: String::new(),
    }
}
fn g_ro(path: &str) -> Grant {
    grant(path, GrantMode::Ro)
}

/// Контекст решения: проект доверен, меток нет, грантов нет.
struct Cx {
    policy: PolicySnapshot,
    roots: Roots,
    mode: RunMode,
    trusted: bool,
    trust_refused: bool,
    taint: BTreeSet<String>,
    grants: Vec<Grant>,
}

impl Cx {
    fn new(mode: RunMode) -> Self {
        Self {
            policy: snap(Vec::new()),
            roots: roots(),
            mode,
            trusted: true,
            trust_refused: false,
            taint: BTreeSet::new(),
            grants: Vec::new(),
        }
    }
    fn policy(mut self, p: PolicySnapshot) -> Self {
        self.policy = p;
        self
    }
    fn grants(mut self, g: Vec<Grant>) -> Self {
        self.grants = g;
        self
    }
    fn taint(mut self, origin: &str) -> Self {
        self.taint.insert(origin.to_owned());
        self
    }
    fn untrusted(mut self, refused: bool) -> Self {
        self.trusted = false;
        self.trust_refused = refused;
        self
    }
    fn decide(&self, spec: &ToolSpec, sp: &(Subjects, Vec<ResolvedPath>)) -> Decision {
        let cx = PolicyCx {
            policy: &self.policy,
            mode: self.mode,
            roots: &self.roots,
            trusted: self.trusted,
            trust_refused: self.trust_refused,
            taint: &self.taint,
            grants: &self.grants,
        };
        let d = decide(&cx, spec, &sp.0, &sp.1);
        // Чистая функция (D30, D92): повтор даёт то же решение.
        assert_eq!(d, decide(&cx, spec, &sp.0, &sp.1));
        d
    }
}

fn ask(reasons: &[&'static str]) -> Decision {
    Decision::Ask {
        reasons: reasons.to_vec(),
    }
}

// ---- Таблица брифа ----

#[test]
fn decide_table() {
    let r = roots();
    let read_out = || one(&r, &pat("/data/x"), Access::Read);
    let edit_out = || one(&r, &pat("/data/x"), Access::Write);

    // 1. Запись в защищённый путь в bypass → Deny; `.zero-agent/work/a` не защищён.
    let by = Cx::new(RunMode::Bypass);
    for arg in [
        ".zero-agent/config.toml".to_owned(),
        r.za_home.join("x").to_string_lossy().into_owned(),
        ".git/HEAD".to_owned(),
    ] {
        assert_eq!(
            by.decide(&edit_spec(), &one(&r, &arg, Access::Write)),
            Decision::Deny(deny_reason::PROTECTED_PATH),
            "{arg}"
        );
    }
    assert_eq!(
        by.decide(&edit_spec(), &one(&r, ".zero-agent/work/a", Access::Write)),
        Decision::Allow
    );

    // 2. deny проекта сильнее глобального allow.
    let global = cfg_with(vec![tool_rule(RuleAction::Allow, "edit")]);
    let project = PolicyConfig {
        rules: vec![tool_rule(RuleAction::Deny, "edit")],
        ..PolicyConfig::default()
    };
    let p = PolicySnapshot::build(&global, Some(&project)).unwrap();
    assert_eq!(
        Cx::new(RunMode::Auto)
            .policy(p)
            .decide(&edit_spec(), &one(&r, "a.txt", Access::Write)),
        Decision::Deny(deny_reason::POLICY_RULE)
    );

    // 3. read-only + edit.
    assert_eq!(
        Cx::new(RunMode::ReadOnly).decide(&edit_spec(), &one(&r, "a.txt", Access::Write)),
        Decision::Deny("read-only mode")
    );

    // 4. Необратимое — Ask в auto и в bypass.
    for mode in [RunMode::Auto, RunMode::Bypass] {
        assert_eq!(
            Cx::new(mode).decide(&bash_spec(), &bash(&r, "rm -rf x")),
            ask(&[ar::IRREVERSIBLE]),
            "{mode:?}"
        );
    }

    // 5. bypass + чтение или правка снаружи без гранта → outside_project (заказчик (а)).
    assert_eq!(
        by.decide(&read_spec(), &read_out()),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    assert_eq!(
        by.decide(&edit_spec(), &edit_out()),
        ask(&[ar::OUTSIDE_PROJECT])
    );

    // 6. auto + правка снаружи.
    let auto = Cx::new(RunMode::Auto);
    assert_eq!(
        auto.decide(&edit_spec(), &edit_out()),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    assert_eq!(
        Cx::new(RunMode::Auto)
            .grants(vec![grant("/data", GrantMode::Rw)])
            .decide(&edit_spec(), &edit_out()),
        Decision::Allow
    );
    assert_eq!(
        Cx::new(RunMode::Auto)
            .grants(vec![g_ro("/data")])
            .decide(&edit_spec(), &edit_out()),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    assert_eq!(
        Cx::new(RunMode::Auto)
            .policy(snap(vec![path_rule(RuleAction::Allow, &pat("/data/**"))]))
            .decide(&edit_spec(), &edit_out()),
        Decision::Allow
    );
    let project_allow = PolicyConfig {
        rules: vec![path_rule(RuleAction::Allow, &pat("/data/**"))],
        ..PolicyConfig::default()
    };
    assert_eq!(
        Cx::new(RunMode::Auto)
            .policy(PolicySnapshot::build(&cfg_with(Vec::new()), Some(&project_allow)).unwrap())
            .decide(&edit_spec(), &edit_out()),
        ask(&[ar::OUTSIDE_PROJECT])
    );

    // 7. auto + чтение снаружи: без гранта — Ask, с грантом ro — Allow.
    assert_eq!(
        auto.decide(&read_spec(), &read_out()),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    assert_eq!(
        Cx::new(RunMode::Auto)
            .grants(vec![g_ro("/data")])
            .decide(&read_spec(), &read_out()),
        Decision::Allow
    );

    // 8. Бриф: при метке tainted Exec спрашивает. Решение заказчика 2026-11-11 это
    // отменило: метка вопроса не даёт ни с правилом allow, ни без него. allow снимает
    // вопрос режима только при совпадении каждого сегмента без подстановок (Ruling C1).
    let allowed = Cx::new(RunMode::Auto)
        .taint("outside_project")
        .policy(snap(vec![cmd_rule(RuleAction::Allow, "cargo test")]));
    for cmd in ["cargo test", "cargo test && curl x", "cargo test $(curl x)"] {
        assert_eq!(
            allowed.decide(&bash_spec(), &bash(&r, cmd)),
            Decision::Allow,
            "{cmd}"
        );
    }
    let ask_mode =
        Cx::new(RunMode::Ask).policy(snap(vec![cmd_rule(RuleAction::Allow, "cargo test")]));
    assert_eq!(
        ask_mode.decide(&bash_spec(), &bash(&r, "cargo test")),
        Decision::Allow
    );
    for cmd in ["cargo test && curl x", "cargo test $(curl x)"] {
        assert_eq!(
            ask_mode.decide(&bash_spec(), &bash(&r, cmd)),
            ask(&[ar::MODE]),
            "{cmd}"
        );
    }

    // 9. Недоверенный проект: Exec — Trust, после отказа — Deny; правка — по режиму.
    assert_eq!(
        Cx::new(RunMode::Auto)
            .untrusted(false)
            .decide(&bash_spec(), &bash(&r, "cargo test")),
        Decision::Trust
    );
    assert_eq!(
        Cx::new(RunMode::Auto)
            .untrusted(true)
            .decide(&bash_spec(), &bash(&r, "cargo test")),
        Decision::Deny(deny_reason::UNTRUSTED_PROJECT)
    );
    assert_eq!(
        Cx::new(RunMode::Auto)
            .untrusted(false)
            .decide(&edit_spec(), &one(&r, "a.txt", Access::Write)),
        Decision::Allow
    );

    // 10. Причины накапливаются в порядке approval_reason (1.5).
    assert_eq!(
        Cx::new(RunMode::Ask).decide(&edit_spec(), &edit_out()),
        ask(&[ar::MODE, ar::OUTSIDE_PROJECT])
    );

    // 11. Правило ask в auto.
    assert_eq!(
        Cx::new(RunMode::Auto)
            .policy(snap(vec![tool_rule(RuleAction::Ask, "edit")]))
            .decide(&edit_spec(), &one(&r, "a.txt", Access::Write)),
        ask(&[ar::RULE])
    );

    // 12. Путь наружу в команде.
    assert_eq!(
        auto.decide(&bash_spec(), &bash(&r, "cat /etc/hosts")),
        ask(&[ar::OUTSIDE_PROJECT])
    );
}

// ---- Режим × вид инструмента ----

#[test]
fn mode_by_kind_table() {
    // Внутри проекта, доверенный, без меток и правил: решает режим (D14), `bypass` —
    // как `auto`.
    let r = roots();
    let ro = Decision::Deny("read-only mode");
    let plan = Decision::Deny("plan mode");
    let allow = Decision::Allow;
    let mode = ask(&[ar::MODE]);
    let control = spec("ask_user", ToolKind::Control, RiskClass::Read);
    let cases: [(RunMode, [&Decision; 4]); 6] = [
        (RunMode::ReadOnly, [&allow, &ro, &ro, &allow]),
        (RunMode::Plan, [&allow, &plan, &plan, &allow]),
        (RunMode::Ask, [&allow, &mode, &mode, &allow]),
        (RunMode::AcceptEdits, [&allow, &allow, &mode, &allow]),
        (RunMode::Auto, [&allow, &allow, &allow, &allow]),
        (RunMode::Bypass, [&allow, &allow, &allow, &allow]),
    ];
    for (m, [read, edit, exec, ctl]) in cases {
        let cx = Cx::new(m);
        assert_eq!(
            &cx.decide(&read_spec(), &one(&r, "a", Access::Read)),
            read,
            "{m:?} read"
        );
        assert_eq!(
            &cx.decide(&edit_spec(), &one(&r, "a", Access::Write)),
            edit,
            "{m:?} edit"
        );
        assert_eq!(
            &cx.decide(&bash_spec(), &bash(&r, "cargo build")),
            exec,
            "{m:?} exec"
        );
        assert_eq!(
            &cx.decide(&control, &(Subjects::default(), vec![])),
            ctl,
            "{m:?} ctl"
        );
    }
}

#[test]
fn approval_always_and_never_follow_gate() {
    let r = roots();
    let mut always = edit_spec();
    always.approval = Approval::Always;
    assert_eq!(
        Cx::new(RunMode::Auto).decide(&always, &one(&r, "a", Access::Write)),
        ask(&[ar::MODE])
    );
    let mut never = bash_spec();
    never.approval = Approval::Never;
    assert_eq!(
        Cx::new(RunMode::Ask).decide(&never, &bash(&r, "cargo build")),
        Decision::Allow
    );
}

// ---- Решения заказчика ----

#[test]
fn customer_a_outside_needs_grant_in_every_mode() {
    // (а): чтение и запись вне проекта — только по гранту; режим это не снимает.
    let r = roots();
    for m in [
        RunMode::Ask,
        RunMode::AcceptEdits,
        RunMode::Auto,
        RunMode::Bypass,
    ] {
        let d = Cx::new(m).decide(&read_spec(), &one(&r, &pat("/data/x"), Access::Read));
        let Decision::Ask { reasons } = d else {
            panic!("{m:?}: {d:?}");
        };
        assert!(reasons.contains(&ar::OUTSIDE_PROJECT), "{m:?}");
    }
    // Грант покрывает только свой каталог по компонентам.
    assert_eq!(
        Cx::new(RunMode::Auto)
            .grants(vec![g_ro("/data")])
            .decide(&read_spec(), &one(&r, &pat("/data2/x"), Access::Read)),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    // Команда с путём наружу — так же; снимает только грант rw (I1: команда может писать).
    assert_eq!(
        Cx::new(RunMode::Auto)
            .grants(vec![grant("/etc", GrantMode::Rw)])
            .decide(
                &bash_spec(),
                &bash(&r, &format!("cat {}", pat("/etc/hosts")))
            ),
        Decision::Allow
    );
}

#[test]
fn customer_b_trust_once_then_by_mode() {
    // (б): Trust перед любым Exec недоверенного проекта, даже в bypass и при
    // необратимой команде; после доверия — по режиму; отказ — Deny без вопроса.
    let r = roots();
    for m in [RunMode::Ask, RunMode::Auto, RunMode::Bypass] {
        assert_eq!(
            Cx::new(m)
                .untrusted(false)
                .decide(&bash_spec(), &bash(&r, "rm -rf x")),
            Decision::Trust,
            "{m:?}"
        );
        assert_eq!(
            Cx::new(m)
                .untrusted(true)
                .decide(&bash_spec(), &bash(&r, "ls")),
            Decision::Deny(deny_reason::UNTRUSTED_PROJECT),
            "{m:?}"
        );
    }
    // Запрет режима и защищённый путь — раньше доверия.
    assert_eq!(
        Cx::new(RunMode::ReadOnly)
            .untrusted(false)
            .decide(&bash_spec(), &bash(&r, "ls")),
        Decision::Deny("read-only mode")
    );
    // Правки и чтение недоверенного проекта — по режиму, без вопроса о доверии.
    assert_eq!(
        Cx::new(RunMode::Ask)
            .untrusted(false)
            .decide(&edit_spec(), &one(&r, "a", Access::Write)),
        ask(&[ar::MODE])
    );
    assert_eq!(
        Cx::new(RunMode::Auto)
            .untrusted(true)
            .decide(&read_spec(), &one(&r, "a", Access::Read)),
        Decision::Allow
    );
    // После доверия — в auto без вопросов, кроме необратимых.
    assert_eq!(
        Cx::new(RunMode::Auto).decide(&bash_spec(), &bash(&r, "cargo test")),
        Decision::Allow
    );
}

#[test]
fn tainted_never_asks() {
    // Решение заказчика 2026-11-11: чтение из сети (`git pull`, `fetch`) на запуск
    // команд не влияет. После метки команда внутри проекта в auto у доверенного
    // проекта идёт без вопроса; необратимая и путь наружу спрашиваются, как и без метки.
    let r = roots();
    for origin in ["network", "outside_project", "tool"] {
        let cx = Cx::new(RunMode::Auto).taint(origin);
        for cmd in ["git pull", "cargo test", "ls && cargo build"] {
            assert_eq!(
                cx.decide(&bash_spec(), &bash(&r, cmd)),
                Decision::Allow,
                "{origin}: {cmd}"
            );
        }
        assert_eq!(
            cx.decide(&bash_spec(), &bash(&r, "rm -rf target")),
            ask(&[ar::IRREVERSIBLE]),
            "{origin}"
        );
        assert_eq!(
            cx.decide(&bash_spec(), &bash(&r, "cat /etc/hosts")),
            ask(&[ar::OUTSIDE_PROJECT]),
            "{origin}"
        );
        // Вопрос режима — по режиму.
        assert_eq!(
            Cx::new(RunMode::Ask)
                .taint(origin)
                .decide(&bash_spec(), &bash(&r, "ls")),
            ask(&[ar::MODE])
        );
    }
    let cx = Cx::new(RunMode::Auto).taint("network");
    assert_eq!(cx.decide(&bash_spec(), &bash(&r, "ls")), Decision::Allow);
    assert_eq!(
        cx.decide(&edit_spec(), &one(&r, "a", Access::Write)),
        Decision::Allow
    );
    assert_eq!(
        cx.decide(&read_spec(), &one(&r, "a", Access::Read)),
        Decision::Allow
    );
    let net = spec("fetch", ToolKind::Read, RiskClass::Network);
    assert_eq!(
        cx.decide(&net, &(Subjects::default(), vec![])),
        Decision::Allow
    );
    // Сетевая команда сама по себе не спрашивает: метка ставится результату.
    assert_eq!(
        Cx::new(RunMode::Auto).decide(&bash_spec(), &bash(&r, "curl -s https://example.com")),
        Decision::Allow
    );
}

#[test]
fn unparsed_command_is_irreversible() {
    let r = roots();
    let d = Cx::new(RunMode::Auto).decide(&bash_spec(), &bash(&r, "echo 'unterminated"));
    assert_eq!(d, ask(&[ar::IRREVERSIBLE]));
    // Метка страховки лексера без метки классификатора — тоже необратимо.
    let mut sp = bash(&r, "ls");
    sp.0.segments[0].unparsed = true;
    assert_eq!(
        Cx::new(RunMode::Bypass).decide(&bash_spec(), &sp),
        ask(&[ar::IRREVERSIBLE])
    );
}

// ---- bypass ----

#[test]
fn bypass_never_lifts_irreversible_or_outside() {
    let r = roots();
    let by = Cx::new(RunMode::Bypass)
        .taint("network")
        .policy(snap(vec![cmd_rule(RuleAction::Allow, "git push*")]));
    // bypass снимает вопрос режима, но не необратимое и не outside_project.
    assert_eq!(
        by.decide(&bash_spec(), &bash(&r, "git push --force")),
        ask(&[ar::IRREVERSIBLE])
    );
    assert_eq!(
        by.decide(&bash_spec(), &bash(&r, "cat /etc/hosts")),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    assert_eq!(
        by.decide(&bash_spec(), &bash(&r, "rm -rf /etc/x")),
        ask(&[ar::OUTSIDE_PROJECT, ar::IRREVERSIBLE])
    );
    assert_eq!(by.decide(&bash_spec(), &bash(&r, "ls")), Decision::Allow);
    // Правило ask bypass не снимает.
    assert_eq!(
        Cx::new(RunMode::Bypass)
            .policy(snap(vec![tool_rule(RuleAction::Ask, "bash")]))
            .decide(&bash_spec(), &bash(&r, "ls")),
        ask(&[ar::RULE])
    );
}

#[test]
fn bypass_requires_global_flag() {
    let off = PolicySnapshot::build(&PolicyConfig::default(), None).unwrap();
    assert!(matches!(
        off.check_mode(RunMode::Bypass),
        Err(PolicyError::BypassDisabled)
    ));
    assert!(off.check_mode(RunMode::Auto).is_ok());
    assert!(snap(Vec::new()).check_mode(RunMode::Bypass).is_ok());
    // Флаг проекта не действует (ARC-041).
    let project = PolicyConfig {
        allow_bypass: true,
        ..PolicyConfig::default()
    };
    let p = PolicySnapshot::build(&PolicyConfig::default(), Some(&project)).unwrap();
    assert!(!p.allow_bypass);
    assert!(p.check_mode(RunMode::Bypass).is_err());
    // Снимок без флага в режиме bypass (например, из старого журнала) не даёт его
    // послаблений: вопрос режима остаётся.
    let r = roots();
    let cx = Cx::new(RunMode::Bypass).policy(off).taint("network");
    let d = cx.decide(&bash_spec(), &bash(&r, "ls"));
    assert_ne!(d, Decision::Allow, "{d:?}");
}

// ---- Пути: `~`, ZA_HOME, `.zero-agent`, ординальное сравнение ----

#[test]
fn tilde_words_are_outside() {
    let r = roots();
    // `~` в команде раскрывает оболочка: путь внешний, грант его не покрывает.
    let sp = bash(&r, "cat ~/.ssh/config");
    assert!(sp.1[0].inside, "лексически `~` — имя внутри проекта");
    for g in [vec![], vec![grant("/", GrantMode::Rw)]] {
        assert_eq!(
            Cx::new(RunMode::Bypass).grants(g).decide(&bash_spec(), &sp),
            ask(&[ar::OUTSIDE_PROJECT])
        );
    }
    // `cd ~` — так же.
    assert_eq!(
        Cx::new(RunMode::Auto).decide(&bash_spec(), &bash(&r, "cd ~ && ls")),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    // Нативный инструмент `~` не раскрывает: это каталог `~` внутри проекта.
    assert_eq!(
        Cx::new(RunMode::Auto).decide(&read_spec(), &one(&r, "~/x", Access::Read)),
        Decision::Allow
    );
}

#[test]
fn za_home_read_asks_and_write_is_denied() {
    let r = roots();
    let inside_home = r.za_home.join("config.toml").to_string_lossy().into_owned();
    let all = Cx::new(RunMode::Bypass)
        .grants(vec![grant("/", GrantMode::Rw)])
        .policy(snap(vec![path_rule(RuleAction::Allow, &pat("/**"))]));
    // Чтение ZA_HOME — Ask даже с грантом и глобальным allow.
    assert_eq!(
        all.decide(&read_spec(), &one(&r, &inside_home, Access::Read)),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    let cmd = format!("cat {}", inside_home.replace('\\', "/"));
    assert_eq!(
        all.decide(&bash_spec(), &bash(&r, &cmd)),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    // Запись — Deny, даже если разрешение пути не отметило защиту.
    let (s, mut p) = one(&r, &inside_home, Access::Write);
    p[0].protected = false;
    assert_eq!(
        all.decide(&edit_spec(), &(s, p)),
        Decision::Deny(deny_reason::PROTECTED_PATH)
    );
    // ZA_HOME внутри проекта — то же самое.
    let nested = Roots {
        project: abs("/proj"),
        za_home: abs("/proj/.za"),
    };
    let mut cx = Cx::new(RunMode::Auto);
    cx.roots = nested.clone();
    assert_eq!(
        cx.decide(&read_spec(), &one(&nested, ".za/x", Access::Read)),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    assert_eq!(
        cx.decide(&edit_spec(), &one(&nested, ".za/x", Access::Write)),
        Decision::Deny(deny_reason::PROTECTED_PATH)
    );
}

#[test]
fn project_service_dir_rules() {
    let r = roots();
    let cx = Cx::new(RunMode::Auto);
    // `.zero-agent` и `.git` проекта читаются, запись — Deny (кроме work/).
    for arg in [".zero-agent/config.toml", ".git/HEAD"] {
        assert_eq!(
            cx.decide(&read_spec(), &one(&r, arg, Access::Read)),
            Decision::Allow
        );
        let (s, mut p) = one(&r, arg, Access::Write);
        p[0].protected = false;
        assert_eq!(
            cx.decide(&edit_spec(), &(s, p)),
            Decision::Deny(deny_reason::PROTECTED_PATH),
            "{arg}"
        );
    }
    assert_eq!(
        cx.decide(
            &edit_spec(),
            &one(&r, ".zero-agent/work/edits/a", Access::Write)
        ),
        Decision::Allow
    );
    // Отметка защиты от `resolve` уважается всегда.
    let (s, mut p) = one(&r, "src/a.rs", Access::Write);
    p[0].protected = true;
    assert_eq!(
        cx.decide(&edit_spec(), &(s, p)),
        Decision::Deny(deny_reason::PROTECTED_PATH)
    );
}

#[test]
fn inside_is_ordinal_by_components() {
    // Ruling I1: без Unicode-свёртки регистра. `K` (U+212A) и `K` — разные имена.
    let r = Roots {
        project: abs("/p/Kelvin"),
        za_home: abs("/home/za"),
    };
    let mut cx = Cx::new(RunMode::Auto);
    cx.roots = r.clone();
    let other = abs("/p/\u{212A}elvin/x");
    // Даже если разрешение ошибочно отметило путь внутренним — политика сверяет сама.
    let lying = (
        Subjects {
            paths: vec![RawPath {
                arg: other.to_string_lossy().into_owned(),
                access: Access::Read,
            }],
            ..Subjects::default()
        },
        vec![ResolvedPath {
            path: other.clone(),
            inside: true,
            protected: false,
        }],
    );
    assert_eq!(cx.decide(&read_spec(), &lying), ask(&[ar::OUTSIDE_PROJECT]));
    // `/p/Kelvin2` — не внутри `/p/Kelvin`.
    assert_eq!(
        cx.decide(&read_spec(), &one(&r, &pat("/p/Kelvin2/x"), Access::Read)),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    // Грант сравнивается так же: `/data` не покрывает `/DATA/x` и `/data2`.
    assert!(!covers(&[g_ro("/data")], &abs("/DATA/x"), Access::Read));
    assert!(!covers(&[g_ro("/data")], &abs("/data2/x"), Access::Read));
}

#[test]
fn unmatched_resolution_is_outside() {
    // Пакет обязан разрешить ровно `subject_paths`; иначе — Ask, а не Allow.
    let r = roots();
    let (s, _) = one(&r, "a", Access::Read);
    assert_eq!(
        Cx::new(RunMode::Auto).decide(&read_spec(), &(s, vec![])),
        ask(&[ar::OUTSIDE_PROJECT])
    );
}

#[test]
fn cwd_is_a_subject_path() {
    let r = roots();
    let mut s = bash(&r, "ls").0;
    s.cwd = Some(pat("/data"));
    let raw = subject_paths(&s);
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].access, Access::Exec);
    let p: Vec<ResolvedPath> = raw.iter().map(|x| rp(&r, &x.arg, x.access)).collect();
    let sp = (s, p);
    assert_eq!(
        Cx::new(RunMode::Auto).decide(&bash_spec(), &sp),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    // Команда в чужом каталоге может писать: нужен грант rw.
    assert_eq!(
        Cx::new(RunMode::Auto)
            .grants(vec![g_ro("/data")])
            .decide(&bash_spec(), &sp),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    assert_eq!(
        Cx::new(RunMode::Auto)
            .grants(vec![grant("/data", GrantMode::Rw)])
            .decide(&bash_spec(), &sp),
        Decision::Allow
    );
}

// ---- Правила ----

#[test]
fn deny_rules_match_argv0_or_any_segment() {
    let r = roots();
    let cx = Cx::new(RunMode::Bypass).policy(snap(vec![cmd_rule(RuleAction::Deny, "rm")]));
    for cmd in ["rm a", "ls && sudo /bin/RM.exe a", "echo $(rm a)"] {
        assert_eq!(
            cx.decide(&bash_spec(), &bash(&r, cmd)),
            Decision::Deny(deny_reason::POLICY_RULE),
            "{cmd}"
        );
    }
    assert_eq!(cx.decide(&bash_spec(), &bash(&r, "ls rm")), Decision::Allow);
    // Шаблон по тексту сегмента.
    let cx = Cx::new(RunMode::Auto).policy(snap(vec![cmd_rule(RuleAction::Deny, "git push*")]));
    assert_eq!(
        cx.decide(&bash_spec(), &bash(&r, "git status; git push origin")),
        Decision::Deny(deny_reason::POLICY_RULE)
    );
    // Правило с несколькими полями — все должны совпасть.
    let both = Rule {
        tool: Some("edit".into()),
        path: Some("src/**".into()),
        ..rule(RuleAction::Deny)
    };
    let cx = Cx::new(RunMode::Auto).policy(snap(vec![both]));
    assert_eq!(
        cx.decide(&edit_spec(), &one(&r, "src/a.rs", Access::Write)),
        Decision::Deny(deny_reason::POLICY_RULE)
    );
    assert_eq!(
        cx.decide(&edit_spec(), &one(&r, "docs/a.md", Access::Write)),
        Decision::Allow
    );
    assert_eq!(
        cx.decide(&read_spec(), &one(&r, "src/a.rs", Access::Read)),
        Decision::Allow
    );
}

#[test]
fn allow_rule_needs_every_segment_without_subst_or_redirect() {
    let r = roots();
    let cx = Cx::new(RunMode::Ask).policy(snap(vec![cmd_rule(RuleAction::Allow, "cargo *")]));
    assert_eq!(
        cx.decide(&bash_spec(), &bash(&r, "cargo build && cargo test")),
        Decision::Allow
    );
    for cmd in [
        "cargo build && ls",
        "cargo build > out.txt",
        "cargo build `ls`",
        "cargo build | tee x",
    ] {
        assert_eq!(
            cx.decide(&bash_spec(), &bash(&r, cmd)),
            ask(&[ar::MODE]),
            "{cmd}"
        );
    }
    // allow не снимает ask-правило, необратимое и outside_project.
    let cx = Cx::new(RunMode::Auto).policy(snap(vec![
        cmd_rule(RuleAction::Allow, "*"),
        tool_rule(RuleAction::Ask, "bash"),
    ]));
    assert_eq!(cx.decide(&bash_spec(), &bash(&r, "ls")), ask(&[ar::RULE]));
    let cx = Cx::new(RunMode::Auto).policy(snap(vec![cmd_rule(RuleAction::Allow, "*")]));
    assert_eq!(
        cx.decide(&bash_spec(), &bash(&r, "cat /etc/hosts")),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    assert_eq!(
        cx.decide(&bash_spec(), &bash(&r, "rm -rf x")),
        ask(&[ar::IRREVERSIBLE])
    );
}

#[test]
fn allow_rule_lifts_mode() {
    let r = roots();
    let cx = Cx::new(RunMode::Ask).policy(snap(vec![Rule {
        tool: Some("edit".into()),
        path: Some("src/**".into()),
        ..rule(RuleAction::Allow)
    }]));
    assert_eq!(
        cx.decide(&edit_spec(), &one(&r, "src/a/b.rs", Access::Write)),
        Decision::Allow
    );
    assert_eq!(
        cx.decide(&edit_spec(), &one(&r, "README.md", Access::Write)),
        ask(&[ar::MODE])
    );
}

// ---- Шаблоны путей ----

#[test]
fn path_globs_by_components() {
    let r = roots();
    let d = |rules: Vec<Rule>, arg: &str| {
        Cx::new(RunMode::Auto)
            .policy(snap(rules))
            .decide(&read_spec(), &one(&r, arg, Access::Read))
    };
    let outside = ask(&[ar::OUTSIDE_PROJECT]);
    // `*` не пересекает разделитель, `**` — да; точное совпадение — тоже.
    let star = || vec![path_rule(RuleAction::Allow, &pat("/data/*"))];
    assert_eq!(d(star(), &pat("/data/x")), Decision::Allow);
    assert_eq!(d(star(), &pat("/data/sub/x")), outside);
    assert_eq!(
        d(
            vec![path_rule(RuleAction::Allow, &pat("/data/**"))],
            &pat("/data/sub/x")
        ),
        Decision::Allow
    );
    assert_eq!(
        d(
            vec![path_rule(RuleAction::Allow, &pat("/data/f"))],
            &pat("/data/f")
        ),
        Decision::Allow
    );
    assert_eq!(
        d(
            vec![path_rule(RuleAction::Allow, &pat("/data/**"))],
            &pat("/data2/x")
        ),
        outside
    );
    // Относительный шаблон — только для путей внутри проекта.
    assert_eq!(
        d(vec![path_rule(RuleAction::Allow, "**")], &pat("/data/x")),
        outside
    );
    // Внутренний путь сопоставляется относительным через `/`.
    let cx =
        Cx::new(RunMode::Auto).policy(snap(vec![path_rule(RuleAction::Deny, "secrets/*.key")]));
    assert_eq!(
        cx.decide(&read_spec(), &one(&r, "secrets/a.key", Access::Read)),
        Decision::Deny(deny_reason::POLICY_RULE)
    );
    // Абсолютный шаблон deny действует и на путь внутри проекта.
    let cx = Cx::new(RunMode::Auto).policy(snap(vec![path_rule(
        RuleAction::Deny,
        &pat("/proj/secrets/**"),
    )]));
    assert_eq!(
        cx.decide(&read_spec(), &one(&r, "secrets/a.key", Access::Read)),
        Decision::Deny(deny_reason::POLICY_RULE)
    );
    // `..` в шаблоне отвергается при сборке.
    assert!(matches!(
        PolicySnapshot::build(
            &cfg_with(vec![path_rule(RuleAction::Allow, &pat("/data/../**"))]),
            None
        ),
        Err(PolicyError::Glob(0, "path", _))
    ));
}

#[cfg(windows)]
#[test]
fn path_globs_ignore_case_and_backslashes_on_windows() {
    let r = roots();
    let cx = Cx::new(RunMode::Auto).policy(snap(vec![path_rule(RuleAction::Allow, r"c:\DATA\**")]));
    assert_eq!(
        cx.decide(&read_spec(), &one(&r, r"C:\data\x", Access::Read)),
        Decision::Allow
    );
}

/// Ссылка на каталог: junction на Windows (`mklink /J` прав не требует), символическая
/// ссылка на Unix.
fn dir_link(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
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

#[test]
fn glob_cannot_be_escaped_by_dotdot_or_link() {
    // Шаблон сопоставляется с каноническим путём (D76): ни `..`, ни ссылка внутри
    // разрешённого каталога не выводят за него.
    let base = support::unique_dir("policy", "glob_escape");
    let project = base.join("proj");
    let data = base.join("data");
    let secret = base.join("secret");
    for d in [&project, &data, &secret] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(data.join("f"), "x").unwrap();
    std::fs::write(secret.join("f"), "s").unwrap();
    dir_link(&data.join("l"), &secret);
    let roots = Roots::canonical(&project, &base.join("home")).unwrap();
    let res = |arg: &str| {
        let lex = classify(&roots.project, arg).unwrap();
        let s = Subjects {
            paths: vec![RawPath {
                arg: arg.to_owned(),
                access: Access::Read,
            }],
            ..Subjects::default()
        };
        (s, vec![resolve(&roots, &lex, Access::Read).unwrap()])
    };
    let data_canon = res(&data.to_string_lossy()).1[0].path.clone();
    let glob = format!("{}/**", data_canon.to_string_lossy().replace('\\', "/"));
    let mut cx = Cx::new(RunMode::Auto).policy(snap(vec![path_rule(RuleAction::Allow, &glob)]));
    cx.roots = roots.clone();
    let shown = |p: &Path| p.to_string_lossy().into_owned();
    assert_eq!(
        cx.decide(&read_spec(), &res(&shown(&data.join("f")))),
        Decision::Allow
    );
    let dotdot = format!("{}/../secret/f", shown(&data));
    assert_eq!(
        cx.decide(&read_spec(), &res(&dotdot)),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    assert_eq!(
        cx.decide(&read_spec(), &res(&shown(&data.join("l").join("f")))),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    // Грант на каталог так же не покрывает путь через ссылку.
    let mut cx = Cx::new(RunMode::Auto).grants(vec![Grant {
        path: data_canon,
        mode: GrantMode::Ro,
        source: GrantSource::Config,
        reason: String::new(),
    }]);
    cx.roots = roots.clone();
    assert_eq!(
        cx.decide(&read_spec(), &res(&shown(&data.join("l").join("f")))),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    assert_eq!(
        cx.decide(&read_spec(), &res(&shown(&data.join("f")))),
        Decision::Allow
    );
}

// ---- Снимок политики ----

#[test]
fn project_allow_is_ignored() {
    let g = cfg_with(vec![tool_rule(RuleAction::Deny, "bash")]);
    let proj_with_allow = PolicyConfig {
        rules: vec![
            tool_rule(RuleAction::Allow, "edit"),
            tool_rule(RuleAction::Ask, "read"),
        ],
        ..PolicyConfig::default()
    };
    let p = PolicySnapshot::build(&g, Some(&proj_with_allow)).unwrap();
    assert_eq!(p.ignored.len(), 1);
    assert_eq!(p.ignored[0], tool_rule(RuleAction::Allow, "edit"));
    assert_eq!(
        p.rules,
        vec![
            (RuleSource::Global, tool_rule(RuleAction::Deny, "bash")),
            (RuleSource::Project, tool_rule(RuleAction::Ask, "read")),
        ]
    );
}

#[test]
fn empty_rule_rejected() {
    assert!(matches!(
        PolicySnapshot::build(&cfg_with(vec![rule(RuleAction::Deny)]), None),
        Err(PolicyError::Empty(0))
    ));
    let e = PolicySnapshot::build(
        &cfg_with(vec![
            tool_rule(RuleAction::Deny, "x"),
            rule(RuleAction::Ask),
        ]),
        None,
    )
    .unwrap_err();
    assert_eq!(
        e.to_string(),
        "policy.rules[1]: нужно хотя бы одно поле tool, path или command"
    );
    // Проектные правила проверяются так же.
    let project = PolicyConfig {
        rules: vec![cmd_rule(RuleAction::Deny, "[")],
        ..PolicyConfig::default()
    };
    assert!(matches!(
        PolicySnapshot::build(&PolicyConfig::default(), Some(&project)),
        Err(PolicyError::Glob(0, "command", _))
    ));
    assert!(matches!(
        PolicySnapshot::build(&cfg_with(vec![path_rule(RuleAction::Deny, "")]), None),
        Err(PolicyError::Glob(0, "path", _))
    ));
}

#[test]
fn hash_is_stable() {
    let g = PolicyConfig {
        allow_bypass: false,
        rules: vec![cmd_rule(RuleAction::Allow, "cargo test")],
        grants: vec![GrantConfig {
            path: abs("/data"),
            mode: GrantMode::Ro,
            reason: "r".into(),
        }],
    };
    let h = PolicySnapshot::build(&g, None).unwrap().hash;
    assert_eq!(h, PolicySnapshot::build(&g.clone(), None).unwrap().hash);
    assert_eq!(h.len(), 64);
    // Любое поле меняет хэш: флаг, правило, грант, источник правила.
    let mut b = g.clone();
    b.allow_bypass = true;
    let mut c = g.clone();
    c.rules[0].command = Some("cargo build".into());
    let mut d = g.clone();
    d.grants[0].mode = GrantMode::Rw;
    let ask_rule = cfg_with(vec![tool_rule(RuleAction::Ask, "x")]);
    let ask_project = PolicyConfig {
        rules: vec![tool_rule(RuleAction::Ask, "x")],
        ..PolicyConfig::default()
    };
    let hashes = [
        h,
        PolicySnapshot::build(&b, None).unwrap().hash,
        PolicySnapshot::build(&c, None).unwrap().hash,
        PolicySnapshot::build(&d, None).unwrap().hash,
        PolicySnapshot::build(&ask_rule, None).unwrap().hash,
        PolicySnapshot::build(&cfg_with(vec![]), Some(&ask_project))
            .unwrap()
            .hash,
    ];
    let unique: BTreeSet<&String> = hashes.iter().collect();
    assert_eq!(unique.len(), hashes.len());
}

#[test]
fn snapshot_roundtrips_json() {
    let p =
        PolicySnapshot::build(&cfg_with(vec![tool_rule(RuleAction::Ask, "bash")]), None).unwrap();
    let back: PolicySnapshot = serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
    assert_eq!(back, p);
    // Снимок R1-B без поля политики читается пустым.
    assert_eq!(
        serde_json::from_value::<PolicySnapshot>(json!({})).unwrap(),
        PolicySnapshot::default()
    );
}

#[test]
fn rule_rejects_unknown_fields() {
    assert!(
        serde_json::from_value::<Rule>(json!({"action": "deny", "tool": "x", "tol": "y"})).is_err()
    );
    assert!(serde_json::from_value::<Rule>(json!({"action": "permit", "tool": "x"})).is_err());
    assert!(serde_json::from_value::<GrantConfig>(json!({"path": "/x", "mode": "wr"})).is_err());
    assert!(serde_json::from_value::<PolicyConfig>(json!({"allow_bypas": true})).is_err());
    let g: GrantConfig = serde_json::from_value(json!({"path": "/x"})).unwrap();
    assert_eq!(g.mode, GrantMode::Ro);
}

// ---- Гранты ----

#[test]
fn covers_by_components() {
    assert!(!covers(
        &[g_ro("/data")],
        Path::new("/data2/x"),
        Access::Read
    ));
    assert!(covers(&[g_ro("/data")], &abs("/data"), Access::Read));
    assert!(covers(&[g_ro("/data")], &abs("/data/a/b"), Access::Read));
    assert!(!covers(&[g_ro("/data")], &abs("/data/a"), Access::Write));
    assert!(!covers(&[g_ro("/data")], &abs("/data/a"), Access::Exec));
    let rw = grant("/data", GrantMode::Rw);
    for a in [Access::Read, Access::Write, Access::Exec] {
        assert!(
            covers(std::slice::from_ref(&rw), &abs("/data/a"), a),
            "{a:?}"
        );
    }
    assert!(!covers(&[], &abs("/data/a"), Access::Read));
    assert!(!covers(&[g_ro("/data/a")], &abs("/data"), Access::Read));
}

#[test]
fn grant_flag_parses_windows_path() {
    assert_eq!(
        parse_grant_flag(r"C:\data=rw").unwrap(),
        (PathBuf::from(r"C:\data"), GrantMode::Rw)
    );
    assert_eq!(
        parse_grant_flag(r"C:\data").unwrap(),
        (PathBuf::from(r"C:\data"), GrantMode::Ro)
    );
    assert_eq!(
        parse_grant_flag("/srv/a=b=ro").unwrap(),
        (PathBuf::from("/srv/a=b"), GrantMode::Ro)
    );
    for bad in ["", "=rw", "/data=wr", "/data="] {
        assert!(parse_grant_flag(bad).is_err(), "{bad}");
    }
}

#[test]
fn grants_text_render() {
    let root = abs("/proj");
    assert_eq!(grants_text(&[], &root), "none");
    assert_eq!(
        grants_text(
            &[Grant {
                path: PathBuf::from("/data"),
                ..g_ro("/x")
            }],
            &root
        ),
        "/data (ro)"
    );
    let two = [g_ro("/data"), grant("/srv/<b>", GrantMode::Rw)];
    let text = grants_text(&two, &root);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].ends_with("data (ro)"), "{text}");
    assert!(
        lines[1].ends_with("&lt;b&gt; (rw)"),
        "пути экранируются: {text}"
    );
}

// ---- Тексты для модели ----

#[test]
fn policy_texts_are_hashed() {
    let texts = runtime_texts();
    let text = |n: &str| {
        texts
            .iter()
            .find(|(x, _)| *x == n)
            .unwrap_or_else(|| panic!("{n}"))
            .1
            .clone()
    };
    assert_eq!(
        text("runtime/policy.denied_protected_path"),
        "denied: protected path"
    );
    assert_eq!(
        text("runtime/policy.denied_policy_rule"),
        "denied: policy rule"
    );
    assert_eq!(
        text("runtime/policy.denied_untrusted_project"),
        "denied: untrusted project"
    );
    assert_eq!(text("runtime/policy.grants_none"), "none");
    assert_eq!(text("runtime/policy.grants"), "/data (ro)\n/srv (rw)");
    for (n, t) in &texts {
        if n.starts_with("runtime/policy.") {
            assert!(t.is_ascii(), "{n}");
        }
    }
}

// ---- Раунд исправлений 1 (ревью задачи 11) ----

#[test]
fn c1_allow_without_command_does_not_apply_to_bash() {
    // C1: для bash allow действует только с `command`; правило по `path` или `tool`
    // не снимает ни `mode`, ни `outside_project` (метка tainted вопроса не даёт вовсе —
    // решение заказчика 2026-11-11, поэтому сигнал здесь — вопрос режима `ask`).
    let r = roots();
    let src = r.project.join("src/a").to_string_lossy().replace('\\', "/");
    let cmd = format!("curl -o x https://example.com; python x; ls {src}");
    let by_path = Cx::new(RunMode::Ask).policy(snap(vec![path_rule(RuleAction::Allow, "src/**")]));
    assert_eq!(
        by_path.decide(&bash_spec(), &bash(&r, &cmd)),
        ask(&[ar::MODE])
    );
    let by_tool = Cx::new(RunMode::Ask).policy(snap(vec![tool_rule(RuleAction::Allow, "bash")]));
    assert_eq!(
        by_tool.decide(&bash_spec(), &bash(&r, "cargo build $(curl x)")),
        ask(&[ar::MODE])
    );
    // В режиме ask allow по инструменту не снимает вопрос режима у bash.
    let ask_mode = Cx::new(RunMode::Ask).policy(snap(vec![tool_rule(RuleAction::Allow, "bash")]));
    assert_eq!(
        ask_mode.decide(&bash_spec(), &bash(&r, "ls")),
        ask(&[ar::MODE])
    );
    // Путь наружу в команде правило без `command` не покрывает.
    let data =
        Cx::new(RunMode::Auto).policy(snap(vec![path_rule(RuleAction::Allow, &pat("/data/**"))]));
    assert_eq!(
        data.decide(&bash_spec(), &bash(&r, &format!("cat {}", pat("/data/a")))),
        ask(&[ar::OUTSIDE_PROJECT])
    );
    // Правило с `command` и `path` покрывает путь своей команды.
    let both = Cx::new(RunMode::Auto).policy(snap(vec![Rule {
        command: Some("cat *".into()),
        path: Some(pat("/data/**")),
        ..rule(RuleAction::Allow)
    }]));
    assert_eq!(
        both.decide(&bash_spec(), &bash(&r, &format!("cat {}", pat("/data/a")))),
        Decision::Allow
    );
    // Для нативного инструмента allow по `path` действует, как прежде.
    assert_eq!(
        Cx::new(RunMode::Ask)
            .policy(snap(vec![path_rule(RuleAction::Allow, "src/**")]))
            .decide(&edit_spec(), &one(&r, "src/a", Access::Write)),
        Decision::Allow
    );
}

#[test]
fn c1_allow_never_lifts_irreversible() {
    // Ruling C1: allow снимает только Ask по режиму; необратимое и `unparsed` остаются.
    let r = roots();
    let cx = Cx::new(RunMode::Ask)
        .taint("outside_project")
        .policy(snap(vec![cmd_rule(RuleAction::Allow, "*")]));
    assert_eq!(
        cx.decide(&bash_spec(), &bash(&r, "cargo test")),
        Decision::Allow
    );
    assert_eq!(
        cx.decide(&bash_spec(), &bash(&r, "git push --force")),
        ask(&[ar::IRREVERSIBLE])
    );
    assert_eq!(
        cx.decide(&bash_spec(), &bash(&r, "echo 'unterminated")),
        ask(&[ar::MODE, ar::IRREVERSIBLE])
    );
}

#[test]
fn command_words_checked_at_build() {
    // N1 повторного ревью: шаблон верен целиком, но не по словам — ошибка сборки, а не
    // «сломанное» правило, срабатывающее на каждый вызов.
    for p in ["echo {a b}", "grep [ ]x"] {
        assert!(
            matches!(
                PolicySnapshot::build(&cfg_with(vec![cmd_rule(RuleAction::Deny, p)]), None),
                Err(PolicyError::Glob(0, "command", _))
            ),
            "{p}"
        );
    }
}

#[test]
fn i1_bash_paths_need_rw_grant() {
    // I1: пути команды считаются записью — грант ro не разрешает bash снаружи.
    let r = roots();
    for cmd in ["cp a /data/x", "cat /data/x", "sed -i s/a/b/ /data/x"] {
        let cmd = cmd.replace("/data", &pat("/data"));
        assert_eq!(
            Cx::new(RunMode::Auto)
                .grants(vec![g_ro("/data")])
                .decide(&bash_spec(), &bash(&r, &cmd)),
            ask(&[ar::OUTSIDE_PROJECT]),
            "{cmd}"
        );
        assert_eq!(
            Cx::new(RunMode::Auto)
                .grants(vec![grant("/data", GrantMode::Rw)])
                .decide(&bash_spec(), &bash(&r, &cmd)),
            Decision::Allow,
            "{cmd}"
        );
    }
}

#[test]
fn i2_deny_command_matches_normalized_command() {
    let r = roots();
    let cx = Cx::new(RunMode::Auto).policy(snap(vec![cmd_rule(RuleAction::Deny, "git push*")]));
    for cmd in [
        "git push origin main",
        "git  push",
        "git -C . push",
        "\"git\" push",
        "sudo git push",
        "env git push",
        "timeout 5 git push",
        "GIT push",
        "/usr/bin/git.exe push",
        "bash -c \"git push\"",
        "git -c user.name=x push origin",
    ] {
        assert_eq!(
            cx.decide(&bash_spec(), &bash(&r, cmd)),
            Decision::Deny(deny_reason::POLICY_RULE),
            "{cmd}"
        );
    }
    for cmd in ["git status", "git log --grep=push", "echo git push"] {
        assert_ne!(
            cx.decide(&bash_spec(), &bash(&r, cmd)),
            Decision::Deny(deny_reason::POLICY_RULE),
            "{cmd}"
        );
    }
    // Слова шаблона после имени — подпоследовательностью среди аргументов.
    let cx = Cx::new(RunMode::Auto).policy(snap(vec![cmd_rule(RuleAction::Ask, "cargo publish")]));
    assert_eq!(
        cx.decide(&bash_spec(), &bash(&r, "cargo --locked publish --dry-run")),
        ask(&[ar::RULE])
    );
    assert_eq!(
        cx.decide(&bash_spec(), &bash(&r, "cargo build")),
        Decision::Allow
    );
}

#[test]
fn i3_approval_always_survives_bypass_and_allow() {
    let r = roots();
    let mut always = edit_spec();
    always.approval = Approval::Always;
    assert_eq!(
        Cx::new(RunMode::Bypass).decide(&always, &one(&r, "a", Access::Write)),
        ask(&[ar::MODE])
    );
    assert_eq!(
        Cx::new(RunMode::Auto)
            .policy(snap(vec![tool_rule(RuleAction::Allow, "*")]))
            .decide(&always, &one(&r, "a", Access::Write)),
        ask(&[ar::MODE])
    );
}

#[test]
fn deny_path_checks_every_bash_word() {
    // Ранее принятое решение: deny с `path` сверяется со всеми словами-аргументами bash.
    let r = roots();
    let cx = Cx::new(RunMode::Auto).policy(snap(vec![
        path_rule(RuleAction::Deny, "secrets/**"),
        path_rule(RuleAction::Deny, ".env"),
    ]));
    for cmd in [
        "cat secrets/a.key",
        "cat ./secrets/a.key",
        "cat .env",
        "grep x .env",
        "echo x > .env",
    ] {
        assert_eq!(
            cx.decide(&bash_spec(), &bash(&r, cmd)),
            Decision::Deny(deny_reason::POLICY_RULE),
            "{cmd}"
        );
    }
    assert_eq!(
        cx.decide(&bash_spec(), &bash(&r, "cat src/a.rs")),
        Decision::Allow
    );
}

#[test]
fn grants_text_escapes_control_chars() {
    // m4: перевод строки в пути не дорисовывает ложную строку `<grants>`.
    let g = Grant {
        path: PathBuf::from("/a\n/etc (rw)"),
        ..g_ro("/x")
    };
    let text = grants_text(&[g], &abs("/proj"));
    assert_eq!(text.lines().count(), 1, "{text}");
}

#[test]
fn tool_pattern_ignores_case() {
    // m2: `deny tool = "Bash"` срабатывает на `bash`.
    let r = roots();
    let cx = Cx::new(RunMode::Auto).policy(snap(vec![tool_rule(RuleAction::Deny, "Bash")]));
    assert_eq!(
        cx.decide(&bash_spec(), &bash(&r, "ls")),
        Decision::Deny(deny_reason::POLICY_RULE)
    );
}
