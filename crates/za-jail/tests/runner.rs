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

/// Время старта процесса (sysinfo), `None` — процесса нет или он зомби.
fn started(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessStatus, ProcessesToUpdate, System};
    let pid = Pid::from_u32(pid);
    let mut sys = System::new().expect("sysinfo");
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid)
        .filter(|p| p.status() != ProcessStatus::Zombie)
        .map(|p| p.start_time())
}

/// Ждёт, пока все `pids` умрут. Процесс мёртв, если его нет, он зомби (Ruling 5.19) или
/// под тем же номером уже другой процесс (другое время старта, ревью задачи 5 M7).
async fn all_dead(pids: &[u32], what: &str) {
    let first: Vec<(u32, Option<u64>)> = pids.iter().map(|p| (*p, started(*p))).collect();
    within(Duration::from_secs(10), what, async {
        loop {
            let alive = first
                .iter()
                .filter(|(p, t)| t.is_some() && started(*p) == *t)
                .count();
            if alive == 0 {
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
            ("LLAMA_URL", "http://u:p@host"),
            ("ZA_JAIL_CALL", "parent-marker"),
        ]),
        &s,
        true,
    );
    assert!(env.iter().all(|(k, _)| {
        let up = k.to_string_lossy().to_ascii_uppercase();
        !up.starts_with("ZA_SECRET")
            && !["ZA_HOME", "DROP_ME", "LLAMA_URL", "ZA_JAIL_CALL"].contains(&up.as_str())
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
fn spec_debug_hides_env_values() {
    let mut s = CommandSpec::new("x", ".", Duration::from_secs(1));
    s.env_set = vec![("TOKEN".into(), "sk-secret-value".into())];
    s.base_env = Some(vec![("ZA_SECRET_K".into(), "base-secret".into())]);
    let d = format!("{s:?}");
    assert!(d.contains("TOKEN=<redacted>"), "{d}");
    assert!(
        !d.contains("sk-secret-value") && !d.contains("base-secret"),
        "{d}"
    );
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
    let file = project.join("pids.txt");
    let file_s = file.display().to_string();
    // С `--pid-file` главный процесс выходит, только когда все три pid записаны.
    let s = spec(
        &project,
        &["--depth", "2", "--background", "--pid-file", &file_s],
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
    assert_eq!(p.len(), 3, "{:?}", out.stdout);
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

/// stdin доходит до процесса целиком (больше буфера канала, без взаимной блокировки с
/// чтением вывода), а `raw_stdout` возвращает байты как есть: `\r\n` и не-UTF-8 не
/// трогаются (чекпоинты git, R1-C1 D89).
#[tokio::test(flavor = "multi_thread")]
async fn stdin_and_raw_stdout_are_exact_bytes() {
    let project = dir("stdin-raw");
    let mut data = b"a\r\nb\n\xff\xfe\x00z".to_vec();
    data.extend(std::iter::repeat_n(b'q', 3 * 1024 * 1024));
    let mut s = spec(&project, &["--cat"], scaled(Duration::from_secs(60)));
    s.stdin = Some(data.clone());
    s.raw_stdout = true;
    let out = within(Duration::from_secs(60), "cat 3 МиБ", DirectRunner.run(s))
        .await
        .expect("запуск");
    assert_eq!(out.exit, Exit::Code(0));
    assert!(out.stdout.is_empty(), "текст не декодируется");
    assert_eq!(out.stdout_raw.as_deref(), Some(data.as_slice()));
    // Без `raw_stdout` — прежнее декодирование, сырых байт нет.
    let mut s = spec(&project, &["--cat"], scaled(Duration::from_secs(60)));
    s.stdin = Some(b"x\r\ny".to_vec());
    let out = within(Duration::from_secs(60), "cat", DirectRunner.run(s))
        .await
        .expect("запуск");
    assert_eq!((out.stdout.as_str(), out.stdout_raw), ("x\ny", None));
}

// ---- Декодирование вывода (Windows) ----

/// Вывод `cmd /u` — UTF-16LE без BOM.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn utf16_output_is_decoded() {
    let project = dir("utf16");
    let mut s = CommandSpec::new("cmd", &project, scaled(Duration::from_secs(20)));
    s.args = ["/u", "/d", "/c", "echo hello"]
        .iter()
        .map(OsString::from)
        .collect();
    let out = within(Duration::from_secs(30), "cmd /u", DirectRunner.run(s))
        .await
        .expect("запуск");
    assert_eq!(out.stdout, "hello\n");
}

/// Вывод `cmd` в OEM-кодировке консоли. На станции с кодовой страницей 866 — кириллица
/// без замен; на прочих страницах проверка пропускается с сообщением.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn oem_output_is_decoded_by_console_code_page() {
    let project = dir("oem");
    let chcp = CommandSpec::new("chcp.com", &project, scaled(Duration::from_secs(20)));
    let cp = within(Duration::from_secs(30), "chcp", DirectRunner.run(chcp))
        .await
        .expect("chcp");
    let page: Option<u32> = cp
        .stdout
        .split(|c: char| !c.is_ascii_digit())
        .rfind(|t| !t.is_empty())
        .and_then(|t| t.parse().ok());
    if page != Some(866) {
        eprintln!("пропуск: кодовая страница консоли {page:?}, не 866");
        return;
    }
    let mut s = CommandSpec::new("cmd", &project, scaled(Duration::from_secs(20)));
    s.args = ["/d", "/c", "echo Привет"]
        .iter()
        .map(OsString::from)
        .collect();
    let out = within(Duration::from_secs(30), "cmd echo", DirectRunner.run(s))
        .await
        .expect("запуск");
    assert_eq!(out.stdout, "Привет\n");
}

// ---- Смерть родителя ----

/// Родитель (`za_jail_tree --runner`) запускает через `DirectRunner` дерево глубины 2.
#[cfg(any(windows, target_os = "linux"))]
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

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn parent_death_closes_job() {
    let project = dir("parent_death");
    let pid_file = project.join("pids.txt");
    let mut parent = spawn_parent(&project, &project.join("unused.json"), &pid_file);
    let p = wait_pid_file(&pid_file, 3).await;
    parent.kill().unwrap();
    let _ = parent.wait();
    // Без reap: Job закрывается вместе с родителем (`KILL_ON_JOB_CLOSE`).
    all_dead(&p, "дерево после смерти родителя").await;
}

/// Вне Linux `reap_leftovers` сразу возвращает 0 и файлов не трогает.
#[cfg(not(target_os = "linux"))]
#[test]
fn reap_is_noop_off_linux() {
    let registry = dir("reap_noop");
    std::fs::write(registry.join("a.json"), "{}").unwrap();
    assert_eq!(DirectRunner::reap_leftovers(&registry), 0);
    assert!(registry.join("a.json").exists());
}

// ---- Linux: реестр групп, subreaper, уход из группы ----

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    use za_jail::Leftover;

    /// Реестр групп как под `ZA_HOME/projects/<id>/procs` (в тесте — внутри своего каталога).
    fn registry(name: &str) -> PathBuf {
        let r = dir(name).join("home/projects/p1/procs");
        std::fs::create_dir_all(&r).unwrap();
        r
    }

    fn boot_id() -> String {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
            .to_string()
    }

    /// `starttime` (поле 22) из `/proc/<pid>/stat`.
    fn starttime(pid: u32) -> u64 {
        let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let rest = &s[s.rfind(')').unwrap() + 1..];
        rest.split_whitespace()
            .nth(22 - 3)
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Живая «жертва» в своей группе: `za_jail_tree --hold`.
    fn victim() -> (std::process::Child, u32) {
        use std::os::unix::process::CommandExt;
        let c = std::process::Command::new(tree())
            .args(["--depth", "0", "--hold"])
            .process_group(0)
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("жертва");
        let pid = c.id();
        (c, pid)
    }

    fn record(pid: u32, start: u64, boot: &str) -> String {
        serde_json::json!({
            "v": 1, "run": "r1", "pgid": pid, "leader_pid": pid,
            "leader_starttime": start, "boot_id": boot, "marker": "m-test",
        })
        .to_string()
    }

    fn reap(dir: &Path) -> usize {
        DirectRunner::reap_leftovers(dir)
    }

    fn alive(pid: u32) -> bool {
        started(pid).is_some()
    }

    fn cleanup(mut c: std::process::Child) {
        let _ = c.kill();
        // Зомби мог собрать subreaper тестового процесса: ошибка ожидания не важна.
        let _ = c.wait();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn leftovers_reaped_after_parent_and_leader_death() {
        let reg = registry("reap");
        let leftover = reg.join("run-1-0.json");
        let project = dir("reap_project");
        let pid_file = project.join("pids.txt");
        let mut parent = spawn_parent(&project, &leftover, &pid_file);
        let p = wait_pid_file(&pid_file, 3).await;
        let rec: serde_json::Value =
            within(Duration::from_secs(20), "запись реестра", async {
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
        assert_eq!(rec["v"], 1);
        assert_eq!(rec["run"], "test-run");
        assert_eq!(rec["leader_pid"], rec["pgid"]);
        assert!(rec["leader_starttime"].as_u64().unwrap() > 0);
        assert_eq!(rec["boot_id"], boot_id().as_str());
        // SIGKILL родителю: его `Drop` не выполняется, группа остаётся.
        parent.kill().unwrap();
        let _ = parent.wait();
        // Лидер группы тоже убит: остатки находятся по группе.
        kill(Pid::from_raw(pgid), Signal::SIGKILL).unwrap();
        let killed = tokio::task::spawn_blocking(move || DirectRunner::reap_leftovers(&reg))
            .await
            .unwrap();
        assert!(killed >= 1, "убито {killed}");
        all_dead(&p, "остатки дерева").await;
        assert!(!leftover.exists(), "запись реестра удалена");
    }

    #[test]
    fn reap_kills_exactly_matching_record() {
        let reg = registry("reap_match");
        let (c, pid) = victim();
        let f = reg.join("a.json");
        std::fs::write(&f, record(pid, starttime(pid), &boot_id())).unwrap();
        assert!(reap(&reg) >= 1);
        assert!(!f.exists());
        cleanup(c);
        assert!(!alive(pid));
    }

    #[test]
    fn reap_ignores_registry_link() {
        let base = dir("reap_link");
        let real = base.join("elsewhere");
        std::fs::create_dir_all(&real).unwrap();
        let (c, pid) = victim();
        std::fs::write(real.join("a.json"), record(pid, starttime(pid), &boot_id())).unwrap();
        std::fs::write(real.join("user.json"), "{\"user\": 1}").unwrap();
        let link = base.join("procs");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(reap(&link), 0);
        assert!(alive(pid), "сигнал через ссылку на каталог");
        assert!(real.join("a.json").exists() && real.join("user.json").exists());
        cleanup(c);
    }

    #[test]
    fn reap_keeps_foreign_json() {
        let reg = registry("reap_foreign");
        let (c, pid) = victim();
        let files = [
            ("garbage.json", "not json".to_string()),
            // Старый формат с `starttime: 0`.
            ("old.json", format!("{{\"pgid\": {pid}, \"starttime\": 0}}")),
        ];
        for (n, t) in &files {
            std::fs::write(reg.join(n), t).unwrap();
        }
        // Ссылка на файл с верной записью — не наш файл.
        let outside = dir("reap_foreign_out").join("rec.json");
        std::fs::write(&outside, record(pid, starttime(pid), &boot_id())).unwrap();
        std::os::unix::fs::symlink(&outside, reg.join("link.json")).unwrap();
        assert_eq!(reap(&reg), 0);
        assert!(alive(pid));
        for (n, _) in &files {
            assert!(reg.join(n).exists(), "{n} удалён");
        }
        assert!(reg.join("link.json").exists() && outside.exists());
        cleanup(c);
    }

    #[test]
    fn reap_without_starttime_sends_no_signal() {
        let reg = registry("reap_zero");
        let (c, pid) = victim();
        let f = reg.join("a.json");
        std::fs::write(&f, record(pid, 0, &boot_id())).unwrap();
        assert_eq!(reap(&reg), 0);
        assert!(alive(pid));
        assert!(
            f.exists(),
            "запись с неизвестным starttime — не наша, не удаляется"
        );
        cleanup(c);
    }

    #[test]
    fn reap_other_boot_sends_no_signal() {
        let reg = registry("reap_boot");
        let (c, pid) = victim();
        let f = reg.join("a.json");
        let other = "00000000-0000-0000-0000-000000000000";
        assert_ne!(other, boot_id());
        std::fs::write(&f, record(pid, starttime(pid), other)).unwrap();
        assert_eq!(reap(&reg), 0);
        assert!(alive(pid));
        // Своя запись прошлой загрузки: удаляется без сигнала.
        assert!(!f.exists());
        cleanup(c);
    }

    #[test]
    fn reap_skips_reused_leader_number() {
        let reg = registry("reap_reused");
        let (c, pid) = victim();
        let f = reg.join("a.json");
        // Номер лидера занят процессом с другим временем старта.
        std::fs::write(&f, record(pid, starttime(pid) - 1, &boot_id())).unwrap();
        assert_eq!(reap(&reg), 0);
        assert!(alive(pid));
        cleanup(c);
    }

    /// Лидер записи мёртв, а номер его группы занят чужой группой (двойной `fork` демона):
    /// её участник без маркера записи не убит (ревью задачи 5, N1).
    #[tokio::test(flavor = "multi_thread")]
    async fn reap_dead_leader_spares_foreign_group() {
        use std::os::unix::process::CommandExt;
        let reg = registry("reap_foreign_group");
        let base = dir("reap_foreign_group_pids");
        let pid_file = base.join("pids.txt");
        // Чужой лидер группы N порождает участника и выходит.
        let mut leader = std::process::Command::new(tree())
            .args(["--depth", "1", "--background", "--pid-file"])
            .arg(&pid_file)
            .process_group(0)
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("чужой лидер");
        let n = leader.id();
        let p = wait_pid_file(&pid_file, 2).await;
        let _ = leader.wait();
        let member = *p.iter().find(|x| **x != n).expect("участник группы");
        assert!(alive(member));
        let f = reg.join("a.json");
        std::fs::write(&f, record(n, 1, &boot_id())).unwrap();
        assert_eq!(reap(&reg), 0);
        assert!(alive(member), "участник чужой группы убит");
        let _ = kill(Pid::from_raw(member as i32), Signal::SIGKILL);
    }

    /// Каталог реестра, открытый на запись другим, — отказ и при чтении, и при записи
    /// (ревью задачи 5, N4).
    #[tokio::test(flavor = "multi_thread")]
    async fn registry_writable_by_others_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let reg = registry("reap_mode");
        std::fs::set_permissions(&reg, std::fs::Permissions::from_mode(0o777)).unwrap();
        let (c, pid) = victim();
        let f = reg.join("a.json");
        std::fs::write(&f, record(pid, starttime(pid), &boot_id())).unwrap();
        assert_eq!(reap(&reg), 0);
        assert!(alive(pid));
        assert!(f.exists());
        cleanup(c);
        let project = dir("reap_mode_project");
        let mut s = spec(&project, &["--exit", "0"], scaled(Duration::from_secs(20)));
        s.leftover = Some(Leftover {
            file: reg.join("b.json"),
            run: "r".into(),
        });
        let err = within(Duration::from_secs(30), "запуск", DirectRunner.run(s))
            .await
            .expect_err("реестр, открытый другим, — отказ");
        assert!(err.to_string().contains("доступен на запись"), "{err}");
        assert!(!reg.join("b.json").exists());
    }

    /// Потомок и внук уходят в свои группы (`setpgid`, как GNU `timeout`): тайм-аут
    /// убивает и их.
    #[tokio::test(flavor = "multi_thread")]
    async fn setpgid_descendant_killed_on_timeout() {
        let project = dir("newpgrp_timeout");
        let s = spec(
            &project,
            &["--depth", "2", "--hold", "--newpgrp"],
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
        assert_eq!(p.len(), 3, "{:?}", out.stdout);
        all_dead(&p, "потомки в своих группах").await;
    }

    /// Главный процесс выходит, потомок в своей группе держит вывод: он усыновлён
    /// агентом-subreaper и убит по маркеру.
    #[tokio::test(flavor = "multi_thread")]
    async fn setpgid_background_child_killed() {
        let project = dir("newpgrp_bg");
        let file = project.join("pids.txt");
        let file_s = file.display().to_string();
        let s = spec(
            &project,
            &[
                "--depth",
                "2",
                "--background",
                "--newpgrp",
                "--pid-file",
                &file_s,
            ],
            Duration::from_secs(3600),
        );
        let out = within(
            Duration::from_secs(20),
            "фоновые потомки",
            DirectRunner.run(s),
        )
        .await
        .expect("запуск");
        assert!(out.background_killed, "{out:?}");
        let p = pids(&out.stdout);
        assert_eq!(p.len(), 3, "{:?}", out.stdout);
        all_dead(&p, "потомки в своих группах").await;
    }

    /// Бросок future: дерево (с уходом из группы) убито, запись реестра удалена.
    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_removes_registry_record() {
        let reg = registry("cancel");
        let project = dir("cancel_project");
        let file = project.join("pids.txt");
        let file_s = file.display().to_string();
        let mut s = spec(
            &project,
            &["--depth", "2", "--hold", "--newpgrp", "--pid-file", &file_s],
            Duration::from_secs(3600),
        );
        let rec = reg.join("run-1-1.json");
        s.leftover = Some(Leftover {
            file: rec.clone(),
            run: "run-1".into(),
        });
        let mut fut = DirectRunner.run(s);
        let p = tokio::select! {
            r = &mut fut => panic!("дерево с --hold завершилось само: {r:?}"),
            p = async {
                let p = wait_pid_file(&file, 3).await;
                within(Duration::from_secs(20), "запись реестра", async {
                    while !rec.exists() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await;
                p
            } => p,
        };
        drop(fut);
        assert!(!rec.exists(), "запись реестра удалена при броске");
        all_dead(&p, "дерево после броска").await;
        assert_eq!(reap(&reg), 0);
    }
}
