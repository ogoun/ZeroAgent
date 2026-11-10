//! `za-jail`: окружение, коды выхода, тайм-аут и отмена убивают дерево, фоновые потомки,
//! предел вывода, остатки после смерти родителя (R1-C1 задача 5, D84).
//!
//! Дерево строит помощник `za_jail_tree` (запускает сам себя, без `sleep` и оболочки).
//! Ожидания — под `within` × `ZA_TEST_TIME_SCALE`; длительности не проверяются.

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use za_jail::{
    CAPTURE_LIMIT, CommandSpec, DirectRunner, EnvMode, Exit, KILL_GRACE, Runner, child_env,
};

#[path = "../../../test-support/unique_dir.rs"]
mod unique;

// ---- Помощники ----

/// Множитель времени тестов: `ZA_TEST_TIME_SCALE` (целое 1–6, по умолчанию 1).
fn scale() -> u32 {
    static SCALE: OnceLock<u32> = OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("ZA_TEST_TIME_SCALE")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map_or(1, |n| n.clamp(1, 6))
    })
}

fn scaled(d: Duration) -> Duration {
    d * scale()
}

/// Ждёт `f` не дольше `base` × [`scale`]; иначе — паника.
async fn within<T>(base: Duration, what: &str, f: impl Future<Output = T>) -> T {
    tokio::time::timeout(scaled(base), f)
        .await
        .unwrap_or_else(|_| panic!("не дождались в срок: {what}"))
}

fn dir(name: &str) -> PathBuf {
    unique::unique_dir("runner", name)
}

fn tree() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_za_jail_tree"))
}

fn spec(cwd: &Path, args: &[&str], timeout: Duration) -> CommandSpec {
    let mut s = CommandSpec::new(tree(), cwd, timeout);
    s.args = args.iter().map(OsString::from).collect();
    s
}

/// Pid из строк `pid <n>`.
fn pids(text: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|l| l.strip_prefix("pid ")?.trim().parse().ok())
        .collect()
}

/// Процесс мёртв: его нет или он зомби (Ruling 5.19).
fn dead(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessStatus, ProcessesToUpdate, System};
    let pid = Pid::from_u32(pid);
    let mut sys = System::new().expect("sysinfo");
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid)
        .is_none_or(|p| p.status() == ProcessStatus::Zombie)
}

/// Ждёт, пока все `pids` умрут.
async fn all_dead(pids: &[u32], what: &str) {
    within(Duration::from_secs(10), what, async {
        loop {
            let alive: Vec<u32> = pids.iter().copied().filter(|p| !dead(*p)).collect();
            if alive.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
}

/// Ждёт `n` строк `pid` в файле.
async fn wait_pid_file(file: &Path, n: usize) -> Vec<u32> {
    within(
        Duration::from_secs(20),
        "pid всех процессов дерева",
        async {
            loop {
                if let Ok(t) = tokio::fs::read_to_string(file).await {
                    let p = pids(&t);
                    if p.len() >= n {
                        return p;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        },
    )
    .await
}

fn names(env: &[(OsString, OsString)]) -> Vec<String> {
    env.iter()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect()
}

fn base(vars: &[(&str, &str)]) -> impl Iterator<Item = (OsString, OsString)> {
    vars.iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect::<Vec<_>>()
        .into_iter()
}

// ---- Окружение ----

#[test]
fn env_drops_secrets_case_insensitive_on_windows() {
    let project = dir("env_secrets");
    let mut s = spec(&project, &[], Duration::from_secs(1));
    s.tmp_dir = Some(project.join(".zero-agent/work/tmp/r1"));
    s.remove_env = vec!["drop_me".into()];
    let env = child_env(
        base(&[
            ("za_secret_x", "v"),
            ("ZA_SECRETS_FILE", "v"),
            ("Za_Secret_Y", "v"),
            ("PATH", "p"),
            ("ZA_HOME", "h"),
            ("za_home", "h"),
            ("DROP_ME", "x"),
            ("TMP", "C:/system-temp"),
            ("KEEP", "k"),
        ]),
        &s,
        true,
    );
    assert!(env.iter().all(|(k, _)| {
        let up = k.to_string_lossy().to_ascii_uppercase();
        !up.starts_with("ZA_SECRET") && up != "ZA_HOME" && up != "DROP_ME"
    }));
    for k in ["TMP", "TEMP", "TMPDIR"] {
        let found: Vec<_> = env.iter().filter(|(n, _)| n == k).collect();
        assert_eq!(found.len(), 1, "{k} один");
        assert!(
            Path::new(&found[0].1).starts_with(&project),
            "{k} внутри проекта"
        );
    }
    assert!(
        env.iter()
            .any(|(n, v)| n == "GIT_TERMINAL_PROMPT" && v == "0")
    );
    assert!(
        env.iter()
            .any(|(n, v)| n == "GCM_INTERACTIVE" && v == "never")
    );
    assert!(names(&env).contains(&"KEEP".to_string()));
    assert!(names(&env).contains(&"PATH".to_string()));
}

#[test]
fn env_unix_names_are_case_sensitive_but_secrets_are_not() {
    let project = dir("env_unix");
    let mut s = spec(&project, &[], Duration::from_secs(1));
    s.remove_env = vec!["DROP_ME".into()];
    let env = child_env(
        base(&[("za_secret_x", "v"), ("za_home", "h"), ("drop_me", "x")]),
        &s,
        false,
    );
    let n = names(&env);
    assert!(!n.contains(&"za_secret_x".to_string()));
    assert!(
        n.contains(&"za_home".to_string()),
        "на Unix za_home — другое имя"
    );
    assert!(n.contains(&"drop_me".to_string()));
}

#[test]
fn env_clear_keeps_whitelist_only() {
    let project = dir("env_clear");
    let mut s = spec(&project, &[], Duration::from_secs(1));
    s.env_mode = EnvMode::Clear;
    s.env_set = vec![("A".into(), "1".into()), ("Path".into(), "override".into())];
    let vars = [
        ("Path", "p"),
        ("SYSTEMROOT", "C:/Windows"),
        ("windir", "C:/Windows"),
        ("HOME", "h"),
        ("ZA_SECRET_X", "v"),
        ("FOO", "f"),
    ];
    let env = child_env(base(&vars), &s, true);
    let mut n = names(&env);
    n.sort();
    assert_eq!(n, ["A", "Path", "SYSTEMROOT", "windir"]);
    assert!(env.iter().any(|(k, v)| k == "Path" && v == "override"));
    // Без регистра — только на Windows.
    let env = child_env(
        base(&[("Path", "p"), ("PATH", "P")]),
        &spec(&project, &[], Duration::ZERO),
        false,
    );
    assert!(names(&env).contains(&"Path".to_string()));
    let mut c = spec(&project, &[], Duration::ZERO);
    c.env_mode = EnvMode::Clear;
    assert_eq!(
        names(&child_env(base(&[("Path", "p"), ("PATH", "P")]), &c, false)),
        ["PATH"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn child_does_not_see_secrets() {
    // Настоящий процесс: `za_jail_tree` не печатает окружение, поэтому проверка — через
    // `cmd /c set` или `env`, если они есть; иначе тест окружения уже покрыт выше.
    let project = dir("child_env");
    let (program, args): (&str, &[&str]) = if cfg!(windows) {
        ("cmd", &["/d", "/c", "set"])
    } else {
        ("env", &[])
    };
    let mut s = CommandSpec::new(program, &project, scaled(Duration::from_secs(20)));
    s.args = args.iter().map(OsString::from).collect();
    let mut b: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    b.push(("ZA_SECRET_T".into(), "leak-me-123".into()));
    b.push(("za_secrets_low".into(), "leak-me-456".into()));
    s.base_env = Some(b);
    let out = within(Duration::from_secs(30), "env", DirectRunner.run(s))
        .await
        .expect("запуск");
    assert_eq!(out.exit, Exit::Code(0));
    assert!(
        !out.stdout.contains("leak-me"),
        "секрет дошёл до дочернего процесса"
    );
    assert!(out.stdout.contains("GIT_TERMINAL_PROMPT=0"));
}

// ---- Запуск ----

#[tokio::test(flavor = "multi_thread")]
async fn exit_code_is_reported() {
    let project = dir("exit_code");
    let out = within(
        Duration::from_secs(20),
        "запуск --exit 3",
        DirectRunner.run(spec(
            &project,
            &["--exit", "3"],
            scaled(Duration::from_secs(20)),
        )),
    )
    .await
    .expect("ненулевой код — данные, не ошибка");
    assert_eq!(out.exit, Exit::Code(3));
    assert_eq!(pids(&out.stdout).len(), 1);
    assert!(!out.background_killed && !out.stdout_truncated && !out.stderr_truncated);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_program_is_spawn_error() {
    let project = dir("missing");
    let s = CommandSpec::new(
        project.join("no-such-program"),
        &project,
        Duration::from_secs(5),
    );
    let err = DirectRunner.run(s).await.expect_err("программы нет");
    assert!(matches!(err, za_jail::RunnerError::Spawn { .. }), "{err:?}");
    assert!(err.to_string().starts_with("failed to start "));
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_kills_whole_tree() {
    let project = dir("timeout");
    let s = spec(
        &project,
        &["--depth", "2", "--hold"],
        scaled(Duration::from_secs(2)),
    );
    let out = within(
        Duration::from_secs(2) + KILL_GRACE * 2 + Duration::from_secs(10),
        "тайм-аут дерева",
        DirectRunner.run(s),
    )
    .await
    .expect("тайм-аут — не ошибка");
    assert_eq!(out.exit, Exit::Timeout);
    let p = pids(&out.stdout);
    assert_eq!(p.len(), 3, "лидер, потомок и внук: {:?}", out.stdout);
    all_dead(&p, "процессы дерева после тайм-аута").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dropped_future_kills_tree() {
    let project = dir("dropped");
    let file = project.join("pids.txt");
    let file_s = file.display().to_string();
    let s = spec(
        &project,
        &["--depth", "2", "--hold", "--pid-file", &file_s],
        Duration::from_secs(3600),
    );
    let mut fut = DirectRunner.run(s);
    let p = tokio::select! {
        r = &mut fut => panic!("дерево с --hold завершилось само: {r:?}"),
        p = wait_pid_file(&file, 3) => p,
    };
    drop(fut);
    all_dead(&p, "процессы дерева после броска future").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn background_child_does_not_hold_call() {
    let project = dir("background");
    let s = spec(
        &project,
        &["--depth", "2", "--background"],
        Duration::from_secs(3600),
    );
    let out = within(
        Duration::from_secs(20),
        "вызов с фоновым потомком",
        DirectRunner.run(s),
    )
    .await
    .expect("запуск");
    assert_eq!(out.exit, Exit::Code(0));
    assert!(out.background_killed, "{out:?}");
    let p = pids(&out.stdout);
    // Главный процесс и хотя бы потомок, державший вывод (внук мог не успеть до убийства).
    assert!(p.len() >= 2, "{:?}", out.stdout);
    all_dead(&p, "фоновые потомки").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn capture_is_capped() {
    let project = dir("capped");
    let n = (20 * 1024 * 1024).to_string();
    let s = spec(&project, &["--spam", &n], scaled(Duration::from_secs(60)));
    let out = within(
        Duration::from_secs(60),
        "20 МиБ вывода",
        DirectRunner.run(s),
    )
    .await
    .expect("запуск");
    assert_eq!(out.exit, Exit::Code(0));
    assert!(out.stdout.len() <= CAPTURE_LIMIT);
    assert_eq!(out.stdout.len(), CAPTURE_LIMIT, "предел заполнен целиком");
    assert!(out.stdout_truncated && !out.stderr_truncated);
}

// ---- Смерть родителя ----

/// Родитель (`za_jail_tree --runner`) запускает через `DirectRunner` дерево глубины 2.
fn spawn_parent(project: &Path, leftover: &Path, pid_file: &Path) -> std::process::Child {
    std::process::Command::new(tree())
        .args(["--runner"])
        .arg(leftover)
        .args(["--depth", "2", "--hold", "--pid-file"])
        .arg(pid_file)
        .current_dir(project)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("родитель")
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn leftovers_reaped_after_parent_and_leader_death() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let project = dir("reap");
    let procs = project.join(".zero-agent/work/procs");
    std::fs::create_dir_all(&procs).unwrap();
    let leftover = procs.join("run-1-0.json");
    let pid_file = project.join("pids.txt");
    let mut parent = spawn_parent(&project, &leftover, &pid_file);
    let p = wait_pid_file(&pid_file, 3).await;
    let rec: serde_json::Value = within(Duration::from_secs(20), "файл остатка", async {
        loop {
            if let Ok(b) = tokio::fs::read(&leftover).await
                && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b)
            {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let pgid = i32::try_from(rec["pgid"].as_i64().expect("pgid")).unwrap();
    assert!(rec["starttime"].as_u64().is_some());
    // SIGKILL родителю: его `Drop` не выполняется, группа остаётся.
    parent.kill().unwrap();
    parent.wait().unwrap();
    // Лидер группы тоже убит: остатки находятся по pgrp, а не по лидеру.
    kill(Pid::from_raw(pgid), Signal::SIGKILL).unwrap();
    let killed = tokio::task::spawn_blocking({
        let project = project.clone();
        move || DirectRunner::reap_leftovers(&project)
    })
    .await
    .unwrap();
    assert!(killed >= 1, "убито {killed}");
    all_dead(&p, "остатки дерева").await;
    assert!(!leftover.exists(), "файл остатка удалён");
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn parent_death_closes_job() {
    let project = dir("parent_death");
    let pid_file = project.join("pids.txt");
    let mut parent = spawn_parent(&project, &project.join("unused.json"), &pid_file);
    let p = wait_pid_file(&pid_file, 3).await;
    parent.kill().unwrap();
    parent.wait().unwrap();
    // Без reap: Job закрывается вместе с родителем (`KILL_ON_JOB_CLOSE`).
    all_dead(&p, "дерево после смерти родителя").await;
    assert_eq!(DirectRunner::reap_leftovers(&project), 0);
}
