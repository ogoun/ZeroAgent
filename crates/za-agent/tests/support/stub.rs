//! Заглушки нативных инструментов с контрактом D74 (R1-C1, D99 вариант (а)): `read`,
//! `edit`, `bash` и инструмент с текстами для хэша промптов. Настоящие `za-tools` в
//! тестах `za-agent` не используются — их сквозные тесты в `zero-agent/tests`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use za_core::shell::{self, Dialect};
use za_core::{
    Access, Approval, Checkpoint, CheckpointRequest, Checkpointer, Postcondition, RawPath,
    RiskClass, Subjects, Tool, ToolCx, ToolKind, ToolOutput, ToolSpec,
};
use za_llm::{BoxFuture, ToolSchema};
use za_protocol::{EffectClass, RunRole, Tier};

/// Спецификация по строке таблицы D74.
pub fn d74_spec(name: &str) -> ToolSpec {
    let (kind, class, risk, timeout, limit) = match name {
        "read" => (
            ToolKind::Read,
            EffectClass::ReadOnly,
            RiskClass::Read,
            30,
            128 * 1024,
        ),
        "edit" => (
            ToolKind::Edit,
            EffectClass::Idempotent,
            RiskClass::LocalWrite,
            30,
            64 * 1024,
        ),
        "bash" => (
            ToolKind::Exec,
            EffectClass::NonIdempotent,
            RiskClass::Exec,
            630,
            64 * 1024,
        ),
        other => panic!("нет строки D74 для {other}"),
    };
    ToolSpec {
        schema: ToolSchema {
            name: name.into(),
            description: format!("stub {name}"),
            parameters: json!({"type": "object"}),
        },
        class,
        kind,
        approval: Approval::Mode,
        risk,
        roles: vec![RunRole::Top, RunRole::Orchestrator, RunRole::Worker],
        min_tier: Tier::Small,
        timeout: Duration::from_secs(timeout),
        output_limit: Some(limit),
        tainted: false,
    }
}

/// Путь вызова: разрешённый пакетом (`ToolCx::paths`) или от корня проекта.
fn target(cx: &ToolCx, args: &Value) -> PathBuf {
    cx.paths.first().map_or_else(
        || {
            cx.project_dir
                .join(args["path"].as_str().unwrap_or_default())
        },
        |p| p.path.clone(),
    )
}

/// `read(path)`: содержимое файла; счётчик вызовов.
pub struct ReadStub {
    pub spec: ToolSpec,
    pub calls: Arc<AtomicUsize>,
}

impl ReadStub {
    pub fn new() -> Self {
        Self {
            spec: d74_spec("read"),
            calls: Arc::default(),
        }
    }
}

impl Tool for ReadStub {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn subjects(&self, args: &Value) -> Subjects {
        Subjects {
            paths: vec![RawPath {
                arg: args["path"].as_str().unwrap_or_default().to_owned(),
                access: Access::Read,
            }],
            ..Subjects::default()
        }
    }

    fn call(&self, args: Value, cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let p = target(&cx, &args);
        Box::pin(async move {
            match std::fs::read_to_string(&p) {
                Ok(t) => ToolOutput::ok(t),
                Err(e) => ToolOutput::error(format!("not found: {e}")),
            }
        })
    }
}

/// `edit(path, old, new)`: замена первого вхождения; маркер повтора по ключу эффекта
/// (как D88): повтор после сбоя не правит файл второй раз. `applied` — число правок.
pub struct EditStub {
    pub spec: ToolSpec,
    pub applied: Arc<AtomicUsize>,
}

impl EditStub {
    pub fn new() -> Self {
        Self {
            spec: d74_spec("edit"),
            applied: Arc::default(),
        }
    }
}

impl Tool for EditStub {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn subjects(&self, args: &Value) -> Subjects {
        Subjects {
            paths: vec![RawPath {
                arg: args["path"].as_str().unwrap_or_default().to_owned(),
                access: Access::Write,
            }],
            ..Subjects::default()
        }
    }

    fn call(&self, args: Value, cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let p = target(&cx, &args);
        let applied = Arc::clone(&self.applied);
        let marker = cx
            .project_dir
            .join(".zero-agent/work/edits")
            .join(format!("{}.done", cx.effect_key.file_stem()));
        Box::pin(async move {
            let hash = |b: &[u8]| blake3::hash(b).to_hex().to_string();
            if marker.exists() {
                let b = std::fs::read(&p).unwrap_or_default();
                return ToolOutput {
                    post: vec![Postcondition::FileHash {
                        path: p.clone(),
                        blake3: hash(&b),
                    }],
                    ..ToolOutput::ok(format!("edited {} (retry)", p.display()))
                };
            }
            let old = args["old"].as_str().unwrap_or_default();
            let new = args["new"].as_str().unwrap_or_default();
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            let out = if old.is_empty() {
                new.to_owned()
            } else if text.contains(old) {
                text.replacen(old, new, 1)
            } else {
                return ToolOutput::error("old not found");
            };
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).unwrap();
            }
            std::fs::write(&p, &out).unwrap();
            std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
            std::fs::write(&marker, b"").unwrap();
            applied.fetch_add(1, Ordering::SeqCst);
            ToolOutput {
                post: vec![Postcondition::FileHash {
                    path: p.clone(),
                    blake3: hash(out.as_bytes()),
                }],
                ..ToolOutput::ok(format!("edited {}", p.display()))
            }
        })
    }
}

/// Исполнение команды заглушкой `bash`: по тексту команды и контексту вызова.
pub type Script = Arc<dyn Fn(&str, &ToolCx) -> ToolOutput + Send + Sync>;

/// `bash(cmd, cwd?)`: разбор команды лексером `za_core::shell` (пути, сеть); исполнение —
/// [`Script`]. По умолчанию `exit N` даёт `exit code: N`, иначе `exit code: 0`.
pub struct BashStub {
    pub spec: ToolSpec,
    pub script: Script,
    /// Команды, полученные инструментом (после маскировки).
    pub seen: Arc<Mutex<Vec<String>>>,
}

impl BashStub {
    pub fn new() -> Self {
        Self::with(Arc::new(|cmd: &str, _cx: &ToolCx| {
            let code = cmd
                .strip_prefix("exit ")
                .and_then(|n| n.trim().parse::<i32>().ok())
                .unwrap_or(0);
            ToolOutput::ok(format!("exit code: {code}"))
        }))
    }

    pub fn with(script: Script) -> Self {
        Self {
            spec: d74_spec("bash"),
            script,
            seen: Arc::default(),
        }
    }
}

impl Tool for BashStub {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn subjects(&self, args: &Value) -> Subjects {
        let cmd = args["cmd"].as_str().unwrap_or_default();
        let segs = shell::segments(cmd, Dialect::Bash);
        let paths = segs
            .iter()
            .flat_map(|s| shell::path_words(s, Dialect::Bash))
            .map(|arg| RawPath {
                arg,
                access: Access::Read,
            })
            .collect();
        Subjects {
            paths,
            cwd: args["cwd"].as_str().map(str::to_owned),
            command: Some(cmd.to_owned()),
            network: segs.iter().any(|s| shell::is_network(s, Dialect::Bash)),
            irreversible: Vec::new(),
            segments: segs,
        }
    }

    fn call(&self, args: Value, cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let cmd = args["cmd"].as_str().unwrap_or_default().to_owned();
        self.seen.lock().unwrap().push(cmd.clone());
        let out = (self.script)(&cmd, &cx);
        Box::pin(async move { out })
    }
}

/// Инструмент с текстами для хэша промптов (D94) и выводом по аргументу `out` с
/// необязательным `digest`.
pub struct TextStub {
    pub spec: ToolSpec,
}

impl TextStub {
    pub fn new(name: &str) -> Self {
        let mut spec = d74_spec("read");
        spec.schema.name = name.into();
        Self { spec }
    }
}

impl Tool for TextStub {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            ToolOutput {
                digest: args["digest"].as_str().map(str::to_owned),
                ..ToolOutput::ok(args["out"].as_str().unwrap_or_default())
            }
        })
    }

    fn texts(&self) -> Vec<(&'static str, String)> {
        vec![("runtime/stub.text", "stub text".to_owned())]
    }
}

/// Чекпоинт-заглушка: запоминает запросы, отвечает видом `files`.
#[derive(Default)]
pub struct CheckpointStub {
    pub requests: Mutex<Vec<CheckpointRequest>>,
}

impl Checkpointer for CheckpointStub {
    fn create(&self, req: CheckpointRequest) -> BoxFuture<'static, Checkpoint> {
        let id = format!(
            "{}/{}",
            req.run,
            req.effect_key
                .file_stem()
                .rsplit('-')
                .next()
                .unwrap_or_default()
        );
        self.requests.lock().unwrap().push(req);
        Box::pin(async move {
            Checkpoint {
                id,
                kind: "files".into(),
                git_ref: None,
                commit: None,
                reason: None,
            }
        })
    }
}

/// Ссылка на каталог: Unix — symlink, Windows — junction (`mklink /J`). `false` — ОС не
/// дала создать (без прав); при `ZA_REQUIRE_LINK_TESTS=1` — паника.
pub fn make_dir_link(link: &Path, target: &Path) -> bool {
    #[cfg(windows)]
    let ok = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .is_ok_and(|o| o.status.success());
    #[cfg(unix)]
    let ok = std::os::unix::fs::symlink(target, link).is_ok();
    if !ok && std::env::var("ZA_REQUIRE_LINK_TESTS").as_deref() == Ok("1") {
        panic!("ссылка {} не создана", link.display());
    }
    ok
}
