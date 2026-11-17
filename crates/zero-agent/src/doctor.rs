//! `zero-agent doctor`: проверка окружения (OPS-014, частично).
//!
//! Проверяет встроенную SQLite (ARC-031), доступность llama.cpp (`LLAMA_URL`, R0),
//! GPU (`nvidia-smi`, D6) и состояние каждой модели из `config.toml` после первого
//! зонда, показывает версию компилятора. Внешние системы необязательны: их
//! недоступность — предупреждение, не отказ (нет GPU, модель `inactive` или
//! `unreachable` — `Warn`). Отказ — старая SQLite и ошибка конфигурации. Строка
//! `secrets` называет источник секретов (R1-C1 D90): пока переменные `ZA_SECRET_*`, Vault
//! памяти — с клиентом ZeroAIMemory. Строки `git` и `shell` (R1-C1 D96): `git` в `PATH` и
//! его версия (без git чекпоинты — копиями файлов, D89), оболочка инструмента `bash`
//! (D85); их нет — `Warn`.
//!
//! R1-C2 (D122, задача 14): строки `web` (`[web]` включён ли и его пределы), `search`
//! (поставщик поиска: не настроен, или пробный запрос к своему экземпляру SearXNG через
//! `WebGateway::get_provider` с подсказками на 403 и 429), `extract` (проба подпроцесса
//! `zero-agent extract html`), `skills` (число скиллов по источникам вне проекта, пропуски и
//! корни, не прочитанные пределом `[skills] max_entries`) и `window` (тир standard или full
//! на окне меньше 32k, D169). Всё это — `Warn`, не `Fail`: run без них работает.
//!
//! R1-M (D136, D152): строка `memory` — сервер ZeroAIMemory из `[memory] url`: версия, коммит,
//! уровень (L0–L2 по флагам `/api/v1/health`), число флагов, токен `ZA_MEMORY_TOKEN` (принят
//! или нет), личный Vault, выключенные типы подсказок; не проверенные Vault и подсказки —
//! `Warn` «не проверено: <причина>»; адрес `http` на хост вне локальной сети — `Warn` «токен
//! идёт открытым текстом» (и при L0). Нет `[memory]` — `Warn` «память не настроена»;
//! сервер недоступен — `Warn`: агент работает без памяти (L0). Токен — параметром, команда
//! читает переменную один раз.
//!
//! R1-M (D157, Minor 4 повторного ревью fix-1 задачи 1): защищённое место (`ZA_HOME`,
//! `~/.zeroaimemory/`) внутри каталога запуска `doctor` — строка stderr, как у `run`
//! ([`sealed_in_cwd_lines`]): у `doctor` нет проекта, им считается текущий каталог.

use std::path::Path;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use za_core::extract::{Extractor, HtmlRequest};
use za_core::paths::Roots;
use za_jail::{CommandSpec, DirectRunner, EnvMode, Exit, Runner};
use za_llm::EndpointView;
use za_memory::{
    Level, MemoryConfig, MemoryStatus, SolutionState, Token, TokenState, VaultState, ZamClient,
};
use za_protocol::Tier;
use za_res::{GPU_QUERY_TIMEOUT, GpuStatus, MonitorConfig, ResourcesConfig, query_nvidia_smi};

use crate::config::{AgentConfig, AppConfigError, LLAMA_URL_ENDPOINT, load_config};
use crate::home::{sealed_in_project, sealed_in_project_line, with_user_home};
use crate::models::{PROBE_WAIT, start_gateway, state_text};
use crate::secret::{StoreSecrets, source_text};

/// Переменная окружения с адресом `llama-server`.
pub const LLAMA_URL_ENV: &str = "LLAMA_URL";
/// Версия компилятора, которым собран бинарник (см. `build.rs`).
const RUSTC_VERSION: &str = env!("ZA_RUSTC_VERSION");
/// Минимальная поддерживаемая версия Rust из манифеста.
const MSRV: &str = env!("CARGO_PKG_RUST_VERSION");

/// Итог одной проверки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "lowercase")]
pub enum Check {
    /// Всё в порядке.
    Ok(String),
    /// Работать можно, но с ограничениями.
    Warn(String),
    /// Работать нельзя.
    Fail(String),
}

impl Check {
    /// Короткая метка для вывода.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ok(_) => "ok",
            Self::Warn(_) => "warn",
            Self::Fail(_) => "FAIL",
        }
    }

    /// Пояснение.
    pub fn detail(&self) -> &str {
        match self {
            Self::Ok(d) | Self::Warn(d) | Self::Fail(d) => d,
        }
    }
}

/// Отчёт `doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    /// Встроенная SQLite; при `Ok` в пояснении — версия.
    pub sqlite: Check,
    /// `llama-server` по `LLAMA_URL`.
    pub llama: Check,
    /// GPU по `nvidia-smi`; недоступен — `Warn`.
    pub gpu: Check,
    /// Источник секретов (D90); всегда `Ok`.
    pub secrets: Check,
    /// `git` в `PATH` и его версия (D89, D96); нет — `Warn`.
    pub git: Check,
    /// Оболочка инструмента `bash` (D85, D96); нет — `Warn`.
    pub shell: Check,
    /// `[web]` (D122): включён ли веб и его пределы.
    pub web: Check,
    /// Поставщик поиска (D111, D122): не настроен или итог пробного запроса.
    pub search: Check,
    /// Извлекатель в подпроцессе (D113, D127): проба `extract html`.
    pub extractor: Check,
    /// Скиллы вне проекта (D114, D122): по источникам, пропуски, пределы корней.
    pub skills: Check,
    /// Тир и окно моделей (D169).
    pub window: Check,
    /// Сервер памяти ZeroAIMemory (D136, D152): уровень, токен, Vault; не настроен или
    /// недоступен — `Warn`.
    pub memory: Check,
    /// Модели по id эндпоинта; `config` — ошибка или предупреждение конфигурации.
    pub models: Vec<(String, Check)>,
    /// Компилятор сборки и MSRV.
    pub rust: String,
}

impl DoctorReport {
    /// Проверки строками отчёта: имя и итог.
    fn rows(&self) -> [(&'static str, &Check); 12] {
        [
            ("sqlite", &self.sqlite),
            ("llama", &self.llama),
            ("gpu", &self.gpu),
            ("secrets", &self.secrets),
            ("git", &self.git),
            ("shell", &self.shell),
            ("web", &self.web),
            ("search", &self.search),
            ("extract", &self.extractor),
            ("skills", &self.skills),
            ("window", &self.window),
            ("memory", &self.memory),
        ]
    }

    /// Есть ли отказ: `doctor` завершается с кодом 1.
    pub fn has_failure(&self) -> bool {
        self.rows()
            .into_iter()
            .map(|(_, c)| c)
            .chain(self.models.iter().map(|(_, c)| c))
            .any(|c| matches!(c, Check::Fail(_)))
    }

    /// Человекочитаемый вывод.
    pub fn render(&self) -> String {
        let mut out = String::from("zero-agent doctor\n");
        for (name, c) in self.rows() {
            out.push_str(&format!("  {name:<7} {:<5} {}\n", c.label(), c.detail()));
        }
        out.push_str(&format!("  {:<7} {:<5} {}\n", "rust", "", self.rust));
        if self.models.is_empty() {
            out.push_str("  модели: не настроены ([models] в config.toml или LLAMA_URL)\n");
        } else {
            out.push_str("  модели:\n");
        }
        for (id, c) in &self.models {
            out.push_str(&format!("    {id:<16} {:<5} {}\n", c.label(), c.detail()));
        }
        out
    }
}

/// Собирает отчёт; `llama_url` — значение `LLAMA_URL`, `home` — `ZA_HOME` (`None` — не
/// определён: GPU и модели не проверяются, `Warn`), `memory_token` — значение
/// `ZA_MEMORY_TOKEN`. Проверки идут параллельно.
///
/// Если эндпоинт `local` создан из `LLAMA_URL` (нет раздела `[models]`), сервер
/// зондируется один раз — как модель, и строка `llama` повторяет её итог.
pub async fn run(
    llama_url: Option<&str>,
    home: Option<&Path>,
    memory_token: Option<Token>,
) -> DoctorReport {
    let cfg = home.map(load_config);
    let covered = matches!(&cfg, Some(Ok(c)) if c.llama_url_fallback);
    let read: Option<AgentConfig> = cfg.as_ref().and_then(|r| r.as_ref().ok()).cloned();
    let llama = async {
        if covered {
            None
        } else {
            Some(check_llama(llama_url).await)
        }
    };
    let memory = async {
        match &read {
            Some(c) => check_memory_live(c.memory.as_ref(), memory_token).await,
            None => no_config(),
        }
    };
    let (llama, (gpu, models, windows), git, (web, search), extractor, skills, memory) = tokio::join!(
        llama,
        check_home(home, cfg),
        check_git(),
        check_web_search(read.as_ref()),
        check_extractor(home),
        check_skills(home, read.as_ref()),
        memory,
    );
    let window = check_window(read.as_ref().and_then(|c| c.context.tier), &windows);
    let llama = llama.unwrap_or_else(|| {
        let shown = llama_url.map(display_url).unwrap_or_default();
        match models.iter().find(|(id, _)| id == LLAMA_URL_ENDPOINT) {
            Some((_, Check::Ok(d))) => {
                Check::Ok(format!("{shown}: модель {LLAMA_URL_ENDPOINT}, {d}"))
            }
            Some((_, Check::Warn(d))) => {
                Check::Warn(format!("{shown}: модель {LLAMA_URL_ENDPOINT}, {d}"))
            }
            Some((_, Check::Fail(d))) => {
                Check::Fail(format!("{shown}: модель {LLAMA_URL_ENDPOINT}, {d}"))
            }
            None => Check::Warn(format!("{shown}: модель {LLAMA_URL_ENDPOINT} не проверена")),
        }
    });
    DoctorReport {
        sqlite: check_sqlite(za_store::sqlite_library_version(), za_store::MIN_SQLITE),
        llama,
        gpu,
        secrets: check_secrets(&StoreSecrets::from_env().short_names()),
        git,
        shell: check_shell(),
        web,
        search,
        extractor,
        skills,
        window,
        memory,
        models,
        rust: rust_version(),
    }
}

/// Строки stderr о защищённых местах внутри текущего каталога (R1-M D157, Minor 4
/// повторного ревью fix-1 задачи 1; [`crate::home::sealed_in_project`]): у `doctor` нет
/// проекта, им считается каталог запуска — тот, что обычно передают `run --project .`.
/// `za_home` — `ZA_HOME` (`None` — не определён: сверяется только `~/.zeroaimemory/`).
/// Каталог или домашний каталог, который не разрешается, — строк нет: отказ старта —
/// забота `run`.
pub async fn sealed_in_cwd_lines(za_home: Option<&Path>) -> Vec<String> {
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let za = za_home.map(Path::to_path_buf);
    tokio::task::spawn_blocking(move || {
        let roots = match &za {
            Some(h) => Roots::canonical(&cwd, h),
            None => Roots::boundary(&cwd),
        };
        let Some(roots) = roots.ok().and_then(|r| with_user_home(r).ok()) else {
            return Vec::new();
        };
        sealed_in_project(&roots, za.is_some())
            .iter()
            .map(|p| sealed_in_project_line(p))
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Строка `memory` с проверкой сервера (D136, D152): клиент по `[memory]`, обнаружение и
/// статус (токен, Vault, подсказки), затем [`check_memory`]. Решение проекта `doctor` не
/// проверяет: проекта у него нет (это делает `memory status`).
pub async fn check_memory_live(cfg: Option<&MemoryConfig>, token: Option<Token>) -> Check {
    let Some(c) = cfg.filter(|c| c.enabled) else {
        return check_memory(cfg, token, None);
    };
    match ZamClient::new(c, token.clone()) {
        Ok(client) => {
            let st = za_memory::status(&client, None).await;
            check_memory(cfg, token, Some(&st))
        }
        Err(e) => Check::Warn(format!(
            "{e}: клиент памяти не собран, работа без памяти (L0)"
        )),
    }
}

/// Строка `memory` по конфигурации, токену и статусу сервера (D152). Токен — параметром
/// (`Option<Token>`): функция не читает окружение. `Ok` — L1/L2, токен принят, Vault открыт,
/// подсказки проверены и включены, адрес не уходит открытым текстом за пределы локальной
/// сети; иначе `Warn` с причинами («не проверено: <причина>» у Vault и подсказок — тоже
/// `Warn`). `Fail` нет: без памяти агент работает (MEM-011).
pub fn check_memory(
    cfg: Option<&MemoryConfig>,
    token: Option<Token>,
    st: Option<&MemoryStatus>,
) -> Check {
    let Some(cfg) = cfg else {
        return Check::Warn(
            "память не настроена ([memory] url в ZA_HOME/config.toml): работа без памяти (L0)"
                .to_owned(),
        );
    };
    if !cfg.enabled {
        return Check::Ok("память выключена ([memory] enabled = false)".to_owned());
    }
    let url = display_url(&cfg.url);
    // Открытый текст — свойство адреса, а не ответа: виден и при L0, и без статуса (M9 ревью
    // задачи 2).
    let plaintext = if plaintext_outside_lan(&cfg.url) {
        PLAINTEXT_WARNING
    } else {
        ""
    };
    let Some(st) = st else {
        return Check::Warn(format!("{url}: не проверено{plaintext}"));
    };
    let d = &st.discovery;
    let Some(info) = d.info.as_ref().filter(|_| d.level != Level::L0) else {
        return Check::Warn(format!(
            "{url}: память недоступна ({}): работа без памяти (L0){plaintext}",
            d.reason.as_deref().unwrap_or("нет ответа")
        ));
    };
    let mut warn = false;
    let git = info
        .git
        .as_deref()
        .map(|g| format!(" ({g})"))
        .unwrap_or_default();
    let mut text = format!(
        "ZeroAIMemory {}{git}, {}, флаги: {}",
        info.version,
        d.level.as_str(),
        info.features.len()
    );
    let token_text = match (&token, &st.token) {
        (None, _) | (_, TokenState::NotSet) => {
            warn = true;
            "токен не задан (ZA_MEMORY_TOKEN): запросы к памяти не пройдут".to_owned()
        }
        (Some(_), TokenState::Accepted) => "токен принят".to_owned(),
        (Some(_), TokenState::Rejected) => {
            warn = true;
            "токен отклонён (401): неверен или отозван".to_owned()
        }
        (Some(_), TokenState::RateLimited(after)) => {
            warn = true;
            match after {
                Some(a) => format!(
                    "токен не проверен: лимит запросов, повтор через {} с",
                    a.as_secs()
                ),
                None => "токен не проверен: лимит запросов".to_owned(),
            }
        }
        (Some(_), TokenState::Unchecked(why)) => {
            warn = true;
            format!("токен не проверен: {why}")
        }
    };
    text.push_str(&format!("; {token_text}"));
    match &st.solution {
        SolutionState::Bound { name, role, access } => {
            text.push_str(&format!("; решение {name} ({role}, {access})"));
        }
        SolutionState::Forbidden(why) => {
            warn = true;
            text.push_str(&format!("; нет доступа к решению: {why}"));
        }
        SolutionState::NotFound => {
            warn = true;
            text.push_str("; решение не найдено на сервере");
        }
        SolutionState::NotBound | SolutionState::Unchecked(_) => {}
    }
    // Проверки после принятого токена: «не проверено» — `Warn` с причиной, а не `Ok` (M2
    // ревью задачи 2). Без принятого токена причина уже названа строкой токена.
    let checked = st.token == TokenState::Accepted;
    match &st.vault {
        VaultState::Unlocked => text.push_str("; Vault открыт"),
        VaultState::Locked(why) => {
            warn = true;
            text.push_str(&format!(
                "; Vault заблокирован: {why} (модели с ключом ждут Vault)"
            ));
        }
        VaultState::Unchecked(why) if checked => {
            warn = true;
            text.push_str(&format!("; Vault: не проверено: {why}"));
        }
        VaultState::Unchecked(_) => {}
    }
    if let Some(why) = st.hooks_unchecked.as_deref().filter(|_| checked) {
        warn = true;
        text.push_str(&format!("; подсказки: не проверено: {why}"));
    }
    if !st.hooks_off.is_empty() {
        warn = true;
        text.push_str(&format!(
            "; выключены подсказки: {}",
            st.hooks_off.join(", ")
        ));
    }
    if !plaintext.is_empty() {
        warn = true;
        text.push_str(plaintext);
    }
    if warn {
        Check::Warn(text)
    } else {
        Check::Ok(text)
    }
}

/// Хвост строки `memory` при [`plaintext_outside_lan`].
const PLAINTEXT_WARNING: &str =
    "; токен идёт открытым текстом: http на хост вне локальной сети (нужен https)";

/// Схема `http`, а хост — не loopback и не частный адрес (D152, preflight 7.1.6): токен
/// идёт открытым текстом через чужие сети. Имя хоста, кроме `localhost`, не разрешается:
/// его адрес неизвестен, и это тоже предупреждение.
fn plaintext_outside_lan(raw: &str) -> bool {
    let Ok(u) = url::Url::parse(raw.trim()) else {
        return false;
    };
    if u.scheme() != "http" {
        return false;
    }
    match u.host() {
        Some(url::Host::Ipv4(ip)) => !(ip.is_loopback() || ip.is_private() || ip.is_link_local()),
        Some(url::Host::Ipv6(ip)) => {
            let seg = ip.segments()[0];
            let ula = (seg & 0xfe00) == 0xfc00;
            let link_local = (seg & 0xffc0) == 0xfe80;
            !(ip.is_loopback() || ula || link_local)
        }
        Some(url::Host::Domain(d)) => !d.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Конфигурация не прочитана — веб, поиск и скиллы не проверяются.
fn no_config() -> Check {
    Check::Warn("не проверено: конфигурация не прочитана (строка config ниже)".to_owned())
}

/// Строки `web` и `search` (D111, D122).
async fn check_web_search(cfg: Option<&AgentConfig>) -> (Check, Check) {
    match cfg {
        Some(c) => (
            crate::web::check_web(&c.web),
            crate::web::check_search(&c.web).await,
        ),
        None => (no_config(), no_config()),
    }
}

/// HTML пробы извлекателя.
const PROBE_HTML: &str =
    "<html><head><title>doctor</title></head><body><p>zero-agent doctor probe</p></body></html>";

/// Подпроцесс извлечения (D113, D127): `zero-agent extract html` этого же бинарника через
/// `za_jail::Runner` с каталогом `ZA_HOME/work/tmp/doctor`; ответ должен содержать текст
/// страницы. Нет — `Warn`: PDF в `read` и страницы `web_fetch` не читаются.
async fn check_extractor(home: Option<&Path>) -> Check {
    const LOST: &str = "PDF в read и страницы web_fetch не читаются";
    let Some(home) = home else {
        return Check::Warn("не проверено: каталог данных не определён (ZA_HOME)".to_owned());
    };
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return Check::Warn(format!("путь бинарника не определён ({e}): {LOST}")),
    };
    let tmp = home.join("work").join("tmp").join("doctor");
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        return Check::Warn(format!("каталог {} не создан ({e}): {LOST}", tmp.display()));
    }
    let ex = crate::extract::ProcessExtractor::new(Arc::new(DirectRunner), exe.clone(), tmp);
    let started = Instant::now();
    let r = ex
        .html(HtmlRequest {
            html: PROBE_HTML.to_owned(),
            base_url: "http://doctor.invalid/".to_owned(),
        })
        .await;
    match r {
        Ok(md) if md.text.contains("zero-agent doctor probe") => Check::Ok(format!(
            "подпроцесс extract отвечает: HTML за {} мс ({})",
            started.elapsed().as_millis(),
            exe.display()
        )),
        Ok(_) => Check::Warn(format!("подпроцесс extract вернул не тот текст: {LOST}")),
        Err(e) => Check::Warn(format!("подпроцесс extract не отвечает ({e}): {LOST}")),
    }
}

/// Скиллы вне проекта (D114, D122): по источникам, пропущенные и корни, не прочитанные
/// пределом `[skills] max_entries` (перенос задачи 12); скиллы проекта — `skills list
/// --project`.
async fn check_skills(home: Option<&Path>, cfg: Option<&AgentConfig>) -> Check {
    let (Some(home), Some(cfg)) = (home, cfg) else {
        return no_config();
    };
    if !cfg.skills.enabled {
        return Check::Ok("скиллы выключены ([skills] enabled = false)".to_owned());
    }
    let cat = match crate::skills::catalog(home, &cfg.skills, None).await {
        Ok(c) => c,
        Err(e) => return Check::Warn(format!("каталог не прочитан: {e}")),
    };
    let mut text = format!("скиллы: {}", crate::skills::summary(&cat));
    let mut limits = crate::skills::limit_lines(&cat, &cfg.skills);
    limits.extend(crate::skills::linked_lines(&cat));
    if cat.skipped.is_empty() {
        return Check::Ok(format!(
            "{text}; скиллы проекта — zero-agent skills list --project"
        ));
    }
    text.push_str(&format!(
        "; пропущено {} (zero-agent skills list)",
        cat.skipped.len()
    ));
    for l in limits {
        text.push_str(&format!("; {l}"));
    }
    Check::Warn(text)
}

/// Предупреждение D169: тир standard или full на окне меньше 32k (у standard окна по
/// умолчанию от 32k, `Tier::for_context`): набор инструментов standard превышает слот окна
/// 16k, рабочая часть окна мала. Окно неизвестно (`n_ctx` — `None`) — run считает его 8192
/// (D43), и при явном тире standard или full предупреждение то же, с пометкой (m4 ревью
/// задачи 14). `None` — тира нет (он берётся по окну), тир small или окно 32k и больше.
pub fn tier_window_warning(tier: Option<Tier>, n_ctx: Option<u32>) -> Option<String> {
    let t = tier?;
    let n = n_ctx.unwrap_or(za_agent::DEFAULT_N_CTX);
    if t == Tier::Small || n >= za_agent::STANDARD_MIN_WINDOW {
        return None;
    }
    let window = match n_ctx {
        Some(n) => format!("окне {n} токенов"),
        None => format!("окне {n} токенов (окно модели неизвестно — run считает его таким, D43)"),
    };
    Some(format!(
        "тир {} выбран на {window}, меньше {}: описания инструментов тира не помещаются в слот окна, а на модель остаётся мало места (D169); на таком окне — тир small или модель с окном от 32k",
        t.as_str(),
        za_agent::STANDARD_MIN_WINDOW
    ))
}

/// Строка `window` (D169): `[context] tier` против окна каждой модели с известным окном
/// (без `[context] tier` тир берётся по окну и предупреждения нет; флаг `--tier` `run`
/// проверяет при старте сам).
fn check_window(tier: Option<Tier>, windows: &[(String, Option<u32>)]) -> Check {
    let known: Vec<(&str, u32)> = windows
        .iter()
        .filter_map(|(id, n)| n.map(|n| (id.as_str(), n)))
        .collect();
    if known.is_empty() {
        return Check::Ok("окно моделей неизвестно: тир — по окну при старте run".to_owned());
    }
    let mut warn = Vec::new();
    let mut ok = Vec::new();
    for (id, n) in known {
        let t = tier.unwrap_or_else(|| Tier::for_context(n));
        match tier_window_warning(Some(t), Some(n)) {
            Some(w) => warn.push(format!("{id}: {w}")),
            None => ok.push(format!("{id}: окно {n}, тир {}", t.as_str())),
        }
    }
    if warn.is_empty() {
        Check::Ok(ok.join("; "))
    } else {
        Check::Warn(warn.join("; "))
    }
}

/// Источник секретов: «источник секретов: переменные ZA_SECRET_* (Vault памяти — с
/// клиентом ZeroAIMemory)». `short` — имена секретов со значением короче 8 символов: они
/// не маскируются, это `Warn` с именами (значения не печатаются).
pub fn check_secrets(short: &[String]) -> Check {
    let text = format!("источник секретов: {}", source_text("env"));
    if short.is_empty() {
        Check::Ok(text)
    } else {
        Check::Warn(format!(
            "{text}; значения короче 8 символов не маскируются: {}",
            short.join(", ")
        ))
    }
}

/// Предел `git --version` в `doctor`.
const GIT_VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// git в `PATH` (абсолютные каталоги, как у чекпоинтов D89) и его версия: `git --version`
/// через `za_jail::Runner` с очищенным окружением. Нет git или он не запустился — `Warn`
/// (чекпоинты без git — копиями файлов).
async fn check_git() -> Check {
    let Some(git) = find_in_path(if cfg!(windows) { "git.exe" } else { "git" }) else {
        return Check::Warn(
            "git не найден в PATH: чекпоинты — копиями затронутых файлов".to_owned(),
        );
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut spec = CommandSpec::new(git.clone(), cwd, GIT_VERSION_TIMEOUT);
    spec.args = vec!["--version".into()];
    spec.env_mode = EnvMode::Clear;
    let runner: Arc<dyn Runner> = Arc::new(DirectRunner);
    match runner.run(spec).await {
        Ok(o) if o.exit == Exit::Code(0) => {
            Check::Ok(format!("{} ({})", o.stdout.trim(), git.display()))
        }
        Ok(o) => Check::Warn(format!("{}: git --version: {:?}", git.display(), o.exit)),
        Err(e) => Check::Warn(format!("{}: {e}", git.display())),
    }
}

/// Файл `name` в абсолютных каталогах `PATH`.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Оболочка `bash` (D85): `pwsh`/`powershell` на Windows, `bash`/`sh` иначе.
fn check_shell() -> Check {
    match za_tools::Shell::detect() {
        Some(s) => Check::Ok(format!("{} ({:?})", s.program.display(), s.dialect)),
        None => {
            Check::Warn("оболочка для bash не найдена в PATH: команды не запустятся".to_owned())
        }
    }
}

/// Модели по id и окна готовых моделей (`context_tokens`, D169).
type Models = (Vec<(String, Check)>, Vec<(String, Option<u32>)>);

/// GPU, модели и их окна по конфигурации `home`.
async fn check_home(
    home: Option<&Path>,
    cfg: Option<Result<AgentConfig, AppConfigError>>,
) -> (Check, Vec<(String, Check)>, Vec<(String, Option<u32>)>) {
    let (Some(home), Some(cfg)) = (home, cfg) else {
        let w = || Check::Warn("каталог данных не определён (ZA_HOME)".to_owned());
        return (w(), vec![("config".to_owned(), w())], Vec::new());
    };
    let cfg = match cfg {
        Ok(c) => c,
        Err(e) => {
            // Конфигурация не прочитана: GPU — по `nvidia-smi` из PATH.
            let gpu = gpu_check(home, &ResourcesConfig::default()).await;
            return (
                gpu,
                vec![("config".to_owned(), Check::Fail(e.to_string()))],
                Vec::new(),
            );
        }
    };
    let models = async {
        if cfg.models.as_ref().is_none_or(|m| m.endpoints.is_empty()) {
            return Models::default();
        }
        match start_gateway(home, &cfg, false).await {
            Ok(s) => {
                s.gateway.wait_probed(PROBE_WAIT).await;
                let view = s.gateway.view();
                let warnings = view
                    .warnings
                    .into_iter()
                    .map(|w| ("config".to_owned(), Check::Warn(w)));
                let windows = view
                    .endpoints
                    .iter()
                    .map(|v| (v.id.0.clone(), v.context_tokens))
                    .collect();
                let checks = view
                    .endpoints
                    .iter()
                    .map(|v| (v.id.0.clone(), check_endpoint(v)))
                    .chain(warnings)
                    .collect();
                (checks, windows)
            }
            Err(e) => (
                vec![("config".to_owned(), Check::Fail(e.to_string()))],
                Vec::new(),
            ),
        }
    };
    let (gpu, (models, windows)) = tokio::join!(gpu_check(home, &cfg.resources), models);
    (gpu, models, windows)
}

/// Один вызов `nvidia-smi` (D6) с очищенным окружением и временным каталогом
/// монитора `ZA_HOME/work/tmp/monitor` (D33).
async fn gpu_check(home: &Path, res: &ResourcesConfig) -> Check {
    let mc = MonitorConfig::from_resources(res, home.to_path_buf(), Vec::new(), None);
    let tmp = mc.tmp_dir();
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        tracing::warn!(dir = %tmp.display(), error = %e, "не создан временный каталог монитора");
    }
    check_gpu(&query_nvidia_smi(&mc.nvidia_smi, GPU_QUERY_TIMEOUT, &tmp).await)
}

/// Итог проверки GPU: карты — `Ok`, недоступен (нет NVIDIA, нет `nvidia-smi`) — `Warn`.
pub fn check_gpu(g: &GpuStatus) -> Check {
    match g {
        GpuStatus::Available(cards) if !cards.is_empty() => {
            Check::Ok(crate::resources::render_gpu(g).join("; "))
        }
        GpuStatus::Available(_) => Check::Warn("nvidia-smi не вернул ни одной карты".to_owned()),
        GpuStatus::Unavailable { reason } => Check::Warn(format!("GPU недоступен: {reason}")),
    }
}

/// Итог проверки модели: `ready` — `Ok`, прочие состояния (`inactive`, `unreachable`,
/// `probing`, `open`, `disabled`) — `Warn` с причиной.
pub fn check_endpoint(v: &EndpointView) -> Check {
    if v.state == "ready" {
        let ctx = v
            .context_tokens
            .map(|n| format!(", контекст {n}"))
            .unwrap_or_default();
        let slots = v.slots.map(|n| format!(", слотов {n}")).unwrap_or_default();
        let model = v.model.as_deref().unwrap_or("модель не названа");
        Check::Ok(format!("ready: {model}{ctx}{slots}"))
    } else {
        Check::Warn(state_text(v, Some(PROBE_WAIT)))
    }
}

/// Версия компилятора сборки и MSRV.
pub fn rust_version() -> String {
    format!("{RUSTC_VERSION} (MSRV {MSRV})")
}

/// Сравнивает версию SQLite с минимальной: `Ok("<версия> (минимум <min>)")` или `Fail`.
pub fn check_sqlite(version: &str, min: &str) -> Check {
    match (parse_version(version), parse_version(min)) {
        (Some(v), Some(m)) if v >= m => Check::Ok(format!("{version} (минимум {min})")),
        (Some(_), Some(_)) => Check::Fail(format!("{version} старше требуемой {min}")),
        _ => Check::Fail(format!("не удалось разобрать версию SQLite {version:?}")),
    }
}

/// `x.y.z` → `(x, y, z)`; недостающие части считаются нулём.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.trim().split('.').map(str::parse::<u32>);
    let major = parts.next()?.ok()?;
    let minor = parts.next().transpose().ok()?.unwrap_or(0);
    let patch = parts.next().transpose().ok()?.unwrap_or(0);
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Зонд `llama-server`: адрес не задан или сервер недоступен — `Warn`, не `Fail`.
///
/// Адрес печатается без учётных данных, параметров запроса и фрагмента
/// ([`display_url`]); адрес с ними не зондируется: текст ошибки HTTP-клиента содержит
/// адрес целиком.
pub async fn check_llama(url: Option<&str>) -> Check {
    let Some(url) = url.map(str::trim).filter(|u| !u.is_empty()) else {
        return Check::Warn(format!("{LLAMA_URL_ENV} не задан, llama.cpp не проверялся"));
    };
    let shown = display_url(url);
    if shown != url {
        return Check::Warn(format!(
            "{shown}: {LLAMA_URL_ENV} с учётными данными или параметрами запроса не проверяется"
        ));
    }
    match za_llm::probe_llama(url).await {
        Ok(p) => {
            let model = if p.model.is_empty() {
                "модель не названа".to_owned()
            } else {
                p.model
            };
            let slots = p
                .total_slots
                .map(|n| format!(", слотов {n}"))
                .unwrap_or_default();
            Check::Ok(format!("{url}: {model}, n_ctx {}{slots}", p.n_ctx))
        }
        Err(e) => Check::Warn(format!("{url}: {e}")),
    }
}

/// Адрес для вывода: схема, хост, порт и путь — без `user:password@`, `?query` и
/// `#fragment`.
pub fn display_url(url: &str) -> String {
    let url = url.trim();
    let url = url.split(['?', '#']).next().unwrap_or("");
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (Some(s), r),
        None => (None, url),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let host = authority.rsplit('@').next().unwrap_or(authority);
    match scheme {
        Some(s) => format!("{s}://{host}{path}"),
        None => format!("{host}{path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_url_hides_credentials_and_query() {
        assert_eq!(
            display_url("http://u:sk-test-xyz@127.0.0.1:9/v1?key=sk-q#f"),
            "http://127.0.0.1:9/v1"
        );
        assert_eq!(
            display_url("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080"
        );
        assert_eq!(display_url("sk-a@host:1/p"), "host:1/p");
    }

    #[tokio::test]
    async fn llama_url_with_credentials_is_not_printed() {
        let c = check_llama(Some("http://u:sk-test-xyz@127.0.0.1:9/?t=sk-q")).await;
        assert!(matches!(c, Check::Warn(_)), "{c:?}");
        assert!(!c.detail().contains("sk-"), "{c:?}");
        assert!(c.detail().contains("http://127.0.0.1:9/"), "{c:?}");
    }

    #[test]
    fn sqlite_minimum_is_inclusive() {
        assert_eq!(
            check_sqlite("3.51.3", "3.51.3"),
            Check::Ok("3.51.3 (минимум 3.51.3)".to_owned())
        );
        assert!(matches!(check_sqlite("3.54.1", "3.51.3"), Check::Ok(_)));
        assert!(matches!(check_sqlite("4.0", "3.51.3"), Check::Ok(_)));
    }

    #[test]
    fn old_or_garbled_sqlite_fails() {
        assert!(matches!(check_sqlite("3.51.2", "3.51.3"), Check::Fail(_)));
        assert!(matches!(check_sqlite("3.9.99", "3.51.3"), Check::Fail(_)));
        assert!(matches!(check_sqlite("три", "3.51.3"), Check::Fail(_)));
        assert!(matches!(check_sqlite("3.51.3.1", "3.51.3"), Check::Fail(_)));
    }

    #[test]
    fn bundled_sqlite_passes() {
        let c = check_sqlite(za_store::sqlite_library_version(), za_store::MIN_SQLITE);
        assert!(matches!(c, Check::Ok(_)), "{c:?}");
    }

    #[tokio::test]
    async fn llama_unset_is_a_warning() {
        assert!(matches!(check_llama(None).await, Check::Warn(_)));
        assert!(matches!(check_llama(Some("  ")).await, Check::Warn(_)));
    }

    #[test]
    fn gpu_unavailable_is_a_warning() {
        let c = check_gpu(&GpuStatus::Unavailable {
            reason: "nvidia-smi не найден".to_owned(),
        });
        assert!(matches!(c, Check::Warn(_)), "{c:?}");
    }

    #[test]
    fn short_secrets_are_a_warning() {
        let c = check_secrets(&["pin".to_owned()]);
        assert!(
            matches!(&c, Check::Warn(d) if d.contains("pin") && d.contains("короче 8")),
            "{c:?}"
        );
        assert!(matches!(check_secrets(&[]), Check::Ok(_)));
    }

    #[test]
    fn failure_only_from_fail() {
        let mut r = DoctorReport {
            sqlite: Check::Ok("3.54.1".to_owned()),
            llama: Check::Warn("нет".to_owned()),
            gpu: Check::Warn("нет".to_owned()),
            secrets: check_secrets(&[]),
            git: Check::Warn("нет".to_owned()),
            shell: Check::Ok("bash".to_owned()),
            web: Check::Ok("веб".to_owned()),
            search: Check::Warn("поиск не настроен".to_owned()),
            extractor: Check::Ok("extract".to_owned()),
            skills: Check::Ok("скиллы".to_owned()),
            window: Check::Ok("окно".to_owned()),
            memory: Check::Warn("память не настроена".to_owned()),
            models: vec![("local".to_owned(), Check::Warn("inactive".to_owned()))],
            rust: rust_version(),
        };
        assert!(!r.has_failure());
        r.models
            .push(("config".to_owned(), Check::Fail("ошибка".to_owned())));
        assert!(r.has_failure());
        r.models.clear();
        r.sqlite = Check::Fail("старая".to_owned());
        assert!(r.has_failure());
    }

    /// D169: standard (и full) на окне меньше 32k — предупреждение; small и окна от 32k —
    /// нет; без `[context] tier` тир берётся по окну.
    #[test]
    fn standard_below_32k_warns() {
        assert!(tier_window_warning(Some(Tier::Standard), Some(16_384)).is_some());
        assert!(tier_window_warning(Some(Tier::Full), Some(16_384)).is_some());
        assert!(tier_window_warning(Some(Tier::Standard), Some(32_768)).is_none());
        assert!(tier_window_warning(Some(Tier::Small), Some(8192)).is_none());
        assert!(tier_window_warning(None, Some(8192)).is_none());
        // m4 ревью задачи 14: окно неизвестно — run считает его 8192 (D43), тир явный.
        let unknown = tier_window_warning(Some(Tier::Standard), None).unwrap();
        assert!(
            unknown.contains("окно модели неизвестно") && unknown.contains("8192"),
            "{unknown}"
        );
        assert!(tier_window_warning(Some(Tier::Small), None).is_none());
        assert!(tier_window_warning(None, None).is_none());
        let w = vec![("local".to_owned(), Some(16_384))];
        let c = check_window(Some(Tier::Standard), &w);
        assert!(
            matches!(&c, Check::Warn(d) if d.starts_with("local: тир standard") && d.contains("16384")),
            "{c:?}"
        );
        assert!(matches!(check_window(None, &w), Check::Ok(d) if d.contains("тир small")));
        assert!(matches!(check_window(None, &[]), Check::Ok(_)));
    }

    /// D152: http на loopback и частные адреса — не предупреждение; на прочие хосты и имена
    /// (кроме `localhost`) — предупреждение; https — нет.
    #[test]
    fn plaintext_token_warning_by_host() {
        for (url, warn) in [
            ("http://127.0.0.1:7700", false),
            ("http://localhost:7700", false),
            ("http://10.0.0.5:7700", false),
            ("http://192.168.0.210:7700", false),
            ("http://172.16.1.1:7700", false),
            ("http://[::1]:7700", false),
            ("http://[fd00::1]:7700", false),
            ("http://example.org:7700", true),
            ("http://8.8.8.8:7700", true),
            ("http://memory.lan:7700", true),
            ("http://[2001:db8::1]:7700", true),
            ("https://example.org", false),
        ] {
            assert_eq!(plaintext_outside_lan(url), warn, "{url}");
        }
    }

    #[test]
    fn json_shape() {
        let r = DoctorReport {
            sqlite: Check::Ok("3.54.1".to_owned()),
            llama: Check::Warn("нет".to_owned()),
            gpu: Check::Ok("RTX".to_owned()),
            secrets: check_secrets(&[]),
            git: Check::Ok("git version 2.47.0".to_owned()),
            shell: Check::Warn("нет".to_owned()),
            web: Check::Ok("веб включён".to_owned()),
            search: Check::Warn("поиск не настроен".to_owned()),
            extractor: Check::Ok("extract".to_owned()),
            skills: Check::Ok("скиллы".to_owned()),
            window: Check::Warn("окно".to_owned()),
            memory: Check::Ok("ZeroAIMemory".to_owned()),
            models: vec![("local".to_owned(), Check::Ok("ready".to_owned()))],
            rust: "rustc 1.99.1".to_owned(),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "sqlite": {"status": "ok", "detail": "3.54.1"},
                "llama": {"status": "warn", "detail": "нет"},
                "gpu": {"status": "ok", "detail": "RTX"},
                "secrets": {"status": "ok", "detail": "источник секретов: переменные ZA_SECRET_* (Vault памяти — с клиентом ZeroAIMemory)"},
                "git": {"status": "ok", "detail": "git version 2.47.0"},
                "shell": {"status": "warn", "detail": "нет"},
                "web": {"status": "ok", "detail": "веб включён"},
                "search": {"status": "warn", "detail": "поиск не настроен"},
                "extractor": {"status": "ok", "detail": "extract"},
                "skills": {"status": "ok", "detail": "скиллы"},
                "window": {"status": "warn", "detail": "окно"},
                "memory": {"status": "ok", "detail": "ZeroAIMemory"},
                "models": [["local", {"status": "ok", "detail": "ready"}]],
                "rust": "rustc 1.99.1",
            })
        );
    }
}
