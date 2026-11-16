//! Шлюз [`WebGateway`] (D107): HTTP-клиент, редиректы, лимиты, `robots.txt`, журнал
//! обращений.
//!
//! **Клиент** — единственное место построения `reqwest::Client` в продукте вне `za-llm`
//! (D129, `cargo xtask policy`): без прокси (`no_proxy()` — иначе `HTTP_PROXY` и прокси
//! системы разрешали бы имена мимо фильтра), без cookie (функции `cookies` нет), без
//! `Accept-Encoding` (функций сжатия нет), `User-Agent: zero-agent/<версия>`, резолвер —
//! [`FilteredResolver`] (D108), `connect_timeout` из конфигурации, TLS — rustls с
//! webpki-roots.
//!
//! **Вызов** [`WebGateway::get`]: разбор и проверка URL, порт из `WebConfig::ports`, замок
//! хоста (запросы к одному хосту идут по одному: `robots.txt` запрашивается один раз,
//! интервал хоста соблюдается), `robots.txt`, лимитер, запрос, тело до `max_bytes`.
//! Редиректы исполняет сам шлюз, а не `reqwest` (`Policy::none`): каждый переход — своя
//! запись журнала, своя проверка URL и порта, `robots.txt` и место в лимитере. До 5
//! переходов на тот же хост (`http` → `https` можно, обратно — нет); иначе
//! `FetchError::Redirect` — следующий вызов модели снова пройдёт политику.
//!
//! **Дедлайн** соблюдается на каждом шаге (`timeout_at`): при истечении шлюз сам
//! возвращает `Timeout` вместе с записями, собранными до этого, и записью прерванного
//! запроса (`reason = "timeout"`).

use std::collections::HashMap;
#[cfg(feature = "test-hooks")]
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::{CONTENT_ENCODING, CONTENT_TYPE, HeaderMap, LOCATION};
use reqwest::{Client, redirect};
use tokio::time::{Instant, timeout_at};
use url::Url;
use za_core::NetRequest;
use za_core::net::normalize_host;
use za_protocol::net_outcome;

use crate::addr::{FilteredResolver, ResolveFail, check_url};
use crate::convert::non_identity_coding;
use crate::limit::RateLimiter;
use crate::robots::{ROBOTS_MAX_BYTES, Rules};
use crate::{Body, FetchError, Fetched, WebConfig, WebError};

/// Предел переходов по редиректам на тот же хост.
const MAX_REDIRECTS: usize = 5;
/// Срок кэша `robots.txt` на источник.
const ROBOTS_TTL: Duration = Duration::from_secs(24 * 3600);
/// Токен агента в `robots.txt` и `User-Agent`.
const AGENT: &str = "zero-agent";
/// Сколько записей держать в кэшах источников и замков хостов без чистки.
const KEPT: usize = 1024;
/// Отказ `get_provider` (D126).
const NOT_PROVIDER: &str = "not the configured search provider";

/// Сетевой шлюз: один объект на процесс, в `Arc` (D107). `Send + Sync`.
pub struct WebGateway {
    cfg: WebConfig,
    client: Client,
    /// Резолвер клиента — для тестовых подмен; без признака его держит только клиент.
    #[cfg(feature = "test-hooks")]
    resolver: Arc<FilteredResolver>,
    limiter: RateLimiter,
    /// `robots.txt` по источнику (`scheme://host:port`): время получения и правила агента,
    /// разобранные один раз (M4 ревью задачи 6: запрос не разбирает текст заново).
    robots: Mutex<HashMap<String, (Instant, Arc<Rules>)>>,
    /// Замки хостов (нормализованный хост).
    hosts: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// Ответ одного запроса.
struct Resp {
    status: u16,
    /// Абсолютный адрес из `Location` у 301, 302, 303, 307, 308.
    location: Option<Url>,
    content_type: Option<String>,
    bytes: Vec<u8>,
    truncated: bool,
}

/// Исход получения `robots.txt`.
enum Robots {
    /// Правила агента; пустые — разрешено всё (4xx, редирект на другой хост).
    Rules(Arc<Rules>),
    /// 5xx или сбой: всё запрещено (`blocked by robots.txt: unavailable`).
    Unavailable,
}

impl WebGateway {
    /// Шлюз с настройками `cfg`; `requests_per_minute` — не меньше 1.
    pub fn new(cfg: WebConfig) -> Result<Self, WebError> {
        if cfg.requests_per_minute == 0 {
            return Err(WebError::Config(
                "requests_per_minute must be at least 1".into(),
            ));
        }
        let resolver = Arc::new(FilteredResolver::new());
        let client = Client::builder()
            .no_proxy()
            .redirect(redirect::Policy::none())
            .referer(false)
            .connect_timeout(cfg.connect_timeout)
            .user_agent(concat!("zero-agent/", env!("CARGO_PKG_VERSION")))
            .dns_resolver(resolver.clone())
            .build()
            .map_err(|e| WebError::Client(e.to_string()))?;
        Ok(Self {
            limiter: RateLimiter::new(cfg.requests_per_minute, cfg.host_interval),
            cfg,
            client,
            #[cfg(feature = "test-hooks")]
            resolver,
            robots: Mutex::default(),
            hosts: Mutex::default(),
        })
    }

    /// Тест (D128): имена списка разрешаются в заданный адрес, фильтр его пропускает;
    /// прочие имена — через внутренний резолвер под фильтром.
    #[cfg(feature = "test-hooks")]
    pub fn with_test_resolve(self, map: Vec<(String, SocketAddr)>) -> Self {
        self.resolver.set_test_names(map);
        self
    }

    /// Тест (D128): внутренний резолвер, фильтр — поверх него (смешанный ответ).
    #[cfg(feature = "test-hooks")]
    pub fn with_inner_resolver(self, r: Arc<dyn reqwest::dns::Resolve>) -> Self {
        self.resolver.set_inner(r);
        self
    }

    /// `GET url` до дедлайна вызова. Ошибки — в `Fetched::result`, журнал — всегда.
    pub async fn get(&self, url: &Url, deadline: Instant) -> Fetched {
        let mut log = Vec::new();
        let result = self.fetch(url, deadline, &mut log).await;
        Fetched {
            result,
            requests: log,
        }
    }

    /// Запрос к экземпляру поставщика поиска (D126): исключение фильтра и клиент
    /// поставщика — задача 9; до неё отказ `blocked: not the configured search provider`.
    pub async fn get_provider(&self, url: &Url, _deadline: Instant) -> Fetched {
        let e = FetchError::Blocked(NOT_PROVIDER.into());
        Fetched {
            requests: vec![refused(&shown(url), &e)],
            result: Err(e),
        }
    }

    async fn fetch(
        &self,
        start: &Url,
        deadline: Instant,
        log: &mut Vec<NetRequest>,
    ) -> Result<Body, FetchError> {
        let mut url = self
            .admit(start)
            .inspect_err(|e| log.push(refused(&shown(start), e)))?;
        let slot = self.host_slot(url.host_str().unwrap_or_default());
        let Ok(_turn) = timeout_at(deadline, slot.lock()).await else {
            log.push(timed_out(url.as_str()));
            return Err(FetchError::Timeout);
        };
        for hop in 0..=MAX_REDIRECTS {
            if self.cfg.robots {
                self.robots_check(&url, deadline, log).await?;
            }
            let mut rec = record(url.as_str());
            let r = self
                .request(&url, self.cfg.max_bytes, deadline, &mut rec)
                .await;
            finish(&mut rec, &r, &url);
            log.push(rec);
            let resp = r?;
            let Some(target) = resp.location else {
                return Ok(Body {
                    final_url: url,
                    status: resp.status,
                    content_type: resp.content_type,
                    bytes: resp.bytes,
                    truncated: resp.truncated,
                });
            };
            if !same_host(&url, &target) {
                // Адрес для модели — без userinfo и фрагмента (M2 ревью задачи 6).
                return Err(FetchError::Redirect(shown(&target)));
            }
            if hop == MAX_REDIRECTS {
                break;
            }
            url = self
                .admit(&target)
                .inspect_err(|e| log.push(refused(&shown(&target), e)))?;
        }
        Err(FetchError::Network("too many redirects".into()))
    }

    /// Проверки URL ([`check_url`]) и порт из `WebConfig::ports`.
    fn admit(&self, url: &Url) -> Result<Url, FetchError> {
        let u = check_url(url)?;
        let port = u.port_or_known_default().unwrap_or(0);
        if !self.cfg.ports.contains(&port) {
            return Err(FetchError::Blocked(format!("port {port} is not allowed")));
        }
        Ok(u)
    }

    /// `robots.txt` источника `url` разрешает путь; иначе отказ с записью страницы.
    async fn robots_check(
        &self,
        url: &Url,
        deadline: Instant,
        log: &mut Vec<NetRequest>,
    ) -> Result<(), FetchError> {
        let origin = url.origin().ascii_serialization();
        let rules = match self.cached_robots(&origin) {
            Some(r) => r,
            None => match self.fetch_robots(url, deadline, log).await {
                Ok(Robots::Rules(r)) => {
                    self.cache_robots(origin, r.clone());
                    r
                }
                Ok(Robots::Unavailable) => {
                    let e = FetchError::Robots("unavailable");
                    log.push(refused(url.as_str(), &e));
                    return Err(e);
                }
                Err(e) => {
                    log.push(match e {
                        FetchError::Timeout => timed_out(url.as_str()),
                        _ => refused(url.as_str(), &e),
                    });
                    return Err(e);
                }
            },
        };
        let mut path = url.path().to_string();
        if let Some(q) = url.query() {
            path = format!("{path}?{q}");
        }
        if rules.allows(&path) {
            return Ok(());
        }
        let e = FetchError::Robots("disallowed");
        log.push(refused(url.as_str(), &e));
        Err(e)
    }

    /// Получить `robots.txt` (RFC 9309, 2.3.1): 2xx — текст до 500 КиБ; 4xx — всё
    /// разрешено; 5xx и сбой — `Unavailable`. Редирект на тот же хост исполняется (до
    /// пяти); на другой хост, цель, отвергнутая фильтром (запись `blocked`), или дальше
    /// пяти — как 4xx (RFC 9309, 2.3.1.2: «недоступен»). `Err` — только `Timeout` и отказ
    /// фильтра при запросе (`Blocked`): тогда и страница не идёт.
    async fn fetch_robots(
        &self,
        page: &Url,
        deadline: Instant,
        log: &mut Vec<NetRequest>,
    ) -> Result<Robots, FetchError> {
        let mut url = page.clone();
        url.set_path("/robots.txt");
        url.set_query(None);
        for _ in 0..=MAX_REDIRECTS {
            let mut rec = record(url.as_str());
            let r = self
                .request(&url, ROBOTS_MAX_BYTES as u64, deadline, &mut rec)
                .await;
            finish(&mut rec, &r, &url);
            log.push(rec);
            let resp = match r {
                Ok(resp) => resp,
                Err(e @ (FetchError::Timeout | FetchError::Blocked(_))) => return Err(e),
                Err(_) => return Ok(Robots::Unavailable),
            };
            if let Some(target) = resp.location {
                // Другой хост не исполняется: соединения с ним нет (как у страницы).
                if !same_host(&url, &target) {
                    break;
                }
                // Отказ фильтра — запись, как у редиректа страницы (D112, M3 ревью задачи 6).
                match self.admit(&target) {
                    Ok(t) => {
                        url = t;
                        continue;
                    }
                    Err(e) => {
                        log.push(refused(&shown(&target), &e));
                        break;
                    }
                }
            }
            return Ok(match resp.status {
                200..=299 => {
                    let text = robots_text(&resp.bytes, resp.truncated);
                    Robots::Rules(Arc::new(Rules::parse(&text, AGENT)))
                }
                400..=499 => Robots::Rules(Arc::default()),
                _ => Robots::Unavailable,
            });
        }
        Ok(Robots::Rules(Arc::default()))
    }

    /// Один `GET`: место в лимитере, запрос, заголовки, тело до `limit`. Запись `rec`
    /// заполняется по ходу; дедлайн — на каждом шаге.
    async fn request(
        &self,
        url: &Url,
        limit: u64,
        deadline: Instant,
        rec: &mut NetRequest,
    ) -> Result<Resp, FetchError> {
        self.limiter
            .acquire(url.host_str().unwrap_or_default(), deadline)
            .await?;
        let t0 = Instant::now();
        let r = self.exchange(url, limit, deadline, rec).await;
        rec.ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
        r
    }

    async fn exchange(
        &self,
        url: &Url,
        limit: u64,
        deadline: Instant,
        rec: &mut NetRequest,
    ) -> Result<Resp, FetchError> {
        let mut resp = timeout_at(deadline, self.client.get(url.clone()).send())
            .await
            .map_err(|_| FetchError::Timeout)?
            .map_err(|e| send_error(&e))?;
        let status = resp.status().as_u16();
        rec.status = Some(status);
        if matches!(status, 301 | 302 | 303 | 307 | 308)
            && let Some(loc) = resp.headers().get(LOCATION)
        {
            let bad = || FetchError::Network("invalid redirect location".into());
            let target = url
                .join(loc.to_str().map_err(|_| bad())?)
                .map_err(|_| bad())?;
            // В журнал — без userinfo и фрагмента (M2 ревью задачи 6); цель с userinfo
            // дальше отвергнет фильтр.
            rec.final_url = Some(shown(&target));
            return Ok(Resp {
                status,
                location: Some(target),
                content_type: None,
                bytes: Vec::new(),
                truncated: false,
            });
        }
        if let Some(enc) = content_encoding(resp.headers()) {
            return Err(FetchError::Encoding(enc));
        }
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let length = resp.content_length();
        let (mut bytes, mut truncated) = (Vec::new(), false);
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        loop {
            let chunk = timeout_at(deadline, resp.chunk())
                .await
                .map_err(|_| FetchError::Timeout)?
                .map_err(|e| FetchError::Network(body_error(&e)))?;
            let Some(chunk) = chunk else { break };
            let room = limit - bytes.len();
            if chunk.len() > room {
                bytes.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
            rec.bytes = bytes.len() as u64;
            // Ровно в предел: обрезано, если сервер не обещал ровно столько.
            if bytes.len() == limit {
                truncated = length != Some(limit as u64);
                break;
            }
        }
        rec.bytes = bytes.len() as u64;
        Ok(Resp {
            status,
            location: None,
            content_type,
            bytes,
            truncated,
        })
    }

    fn host_slot(&self, host: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= KEPT && !m.contains_key(host) {
            m.retain(|_, s| Arc::strong_count(s) > 1);
        }
        m.entry(host.to_string()).or_default().clone()
    }

    fn cached_robots(&self, origin: &str) -> Option<Arc<Rules>> {
        let m = self.robots.lock().unwrap_or_else(|e| e.into_inner());
        m.get(origin)
            .filter(|(at, _)| at.elapsed() < ROBOTS_TTL)
            .map(|(_, t)| t.clone())
    }

    fn cache_robots(&self, origin: String, rules: Arc<Rules>) {
        let mut m = self.robots.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= KEPT {
            m.retain(|_, (at, _)| at.elapsed() < ROBOTS_TTL);
            if m.len() >= KEPT {
                m.clear();
            }
        }
        m.insert(origin, (Instant::now(), rules));
    }
}

/// Текст `robots.txt`: UTF-8 с заменой; обрезанный — до последней целой строки.
fn robots_text(bytes: &[u8], truncated: bool) -> std::borrow::Cow<'_, str> {
    let bytes = match bytes.iter().rposition(|b| *b == b'\n') {
        Some(i) if truncated => &bytes[..=i],
        _ if truncated => &[],
        _ => bytes,
    };
    String::from_utf8_lossy(bytes)
}

/// Переход допустим: тот же нормализованный хост, `http` → `http`/`https`,
/// `https` → только `https`.
fn same_host(from: &Url, to: &Url) -> bool {
    let host = |u: &Url| u.host_str().and_then(normalize_host);
    host(from).is_some()
        && host(from) == host(to)
        && matches!(
            (from.scheme(), to.scheme()),
            ("http", "http" | "https") | ("https", "https")
        )
}

/// Адрес для журнала: без userinfo и фрагмента.
fn shown(url: &Url) -> String {
    let mut u = url.clone();
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u.set_fragment(None);
    u.to_string()
}

fn record(url: &str) -> NetRequest {
    NetRequest {
        method: "GET".into(),
        url: url.into(),
        outcome: net_outcome::OK.into(),
        ..NetRequest::default()
    }
}

/// Отказ до соединения.
fn refused(url: &str, e: &FetchError) -> NetRequest {
    NetRequest {
        outcome: net_outcome::BLOCKED.into(),
        reason: Some(e.reason()),
        ..record(url)
    }
}

/// Запрос не начат: дедлайн истёк в ожидании.
fn timed_out(url: &str) -> NetRequest {
    NetRequest {
        outcome: net_outcome::ERROR.into(),
        reason: Some(FetchError::Timeout.reason()),
        ..record(url)
    }
}

/// Исход записи по результату запроса. В лог отладки — только хост и исход: строка
/// запроса может нести секрет, маскирует её только запись журнала (D112, M5 ревью задачи 6).
fn finish(rec: &mut NetRequest, r: &Result<Resp, FetchError>, url: &Url) {
    if let Err(e) = r {
        rec.outcome = match e {
            FetchError::Blocked(_) => net_outcome::BLOCKED,
            _ => net_outcome::ERROR,
        }
        .into();
        rec.reason = Some(e.reason());
    }
    tracing::debug!(
        host = url.host_str().unwrap_or_default(),
        status = ?rec.status,
        bytes = rec.bytes,
        outcome = %rec.outcome,
        reason = ?rec.reason,
        "web request"
    );
}

/// Первое кодирование `Content-Encoding`, кроме `identity`, по всем строкам заголовка — то же
/// правило, что у `convert` (D107, D109).
fn content_encoding(h: &HeaderMap) -> Option<String> {
    h.get_all(CONTENT_ENCODING)
        .iter()
        .find_map(|v| non_identity_coding(&String::from_utf8_lossy(v.as_bytes())))
}

/// Ошибка отправки: отказ фильтра — `Blocked`, прочее — `Network` с английским текстом
/// (сообщения ОС бывают локализованы — по ним берётся только вид ошибки).
fn send_error(e: &reqwest::Error) -> FetchError {
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(s) = src {
        if let Some(f) = s.downcast_ref::<ResolveFail>() {
            return match f {
                ResolveFail::Blocked(r) => FetchError::Blocked((*r).into()),
                ResolveFail::NotFound => FetchError::Network(f.to_string()),
            };
        }
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            return FetchError::Network(io_text(io));
        }
        src = s.source();
    }
    FetchError::Network(
        if e.is_timeout() {
            "connection timed out"
        } else if e.is_connect() {
            "connection failed"
        } else {
            "request failed"
        }
        .into(),
    )
}

/// Ошибка чтения тела.
fn body_error(e: &reqwest::Error) -> String {
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(s) = src {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            return format!("error reading response body: {}", io_text(io));
        }
        src = s.source();
    }
    "error reading response body".into()
}

fn io_text(io: &std::io::Error) -> String {
    use std::io::ErrorKind as K;
    let t = match io.kind() {
        K::ConnectionRefused => "connection refused",
        K::ConnectionReset => "connection reset",
        K::ConnectionAborted => "connection aborted",
        K::TimedOut => "connection timed out",
        K::HostUnreachable => "host unreachable",
        K::NetworkUnreachable => "network unreachable",
        K::AddrNotAvailable => "address not available",
        K::UnexpectedEof => "connection closed",
        // Ошибка библиотеки (TLS и подобные) — её текст английский.
        _ if io.raw_os_error().is_none() => {
            return io
                .to_string()
                .chars()
                .filter(|c| c.is_ascii() && !c.is_ascii_control())
                .take(200)
                .collect();
        }
        _ => "connection failed",
    };
    t.into()
}
