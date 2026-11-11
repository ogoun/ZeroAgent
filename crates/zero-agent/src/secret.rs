//! Секреты бинарника (R1-C1 D90): кэш [`StoreSecrets`] для `Gateway` и маскировки,
//! маска значений JSON, фильтр секретов чекпоинтов (R1-C2 D103), команда
//! `zero-agent secret list`.
//!
//! Источник — [`za_secrets::SecretStore`]; до клиента памяти ZeroAIMemory это
//! [`EnvStore`] (`ZA_SECRET_<KEY_REF>`), для пользователя поведение прежнее. Значения
//! читаются один раз при старте команды и дальше отдаются из кэша: `Gateway` берёт
//! ключи, маскировщик — известные значения (`known_values`).

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use serde_json::Value;
use za_llm::config::secret_env_name;
use za_llm::{Secret, SecretSource};
use za_secrets::{EnvStore, MIN_SECRET_LEN, SecretMasker, SecretStore, known_values};
use za_tools::SecretFilter;

use crate::config::load_config;

/// Секреты, прочитанные из хранилища при старте (D90, 1.15).
#[derive(Clone, Default)]
pub struct StoreSecrets {
    /// Вид исходного хранилища (`env`).
    kind: &'static str,
    /// Имя → значение; пустые значения не хранятся.
    cache: BTreeMap<String, String>,
}

impl StoreSecrets {
    /// Читает все секреты `store` один раз: `names()` и `get` на каждое имя.
    pub fn from_store(store: &dyn SecretStore) -> Self {
        let cache = store
            .names()
            .into_iter()
            .filter_map(|n| {
                let v = store.get(&n).filter(|v| !v.is_empty())?;
                Some((n, v))
            })
            .collect();
        Self {
            kind: store.kind(),
            cache,
        }
    }

    /// Секреты из переменных окружения процесса `ZA_SECRET_*` ([`EnvStore`]).
    pub fn from_env() -> Self {
        Self::from_store(&EnvStore::from_vars(std::env::vars_os()))
    }

    /// Маскировщик по известным значениям кэша и шаблонам D90.
    pub fn masker(&self) -> SecretMasker {
        SecretMasker::new(known_values(self))
    }

    /// Фильтр секретов чекпоинтов (D103): `true`, если в байтах файла есть известное
    /// значение хранилища (`known_values`) или блок PEM закрытого ключа. Широкие шаблоны
    /// [`SecretMasker`] не применяются: ложное срабатывание молча лишило бы файл
    /// чекпоинта и восстановления. Размер файла ограничивает `za-tools`
    /// (`SECRET_SCAN_MAX`).
    pub fn checkpoint_filter(&self) -> SecretFilter {
        let values: Vec<Vec<u8>> = known_values(self)
            .into_iter()
            .map(|(_, v)| v.into_bytes())
            .collect();
        Arc::new(move |bytes: &[u8]| {
            values.iter().any(|v| find(bytes, v).is_some()) || has_private_key_pem(bytes)
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
/// DSA, OpenSSH, зашифрованный), метка — в пределах одной строки.
fn has_private_key_pem(bytes: &[u8]) -> bool {
    const BEGIN: &[u8] = b"-----BEGIN ";
    const END: &[u8] = b"PRIVATE KEY-----";
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
        if find(label, END).is_some() {
            return true;
        }
        rest = after;
    }
    false
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
        "env" => "переменные ZA_SECRET_* (Vault памяти — с клиентом ZeroAIMemory)".to_owned(),
        other => other.to_owned(),
    }
}

/// `zero-agent secret list`. Ошибка конфигурации не мешает списку переменных:
/// предупреждение в stderr, список — по `ZA_SECRET_*`.
pub fn cmd_secret_list(home: &Path) -> ExitCode {
    let key_refs = match load_config(home) {
        Ok(cfg) => cfg
            .models
            .map(|m| {
                m.endpoints
                    .into_iter()
                    .filter_map(|e| e.key_ref)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        Err(e) => {
            eprintln!("zero-agent: config.toml не прочитан, key_ref не показаны: {e}");
            Vec::new()
        }
    };
    let secrets = StoreSecrets::from_env();
    print!(
        "{}",
        render_list(
            &list_entries(&key_refs, &secrets),
            SecretStore::kind(&secrets)
        )
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

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
            assert!(!f(text.as_bytes()), "{text:?}");
        }
        let pem = |label: &str| format!("x\n-----BEGIN {label}-----\nMII\n-----END {label}-----\n");
        for text in [
            format!("DB_URL=postgres://u:{value}@h/db\n"),
            pem(&["PRIVATE", " KEY"].concat()),
            pem(&["RSA PRIVATE", " KEY"].concat()),
            pem(&["OPENSSH PRIVATE", " KEY"].concat()),
            pem(&["ENCRYPTED PRIVATE", " KEY"].concat()),
        ] {
            assert!(f(text.as_bytes()), "{text:?}");
        }
        // Без секретов в хранилище — только PEM.
        let none = env(&[]).checkpoint_filter();
        assert!(!none(format!("v={value}").as_bytes()));
    }
}
