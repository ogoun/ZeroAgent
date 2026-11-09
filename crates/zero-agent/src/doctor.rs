//! `zero-agent doctor`: проверка окружения (OPS-014, частично).
//!
//! Проверяет встроенную SQLite (ARC-031), доступность llama.cpp (`LLAMA_URL`, R0),
//! GPU (`nvidia-smi`, D6) и состояние каждой модели из `config.toml` после первого
//! зонда, показывает версию компилятора. Внешние системы необязательны: их
//! недоступность — предупреждение, не отказ (нет GPU, модель `inactive` или
//! `unreachable` — `Warn`). Отказ — старая SQLite и ошибка конфигурации. Проверка
//! памяти ZeroAIMemory появится вместе с её клиентом.

use std::path::Path;

use serde::Serialize;
use za_llm::EndpointView;
use za_res::{GPU_QUERY_TIMEOUT, GpuStatus, MonitorConfig, ResourcesConfig, query_nvidia_smi};

use crate::config::load_config;
use crate::models::{PROBE_WAIT, start_gateway, state_text};

/// Переменная окружения с адресом `llama-server`.
pub const LLAMA_URL_ENV: &str = "LLAMA_URL";
/// Версия компилятора, которым собран бинарник (см. `build.rs`).
const RUSTC_VERSION: &str = env!("ZA_RUSTC_VERSION");
/// Минимальная поддерживаемая версия Rust из манифеста.
const MSRV: &str = env!("CARGO_PKG_RUST_VERSION");

/// Итог одной проверки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "lowercase")]
pub enum Check {
    /// Всё в порядке.
    Ok(String),
    /// Работать можно, но с ограничениями.
    Warn(String),
    /// Работать нельзя.
    Fail(String),
}

impl Check {
    /// Короткая метка для вывода.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ok(_) => "ok",
            Self::Warn(_) => "warn",
            Self::Fail(_) => "FAIL",
        }
    }

    /// Пояснение.
    pub fn detail(&self) -> &str {
        match self {
            Self::Ok(d) | Self::Warn(d) | Self::Fail(d) => d,
        }
    }
}

/// Отчёт `doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    /// Встроенная SQLite; при `Ok` в пояснении — версия.
    pub sqlite: Check,
    /// `llama-server` по `LLAMA_URL`.
    pub llama: Check,
    /// GPU по `nvidia-smi`; недоступен — `Warn`.
    pub gpu: Check,
    /// Модели по id эндпоинта; `config` — ошибка или предупреждение конфигурации.
    pub models: Vec<(String, Check)>,
    /// Компилятор сборки и MSRV.
    pub rust: String,
}

impl DoctorReport {
    /// Есть ли отказ: `doctor` завершается с кодом 1.
    pub fn has_failure(&self) -> bool {
        [&self.sqlite, &self.llama, &self.gpu]
            .into_iter()
            .chain(self.models.iter().map(|(_, c)| c))
            .any(|c| matches!(c, Check::Fail(_)))
    }

    /// Человекочитаемый вывод.
    pub fn render(&self) -> String {
        let mut out = String::from("zero-agent doctor\n");
        for (name, c) in [
            ("sqlite", &self.sqlite),
            ("llama", &self.llama),
            ("gpu", &self.gpu),
        ] {
            out.push_str(&format!("  {name:<7} {:<5} {}\n", c.label(), c.detail()));
        }
        out.push_str(&format!("  {:<7} {:<5} {}\n", "rust", "", self.rust));
        if self.models.is_empty() {
            out.push_str("  модели: не настроены ([models] в config.toml или LLAMA_URL)\n");
        } else {
            out.push_str("  модели:\n");
        }
        for (id, c) in &self.models {
            out.push_str(&format!("    {id:<16} {:<5} {}\n", c.label(), c.detail()));
        }
        out
    }
}

/// Собирает отчёт; `llama_url` — значение `LLAMA_URL`, `home` — `ZA_HOME` (`None` — не
/// определён: GPU и модели не проверяются, `Warn`). Проверки идут параллельно.
pub async fn run(llama_url: Option<&str>, home: Option<&Path>) -> DoctorReport {
    let (llama, (gpu, models)) = tokio::join!(check_llama(llama_url), check_home(home));
    DoctorReport {
        sqlite: check_sqlite(za_store::sqlite_library_version(), za_store::MIN_SQLITE),
        llama,
        gpu,
        models,
        rust: rust_version(),
    }
}

/// GPU и модели по конфигурации `home`.
async fn check_home(home: Option<&Path>) -> (Check, Vec<(String, Check)>) {
    let Some(home) = home else {
        let w = || Check::Warn("каталог данных не определён (ZA_HOME)".to_owned());
        return (w(), vec![("config".to_owned(), w())]);
    };
    let cfg = match load_config(home) {
        Ok(c) => c,
        Err(e) => {
            // Конфигурация не прочитана: GPU — по `nvidia-smi` из PATH.
            let gpu = gpu_check(home, &ResourcesConfig::default()).await;
            return (gpu, vec![("config".to_owned(), Check::Fail(e.to_string()))]);
        }
    };
    let models = async {
        if cfg.models.as_ref().is_none_or(|m| m.endpoints.is_empty()) {
            return Vec::new();
        }
        match start_gateway(home, &cfg, false).await {
            Ok(s) => {
                s.gateway.wait_probed(PROBE_WAIT).await;
                let view = s.gateway.view();
                let warnings = view
                    .warnings
                    .into_iter()
                    .map(|w| ("config".to_owned(), Check::Warn(w)));
                view.endpoints
                    .iter()
                    .map(|v| (v.id.0.clone(), check_endpoint(v)))
                    .chain(warnings)
                    .collect()
            }
            Err(e) => vec![("config".to_owned(), Check::Fail(e.to_string()))],
        }
    };
    tokio::join!(gpu_check(home, &cfg.resources), models)
}

/// Один вызов `nvidia-smi` (D6) с очищенным окружением и временным каталогом
/// монитора `ZA_HOME/work/tmp/monitor` (D33).
async fn gpu_check(home: &Path, res: &ResourcesConfig) -> Check {
    let mc = MonitorConfig::from_resources(res, home.to_path_buf(), Vec::new(), None);
    let tmp = mc.tmp_dir();
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        tracing::warn!(dir = %tmp.display(), error = %e, "не создан временный каталог монитора");
    }
    check_gpu(&query_nvidia_smi(&mc.nvidia_smi, GPU_QUERY_TIMEOUT, &tmp).await)
}

/// Итог проверки GPU: карты — `Ok`, недоступен (нет NVIDIA, нет `nvidia-smi`) — `Warn`.
pub fn check_gpu(g: &GpuStatus) -> Check {
    match g {
        GpuStatus::Available(cards) if !cards.is_empty() => {
            Check::Ok(crate::resources::render_gpu(g).join("; "))
        }
        GpuStatus::Available(_) => Check::Warn("nvidia-smi не вернул ни одной карты".to_owned()),
        GpuStatus::Unavailable { reason } => Check::Warn(format!("GPU недоступен: {reason}")),
    }
}

/// Итог проверки модели: `ready` — `Ok`, прочие состояния (`inactive`, `unreachable`,
/// `probing`, `open`, `disabled`) — `Warn` с причиной.
pub fn check_endpoint(v: &EndpointView) -> Check {
    if v.state == "ready" {
        let ctx = v
            .context_tokens
            .map(|n| format!(", контекст {n}"))
            .unwrap_or_default();
        let slots = v.slots.map(|n| format!(", слотов {n}")).unwrap_or_default();
        let model = v.model.as_deref().unwrap_or("модель не названа");
        Check::Ok(format!("ready: {model}{ctx}{slots}"))
    } else {
        Check::Warn(state_text(v))
    }
}

/// Версия компилятора сборки и MSRV.
pub fn rust_version() -> String {
    format!("{RUSTC_VERSION} (MSRV {MSRV})")
}

/// Сравнивает версию SQLite с минимальной: `Ok("<версия> (минимум <min>)")` или `Fail`.
pub fn check_sqlite(version: &str, min: &str) -> Check {
    match (parse_version(version), parse_version(min)) {
        (Some(v), Some(m)) if v >= m => Check::Ok(format!("{version} (минимум {min})")),
        (Some(_), Some(_)) => Check::Fail(format!("{version} старше требуемой {min}")),
        _ => Check::Fail(format!("не удалось разобрать версию SQLite {version:?}")),
    }
}

/// `x.y.z` → `(x, y, z)`; недостающие части считаются нулём.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.trim().split('.').map(str::parse::<u32>);
    let major = parts.next()?.ok()?;
    let minor = parts.next().transpose().ok()?.unwrap_or(0);
    let patch = parts.next().transpose().ok()?.unwrap_or(0);
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Зонд `llama-server`: адрес не задан или сервер недоступен — `Warn`, не `Fail`.
pub async fn check_llama(url: Option<&str>) -> Check {
    let Some(url) = url.map(str::trim).filter(|u| !u.is_empty()) else {
        return Check::Warn(format!("{LLAMA_URL_ENV} не задан, llama.cpp не проверялся"));
    };
    match za_llm::probe_llama(url).await {
        Ok(p) => {
            let model = if p.model.is_empty() {
                "модель не названа".to_owned()
            } else {
                p.model
            };
            let slots = p
                .total_slots
                .map(|n| format!(", слотов {n}"))
                .unwrap_or_default();
            Check::Ok(format!("{url}: {model}, n_ctx {}{slots}", p.n_ctx))
        }
        Err(e) => Check::Warn(format!("{url}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_minimum_is_inclusive() {
        assert_eq!(
            check_sqlite("3.51.3", "3.51.3"),
            Check::Ok("3.51.3 (минимум 3.51.3)".to_owned())
        );
        assert!(matches!(check_sqlite("3.54.1", "3.51.3"), Check::Ok(_)));
        assert!(matches!(check_sqlite("4.0", "3.51.3"), Check::Ok(_)));
    }

    #[test]
    fn old_or_garbled_sqlite_fails() {
        assert!(matches!(check_sqlite("3.51.2", "3.51.3"), Check::Fail(_)));
        assert!(matches!(check_sqlite("3.9.99", "3.51.3"), Check::Fail(_)));
        assert!(matches!(check_sqlite("три", "3.51.3"), Check::Fail(_)));
        assert!(matches!(check_sqlite("3.51.3.1", "3.51.3"), Check::Fail(_)));
    }

    #[test]
    fn bundled_sqlite_passes() {
        let c = check_sqlite(za_store::sqlite_library_version(), za_store::MIN_SQLITE);
        assert!(matches!(c, Check::Ok(_)), "{c:?}");
    }

    #[tokio::test]
    async fn llama_unset_is_a_warning() {
        assert!(matches!(check_llama(None).await, Check::Warn(_)));
        assert!(matches!(check_llama(Some("  ")).await, Check::Warn(_)));
    }

    #[test]
    fn gpu_unavailable_is_a_warning() {
        let c = check_gpu(&GpuStatus::Unavailable {
            reason: "nvidia-smi не найден".to_owned(),
        });
        assert!(matches!(c, Check::Warn(_)), "{c:?}");
    }

    #[test]
    fn failure_only_from_fail() {
        let mut r = DoctorReport {
            sqlite: Check::Ok("3.54.1".to_owned()),
            llama: Check::Warn("нет".to_owned()),
            gpu: Check::Warn("нет".to_owned()),
            models: vec![("local".to_owned(), Check::Warn("inactive".to_owned()))],
            rust: rust_version(),
        };
        assert!(!r.has_failure());
        r.models
            .push(("config".to_owned(), Check::Fail("ошибка".to_owned())));
        assert!(r.has_failure());
        r.models.clear();
        r.sqlite = Check::Fail("старая".to_owned());
        assert!(r.has_failure());
    }

    #[test]
    fn json_shape() {
        let r = DoctorReport {
            sqlite: Check::Ok("3.54.1".to_owned()),
            llama: Check::Warn("нет".to_owned()),
            gpu: Check::Ok("RTX".to_owned()),
            models: vec![("local".to_owned(), Check::Ok("ready".to_owned()))],
            rust: "rustc 1.99.1".to_owned(),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "sqlite": {"status": "ok", "detail": "3.54.1"},
                "llama": {"status": "warn", "detail": "нет"},
                "gpu": {"status": "ok", "detail": "RTX"},
                "models": [["local", {"status": "ok", "detail": "ready"}]],
                "rust": "rustc 1.99.1",
            })
        );
    }
}
