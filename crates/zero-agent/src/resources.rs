//! Команда `resources`: ресурсы узла, очередь к моделям и эндпоинты (UI-046, D32).
//!
//! Данные — `ResourcesView` запущенного `Gateway` с монитором ресурсов. Первые замеры
//! монитора идут в фоне (CPU — через 1 с, GPU — до 5 с на `nvidia-smi`, тома и
//! категории — сразу), поэтому разовый вывод ждёт их не дольше [`FIRST_SAMPLE_WAIT`];
//! не дождавшаяся часть видна по пустой отметке времени (`*_sampled_at: null`).
//! `--watch` печатает вид каждые [`WATCH_EVERY`] до прерывания (Ctrl+C).

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use za_llm::ResourcesView;
use za_res::{GpuStatus, ResourceSnapshot};

use crate::config::load_config;
use crate::models::{CommandError, render_endpoints, start_gateway};

/// Предел ожидания первых замеров всех частей снимка.
pub const FIRST_SAMPLE_WAIT: Duration = Duration::from_secs(10);
/// Не раньше этого срока: первый замер CPU монитор делает через период (1 с).
const FIRST_CPU: Duration = Duration::from_millis(1100);
/// Период вывода `--watch`.
pub const WATCH_EVERY: Duration = Duration::from_secs(2);

/// Все части снимка замерены хотя бы раз.
fn complete(s: &ResourceSnapshot) -> bool {
    s.gpu_sampled_at.is_some()
        && s.volumes_sampled_at.is_some()
        && s.categories_sampled_at.is_some()
}

/// `zero-agent resources [--json] [--watch]`.
pub async fn cmd_resources(home: &Path, json: bool, watch: bool) -> Result<ExitCode, CommandError> {
    let cfg = load_config(home)?;
    let s = start_gateway(home, &cfg, true).await?;
    let started = tokio::time::Instant::now();
    tokio::time::sleep(FIRST_CPU).await;
    while started.elapsed() < FIRST_SAMPLE_WAIT
        && !s.gateway.view().resources.as_ref().is_some_and(complete)
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    loop {
        let view = s.gateway.view();
        if json {
            let text = if watch {
                serde_json::to_string(&view)?
            } else {
                serde_json::to_string_pretty(&view)?
            };
            println!("{text}");
        } else {
            print!("{}", render(&view));
        }
        if !watch {
            return Ok(ExitCode::SUCCESS);
        }
        if !json {
            println!();
        }
        tokio::time::sleep(WATCH_EVERY).await;
    }
}

const MIB: u64 = 1 << 20;
const GIB: f64 = (1u64 << 30) as f64;

fn gib(bytes: u64) -> String {
    format!("{:.1} ГиБ", bytes as f64 / GIB)
}

fn size(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        gib(bytes)
    } else {
        format!("{:.1} МиБ", bytes as f64 / MIB as f64)
    }
}

fn snake<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|x| x.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn opt<T: std::fmt::Display>(v: Option<T>, unit: &str) -> String {
    v.map_or_else(|| "н/д".to_owned(), |x| format!("{x}{unit}"))
}

/// Строки GPU: по карте или причина недоступности.
pub fn render_gpu(g: &GpuStatus) -> Vec<String> {
    match g {
        GpuStatus::Available(cards) => cards
            .iter()
            .map(|c| {
                format!(
                    "{} {}: всего {} МиБ, занято {}, свободно {}, загрузка {}, {}",
                    c.index,
                    c.name,
                    c.mem_total_mib,
                    opt(c.mem_used_mib, " МиБ"),
                    opt(c.mem_free_mib, " МиБ"),
                    opt(c.util_percent, " %"),
                    opt(c.temp_c, " °C"),
                )
            })
            .collect(),
        GpuStatus::Unavailable { reason } => vec![format!("недоступен: {reason}")],
    }
}

/// Вид для человека.
pub fn render(v: &ResourcesView) -> String {
    let mut out = String::new();
    match &v.resources {
        Some(s) => {
            out.push_str(&format!("Ресурсы узла (замер {})\n", s.sampled_at));
            out.push_str(&format!(
                "  CPU  {:.1} % из {} ядер, занят подряд {} мс\n",
                s.cpu.usage_percent, s.cpu.cores, s.cpu.busy_ms
            ));
            out.push_str(&format!(
                "  RAM  свободно {} из {}\n",
                gib(s.ram.available),
                gib(s.ram.total)
            ));
            for line in render_gpu(&s.gpu) {
                out.push_str(&format!("  GPU  {line}\n"));
            }
            if s.volumes_sampled_at.is_none() {
                out.push_str("  Тома: замер не завершён\n");
            }
            for vol in &s.volumes {
                let roles: Vec<String> = vol.roles.iter().map(snake).collect();
                out.push_str(&format!(
                    "  Том  {} ({}, {}): свободно {} из {}{}\n",
                    vol.mount.display(),
                    vol.name,
                    snake(&vol.kind),
                    gib(vol.available),
                    gib(vol.total),
                    if roles.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", roles.join(", "))
                    }
                ));
            }
            if s.categories_sampled_at.is_none() {
                out.push_str("  Категории данных: обход не завершён\n");
            }
            for c in &s.categories {
                out.push_str(&format!(
                    "  Данные {:<16} {}{} {}\n",
                    snake(&c.category),
                    c.bytes.map_or_else(|| "нет".to_owned(), size),
                    if c.truncated {
                        " (не меньше)"
                    } else {
                        ""
                    },
                    c.path.display()
                ));
            }
        }
        None => out.push_str("Ресурсы узла: монитор не запущен\n"),
    }
    out.push_str("Очередь к моделям\n");
    if v.queue.groups.is_empty() {
        out.push_str("  групп нет\n");
    }
    for g in &v.queue.groups {
        out.push_str(&format!(
            "  {} ({}): занято {} из {}, ждут {}{}, потрачено сегодня {:.4} USD\n",
            g.id,
            snake(&g.kind),
            g.active,
            g.max_parallel,
            g.waiting.len(),
            g.cooldown_ms
                .map_or_else(String::new, |ms| format!(", пауза {ms} мс")),
            g.spent_today_usd
        ));
        for w in &g.waiting {
            out.push_str(&format!(
                "    #{} run {} {} ждёт {} мс: {:?}\n",
                w.position,
                w.run_id,
                snake(&w.priority),
                w.waited_ms,
                w.state
            ));
        }
    }
    out.push_str("Модели\n");
    for line in render_endpoints(&v.endpoints, &v.warnings).lines() {
        out.push_str(&format!("  {line}\n"));
    }
    out
}
