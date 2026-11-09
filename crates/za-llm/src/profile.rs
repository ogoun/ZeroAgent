//! Профили моделей на связку «модель, квант, рантайм, версия» (MDL-003, MDL-004, D13).
//!
//! Профиль пишет зонд ([`crate::probe_endpoint`]) и хранит [`ProfileStore`] в
//! `ZA_HOME/models/profiles/<bundle_key>.json`. Смена модели, кванта или версии
//! рантайма даёт новый ключ: старый профиль к новой связке не применяется. Ключей и
//! токенов в профиле нет — только возможности, контекст, цены и заметки.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::caps::{BackendKind, ContextSource, ProviderCapabilities, ThinkingSupport};
use crate::config::Price;
use crate::types::EndpointId;

/// Консервативный контекст, если ничто его не сообщило (D18, таблица 2.3 части 02).
pub const CONSERVATIVE_CONTEXT: u32 = 8192;

/// Длина промпта пробы переполнения в символах (D18).
pub const OVERFLOW_PROBE_CHARS: usize = 2_000_000;

/// Связка, на которую хранится профиль (MDL-003).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelBundle {
    pub model: String,
    pub quant: Option<String>,
    pub runtime: BackendKind,
    pub runtime_version: Option<String>,
}

impl ModelBundle {
    /// Ключ связки: первые 16 hex `blake3` канонического JSON (ключи по алфавиту).
    pub fn key(&self) -> String {
        let canonical = serde_json::to_value(self)
            .map(|v| canonical_json(&v))
            .unwrap_or_default();
        blake3::hash(canonical.as_bytes()).to_hex()[..16].to_owned()
    }
}

/// JSON с ключами объектов по алфавиту, без пробелов: не зависит от функции
/// `preserve_order` у `serde_json` и порядка полей.
fn canonical_json(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let items: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical_json(&m[k])))
                .collect();
            format!("{{{}}}", items.join(","))
        }
        Value::Array(a) => {
            let items: Vec<String> = a.iter().map(canonical_json).collect();
            format!("[{}]", items.join(","))
        }
        other => other.to_string(),
    }
}

/// Профиль модели: реальные возможности по зонду.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelProfile {
    pub endpoint: EndpointId,
    pub bundle: ModelBundle,
    pub caps: ProviderCapabilities,
    /// Способ мышления: Anthropic — Models API, OpenAI Responses — по имени модели,
    /// прочие — `Default`.
    pub thinking: ThinkingSupport,
    pub context_tokens: u32,
    /// Предел вывода: Anthropic Models API `max_tokens`; у прочих `None`.
    pub max_output_tokens: Option<u32>,
    /// Цены OpenRouter (USD за токен × 1e6). `Gateway` берёт `config.price.or(profile.price)`.
    pub price: Option<Price>,
    /// Слоты llama.cpp (`total_slots`).
    pub slots: Option<u32>,
    /// Сохранение слотов работает (D29); `None` — ещё не проверялось.
    pub slot_save: Option<bool>,
    pub probed_at: jiff::Timestamp,
    /// Заметки зонда для `models` и `doctor` (подсказки, частичные отказы).
    pub notes: Vec<String>,
}

/// Состояние эндпоинта после зонда.
// Форма задана интерфейсом задачи 9; состояние одно на эндпоинт и меняется только
// зондом, лишние байты вариантов без профиля не стоят упаковки в `Box`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum EndpointState {
    Ready(ModelProfile),
    /// Не используется до действия пользователя: нет ключа, ключ отвергнут, нет модели.
    Inactive {
        reason: String,
    },
    /// Сервер не ответил или ответил не по протоколу; зонд повторяется (D40).
    Unreachable {
        reason: String,
    },
}

/// Каталог профилей (D13).
#[derive(Debug, Clone)]
pub struct ProfileStore {
    dir: PathBuf,
}

impl ProfileStore {
    /// Открывает каталог, создавая его при необходимости.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    /// Профиль по ключу связки. Нет файла, файл повреждён или ключ не из 16 hex —
    /// `None` (повреждение пишется в `warn`: профиль будет снят заново).
    pub fn load(&self, key: &str) -> Option<ModelProfile> {
        let path = self.path(key)?;
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "профиль не прочитан");
                return None;
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "профиль повреждён");
                None
            }
        }
    }

    /// Атомарная запись: `<key>.json.tmp` рядом, затем `rename` (D13).
    pub fn save(&self, p: &ModelProfile) -> std::io::Result<()> {
        let key = p.bundle.key();
        let path = self
            .path(&key)
            .ok_or_else(|| std::io::Error::other("ключ связки не из 16 hex"))?;
        let tmp = self.dir.join(format!("{key}.json.tmp"));
        let body = serde_json::to_vec_pretty(p).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)
    }

    fn path(&self, key: &str) -> Option<PathBuf> {
        let ok = key.len() == 16 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        ok.then(|| self.dir.join(format!("{key}.json")))
    }
}

/// Размер контекста по порядку MDL-004: конфигурация → рантайм (значение и поле
/// ответа) → проба переполнения → [`CONSERVATIVE_CONTEXT`]. Нули пропускаются.
pub fn resolve_context(
    config: Option<u32>,
    runtime: Option<(u32, String)>,
    overflow: Option<u32>,
) -> (u32, ContextSource) {
    if let Some(n) = config.filter(|n| *n > 0) {
        return (n, ContextSource::Config);
    }
    if let Some((n, path)) = runtime.filter(|(n, _)| *n > 0) {
        return (n, ContextSource::Runtime { path });
    }
    if let Some(n) = overflow.filter(|n| *n > 0) {
        return (n, ContextSource::OverflowProbe);
    }
    (CONSERVATIVE_CONTEXT, ContextSource::Conservative)
}

/// Предел контекста из текста ошибки сервера (D18):
/// - vLLM, OpenAI: «maximum context length is N tokens»;
/// - llama.cpp: «exceeds the available context size (N tokens)»;
/// - Anthropic: «prompt is too long: M tokens > N maximum».
pub fn parse_context_limit(msg: &str) -> Option<u32> {
    let lower = msg.to_ascii_lowercase();
    for marker in ["maximum context length is", "context size ("] {
        if let Some(i) = lower.find(marker) {
            return first_number(&lower[i + marker.len()..]);
        }
    }
    // «… N tokens > M maximum»: предел — число после `>`.
    if let Some(i) = lower.find("prompt is too long")
        && let Some(j) = lower[i..].find('>')
    {
        return first_number(&lower[i + j + 1..]);
    }
    None
}

/// Первое число в начале текста (после пробелов).
fn first_number(s: &str) -> Option<u32> {
    let s = s.trim_start();
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s[..end].parse().ok().filter(|n| *n > 0)
}

/// Квант из имени файла GGUF: последний сегмент имени (разделители `-`, `.`) вида
/// `Q4_K_M`, `IQ3_XXS`, `Q8_0`, `F16`, `BF16`, `TQ1_0`, `MXFP4`. Суффикс шардов
/// `-00001-of-00002` и каталог отбрасываются.
pub fn quant_from_filename(name: &str) -> Option<String> {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = file
        .strip_suffix(".gguf")
        .or_else(|| file.strip_suffix(".GGUF"))
        .unwrap_or(file);
    stem.split(['-', '.'])
        .rev()
        .find(|seg| is_quant(seg))
        .map(str::to_ascii_uppercase)
}

fn is_quant(seg: &str) -> bool {
    let s = seg.to_ascii_uppercase();
    if matches!(s.as_str(), "F16" | "BF16" | "F32" | "MXFP4") {
        return true;
    }
    let rest = s
        .strip_prefix("IQ")
        .or_else(|| s.strip_prefix("TQ"))
        .or_else(|| s.strip_prefix('Q'));
    let Some(rest) = rest else {
        return false;
    };
    // Цифра разрядности, затем `_` и буквенно-цифровые части: `4_K_M`, `3_XXS`, `8_0`.
    let mut parts = rest.split('_');
    let bits = parts.next().unwrap_or_default();
    !bits.is_empty()
        && bits.bytes().all(|b| b.is_ascii_digit())
        && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}
