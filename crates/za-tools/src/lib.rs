//! `za-tools` — нативные инструменты zero-agent (R1-C1 D73, D74, D87).
//!
//! `read`, `search`, `glob` ([`read_tool`], [`search_tool`], [`glob_tool`]): контракт по
//! таблице D74 (вид `Read`, класс `ReadOnly`, риск `Read`), описания для модели — на
//! английском (D97), тексты результатов — в [`za_core::Tool::texts`] (хэш промптов, D94).
//!
//! Граница проекта — дело политики: инструмент сообщает предметы вызова
//! ([`za_core::Tool::subjects`], `Access::Read`), а пакет решает о вызове до исполнения
//! (D77, D98). Сам инструмент разбирает путь ([`za_core::paths::classify`]: сетевые и
//! device-пути отвергаются до обращения к ФС), разрешает его и открывает файлы только через
//! [`za_core::paths::open_checked`]. Вся работа с ФС — в `spawn_blocking`. Обход `search`
//! и `glob` не заходит в ссылки и junction и пропускает `.git` и `.zero-agent`. Правила
//! игнорирования (`.gitignore`, `.ignore`, `.git/info/exclude`) он читает сам, через
//! `open_checked`, от корня проекта до каталога поиска; выше корня проекта — нет.
//!
//! [`ReadTracker`] помнит, какие файлы прочитаны в run (раздел 4.1 ТЗ 02), [`nearest`]
//! подсказывает похожие имена несуществующего пути (таблица 3.4).
//!
//! `edit` и `multi_edit` ([`edit_tool`], [`multi_edit_tool`], D88): вид `Edit`, класс
//! `Idempotent`, риск `LocalWrite`. Правят только прочитанный и не изменившийся файл
//! (трекер по каноническому пути), толерантный патчер [`apply`] и защита от перезаписи
//! [`shrink_guard`], запись — атомарная, по каноническому пути, через `za_core::paths`;
//! повтор после сбоя — по маркеру `ZA_HOME/projects/<project_id>/edits/<file_stem>.json`.
//!
//! `bash` ([`bash_tool`], D85): вид `Exec`, класс `NonIdempotent`, риск `Exec` (по вызову —
//! метки необратимости D81 в предметах). Запуск только через `za_jail::Runner`; ненулевой
//! код выхода — данные, ошибка — только сбой запуска и тайм-аут. Предметы вызова — разбор
//! [`za_core::shell`]; решение принимает политика. Сжатие вывода по типу — [`digest()`]
//! (D86).

mod bash;
mod digest;
mod edit;
mod fs;
mod nearest;
mod patch;
mod rules;
mod tracker;
#[cfg(test)]
#[path = "../../../test-support/unique_dir.rs"]
mod unique_dir;

pub use bash::{
    BASH_DEFAULT_TIMEOUT_S, BASH_MAX_TIMEOUT_S, Shell, bash_tool, format_output, powershell_encoded,
};
pub use digest::digest;
pub use edit::{
    EditHome, EditHook, EditPoint, clean_edit_leftovers, clean_leftovers_older, edit_tool,
    edit_tool_with_hook, multi_edit_tool, multi_edit_tool_with_hook,
};

pub use fs::{
    WalkLimits, glob_in, glob_split, glob_tool, read_at, read_tool, search_in, search_tool,
};
pub use nearest::nearest;
pub use patch::{MatchLevel, PatchError, apply, shrink_guard};
pub use tracker::{ReadTracker, TrackError};

/// Сколько строк `read` показывает по умолчанию.
pub const READ_DEFAULT_LIMIT: usize = 500;
/// Сколько байт файла `read` читает самое большее.
pub const READ_MAX_BYTES: usize = 4 << 20;
/// Сколько совпадений `search` показывает самое большее.
pub const SEARCH_MAX: usize = 200;
/// Сколько путей `glob` показывает самое большее.
pub const GLOB_MAX: usize = 500;
