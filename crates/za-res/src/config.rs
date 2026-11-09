//! Пороги ресурсов (раздел `[resources]`) и параметры монитора.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Пороги допуска и путь к `nvidia-smi` — единственный источник значений (D10).
///
/// Поля, не указанные в конфигурации, берут значения по умолчанию; неизвестные поля — ошибка.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResourcesConfig {
    /// Запас RAM, МиБ; `None` — `max(2 ГиБ, 10 % RAM)`.
    pub ram_reserve_mib: Option<u64>,
    /// Аварийный пол RAM для `Interactive`, `Top`, `Orchestrator`, МиБ.
    pub ram_floor_mib: u64,
    /// Запас VRAM для фоновых (`Job`, `Arena`), МиБ.
    pub vram_reserve_mib: u64,
    /// Пол VRAM для `Worker`, МиБ.
    pub vram_floor_mib: u64,
    /// Запас места на томе, МиБ.
    pub disk_margin_mib: u64,
    /// Низкое место на томе журнала, МиБ.
    pub journal_low_mib: u64,
    /// CPU занят при загрузке не меньше этого процента…
    pub cpu_busy_percent: f32,
    /// …дольше этого числа секунд.
    pub cpu_busy_for_s: u64,
    /// Гистерезис возобновления: свободно ≥ порог × (1 + hysteresis).
    pub hysteresis: f32,
    /// Исполняемый файл `nvidia-smi` (D6).
    pub nvidia_smi: PathBuf,
}

impl Default for ResourcesConfig {
    fn default() -> Self {
        Self {
            ram_reserve_mib: None,
            ram_floor_mib: 512,
            vram_reserve_mib: 1024,
            vram_floor_mib: 256,
            disk_margin_mib: 5120,
            journal_low_mib: 2048,
            cpu_busy_percent: 95.0,
            cpu_busy_for_s: 10,
            hysteresis: 0.1,
            nvidia_smi: PathBuf::from("nvidia-smi"),
        }
    }
}

/// Недопустимое значение в разделе `[resources]`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ResourcesConfigError {
    /// Гистерезис вне `0..=1` или не число: `inf` дал бы вечную паузу фоновых.
    #[error("resources.hysteresis = {0}: нужно число от 0 до 1")]
    Hysteresis(f32),
    /// `0` — фоновые ждали бы всегда (`busy_ms ≥ 0`).
    #[error("resources.cpu_busy_for_s = 0: нужно не меньше 1")]
    CpuBusyFor,
    #[error("resources.cpu_busy_percent = {0}: нужно число больше 0 и не больше 100")]
    CpuBusyPercent(f32),
}

impl ResourcesConfig {
    /// Проверка значений; вызывается при чтении конфигурации.
    pub fn validate(&self) -> Result<(), ResourcesConfigError> {
        if !(0.0..=1.0).contains(&self.hysteresis) {
            return Err(ResourcesConfigError::Hysteresis(self.hysteresis));
        }
        if self.cpu_busy_for_s == 0 {
            return Err(ResourcesConfigError::CpuBusyFor);
        }
        if !(self.cpu_busy_percent > 0.0 && self.cpu_busy_percent <= 100.0) {
            return Err(ResourcesConfigError::CpuBusyPercent(self.cpu_busy_percent));
        }
        Ok(())
    }

    /// Копия, в которой недопустимые значения заменены умолчаниями (с `warn`).
    pub(crate) fn sanitized(mut self) -> Self {
        let def = Self::default();
        while let Err(e) = self.validate() {
            tracing::warn!(error = %e, "недопустимый порог ресурсов, взято значение по умолчанию");
            match e {
                ResourcesConfigError::Hysteresis(_) => self.hysteresis = def.hysteresis,
                ResourcesConfigError::CpuBusyFor => self.cpu_busy_for_s = def.cpu_busy_for_s,
                ResourcesConfigError::CpuBusyPercent(_) => {
                    self.cpu_busy_percent = def.cpu_busy_percent;
                }
            }
        }
        self
    }
}

/// Параметры монитора. Своих порогов нет — копии из [`ResourcesConfig`].
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorConfig {
    /// Каталог `ZA_HOME`.
    pub za_home: PathBuf,
    /// Корни проектов; их `.zero-agent/work` входит в категории данных.
    pub projects: Vec<PathBuf>,
    /// Каталог моделей (категория `Models`), если задан.
    pub models_dir: Option<PathBuf>,
    /// Исполняемый файл `nvidia-smi`.
    pub nvidia_smi: PathBuf,
    /// Порог занятости CPU для `CpuSample::busy_ms`.
    pub cpu_busy_percent: f32,
    /// Период опроса CPU и RAM.
    pub cpu_ram_every: Duration,
    /// Период опроса GPU, пока он доступен.
    pub gpu_every: Duration,
    /// Период опроса томов.
    pub volumes_every: Duration,
    /// Период обхода категорий данных.
    pub categories_every: Duration,
}

impl MonitorConfig {
    /// Единственный конструктор: пороги из `ResourcesConfig`, периоды по D11
    /// (CPU и RAM — 1 с, GPU — 2 с, тома — 5 с, категории — 300 с).
    pub fn from_resources(
        res: &ResourcesConfig,
        za_home: PathBuf,
        projects: Vec<PathBuf>,
        models_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            za_home,
            projects,
            models_dir,
            nvidia_smi: res.nvidia_smi.clone(),
            cpu_busy_percent: res.cpu_busy_percent,
            cpu_ram_every: Duration::from_secs(1),
            gpu_every: Duration::from_secs(2),
            volumes_every: Duration::from_secs(5),
            categories_every: Duration::from_secs(300),
        }
    }

    /// Временный каталог служебных процессов монитора (D33): `ZA_HOME/work/tmp/monitor`.
    pub fn tmp_dir(&self) -> PathBuf {
        self.za_home.join("work").join("tmp").join("monitor")
    }
}
