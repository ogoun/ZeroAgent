//! Нормализация хоста — одна функция для всех сравнений хостов (R1-C2 D124).
//!
//! Потребители: хосты команд `bash` (`za_shell::network_hosts` отдаёт сырой хост, D104),
//! `web_fetch`, разбор URL шлюза, шаблоны `domain` политики (D105), лексический запрет
//! `localhost`. Расхождение нормализации между ними было бы обходом правила по домену.

use url::Host;

/// Нормализованный хост: снята одна последняя точка, нижний регистр, IDN — в punycode,
/// IP-литерал — в каноническую запись `url::Host` без скобок (WHATWG: `0x7f.1` →
/// `127.0.0.1`, `[::FFFF:127.0.0.1]` → `::ffff:7f00:1`). `None` — пустой или
/// недопустимый хост.
pub fn normalize_host(raw: &str) -> Option<String> {
    let s = raw.strip_suffix('.').unwrap_or(raw).to_lowercase();
    if s.is_empty() {
        return None;
    }
    let host = Host::parse(&s).ok()?;
    match &host {
        Host::Domain(d) if d.is_empty() => None,
        Host::Domain(d) => Some(d.clone()),
        Host::Ipv4(ip) => Some(ip.to_string()),
        // Запись `url` (сжатие нулей, IPv4 внутри — шестнадцатерично), не `std`.
        Host::Ipv6(_) => {
            let t = host.to_string();
            Some(t.trim_start_matches('[').trim_end_matches(']').to_string())
        }
    }
}
