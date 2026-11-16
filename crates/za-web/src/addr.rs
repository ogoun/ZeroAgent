//! Фильтр адресов (D108) и разбор URL (D107, D124).
//!
//! **Разрешающий список.** Разрешён только глобально маршрутизируемый адрес: IPv4 вне всех
//! блоков реестра IANA IPv4 Special-Purpose (целиком, в том числе с «Globally Reachable =
//! True») и вне `224.0.0.0/4`; IPv6 — только `2000::/3` вне всех блоков реестра IANA IPv6
//! Special-Purpose. Новый блок реестра правки кода не требует: неизвестное запрещено.
//! Вложенный IPv4 (`::ffff:0:0/96`, `64:ff9b::/96`, `2002::/16`) проверяется правилом IPv4.
//! Локальный NAT64 `64:ff9b:1::/48` и устаревший IPv4-compatible `::/96` — отказ целиком.
//!
//! **Где.** [`FilteredResolver`] — резолвер клиента шлюза: соединение идёт только по
//! адресам, которые он пропустил, поэтому проверка и подключение не разделены во времени.
//! Смешанный ответ (хоть один адрес запрещён) — отказ всего имени (признак rebinding).
//! IP-литерал резолвер не видит (`hyper` соединяется сразу) — его проверяет [`parse_url`].

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::Url;
use za_core::net::normalize_host;

use crate::{FetchError, LOCAL_ADDRESS};

/// Дата снимка реестров IANA (поле «Last Updated» обоих реестров; сверено 2026-11-16).
pub const IANA_SNAPSHOT: &str = "2025-10-09";

/// Блок адресов реестра: начало сети, длина префикса, имя строки реестра.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block<A> {
    pub addr: A,
    pub len: u8,
    pub name: &'static str,
}

impl<A: fmt::Display> fmt::Display for Block<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.len)
    }
}

const fn v4(a: u8, b: u8, c: u8, d: u8, len: u8, name: &'static str) -> Block<Ipv4Addr> {
    Block {
        addr: Ipv4Addr::new(a, b, c, d),
        len,
        name,
    }
}

const fn v6(s: [u16; 8], len: u8, name: &'static str) -> Block<Ipv6Addr> {
    Block {
        addr: Ipv6Addr::new(s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]),
        len,
        name,
    }
}

/// IANA IPv4 Special-Purpose Address Registry, все строки
/// (<https://www.iana.org/assignments/iana-ipv4-special-registry/>, [`IANA_SNAPSHOT`]).
#[rustfmt::skip]
pub const IPV4_SPECIAL: &[Block<Ipv4Addr>] = &[
    v4(0, 0, 0, 0, 8, "This network"),
    v4(0, 0, 0, 0, 32, "This host on this network"),
    v4(10, 0, 0, 0, 8, "Private-Use"),
    v4(100, 64, 0, 0, 10, "Shared Address Space"),
    v4(127, 0, 0, 0, 8, "Loopback"),
    v4(169, 254, 0, 0, 16, "Link Local"),
    v4(172, 16, 0, 0, 12, "Private-Use"),
    v4(192, 0, 0, 0, 24, "IETF Protocol Assignments"),
    v4(192, 0, 0, 0, 29, "IPv4 Service Continuity Prefix"),
    v4(192, 0, 0, 8, 32, "IPv4 dummy address"),
    v4(192, 0, 0, 9, 32, "Port Control Protocol Anycast"),
    v4(192, 0, 0, 10, 32, "Traversal Using Relays around NAT Anycast"),
    v4(192, 0, 0, 170, 32, "NAT64/DNS64 Discovery"),
    v4(192, 0, 0, 171, 32, "NAT64/DNS64 Discovery"),
    v4(192, 0, 2, 0, 24, "Documentation (TEST-NET-1)"),
    v4(192, 31, 196, 0, 24, "AS112-v4"),
    v4(192, 52, 193, 0, 24, "AMT"),
    v4(192, 88, 99, 0, 24, "Deprecated (6to4 Relay Anycast)"),
    v4(192, 88, 99, 2, 32, "6a44-relay anycast address"),
    v4(192, 168, 0, 0, 16, "Private-Use"),
    v4(192, 175, 48, 0, 24, "Direct Delegation AS112 Service"),
    v4(198, 18, 0, 0, 15, "Benchmarking"),
    v4(198, 51, 100, 0, 24, "Documentation (TEST-NET-2)"),
    v4(203, 0, 113, 0, 24, "Documentation (TEST-NET-3)"),
    v4(240, 0, 0, 0, 4, "Reserved"),
    v4(255, 255, 255, 255, 32, "Limited Broadcast"),
];

/// Групповые адреса IPv4 (реестр IANA IPv4 Multicast Address Space, RFC 5771): не
/// special-purpose, но и не одноадресные.
const IPV4_MULTICAST: Block<Ipv4Addr> = v4(224, 0, 0, 0, 4, "Multicast");

/// IANA IPv6 Special-Purpose Address Registry, все строки
/// (<https://www.iana.org/assignments/iana-ipv6-special-registry/>, [`IANA_SNAPSHOT`]).
#[rustfmt::skip]
pub const IPV6_SPECIAL: &[Block<Ipv6Addr>] = &[
    v6([0, 0, 0, 0, 0, 0, 0, 1], 128, "Loopback Address"),
    v6([0, 0, 0, 0, 0, 0, 0, 0], 128, "Unspecified Address"),
    v6([0, 0, 0, 0, 0, 0xffff, 0, 0], 96, "IPv4-mapped Address"),
    v6([0x64, 0xff9b, 0, 0, 0, 0, 0, 0], 96, "IPv4-IPv6 Translat."),
    v6([0x64, 0xff9b, 1, 0, 0, 0, 0, 0], 48, "IPv4-IPv6 Translat."),
    v6([0x100, 0, 0, 0, 0, 0, 0, 0], 64, "Discard-Only Address Block"),
    v6([0x100, 0, 0, 1, 0, 0, 0, 0], 64, "Dummy IPv6 Prefix"),
    v6([0x2001, 0, 0, 0, 0, 0, 0, 0], 23, "IETF Protocol Assignments"),
    v6([0x2001, 0, 0, 0, 0, 0, 0, 0], 32, "TEREDO"),
    v6([0x2001, 1, 0, 0, 0, 0, 0, 1], 128, "Port Control Protocol Anycast"),
    v6([0x2001, 1, 0, 0, 0, 0, 0, 2], 128, "Traversal Using Relays around NAT Anycast"),
    v6([0x2001, 1, 0, 0, 0, 0, 0, 3], 128, "DNS-SD Service Registration Protocol Anycast"),
    v6([0x2001, 2, 0, 0, 0, 0, 0, 0], 48, "Benchmarking"),
    v6([0x2001, 3, 0, 0, 0, 0, 0, 0], 32, "AMT"),
    v6([0x2001, 4, 0x112, 0, 0, 0, 0, 0], 48, "AS112-v6"),
    v6([0x2001, 0x10, 0, 0, 0, 0, 0, 0], 28, "Deprecated (previously ORCHID)"),
    v6([0x2001, 0x20, 0, 0, 0, 0, 0, 0], 28, "ORCHIDv2"),
    v6([0x2001, 0x30, 0, 0, 0, 0, 0, 0], 28, "Drone Remote ID Protocol Entity Tags (DETs) Prefix"),
    v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0], 32, "Documentation"),
    v6([0x2002, 0, 0, 0, 0, 0, 0, 0], 16, "6to4"),
    v6([0x2620, 0x4f, 0x8000, 0, 0, 0, 0, 0], 48, "Direct Delegation AS112 Service"),
    v6([0x3fff, 0, 0, 0, 0, 0, 0, 0], 20, "Documentation"),
    v6([0x5f00, 0, 0, 0, 0, 0, 0, 0], 16, "Segment Routing (SRv6) SIDs"),
    v6([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7, "Unique-Local"),
    v6([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10, "Link-Local Unicast"),
];

/// Глобальный одноадресный IPv6 (RFC 4291, 2.4): вне него — отказ целиком.
const IPV6_GLOBAL_UNICAST: Block<Ipv6Addr> = v6([0x2000, 0, 0, 0, 0, 0, 0, 0], 3, "Global Unicast");

fn in_v4(ip: Ipv4Addr, b: &Block<Ipv4Addr>) -> bool {
    let mask = u32::MAX.checked_shl(32 - u32::from(b.len)).unwrap_or(0);
    ip.to_bits() & mask == b.addr.to_bits() & mask
}

fn in_v6(ip: Ipv6Addr, b: &Block<Ipv6Addr>) -> bool {
    let mask = u128::MAX.checked_shl(128 - u32::from(b.len)).unwrap_or(0);
    ip.to_bits() & mask == b.addr.to_bits() & mask
}

/// IPv4, вложенный в IPv6 однозначно и по стандартной трансляции: mapped (`::ffff:0:0/96`),
/// NAT64 с общеизвестным префиксом RFC 6052 (`64:ff9b::/96`), 6to4 (биты 16–47).
///
/// Остальные формы вердикта по вложенному адресу не получают и проверяются правилом IPv6
/// (вне `2000::/3` или в реестре — отказ):
/// - локальный NAT64 RFC 8215 `64:ff9b:1::/48` — целиком, в любой форме (Ruling I1 ревью
///   задачи 6): длину префикса и трансляцию выбирает оператор сети;
/// - IPv4-compatible `::/96` — устарел (RFC 4291, 2.5.5.1), глобально не маршрутизируется
///   (M6 ревью задачи 6).
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let bits = ip.to_bits();
    let low = Ipv4Addr::from_bits(bits as u32);
    match ip.segments() {
        [0, 0, 0, 0, 0, 0xffff, _, _] | [0x64, 0xff9b, 0, 0, 0, 0, _, _] => Some(low),
        [0x2002, ..] => Some(Ipv4Addr::from_bits((bits >> 80) as u32)),
        _ => None,
    }
}

fn check_v4(ip: Ipv4Addr) -> Result<(), &'static str> {
    if IPV4_SPECIAL.iter().any(|b| in_v4(ip, b)) || in_v4(ip, &IPV4_MULTICAST) {
        return Err(LOCAL_ADDRESS);
    }
    Ok(())
}

/// Разрешающий список D108: `Ok` — только глобально маршрутизируемый адрес; иначе
/// `Err("local or private address")`.
pub fn check_ip(ip: IpAddr) -> Result<(), &'static str> {
    match ip {
        IpAddr::V4(v) => check_v4(v),
        IpAddr::V6(v) => {
            if let Some(inner) = embedded_v4(v) {
                return check_v4(inner);
            }
            if !in_v6(v, &IPV6_GLOBAL_UNICAST) || IPV6_SPECIAL.iter().any(|b| in_v6(v, b)) {
                return Err(LOCAL_ADDRESS);
            }
            Ok(())
        }
    }
}

/// Предел длины URL (D107).
const MAX_URL: usize = 2048;

/// Разбор URL для шлюза: только `http`, `https`, без userinfo, не длиннее 2048; хост — через
/// `normalize_host` (D124) до любых сравнений; IP-литерал — [`check_ip`] сразу (резолвер
/// его не увидит), `localhost` и `*.localhost` — отказ лексически. Фрагмент снимается.
/// Порт проверяет `WebGateway::get` по `WebConfig::ports`.
pub fn parse_url(s: &str) -> Result<Url, FetchError> {
    let url = Url::parse(s).map_err(|e| FetchError::InvalidUrl(e.to_string()))?;
    check_url(&url)
}

/// Проверки [`parse_url`] для уже разобранного адреса; результат — адрес с нормализованным
/// хостом и без фрагмента.
pub(crate) fn check_url(url: &Url) -> Result<Url, FetchError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(FetchError::Scheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(FetchError::Credentials);
    }
    if url.as_str().len() > MAX_URL {
        return Err(FetchError::InvalidUrl(format!(
            "URL is longer than {MAX_URL} characters"
        )));
    }
    let invalid = || FetchError::InvalidUrl("invalid host".into());
    let host = normalize_host(url.host_str().ok_or_else(invalid)?).ok_or_else(invalid)?;
    let mut out = url.clone();
    out.set_fragment(None);
    // `normalize_host` отдаёт IP-литерал канонической записью (IPv6 без скобок), домен
    // такой записью не бывает.
    if let Ok(ip) = host.parse::<IpAddr>() {
        check_ip(ip).map_err(|r| FetchError::Blocked(r.into()))?;
        out.set_ip_host(ip).map_err(|()| invalid())?;
    } else {
        if is_localhost(&host) {
            return Err(FetchError::Blocked(LOCAL_ADDRESS.into()));
        }
        out.set_host(Some(&host)).map_err(|_| invalid())?;
    }
    Ok(out)
}

/// `localhost` и `*.localhost` (RFC 6761) — нормализованный хост.
pub(crate) fn is_localhost(host: &str) -> bool {
    host == "localhost" || host.ends_with(".localhost")
}

/// Ошибка резолвера шлюза: её узнаёт `gateway` в цепочке причин ошибки `reqwest`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ResolveFail {
    #[error("{0}")]
    Blocked(&'static str),
    #[error("could not resolve host")]
    NotFound,
}

/// Внутренний резолвер по умолчанию — системный (`tokio::net::lookup_host`).
struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Резолвер клиента шлюза (D108): каждый адрес ответа внутреннего резолвера — через
/// [`check_ip`]; хоть один запрещён — отказ всего имени.
pub(crate) struct FilteredResolver {
    #[cfg(not(feature = "test-hooks"))]
    inner: Arc<dyn Resolve>,
    #[cfg(feature = "test-hooks")]
    hooks: std::sync::RwLock<Hooks>,
}

/// Тестовые подмены (D128): имена списка → заданный адрес мимо фильтра; внутренний резолвер.
#[cfg(feature = "test-hooks")]
struct Hooks {
    inner: Arc<dyn Resolve>,
    names: Vec<(String, SocketAddr)>,
}

#[cfg(not(feature = "test-hooks"))]
impl FilteredResolver {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(SystemResolver),
        }
    }

    fn inner(&self) -> Arc<dyn Resolve> {
        self.inner.clone()
    }
}

#[cfg(feature = "test-hooks")]
impl FilteredResolver {
    pub(crate) fn new() -> Self {
        Self {
            hooks: std::sync::RwLock::new(Hooks {
                inner: Arc::new(SystemResolver),
                names: Vec::new(),
            }),
        }
    }

    /// Имена списка (нормализованные) разрешаются в заданный адрес, фильтр его пропускает.
    pub(crate) fn set_test_names(&self, names: Vec<(String, SocketAddr)>) {
        let names = names
            .into_iter()
            .filter_map(|(n, a)| Some((normalize_host(&n)?, a)))
            .collect();
        self.hooks.write().unwrap_or_else(|e| e.into_inner()).names = names;
    }

    /// Внутренний резолвер под фильтром.
    pub(crate) fn set_inner(&self, inner: Arc<dyn Resolve>) {
        self.hooks.write().unwrap_or_else(|e| e.into_inner()).inner = inner;
    }

    /// Заданный адрес имени из списка.
    fn fixed(&self, host: &str) -> Option<SocketAddr> {
        let h = self.hooks.read().unwrap_or_else(|e| e.into_inner());
        h.names.iter().find(|(n, _)| n == host).map(|(_, a)| *a)
    }

    fn inner(&self) -> Arc<dyn Resolve> {
        self.hooks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .inner
            .clone()
    }
}

impl Resolve for FilteredResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let Some(host) = normalize_host(name.as_str()) else {
            return Box::pin(async { Err(ResolveFail::NotFound.into()) });
        };
        if is_localhost(&host) {
            return Box::pin(async { Err(ResolveFail::Blocked(LOCAL_ADDRESS).into()) });
        }
        #[cfg(feature = "test-hooks")]
        if let Some(a) = self.fixed(&host) {
            return Box::pin(async move { Ok(Box::new(std::iter::once(a)) as Addrs) });
        }
        let inner = self.inner();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = inner
                .resolve(name)
                .await
                .map_err(|_| ResolveFail::NotFound)?
                .collect();
            if addrs.is_empty() {
                return Err(ResolveFail::NotFound.into());
            }
            for a in &addrs {
                check_ip(a.ip()).map_err(ResolveFail::Blocked)?;
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}
