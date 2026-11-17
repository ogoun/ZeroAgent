//! Секреты бинарника (R1-C1 D90; R1-M D143, D144, D160): кэш [`StoreSecrets`] для `Gateway`
//! и маскировки, маска значений JSON, фильтр секретов чекпоинтов (R1-C2 D103), команда
//! `zero-agent secret list`.
//!
//! Источник ключей — Vault памяти ZeroAIMemory (решение владельца 1): команда раскрывает
//! только нужные ей ключи ([`crate::memory::wait_for_keys`]), [`StoreSecrets::for_command`]
//! кладёт их в кэш, маска — снимок маски процесса после раскрытия (D160). Команды без
//! раскрытия — [`StoreSecrets::unrevealed`] (`models`, `doctor`, `inspect`, `resources`) и
//! [`StoreSecrets::patterns_only`] (`audit`, `journal export`, `checkpoints restore`: журнал
//! маскировался при записи, D100, D144). `ZA_SECRET_*` читает только сборка с признаком
//! `test-hooks` без `[memory]` (тесты R1-A…R1-C2); обычная и release-сборка — нет.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;
use za_llm::config::secret_env_name;
use za_llm::{Secret, SecretSource};
use za_memory::{SolutionVault, VaultScope};
use za_secrets::{MIN_SECRET_LEN, MaskCell, SecretMasker, SecretStore, known_values};
use za_tools::SecretFilter;

use crate::config::{AgentConfig, load_config};
use crate::memory::{KeyRing, KeySource, Memory, command_memory, fallback_reason};

/// Версия правил фильтра секретов чекпоинтов (входит в отпечаток: смена правил — новая
/// проверка файлов). v2 — ключи PGP и PuTTY (финальное ревью безопасности, Minor 2).
const FILTER_RULES_V: &str = "zero-agent checkpoint filter v2 (values, pem, pgp, ppk)";

/// Описание Vault как источника секретов (`secret list`, `doctor`, D152).
pub const VAULT_SOURCE: &str = "Vault памяти (решение, затем личный)";

/// Секреты команды: значения, причины «нет ключа», маска (D90, D143, D160).
#[derive(Clone)]
pub struct StoreSecrets {
    /// Вид источника: `vault`, `env` (тестовая сборка), `none` (памяти нет), `patterns`.
    kind: &'static str,
    /// Имя → значение; пустые значения не хранятся.
    cache: BTreeMap<String, String>,
    /// `key_ref` → причина «нет ключа», которую назвала фаза раскрытия.
    missing: BTreeMap<String, String>,
    /// Источник кольца: причина для прочих `key_ref`; `None` — подсказка `ZA_SECRET_*`.
    source: Option<KeySource>,
    /// Маска команды: снимок маски процесса после раскрытия (D160).
    masker: SecretMasker,
}

impl Default for StoreSecrets {
    /// Без секретов, маска — только шаблоны.
    fn default() -> Self {
        Self::patterns_only()
    }
}

impl StoreSecrets {
    /// Читает все секреты `store` один раз: `names()` и `get` на каждое имя; маска — по
    /// известным значениям и шаблонам D90.
    pub fn from_store(store: &dyn SecretStore) -> Self {
        let cache = store
            .names()
            .into_iter()
            .filter_map(|n| {
                let v = store.get(&n).filter(|v| !v.is_empty())?;
                Some((n, v))
            })
            .collect();
        let mut s = Self {
            kind: store.kind(),
            cache,
            missing: BTreeMap::new(),
            source: None,
            masker: SecretMasker::default(),
        };
        s.masker = SecretMasker::new(known_values(&s));
        s
    }

    /// Ключи команды после фазы раскрытия (D143, D160): значения и причины кольца `ring`;
    /// значения кольца — в маске процесса `cell` (у Vault они там уже есть, у `ZA_SECRET_*`
    /// тестовой сборки добавляются здесь), маска команды — её снимок после этого.
    pub fn for_command(ring: &KeyRing, cell: &MaskCell) -> Self {
        cell.extend(
            ring.values()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        );
        let kind = match ring.source {
            KeySource::Vault => "vault",
            KeySource::Env => "env",
            KeySource::Off(_) => "none",
        };
        Self {
            kind,
            cache: ring
                .values()
                .iter()
                .filter(|(_, v)| !v.is_empty())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            missing: ring.missing().clone(),
            source: Some(ring.source.clone()),
            masker: (*cell.current()).clone(),
        }
    }

    /// Команда без раскрытия ключей (`models`, `doctor`, `inspect`, `resources`, D143):
    /// эндпоинты с `key_ref` — `Inactive` с причиной; в тестовой сборке без `[memory]` —
    /// `ZA_SECRET_*`.
    pub fn unrevealed(cfg: &AgentConfig) -> Self {
        Self::for_command(&KeyRing::unrevealed(cfg), &MaskCell::default())
    }

    /// Только шаблоны (D144): `audit`, `journal export` и `checkpoints restore` ключи не
    /// раскрывают — журнал маскировался при записи (D100), фильтр чекпоинтов — только PEM.
    pub fn patterns_only() -> Self {
        Self {
            kind: "patterns",
            cache: BTreeMap::new(),
            missing: BTreeMap::new(),
            source: None,
            masker: SecretMasker::default(),
        }
    }

    /// Маскировщик команды: известные значения (раскрытые, токен памяти) и шаблоны D90.
    pub fn masker(&self) -> SecretMasker {
        self.masker.clone()
    }

    /// Фильтр секретов чекпоинтов (D103): `true`, если в байтах файла есть известное
    /// значение хранилища (`known_values`), блок PEM закрытого ключа или закрытого ключа
    /// PGP ([`has_private_key_pem`]) либо файл — ключ PuTTY ([`is_putty_key`]). Широкие шаблоны
    /// [`SecretMasker`] не применяются: ложное срабатывание молча лишило бы файл
    /// чекпоинта и восстановления. Размер файла ограничивает `za-tools`
    /// (`SECRET_SCAN_MAX`).
    ///
    /// Отпечаток фильтра — BLAKE3 отсортированного набора значений (с длинами, с меткой
    /// версии правил PEM): `za-tools` кэширует итог проверки файла, пока отпечаток и stat
    /// файла те же (ревью задачи 2, сомнение 2). Отпечаток живёт только в памяти.
    pub fn checkpoint_filter(&self) -> SecretFilter {
        let mut values: Vec<Vec<u8>> = known_values(self)
            .into_iter()
            .map(|(_, v)| v.into_bytes())
            .collect();
        values.sort();
        values.dedup();
        let mut h = blake3::Hasher::new();
        h.update(FILTER_RULES_V.as_bytes());
        for v in &values {
            h.update(&(v.len() as u64).to_le_bytes());
            h.update(v);
        }
        let fingerprint = *h.finalize().as_bytes();
        let finders: Vec<memchr::memmem::Finder<'static>> = values
            .iter()
            .map(|v| memchr::memmem::Finder::new(v).into_owned())
            .collect();
        SecretFilter::new(fingerprint, move |bytes: &[u8]| {
            finders.iter().any(|f| f.find(bytes).is_some())
                || has_private_key_pem(bytes)
                || is_putty_key(bytes)
        })
    }

    /// Имена секретов со значением короче [`MIN_SECRET_LEN`] символов: маска их не
    /// ищет (D90), `secret list` и `doctor` об этом предупреждают (ревью задачи 10, m3).
    pub fn short_names(&self) -> Vec<String> {
        self.cache
            .iter()
            .filter(|(_, v)| v.chars().count() < MIN_SECRET_LEN)
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Значение по `key_ref` в любом написании, дающем ту же переменную
    /// (`open-router`, `open_router`, `OPEN.ROUTER`), как у `ZA_SECRET_<KEY_REF>` (D4).
    fn lookup(&self, key_ref: &str) -> Option<&String> {
        if let Some(v) = self.cache.get(key_ref) {
            return Some(v);
        }
        let want = secret_env_name(key_ref);
        self.cache
            .iter()
            .find(|(n, _)| secret_env_name(n) == want)
            .map(|(_, v)| v)
    }
}

impl SecretSource for StoreSecrets {
    fn get(&self, key_ref: &str) -> Option<Secret> {
        self.lookup(key_ref).map(|v| Secret::new(v.clone()))
    }

    /// Причина источника (D143): «ждёт Vault памяти», «не найден в Vault», «память не
    /// настроена», «не раскрыт этой командой»; у `ZA_SECRET_*` тестовой сборки — подсказка о
    /// переменной.
    fn missing_reason(&self, key_ref: &str) -> String {
        if let Some(r) = self.missing.get(key_ref) {
            return r.clone();
        }
        match &self.source {
            Some(src) => fallback_reason(src, key_ref),
            None => za_llm::http::missing_key_reason(key_ref),
        }
    }
}

impl SecretStore for StoreSecrets {
    fn get(&self, name: &str) -> Option<String> {
        self.lookup(name).cloned()
    }

    fn names(&self) -> Vec<String> {
        self.cache.keys().cloned().collect()
    }

    fn kind(&self) -> &'static str {
        self.kind
    }
}

impl fmt::Debug for StoreSecrets {
    /// Только вид и имена: значения не печатаются.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreSecrets")
            .field("kind", &self.kind)
            .field("names", &self.cache.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Начало первого вхождения `needle` в `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Блок PEM закрытого ключа: строка `-----BEGIN … PRIVATE KEY-----` (PKCS#8, RSA, EC,
/// DSA, OpenSSH, зашифрованный) или брони закрытого ключа PGP `-----BEGIN PGP PRIVATE KEY
/// BLOCK-----` (RFC 4880, финальное ревью безопасности, Minor 2), метка — в пределах одной
/// строки. Открытые ключи, подписи и сообщения PGP — не секрет.
fn has_private_key_pem(bytes: &[u8]) -> bool {
    const BEGIN: &[u8] = b"-----BEGIN ";
    const END: &[u8] = b"PRIVATE KEY-----";
    const PGP_END: &[u8] = b"PRIVATE KEY BLOCK-----";
    /// Длина метки самое большее (`ENCRYPTED PRIVATE KEY-----` и подобные).
    const LABEL_MAX: usize = 64;
    let mut rest = bytes;
    while let Some(i) = find(rest, BEGIN) {
        let after = &rest[i + BEGIN.len()..];
        let line_end = after
            .iter()
            .position(|b| *b == b'\n' || *b == b'\r')
            .unwrap_or(after.len());
        let label = &after[..line_end.min(LABEL_MAX)];
        if find(label, END).is_some() || find(label, PGP_END).is_some() {
            return true;
        }
        rest = after;
    }
    false
}

/// Файл ключа PuTTY (`.ppk`, финальное ревью безопасности, Minor 2): строка
/// `PuTTY-User-Key-File-<версия>:` в начале файла (после необязательной метки порядка
/// байтов UTF-8). Формат не PEM, и имя файла может быть любым. Упоминание формата в тексте
/// не в начале файла — не секрет.
fn is_putty_key(bytes: &[u8]) -> bool {
    const MAGIC: &[u8] = b"PuTTY-User-Key-File-";
    const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
    bytes
        .strip_prefix(BOM)
        .unwrap_or(bytes)
        .strip_prefix(MAGIC)
        .is_some_and(|rest| {
            let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
            digits > 0 && rest.get(digits) == Some(&b':')
        })
}

/// Значение JSON, в котором замаскированы все строки и ключи объектов (D90, D100):
/// аргументы вызовов, payload событий, записи аудита. Маскируются декодированные
/// значения строк; при выводе `serde_json` кодирует их обратно. Строка, которая сама —
/// текст JSON (`raw_arguments`), обходит общий [`za_protocol::mask_json_with`].
pub fn mask_json(m: &SecretMasker, v: &Value) -> Value {
    za_protocol::mask_json_with(&|s: &str| m.mask(s).into_owned(), v)
}

/// Строка списка `secret list`: имя, задан ли, переменная окружения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub name: String,
    pub set: bool,
    /// Имя упомянуто как `key_ref` в `config.toml`.
    pub in_config: bool,
    /// Значение короче 8 символов: не маскируется.
    pub short: bool,
}

/// Список `secret list`: `key_ref` из конфигурации и имена секретов хранилища, без
/// повторов (одна переменная — одна строка, написание из конфигурации), по имени.
pub fn list_entries(key_refs: &[String], secrets: &StoreSecrets) -> Vec<ListEntry> {
    let short = |k: &str| {
        secrets
            .lookup(k)
            .is_some_and(|v| v.chars().count() < MIN_SECRET_LEN)
    };
    let mut by_var: BTreeMap<String, ListEntry> = BTreeMap::new();
    for k in key_refs {
        by_var
            .entry(secret_env_name(k))
            .or_insert_with(|| ListEntry {
                name: k.clone(),
                set: secrets.lookup(k).is_some(),
                in_config: true,
                short: short(k),
            });
    }
    for n in SecretStore::names(secrets) {
        by_var
            .entry(secret_env_name(&n))
            .or_insert_with(|| ListEntry {
                short: short(&n),
                name: n,
                set: true,
                in_config: false,
            });
    }
    let mut out: Vec<ListEntry> = by_var.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Текст `secret list`: без значений, только имена, переменные и отметка «задан».
pub fn render_list(entries: &[ListEntry], kind: &str) -> String {
    let mut out = format!("источник секретов: {}\n", source_text(kind));
    if entries.is_empty() {
        out.push_str("секретов нет: ни key_ref в config.toml, ни переменных ZA_SECRET_*\n");
        return out;
    }
    for e in entries {
        let state = if e.set {
            "задан"
        } else {
            "не задан"
        };
        let mut note = String::new();
        if !e.in_config {
            note.push_str("  (нет в config.toml)");
        }
        if e.short {
            note.push_str(SHORT_NOTE);
        }
        out.push_str(&format!(
            "  {:<20} {:<9} {}{note}\n",
            e.name,
            state,
            secret_env_name(&e.name)
        ));
    }
    out
}

/// Пометка `secret list` у значения короче 8 символов.
const SHORT_NOTE: &str = "  (короче 8 символов — не маскируется)";

/// Описание источника секретов для `secret list` и `doctor`.
pub fn source_text(kind: &str) -> String {
    match kind {
        "env" => "переменные ZA_SECRET_* (сборка с test-hooks без [memory])".to_owned(),
        "vault" => VAULT_SOURCE.to_owned(),
        other => other.to_owned(),
    }
}

/// `zero-agent secret list [--project P]` (D144): `key_ref` конфигурации — где найден в Vault
/// (решения, затем личном), тип записи, «Vault заблокирован»; отдельной строкой — число
/// записей окружения. Значения не раскрываются. Vault решения — у доверенного проекта
/// `project` (по умолчанию — текущий каталог). Нет `[memory]` — ключей моделей нет; в
/// тестовой сборке без `[memory]` — список `ZA_SECRET_*`. Ошибка конфигурации не мешает
/// списку: предупреждение в stderr.
pub async fn cmd_secret_list(home: &Path, project: Option<&Path>) -> ExitCode {
    let cfg = match load_config(home) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("zero-agent: config.toml не прочитан, key_ref не показаны: {e}");
            AgentConfig::default()
        }
    };
    let key_refs: Vec<String> = cfg
        .models
        .as_ref()
        .map(|m| {
            m.endpoints
                .iter()
                .filter_map(|e| e.key_ref.clone())
                .collect()
        })
        .unwrap_or_default();
    let ring = KeyRing::unrevealed(&cfg);
    match &ring.source {
        KeySource::Env => {
            let secrets = StoreSecrets::for_command(&ring, &MaskCell::default());
            print!(
                "{}",
                render_list(
                    &list_entries(&key_refs, &secrets),
                    SecretStore::kind(&secrets)
                )
            );
        }
        KeySource::Off(why) => print!("{}", render_unavailable(&key_refs, &why.reason())),
        KeySource::Vault => {
            let (mem, _) = command_memory(home, &cfg, project).await;
            print!("{}", vault_list(&mem, &key_refs).await);
        }
    }
    ExitCode::SUCCESS
}

/// `secret list` без памяти: ключей моделей нет.
fn render_unavailable(key_refs: &[String], why: &str) -> String {
    let mut out = format!(
        "источник секретов: {VAULT_SOURCE} — {why}: ключей моделей нет (ZA_SECRET_* не читаются)\n"
    );
    for k in key_refs {
        out.push_str(&format!("  {k:<20} недоступен\n"));
    }
    out
}

/// `secret list` по Vault памяти: поиск без раскрытия (D144).
async fn vault_list(mem: &Memory, key_refs: &[String]) -> String {
    let mut out = format!("источник секретов: {VAULT_SOURCE}\n");
    let Some(rt) = mem.runtime() else {
        return out;
    };
    if !rt.client.has_token() {
        out.push_str("  токен памяти не задан (ZA_MEMORY_TOKEN): Vault не проверен\n");
        return out;
    }
    let vault = rt.keys.vault();
    let solution = rt.keys.solution();
    let mut refs: Vec<&String> = key_refs.iter().collect();
    refs.sort();
    refs.dedup();
    for k in refs {
        let line = match vault.locate(k, solution).await {
            Ok((item, locked)) => {
                let place = match item.scope {
                    VaultScope::Solution(_) => "в Vault решения",
                    VaultScope::Knowledge => "в личном Vault",
                };
                let lock = match locked {
                    Some(r) if r.trim().is_empty() => "  (Vault заблокирован)".to_owned(),
                    Some(r) => format!("  (Vault заблокирован: {r})"),
                    None => String::new(),
                };
                format!("  {k:<20} {place}  {}{lock}\n", item.item_type)
            }
            Err(e) => format!("  {k:<20} {e}\n"),
        };
        out.push_str(&line);
    }
    if let SolutionVault::Untrusted = solution {
        out.push_str("  Vault решения не читается: проект не доверен (zero-agent trust)\n");
    }
    match vault.environment(solution).await {
        Ok(env) => out.push_str(&format!(
            "записей окружения в Vault: {} (host, environment — не ключи, не раскрываются)\n",
            env.len()
        )),
        Err(e) => out.push_str(&format!("записи окружения не проверены: {e}\n")),
    }
    out
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use za_secrets::EnvStore;

    use super::*;

    /// Заглушка хранилища со счётчиком обращений `get`.
    struct Counting {
        gets: AtomicUsize,
        values: BTreeMap<String, String>,
    }

    impl SecretStore for Counting {
        fn get(&self, name: &str) -> Option<String> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            self.values.get(name).cloned()
        }
        fn names(&self) -> Vec<String> {
            self.values.keys().cloned().collect()
        }
        fn kind(&self) -> &'static str {
            "stub"
        }
    }

    fn env(list: &[(&str, &str)]) -> StoreSecrets {
        StoreSecrets::from_store(&EnvStore::from_vars(
            list.iter()
                .map(|(k, v)| (OsString::from(*k), OsString::from(*v))),
        ))
    }

    #[test]
    fn store_secrets_caches_at_start() {
        let stub = Counting {
            gets: AtomicUsize::new(0),
            values: [("a", "value-aaaaaaaa"), ("open-router", "value-or-123456")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        };
        let s = StoreSecrets::from_store(&stub);
        let n = stub.names().len();
        assert_eq!(stub.gets.load(Ordering::SeqCst), n);
        for _ in 0..3 {
            assert_eq!(
                SecretSource::get(&s, "open_router").map(|k| k.expose().to_owned()),
                Some("value-or-123456".to_owned())
            );
            assert!(SecretSource::get(&s, "missing").is_none());
            let _ = s.masker();
        }
        assert_eq!(stub.gets.load(Ordering::SeqCst), n, "get после построения");
        assert_eq!(SecretStore::kind(&s), "stub");
    }

    /// Поведение прежнего `EnvSecrets`: ключ — `ZA_SECRET_<KEY_REF>`, пустое значение —
    /// не задан.
    #[test]
    fn store_secrets_resolve_key_refs_like_env_secrets() {
        let s = env(&[
            ("ZA_SECRET_OPEN_ROUTER", "or-key-123456"),
            ("ZA_SECRET_ANTHROPIC", "an-key-123456"),
            ("ZA_SECRET_EMPTY", ""),
            ("OPEN_ROUTER", "not-a-secret"),
        ]);
        for k in ["open-router", "open_router", "OPEN.ROUTER"] {
            assert_eq!(
                SecretSource::get(&s, k).map(|v| v.expose().to_owned()),
                Some("or-key-123456".to_owned()),
                "{k}"
            );
        }
        assert_eq!(
            SecretSource::get(&s, "anthropic").map(|v| v.expose().to_owned()),
            Some("an-key-123456".to_owned())
        );
        assert!(SecretSource::get(&s, "empty").is_none());
        assert_eq!(
            za_secrets::env_var_name("my.key-2"),
            secret_env_name("my.key-2")
        );
    }

    #[test]
    fn debug_has_no_values() {
        let s = env(&[("ZA_SECRET_ANTHROPIC", "an-key-123456-zz")]);
        let d = format!("{s:?}");
        assert!(d.contains("anthropic"), "{d}");
        assert!(!d.contains("an-key-123456-zz"), "{d}");
        let m = format!("{:?}", s.masker());
        assert!(!m.contains("an-key-123456-zz"), "{m}");
    }

    #[test]
    fn mask_json_args() {
        let m = SecretMasker::new(vec![("anthropic".into(), "verylongsecretvalue".into())]);
        let v = json!({"cmd": "echo verylongsecretvalue", "n": 1, "list": ["verylongsecretvalue", true],
                       "verylongsecretvalue": null, "nested": {"url": "https://u:p4ss@host/x"}});
        assert_eq!(
            mask_json(&m, &v),
            json!({"cmd": "echo [secret masked: anthropic]", "n": 1,
                   "list": ["[secret masked: anthropic]", true],
                   "[secret masked: anthropic]": null,
                   "nested": {"url": "https://u:[secret masked: url_password]@host/x"}})
        );
    }

    /// Фиктивный токен GitHub, собранный из частей: цельная строка формата токена в
    /// исходнике блокируется защитой push от секретов.
    fn ghp() -> String {
        ["gh", "p_0123456789abcdefghijklmnopqrstuvwxyz"].concat()
    }

    /// `raw_arguments` — текст JSON в строке (ревью задачи 10, I2): токен после `\n`
    /// маскируется без переписывания текста; токен, скрытый `g`, — через
    /// декодирование и новое кодирование.
    #[test]
    fn mask_json_raw_arguments() {
        let m = SecretMasker::new(vec![]);
        let tok = ghp();
        // Порядок ключей и пробелы сохраняются: маска сырого текста.
        let raw = format!(r#"{{"z": 1, "cmd": "cd x\nexport GH={tok}\nrun"}}"#);
        let v = mask_json(&m, &json!({"raw_arguments": raw}));
        assert_eq!(
            v["raw_arguments"],
            json!(r#"{"z": 1, "cmd": "cd x\nexport GH=[secret masked: github_token]\nrun"}"#)
        );
        let raw = format!(r#"{{"c":"a\n{tok}"}}"#);
        let v = mask_json(&m, &json!({"raw_arguments": raw}));
        assert_eq!(
            v["raw_arguments"],
            json!(r#"{"c":"a\n[secret masked: github_token]"}"#)
        );
        // `g` в виде `g`: в сыром тексте префикса нет, в декодированном — есть.
        let hidden = format!(r#"{{"c":"g{}"}}"#, &tok[1..]);
        let v = mask_json(&m, &json!({"raw_arguments": hidden}));
        let out = v["raw_arguments"].as_str().unwrap();
        assert!(!out.contains(&tok[1..]), "{out}");
        assert_eq!(
            serde_json::from_str::<Value>(out).unwrap(),
            json!({"c": "[secret masked: github_token]"})
        );
        // Не JSON и неразбираемый JSON — только маска текста.
        let v = mask_json(&m, &json!(["{not json", format!("{{\"a\": \"{tok}")]));
        assert_eq!(
            v,
            json!(["{not json", "{\"a\": \"[secret masked: github_token]"])
        );
    }

    #[test]
    fn short_values_are_reported() {
        let s = env(&[
            ("ZA_SECRET_PIN", "1234567"),
            ("ZA_SECRET_LONG", "long-enough-1"),
        ]);
        assert_eq!(s.short_names(), vec!["pin".to_owned()]);
        let e = list_entries(&[], &s);
        assert!(e.iter().any(|x| x.name == "pin" && x.short));
        assert!(e.iter().any(|x| x.name == "long" && !x.short));
        let text = render_list(&e, "env");
        assert!(text.contains("короче 8 символов"), "{text}");
        assert!(!text.contains("1234567"), "{text}");
    }

    #[test]
    fn list_merges_config_and_store_without_values() {
        let s = env(&[
            ("ZA_SECRET_ANTHROPIC", "an-key-123456"),
            ("ZA_SECRET_EXTRA_KEY", "ex-key-123456"),
        ]);
        let refs = vec![
            "anthropic".to_owned(),
            "open-router".to_owned(),
            "Anthropic".to_owned(),
        ];
        let e = list_entries(&refs, &s);
        assert_eq!(
            e,
            vec![
                ListEntry {
                    name: "anthropic".into(),
                    set: true,
                    in_config: true,
                    short: false,
                },
                ListEntry {
                    name: "extra-key".into(),
                    set: true,
                    in_config: false,
                    short: false,
                },
                ListEntry {
                    name: "open-router".into(),
                    set: false,
                    in_config: true,
                    short: false,
                },
            ]
        );
        let text = render_list(&e, "env");
        assert!(text.contains("ZA_SECRET_OPEN_ROUTER"), "{text}");
        assert!(!text.contains("an-key-123456"), "{text}");
        assert!(!text.contains("ex-key-123456"), "{text}");
    }

    /// Совместимость с `za-jail`: каждая переменная, из которой `EnvStore` берёт
    /// секрет, снимается из окружения дочернего процесса (`EnvMode::Inherit`).
    #[test]
    fn jail_strips_every_var_env_store_reads() {
        let vars: Vec<(OsString, OsString)> = [
            "ZA_SECRET_A",
            "ZA_SECRET_OPEN_ROUTER",
            "za_secret_lower",
            "Za_Secret_Mixed",
            "ZA_SECRET_X_1",
            "PATH",
        ]
        .iter()
        .map(|k| (OsString::from(*k), OsString::from("value-12345678")))
        .collect();
        for windows in [true, false] {
            let read = EnvStore::from_vars_with(vars.clone().into_iter(), windows);
            let spec = za_jail::CommandSpec::new("x", ".", std::time::Duration::from_secs(1));
            let child = za_jail::child_env(vars.clone().into_iter(), &spec, windows);
            let read_vars: Vec<String> = read
                .names()
                .iter()
                .map(|n| za_secrets::env_var_name(n))
                .collect();
            assert!(!read_vars.is_empty());
            for v in &read_vars {
                assert!(
                    !child
                        .iter()
                        .any(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case(v)),
                    "{v} дошла до дочернего процесса (windows={windows})"
                );
            }
            assert!(child.iter().any(|(k, _)| k == "PATH"));
        }
    }

    /// D103: фильтр чекпоинтов — только точные признаки. Текст «как секрет» без
    /// известного значения и без PEM не секрет (нет ложного пропуска); известное значение
    /// и блок закрытого ключа — секрет; открытый ключ и сертификат — нет.
    #[test]
    fn pattern_like_text_is_not_secret() {
        let value = ["zeta", "-filter-", "value-5d1e"].concat();
        let s = env(&[("ZA_SECRET_DB", value.as_str()), ("ZA_SECRET_SHORT", "abc")]);
        let f = s.checkpoint_filter();
        let ghp = ["ghp_", "abcdefghijklmnopqrstuvwxyz0123456789"].concat();
        for text in [
            "api_key = example\n".to_owned(),
            "password: hunter2\nsecret = changeme\n".to_owned(),
            format!("token = {ghp}\n"),
            "-----BEGIN PUBLIC KEY-----\nMIIB\n-----END PUBLIC KEY-----\n".to_owned(),
            "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".to_owned(),
            "-----BEGIN \nPRIVATE KEY-----\n".to_owned(),
            "abc abc abc\n".to_owned(),
            String::new(),
        ] {
            assert!(!f.matches(text.as_bytes()), "{text:?}");
        }
        let pem = |label: &str| format!("x\n-----BEGIN {label}-----\nMII\n-----END {label}-----\n");
        for text in [
            format!("DB_URL=postgres://u:{value}@h/db\n"),
            pem(&["PRIVATE", " KEY"].concat()),
            pem(&["RSA PRIVATE", " KEY"].concat()),
            pem(&["OPENSSH PRIVATE", " KEY"].concat()),
            pem(&["ENCRYPTED PRIVATE", " KEY"].concat()),
        ] {
            assert!(f.matches(text.as_bytes()), "{text:?}");
        }
        // Без секретов в хранилище — только PEM.
        let none = env(&[]).checkpoint_filter();
        assert!(!none.matches(format!("v={value}").as_bytes()));
    }

    /// Финальное ревью безопасности, Minor 2: закрытый ключ PGP (блок брони
    /// `PGP PRIVATE KEY BLOCK`) и ключ PuTTY (`PuTTY-User-Key-File-` в начале файла) —
    /// секрет. Признаки точные: открытый ключ PGP, подпись, сообщение, упоминание формата
    /// PuTTY в тексте и строка не в начале файла секретом не считаются. Фиктивные ключи
    /// собираются из частей (сканеры репозитория их не видят целиком).
    #[test]
    fn pgp_and_putty_keys_are_secret() {
        let f = env(&[]).checkpoint_filter();
        let armor =
            |label: &str| format!("-----BEGIN {label}-----\n\nlQOYBF\n-----END {label}-----\n");
        let pgp = ["PGP PRIVATE", " KEY BLOCK"].concat();
        let ppk = ["PuTTY-User-", "Key-File-3: ssh-ed25519\nEncryption: none\n"].concat();
        for text in [
            armor(&pgp),
            format!("notes\n{}", armor(&pgp)),
            ppk.clone(),
            format!("\u{feff}{ppk}"),
            ["PuTTY-User-", "Key-File-2: ssh-rsa\r\n"].concat(),
        ] {
            assert!(f.matches(text.as_bytes()), "{text:?}");
        }
        for text in [
            armor("PGP PUBLIC KEY BLOCK"),
            armor("PGP SIGNATURE"),
            armor("PGP MESSAGE"),
            format!("-----BEGIN {pgp}\n"),
            format!("The file starts with {ppk}"),
            format!("\n{ppk}"),
            "PuTTY-User-Key-File".to_owned(),
        ] {
            assert!(!f.matches(text.as_bytes()), "{text:?}");
        }
    }

    /// Сомнение 2 ревью задачи 2: отпечаток фильтра зависит только от набора значений —
    /// тот же набор в другом порядке и под другими именами даёт тот же отпечаток, другое
    /// значение — другой.
    #[test]
    fn filter_fingerprint_follows_value_set() {
        let a = ["zeta", "-fp-", "value-0001"].concat();
        let b = ["zeta", "-fp-", "value-0002"].concat();
        let one = env(&[("ZA_SECRET_A", a.as_str()), ("ZA_SECRET_B", b.as_str())]);
        let same = env(&[("ZA_SECRET_X", b.as_str()), ("ZA_SECRET_Y", a.as_str())]);
        let other = env(&[("ZA_SECRET_A", a.as_str())]);
        let fp = one.checkpoint_filter().fingerprint();
        assert_eq!(fp, same.checkpoint_filter().fingerprint());
        assert_ne!(fp, other.checkpoint_filter().fingerprint());
        assert_ne!(fp, env(&[]).checkpoint_filter().fingerprint());
    }
}
