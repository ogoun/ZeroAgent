//! Классификация HTTP-статусов, `Retry-After` и заголовки лимитов провайдеров.

use std::time::{Duration, SystemTime};

use reqwest::header::HeaderMap;

use super::secret::truncate;
use crate::error::{ErrorClass, ModelError};

/// Предел тела в сообщении ошибки.
const MESSAGE_BODY_LIMIT: usize = 2048;

/// Признаки переполнения контекста в теле ответа 400/413 (в нижнем регистре).
/// llama.cpp: `exceed_context_size_error` («the request exceeds the available context
/// size»); OpenAI, vLLM, OpenRouter: «maximum context length», `context_length_exceeded`;
/// Anthropic: «prompt is too long».
const OVERFLOW_MARKERS: &[&str] = &[
    "exceed_context_size",
    "exceeds the available context size",
    "context_length_exceeded",
    "maximum context length",
    "prompt is too long",
    "context window",
];

/// Лимиты, которые провайдер сообщил в заголовках ответа.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderLimits {
    pub remaining_requests: Option<u64>,
    pub remaining_tokens: Option<u64>,
    /// Через сколько сбросится исчерпанный лимит (если исчерпан — именно его,
    /// иначе наибольший из названных).
    pub reset_after: Option<Duration>,
}

/// Ошибка по HTTP-статусу ответа. `body` уже очищен от ключей ([`super::redact`]).
///
/// 401/403 → `Auth`, 404 → `NotFound`, 408 → `Timeout`, 429 → `RateLimited`,
/// 503/529 → `Overloaded`, 501 → `BadRequest` (llama.cpp без `--slot-save-path`:
/// не повторяется), прочие 5xx → `Server`, 400/413 с признаком переполнения →
/// `ContextOverflow`, прочее → `BadRequest`. `retry_after` у 429, 503, 529 — из
/// `retry-after-ms` или `Retry-After`; у 429 без них — `reset_after` лимитов.
pub fn classify_status(status: u16, headers: &HeaderMap, body: &str) -> ModelError {
    let class = match status {
        401 | 403 => ErrorClass::Auth,
        404 => ErrorClass::NotFound,
        408 => ErrorClass::Timeout,
        429 => ErrorClass::RateLimited,
        503 | 529 => ErrorClass::Overloaded,
        501 => ErrorClass::BadRequest,
        500..=599 => ErrorClass::Server,
        400 | 413 if is_context_overflow(body) => ErrorClass::ContextOverflow,
        _ => ErrorClass::BadRequest,
    };
    let retry_after = match status {
        429 => header_retry_after(headers).or_else(|| parse_rate_limits(headers).reset_after),
        503 | 529 => header_retry_after(headers),
        _ => None,
    };
    let body = truncate(body.trim().to_owned(), MESSAGE_BODY_LIMIT);
    let message = if body.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {body}")
    };
    ModelError {
        class,
        message,
        retry_after,
        http_status: Some(status),
    }
}

fn is_context_overflow(body: &str) -> bool {
    let lower = body.to_lowercase();
    OVERFLOW_MARKERS.iter().any(|m| lower.contains(m))
}

/// `retry-after-ms` (OpenAI) приоритетнее `Retry-After`.
fn header_retry_after(h: &HeaderMap) -> Option<Duration> {
    if let Some(ms) = header_str(h, "retry-after-ms").and_then(|v| v.trim().parse::<f64>().ok())
        && ms.is_finite()
        && ms >= 0.0
    {
        return Some(Duration::from_secs_f64(ms / 1000.0));
    }
    header_str(h, "retry-after").and_then(|v| parse_retry_after(v, SystemTime::now()))
}

/// Значение `Retry-After`: число секунд (допускается дробное) или HTTP-дата
/// (RFC 9110, `Wed, 21 Oct 2026 07:28:00 GMT`). Дата в прошлом — нулевая пауза.
pub fn parse_retry_after(v: &str, now: SystemTime) -> Option<Duration> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    if v.bytes().all(|b| b.is_ascii_digit()) {
        return v.parse::<u64>().ok().map(Duration::from_secs);
    }
    if let Ok(secs) = v.parse::<f64>() {
        return (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs));
    }
    let ts = jiff::fmt::rfc2822::DateTimeParser::new()
        .parse_timestamp(v)
        .ok()?;
    Some(until(SystemTime::from(ts), now))
}

/// Лимиты из заголовков OpenAI (`x-ratelimit-*-requests|tokens`), Anthropic
/// (`anthropic-ratelimit-*`) и OpenRouter (`x-ratelimit-remaining`, `x-ratelimit-reset`).
pub fn parse_rate_limits(h: &HeaderMap) -> ProviderLimits {
    let now = SystemTime::now();
    let remaining_requests = first_u64(
        h,
        &[
            "x-ratelimit-remaining-requests",
            "anthropic-ratelimit-requests-remaining",
            "x-ratelimit-remaining",
        ],
    );
    let remaining_tokens = first_u64(
        h,
        &[
            "x-ratelimit-remaining-tokens",
            "anthropic-ratelimit-tokens-remaining",
            "anthropic-ratelimit-input-tokens-remaining",
        ],
    );
    let reset_requests = header_str(h, "x-ratelimit-reset-requests")
        .and_then(parse_go_duration)
        .or_else(|| {
            header_str(h, "anthropic-ratelimit-requests-reset").and_then(|v| rfc3339_in(v, now))
        })
        .or_else(|| header_str(h, "x-ratelimit-reset").and_then(|v| openrouter_reset(v, now)));
    let reset_tokens = header_str(h, "x-ratelimit-reset-tokens")
        .and_then(parse_go_duration)
        .or_else(|| {
            header_str(h, "anthropic-ratelimit-tokens-reset").and_then(|v| rfc3339_in(v, now))
        });
    let reset_after = if remaining_requests == Some(0) && reset_requests.is_some() {
        reset_requests
    } else if remaining_tokens == Some(0) && reset_tokens.is_some() {
        reset_tokens
    } else {
        reset_requests.max(reset_tokens)
    };
    ProviderLimits {
        remaining_requests,
        remaining_tokens,
        reset_after,
    }
}

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

fn first_u64(h: &HeaderMap, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|n| header_str(h, n).and_then(|v| v.trim().parse::<u64>().ok()))
}

fn until(t: SystemTime, now: SystemTime) -> Duration {
    t.duration_since(now).unwrap_or(Duration::ZERO)
}

/// Момент RFC 3339 (`2026-10-21T07:28:00Z`) → пауза от `now`.
fn rfc3339_in(v: &str, now: SystemTime) -> Option<Duration> {
    let ts: jiff::Timestamp = v.trim().parse().ok()?;
    Some(until(SystemTime::from(ts), now))
}

/// OpenRouter: `x-ratelimit-reset` — момент в миллисекундах эпохи Unix.
/// Небольшое число считается паузой в секундах.
fn openrouter_reset(v: &str, now: SystemTime) -> Option<Duration> {
    let n: u64 = v.trim().parse().ok()?;
    if n >= 100_000_000_000 {
        Some(until(
            SystemTime::UNIX_EPOCH + Duration::from_millis(n),
            now,
        ))
    } else {
        Some(Duration::from_secs(n))
    }
}

/// Длительность в стиле Go (`6m0s`, `1.5s`, `20ms`, `1h2m3s`); голое число — секунды.
fn parse_go_duration(v: &str) -> Option<Duration> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    if let Ok(secs) = v.parse::<f64>() {
        return (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs));
    }
    let mut total = 0.0_f64;
    let mut rest = v;
    while !rest.is_empty() {
        let num_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if num_len == 0 {
            return None;
        }
        let n: f64 = rest[..num_len].parse().ok()?;
        rest = &rest[num_len..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let scale = match &rest[..unit_len] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 1e-3,
            "us" | "µs" => 1e-6,
            "ns" => 1e-9,
            _ => return None,
        };
        rest = &rest[unit_len..];
        total += n * scale;
    }
    total.is_finite().then(|| Duration::from_secs_f64(total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn go_durations() {
        let d = |s| parse_go_duration(s);
        assert_eq!(d("6m0s"), Some(Duration::from_secs(360)));
        assert_eq!(d("20ms"), Some(Duration::from_millis(20)));
        assert_eq!(d("1h2m3s"), Some(Duration::from_secs(3723)));
        assert_eq!(d("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(d("12"), Some(Duration::from_secs(12)));
        assert_eq!(d("5x"), None);
        assert_eq!(d("s"), None);
    }

    #[test]
    fn rate_limited_falls_back_to_reset() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-ratelimit-remaining-tokens",
            HeaderValue::from_static("0"),
        );
        h.insert("x-ratelimit-reset-tokens", HeaderValue::from_static("20s"));
        h.insert("x-ratelimit-reset-requests", HeaderValue::from_static("1s"));
        let e = classify_status(429, &h, "");
        assert_eq!(e.retry_after, Some(Duration::from_secs(20)));
    }

    #[test]
    fn retry_after_ms_wins() {
        let mut h = HeaderMap::new();
        h.insert("retry-after-ms", HeaderValue::from_static("1500"));
        h.insert("retry-after", HeaderValue::from_static("9"));
        let e = classify_status(529, &h, "overloaded");
        assert_eq!(e.retry_after, Some(Duration::from_millis(1500)));
        assert_eq!(e.message, "HTTP 529: overloaded");
    }

    #[test]
    fn overflow_on_413_and_anthropic_wording() {
        let h = HeaderMap::new();
        let e = classify_status(
            400,
            &h,
            "prompt is too long: 210000 tokens > 200000 maximum",
        );
        assert_eq!(e.class, ErrorClass::ContextOverflow);
        assert_eq!(
            classify_status(413, &h, "payload too large").class,
            ErrorClass::BadRequest
        );
    }

    #[test]
    fn past_date_is_zero() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_792_567_670);
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2026 07:00:00 GMT", now),
            Some(Duration::ZERO)
        );
    }
}
