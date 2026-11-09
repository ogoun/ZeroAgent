//! Снимок ресурсов узла (serde: перечисления в `snake_case`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Тип носителя тома.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskKind {
    Hdd,
    Ssd,
    Unknown,
}

/// Категория данных агента на диске (RES-016, D42).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataCategory {
    /// `.zero-agent/work` проекта без `spill`, `worktrees`, `browser`.
    ProjectWork,
    ZaHome,
    Models,
    UvCache,
    CargoCache,
    Worktrees,
    Spill,
    BrowserProfiles,
}

/// Загрузка CPU.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CpuSample {
    /// Средняя загрузка всех ядер, 0–100.
    pub usage_percent: f32,
    /// Число логических ядер.
    pub cores: u32,
    /// Сколько миллисекунд подряд загрузка не ниже `cpu_busy_percent`; 0 — ниже.
    pub busy_ms: u64,
}

/// Оперативная память, байты.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemSample {
    pub total: u64,
    pub available: u64,
}

/// Одна видеокарта по данным `nvidia-smi`. `None` — значение недоступно (`[N/A]` под WDDM).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuSample {
    pub index: u32,
    pub name: String,
    pub uuid: String,
    pub util_percent: Option<u32>,
    pub mem_total_mib: u64,
    pub mem_used_mib: Option<u64>,
    pub mem_free_mib: Option<u64>,
    pub temp_c: Option<u32>,
}

/// Состояние GPU узла.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuStatus {
    Available(Vec<GpuSample>),
    /// Нет `nvidia-smi`, другой вендор, ошибка или таймаут; `reason` — для человека.
    Unavailable {
        reason: String,
    },
}

/// Том (точка монтирования).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeSample {
    pub mount: PathBuf,
    pub name: String,
    pub kind: DiskKind,
    /// Байты.
    pub total: u64,
    /// Байты.
    pub available: u64,
    /// Чтение, байт/с; `None` — нет предыдущего замера или платформа не даёт счётчиков.
    pub read_bps: Option<u64>,
    /// Запись, байт/с; `None` — как у `read_bps`.
    pub write_bps: Option<u64>,
    /// Категории данных, лежащие на этом томе.
    pub roles: Vec<DataCategory>,
}

/// Занятое категорией место.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoryUsage {
    pub category: DataCategory,
    pub path: PathBuf,
    /// Байты; `None` — каталога нет или он не читается.
    pub bytes: Option<u64>,
    /// Обход неполный — предел записей (D11) или нечитаемый вложенный каталог:
    /// `bytes` — нижняя оценка.
    pub truncated: bool,
}

/// Снимок ресурсов узла.
///
/// Части снимка опрашиваются независимо (CPU и RAM, GPU, тома, категории), у каждой своя
/// отметка времени: потребитель видит устаревшую часть (например, зависший замер томов).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    /// Время замера CPU и RAM.
    pub sampled_at: jiff::Timestamp,
    pub cpu: CpuSample,
    pub ram: MemSample,
    /// Время замера GPU; `None` — ещё не опрашивался.
    pub gpu_sampled_at: Option<jiff::Timestamp>,
    pub gpu: GpuStatus,
    /// Время замера томов; `None` — ещё не было.
    pub volumes_sampled_at: Option<jiff::Timestamp>,
    pub volumes: Vec<VolumeSample>,
    /// Время обхода категорий; `None` — ещё не было.
    pub categories_sampled_at: Option<jiff::Timestamp>,
    pub categories: Vec<CategoryUsage>,
}
