//! Приёмка R0: бинарник `zero-agent` из отдельного процесса.
//!
//! Каждый тест получает свой `ZA_HOME` и каталог проекта внутри
//! `CARGO_TARGET_TMPDIR`. Журнал проекта лежит в
//! `ZA_HOME/projects/<project_id>/journal.db` (ARC-020) и после выхода
//! бинарника открывается тестом напрямую.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::{Value, json};
use za_protocol::{EffectClass, EffectKey, EventKind, RunId, RunRole, RunStatus, Seq};
use za_store::{NewRun, ProjectStore};
use zero_agent::home::project_id;
use zero_agent::journal::open_retrying;

/// Код выхода «нужно решение пользователя».
const NEEDS_DECISION: i32 = 3;

struct Env {
    home: PathBuf,
    project: PathBuf,
}

/// Чистые `ZA_HOME` и каталог проекта для теста.
fn env(name: &str) -> Env {
    env_at(name, "home", "project")
}

fn env_at(name: &str, home: &str, project: &str) -> Env {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("r0-acceptance")
        .join(name);
    let _ = std::fs::remove_dir_all(&root);
    let e = Env {
        home: root.join(home),
        project: root.join(project),
    };
    std::fs::create_dir_all(&e.project).unwrap();
    e
}

impl Env {
    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[])
    }

    /// Запуск бинарника с дополнительными переменными окружения; `ZA_LOG` и
    /// `RUST_LOG` окружения тестов не наследуются.
    fn run_with(&self, args: &[&str], vars: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_zero-agent"));
        cmd.args(args)
            .env("ZA_HOME", &self.home)
            .env_remove("LLAMA_URL")
            .env_remove("ZA_LOG")
            .env_remove("RUST_LOG");
        for (k, v) in vars {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn project_arg(&self) -> &str {
        self.project.to_str().unwrap()
    }

    /// `demo-run --project <dir> --steps <n>` и дополнительные аргументы.
    fn demo(&self, steps: u32, extra: &[&str]) -> Output {
        let steps = steps.to_string();
        let mut args = vec![
            "demo-run",
            "--project",
            self.project_arg(),
            "--steps",
            &steps,
        ];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    fn replay(&self, run: &str) -> Output {
        self.run(&["replay", "--project", self.project_arg(), "--run", run])
    }

    fn journal_dir(&self) -> PathBuf {
        self.home
            .join("projects")
            .join(project_id(&self.project).unwrap())
    }

    /// Журнал проекта; после убитого процесса блокировка на Windows может
    /// сниматься не сразу, поэтому `Locked` ограниченно повторяется.
    fn store(&self) -> ProjectStore {
        open_retrying(&self.journal_dir()).unwrap()
    }

    /// Хэш реплея run; бинарник должен завершиться с кодом 0.
    fn replay_hash(&self, run: &str) -> String {
        let o = self.replay(run);
        assert_eq!(o.status.code(), Some(0), "{o:?}");
        let hash = stdout(&o).trim().to_owned();
        assert_eq!(hash.len(), 64, "{hash:?}");
        hash
    }

    fn log_lines(&self) -> Vec<String> {
        std::fs::read_to_string(self.project.join("log.txt"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

/// id run с первой строки stdout `demo-run`.
fn run_id(o: &Output) -> String {
    let out = stdout(o);
    let first = out.lines().next().unwrap_or_default().trim().to_owned();
    assert!(
        uuid::Uuid::parse_str(&first).is_ok(),
        "первая строка stdout не id run: {out:?}, stderr: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    first
}

fn parse_run(id: &str) -> RunId {
    RunId(uuid::Uuid::parse_str(id).unwrap())
}

fn steps(n: u32) -> Vec<String> {
    (1..=n).map(|i| format!("step {i}")).collect()
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

/// Число событий `effect.intent` по ключу эффекта.
async fn intents(s: &ProjectStore, run: RunId, key: &EffectKey) -> usize {
    s.read_run(run, Seq(0), usize::MAX)
        .await
        .unwrap()
        .iter()
        .filter(|e| e.kind == EventKind::EffectIntent && e.payload["key"] == key.0.as_str())
        .count()
}

/// Сбой `--crash-at <crash>` на 5 шагах, затем `--resume` в чистом проекте;
/// возвращает хэш реплея.
fn crash_and_resume(name: &str, crash: &str) -> String {
    let e = env(name);
    let id = run_id(&e.demo(5, &["--crash-at", crash]));
    let resumed = e.demo(5, &["--resume", &id]);
    assert_eq!(resumed.status.code(), Some(NEEDS_DECISION), "{resumed:?}");
    e.replay_hash(&id)
}

#[test]
fn crash_after_execute_resumes_without_repeating_idempotent_effects() {
    let e = env("crash_execute");
    let crashed = e.demo(5, &["--crash-at", "execute:3"]);
    assert!(
        !crashed.status.success(),
        "аварийное завершение даёт ненулевой код"
    );
    let id = run_id(&crashed);
    assert_eq!(e.log_lines(), steps(3));

    let resumed = e.demo(5, &["--resume", &id]);
    assert_eq!(resumed.status.code(), Some(NEEDS_DECISION), "{resumed:?}");
    assert_eq!(run_id(&resumed), id);
    let run = parse_run(&id);
    let unknown = format!("unk-{}", EffectKey::derive(run, 3, 1).0);
    assert!(stdout(&resumed).contains(&unknown), "{}", stdout(&resumed));
    // Шаги не повторены: файл не изменился.
    assert_eq!(e.log_lines(), steps(3));

    // Реплей после сбоя стабилен: два подряд дают один хэш.
    let hash = e.replay_hash(&id);
    assert_eq!(e.replay_hash(&id), hash);

    // Повтор даёт то же решение, не трогает файл и не меняет хэш.
    let again = e.demo(5, &["--resume", &id]);
    assert_eq!(again.status.code(), Some(NEEDS_DECISION));
    assert!(stdout(&again).contains(&unknown));
    assert_eq!(e.log_lines(), steps(3));
    assert_eq!(e.replay_hash(&id), hash);

    // Тот же сбой и то же продолжение в другом проекте дают тот же хэш.
    assert_eq!(crash_and_resume("crash_execute_other", "execute:3"), hash);

    let s = e.store();
    rt().block_on(async {
        let r = s.get_run(run).await.unwrap().unwrap();
        assert_eq!(r.status, RunStatus::Paused);
        // Намерение `tool` шага 3 без квитанции — исход неизвестен.
        let pending = s.pending_effects(run).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].key, EffectKey::derive(run, 3, 1));
        let ints = s.interrupts(run).await.unwrap();
        assert_eq!(ints.len(), 1);
        assert_eq!(ints[0].id, unknown);
        assert_eq!(ints[0].kind, "unknown_outcome");
        // Идемпотентные `model` выполненных шагов не повторялись.
        for n in 1..=3 {
            assert_eq!(intents(&s, run, &EffectKey::derive(run, n, 0)).await, 1);
            assert_eq!(intents(&s, run, &EffectKey::derive(run, n, 1)).await, 1);
        }
        assert_eq!(intents(&s, run, &EffectKey::derive(run, 4, 0)).await, 0);
    });
}

#[test]
fn crash_after_intent_is_not_silently_retried() {
    let e = env("crash_intent");
    let crashed = e.demo(5, &["--crash-at", "intent:2"]);
    assert!(!crashed.status.success());
    let id = run_id(&crashed);
    assert_eq!(e.log_lines(), steps(1));

    let resumed = e.demo(5, &["--resume", &id]);
    assert_eq!(resumed.status.code(), Some(NEEDS_DECISION));
    // `NonIdempotent` шага 2 не выполнен повторно.
    assert_eq!(e.log_lines(), steps(1));

    let hash = e.replay_hash(&id);
    assert_eq!(e.replay_hash(&id), hash);
    assert_eq!(crash_and_resume("crash_intent_other", "intent:2"), hash);

    let run = parse_run(&id);
    let s = e.store();
    rt().block_on(async {
        let ints = s.interrupts(run).await.unwrap();
        assert_eq!(ints.len(), 1);
        assert_eq!(
            ints[0].id,
            format!("unk-{}", EffectKey::derive(run, 2, 1).0)
        );
        let tool = s.effect(run, &EffectKey::derive(run, 2, 1)).await.unwrap();
        assert!(tool.unwrap().receipt_seq.is_none());
    });
}

#[test]
fn replay_hash_is_identical_across_two_reads() {
    let e = env("replay");
    let done = e.demo(5, &[]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    let id = run_id(&done);
    assert_eq!(e.log_lines(), steps(5));

    let first = e.replay(&id);
    let second = e.replay(&id);
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    let hash = stdout(&first).trim().to_owned();
    assert_eq!(hash.len(), 64, "{hash:?}");
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(stdout(&second).trim(), hash);

    // Повтор завершённого run ничего не меняет и хэш тот же.
    let rerun = e.demo(5, &["--resume", &id]);
    assert_eq!(rerun.status.code(), Some(0));
    assert_eq!(e.log_lines(), steps(5));
    assert_eq!(stdout(&e.replay(&id)).trim(), hash);

    // Тот же сценарий в другом проекте даёт тот же хэш.
    let other = env("replay_other");
    let id2 = run_id(&other.demo(5, &[]));
    assert_ne!(id2, id);
    assert_eq!(stdout(&other.replay(&id2)).trim(), hash);
}

#[test]
fn replay_of_unknown_run_fails() {
    let e = env("replay_unknown");
    assert_eq!(e.demo(1, &[]).status.code(), Some(0));
    let o = e.replay(&RunId::new().to_string());
    assert_eq!(o.status.code(), Some(1));
    let o = e.demo(1, &["--resume", &RunId::new().to_string()]);
    assert_eq!(o.status.code(), Some(1));
}

#[test]
fn cyrillic_path_with_spaces_works() {
    let e = env_at("cyrillic", "домашний каталог", "Рабочий стол/проект 1");
    let done = e.demo(3, &[]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    let id = run_id(&done);
    assert_eq!(e.log_lines(), steps(3));
    assert!(e.journal_dir().join("journal.db").is_file());
    let o = e.replay(&id);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(stdout(&o).trim().len(), 64);
}

#[test]
fn journal_lives_outside_project() {
    let e = env("outside");
    assert_eq!(e.demo(1, &[]).status.code(), Some(0));
    assert!(!e.project.join("journal.db").exists());
    assert!(e.journal_dir().join("journal.db").is_file());
    assert!(e.journal_dir().starts_with(&e.home));
}

/// `doctor --json` при недоступном llama-server: Warn, код 0.
#[test]
fn doctor_warns_when_llama_is_down_and_exits_zero() {
    let e = env("doctor_down");
    let o = Command::new(env!("CARGO_BIN_EXE_zero-agent"))
        .args(["doctor", "--json"])
        .env("ZA_HOME", &e.home)
        .env("LLAMA_URL", "http://127.0.0.1:9")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["sqlite"]["status"], "ok", "{v}");
    assert!(v["sqlite"]["detail"].as_str().unwrap().starts_with("3."));
    assert_eq!(v["llama"]["status"], "warn", "{v}");
    assert!(v["rust"].as_str().unwrap().contains("rustc"), "{v}");
}

/// `doctor` без `LLAMA_URL`: человекочитаемый вывод, Warn, код 0.
#[test]
fn doctor_without_llama_url_exits_zero() {
    let e = env("doctor_unset");
    let o = e.run(&["doctor"]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let out = stdout(&o);
    assert!(out.contains("sqlite"), "{out}");
    assert!(out.contains("LLAMA_URL"), "{out}");
}

/// Golden-хэш реплея демонстрации на 5 шагах: одинаков на любой ОС и при
/// любом времени; меняется только вместе со сценарием или форматом хэша.
#[test]
fn demo_replay_hash_matches_golden() {
    let e = env("golden");
    let done = e.demo(5, &[]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    let actual = e.replay_hash(&run_id(&done));
    let golden = include_str!("golden/demo-replay.hash").trim();
    assert_eq!(
        actual, golden,
        "replay_hash демонстрации изменился; если намеренно, запишите в \
         crates/zero-agent/tests/golden/demo-replay.hash: {actual}"
    );
}

/// Журнал записан с другим `prefix_hash` у `model` шага 1: `--resume`
/// завершается кодом 1 с понятным сообщением, шагов не выполняет и оставляет
/// run в `paused`.
#[test]
fn resume_with_prefix_drift_fails() {
    let e = env("prefix_drift");
    let run = RunId::new();
    {
        let s = e.store();
        rt().block_on(async {
            s.create_run(NewRun {
                id: run,
                project_id: "demo".into(),
                parent_id: None,
                role: RunRole::Top,
                model_profile: None,
            })
            .await
            .unwrap();
            s.set_status(run, RunStatus::Running, 0).await.unwrap();
            let key = EffectKey::derive(run, 1, 0);
            s.record_intent(
                run,
                &key,
                EffectClass::Idempotent,
                "model",
                json!({"step": 1, "prefix_hash": "0".repeat(64)}),
            )
            .await
            .unwrap();
            s.record_receipt(
                run,
                &key,
                json!({"outcome": "ok", "value": {"text": "ответ модели на шаг 1"}}),
            )
            .await
            .unwrap();
        });
    }
    let o = e.demo(2, &["--resume", &run.to_string()]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("расходится с журналом"), "{err}");
    assert!(!e.project.join("log.txt").exists(), "шаги не выполнены");
    let r = rt().block_on(e.store().get_run(run)).unwrap().unwrap();
    assert_eq!(
        r.status,
        RunStatus::Paused,
        "run на паузе после расхождения"
    );
}

/// Логи ядра и хранилища идут в stderr: по умолчанию только `warn` и выше,
/// `ZA_LOG` (или `RUST_LOG`) меняет уровень.
#[test]
fn log_level_comes_from_za_log() {
    let e = env("logging");
    let args = ["demo-run", "--project", e.project_arg(), "--steps", "1"];
    let quiet = e.run_with(&args, &[]);
    assert_eq!(quiet.status.code(), Some(0), "{quiet:?}");
    assert!(
        !String::from_utf8_lossy(&quiet.stderr).contains("журнал открыт"),
        "{quiet:?}"
    );
    let loud = e.run_with(&args, &[("ZA_LOG", "info")]);
    assert_eq!(loud.status.code(), Some(0), "{loud:?}");
    let err = String::from_utf8_lossy(&loud.stderr);
    assert!(err.contains("журнал открыт"), "{err}");
    assert!(!stdout(&loud).contains("журнал открыт"), "логи не в stdout");
    let rust_log = e.run_with(&args, &[("RUST_LOG", "za_store=info")]);
    assert!(String::from_utf8_lossy(&rust_log.stderr).contains("журнал открыт"));
}
