//! Каталог агента в `ZA_HOME` для одного проекта ([`AgentHome`]): общий для `edit`
//! (маркеры повтора) и `bash` (реестр групп процессов).

use std::path::PathBuf;

/// `ZA_HOME` и проект: файлы агента вне проекта — `za_home/projects/<project_id>/<sub>`
/// (через `za_core::paths::home_dir`). У `edit` это маркеры повтора `edits` (m11 задачи
/// 7): вне проекта их не подделает `bash`. У `bash` — реестр групп процессов `procs`
/// (D84). `ZA_HOME` должен существовать; `project_id` — идентификатор проекта журнала
/// (`zero-agent::home::project_id`), без `/`, `\` и `:`. Тот же `ZA_HOME` защищает от
/// правки разрешение пути (D79).
#[derive(Debug, Clone)]
pub struct AgentHome {
    pub za_home: PathBuf,
    pub project_id: String,
}
