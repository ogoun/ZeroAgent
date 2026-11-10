//! Библиотечная часть бинарника zero-agent: каталог данных, открытие журнала, конфигурация
//! (`config.toml`), команды моделей (`models`, `ask`), ресурсов (`resources`), `doctor` и
//! цикла агента (`run`, `answer`, R1-B1), инспектора запроса и метрик run (`inspect`, `stats`,
//! R1-B2).

pub mod config;
pub mod doctor;
pub mod home;
pub mod inspect;
pub mod journal;
pub mod models;
pub mod resources;
pub mod run;
