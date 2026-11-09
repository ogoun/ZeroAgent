//! Профили моделей на связку «модель, квант, рантайм, версия» и факты эндпоинтов
//! (MDL-003, MDL-004, D13).
//!
//! Зонд ([`crate::probe_endpoint`]) даёт две части:
//! - [`ModelProfile`] — возможности модели, общие для всех эндпоинтов одной связки:
//!   `ZA_HOME/models/profiles/<bundle_key>.json`. Смена модели, кванта или версии
//!   рантайма даёт новый ключ: старый профиль к новой связке не применяется;
//! - [`EndpointFacts`] — то, что зависит от запуска сервера и конфигурации эндпоинта
//!   (контекст сервера и из конфигурации, проба переполнения, слоты, сохранение
//!   слотов): `ZA_HOME/models/endpoints/<endpoint_id>.json`. Два эндпоинта одной
//!   связки с разными `-c`/`-np` друг друга не перезаписывают.
//!
//! Ключей и токенов в файлах нет. Запись атомарная: уникальный временный файл рядом,
//! `sync_all`, `rename`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Профиль связки: возможности модели по зонду, без данных конкретного эндпоинта.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelProfile {
    pub bundle: ModelBundle,
    /// Возможности модели. `caps.context_source` здесь не источник истины: контекст —
    /// факт эндпоинта, действующие возможности даёт [`ProbedEndpoint::caps`].
    pub caps: ProviderCapabilities,
    /// Способ мышления: Anthropic — Models API, OpenAI — по имени модели, прочие —
    /// `Default`.
    pub thinking: ThinkingSupport,
    /// Предел вывода: Anthropic Models API `max_tokens`; у прочих `None`.
    pub max_output_tokens: Option<u32>,
    /// Цены OpenRouter (USD за токен × 1e6). `Gateway` берёт `config.price.or(profile.price)`.
    pub price: Option<Price>,
    pub probed_at: jiff::Timestamp,
    /// Заметки зонда о модели (подсказки, частичные отказы).
    pub notes: Vec<String>,
}

/// Факты эндпоинта: зависят от запуска сервера и конфигурации, а не от связки.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndpointFacts {
    pub endpoint: EndpointId,
    /// Ключ связки, которую обслуживает эндпоинт ([`ModelBundle::key`]).
    pub bundle_key: String,
    /// Действующий контекст по порядку MDL-004.
    pub context_tokens: u32,
    pub context_source: ContextSource,
    /// Контекст, который сообщил рантайм (`n_ctx` сервера, `max_model_len`, Models API),
    /// до учёта конфигурации.
    pub runtime_context: Option<u32>,
    /// Слоты llama.cpp (`total_slots`).
    pub slots: Option<u32>,
    /// Сохранение слотов работает (D29); `None` — ещё не проверялось.
    pub slot_save: Option<bool>,
    pub probed_at: jiff::Timestamp,
    /// Заметки зонда об эндпоинте (контекст, проба переполнения).
    pub notes: Vec<String>,
}

/// Результат успешного зонда: профиль связки и факты эндпоинта.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbedEndpoint {
    pub profile: ModelProfile,
    pub facts: EndpointFacts,
}

impl ProbedEndpoint {
    /// Действующие возможности: профиль связки с источником контекста эндпоинта.
    pub fn caps(&self) -> ProviderCapabilities {
        let mut c = self.profile.caps.clone();
        c.context_source = self.facts.context_source.clone();
        c
    }
}

/// Состояние эндпоинта после зонда.
// Состояние одно на эндпоинт и меняется только зондом: лишние байты вариантов без
// профиля не стоят упаковки в `Box`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum EndpointState {
    Ready(ProbedEndpoint),
    /// Не используется до действия пользователя: нет ключа, ключ отвергнут, нет модели.
    Inactive {
        reason: String,
    },
    /// Сервер не ответил или ответил не по протоколу; зонд повторяется (D40).
    Unreachable {
        reason: String,
    },
}

/// Хранилище профилей связок и фактов эндпоинтов (D13): каталог `ZA_HOME/models`,
/// внутри `profiles/` и `endpoints/`.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    profiles: PathBuf,
    endpoints: PathBuf,
}

/// Счётчик временных файлов процесса: имя временного файла уникально и для
/// параллельной записи одного ключа.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Запись файлов профилей внутри процесса идёт по одной: `rename` поверх файла,
/// который в этот миг заменяет другой поток, на Windows даёт `PermissionDenied`.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

impl ProfileStore {
    /// Открывает каталог моделей (`ZA_HOME/models`), создавая `profiles/` и `endpoints/`.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        let profiles = dir.join("profiles");
        let endpoints = dir.join("endpoints");
        std::fs::create_dir_all(&profiles)?;
        std::fs::create_dir_all(&endpoints)?;
        Ok(Self {
            profiles,
            endpoints,
        })
    }

    /// Профиль по ключу связки. Нет файла, ключ не из 16 hex — `None`; файл повреждён
    /// или принадлежит другой связке — `warn` и `None` (профиль снимется заново).
    pub fn load(&self, key: &str) -> Option<ModelProfile> {
        let path = self.profile_path(key)?;
        let p: ModelProfile = read_json(&path)?;
        if p.bundle.key() != key {
            tracing::warn!(path = %path.display(), "профиль другой связки, не используется");
            return None;
        }
        Some(p)
    }

    /// Атомарная запись профиля связки.
    pub fn save(&self, p: &ModelProfile) -> std::io::Result<()> {
        let key = p.bundle.key();
        let path = self
            .profile_path(&key)
            .ok_or_else(|| std::io::Error::other("ключ связки не из 16 hex"))?;
        write_atomic(&path, p)
    }

    /// Факты эндпоинта по id; чужой или повреждённый файл — `warn` и `None`.
    pub fn load_endpoint(&self, id: &EndpointId) -> Option<EndpointFacts> {
        let path = self.endpoint_path(id);
        let f: EndpointFacts = read_json(&path)?;
        if &f.endpoint != id {
            tracing::warn!(path = %path.display(), "факты другого эндпоинта, не используются");
            return None;
        }
        Some(f)
    }

    /// Атомарная запись фактов эндпоинта.
    pub fn save_endpoint(&self, f: &EndpointFacts) -> std::io::Result<()> {
        write_atomic(&self.endpoint_path(&f.endpoint), f)
    }

    /// Профиль связки и факты эндпоинта одного зонда.
    pub fn save_probed(&self, p: &ProbedEndpoint) -> std::io::Result<()> {
        self.save(&p.profile)?;
        self.save_endpoint(&p.facts)
    }

    fn profile_path(&self, key: &str) -> Option<PathBuf> {
        let ok = key.len() == 16 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        ok.then(|| self.profiles.join(format!("{key}.json")))
    }

    /// Имя файла эндпоинта: id, если он из `[A-Za-z0-9_-]` (до 64 символов), иначе
    /// `id-<blake3[..16]>` — без выхода за каталог и без зарезервированных имён.
    fn endpoint_path(&self, id: &EndpointId) -> PathBuf {
        let s = &id.0;
        let plain = !s.is_empty()
            && s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        let name = if plain {
            s.clone()
        } else {
            format!("id-{}", &blake3::hash(s.as_bytes()).to_hex()[..16])
        };
        self.endpoints.join(format!("{name}.json"))
    }
}

/// JSON из файла: нет файла — `None`, ошибка чтения или разбора — `warn` и `None`.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "файл профиля не прочитан");
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "файл профиля повреждён");
            None
        }
    }
}

/// Атомарная запись (D13): уникальный временный файл рядом
/// (`<имя>.<pid>.<n>.tmp`), `sync_all`, `rename`. Внутри процесса записи идут по
/// одной ([`WRITE_LOCK`]); от другого процесса на Windows `rename` может получить
/// `PermissionDenied` — повторы с паузой 2, 4 … 64 мс, всего около 0,5 с.
fn write_atomic<T: Serialize>(path: &Path, v: &T) -> std::io::Result<()> {
    use std::io::Write;
    let body = serde_json::to_vec_pretty(v).map_err(std::io::Error::other)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| std::io::Error::other("имя файла профиля"))?;
    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!("{name}.{}.{n}.tmp", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
        drop(f);
        let _guard = WRITE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut attempt: u32 = 0;
        loop {
            match std::fs::rename(&tmp, path) {
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && attempt < 10 => {
                    attempt += 1;
                    let ms = 1u64 << attempt.min(6);
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
                r => return r,
            }
        }
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
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
