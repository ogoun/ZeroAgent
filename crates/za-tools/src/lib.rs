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
//! и `glob` не заходит в ссылки и junction, не читает `.gitignore` родительских каталогов и
//! пропускает `.git` и `.zero-agent`.
//!
//! [`ReadTracker`] помнит, какие файлы прочитаны в run (раздел 4.1 ТЗ 02), [`nearest`]
//! подсказывает похожие имена несуществующего пути (таблица 3.4).

mod fs;
mod nearest;
mod tracker;

pub use fs::{
    WalkLimits, glob_in, glob_split, glob_tool, read_at, read_tool, search_in, search_tool,
};
pub use nearest::nearest;
pub use tracker::{ReadTracker, TrackError};

/// Сколько строк `read` показывает по умолчанию.
pub const READ_DEFAULT_LIMIT: usize = 500;
/// Сколько байт файла `read` читает самое большее.
pub const READ_MAX_BYTES: usize = 4 << 20;
/// Сколько совпадений `search` показывает самое большее.
pub const SEARCH_MAX: usize = 200;
/// Сколько путей `glob` показывает самое большее.
pub const GLOB_MAX: usize = 500;
