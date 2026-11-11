//! Секреты агента (R1-C1 D90, D100; SEC-040, SEC-041, SEC-044).
//!
//! - [`SecretStore`] — источник секретов по имени `key_ref`. Целевая реализация —
//!   Vault памяти ZeroAIMemory (решение заказчика, CR-21 `docs/memory_upgrade.md`), она
//!   появится с клиентом памяти. До него — только [`EnvStore`] (`ZA_SECRET_*`).
//!   Хранилищ ОС нет; записи секретов из агента нет.
//! - [`known_values`] — значения для маскировки, [`SecretMasker`] — сканер: известные
//!   значения и шаблоны ключей D90 заменяются на `[secret masked: …]`.
//!
//! Крейт без зависимостей (D73): маска значений JSON — у потребителей, поверх
//! [`SecretMasker::mask`] для каждой строки.

mod scan;
mod store;

pub use scan::{MASK_PREFIX, SecretMasker};
pub use store::{ENV_PREFIX, EnvStore, MIN_SECRET_LEN, SecretStore, env_var_name, known_values};
