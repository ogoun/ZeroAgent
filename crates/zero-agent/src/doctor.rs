//! `zero-agent doctor`: проверка окружения (OPS-014, частично).
//!
//! Этап R0 проверяет встроенную SQLite (ARC-031) и доступность llama.cpp
//! (`LLAMA_URL`) и показывает версию компилятора. Внешние системы
//! необязательны: их недоступность — предупреждение, не отказ. Проверка
//! памяти ZeroAIMemory появится вместе с её клиентом (задача 9).

use serde::Serialize;

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
    /// Компилятор сборки и MSRV.
    pub rust: String,
}

impl DoctorReport {
    /// Есть ли отказ: `doctor` завершается с кодом 1.
    pub fn has_failure(&self) -> bool {
        [&self.sqlite, &self.llama]
            .iter()
            .any(|c| matches!(c, Check::Fail(_)))
    }

    /// Человекочитаемый вывод.
    pub fn render(&self) -> String {
        let mut out = String::from("zero-agent doctor\n");
        for (name, c) in [("sqlite", &self.sqlite), ("llama", &self.llama)] {
            out.push_str(&format!("  {name:<7} {:<5} {}\n", c.label(), c.detail()));
        }
        out.push_str(&format!("  {:<7} {:<5} {}\n", "rust", "", self.rust));
        out
    }
}

/// Собирает отчёт; `llama_url` — значение `LLAMA_URL`.
pub async fn run(llama_url: Option<&str>) -> DoctorReport {
    DoctorReport {
        sqlite: check_sqlite(za_store::sqlite_library_version(), za_store::MIN_SQLITE),
        llama: check_llama(llama_url).await,
        rust: rust_version(),
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
    fn failure_only_from_fail() {
        let mut r = DoctorReport {
            sqlite: Check::Ok("3.54.1".to_owned()),
            llama: Check::Warn("нет".to_owned()),
            rust: rust_version(),
        };
        assert!(!r.has_failure());
        r.sqlite = Check::Fail("старая".to_owned());
        assert!(r.has_failure());
    }

    #[test]
    fn json_shape() {
        let r = DoctorReport {
            sqlite: Check::Ok("3.54.1".to_owned()),
            llama: Check::Warn("нет".to_owned()),
            rust: "rustc 1.99.1".to_owned(),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "sqlite": {"status": "ok", "detail": "3.54.1"},
                "llama": {"status": "warn", "detail": "нет"},
                "rust": "rustc 1.99.1",
            })
        );
    }
}
