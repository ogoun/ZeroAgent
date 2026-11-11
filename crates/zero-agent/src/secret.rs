//! Секреты бинарника (R1-C1 D90): кэш [`StoreSecrets`] для `Gateway` и маскировки,
//! маска значений JSON, команда `zero-agent secret list`.
//!
//! Источник — [`za_secrets::SecretStore`]; до клиента памяти ZeroAIMemory это
//! [`EnvStore`] (`ZA_SECRET_<KEY_REF>`), для пользователя поведение прежнее. Значения
//! читаются один раз при старте команды и дальше отдаются из кэша: `Gateway` берёт
//! ключи, маскировщик — известные значения (`known_values`).

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;
use za_llm::config::secret_env_name;
use za_llm::{Secret, SecretSource};
use za_secrets::{EnvStore, SecretMasker, SecretStore, known_values};

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

/// Значение JSON, в котором замаскированы все строки и ключи объектов (D90, D100):
/// аргументы вызовов, payload событий, записи аудита.
pub fn mask_json(m: &SecretMasker, v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(m.mask(s).into_owned()),
        Value::Array(a) => Value::Array(a.iter().map(|x| mask_json(m, x)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (m.mask(k).into_owned(), mask_json(m, x)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Строка списка `secret list`: имя, задан ли, переменная окружения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub name: String,
    pub set: bool,
    /// Имя упомянуто как `key_ref` в `config.toml`.
    pub in_config: bool,
}

/// Список `secret list`: `key_ref` из конфигурации и имена секретов хранилища, без
/// повторов (одна переменная — одна строка, написание из конфигурации), по имени.
pub fn list_entries(key_refs: &[String], secrets: &StoreSecrets) -> Vec<ListEntry> {
    let mut by_var: BTreeMap<String, ListEntry> = BTreeMap::new();
    for k in key_refs {
        by_var
            .entry(secret_env_name(k))
            .or_insert_with(|| ListEntry {
                name: k.clone(),
                set: secrets.lookup(k).is_some(),
                in_config: true,
            });
    }
    for n in SecretStore::names(secrets) {
        by_var
            .entry(secret_env_name(&n))
            .or_insert_with(|| ListEntry {
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
        let note = if e.in_config {
            ""
        } else {
            "  (нет в config.toml)"
        };
        out.push_str(&format!(
            "  {:<20} {:<9} {}{note}\n",
            e.name,
            state,
            secret_env_name(&e.name)
        ));
    }
    out
}

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
                    in_config: true
                },
                ListEntry {
                    name: "extra-key".into(),
                    set: true,
                    in_config: false
                },
                ListEntry {
                    name: "open-router".into(),
                    set: false,
                    in_config: true
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
}
