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
/// Anthropic: «prompt is too long», «input length and `max_tokens` exceed context limit».
const OVERFLOW_MARKERS: &[&str] = &[
    "exceed_context_size",
    "exceeds the available context size",
    "context_length_exceeded",
    "maximum context length",
    "prompt is too long",
    "exceeds the context window",
    "exceed context limit",
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

/// Потолок любой паузы, названной провайдером (`Retry-After`, сброс лимитов).
///
/// Значения из заголовков выбирает удалённая сторона, поэтому они не должны вызывать
/// панику. Правило единое: значение, которое не разобралось, отрицательное или
/// `NaN`, игнорируется (`None`); слишком большое (в том числе `1e30`, `inf`, момент
/// за пределами `Duration`) насыщается до 24 ч. Потолок намного больше 120 с, поэтому
/// ретраер (D19) ведёт себя как при любом долгом `Retry-After`: повторов нет.
pub const MAX_PROVIDER_PAUSE: Duration = Duration::from_secs(24 * 3600);

/// Секунды из заголовка → пауза по правилу [`MAX_PROVIDER_PAUSE`].
fn secs_hint(secs: f64) -> Option<Duration> {
    if secs.is_nan() || secs < 0.0 {
        return None;
    }
    Some(
        Duration::try_from_secs_f64(secs)
            .unwrap_or(MAX_PROVIDER_PAUSE)
            .min(MAX_PROVIDER_PAUSE),
    )
}

/// Пауза от `now` до момента `target_ns` (наносекунды эпохи Unix, без `SystemTime`
/// для момента: на Windows `SystemTime` не вмещает даты раньше 1601 года).
/// Момент в прошлом — нулевая пауза, далёкий — потолок.
fn until_ns(target_ns: i128, now: SystemTime) -> Duration {
    let now_ns = match now.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    };
    let diff = target_ns.saturating_sub(now_ns);
    if diff <= 0 {
        return Duration::ZERO;
    }
    let cap = MAX_PROVIDER_PAUSE.as_nanos() as i128;
    Duration::from_nanos(diff.min(cap) as u64)
}

/// `retry-after-ms` (OpenAI) приоритетнее `Retry-After`.
fn header_retry_after(h: &HeaderMap) -> Option<Duration> {
    if let Some(d) = header_str(h, "retry-after-ms")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .and_then(|ms| secs_hint(ms / 1000.0))
    {
        return Some(d);
    }
    header_str(h, "retry-after").and_then(|v| parse_retry_after(v, SystemTime::now()))
}

/// Значение `Retry-After`: число секунд (допускается дробное) или HTTP-дата
/// (RFC 9110, `Wed, 21 Oct 2026 07:28:00 GMT`). Дата в прошлом — нулевая пауза;
/// пределы — [`MAX_PROVIDER_PAUSE`].
pub fn parse_retry_after(v: &str, now: SystemTime) -> Option<Duration> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    if v.bytes().all(|b| b.is_ascii_digit()) {
        return Some(
            v.parse::<u64>()
                .map(Duration::from_secs)
                .unwrap_or(MAX_PROVIDER_PAUSE)
                .min(MAX_PROVIDER_PAUSE),
        );
    }
    if let Ok(secs) = v.parse::<f64>() {
        return secs_hint(secs);
    }
    let ts = jiff::fmt::rfc2822::DateTimeParser::new()
        .parse_timestamp(v)
        .ok()?;
    Some(until_ns(ts.as_nanosecond(), now))
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

/// Момент RFC 3339 (`2026-10-21T07:28:00Z`) → пауза от `now`.
fn rfc3339_in(v: &str, now: SystemTime) -> Option<Duration> {
    let ts: jiff::Timestamp = v.trim().parse().ok()?;
    Some(until_ns(ts.as_nanosecond(), now))
}

/// OpenRouter: `x-ratelimit-reset` — момент в миллисекундах эпохи Unix.
/// Небольшое число считается паузой в секундах.
fn openrouter_reset(v: &str, now: SystemTime) -> Option<Duration> {
    let n: u64 = v.trim().parse().ok()?;
    if n >= 100_000_000_000 {
        Some(until_ns(i128::from(n) * 1_000_000, now))
    } else {
        Some(Duration::from_secs(n).min(MAX_PROVIDER_PAUSE))
    }
}

/// Длительность в стиле Go (`6m0s`, `1.5s`, `20ms`, `1h2m3s`); голое число — секунды.
fn parse_go_duration(v: &str) -> Option<Duration> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    if let Ok(secs) = v.parse::<f64>() {
        return secs_hint(secs);
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
    secs_hint(total)
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

    /// Значения заголовков от удалённой стороны не вызывают панику (ревью задачи 3).
    #[test]
    fn hostile_header_values_do_not_panic() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_792_567_670);
        let cap = Some(MAX_PROVIDER_PAUSE);
        // 1e30: насыщение до потолка.
        assert_eq!(parse_retry_after("1e30", now), cap);
        assert_eq!(parse_go_duration("99999999999999999999999s"), cap);
        assert_eq!(parse_retry_after("99999999999999999999999", now), cap);
        assert_eq!(parse_retry_after("inf", now), cap);
        // NaN и отрицательные: игнор.
        assert_eq!(parse_retry_after("NaN", now), None);
        assert_eq!(parse_retry_after("-1", now), None);
        assert_eq!(parse_go_duration("-1"), None);
        assert_eq!(parse_go_duration("NaN"), None);
        // Огромный момент в мс эпохи: потолок.
        assert_eq!(openrouter_reset(&u64::MAX.to_string(), now), cap);
        // Дата раньше 1601 года (на Windows вне `SystemTime`): прошлое — ноль.
        assert_eq!(
            rfc3339_in("1500-01-01T00:00:00Z", now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            rfc3339_in("0001-01-01T00:00:00Z", now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            parse_retry_after("01 Jan 1500 00:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        // Далёкое будущее — потолок.
        assert_eq!(rfc3339_in("9000-01-01T00:00:00Z", now), cap);

        let mut h = HeaderMap::new();
        h.insert("retry-after-ms", HeaderValue::from_static("1e30"));
        assert_eq!(classify_status(429, &h, "").retry_after, cap);
        let mut h = HeaderMap::new();
        h.insert("retry-after-ms", HeaderValue::from_static("NaN"));
        h.insert("retry-after", HeaderValue::from_static("-1"));
        h.insert(
            "x-ratelimit-reset",
            HeaderValue::from_static("18446744073709551615"),
        );
        h.insert(
            "anthropic-ratelimit-tokens-reset",
            HeaderValue::from_static("0001-01-01T00:00:00Z"),
        );
        let e = classify_status(429, &h, "");
        assert_eq!(e.retry_after, cap);
        assert_eq!(parse_rate_limits(&h).reset_after, cap);
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
