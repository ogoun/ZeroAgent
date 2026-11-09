//! Ресурсы узла zero-agent: снимки CPU, RAM, GPU/VRAM, томов и занятого агентом места.
//!
//! Из крейтов `za-*` зависит только от `za-protocol` (D1). CPU, RAM и тома — `sysinfo` (D7),
//! GPU — процесс `nvidia-smi` (D6). Время — `tokio::time::Instant`.

mod config;
mod gpu;
mod host;
mod monitor;
mod snapshot;
mod volumes;

pub use config::{MonitorConfig, ResourcesConfig};
pub use gpu::{
    GPU_NOT_POLLED, GPU_RETRY_UNAVAILABLE, next_gpu_poll, parse_nvidia_smi_csv, query_nvidia_smi,
};
pub use monitor::{GPU_QUERY_TIMEOUT, ResourceMonitor, host_sample_once};
pub use snapshot::{
    CategoryUsage, CpuSample, DataCategory, DiskKind, GpuSample, GpuStatus, MemSample,
    ResourceSnapshot, VolumeSample,
};
pub use volumes::{
    CATEGORY_MAX_ENTRIES, default_category_paths, dir_size, measure_categories, volume_for,
};
