//! Нормализация хоста — одна функция для всех сравнений хостов (R1-C2 D124).
//!
//! Потребители: хосты команд `bash` (`za_shell::network_hosts` отдаёт сырой хост, D104;
//! сбор — [`shell_hosts`]), `web_fetch`, разбор URL шлюза, шаблоны `domain` политики (D105),
//! лексический запрет `localhost`. Расхождение нормализации между ними было бы обходом
//! правила по домену.
//!
//! **`None` = хост неизвестен.** Хост, который [`normalize_host`] не принимает, нельзя
//! пропускать: потребитель считает его неизвестным (`hosts_unknown = true`, у `web_fetch` —
//! отказ). Иначе запись вроде `1.2.3.4.5` или `evil.test..` обходила бы правило по домену.

use url::Host;
use za_shell::{Dialect, Segment, network_hosts};

/// Нормализованный хост: снята одна последняя точка, нижний регистр, IDN — в punycode,
/// IP-литерал — в каноническую запись `url::Host` без скобок (WHATWG: `0x7f.1` →
/// `127.0.0.1`, `[::FFFF:127.0.0.1]` → `::ffff:7f00:1`). `None` — хост неизвестен:
/// пустой, с пустой меткой (`a..b`, `.a`, `evil.test..`) или недопустимый. Пустая метка
/// проверяется и после отображения UTS 46: оно превращает `。` (U+3002) и похожие символы
/// в точку (`a。.test`, `a.test。` — `None`).
pub fn normalize_host(raw: &str) -> Option<String> {
    let s = raw.strip_suffix('.').unwrap_or(raw).to_lowercase();
    if s.is_empty() || (!s.starts_with('[') && s.split('.').any(str::is_empty)) {
        return None;
    }
    let host = Host::parse(&s).ok()?;
    match &host {
        Host::Domain(d) if d.split('.').any(str::is_empty) => None,
        Host::Domain(d) => Some(d.clone()),
        Host::Ipv4(ip) => Some(ip.to_string()),
        // Запись `url` (сжатие нулей, IPv4 внутри — шестнадцатерично), не `std`.
        Host::Ipv6(_) => {
            let t = host.to_string();
            Some(t.trim_start_matches('[').trim_end_matches(']').to_string())
        }
    }
}

/// Хосты команды для правил по домену (D104, D105, D124) — сбор для всех потребителей
/// (`bash::subjects_in`): нормализованные хосты всех сегментов без повторов и признак
/// неопределённого хоста. Сырой хост, для которого [`normalize_host`] даёт `None`, ставит
/// признак, а не пропадает.
pub fn shell_hosts(segs: &[Segment], d: Dialect) -> (Vec<String>, bool) {
    let mut hosts: Vec<String> = Vec::new();
    let mut unknown = false;
    for s in segs {
        let h = network_hosts(s, d);
        unknown |= h.unknown;
        for raw in &h.hosts {
            match normalize_host(raw) {
                Some(n) if !hosts.contains(&n) => hosts.push(n),
                Some(_) => {}
                None => unknown = true,
            }
        }
    }
    (hosts, unknown)
}
