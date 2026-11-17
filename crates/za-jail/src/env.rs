//! Окружение дочернего процесса (D84; R1-M D159): разрешённый набор вместо унаследованного
//! целиком; секреты агента и переменные памяти (`ZA_MEMORY_*`, `ZAM_*`) детям не уходят.

use std::ffi::{OsStr, OsString};

use crate::{CommandSpec, EnvMode};

/// Префиксы имён, которые `EnvMode::Inherit` снимает без учёта регистра на любой
/// платформе:
/// - `ZA_SECRET` — секреты агента: `ZA_SECRET_*` и `ZA_SECRETS_*` (D84);
/// - `ZA_MEMORY_` — переменные клиента памяти агента, прежде всего токен
///   `ZA_MEMORY_TOKEN` (R1-M D134, D159);
/// - `ZAM_` — переменные клиента `zam` ZeroAIMemory: `ZAM_URL`, `ZAM_TOKEN`,
///   `ZAM_SESSION*`, `ZAM_MACHINE` и будущие (D159).
///
/// Префиксы памяти — с подчёркиванием: `ZAMBA_X` и `ZA_MEMORYX` остаются.
const REMOVED_PREFIXES: &[&str] = &["ZA_SECRET", "ZA_MEMORY_", "ZAM_"];
/// Переменные, которые `EnvMode::Inherit` снимает всегда: каталог данных агента, адрес
/// его модели (`LLAMA_URL` может содержать учётные данные, ревью задачи 5 M9) и маркер
/// дерева родителя (свой ставит `DirectRunner` на Linux).
///
/// Прочие ключи провайдеров пользователя (`*_API_KEY`, `*_TOKEN`) не снимаются: по D84 и
/// SEC-041 собственные секреты агента — `ZA_SECRET_*` и токен памяти `ZA_MEMORY_TOKEN`
/// (снимаются по префиксу, [`REMOVED_PREFIXES`]), а `ZAM_TOKEN` клиента `zam` — тоже по
/// префиксу (D159).
const ALWAYS_REMOVED: &[&str] = &["ZA_HOME", "LLAMA_URL", "ZA_JAIL_CALL"];
/// Единственные переменные родителя, которые переходят в `EnvMode::Clear`.
const CLEAR_KEEP: &[&str] = &["PATH", "SystemRoot", "WINDIR"];
/// Переменные временного каталога, указывающие на `CommandSpec::tmp_dir`.
const TMP_VARS: &[&str] = &["TMP", "TEMP", "TMPDIR"];
/// Неинтерактивный git: без запроса пароля в терминале и окна Git Credential Manager.
const NON_INTERACTIVE: &[(&str, &str)] =
    &[("GIT_TERMINAL_PROMPT", "0"), ("GCM_INTERACTIVE", "never")];

/// Окружение дочернего процесса из окружения-основы `base` (обычно `std::env::vars_os()`).
///
/// - `EnvMode::Inherit`: всё из `base`, кроме секретов (`ZA_SECRET*`), переменных памяти
///   (`ZA_MEMORY_*`, `ZAM_*`, R1-M D159), `ZA_HOME`, `LLAMA_URL`, `ZA_JAIL_CALL` и
///   `spec.remove_env`; плюс `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never`.
/// - `EnvMode::Clear`: из `base` — только `PATH`, `SystemRoot`, `WINDIR`.
///
/// Затем `TMP`, `TEMP`, `TMPDIR` = `spec.tmp_dir` (если задан) и `spec.env_set` (заменяют
/// одноимённые). `windows` — имена без учёта регистра (`Path` и `PATH` — одна переменная).
/// Префиксы [`REMOVED_PREFIXES`] снимаются без учёта регистра на любой платформе:
/// `za_secret_x` или `zam_token` на Unix — другие имена, но передавать их дочернему
/// процессу незачем (консервативнее спецификации D84).
pub fn child_env(
    base: impl Iterator<Item = (OsString, OsString)>,
    spec: &CommandSpec,
    windows: bool,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = match spec.env_mode {
        EnvMode::Inherit => base
            .filter(|(k, _)| {
                !is_secret(k)
                    && !ALWAYS_REMOVED
                        .iter()
                        .any(|r| same_name(k, OsStr::new(r), windows))
                    && !spec.remove_env.iter().any(|r| same_name(k, r, windows))
            })
            .collect(),
        EnvMode::Clear => base
            .filter(|(k, _)| {
                CLEAR_KEEP
                    .iter()
                    .any(|w| same_name(k, OsStr::new(w), windows))
            })
            .collect(),
    };
    if spec.env_mode == EnvMode::Inherit {
        for (k, v) in NON_INTERACTIVE {
            set(&mut env, OsString::from(k), OsString::from(v), windows);
        }
    }
    if let Some(tmp) = &spec.tmp_dir {
        for k in TMP_VARS {
            set(
                &mut env,
                OsString::from(k),
                tmp.as_os_str().to_owned(),
                windows,
            );
        }
    }
    for (k, v) in &spec.env_set {
        set(&mut env, k.clone(), v.clone(), windows);
    }
    env
}

/// Имя секрета или переменной памяти: начинается с одного из [`REMOVED_PREFIXES`] без
/// учёта регистра.
fn is_secret(name: &OsStr) -> bool {
    let n = name.to_string_lossy();
    REMOVED_PREFIXES.iter().any(|p| {
        n.get(..p.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(p))
    })
}

/// Одна ли это переменная: на Windows — без учёта регистра ASCII.
fn same_name(a: &OsStr, b: &OsStr, windows: bool) -> bool {
    if windows {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

/// Ставит переменную, убирая прежние одноимённые.
fn set(env: &mut Vec<(OsString, OsString)>, k: OsString, v: OsString, windows: bool) {
    env.retain(|(n, _)| !same_name(n, &k, windows));
    env.push((k, v));
}
