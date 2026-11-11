//! Capability нативных инструментов (R1-C1 D74, D93, D97): `read`, `search`, `glob`,
//! `edit`, `multi_edit`, `bash` одним набором для run.
//!
//! `request_grant` сюда не входит: это встроенный инструмент цикла (`za-agent`), он
//! открывает прерывание одобрения и дописывает грант в состояние run. Инструкций у
//! capability нет (PRM-005): правила — в описаниях и схемах инструментов, механизм
//! инструкций на top small (D93) нужен скиллам R1-C2.

use std::sync::Arc;

use za_core::Capability;
use za_jail::Runner;

use crate::{
    AgentHome, ReadTracker, Shell, bash_tool, edit_tool, glob_tool, multi_edit_tool, read_tool,
    search_tool,
};

/// Имя capability нативных инструментов.
pub const NATIVE_CAPABILITY: &str = "native";

/// Зависимости нативных инструментов одного процесса: запуск команд (`bash`, только
/// через [`Runner`], SEC-006), общий трекер чтений (`read` → `edit`, D87), оболочка
/// (D85) и каталог агента в `ZA_HOME` (маркеры правок, реестр групп процессов).
#[derive(Clone)]
pub struct NativeDeps {
    pub runner: Arc<dyn Runner>,
    pub tracker: Arc<ReadTracker>,
    pub shell: Shell,
    pub home: AgentHome,
}

impl std::fmt::Debug for NativeDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeDeps")
            .field("shell", &self.shell)
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

/// Capability `native`: инструменты D74 кроме `request_grant`, по имени; без инструкций
/// и хуков.
pub fn native_capability(d: &NativeDeps) -> Capability {
    let mut tools = vec![
        read_tool(Arc::clone(&d.tracker)),
        search_tool(),
        glob_tool(),
        edit_tool(Arc::clone(&d.tracker), d.home.clone()),
        multi_edit_tool(Arc::clone(&d.tracker), d.home.clone()),
        bash_tool(Arc::clone(&d.runner), d.shell.clone(), d.home.clone()),
    ];
    tools.sort_by(|a, b| a.spec().schema.name.cmp(&b.spec().schema.name));
    Capability {
        name: NATIVE_CAPABILITY.to_owned(),
        tools,
        instructions: None,
        ..Capability::default()
    }
}
