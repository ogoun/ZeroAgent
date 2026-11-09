//! Библиотечная часть бинарника zero-agent: каталог данных, открытие журнала, конфигурация
//! (`config.toml`), команды моделей (`models`, `ask`), ресурсов (`resources`) и `doctor`.

pub mod config;
pub mod doctor;
pub mod home;
pub mod journal;
pub mod models;
pub mod resources;
