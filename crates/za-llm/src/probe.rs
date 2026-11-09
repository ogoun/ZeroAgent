//! Зонды эндпоинтов (MDL-003, MDL-004, MDL-010, MDL-021).
//!
//! [`probe_llama`] (R0) — размер контекста и модель llama-server по `GET {base}/props`.
//! [`probe_endpoint`] — зонд любого бэкенда (задача 9): реальные возможности, контекст,
//! связка и цены → [`ModelProfile`]. Пути — относительно
//! [`EndpointConfig::effective_base_url`] (D37).
//!
//! Положение `n_ctx` различается между версиями `llama-server`, поэтому оно
//! берётся из корня ответа или из `default_generation_settings`; на живом сервере
//! это проверяет тест `live_probe` (`LLAMA_TEST_URL`).

use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use crate::caps::{
    BackendKind, PoolKind, ProviderCapabilities, ReasoningField, ThinkingSupport, default_caps,
};
use crate::config::{EndpointConfig, Price};
use crate::error::{ErrorClass, ModelError};
use crate::http::{HttpTransport, Secret, SecretSource, Timeouts, redact};
use crate::profile::{
    CONSERVATIVE_CONTEXT, EndpointFacts, EndpointState, ModelBundle, ModelProfile,
    OVERFLOW_PROBE_CHARS, ProbedEndpoint, parse_context_limit, quant_from_filename,
    resolve_context,
};
use crate::wire::ANTHROPIC_VERSION;

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
    /// Путь к файлу модели (`model_path`): из имени берётся квант.
    pub model_path: Option<String>,
    /// Сборка сервера (`build_info`, `b<номер>-<коммит>`).
    pub build_info: Option<String>,
    /// Шаблон умеет параллельные вызовы
    /// (`chat_template_caps.supports_parallel_tool_calls`).
    pub parallel_tool_calls: Option<bool>,
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

/// Разбирает ответ `/props`; общий для [`probe_llama`] и [`probe_endpoint`].
pub(crate) fn parse_props(props: &Value) -> Result<LlamaProbe, String> {
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
        model_path: non_empty_str(props, "model_path").map(str::to_owned),
        build_info: non_empty_str(props, "build_info").map(str::to_owned),
        parallel_tool_calls: props
            .pointer("/chat_template_caps/supports_parallel_tool_calls")
            .and_then(Value::as_bool),
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

// ---------------------------------------------------------------------------
// Зонд эндпоинта (задача 9).

/// Транспорт зонда эндпоинта: `Timeouts::probe_local()` без прокси для адресов этого
/// хоста и локальной сети, `Timeouts::probe_cloud()` с прокси окружения для облака.
pub fn probe_transport(ep: &EndpointConfig) -> HttpTransport {
    if ep.host_is_local() {
        HttpTransport::local(Timeouts::probe_local())
    } else {
        HttpTransport::cloud(Timeouts::probe_cloud())
    }
}

/// Зонд эндпоинта (MDL-003). Облачный эндпоинт без ключа и эндпоинт с `key_ref`, для
/// которого ключа нет, — `Inactive` без единого сетевого запроса (MDL-021). Ключ
/// отвергнут (401, 403) или модели нет (404, нет в каталоге) — `Inactive`; нет
/// соединения, таймаут, ошибка сервера, ответ не по протоколу — `Unreachable`.
/// Таймауты и прокси — у переданного транспорта (рабочему коду — [`probe_transport`]).
/// Ключ не попадает ни в профиль, ни в причину состояния: тексты проходят [`redact`].
pub async fn probe_endpoint(
    ep: &EndpointConfig,
    http: &HttpTransport,
    secrets: &dyn SecretSource,
) -> EndpointState {
    let secret = match endpoint_secret(ep, secrets) {
        Ok(s) => s,
        Err(reason) => return EndpointState::Inactive { reason },
    };
    let probe = Probe {
        ep,
        http,
        secret: secret.as_ref(),
        base: ep.effective_base_url(),
    };
    let result = match ep.backend {
        BackendKind::LlamaCpp => probe.llama().await,
        BackendKind::Vllm => probe.vllm().await,
        BackendKind::OpenRouter => probe.openrouter().await,
        BackendKind::Anthropic => probe.anthropic().await,
        BackendKind::OpenAiResponses | BackendKind::OpenAiChat => probe.openai().await,
        BackendKind::OpenAiCompatible => probe.compatible().await,
    };
    let secrets_list = probe.secrets();
    match result {
        Ok(found) => {
            let p = probe.profile(found, &secrets_list);
            tracing::debug!(endpoint = %ep.id, bundle = %p.facts.bundle_key,
                            context = p.facts.context_tokens, "зонд эндпоинта");
            EndpointState::Ready(p)
        }
        Err(Fail::Inactive(m)) => EndpointState::Inactive {
            reason: redact(&m, &secrets_list),
        },
        Err(Fail::Unreachable(m)) => EndpointState::Unreachable {
            reason: redact(&m, &secrets_list),
        },
    }
}

/// Ключ эндпоинта; `Err` — причина `Inactive`. Значение ключа в текст не попадает.
fn endpoint_secret(
    ep: &EndpointConfig,
    secrets: &dyn SecretSource,
) -> Result<Option<Secret>, String> {
    match &ep.key_ref {
        // Без `validate` значение могло быть самим ключом: в причину оно не попадает.
        Some(k) if !crate::config::valid_key_ref(k) => Err(format!(
            "key_ref эндпоинта {} недопустим: ожидается имя ключа",
            ep.id
        )),
        Some(k) => secrets
            .get(k)
            .map(Some)
            .ok_or_else(|| format!("ключ {k} не настроен")),
        None if ep.pool() == PoolKind::Cloud => Err(format!(
            "ключ не настроен: у облачного эндпоинта {} нет key_ref",
            ep.id
        )),
        None => Ok(None),
    }
}

/// Отказ зонда.
enum Fail {
    Inactive(String),
    Unreachable(String),
}

impl Fail {
    fn from_error(what: &str, e: &ModelError) -> Self {
        let m = format!("{what}: {:?}: {}", e.class, e.message);
        match e.class {
            ErrorClass::Auth | ErrorClass::NotFound => Fail::Inactive(m),
            _ => Fail::Unreachable(m),
        }
    }
}

/// Что нашёл зонд бэкенда.
struct Found {
    model: String,
    quant: Option<String>,
    runtime_version: Option<String>,
    caps: ProviderCapabilities,
    thinking: ThinkingSupport,
    /// Контекст рантайма и путь ответа, откуда он взят.
    runtime_ctx: Option<(u32, String)>,
    overflow: Option<u32>,
    max_output_tokens: Option<u32>,
    price: Option<Price>,
    slots: Option<u32>,
    /// Заметки о модели (в профиль связки).
    notes: Vec<String>,
    /// Заметки об эндпоинте: контекст, проба переполнения (в факты эндпоинта).
    ep_notes: Vec<String>,
}

impl Found {
    fn new(ep: &EndpointConfig, model: String) -> Self {
        Self {
            model,
            quant: None,
            runtime_version: None,
            caps: default_caps(ep.backend),
            thinking: ThinkingSupport::default(),
            runtime_ctx: None,
            overflow: None,
            max_output_tokens: None,
            price: None,
            slots: None,
            notes: Vec::new(),
            ep_notes: Vec::new(),
        }
    }
}

struct Probe<'a> {
    ep: &'a EndpointConfig,
    http: &'a HttpTransport,
    secret: Option<&'a Secret>,
    base: String,
}

impl Probe<'_> {
    fn secrets(&self) -> Vec<&Secret> {
        self.secret.into_iter().collect()
    }

    /// `Authorization: Bearer <ключ>`, если ключ есть.
    fn bearer(&self) -> Result<HeaderMap, ModelError> {
        let mut h = HeaderMap::new();
        if let Some(s) = self.secret {
            h.insert(AUTHORIZATION, s.header_value("Bearer ")?);
        }
        Ok(h)
    }

    async fn get(&self, path: &str, headers: HeaderMap) -> Result<Value, ModelError> {
        self.get_url(&format!("{}{path}", self.base), headers).await
    }

    async fn get_url(&self, url: &str, headers: HeaderMap) -> Result<Value, ModelError> {
        self.http.get_json(url, headers, &self.secrets()).await
    }

    /// `GET {base}{path}` с `Authorization: Bearer`.
    async fn get_bearer(&self, path: &str) -> Result<Value, Fail> {
        let h = self
            .bearer()
            .map_err(|e| Fail::from_error(&format!("GET {path}"), &e))?;
        self.get(path, h)
            .await
            .map_err(|e| Fail::from_error(&format!("GET {path}"), &e))
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, ModelError> {
        let url = format!("{}{path}", self.base);
        self.http
            .post_json(&url, self.bearer()?, body, &self.secrets())
            .await
            .map(|(v, _)| v)
    }

    fn configured_model(&self) -> String {
        self.ep.model.clone().unwrap_or_default()
    }

    /// Итог зонда: профиль связки и факты эндпоинта (контекст по порядку MDL-004);
    /// заметки очищены от ключа.
    fn profile(&self, f: Found, secrets: &[&Secret]) -> ProbedEndpoint {
        let (context_tokens, source) =
            resolve_context(self.ep.context_tokens, f.runtime_ctx.clone(), f.overflow);
        let mut ep_notes = f.ep_notes;
        if let (Some(cfg), Some((rt, _))) = (self.ep.context_tokens, &f.runtime_ctx)
            && cfg > *rt
        {
            ep_notes.push(format!(
                "context_tokens из конфигурации ({cfg}) больше контекста рантайма ({rt})"
            ));
        }
        let clean = |v: Vec<String>| v.iter().map(|n| redact(n, secrets)).collect();
        let probed_at = jiff::Timestamp::now();
        let bundle = ModelBundle {
            model: f.model,
            quant: f.quant,
            runtime: self.ep.backend,
            runtime_version: f.runtime_version,
        };
        let mut caps = f.caps;
        caps.context_source = source.clone();
        let facts = EndpointFacts {
            endpoint: self.ep.id.clone(),
            bundle_key: bundle.key(),
            context_tokens,
            context_source: source,
            runtime_context: f.runtime_ctx.map(|(n, _)| n),
            slots: f.slots,
            slot_save: None,
            probed_at,
            notes: clean(ep_notes),
        };
        let profile = ModelProfile {
            bundle,
            caps,
            thinking: f.thinking,
            max_output_tokens: f.max_output_tokens,
            price: f.price,
            probed_at,
            notes: clean(f.notes),
        };
        ProbedEndpoint { profile, facts }
    }

    /// llama.cpp: `/props` (через [`parse_props`]) и тест `/apply-template` (MDL-010).
    async fn llama(&self) -> Result<Found, Fail> {
        let props = self.get_bearer("/props").await?;
        let lp = parse_props(&props)
            .map_err(|m| Fail::Unreachable(format!("GET /props: неожиданный ответ: {m}")))?;
        let mut f = Found::new(self.ep, lp.model.clone());
        f.quant = lp.model_path.as_deref().and_then(quant_from_filename);
        f.runtime_version = lp.build_info.clone();
        f.slots = lp.total_slots;
        f.runtime_ctx = Some((lp.n_ctx, "/props".to_owned()));
        if let Some(p) = lp.parallel_tool_calls {
            f.caps.parallel_tools = p;
        }
        match self.reasoning_roundtrip().await {
            Ok(r) => f.caps.reasoning_roundtrip = r,
            Err(e) => f.notes.push(format!(
                "тест /apply-template не выполнен ({:?}: {}); reasoning_roundtrip = {} по умолчанию",
                e.class, e.message, f.caps.reasoning_roundtrip
            )),
        }
        Ok(f)
    }

    /// Влияет ли рассуждение прошлого ответа на промпт (MDL-010): два диалога,
    /// различающиеся только `reasoning_content` ассистента в цикле инструмента (после
    /// последнего сообщения пользователя: там шаблоны его сохраняют, если сохраняют
    /// вообще). Промпты различаются — рассуждение нужно возвращать.
    async fn reasoning_roundtrip(&self) -> Result<bool, ModelError> {
        let dialog = |reasoning: &str| {
            json!({
                "messages": [
                    {"role": "user", "content": "Прочитай файл a.txt"},
                    {"role": "assistant", "content": "", "reasoning_content": reasoning,
                     "tool_calls": [{"id": "call_1", "type": "function",
                                     "function": {"name": "read_file",
                                                  "arguments": "{\"path\":\"a.txt\"}"}}]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "ok"}
                ],
                "tools": [{"type": "function", "function": {
                    "name": "read_file", "description": "Прочитать файл.",
                    "parameters": {"type": "object",
                                   "properties": {"path": {"type": "string"}},
                                   "required": ["path"]}}}]
            })
        };
        let a = self
            .post("/apply-template", &dialog("ZA-PROBE-REASONING-ONE"))
            .await?;
        let b = self
            .post("/apply-template", &dialog("ZA-PROBE-REASONING-TWO"))
            .await?;
        let prompt = |v: &Value| v.get("prompt").and_then(Value::as_str).map(str::to_owned);
        match (prompt(&a), prompt(&b)) {
            (Some(a), Some(b)) => Ok(a != b),
            _ => Err(ModelError::new(
                ErrorClass::Malformed,
                "в ответе /apply-template нет строки prompt",
            )),
        }
    }

    /// vLLM: `{base}/models` и `{корень}/version` (D37).
    async fn vllm(&self) -> Result<Found, Fail> {
        let models = self.get_bearer("/models").await?;
        let entry = self.pick_model(&models)?;
        let mut f = Found::new(self.ep, model_id(entry));
        f.runtime_ctx = u32_field(entry, "max_model_len").map(|n| (n, "/models".to_owned()));
        if let Some(root) = self.base.strip_suffix("/v1") {
            let r = match self.bearer() {
                Ok(h) => self.get_url(&format!("{root}/version"), h).await,
                Err(e) => Err(e),
            };
            match r {
                Ok(v) => {
                    f.runtime_version = v.get("version").and_then(Value::as_str).map(str::to_owned);
                }
                Err(e) => f
                    .notes
                    .push(format!("GET /version: {:?}: {}", e.class, e.message)),
            }
        }
        self.overflow_if_needed(&mut f).await;
        Ok(f)
    }

    /// OpenRouter: каталог `{base}/models`.
    async fn openrouter(&self) -> Result<Found, Fail> {
        let models = self.get_bearer("/models").await?;
        let want = self.configured_model();
        // Варианты `:nitro`, `:floor`, `:online` и т. п. — ярлыки маршрутизации, в
        // каталоге их нет: ищется запись без суффикса. `:free` и другие варианты со своей
        // записью находятся точным совпадением раньше.
        let entry = find_model(&models, &want)
            .or_else(|| {
                want.rsplit_once(':')
                    .and_then(|(base, _)| find_model(&models, base))
            })
            .ok_or_else(|| {
                Fail::Inactive(format!("модель {want} не найдена в каталоге OpenRouter"))
            })?;
        let mut f = Found::new(self.ep, want);
        f.runtime_ctx = u32_field(entry, "context_length").map(|n| (n, "/models".to_owned()));
        let params: Vec<&str> = entry
            .get("supported_parameters")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let has = |p: &str| params.contains(&p);
        if !has("tools") {
            f.notes
                .push("модель не объявляет tools: нативные вызовы могут не работать".to_owned());
        }
        f.caps.tool_choice_required = has("tool_choice");
        f.caps.strict_schema = has("structured_outputs");
        if !has("reasoning") {
            f.caps.reasoning_field = ReasoningField::None;
            f.caps.reasoning_roundtrip = false;
        }
        f.price = openrouter_price(entry);
        Ok(f)
    }

    /// Anthropic: Models API `{base}/models/{model}` — `max_input_tokens`, `max_tokens`,
    /// `capabilities` (поля `context_window` в API нет).
    async fn anthropic(&self) -> Result<Found, Fail> {
        let model = self.configured_model();
        let path = format!("/models/{}", path_segment(&model));
        let mut h = HeaderMap::new();
        if let Some(s) = self.secret {
            let v = s
                .header_value("")
                .map_err(|e| Fail::from_error(&format!("GET {path}"), &e))?;
            h.insert(HeaderName::from_static("x-api-key"), v);
        }
        h.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        let v = self
            .get(&path, h)
            .await
            .map_err(|e| Fail::from_error(&format!("GET {path}"), &e))?;
        let mut f = Found::new(self.ep, model);
        f.runtime_ctx = u32_field(&v, "max_input_tokens").map(|n| (n, "/models".to_owned()));
        f.max_output_tokens = u32_field(&v, "max_tokens");
        match v.get("capabilities").filter(|c| c.is_object()) {
            Some(c) => {
                let sup = |p: &str| c.pointer(p).and_then(Value::as_bool).unwrap_or(false);
                let thinks = sup("/thinking/supported");
                f.thinking = ThinkingSupport {
                    adaptive: thinks && sup("/thinking/types/adaptive/supported"),
                    effort: sup("/effort/supported"),
                    budget: thinks && sup("/thinking/types/enabled/supported"),
                    effort_xhigh: sup("/effort/xhigh/supported"),
                    effort_max: sup("/effort/max/supported"),
                    sampling: false,
                };
                f.caps.strict_schema = sup("/structured_outputs/supported");
            }
            None => {
                // Как до зонда (решение задачи 5): модель считается актуальной.
                f.thinking = ThinkingSupport {
                    adaptive: true,
                    effort: true,
                    ..ThinkingSupport::default()
                };
                f.notes.push(
                    "Models API не сообщил capabilities: adaptive и effort по умолчанию".to_owned(),
                );
            }
        }
        Ok(f)
    }

    /// OpenAI: `{base}/models/{model}` — только существование модели.
    async fn openai(&self) -> Result<Found, Fail> {
        let model = self.configured_model();
        self.get_bearer(&format!("/models/{}", path_segment(&model)))
            .await?;
        let mut f = Found::new(self.ep, model.clone());
        if self.ep.context_tokens.is_none() {
            f.ep_notes.push(format!(
                "OpenAI не сообщает контекст модели: задайте context_tokens \
                 (сейчас {CONSERVATIVE_CONTEXT})"
            ));
        }
        // Responses читает все поля, Chat Completions — только `sampling`.
        f.thinking = openai_thinking(&model);
        f.notes.push(
            "возможности рассуждения OpenAI — по имени модели (API их не сообщает)".to_owned(),
        );
        Ok(f)
    }

    /// Совместимый сервер: `{base}/models` (`max_model_len`, `context_length`,
    /// `context_window`), иначе проба переполнения (D17, D18).
    async fn compatible(&self) -> Result<Found, Fail> {
        let mut f = Found::new(self.ep, self.configured_model());
        let models = match self.bearer() {
            Ok(h) => self.get("/models", h).await,
            Err(e) => Err(e),
        };
        match models {
            Ok(models) => {
                let entry = if f.model.is_empty() {
                    first_model(&models)
                } else {
                    find_model(&models, &f.model)
                };
                if let Some(e) = entry {
                    if f.model.is_empty() {
                        f.model = model_id(e);
                    }
                    f.runtime_ctx = ["max_model_len", "context_length", "context_window"]
                        .iter()
                        .find_map(|k| u32_field(e, k))
                        .map(|n| (n, "/models".to_owned()));
                }
            }
            // Сервер недоступен или ключ отвергнут — дальше пробовать нечего.
            Err(e)
                if matches!(
                    e.class,
                    ErrorClass::Transport | ErrorClass::Timeout | ErrorClass::Auth
                ) =>
            {
                return Err(Fail::from_error("GET /models", &e));
            }
            // Списка моделей нет (404 и т. п.): контекст ищется пробой.
            Err(e) => f
                .ep_notes
                .push(format!("GET /models: {:?}: {}", e.class, e.message)),
        }
        self.overflow_if_needed(&mut f).await;
        Ok(f)
    }

    /// Проба переполнения (D18): только сервер на этом хосте и только если ни
    /// конфигурация, ни рантайм контекст не дали. Облако не трогается никогда.
    async fn overflow_if_needed(&self, f: &mut Found) {
        if self.ep.context_tokens.is_some() || f.runtime_ctx.is_some() || !self.ep.on_host() {
            return;
        }
        let mut body = json!({
            "messages": [{"role": "user", "content": overflow_prompt()}],
            "max_tokens": 1,
            "stream": false,
        });
        if !f.model.is_empty() {
            body["model"] = json!(f.model);
        }
        match self.post("/chat/completions", &body).await {
            Ok(_) => f.ep_notes.push(format!(
                "проба переполнения: сервер принял {OVERFLOW_PROBE_CHARS} символов, \
                 контекст не определён"
            )),
            Err(e) => match parse_context_limit(&e.message) {
                Some(n) => f.overflow = Some(n),
                None => f.ep_notes.push(format!(
                    "проба переполнения: предел не найден в ответе ({:?})",
                    e.class
                )),
            },
        }
    }

    /// Запись модели в списке `data`: заданная в конфигурации или первая.
    fn pick_model<'v>(&self, models: &'v Value) -> Result<&'v Value, Fail> {
        let want = self.configured_model();
        let entry = if want.is_empty() {
            first_model(models)
        } else {
            find_model(models, &want)
        };
        entry.ok_or_else(|| {
            let have: Vec<String> = data(models).iter().map(model_id).collect();
            Fail::Inactive(format!(
                "модель {want} не обслуживается сервером (есть: {})",
                have.join(", ")
            ))
        })
    }
}

/// Сегмент пути URL: всё вне `[A-Za-z0-9-._~:@!$&'()*+,;=]` кодируется `%XX`, в
/// том числе `/` (`ft:gpt-4o:org::id` остаётся как есть, `org/model` → `org%2Fmodel`).
fn path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric() || b"-._~:@!$&'()*+,;=".contains(&b);
        if keep {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Текст пробы переполнения: однобуквенные слова через пробел — около токена на два
/// символа у BPE (повтор одной буквы сжимается в длинные токены и до предела не доходит).
fn overflow_prompt() -> String {
    const PATTERN: &str = "a b c d e f g h i j k l m n o p q r s t u v w x y z ";
    PATTERN.chars().cycle().take(OVERFLOW_PROBE_CHARS).collect()
}

fn data(models: &Value) -> &[Value] {
    models
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn find_model<'v>(models: &'v Value, id: &str) -> Option<&'v Value> {
    data(models)
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(id))
}

fn first_model(models: &Value) -> Option<&Value> {
    data(models).first()
}

fn model_id(e: &Value) -> String {
    e.get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Положительное `u32` по ключу; дробное округляется вниз.
fn u32_field(v: &Value, key: &str) -> Option<u32> {
    let x = v.get(key)?;
    let n = x
        .as_u64()
        .or_else(|| x.as_f64().filter(|f| *f >= 1.0).map(|f| f as u64))?;
    u32::try_from(n).ok().filter(|n| *n > 0)
}

/// Цены OpenRouter: `pricing.prompt`, `pricing.completion`, `pricing.input_cache_read`
/// — USD за токен строкой; × 1e6 десятичным сдвигом, без ошибки двоичной дроби.
/// Отрицательная цена (маршрутизаторы вроде `openrouter/auto`) — цены нет.
fn openrouter_price(entry: &Value) -> Option<Price> {
    let p = entry.get("pricing")?;
    let get = |k: &str| p.get(k).and_then(per_token_to_mtok);
    Some(Price {
        input_per_mtok: get("prompt")?,
        cached_per_mtok: get("input_cache_read"),
        cache_write_per_mtok: get("input_cache_write"),
        output_per_mtok: get("completion")?,
    })
}

/// «USD за токен» → «USD за миллион токенов».
fn per_token_to_mtok(v: &Value) -> Option<f64> {
    let s = match v {
        Value::String(s) => s.trim().to_owned(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let r = if s.contains(['e', 'E']) {
        s.parse::<f64>().ok().map(|x| x * 1e6)
    } else {
        shift_decimal(&s, 6).and_then(|t| t.parse::<f64>().ok())
    };
    r.filter(|x| x.is_finite() && *x >= 0.0)
}

/// Десятичная строка, умноженная на 10^k сдвигом точки: `"0.000003"`, 6 → `"3"`.
fn shift_decimal(s: &str, k: usize) -> Option<String> {
    if s.is_empty() || s.starts_with('-') {
        return None;
    }
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let digits_only = |t: &str| t.bytes().all(|b| b.is_ascii_digit());
    if !digits_only(int) || !digits_only(frac) || (int.is_empty() && frac.is_empty()) {
        return None;
    }
    let mut frac = frac.to_owned();
    while frac.len() < k {
        frac.push('0');
    }
    let (moved, rest) = frac.split_at(k);
    let joined = format!("{int}{moved}");
    let trimmed = joined.trim_start_matches('0');
    let whole = if trimmed.is_empty() { "0" } else { trimmed };
    Some(if rest.is_empty() {
        whole.to_owned()
    } else {
        format!("{whole}.{rest}")
    })
}

/// Возможности рассуждения модели OpenAI по имени: `GET /models/{id}` их не сообщает
/// (поля `id`, `created`, `object`, `owned_by`, `shutdown_date`; проверено 2026-11-09).
/// - `gpt-4*`, `gpt-3.5*`, `chatgpt-*` — без рассуждений: `reasoning` и `include`
///   не отправляются, `temperature`/`top_p` принимаются;
/// - `o1`, `o3`, `o4…`, `gpt-5`, `gpt-5.1` — рассуждают, `temperature`/`top_p` — 400;
/// - `gpt-5.2`… — то же и `xhigh`;
/// - `gpt-6*` — `xhigh` и `max` (страница «Models»: GPT-6 Astra, 6.1 Sol/Terra, 6 Luna);
/// - неизвестное имя — рассуждающая модель без `xhigh`/`max` и без сэмплирования
///   (лишнее поле даёт 400, его отсутствие — нет).
#[doc(hidden)]
pub fn openai_thinking(model: &str) -> ThinkingSupport {
    let m = model.to_ascii_lowercase();
    if m.starts_with("gpt-4") || m.starts_with("gpt-3.5") || m.starts_with("chatgpt-") {
        return ThinkingSupport {
            sampling: true,
            ..ThinkingSupport::default()
        };
    }
    let reasoning = ThinkingSupport {
        effort: true,
        ..ThinkingSupport::default()
    };
    if m.starts_with("gpt-6") {
        return ThinkingSupport {
            effort_xhigh: true,
            effort_max: true,
            ..reasoning
        };
    }
    if let Some(rest) = m.strip_prefix("gpt-5.") {
        let minor: u32 = rest
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0);
        return ThinkingSupport {
            effort_xhigh: minor >= 2,
            ..reasoning
        };
    }
    reasoning
}
