//! Библиотечная часть бинарника zero-agent: каталог данных, открытие журнала, конфигурация
//! (`config.toml`), команды моделей (`models`, `ask`), ресурсов (`resources`), `doctor` и
//! цикла агента (`run`, `answer`, R1-B1), инспектора запроса и метрик run (`inspect`, `stats`,
//! R1-B2), журнала аудита и выгрузки журнала (`audit`, `journal export`), набора доверенных
//! файлов проекта и доверия к нему (`trust`), чекпоинтов рабочей копии (`checkpoints`),
//! секретов (`secret list`, R1-C1).

pub mod audit;
pub mod checkpoints;
pub mod config;
pub mod doctor;
pub mod home;
pub mod inspect;
pub mod journal;
pub mod models;
pub mod resources;
pub mod run;
pub mod secret;
pub mod trust;

/// Уникальный каталог unit-тестов — общий помощник workspace.
#[cfg(test)]
#[path = "../../../test-support/unique_dir.rs"]
mod test_unique_dir;
