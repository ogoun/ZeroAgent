//! Проверка лимитов фиксированных промптов (PRM-007, R1-B2 D64):
//! `cargo xtask prompt-budget [--llama http://host:port]`.
//!
//! Каждый случай `za_agent::budget_cases()` считается токенизаторами cl100k и o200k
//! (`tiktoken-rs`, словари встроены в крейт — сеть не нужна) и оценкой Qwen
//! ⌈cl100k × 1,05⌉ (приложение A: Qwen на 3–5 % больше cl100k). Превышение любого
//! счёта — код 1. Флаг `--llama` добавляет точный счёт токенизатором целевой модели
//! через `POST /tokenize` llama-server (HTTP/1.1 поверх `TcpStream`, только `http://`);
//! без флага к сети команда не обращается.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::ExitCode;
use std::time::Duration;

use za_agent::budget_cases;

/// Предел подключения, записи и чтения `/tokenize`.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Предел размера ответа `/tokenize`, байты: ответ на промпт в тысячи токенов меньше.
const MAX_RESPONSE: u64 = 16 * 1024 * 1024;

/// Счёт токенов одного текста.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Count {
    pub cl100k: usize,
    pub o200k: usize,
    /// Оценка Qwen: [`Count::qwen_of`] от `cl100k`.
    pub qwen_est: usize,
    /// Точный счёт токенизатором целевой модели (`--llama`).
    pub exact: Option<usize>,
}

impl Count {
    /// Оценка Qwen по cl100k: ⌈n × 1,05⌉ (D64).
    pub fn qwen_of(n: usize) -> usize {
        (n * 105).div_ceil(100)
    }
}

/// Счёт cl100k и o200k (`encode_with_special_tokens`) и оценка Qwen; `exact` — `None`.
pub fn count(text: &str) -> Count {
    let cl100k = tiktoken_rs::cl100k_base_singleton()
        .encode_with_special_tokens(text)
        .len();
    let o200k = tiktoken_rs::o200k_base_singleton()
        .encode_with_special_tokens(text)
        .len();
    Count {
        cl100k,
        o200k,
        qwen_est: Count::qwen_of(cl100k),
        exact: None,
    }
}

/// Любой из счётов (cl100k, o200k, оценка Qwen, точный) выше `limit`.
pub fn over_limit(c: &Count, limit: u32) -> bool {
    let limit = limit as usize;
    [Some(c.cl100k), Some(c.o200k), Some(c.qwen_est), c.exact]
        .into_iter()
        .flatten()
        .any(|n| n > limit)
}

/// Число токенов `text` по `POST {base}/tokenize` llama-server (`{"content": text}` →
/// `{"tokens": [...]}`). `base` — `http://host:port` (путь-префикс допустим); HTTP/1.1
/// с `Connection: close`, тело ответа по `Content-Length`, `chunked` или до закрытия.
pub fn tokenize_http(base: &str, text: &str) -> Result<usize, String> {
    let rest = base
        .strip_prefix("http://")
        .ok_or_else(|| format!("{base}: допустим только http://"))?;
    let (authority, prefix) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].trim_end_matches('/')),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(format!("{base}: нет адреса"));
    }
    let addrs: Vec<_> = authority
        .to_socket_addrs()
        .map_err(|e| format!("{authority}: {e}"))?
        .collect();
    let mut stream = None;
    let mut last = String::from("адрес не найден");
    for a in &addrs {
        match TcpStream::connect_timeout(a, HTTP_TIMEOUT) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = format!("{a}: {e}"),
        }
    }
    let mut stream = stream.ok_or(last)?;
    let io = |e: std::io::Error| format!("{authority}: {e}");
    stream.set_read_timeout(Some(HTTP_TIMEOUT)).map_err(io)?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT)).map_err(io)?;
    let body = serde_json::json!({ "content": text }).to_string();
    let request = format!(
        "POST {prefix}/tokenize HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).map_err(io)?;
    let mut raw = Vec::new();
    stream
        .take(MAX_RESPONSE)
        .read_to_end(&mut raw)
        .map_err(io)?;
    let body = response_body(&raw)?;
    let v: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| format!("/tokenize: ответ не JSON: {e}"))?;
    v.get("tokens")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .ok_or_else(|| "/tokenize: в ответе нет массива tokens".into())
}

/// Тело ответа HTTP/1.1 со статусом 2xx: по `Content-Length`, `chunked` или до конца.
fn response_body(raw: &[u8]) -> Result<Vec<u8>, String> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("/tokenize: ответ без заголовков")?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let rest = &raw[end + 4..];
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default();
    let code = status.split(' ').nth(1).unwrap_or_default();
    if !code.starts_with('2') || code.len() != 3 {
        return Err(format!("/tokenize: {status}"));
    }
    let mut length = None;
    let mut chunked = false;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if k == "content-length" {
            length = Some(
                v.parse::<usize>()
                    .map_err(|e| format!("/tokenize: Content-Length: {e}"))?,
            );
        } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
    }
    if chunked {
        return dechunk(rest);
    }
    match length {
        Some(n) if rest.len() < n => Err(format!(
            "/tokenize: тело короче Content-Length ({} < {n})",
            rest.len()
        )),
        Some(n) => Ok(rest[..n].to_vec()),
        None => Ok(rest.to_vec()),
    }
}

/// Тело `Transfer-Encoding: chunked` (расширения чанков и трейлеры пропускаются).
fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("/tokenize: обрыв chunked")?;
        let line = String::from_utf8_lossy(&rest[..line_end]);
        let size = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size, 16)
            .map_err(|e| format!("/tokenize: размер чанка `{size}`: {e}"))?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if rest.len() < size + 2 {
            return Err("/tokenize: обрыв chunked".into());
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

/// Аргументы команды: `--llama <http://host:port>` или ничего.
fn parse_args(args: &[String]) -> Result<Option<String>, String> {
    match args {
        [] => Ok(None),
        [flag, url] if flag == "--llama" => Ok(Some(url.clone())),
        _ => Err(format!("неизвестные аргументы: {}", args.join(" "))),
    }
}

/// Точка входа команды `prompt-budget`: таблица `случай | cl100k | o200k | qwen≈ |
/// точно | лимит`; код 1 — есть превышение, 2 — ошибка аргументов или `/tokenize`.
pub fn run(args: &[String]) -> ExitCode {
    let llama = match parse_args(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("prompt-budget: {e}");
            eprintln!("использование: cargo xtask prompt-budget [--llama http://host:port]");
            return ExitCode::from(2);
        }
    };
    let cases = budget_cases();
    let width = cases.iter().map(|c| c.name.len()).max().unwrap_or(0).max(6);
    println!(
        "{:<width$} | {:>6} | {:>6} | {:>6} | {:>6} | {:>5}",
        "случай", "cl100k", "o200k", "qwen≈", "точно", "лимит"
    );
    let mut over = Vec::new();
    for case in &cases {
        let mut c = count(&case.text);
        if let Some(base) = &llama {
            match tokenize_http(base, &case.text) {
                Ok(n) => c.exact = Some(n),
                Err(e) => {
                    eprintln!("prompt-budget: {}: {e}", case.name);
                    return ExitCode::from(2);
                }
            }
        }
        let exact = c.exact.map_or_else(|| "-".to_string(), |n| n.to_string());
        let bad = over_limit(&c, case.limit);
        println!(
            "{:<width$} | {:>6} | {:>6} | {:>6} | {:>6} | {:>5}{}",
            case.name,
            c.cl100k,
            c.o200k,
            c.qwen_est,
            exact,
            case.limit,
            if bad { "  ПРЕВЫШЕН" } else { "" }
        );
        if bad {
            over.push(case.name.as_str());
        }
    }
    if over.is_empty() {
        println!("prompt-budget: {} случаев в лимитах", cases.len());
        ExitCode::SUCCESS
    } else {
        eprintln!("prompt-budget: превышен лимит: {}", over.join(", "));
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;
    use za_agent::{budget_cases, role_core};
    use za_protocol::RunRole;

    #[test]
    fn qwen_estimate_rounds_up() {
        assert_eq!(Count::qwen_of(100), 105);
        assert_eq!(Count::qwen_of(101), 107);
        assert_eq!(Count::qwen_of(20), 21);
    }

    #[test]
    fn appendix_core_is_near_measured() {
        // Замер приложения A: Top core 699 cl100k.
        let n = count(role_core(RunRole::Top)).cl100k;
        assert!(n.abs_diff(699) <= 15, "top core: {n} cl100k");
    }

    #[test]
    fn worker_core_is_near_measured() {
        // D71: новый Core worker — 203 cl100k.
        let n = count(role_core(RunRole::Worker)).cl100k;
        assert!(n.abs_diff(203) <= 5, "worker core: {n} cl100k");
    }

    #[test]
    fn all_cases_within_limits() {
        for case in budget_cases() {
            let c = count(&case.text);
            assert!(
                !over_limit(&c, case.limit),
                "{}: {c:?} при лимите {}",
                case.name,
                case.limit
            );
        }
    }

    #[test]
    fn over_limit_checks_every_count() {
        let base = Count {
            cl100k: 10,
            o200k: 10,
            qwen_est: 10,
            exact: None,
        };
        assert!(!over_limit(&base, 10));
        assert!(over_limit(
            &Count {
                cl100k: 11,
                ..base.clone()
            },
            10
        ));
        assert!(over_limit(
            &Count {
                o200k: 11,
                ..base.clone()
            },
            10
        ));
        assert!(over_limit(
            &Count {
                qwen_est: 11,
                ..base.clone()
            },
            10
        ));
        assert!(over_limit(
            &Count {
                exact: Some(11),
                ..base.clone()
            },
            10
        ));
        assert!(!over_limit(
            &Count {
                exact: Some(10),
                ..base
            },
            10
        ));
    }

    /// Поддельный llama-server: один запрос, ответ `response`; возвращает адрес и поток,
    /// который отдаёт принятый запрос.
    fn fake_server(response: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            // Заголовки, затем тело по Content-Length.
            loop {
                let n = s.read(&mut buf).unwrap();
                assert!(n > 0, "клиент закрыл соединение до конца запроса");
                req.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&req);
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    if req.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            s.write_all(response.as_bytes()).unwrap();
            String::from_utf8(req).unwrap()
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn tokenize_http_parses_tokens() {
        let (base, server) = fake_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{\"tokens\":[1,2,3,4]}",
        );
        assert_eq!(tokenize_http(&base, "hello \"world\""), Ok(4));
        let req = server.join().unwrap();
        assert!(req.starts_with("POST /tokenize HTTP/1.1\r\n"), "{req}");
        let body = &req[req.find("\r\n\r\n").unwrap() + 4..];
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v, serde_json::json!({"content": "hello \"world\""}));
    }

    #[test]
    fn tokenize_http_reads_chunked_body() {
        let (base, server) = fake_server(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n6\r\n{\"toke\r\n9\r\nns\":[7,8]\r\n1\r\n}\r\n0\r\n\r\n",
        );
        assert_eq!(tokenize_http(&base, "x"), Ok(2));
        server.join().unwrap();
    }

    #[test]
    fn args_are_parsed() {
        assert_eq!(parse_args(&[]), Ok(None));
        let a = ["--llama".to_string(), "http://127.0.0.1:8080".to_string()];
        assert_eq!(parse_args(&a), Ok(Some("http://127.0.0.1:8080".into())));
        assert!(parse_args(&["--llama".to_string()]).is_err());
        assert!(parse_args(&["--x".to_string()]).is_err());
    }

    #[test]
    fn tokenize_http_rejects_errors() {
        let (base, server) =
            fake_server("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 2\r\n\r\n{}");
        let err = tokenize_http(&base, "x").unwrap_err();
        assert!(err.contains("500"), "{err}");
        server.join().unwrap();
        assert!(tokenize_http("https://127.0.0.1:1", "x").is_err());
        assert!(tokenize_http("127.0.0.1:1", "x").is_err());
    }
}
