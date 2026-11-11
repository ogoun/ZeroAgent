//! Capability нативных инструментов (R1-C1 D74, D93, D94, D97): контракт каждой строки
//! таблицы D74 и golden описаний и текстов.
//!
//! Golden пишутся только с `ZA_UPDATE_GOLDEN=1`:
//! - `tests/golden/native_tools.json` — описания для модели (`za_llm::render_tools`,
//!   тот же канонический JSON, что в префиксе запроса), одинаковые на всех ОС (D97);
//! - `tests/golden/native_texts.json` — тексты результатов и пометок (`Tool::texts`), они
//!   входят в хэш промптов (D94).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use za_core::shell::Dialect;
use za_core::{Approval, Capability, RiskClass, ToolKind};
use za_protocol::{EffectClass, RunRole, Tier};
use za_tools::{AgentHome, NATIVE_CAPABILITY, NativeDeps, ReadTracker, Shell, native_capability};

/// Зависимости без запуска: инструменты здесь не вызываются, нужны только спецификации.
fn deps() -> NativeDeps {
    NativeDeps {
        runner: Arc::new(za_jail::DirectRunner),
        tracker: ReadTracker::new(),
        shell: Shell {
            program: PathBuf::from("no-shell"),
            dialect: Dialect::Bash,
        },
        home: AgentHome {
            za_home: PathBuf::from("no-home"),
            project_id: "0".into(),
        },
    }
}

fn native() -> Capability {
    native_capability(&deps())
}

/// Таблица D74 без `request_grant` (встроенный инструмент цикла): имя, вид, класс, риск,
/// наименьший тир, предел времени (с), предел результата (байты).
const TABLE_D74: &[(&str, ToolKind, EffectClass, RiskClass, Tier, u64, usize)] = &[
    (
        "bash",
        ToolKind::Exec,
        EffectClass::NonIdempotent,
        RiskClass::Exec,
        Tier::Small,
        630,
        64 * 1024,
    ),
    (
        "edit",
        ToolKind::Edit,
        EffectClass::Idempotent,
        RiskClass::LocalWrite,
        Tier::Small,
        30,
        64 * 1024,
    ),
    (
        "glob",
        ToolKind::Read,
        EffectClass::ReadOnly,
        RiskClass::Read,
        Tier::Standard,
        30,
        64 * 1024,
    ),
    (
        "multi_edit",
        ToolKind::Edit,
        EffectClass::Idempotent,
        RiskClass::LocalWrite,
        Tier::Standard,
        30,
        64 * 1024,
    ),
    (
        "read",
        ToolKind::Read,
        EffectClass::ReadOnly,
        RiskClass::Read,
        Tier::Small,
        30,
        128 * 1024,
    ),
    (
        "search",
        ToolKind::Read,
        EffectClass::ReadOnly,
        RiskClass::Read,
        Tier::Small,
        60,
        64 * 1024,
    ),
];

#[test]
fn native_tools_contract() {
    let cap = native();
    assert_eq!(cap.name, NATIVE_CAPABILITY);
    // PRM-005: правила — в описаниях и схемах, инструкций у capability нет (D93).
    assert_eq!(cap.instructions, None);
    assert!(cap.hooks.is_empty());
    let names: Vec<&str> = cap
        .tools
        .iter()
        .map(|t| t.spec().schema.name.as_str())
        .collect();
    let want: Vec<&str> = TABLE_D74.iter().map(|r| r.0).collect();
    assert_eq!(names, want, "набор и порядок по имени");
    for (name, kind, class, risk, tier, secs, limit) in TABLE_D74 {
        let t = cap
            .tools
            .iter()
            .find(|t| t.spec().schema.name == *name)
            .unwrap();
        let s = t.spec();
        assert_eq!(s.kind, *kind, "{name}: kind");
        assert_eq!(s.class, *class, "{name}: class");
        assert_eq!(s.risk, *risk, "{name}: risk");
        assert_eq!(s.min_tier, *tier, "{name}: min_tier");
        assert_eq!(s.timeout, Duration::from_secs(*secs), "{name}: timeout");
        assert_eq!(s.output_limit, Some(*limit), "{name}: output_limit");
        assert!(!s.tainted, "{name}: tainted — по вызову (D82)");
        assert_eq!(s.approval, Approval::Mode, "{name}: approval");
        assert_eq!(
            s.roles,
            [RunRole::Top, RunRole::Orchestrator, RunRole::Worker],
            "{name}: roles"
        );
        // D97: описание — ASCII, одинаковое на всех ОС; схема — объект.
        assert!(s.schema.description.is_ascii(), "{name}");
        assert_eq!(s.schema.parameters["type"], "object", "{name}");
    }
}

/// Сравнивает `actual` с golden `tests/golden/<file>`; с `ZA_UPDATE_GOLDEN=1` — пишет.
fn check_golden(file: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(file);
    if std::env::var("ZA_UPDATE_GOLDEN").as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
    }
    let golden = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; ZA_UPDATE_GOLDEN=1", path.display()))
        .replace("\r\n", "\n");
    assert_eq!(golden, actual, "{file}: обновить — ZA_UPDATE_GOLDEN=1");
}

#[test]
fn native_tools_and_texts_golden() {
    let cap = native();
    // Описания: канонический JSON префикса (`render_tools`), в файле — с отступами.
    let rendered = za_llm::render_tools(&za_llm::PromptBlocks {
        tools: cap.tools.iter().map(|t| t.spec().schema.clone()).collect(),
        ..Default::default()
    });
    let v: Value = serde_json::from_str(&rendered).unwrap();
    check_golden(
        "native_tools.json",
        &format!("{}\n", serde_json::to_string_pretty(&v).unwrap()),
    );
    // Тексты: один golden на все нативные инструменты, имена — `runtime/{fs,edit,bash}.*`.
    let mut all = serde_json::Map::new();
    for tool in &cap.tools {
        let texts = tool.texts();
        assert!(!texts.is_empty(), "{}", tool.spec().schema.name);
        for (name, text) in texts {
            assert!(
                name.starts_with("runtime/fs.")
                    || name.starts_with("runtime/edit.")
                    || name.starts_with("runtime/bash."),
                "{name}"
            );
            assert!(text.is_ascii(), "{name}: {text}");
            assert!(
                all.insert(name.to_owned(), Value::String(text)).is_none(),
                "{name}"
            );
        }
    }
    check_golden(
        "native_texts.json",
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&Value::Object(all)).unwrap()
        ),
    );
}
