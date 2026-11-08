//! Зонд llama.cpp: реальный размер контекста и модель по `GET {base}/props`.
//!
//! Положение `n_ctx` различается между версиями `llama-server`, поэтому оно
//! берётся из корня ответа или из `default_generation_settings`; на живом сервере
//! это проверяет тест `live_probe` (`LLAMA_TEST_URL`).

use std::time::Duration;

use serde_json::Value;

/// Таймаут установки соединения: недоступный сервер должен определяться быстро.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Общий таймаут запроса вместе с чтением тела.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Результат зонда.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamaProbe {
    /// Размер контекста, с которым запущен сервер (больше нуля).
    pub n_ctx: u32,
    /// Имя модели для показа: `model_alias`, иначе `model`, иначе имя файла
    /// из `model_path` без каталога. Пустая строка, если сервер ничего из этого не отдал.
    pub model: String,
    /// Число слотов (`total_slots`), если сервер его сообщает.
    pub total_slots: Option<u32>,
}

/// Ошибка зонда.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// Сервер не ответил: нет соединения, таймаут, некорректный адрес.
    #[error("llama-server недоступен: {0}")]
    Unreachable(String),
    /// Сервер ответил, но не так, как `llama-server`: не 2xx, не JSON, нет `n_ctx`.
    #[error("неожиданный ответ llama-server: {0}")]
    BadResponse(String),
}

/// Опрашивает `GET {base_url}/props` (завершающий `/` в `base_url` допустим).
/// Соединение — не дольше 2 с, весь запрос — не дольше 5 с. Прокси из окружения
/// (`HTTP_PROXY`, `ALL_PROXY`) не используются: сервер локальный, идти к нему надо напрямую.
pub async fn probe_llama(base_url: &str) -> Result<LlamaProbe, ProbeError> {
    let url = format!("{}/props", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOTAL_TIMEOUT)
        .no_proxy()
        .build()
        .map_err(|e| ProbeError::Unreachable(format!("HTTP-клиент: {e}")))?;

    let resp = client.get(&url).send().await.map_err(transport_error)?;
    let status = resp.status();
    let body = resp.bytes().await.map_err(transport_error)?;
    if !status.is_success() {
        return Err(ProbeError::BadResponse(format!("{url}: HTTP {status}")));
    }
    let props: Value = serde_json::from_slice(&body)
        .map_err(|e| ProbeError::BadResponse(format!("{url}: тело не JSON: {e}")))?;
    let probe = parse_props(&props).map_err(|m| ProbeError::BadResponse(format!("{url}: {m}")))?;
    tracing::debug!(%url, n_ctx = probe.n_ctx, model = %probe.model, "зонд llama-server");
    Ok(probe)
}

/// Ошибка транспорта (соединение, таймаут, обрыв тела) — сервер недоступен.
fn transport_error(e: reqwest::Error) -> ProbeError {
    ProbeError::Unreachable(e.to_string())
}

/// Разбирает ответ `/props`.
fn parse_props(props: &Value) -> Result<LlamaProbe, String> {
    let n_ctx = props
        .get("n_ctx")
        .or_else(|| props.pointer("/default_generation_settings/n_ctx"))
        .ok_or_else(|| "нет поля n_ctx".to_owned())?;
    let n_ctx = n_ctx
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|&n| n > 0)
        .ok_or_else(|| format!("n_ctx не положительное u32: {n_ctx}"))?;

    let model = ["model_alias", "model"]
        .iter()
        .find_map(|k| non_empty_str(props, k))
        .map(str::to_owned)
        .or_else(|| non_empty_str(props, "model_path").map(basename))
        .unwrap_or_default();

    let total_slots = props
        .get("total_slots")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());

    Ok(LlamaProbe {
        n_ctx,
        model,
        total_slots,
    })
}

fn non_empty_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

/// Имя файла без каталога; разделители `/` и `\` (путь может прийти с Windows).
fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned()
}
