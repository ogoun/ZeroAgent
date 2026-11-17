//! Проверка политики зависимостей и архитектуры: `cargo xtask policy`.
//!
//! Правила: только crates.io в `Cargo.lock`, запрет перечисленных
//! агентных фреймворков, направление зависимостей между `za-*`
//! (`za-res` — только от `za-protocol` и `za-jail`, `za-core` и `za-agent` — `za-llm` без функций
//! по умолчанию, `za-agent` — только над нижними слоями, и они от него не зависят),
//! лимит размера `za-core` (ARC-011), инструменты сборки (`tiktoken-rs`) — только у
//! `xtask`. R1-C1 (D73): направления `za-jail`, `za-tools`, `za-secrets`; запуск процессов
//! (`std::process::Command`, `tokio::process::Command`) в `crates/*/src` — только в
//! `za-jail` (SEC-006); в `Cargo.lock` нет `openssl-sys`, `native-tls`, `libdbus-sys`.
//! R1-C2 (D107, D129): `za-web` — только от `za-protocol` и `za-core`, от него — только
//! `zero-agent` и `xtask`; от `reqwest` напрямую (любой вид) — только `za-llm` и `za-web`;
//! у `reqwest` в сборке workspace (с тестами) нет функций сжатия, cookie, SOCKS, HTTP/2 и
//! корней ОС; HTTP-клиент `reqwest` строится только в `crates/za-web/src/gateway.rs` и
//! `crates/za-llm`. R1-C2 (D113, D127): `za-extract` — без крейтов workspace, от него —
//! только `zero-agent`; `htmd`, `html5ever`, `markup5ever_rcdom`, `pdf-extract`, `lopdf` —
//! только у `za-extract`.
//! R1-C2 (D114): `za-skills` — только от `za-protocol` и `za-core` (все виды); `za-agent`
//! может зависеть от него, а он от `za-agent` — нет.
//! R1-M (D133, ADR 0007): `za-memory` — только от `za-protocol`, `za-core`, `za-store`,
//! `za-secrets` (все виды), от него — только `zero-agent` и `xtask`; он — третий держатель
//! HTTP-клиента: `reqwest` напрямую, клиент строится только в
//! `crates/za-memory/src/transport.rs`, вне него в крейте `reqwest` не упоминается, атрибут
//! `path` и `include!` в его `src/` запрещены (как у `za-web`). Fix-2 R1-M (M4, M6 ревью
//! задачи 2): файл, где строится клиент в `za-llm`, `za-web`, `za-memory`, содержит
//! `.no_proxy()` и `.redirect(…Policy::none())` (исключения — с причиной); файл клиента
//! держателя — один источник (`HTTP_HOLDERS`), он существует и не реэкспортирует `reqwest`,
//! кроме `Method`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde::Deserialize;

/// Допустимый префикс источника пакета в `Cargo.lock`.
const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";
/// Запрещённые пакеты (чужие агентные фреймворки).
const FORBIDDEN_PACKAGES: &[&str] = &["rig-core", "adk-rust", "autoagents"];
/// Запрещённые пакеты: TLS только встроенный (rustls), без системного D-Bus (R1-C1).
const FORBIDDEN_SYSTEM_PACKAGES: &[&str] = &["openssl-sys", "native-tls", "libdbus-sys"];
/// Крейты, от которых `za-core` зависеть не должен.
const CORE_FORBIDDEN_DEPS: &[&str] = &[
    "za-server",
    "za-peer",
    "za-browser",
    // `za-agent` — общим правилом нижних слоёв ниже.
    "za-res",
    // `reqwest` — общим правилом `REQWEST_USERS` ниже.
    // Крейты R1-C1 стоят над `za-core` (D73).
    "za-jail",
    "za-tools",
    "za-secrets",
];
/// Крейты workspace, от которых может зависеть `za-agent` (ADR 0004).
/// R1-C1 D73: `za-secrets` — да; `za-tools` и `za-jail` — нет ни в каком виде.
/// R1-C2 D114: `za-skills` — да (каталог и активация скиллов).
const AGENT_ALLOWED_ZA_DEPS: &[&str] = &[
    "za-protocol",
    "za-store",
    "za-core",
    "za-llm",
    "za-res",
    "za-secrets",
    "za-skills",
];
/// Крейты, подключающие `za-llm` только без функций: без HTTP и `Gateway`.
const PURE_LLM_USERS: &[&str] = &["za-core", "za-agent"];
/// Крейты `za-*`, от которых может зависеть `za-res`: протокол (D1) и запуск
/// `nvidia-smi` через `za-jail` (R1-C1 D73).
const RES_ALLOWED_ZA_DEPS: &[&str] = &["za-protocol", "za-jail"];
/// Крейты `za-*`, от которых может зависеть `za-tools` (обычные зависимости, R1-C1 D73).
const TOOLS_ALLOWED_ZA_DEPS: &[&str] = &["za-protocol", "za-core", "za-jail"];
/// Где можно запускать процессы, и почему (SEC-006). Путь — от корня, через `/`.
const PROCESS_ALLOWED: &[(&str, &str)] = &[
    (
        "crates/za-jail/",
        "единственная точка запуска процессов: дерево, тайм-аут, окружение (R1-C1 D84)",
    ),
    (
        "crates/zero-agent/build.rs",
        "сценарий сборки: `$RUSTC --version` при компиляции, в продукт не входит",
    ),
];
/// Крейты только для `xtask` (R1-B2 D64): счёт токенов промптов в CI, не в продукте.
const XTASK_ONLY_DEPS: &[&str] = &["tiktoken-rs"];
/// Крейты workspace, от которых может зависеть `za-skills` (все виды, R1-C2 D114): разбор,
/// обход и загрузка — над `za-core`, без цикла с `za-agent` даже в тестах.
const SKILLS_ALLOWED_ZA_DEPS: &[&str] = &["za-protocol", "za-core"];
/// Крейты `za-*`, от которых может зависеть сетевой шлюз `za-web` (все виды, R1-C2).
const WEB_ALLOWED_ZA_DEPS: &[&str] = &["za-protocol", "za-core"];
/// Кто может зависеть от `za-web` (все виды): бинарник и `xtask` (бюджеты промптов).
const WEB_USERS: &[&str] = &["zero-agent", "xtask"];
/// Крейты `za-*`, от которых может зависеть клиент памяти `za-memory` (все виды, R1-M D133):
/// трейты и порт `za-core`, outbox `za-store`, маска `za-secrets`; не цикл, не модели, не
/// инструменты, не шлюз.
const MEMORY_ALLOWED_ZA_DEPS: &[&str] = &["za-protocol", "za-core", "za-store", "za-secrets"];
/// Кто может зависеть от `za-memory` (все виды, D133): бинарник и `xtask` (бюджет промптов,
/// снимки формы ответов). `za-agent` видит память только через порт `za-core` (D138).
const MEMORY_USERS: &[&str] = &["zero-agent", "xtask"];
/// Кто может зависеть от `za-extract` (все виды, D113, D127): только бинарник — его скрытая
/// подкоманда `extract` и есть подпроцесс разбора.
const EXTRACT_USERS: &[&str] = &["zero-agent"];
/// Разборщики враждебного HTML и PDF — прямая зависимость (все виды) только у `za-extract`
/// (D113, D127): в процессе агента рекурсивный обход DOM переполнил бы стек на глубокой
/// вложенности, а паника или вечный цикл разборщика PDF — уронили бы или повесили агента.
const EXTRACT_ONLY_DEPS: &[&str] = &[
    "htmd",
    "html5ever",
    "markup5ever_rcdom",
    "pdf-extract",
    "lopdf",
];
/// Функции `reqwest`, которых не должно быть в сборке (D107, D129): распаковка тела
/// (`Accept-Encoding`), cookie, SOCKS и HTTP/2–3, корни хранилища ОС и системный TLS.
const REQWEST_FORBIDDEN_FEATURES: &[&str] = &[
    "gzip",
    "brotli",
    "deflate",
    "zstd",
    "cookies",
    "socks",
    "http2",
    "http3",
    "rustls-tls-native-roots",
    "rustls-tls-native-roots-no-provider",
    "native-tls",
    "default-tls",
];
/// Кто может зависеть от `reqwest` напрямую (все виды; Ruling I2 ревью задачи 6, D129):
/// транспорт моделей, сетевой шлюз и клиент памяти (R1-M D133, третий держатель).
const REQWEST_USERS: &[&str] = &["za-llm", "za-web", "za-memory"];
/// Крейты, где HTTP-клиент `reqwest` можно строить в любом файле, и почему (D129). Путь —
/// от корня, через `/`. Прочие места клиента — файлы держателей [`HTTP_HOLDERS`]: вместе
/// они — [`http_client_allowed`].
const HTTP_CLIENT_CRATES: &[(&str, &str)] = &[("crates/za-llm/", "транспорт моделей (R1-A)")];

/// Держатель HTTP-клиента со строгим подмножеством путей `reqwest` вне своего файла клиента
/// (D129, R1-M D133).
struct HttpHolder {
    /// Крейт — префикс пути от корня (`crates/za-web/`).
    prefix: &'static str,
    /// Файл клиента от `src/` крейта: единственное место, где строится `reqwest::Client`
    /// ([`HttpHolder::client_file`] — единственный источник для [`check_http_client`],
    /// [`check_holder_reexports`] и [`check_client_files_present`]; M6 ревью задачи 2 R1-M).
    file: &'static str,
    /// Пути `reqwest`, допустимые в остальных файлах крейта (строка после имени `reqwest`
    /// начинается с одного из них).
    paths: &'static [&'static str],
    /// Почему клиент здесь (D129).
    why: &'static str,
}

impl HttpHolder {
    /// Путь файла клиента от корня (`crates/za-web/src/gateway.rs`).
    fn client_file(&self) -> String {
        format!("{}src/{}", self.prefix, self.file)
    }

    /// Имя крейта (`za-web`).
    fn krate(&self) -> &'static str {
        self.prefix
            .trim_start_matches("crates/")
            .trim_end_matches('/')
    }
}

/// Держатели HTTP-клиента (D129, R1-M D133). У `za-web` вне `gateway.rs` допустимы типы
/// резолвера (`addr.rs`, тестовая подмена `lib.rs`) и код статуса (`fetch.rs`); у
/// `za-memory` — ничего: метод HTTP другие модули берут реэкспортом `transport::Method`. Для
/// этих же крейтов в `src/` запрещены атрибут `path` и `include!` ([`check_holder_includes`]).
const HTTP_HOLDERS: &[HttpHolder] = &[
    HttpHolder {
        prefix: "crates/za-web/",
        file: "gateway.rs",
        paths: &["::dns::", "::StatusCode"],
        why: "сетевой шлюз: фильтр адресов в резолвере, без прокси и cookie (D107, D108)",
    },
    HttpHolder {
        prefix: "crates/za-memory/",
        file: "transport.rs",
        paths: &[],
        why: "клиент памяти: без прокси и редиректов, чувствительный Authorization (R1-M D133)",
    },
];

/// Можно ли строить HTTP-клиент `reqwest` в исходнике `rel` (D129): файл клиента держателя
/// [`HTTP_HOLDERS`] или крейт из [`HTTP_CLIENT_CRATES`].
fn http_client_allowed(rel: &str) -> bool {
    HTTP_HOLDERS.iter().any(|h| rel == h.client_file())
        || HTTP_CLIENT_CRATES
            .iter()
            .any(|(prefix, _)| rel.starts_with(prefix))
}

/// Свойство клиента «без прокси окружения» (образец в коде — вызов `.no_proxy()`).
const NO_PROXY: &str = ".no_proxy()";
/// Свойство клиента «без редиректов `reqwest`» (образец — `.redirect(` и `Policy::none()` в
/// одной строке).
const NO_REDIRECT: &str = "redirect(…Policy::none())";
/// Свойства, без которых `reqwest::Client` строить нельзя ни в одном крейте из
/// [`REQWEST_USERS`] (M4 ревью задачи 2 R1-M): свойство и что оно даёт.
const CLIENT_PROPERTIES: &[(&str, &str)] = &[
    (
        NO_PROXY,
        "запросы и токены не уходят прокси из HTTP_PROXY/ALL_PROXY (D107, D133)",
    ),
    (
        NO_REDIRECT,
        "3xx не повторяется на другой хост с телом и заголовками (D108, D133)",
    ),
];
/// Исключения из [`CLIENT_PROPERTIES`]: файл, свойство и причина. Поведение этих мест не
/// менялось (бриф fix-2 R1-M, M4): исключение записано, а не исправлено.
const CLIENT_PROPERTY_EXEMPT: &[(&str, &str, &str)] = &[(
    "crates/za-llm/src/probe.rs",
    NO_REDIRECT,
    "зонд R0 probe_llama: GET /props локального llama-server без учётных данных (Authorization нет), переход по 3xx секретов не уносит; транспорт коннекторов R1-A (http/transport.rs) редиректы выключает, а этот зонд R1-A не трогал",
)];
/// Что можно реэкспортировать из `reqwest` в файле клиента держателя (M6 ревью задачи 2
/// R1-M): метод HTTP для других модулей `za-memory` (`transport::Method`).
const HOLDER_REEXPORT_ALLOWED: &[&str] = &["reqwest::Method"];

/// Нарушение политики.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation(pub String);

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Метаданные workspace (подмножество `cargo metadata --no-deps`).
#[derive(Debug, Deserialize)]
pub struct Metadata {
    pub packages: Vec<Package>,
}

#[derive(Debug, Deserialize)]
pub struct Package {
    pub name: String,
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug, Deserialize)]
pub struct Dependency {
    pub name: String,
    /// `default-features` зависимости; в `cargo metadata` поле есть всегда.
    #[serde(default = "default_true")]
    pub uses_default_features: bool,
    /// `None` — обычная зависимость, иначе `"dev"` или `"build"`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Явно включённые функции зависимости (`features = [...]`).
    #[serde(default)]
    pub features: Vec<String>,
    /// Имя зависимости в крейте, если оно отличается от имени пакета
    /// (`http = { package = "reqwest" }`); `name` — всегда имя пакета.
    #[serde(default)]
    pub rename: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Проверяет источники и имена пакетов в тексте `Cargo.lock`.
pub fn check_lock_sources(lock: &str) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut name = String::new();
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            name.clear();
        } else if let Some(v) = value_of(line, "name") {
            name = v.to_string();
            if FORBIDDEN_PACKAGES.contains(&v) {
                out.push(Violation(format!("запрещённый пакет `{v}`")));
            }
            if FORBIDDEN_SYSTEM_PACKAGES.contains(&v) {
                out.push(Violation(format!(
                    "запрещённый пакет `{v}`: TLS только rustls, без системных OpenSSL и D-Bus"
                )));
            }
        } else if let Some(src) = value_of(line, "source")
            && !src.starts_with(CRATES_IO)
        {
            out.push(Violation(format!(
                "пакет `{name}`: недопустимый источник `{src}`"
            )));
        }
    }
    out
}

/// Значение строки вида `key = "value"`.
fn value_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.strip_prefix(key)?
        .trim_start()
        .strip_prefix('=')?
        .trim()
        .strip_prefix('"')?
        .strip_suffix('"')
}

/// Проверяет направление зависимостей между крейтами workspace.
pub fn check_direction(metadata: &Metadata) -> Vec<Violation> {
    let mut out = Vec::new();
    for pkg in &metadata.packages {
        for dep in &pkg.dependencies {
            if pkg.name == "za-core" && CORE_FORBIDDEN_DEPS.contains(&dep.name.as_str()) {
                out.push(Violation(format!(
                    "za-core не должен зависеть от {}",
                    dep.name
                )));
            }
            // Все виды, включая dev: `za-llm` сам зависит от `za-res` (D1).
            if pkg.name == "za-res"
                && dep.name.starts_with("za-")
                && !RES_ALLOWED_ZA_DEPS.contains(&dep.name.as_str())
            {
                out.push(Violation(format!(
                    "za-res не должен зависеть от {} (допустимы {})",
                    dep.name,
                    RES_ALLOWED_ZA_DEPS.join(", ")
                )));
            }
            // Все виды: `za-jail` — нижний слой без крейтов workspace (D73).
            if pkg.name == "za-jail" && is_workspace_crate(&dep.name) {
                out.push(Violation(format!(
                    "za-jail не должен зависеть от {}",
                    dep.name
                )));
            }
            // Обычные зависимости: `za-secrets` — без крейтов workspace и внешних (D73).
            if pkg.name == "za-secrets" && dep.kind.as_deref() != Some("dev") {
                out.push(Violation(format!(
                    "za-secrets не должен иметь зависимостей ({})",
                    dep.name
                )));
            }
            if pkg.name == "za-tools"
                && dep.kind.as_deref() != Some("dev")
                && is_workspace_crate(&dep.name)
                && !TOOLS_ALLOWED_ZA_DEPS.contains(&dep.name.as_str())
            {
                out.push(Violation(format!(
                    "za-tools не должен зависеть от {} (допустимы {})",
                    dep.name,
                    TOOLS_ALLOWED_ZA_DEPS.join(", ")
                )));
            }
            // Только то, что попадает в сборку: dev-зависимости тестов не в счёт.
            // `za-llm` — без функций по умолчанию и без явных (`http`, `gateway`, `probe`).
            if PURE_LLM_USERS.contains(&pkg.name.as_str())
                && dep.name == "za-llm"
                && dep.kind.as_deref() != Some("dev")
            {
                if dep.uses_default_features {
                    out.push(Violation(format!(
                        "{} подключает za-llm с функциями по умолчанию (нужно default-features = false)",
                        pkg.name
                    )));
                } else if !dep.features.is_empty() {
                    out.push(Violation(format!(
                        "{} подключает za-llm с функциями {:?} (допустим только пустой набор)",
                        pkg.name, dep.features
                    )));
                }
            }
            // Все виды: нижние слои не видят цикл даже в тестах (ADR 0004).
            if AGENT_ALLOWED_ZA_DEPS.contains(&pkg.name.as_str()) && dep.name == "za-agent" {
                out.push(Violation(format!(
                    "{} не должен зависеть от za-agent",
                    pkg.name
                )));
            }
            if pkg.name == "za-agent"
                && is_workspace_crate(&dep.name)
                && !AGENT_ALLOWED_ZA_DEPS.contains(&dep.name.as_str())
            {
                out.push(Violation(format!(
                    "za-agent не должен зависеть от {} (допустимы {})",
                    dep.name,
                    AGENT_ALLOWED_ZA_DEPS.join(", ")
                )));
            }
            // Все виды (Ruling I2 ревью задачи 6, D129): HTTP-клиент `reqwest` — только у
            // транспорта моделей и сетевого шлюза. Иначе второй клиент вне шлюза строится и
            // без `Client::builder` в исходнике. `za-agent` видит модель только через
            // `ModelPort`, `za-core` — без HTTP.
            if dep.name == "reqwest" && !REQWEST_USERS.contains(&pkg.name.as_str()) {
                out.push(Violation(format!(
                    "{} не должен зависеть от reqwest (только {}; D129: HTTP-клиент вне шлюза — обход фильтра адресов)",
                    pkg.name,
                    REQWEST_USERS.join(", ")
                )));
            }
            // Все виды и все манифесты (Minor 2 повторного ревью финального прохода):
            // переименованный `reqwest` прячет имя крейта от стража исходников
            // (`check_http_client`), и внутри `za-web` клиент строился бы вне шлюза.
            if dep.name == "reqwest"
                && let Some(alias) = &dep.rename
            {
                out.push(Violation(format!(
                    "{}: зависимость reqwest переименована в `{alias}` (D129: страж HTTP-клиента ищет имя крейта в исходнике)",
                    pkg.name
                )));
            }
            // Все виды: и dev-зависимость тянет крейт в сборку тестов продукта.
            if pkg.name != "xtask" && XTASK_ONLY_DEPS.contains(&dep.name.as_str()) {
                out.push(Violation(format!(
                    "{} не должен зависеть от {} (только xtask)",
                    pkg.name, dep.name
                )));
            }
            // `za-shell` — чистый разбор без зависимостей; продукт видит его только
            // через `za_core::shell` (R1-C1 D99).
            if pkg.name == "za-shell" && dep.kind.as_deref() != Some("dev") {
                out.push(Violation(format!(
                    "za-shell не должен иметь зависимостей ({})",
                    dep.name
                )));
            }
            if dep.name == "za-shell" && pkg.name != "za-core" {
                out.push(Violation(format!(
                    "{} не должен зависеть от za-shell (только za-core, D99)",
                    pkg.name
                )));
            }
            // Все виды: шлюз — над `za-core`, без цикла и без обходных путей (D107).
            if pkg.name == "za-web"
                && is_workspace_crate(&dep.name)
                && !WEB_ALLOWED_ZA_DEPS.contains(&dep.name.as_str())
            {
                out.push(Violation(format!(
                    "za-web не должен зависеть от {} (допустимы {})",
                    dep.name,
                    WEB_ALLOWED_ZA_DEPS.join(", ")
                )));
            }
            if dep.name == "za-web" && !WEB_USERS.contains(&pkg.name.as_str()) {
                out.push(Violation(format!(
                    "{} не должен зависеть от za-web (только {})",
                    pkg.name,
                    WEB_USERS.join(", ")
                )));
            }
            // Все виды (R1-M D133): клиент памяти — над `za-core`, без цикла и моделей; от
            // него — только бинарник и `xtask`.
            if pkg.name == "za-memory"
                && is_workspace_crate(&dep.name)
                && !MEMORY_ALLOWED_ZA_DEPS.contains(&dep.name.as_str())
            {
                out.push(Violation(format!(
                    "za-memory не должен зависеть от {} (допустимы {})",
                    dep.name,
                    MEMORY_ALLOWED_ZA_DEPS.join(", ")
                )));
            }
            if dep.name == "za-memory" && !MEMORY_USERS.contains(&pkg.name.as_str()) {
                out.push(Violation(format!(
                    "{} не должен зависеть от za-memory (только {})",
                    pkg.name,
                    MEMORY_USERS.join(", ")
                )));
            }
            // Все виды (D114): скиллы — над `za-core`; от `za-agent` — ни в каком виде.
            if pkg.name == "za-skills"
                && is_workspace_crate(&dep.name)
                && !SKILLS_ALLOWED_ZA_DEPS.contains(&dep.name.as_str())
            {
                out.push(Violation(format!(
                    "za-skills не должен зависеть от {} (допустимы {})",
                    dep.name,
                    SKILLS_ALLOWED_ZA_DEPS.join(", ")
                )));
            }
            // Все виды (D113, D127): извлекатель — лист без крейтов workspace, его зовёт
            // только бинарник в подпроцессе.
            if pkg.name == "za-extract" && is_workspace_crate(&dep.name) {
                out.push(Violation(format!(
                    "za-extract не должен зависеть от {} (без крейтов workspace)",
                    dep.name
                )));
            }
            if dep.name == "za-extract" && !EXTRACT_USERS.contains(&pkg.name.as_str()) {
                out.push(Violation(format!(
                    "{} не должен зависеть от za-extract (только {}: разбор — в подпроцессе, D127)",
                    pkg.name,
                    EXTRACT_USERS.join(", ")
                )));
            }
            if pkg.name != "za-extract" && EXTRACT_ONLY_DEPS.contains(&dep.name.as_str()) {
                out.push(Violation(format!(
                    "{} не должен зависеть от {} (только za-extract, в подпроцессе: D127)",
                    pkg.name, dep.name
                )));
            }
            if pkg.name == "za-protocol" && dep.name.starts_with("za-") {
                out.push(Violation(format!(
                    "za-protocol не должен зависеть от {}",
                    dep.name
                )));
            }
        }
    }
    out
}

/// Итоговые функции крейта `krate` из вывода `cargo tree --prefix none -f "{p}|{f}"`;
/// `None` — крейта в дереве нет. Повторы (`(*)`) объединяются.
pub fn parse_tree_features(tree: &str, krate: &str) -> Option<Vec<String>> {
    let mut found: Option<Vec<String>> = None;
    for line in tree.lines() {
        let Some((pkg, feats)) = line.split_once('|') else {
            continue;
        };
        if pkg.split(' ').next() != Some(krate) {
            continue;
        }
        let list = found.get_or_insert_with(Vec::new);
        let feats = feats.trim_end().trim_end_matches("(*)").trim();
        for f in feats.split(',').map(str::trim).filter(|f| !f.is_empty()) {
            if !list.iter().any(|x| x == f) {
                list.push(f.to_string());
            }
        }
    }
    found
}

/// Нарушение, если в итоговой сборке `pkg` у `za-llm` включена хоть одна функция.
pub fn check_resolved_llm_features(pkg: &str, features: &[String]) -> Option<Violation> {
    (!features.is_empty()).then(|| {
        Violation(format!(
            "{pkg}: в сборке `-p {pkg}` у za-llm включены функции {features:?} (допустим только пустой набор)"
        ))
    })
}

/// Нарушение, если в сборке workspace у `reqwest` есть функция из
/// [`REQWEST_FORBIDDEN_FEATURES`] (D129): защита от объединения функций Cargo другим крейтом.
pub fn check_reqwest_features(features: &[String]) -> Option<Violation> {
    let bad: Vec<&str> = features
        .iter()
        .map(String::as_str)
        .filter(|f| REQWEST_FORBIDDEN_FEATURES.contains(f))
        .collect();
    (!bad.is_empty()).then(|| {
        Violation(format!(
            "reqwest: в сборке workspace включены функции {bad:?} (запрещены: сжатие, cookie, SOCKS, HTTP/2, корни ОС; D129)"
        ))
    })
}

/// Итоговые функции `za-llm` в сборке `-p pkg` (обычные зависимости, все цели).
///
/// `resolve` из `cargo metadata` для этого не годится: он объединяет функции по всему
/// workspace (бинарник `zero-agent` включает `gateway`), а `cargo tree -p` разрешает их
/// так же, как `cargo build -p` — с собственным `[features]` и транзитивными включениями.
fn resolved_llm_features(root: &Path, pkg: &str) -> Result<Vec<String>, String> {
    resolved_features(root, &["-p", pkg, "-e", "normal"], "za-llm")
}

/// Итоговые функции `reqwest` в сборке всего workspace вместе с тестами (`-e normal,dev`:
/// функции разрешаются как у `cargo test --workspace`) — надмножество сборки любого крейта.
fn resolved_reqwest_features(root: &Path) -> Result<Vec<String>, String> {
    resolved_features(root, &["--workspace", "-e", "normal,dev"], "reqwest")
}

/// Итоговые функции крейта `krate` в выводе `cargo tree <sel>` (все цели).
fn resolved_features(root: &Path, sel: &[&str], krate: &str) -> Result<Vec<String>, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let what = format!("cargo tree {}", sel.join(" "));
    let res = Command::new(cargo)
        .args(["tree", "--locked", "--color", "never"])
        .args(sel)
        .args(["--target", "all", "--prefix", "none", "-f", "{p}|{f}"])
        // CI задаёт CARGO_TERM_COLOR=always: без явного отказа от цвета маркер `(*)` приходит
        // с ANSI-кодами и разбирается как имя функции.
        .env("CARGO_TERM_COLOR", "never")
        .current_dir(root)
        .output()
        .map_err(|e| format!("{what}: {e}"))?;
    if !res.status.success() {
        return Err(format!("{what}: {}", String::from_utf8_lossy(&res.stderr)));
    }
    Ok(parse_tree_features(&String::from_utf8_lossy(&res.stdout), krate).unwrap_or_default())
}

/// Построение HTTP-клиента `reqwest` в исходнике `rel` вне [`http_client_allowed`] (D129):
/// `Client::builder`, `ClientBuilder`, `Client::new(`, `Client::default(`, упоминание типа
/// `reqwest::Client`, `reqwest::get(`, `reqwest::blocking`. Имя должно начинаться словом:
/// `MemoryClient::new(` — не клиент `reqwest`. Строки-комментарии не считаются. Главный
/// страж — зависимость (`REQWEST_USERS`): этот проверяет файлы внутри держателей.
///
/// Внутри держателя из [`HTTP_HOLDERS`] (`za-web`, `za-memory`) вне его файла клиента
/// действует ещё строгое подмножество (Minor 1 финального ревью безопасности R1-C2, m1 ревью
/// fix-5/6; R1-M D133): из `reqwest` допустимы только пути держателя. Импорт `use
/// reqwest::{Client}` с `Default::default()` образцы выше не видят, а клиент по умолчанию —
/// системный резолвер, прокси из окружения и редиректы: обход фильтра D108 и правил D133.
/// Перечня обходов здесь нет: любое другое упоминание крейта (группа импорта, псевдоним,
/// `extern crate`, иной тип) — нарушение.
pub fn check_http_client(rel: &str, text: &str) -> Vec<Violation> {
    const PATTERNS: &[&str] = &[
        "Client::builder",
        "ClientBuilder",
        "Client::new(",
        "Client::default(",
        "reqwest::Client",
        "reqwest::get(",
        "reqwest::blocking",
    ];
    if http_client_allowed(rel) {
        return Vec::new();
    }
    let holder = HTTP_HOLDERS.iter().find(|h| rel.starts_with(h.prefix));
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let code = line.trim_start();
        if code.starts_with("//") {
            continue;
        }
        let hit = PATTERNS
            .iter()
            .any(|p| code.match_indices(p).any(|(at, _)| word_start(code, at)));
        if hit {
            let places = HTTP_HOLDERS
                .iter()
                .map(|h| h.client_file().trim_start_matches("crates/").to_owned())
                .chain(HTTP_CLIENT_CRATES.iter().map(|(p, _)| {
                    p.trim_start_matches("crates/")
                        .trim_end_matches('/')
                        .to_owned()
                }))
                .collect::<Vec<_>>()
                .join(", ");
            out.push(Violation(format!(
                "{rel}:{}: HTTP-клиент reqwest вне {places} (D129, D133: обход фильтра адресов и правил клиента)",
                i + 1
            )));
            continue;
        }
        let Some(h) = holder else {
            continue;
        };
        if mentions_reqwest(code, h.paths) {
            let allowed = if h.paths.is_empty() {
                "ничего".to_owned()
            } else {
                h.paths
                    .iter()
                    .map(|p| format!("reqwest{p}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            out.push(Violation(format!(
                "{rel}:{}: в {} вне {} из reqwest допустимо только {allowed} (D129, D133: клиент — только в {})",
                i + 1,
                h.krate(),
                h.file,
                h.file,
            )));
        }
    }
    out
}

/// Символ идентификатора Rust (для границ слова в текстовых стражах).
fn ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Слово начинается в `code` с позиции `at`: перед ним не символ идентификатора.
fn word_start(code: &str, at: usize) -> bool {
    code[..at].chars().next_back().is_none_or(|c| !ident(c))
}

/// Упоминание крейта `reqwest` словом (`reqwest::…`, `::reqwest`, `use reqwest as r`), кроме
/// путей `allowed` (строка после имени начинается с одного из них).
fn mentions_reqwest(code: &str, allowed: &[&str]) -> bool {
    code.match_indices("reqwest").any(|(at, w)| {
        let rest = &code[at + w.len()..];
        word_start(code, at)
            && !rest.chars().next().is_some_and(ident)
            && !allowed.iter().any(|p| rest.starts_with(p))
    })
}

/// Строки кода `text` с номерами (с 1), без строк-комментариев.
fn code_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim_start()))
        .filter(|(_, code)| !code.starts_with("//"))
}

/// Свойства клиента в исходнике `rel` крейта из [`REQWEST_USERS`] (M4 ревью задачи 2 R1-M):
/// файл, где строится `reqwest::Client` (`Client::builder` словом вне комментария), содержит
/// в коде вызов `.no_proxy()` и `.redirect(…Policy::none())` (оба — в одной строке), иначе
/// нарушение, кроме [`CLIENT_PROPERTY_EXEMPT`]. Без теста с `HTTP_PROXY` удаление
/// `.no_proxy()` иначе прошло бы незамеченным: CI без прокси, серверы — на `127.0.0.1`.
///
/// Граница: проверка текстовая и на файл, а не на каждый вызов `builder`. У
/// `za-llm/src/http/transport.rs` вызов `no_proxy()` условный — только у локального
/// эндпоинта: облачные идут через `HTTPS_PROXY` по решению R1-A (D26); страж видит вызов, а
/// условие — решение R1-A, не нарушение.
pub fn check_client_properties(rel: &str, text: &str) -> Vec<Violation> {
    let in_user = REQWEST_USERS
        .iter()
        .any(|u| rel.starts_with(&format!("crates/{u}/")));
    if !in_user {
        return Vec::new();
    }
    let builds = code_lines(text).any(|(_, code)| {
        code.match_indices("Client::builder")
            .any(|(at, _)| word_start(code, at))
    });
    if !builds {
        return Vec::new();
    }
    let has = |prop: &str| {
        code_lines(text).any(|(_, code)| match prop {
            NO_PROXY => code.contains(NO_PROXY),
            _ => code
                .split_once(".redirect(")
                .is_some_and(|(_, rest)| rest.contains("Policy::none()")),
        })
    };
    CLIENT_PROPERTIES
        .iter()
        .filter(|(prop, _)| {
            !CLIENT_PROPERTY_EXEMPT
                .iter()
                .any(|(file, p, _)| rel == *file && p == prop)
        })
        .filter(|(prop, _)| !has(prop))
        .map(|(prop, why)| {
            Violation(format!(
                "{rel}: HTTP-клиент reqwest без {prop}: {why} (M4 ревью задачи 2 R1-M; исключение — только с причиной в CLIENT_PROPERTY_EXEMPT)"
            ))
        })
        .collect()
}

/// Реэкспорт `reqwest` из файла клиента держателя [`HTTP_HOLDERS`] (M6 ревью задачи 2 R1-M):
/// `pub use …reqwest…` и `pub type … = …reqwest…` с любой видимостью (`pub(crate)`,
/// `pub(super)`) — нарушение, кроме [`HOLDER_REEXPORT_ALLOWED`] (`pub use reqwest::Method;`).
/// Иначе `pub use reqwest::Client as Http` или `pub type Http = reqwest::Client` дали бы
/// другим модулям крейта клиент по умолчанию через `Http::default()` (системный прокси,
/// редиректы), а [`check_http_client`] этого имени не видит. Инструкция может идти на
/// несколько строк (до `;`).
///
/// Граница: только инструкции `use` и `type`, начатые с видимости в начале строки;
/// двухшаговый псевдоним (`use reqwest::Client as C;` и `pub use C as Http;`) и публичное
/// поле типа клиента страж не видит — их ловит ревью.
pub fn check_holder_reexports(rel: &str, text: &str) -> Vec<Violation> {
    let Some(h) = HTTP_HOLDERS.iter().find(|h| rel == h.client_file()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut lines = code_lines(text);
    while let Some((n, code)) = lines.next() {
        let Some(item) = strip_visibility(code) else {
            continue;
        };
        if !(item.starts_with("use ") || item.starts_with("type ")) {
            continue;
        }
        let mut stmt = item.to_owned();
        while !stmt.contains(';') {
            let Some((_, next)) = lines.next() else {
                break;
            };
            stmt.push(' ');
            stmt.push_str(next);
        }
        if !mentions_reqwest(&stmt, &[]) {
            continue;
        }
        let tight: String = stmt.chars().filter(|c| !c.is_whitespace()).collect();
        let allowed = HOLDER_REEXPORT_ALLOWED
            .iter()
            .any(|p| tight == format!("use{p};"));
        if !allowed {
            out.push(Violation(format!(
                "{rel}:{n}: реэкспорт reqwest из {} (допустимо только {}): другие модули {} собрали бы клиент в обход {} (D129, D133)",
                h.file,
                HOLDER_REEXPORT_ALLOWED
                    .iter()
                    .map(|p| format!("pub use {p};"))
                    .collect::<Vec<_>>()
                    .join(", "),
                h.krate(),
                h.file,
            )));
        }
    }
    out
}

/// Остаток строки после видимости `pub`, `pub(crate)`, `pub(super)`, `pub(in …)`; не
/// публичная строка — `None`.
fn strip_visibility(code: &str) -> Option<&str> {
    let rest = code.strip_prefix("pub")?;
    if let Some(inner) = rest.strip_prefix('(') {
        let close = inner.find(')')?;
        return Some(inner[close + 1..].trim_start());
    }
    rest.starts_with(char::is_whitespace)
        .then(|| rest.trim_start())
}

/// Файл клиента каждого держателя [`HTTP_HOLDERS`] есть среди исходников `rels` (M6 ревью
/// задачи 2 R1-M): после переименования файла без правки таблицы освобождение и проверки
/// свойств молча указывали бы на пустое место.
pub fn check_client_files_present<'a>(
    rels: impl IntoIterator<Item = &'a str> + Clone,
) -> Vec<Violation> {
    HTTP_HOLDERS
        .iter()
        .filter(|h| !rels.clone().into_iter().any(|r| r == h.client_file()))
        .map(|h| {
            Violation(format!(
                "{}: файл клиента держателя HTTP {} не найден ({}): переименован — поправьте HTTP_HOLDERS (M6 ревью задачи 2 R1-M)",
                h.client_file(),
                h.krate(),
                h.why,
            ))
        })
        .collect()
}

/// Подключение файла в обход `src/` в исходнике `rel` держателя HTTP-клиента из
/// [`HTTP_HOLDERS`] — `za-web`, `za-memory` (Minor 2 повторного ревью финального прохода
/// R1-C2, D129; R1-M D133): атрибут `path` (`#[path = …]`, `#![path = …]`, аргумент
/// `cfg_attr`, отдельная строка многострочного атрибута) и `include!`. Страж исходников
/// сканирует только `crates/*/src` и `build.rs`, а модуль с атрибутом `path` или `include!`
/// взял бы код из любого файла, в том числе вне крейта. Строка сравнивается без пробелов
/// (`# [ path = …]`, `include ! (…)`); признак атрибута — `path=` перед строковым литералом,
/// поэтому присваивание `path = format!(…)` не нарушение. Строки-комментарии не считаются.
/// Правило — для всех файлов держателя, и для файла клиента: клиент строится только в нём
/// самом. Тесты крейтов (`tests/`) подключают `test-support/*.rs` через `#[path]` законно:
/// их страж не сканирует.
pub fn check_holder_includes(rel: &str, text: &str) -> Vec<Violation> {
    let Some(h) = HTTP_HOLDERS.iter().find(|h| rel.starts_with(h.prefix)) else {
        return Vec::new();
    };
    let krate = h.krate();
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let code = line.trim_start();
        if code.starts_with("//") {
            continue;
        }
        let tight: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        let hit = tight.contains("#[path")
            || tight.contains("#![path")
            || tight.contains("include!")
            || ["path=\"", "path=r\"", "path=r#"]
                .iter()
                .any(|p| tight.contains(p));
        if hit {
            out.push(Violation(format!(
                "{rel}:{}: в {krate} атрибут path и include! запрещены (D129: код крейта — только модули src/, их видит страж HTTP-клиента)",
                i + 1
            )));
        }
    }
    out
}

/// Запуск процессов в исходнике `rel` (путь от корня через `/`) вне [`PROCESS_ALLOWED`]:
/// `std::process::Command`, `tokio::process::Command`, `Command::new`, импорт `Command`
/// из `process::{…}`, `nix`-запуск (`posix_spawn`, `unistd::exec*`, `unistd::fork`). Строки-
/// комментарии не считаются (блочные `/* */` могут дать ложное срабатывание).
pub fn check_process_spawn(rel: &str, text: &str) -> Vec<Violation> {
    if PROCESS_ALLOWED
        .iter()
        .any(|(prefix, _)| rel.starts_with(prefix))
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let code = line.trim_start();
        if code.starts_with("//") {
            continue;
        }
        let hit = code.contains("process::Command")
            || code.contains("Command::new")
            || (code.contains("process::{") && code.contains("Command"))
            // Безопасные API запуска `nix` (ревью задачи 5, M5).
            || code.contains("posix_spawn")
            || code.contains("unistd::exec")
            || code.contains("unistd::fork");
        if hit {
            out.push(Violation(format!(
                "{rel}:{}: запуск процесса вне za-jail (только через za_jail::Runner, SEC-006)",
                i + 1
            )));
        }
    }
    out
}

/// Исходники `crates/*/src/**/*.rs` и `crates/*/build.rs`: путь от корня через `/` и текст.
fn crate_sources(root: &Path) -> std::io::Result<Vec<(String, String)>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(root, &path, out)?;
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel, std::fs::read_to_string(&path)?));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    for krate in std::fs::read_dir(root.join("crates"))? {
        let krate = krate?.path();
        let src = krate.join("src");
        if src.is_dir() {
            walk(root, &src, &mut out)?;
        }
        // Сценарий сборки тоже может запускать процессы.
        let build = krate.join("build.rs");
        if build.is_file() {
            let rel = format!(
                "crates/{}/build.rs",
                krate.file_name().unwrap_or_default().to_string_lossy()
            );
            out.push((rel, std::fs::read_to_string(&build)?));
        }
    }
    Ok(out)
}

/// Крейт workspace: `za-*` или бинарник `zero-agent`.
fn is_workspace_crate(name: &str) -> bool {
    name.starts_with("za-") || name == "zero-agent"
}

/// Проверяет, что размер `za-core` в строках не превышает лимит.
pub fn check_core_size(lines: usize, limit: usize) -> Option<Violation> {
    check_size("za-core", lines, limit)
}

/// Проверяет, что размер крейта в строках не превышает лимит.
pub fn check_size(krate: &str, lines: usize, limit: usize) -> Option<Violation> {
    (lines > limit).then(|| Violation(format!("{krate}: {lines} строк при лимите {limit}")))
}

/// Лимит строк из файла `xtask/<file>`.
fn read_limit(root: &Path, file: &str) -> Result<usize, String> {
    std::fs::read_to_string(root.join("xtask").join(file))
        .map_err(|e| format!("{file}: {e}"))?
        .trim()
        .parse()
        .map_err(|e| format!("{file}: {e}"))
}

fn count_lines(dir: &Path) -> std::io::Result<usize> {
    let mut total = 0;
    if !dir.exists() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            total += count_lines(&path)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            total += std::fs::read_to_string(&path)?.lines().count();
        }
    }
    Ok(total)
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("корень workspace")
        .to_path_buf()
}

fn collect(root: &Path) -> Result<Vec<Violation>, String> {
    let mut out = Vec::new();
    let lock =
        std::fs::read_to_string(root.join("Cargo.lock")).map_err(|e| format!("Cargo.lock: {e}"))?;
    out.extend(check_lock_sources(&lock));

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let res = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !res.status.success() {
        return Err(format!(
            "cargo metadata: {}",
            String::from_utf8_lossy(&res.stderr)
        ));
    }
    let metadata: Metadata =
        serde_json::from_slice(&res.stdout).map_err(|e| format!("metadata: {e}"))?;
    out.extend(check_direction(&metadata));
    for pkg in PURE_LLM_USERS {
        let features = resolved_llm_features(root, pkg)?;
        out.extend(check_resolved_llm_features(pkg, &features));
    }
    out.extend(check_reqwest_features(&resolved_reqwest_features(root)?));

    let limit = read_limit(root, "core-size-limit.txt")?;
    let lines =
        count_lines(&root.join("crates/za-core/src")).map_err(|e| format!("za-core: {e}"))?;
    out.extend(check_core_size(lines, limit));
    let limit = read_limit(root, "shell-size-limit.txt")?;
    let lines =
        count_lines(&root.join("crates/za-shell/src")).map_err(|e| format!("za-shell: {e}"))?;
    out.extend(check_size("za-shell", lines, limit));
    let sources = crate_sources(root).map_err(|e| format!("crates/*/src: {e}"))?;
    out.extend(check_client_files_present(
        sources.iter().map(|(rel, _)| rel.as_str()),
    ));
    for (rel, text) in &sources {
        out.extend(check_process_spawn(rel, text));
        out.extend(check_http_client(rel, text));
        out.extend(check_holder_includes(rel, text));
        out.extend(check_client_properties(rel, text));
        out.extend(check_holder_reexports(rel, text));
    }
    Ok(out)
}

/// Точка входа команды `policy`.
pub fn run() -> ExitCode {
    match collect(&root()) {
        Ok(v) if v.is_empty() => {
            println!("policy: нарушений нет");
            ExitCode::SUCCESS
        }
        Ok(v) => {
            for x in &v {
                eprintln!("policy: {x}");
            }
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("policy: ошибка: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_dependency_is_rejected() {
        let lock = "[[package]]\nname = \"x\"\nversion = \"1.0.0\"\nsource = \"git+https://example.com/x\"\n";
        assert_eq!(check_lock_sources(lock).len(), 1);
    }
    #[test]
    fn crates_io_source_is_allowed() {
        let lock = "[[package]]\nname = \"x\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
        assert!(check_lock_sources(lock).is_empty());
    }
    #[test]
    fn forbidden_package_is_rejected() {
        let lock = "[[package]]
name = \"rig-core\"
version = \"1.0.0\"
";
        assert_eq!(check_lock_sources(lock).len(), 1);
    }
    #[test]
    fn path_package_without_source_is_allowed() {
        assert!(
            check_lock_sources(
                "[[package]]
name = \"za-core\"
version = \"0.0.1\"
"
            )
            .is_empty()
        );
    }
    fn dep(name: &str) -> Dependency {
        Dependency {
            name: name.into(),
            uses_default_features: true,
            kind: None,
            features: Vec::new(),
            rename: None,
        }
    }
    fn meta_with(name: &str, dep: Dependency) -> Metadata {
        Metadata {
            packages: vec![Package {
                name: name.into(),
                dependencies: vec![dep],
            }],
        }
    }
    fn meta(name: &str, d: &str) -> Metadata {
        meta_with(name, dep(d))
    }
    #[test]
    fn res_must_not_depend_on_llm() {
        assert_eq!(check_direction(&meta("za-res", "za-llm")).len(), 1);
        assert_eq!(check_direction(&meta("za-res", "za-store")).len(), 1);
        let dev = Dependency {
            kind: Some("dev".into()),
            ..dep("za-llm")
        };
        assert_eq!(check_direction(&meta_with("za-res", dev)).len(), 1);
        assert!(check_direction(&meta("za-res", "za-protocol")).is_empty());
        assert!(check_direction(&meta("za-res", "sysinfo")).is_empty());
    }
    #[test]
    fn core_uses_llm_without_defaults() {
        assert_eq!(check_direction(&meta("za-core", "za-llm")).len(), 1);
        let no_defaults = Dependency {
            uses_default_features: false,
            ..dep("za-llm")
        };
        assert!(check_direction(&meta_with("za-core", no_defaults)).is_empty());
        let dev = Dependency {
            kind: Some("dev".into()),
            ..dep("za-llm")
        };
        assert!(check_direction(&meta_with("za-core", dev)).is_empty());
    }
    #[test]
    fn metadata_dependency_fields_are_parsed() {
        let json = r#"{"packages":[{"name":"za-core","dependencies":[
            {"name":"za-llm","uses_default_features":false,"kind":null},
            {"name":"tokio","uses_default_features":true,"kind":"dev"}]}]}"#;
        let m: Metadata = serde_json::from_str(json).unwrap();
        let deps = &m.packages[0].dependencies;
        assert!(!deps[0].uses_default_features);
        assert_eq!(deps[0].kind, None);
        assert_eq!(deps[1].kind.as_deref(), Some("dev"));
        assert!(check_direction(&m).is_empty());
    }
    #[test]
    fn core_must_not_depend_on_server() {
        assert_eq!(check_direction(&meta("za-core", "za-server")).len(), 1);
        assert!(check_direction(&meta("za-core", "za-protocol")).is_empty());
    }
    #[test]
    fn protocol_must_not_depend_on_za_crates() {
        assert_eq!(check_direction(&meta("za-protocol", "za-store")).len(), 1);
        assert!(check_direction(&meta("za-protocol", "serde")).is_empty());
    }
    #[test]
    fn policy_rejects_core_on_agent() {
        assert_eq!(check_direction(&meta("za-core", "za-agent")).len(), 1);
        let dev = Dependency {
            kind: Some("dev".into()),
            ..dep("za-agent")
        };
        assert_eq!(check_direction(&meta_with("za-core", dev)).len(), 1);
        for lower in ["za-protocol", "za-store", "za-llm", "za-res"] {
            assert!(
                !check_direction(&meta(lower, "za-agent")).is_empty(),
                "{lower} → za-agent"
            );
        }
        assert!(check_direction(&meta("zero-agent", "za-agent")).is_empty());
    }
    #[test]
    fn agent_uses_llm_without_defaults() {
        assert_eq!(check_direction(&meta("za-agent", "za-llm")).len(), 1);
        let no_defaults = Dependency {
            uses_default_features: false,
            ..dep("za-llm")
        };
        assert!(check_direction(&meta_with("za-agent", no_defaults)).is_empty());
        let dev = Dependency {
            kind: Some("dev".into()),
            ..dep("za-llm")
        };
        assert!(check_direction(&meta_with("za-agent", dev)).is_empty());
    }
    #[test]
    fn agent_depends_only_on_lower_layers() {
        for ok in ["za-protocol", "za-store", "za-core", "za-res", "serde"] {
            assert!(check_direction(&meta("za-agent", ok)).is_empty(), "{ok}");
        }
        assert_eq!(check_direction(&meta("za-agent", "zero-agent")).len(), 1);
        // Два правила: нижние слои `za-agent` и «от za-memory — только zero-agent и xtask»
        // (R1-M D133).
        assert_eq!(check_direction(&meta("za-agent", "za-memory")).len(), 2);
        assert_eq!(check_direction(&meta("za-agent", "reqwest")).len(), 1);
    }
    #[test]
    fn llm_features_must_be_empty_for_core_and_agent() {
        for pkg in ["za-agent", "za-core"] {
            for f in ["gateway", "http", "probe"] {
                let with = Dependency {
                    uses_default_features: false,
                    features: vec![f.into()],
                    ..dep("za-llm")
                };
                assert_eq!(
                    check_direction(&meta_with(pkg, with)).len(),
                    1,
                    "{pkg}: {f}"
                );
            }
            let dev = Dependency {
                kind: Some("dev".into()),
                features: vec!["gateway".into()],
                ..dep("za-llm")
            };
            assert!(check_direction(&meta_with(pkg, dev)).is_empty(), "{pkg}");
        }
        let json = r#"{"packages":[{"name":"za-agent","dependencies":[
            {"name":"za-llm","uses_default_features":false,"kind":null,"features":["gateway"]}]}]}"#;
        let m: Metadata = serde_json::from_str(json).unwrap();
        assert_eq!(m.packages[0].dependencies[0].features, vec!["gateway"]);
        assert_eq!(check_direction(&m).len(), 1);
    }
    #[test]
    fn resolved_llm_features_are_parsed_from_tree() {
        let clean = "za-agent v0.0.1 (H:\\p\\crates\\za-agent)|\n\
            za-core v0.0.1 (H:\\p\\crates\\za-core)|\n\
            za-llm v0.0.1 (H:\\p\\crates\\za-llm)|\n\
            za-llm v0.0.1 (H:\\p\\crates\\za-llm)| (*)\n\
            tokio v1.54.1|default,sync\n";
        assert_eq!(
            parse_tree_features(clean, "za-llm"),
            Some(Vec::<String>::new())
        );
        assert_eq!(check_resolved_llm_features("za-agent", &[]), None);
        // Включение через собственный `[features]` или транзитивно через другой крейт
        // видно только в итоговом наборе сборки `-p za-agent`.
        let dirty = "za-agent v0.0.1 (/p/za-agent)|x\n\
            za-llm v0.0.1 (/p/za-llm)|gateway,http\n\
            za-llm v0.0.1 (/p/za-llm)|gateway,http (*)\n";
        let f = parse_tree_features(dirty, "za-llm").unwrap();
        assert_eq!(f, vec!["gateway", "http"]);
        assert!(check_resolved_llm_features("za-agent", &f).is_some());
        assert_eq!(parse_tree_features("tokio v1|sync\n", "za-llm"), None);
        // Префикс имени не путается: `za-llm-x` — другой крейт.
        assert_eq!(
            parse_tree_features("za-llm-x v1 (/p)|http\n", "za-llm"),
            None
        );
    }
    #[test]
    fn core_must_not_depend_on_res_or_http() {
        assert_eq!(check_direction(&meta("za-shell", "serde")).len(), 1);
        assert_eq!(check_direction(&meta("za-agent", "za-shell")).len(), 2);
        assert!(check_direction(&meta("za-core", "za-shell")).is_empty());
        assert!(check_size("za-shell", 10, 10).is_none());
        assert!(check_size("za-shell", 11, 10).is_some());
        assert_eq!(check_direction(&meta("za-core", "za-res")).len(), 1);
        assert_eq!(check_direction(&meta("za-core", "reqwest")).len(), 1);
    }
    #[test]
    fn tiktoken_only_in_xtask() {
        assert!(check_direction(&meta("xtask", "tiktoken-rs")).is_empty());
        for pkg in ["za-agent", "za-llm", "zero-agent"] {
            assert_eq!(check_direction(&meta(pkg, "tiktoken-rs")).len(), 1, "{pkg}");
        }
        let dev = Dependency {
            kind: Some("dev".into()),
            ..dep("tiktoken-rs")
        };
        assert_eq!(check_direction(&meta_with("za-core", dev)).len(), 1);
    }
    #[test]
    fn system_tls_and_dbus_packages_are_rejected() {
        for p in ["openssl-sys", "native-tls", "libdbus-sys"] {
            let lock = format!(
                "[[package]]\nname = \"{p}\"\nversion = \"1.0.0\"\nsource = \"{CRATES_IO}\"\n"
            );
            assert_eq!(check_lock_sources(&lock).len(), 1, "{p}");
        }
    }
    #[test]
    fn r1c1_crate_directions() {
        // za-jail — без крейтов workspace, в том числе dev.
        assert_eq!(check_direction(&meta("za-jail", "za-core")).len(), 1);
        let dev = Dependency {
            kind: Some("dev".into()),
            ..dep("za-protocol")
        };
        assert_eq!(check_direction(&meta_with("za-jail", dev)).len(), 1);
        assert!(check_direction(&meta("za-jail", "process-wrap")).is_empty());
        // za-res → za-jail разрешено.
        assert!(check_direction(&meta("za-res", "za-jail")).is_empty());
        // za-agent → za-secrets да, za-tools и za-jail нет ни в каком виде.
        assert!(check_direction(&meta("za-agent", "za-secrets")).is_empty());
        for d in ["za-tools", "za-jail"] {
            assert_eq!(check_direction(&meta("za-agent", d)).len(), 1, "{d}");
            let dev = Dependency {
                kind: Some("dev".into()),
                ..dep(d)
            };
            assert_eq!(
                check_direction(&meta_with("za-agent", dev)).len(),
                1,
                "dev {d}"
            );
        }
        // za-core — не от новых крейтов.
        for d in ["za-jail", "za-tools", "za-secrets"] {
            assert_eq!(check_direction(&meta("za-core", d)).len(), 1, "{d}");
        }
        // za-tools — только от za-protocol, za-core, za-jail.
        for ok in ["za-protocol", "za-core", "za-jail", "similar"] {
            assert!(check_direction(&meta("za-tools", ok)).is_empty(), "{ok}");
        }
        assert_eq!(check_direction(&meta("za-tools", "za-store")).len(), 1);
        // xtask → za-tools (случаи бюджета промптов, D93).
        assert!(check_direction(&meta("xtask", "za-tools")).is_empty());
        // za-secrets — без зависимостей.
        assert_eq!(check_direction(&meta("za-secrets", "serde")).len(), 1);
        assert_eq!(check_direction(&meta("za-secrets", "za-protocol")).len(), 1);
    }
    #[test]
    fn process_spawn_outside_jail_is_rejected() {
        let v = check_process_spawn(
            "crates/za-tools/src/x.rs",
            "let c = Command::new(\"git\");\n",
        );
        assert_eq!(v.len(), 1);
        assert!(v[0].0.contains("crates/za-tools/src/x.rs:1"), "{v:?}");
        for line in [
            "use std::process::Command;",
            "use tokio::process::Command;",
            "use std::process::{Command, Stdio};",
            "let c = tokio::process::Command::new(exe);",
            "nix::spawn::posix_spawnp(&path, &actions, &attr, &args, &env)",
            "use nix::unistd::execvp;",
            "match unsafe { nix::unistd::fork() } {",
        ] {
            assert_eq!(
                check_process_spawn("crates/za-res/src/gpu.rs", line).len(),
                1,
                "{line}"
            );
        }
        assert!(check_process_spawn("crates/za-jail/src/direct.rs", "Command::new(x)").is_empty());
        assert!(
            check_process_spawn("crates/zero-agent/build.rs", "Command::new(rustc)").is_empty()
        );
        assert_eq!(
            check_process_spawn("crates/za-tools/build.rs", "Command::new(x)").len(),
            1
        );
        assert!(
            check_process_spawn("crates/za-core/src/a.rs", "// Command::new — пример").is_empty()
        );
        assert!(
            check_process_spawn("crates/za-core/src/a.rs", "use std::process::Stdio;").is_empty()
        );
    }
    #[test]
    fn r1c2_web_directions() {
        // za-web — только от za-protocol и za-core, в любом виде.
        for ok in ["za-protocol", "za-core", "reqwest", "url"] {
            assert!(check_direction(&meta("za-web", ok)).is_empty(), "{ok}");
        }
        for bad in [
            "za-agent",
            "za-tools",
            "za-jail",
            "za-store",
            "za-extract",
            "za-llm",
            "zero-agent",
        ] {
            assert!(!check_direction(&meta("za-web", bad)).is_empty(), "{bad}");
            let dev = Dependency {
                kind: Some("dev".into()),
                ..dep(bad)
            };
            assert!(
                !check_direction(&meta_with("za-web", dev)).is_empty(),
                "dev {bad}"
            );
        }
        // От za-web — только zero-agent и xtask (случаи бюджета промптов).
        for ok in ["zero-agent", "xtask"] {
            assert!(check_direction(&meta(ok, "za-web")).is_empty(), "{ok}");
        }
        let to_web = |v: Vec<Violation>| v.iter().any(|x| x.0.contains("зависеть от za-web"));
        for bad in ["za-agent", "za-tools", "za-core", "za-skills", "za-extract"] {
            assert!(to_web(check_direction(&meta(bad, "za-web"))), "{bad}");
            let dev = Dependency {
                kind: Some("dev".into()),
                ..dep("za-web")
            };
            assert!(to_web(check_direction(&meta_with(bad, dev))), "dev {bad}");
        }
    }
    /// D114: `za-skills` — только от `za-protocol` и `za-core` в любом виде; `za-agent` и
    /// бинарник могут зависеть от него.
    #[test]
    fn r1c2_skills_directions() {
        let dev = |name: &str| Dependency {
            kind: Some("dev".into()),
            ..dep(name)
        };
        for ok in ["za-protocol", "za-core", "blake3", "serde_json"] {
            assert!(check_direction(&meta("za-skills", ok)).is_empty(), "{ok}");
            assert!(
                check_direction(&meta_with("za-skills", dev(ok))).is_empty(),
                "dev {ok}"
            );
        }
        for bad in [
            "za-agent",
            "za-tools",
            "za-jail",
            "za-store",
            "za-web",
            "za-llm",
            "zero-agent",
        ] {
            assert!(
                !check_direction(&meta("za-skills", bad)).is_empty(),
                "{bad}"
            );
            assert!(
                !check_direction(&meta_with("za-skills", dev(bad))).is_empty(),
                "dev {bad}"
            );
        }
        for ok in ["za-agent", "zero-agent", "xtask"] {
            assert!(check_direction(&meta(ok, "za-skills")).is_empty(), "{ok}");
        }
    }
    /// D113, D127: `za-extract` — без крейтов workspace в любом виде, от него — только
    /// `zero-agent`; `htmd` и его разборщики — прямой зависимостью только у `za-extract`.
    #[test]
    fn r1c2_extract_directions() {
        let dev = |name: &str| Dependency {
            kind: Some("dev".into()),
            ..dep(name)
        };
        for ok in ["htmd", "markup5ever_rcdom", "pdf-extract", "serde"] {
            assert!(check_direction(&meta("za-extract", ok)).is_empty(), "{ok}");
        }
        for bad in ["za-core", "za-protocol", "za-jail", "za-web", "zero-agent"] {
            assert!(
                !check_direction(&meta("za-extract", bad)).is_empty(),
                "{bad}"
            );
            assert!(
                !check_direction(&meta_with("za-extract", dev(bad))).is_empty(),
                "dev {bad}"
            );
        }
        assert!(check_direction(&meta("zero-agent", "za-extract")).is_empty());
        let to_extract =
            |v: Vec<Violation>| v.iter().any(|x| x.0.contains("зависеть от za-extract"));
        for bad in [
            "za-web",
            "za-agent",
            "za-tools",
            "za-core",
            "za-skills",
            "xtask",
        ] {
            assert!(
                to_extract(check_direction(&meta(bad, "za-extract"))),
                "{bad}"
            );
            assert!(
                to_extract(check_direction(&meta_with(bad, dev("za-extract")))),
                "dev {bad}"
            );
        }
        for parser in EXTRACT_ONLY_DEPS {
            for bad in ["za-web", "za-agent", "zero-agent"] {
                assert!(
                    !check_direction(&meta(bad, parser)).is_empty(),
                    "{bad} → {parser}"
                );
                assert!(
                    !check_direction(&meta_with(bad, dev(parser))).is_empty(),
                    "dev {bad} → {parser}"
                );
            }
        }
    }
    /// Ruling I2 ревью задачи 6: прямая зависимость от `reqwest` любого вида (обычная, dev,
    /// build) — только у `za-llm`, `za-web` и клиента памяти `za-memory` (R1-M D133). Иначе
    /// второй HTTP-клиент вне шлюза строится и без `Client::builder` в исходнике
    /// (`Client::default()`, обёртка, макрос).
    #[test]
    fn reqwest_only_in_http_holders() {
        let kinds = [None, Some("dev"), Some("build")];
        let with_kind = |k: Option<&str>| Dependency {
            kind: k.map(str::to_owned),
            ..dep("reqwest")
        };
        let reqwest_rule = |v: &[Violation]| {
            v.iter()
                .filter(|x| x.0.contains("от reqwest (только za-llm, za-web"))
                .count()
        };
        for ok in ["za-llm", "za-web", "za-memory"] {
            for k in kinds {
                let v = check_direction(&meta_with(ok, with_kind(k)));
                assert!(v.is_empty(), "{ok} {k:?}: {v:?}");
            }
        }
        for bad in [
            "za-agent",
            "za-core",
            "za-tools",
            "za-skills",
            "za-extract",
            "za-store",
            "za-jail",
            "za-res",
            "za-protocol",
            "zero-agent",
            "xtask",
        ] {
            for k in kinds {
                let v = check_direction(&meta_with(bad, with_kind(k)));
                assert_eq!(reqwest_rule(&v), 1, "{bad} {k:?}: {v:?}");
            }
        }
        // Переименование (`http = { package = "reqwest" }`) не прячет крейт: `cargo
        // metadata` даёт в `name` имя пакета.
        let json = r#"{"packages":[{"name":"za-tools","dependencies":[
            {"name":"reqwest","rename":"http","uses_default_features":false,"kind":null}]}]}"#;
        let m: Metadata = serde_json::from_str(json).unwrap();
        assert_eq!(reqwest_rule(&check_direction(&m)), 1);
    }
    /// Minor 2 повторного ревью финального прохода: переименование зависимости `reqwest`
    /// запрещено во всех манифестах, в том числе у разрешённых держателей. Иначе в
    /// исходнике `za-web` нет слова `reqwest` (`http = { package = "reqwest" }`, затем
    /// `use http::Client`), и страж путей `check_http_client` клиента вне шлюза не видит.
    #[test]
    fn renamed_reqwest_is_rejected_everywhere() {
        let meta = |pkg: &str, rename: &str| -> Metadata {
            let json = format!(
                r#"{{"packages":[{{"name":"{pkg}","dependencies":[
                {{"name":"reqwest","rename":{rename},"uses_default_features":false,"kind":null}}]}}]}}"#
            );
            serde_json::from_str(&json).unwrap()
        };
        let renamed = |v: &[Violation]| v.iter().filter(|x| x.0.contains("переименована")).count();
        for pkg in ["za-web", "za-llm", "za-memory", "za-tools"] {
            let v = check_direction(&meta(pkg, r#""http""#));
            assert_eq!(renamed(&v), 1, "{pkg}: {v:?}");
        }
        for pkg in ["za-web", "za-llm"] {
            let v = check_direction(&meta(pkg, "null"));
            assert!(v.is_empty(), "{pkg}: {v:?}");
        }
        // Переименование другого крейта — не это правило.
        let json = r#"{"packages":[{"name":"za-web","dependencies":[
            {"name":"url","rename":"u","uses_default_features":true,"kind":null}]}]}"#;
        let m: Metadata = serde_json::from_str(json).unwrap();
        assert_eq!(renamed(&check_direction(&m)), 0);
    }
    /// Minor 2 повторного ревью финального прохода: в `za-web` и `za-memory` (R1-M D133) код
    /// подключается только из `src/` обычными модулями. Атрибут `path` (`#[path = …]`,
    /// `#![path = …]`, внутри `cfg_attr`, с пробелами между лексемами) и `include!`
    /// подключили бы файл, который страж исходников не сканирует. Правило — для всех файлов
    /// держателя, и для файла клиента (`gateway.rs`, `transport.rs`).
    #[test]
    fn holder_crates_include_no_outside_files() {
        for rel in [
            "crates/za-web/src/lib.rs",
            "crates/za-web/src/gateway.rs",
            "crates/za-memory/src/lib.rs",
            "crates/za-memory/src/transport.rs",
            "crates/za-memory/src/gateway.rs",
        ] {
            for line in [
                r#"#[path = "../extra/client.rs"] mod client;"#,
                r#"# [ path = "../../za-core/src/x.rs" ]"#,
                r#"#![path = "x.rs"]"#,
                r#"#[cfg_attr(all(), path = "../extra/client.rs")]"#,
                r#"    path = "../extra/client.rs""#,
                r#"    path = r"../extra/client.rs","#,
                r#"include!("../extra/client.rs");"#,
                r#"include ! (concat!(env!("OUT_DIR"), "/gen.rs"));"#,
            ] {
                let v = check_holder_includes(rel, line);
                assert_eq!(v.len(), 1, "{rel}: {line}");
                assert!(v[0].0.contains(&format!("{rel}:1")), "{v:?}");
            }
        }
        for line in [
            "// #[path = \"x.rs\"] — пример в комментарии",
            "path = format!(\"{path}?{q}\");",
            "let path = normalize(path);",
            "const DOC: &str = include_str!(\"doc.md\");",
            "mod gateway;",
        ] {
            for rel in [
                "crates/za-web/src/gateway.rs",
                "crates/za-memory/src/transport.rs",
            ] {
                assert!(check_holder_includes(rel, line).is_empty(), "{rel}: {line}");
            }
        }
        // Вне держателей правило не действует; тесты крейтов подключают `test-support/*.rs`
        // через `#[path]` законно — их страж не сканирует.
        for rel in ["crates/zero-agent/tests/x.rs", "crates/za-core/src/lib.rs"] {
            assert!(
                check_holder_includes(rel, "#[path = \"a.rs\"]").is_empty(),
                "{rel}"
            );
        }
        let v = check_holder_includes("crates/za-memory/src/x.rs", "#[path = \"a.rs\"]");
        assert!(v[0].0.contains("в za-memory атрибут path"), "{v:?}");
    }
    #[test]
    fn reqwest_features_are_guarded() {
        let clean = "reqwest v0.12.28|__rustls,__tls,json,rustls-tls,rustls-tls-webpki-roots\n\
            reqwest v0.12.28|__rustls,__tls,json,rustls-tls,rustls-tls-webpki-roots (*)\n";
        let f = parse_tree_features(clean, "reqwest").unwrap();
        assert_eq!(check_reqwest_features(&f), None);
        for bad in REQWEST_FORBIDDEN_FEATURES {
            let tree = format!("reqwest v0.12.28|json,rustls-tls,{bad}\n");
            let f = parse_tree_features(&tree, "reqwest").unwrap();
            let v = check_reqwest_features(&f).expect(bad);
            assert!(v.0.contains(bad), "{v}");
        }
        assert_eq!(check_reqwest_features(&[]), None);
    }
    #[test]
    fn http_client_outside_gateway_is_rejected() {
        for line in [
            "let c = reqwest::Client::builder().build()?;",
            "let c = Client::builder()",
            "let b: reqwest::ClientBuilder = x;",
            "let c = reqwest::Client::new();",
            "let c = reqwest::Client::default();",
            "let c = Client::default();",
            "let c = <reqwest::Client as Default>::default();",
            "let r = reqwest::get(url).await?;",
            "let c = reqwest::blocking::Client::new();",
        ] {
            let v = check_http_client("crates/za-tools/src/web.rs", line);
            assert_eq!(v.len(), 1, "{line}");
            assert!(v[0].0.contains("crates/za-tools/src/web.rs:1"), "{v:?}");
            // Остальные файлы za-web — тоже нет: клиент только в gateway.rs.
            assert_eq!(
                check_http_client("crates/za-web/src/fetch.rs", line).len(),
                1,
                "{line}"
            );
        }
        for rel in [
            "crates/za-web/src/gateway.rs",
            "crates/za-memory/src/transport.rs",
            "crates/za-llm/src/probe.rs",
        ] {
            assert!(
                check_http_client(rel, "Client::builder()").is_empty(),
                "{rel}"
            );
        }
        for line in [
            "// reqwest::Client::builder — пример",
            "let m = MemoryClient::new(cfg);",
            "let b = HttpClientBuilder::default();",
        ] {
            assert!(
                check_http_client("crates/za-core/src/a.rs", line).is_empty(),
                "{line}"
            );
        }
    }
    /// Финальное ревью безопасности, Minor 1 (m1 ревью fix-5/6): внутри `za-web` вне
    /// `gateway.rs` из `reqwest` допустимы только `reqwest::dns::` и `reqwest::StatusCode`.
    /// Клиент через импорт и `Default::default()` образцы `Client::…` не видят, а этот
    /// страж — да; псевдоним крейта и импорт группы — тоже.
    #[test]
    fn web_crate_uses_only_allowed_reqwest_paths() {
        let rel = "crates/za-web/src/x.rs";
        for line in [
            "use reqwest::{Client};",
            "use reqwest::Client as C;",
            "let c: Client = Default::default(); let t = ::reqwest::Url::parse(s);",
            "use reqwest as r;",
            "use reqwest;",
            "extern crate reqwest;",
            "let p = reqwest::Proxy::all(u);",
        ] {
            let v = check_http_client(rel, line);
            assert!(!v.is_empty(), "{line}");
            assert!(
                v.iter().all(|x| x.0.contains("crates/za-web/src/x.rs:1")),
                "{v:?}"
            );
        }
        for line in [
            "use reqwest::dns::{Addrs, Name, Resolve, Resolving};",
            "match reqwest::StatusCode::from_u16(status) {",
            "// use reqwest::Client — пример в комментарии",
            "let reqwest_error = e;",
            "let n = my_reqwest::Client::new_name();",
        ] {
            assert!(check_http_client(rel, line).is_empty(), "{line}");
        }
        // Вне держателей правило путей не действует (там — зависимость, `REQWEST_USERS`), а в
        // `gateway.rs` клиент разрешён.
        assert!(check_http_client("crates/za-llm/src/a.rs", "use reqwest::{Client};").is_empty());
        assert!(
            check_http_client("crates/za-web/src/gateway.rs", "use reqwest::{Client};").is_empty()
        );
    }
    /// R1-M D133: в `za-memory` вне `transport.rs` `reqwest` не упоминается вовсе — ни
    /// клиент, ни типы (`reqwest::dns`, `StatusCode`, `Method`): метод HTTP — реэкспорт
    /// `transport::Method`.
    #[test]
    fn memory_crate_mentions_reqwest_only_in_transport() {
        let rel = "crates/za-memory/src/gateway.rs";
        for line in [
            "let c = reqwest::Client::builder().build()?;",
            "let c = Client::builder()",
            "use reqwest::{Client};",
            "use reqwest::Method;",
            "use reqwest::dns::Name;",
            "match reqwest::StatusCode::from_u16(s) {",
            "use reqwest as r;",
            "extern crate reqwest;",
        ] {
            let v = check_http_client(rel, line);
            assert_eq!(v.len(), 1, "{line}: {v:?}");
            assert!(
                v[0].0.contains("crates/za-memory/src/gateway.rs:1"),
                "{v:?}"
            );
        }
        let v = check_http_client(rel, "use reqwest::Method;");
        assert!(v[0].0.contains("допустимо только ничего"), "{v:?}");
        for line in [
            "use crate::transport::{Method, ZamClient};",
            "// reqwest::Client — только в transport.rs",
            "let reqwest_like = 1;",
        ] {
            assert!(check_http_client(rel, line).is_empty(), "{line}");
        }
        for line in [
            "let http = reqwest::Client::builder()",
            "pub use reqwest::Method;",
            "use reqwest::header::{ACCEPT, HeaderValue};",
        ] {
            assert!(
                check_http_client("crates/za-memory/src/transport.rs", line).is_empty(),
                "{line}"
            );
        }
    }
    /// R1-M D133: `za-memory` — только от `za-protocol`, `za-core`, `za-store`, `za-secrets`
    /// в любом виде; от него — только `zero-agent` и `xtask`; `za-agent` от него не зависит
    /// (порт памяти — в `za-core`, D138).
    #[test]
    fn r1m_memory_directions() {
        let dev = |name: &str| Dependency {
            kind: Some("dev".into()),
            ..dep(name)
        };
        for ok in [
            "za-protocol",
            "za-core",
            "za-store",
            "za-secrets",
            "reqwest",
            "url",
            "axum",
        ] {
            assert!(check_direction(&meta("za-memory", ok)).is_empty(), "{ok}");
            assert!(
                check_direction(&meta_with("za-memory", dev(ok))).is_empty(),
                "dev {ok}"
            );
        }
        for bad in [
            "za-agent",
            "za-llm",
            "za-tools",
            "za-jail",
            "za-web",
            "za-skills",
            "za-extract",
            "za-res",
            "za-shell",
            "zero-agent",
        ] {
            assert!(
                !check_direction(&meta("za-memory", bad)).is_empty(),
                "{bad}"
            );
            assert!(
                !check_direction(&meta_with("za-memory", dev(bad))).is_empty(),
                "dev {bad}"
            );
        }
        let to_memory = |v: Vec<Violation>| v.iter().any(|x| x.0.contains("зависеть от za-memory"));
        for ok in ["zero-agent", "xtask"] {
            assert!(check_direction(&meta(ok, "za-memory")).is_empty(), "{ok}");
        }
        for bad in [
            "za-tools",
            "za-agent",
            "za-core",
            "za-web",
            "za-skills",
            "za-store",
            "za-llm",
        ] {
            assert!(to_memory(check_direction(&meta(bad, "za-memory"))), "{bad}");
            assert!(
                to_memory(check_direction(&meta_with(bad, dev("za-memory")))),
                "dev {bad}"
            );
        }
    }
    /// M6 ревью задачи 2 R1-M: файл клиента держателя — один источник (`HTTP_HOLDERS`).
    /// Освобождение `check_http_client`, страж реэкспорта и наличие файла берут путь оттуда;
    /// второй таблицы с тем же путём нет, и переименование файла без правки таблицы — нарушение.
    #[test]
    fn client_file_has_one_source() {
        for h in HTTP_HOLDERS {
            let file = h.client_file();
            assert!(file.starts_with(&format!("{}src/", h.prefix)), "{file}");
            assert!(http_client_allowed(&file), "{file}");
            assert!(
                check_http_client(&file, "let c = Client::builder()").is_empty(),
                "{file}"
            );
            let other = format!("{}src/other.rs", h.prefix);
            assert!(!http_client_allowed(&other), "{other}");
            assert_eq!(
                check_http_client(&other, "let c = Client::builder()").len(),
                1,
                "{other}"
            );
        }
        // `za-llm` — весь крейт (R1-A).
        assert!(http_client_allowed("crates/za-llm/src/probe.rs"));
        assert!(!http_client_allowed("crates/za-core/src/a.rs"));
        let all: Vec<String> = HTTP_HOLDERS.iter().map(HttpHolder::client_file).collect();
        assert!(check_client_files_present(all.iter().map(String::as_str)).is_empty());
        let renamed = [
            "crates/za-web/src/gateway.rs",
            "crates/za-memory/src/client.rs",
        ];
        let v = check_client_files_present(renamed);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(
            v[0].0.contains("crates/za-memory/src/transport.rs"),
            "{v:?}"
        );
        // Настоящие исходники: файлы клиентов на месте.
        let sources = crate_sources(&root()).unwrap();
        assert!(check_client_files_present(sources.iter().map(|(r, _)| r.as_str())).is_empty());
    }

    /// M4 ревью задачи 2 R1-M: файл, где строится клиент `reqwest` (в `za-llm`, `za-web`,
    /// `za-memory`), содержит в коде `.no_proxy()` и `.redirect(…Policy::none())`; иначе
    /// нарушение. Исключения — только из таблицы с причиной.
    #[test]
    fn client_builder_needs_no_proxy_and_no_redirect() {
        let full = "let c = reqwest::Client::builder()\n    .no_proxy()\n    .redirect(reqwest::redirect::Policy::none())\n    .build();";
        let no_proxy = "let c = reqwest::Client::builder()\n    .redirect(reqwest::redirect::Policy::none())\n    .build();";
        let no_redirect = "let c = reqwest::Client::builder()\n    .no_proxy()\n    .build();";
        let commented = "let c = reqwest::Client::builder()\n    // .no_proxy()\n    // .redirect(reqwest::redirect::Policy::none())\n    .build();";
        let limited = "let c = Client::builder()\n    .no_proxy()\n    .redirect(redirect::Policy::limited(3))\n    .build(); // Policy::none() — нет";
        for rel in [
            "crates/za-memory/src/transport.rs",
            "crates/za-web/src/gateway.rs",
            "crates/za-llm/src/http/transport.rs",
            "crates/za-llm/src/x.rs",
        ] {
            assert!(check_client_properties(rel, full).is_empty(), "{rel}");
            let v = check_client_properties(rel, no_proxy);
            assert_eq!(v.len(), 1, "{rel}: {v:?}");
            assert!(v[0].0.contains(".no_proxy()"), "{v:?}");
            assert!(v[0].0.starts_with(rel), "{v:?}");
            let v = check_client_properties(rel, no_redirect);
            assert_eq!(v.len(), 1, "{rel}: {v:?}");
            assert!(v[0].0.contains("Policy::none()"), "{v:?}");
            assert_eq!(check_client_properties(rel, commented).len(), 2, "{rel}");
            assert_eq!(check_client_properties(rel, limited).len(), 1, "{rel}");
        }
        // Клиент не строится — нечего проверять; вне держателей правило не действует (там
        // клиента нет вовсе: `check_http_client`, `REQWEST_USERS`).
        assert!(
            check_client_properties("crates/za-memory/src/discovery.rs", "let x = 1;").is_empty()
        );
        assert!(check_client_properties("crates/za-core/src/a.rs", no_proxy).is_empty());
        assert!(
            check_client_properties(
                "crates/za-memory/src/transport.rs",
                "let m = MemoryClient::builder();"
            )
            .is_empty()
        );
        // Исключение с причиной: зонд R0 `probe_llama` без `Policy::none`, но прокси — нет.
        assert!(check_client_properties("crates/za-llm/src/probe.rs", no_redirect).is_empty());
        assert_eq!(
            check_client_properties("crates/za-llm/src/probe.rs", no_proxy).len(),
            1
        );
        for (file, prop, why) in CLIENT_PROPERTY_EXEMPT {
            assert!(CLIENT_PROPERTIES.iter().any(|(p, _)| p == prop), "{file}");
            assert!(!why.trim().is_empty(), "{file}");
        }
    }

    /// M4: настоящие файлы клиентов `za-llm`, `za-web` и `za-memory` строят клиент (проверка
    /// не пустая) и проходят страж свойств; без `.no_proxy()` клиент памяти — нарушение.
    #[test]
    fn real_client_files_have_properties() {
        let root = root();
        for rel in [
            "crates/za-web/src/gateway.rs",
            "crates/za-memory/src/transport.rs",
            "crates/za-llm/src/http/transport.rs",
            "crates/za-llm/src/probe.rs",
        ] {
            let text = std::fs::read_to_string(root.join(rel)).unwrap();
            assert!(text.contains("Client::builder"), "{rel}");
            let v = check_client_properties(rel, &text);
            assert!(v.is_empty(), "{rel}: {v:?}");
        }
        let rel = "crates/za-memory/src/transport.rs";
        let text = std::fs::read_to_string(root.join(rel)).unwrap();
        for cut in [
            ".no_proxy()",
            ".redirect(reqwest::redirect::Policy::none())",
        ] {
            let mutated = text
                .lines()
                .filter(|l| l.trim() != cut)
                .collect::<Vec<_>>()
                .join("\n");
            assert_ne!(mutated.lines().count(), text.lines().count(), "{cut}");
            assert_eq!(check_client_properties(rel, &mutated).len(), 1, "{cut}");
        }
    }

    /// M6 ревью задачи 2 R1-M: из файла клиента держателя `reqwest` не реэкспортируется,
    /// кроме `pub use reqwest::Method;`. Иначе `Http::default()` в другом модуле крейта —
    /// клиент по умолчанию (прокси окружения, редиректы), которого `check_http_client` не видит.
    #[test]
    fn holder_client_file_reexports_only_method() {
        for rel in [
            "crates/za-memory/src/transport.rs",
            "crates/za-web/src/gateway.rs",
        ] {
            for text in [
                "pub use reqwest::Client as Http;",
                "pub type Http = reqwest::Client;",
                "pub(crate) use reqwest::Client;",
                "pub(super) type B = ::reqwest::ClientBuilder;",
                "pub use reqwest::{\n    Client,\n    Method,\n};",
                "pub use reqwest::*;",
                "pub use reqwest;",
                "pub use reqwest::Method as M;",
                "pub type M = reqwest::Method;",
            ] {
                let v = check_holder_reexports(rel, text);
                assert_eq!(v.len(), 1, "{rel}: {text}: {v:?}");
                assert!(v[0].0.contains(&format!("{rel}:1")), "{v:?}");
            }
            let v = check_holder_reexports(rel, "fn a() {}\npub type Http =\n    reqwest::Client;");
            assert_eq!(v.len(), 1, "{v:?}");
            assert!(v[0].0.contains(&format!("{rel}:2")), "{v:?}");
            for text in [
                "pub use reqwest::Method;",
                "pub(crate) use reqwest::Method;",
                "use reqwest::{Client, redirect};",
                "use reqwest::Client as Http;",
                "// pub use reqwest::Client as Http;",
                "pub use crate::config::Token;",
                "pub type Res = Result<(), ZamError>;",
                "pub fn reqwest_like() {}",
                "pub struct ZamClient {",
            ] {
                assert!(
                    check_holder_reexports(rel, text).is_empty(),
                    "{rel}: {text}"
                );
            }
        }
        // Вне файла клиента правило не действует (там — пути держателя, `check_http_client`).
        assert!(
            check_holder_reexports(
                "crates/za-web/src/lib.rs",
                "pub use reqwest::dns::{Addrs, Name};"
            )
            .is_empty()
        );
        assert!(
            check_holder_reexports(
                "crates/za-llm/src/http/mod.rs",
                "pub use reqwest::header::HeaderMap;"
            )
            .is_empty()
        );
        // Настоящие файлы клиентов чисты.
        for h in HTTP_HOLDERS {
            let file = h.client_file();
            let text = std::fs::read_to_string(root().join(&file)).unwrap();
            let v = check_holder_reexports(&file, &text);
            assert!(v.is_empty(), "{file}: {v:?}");
        }
    }
    #[test]
    fn core_over_limit_is_reported() {
        assert!(check_core_size(5001, 5000).is_some());
        assert!(check_core_size(5000, 5000).is_none());
    }
}
