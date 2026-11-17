//! Секреты агента (R1-C1 D90, D100; R1-M D143, D160; SEC-040, SEC-041, SEC-044).
//!
//! - [`SecretStore`] — источник секретов по имени `key_ref`. В продукте ключи моделей и
//!   поиска берутся только из Vault памяти ZeroAIMemory (решение владельца 1, D143):
//!   реализация — `za_memory::keys::VaultKeys`. [`EnvStore`] (`ZA_SECRET_*`) остаётся для
//!   тестов `za-secrets` и `za-llm`, CI и сборки бинарника с признаком `test-hooks` без
//!   `[memory]`; обычная и release-сборка бинарника его не читают. Хранилищ ОС нет; записи
//!   секретов из агента нет.
//! - [`known_values`] — значения для маскировки, [`SecretMasker`] — сканер: известные
//!   значения и шаблоны ключей D90 заменяются на `[secret masked: …]`.
//! - [`MaskCell`] — маска процесса, пересобираемая после каждого раскрытия Vault (D160).
//!
//! Крейт без зависимостей (D73): маска значений JSON — у потребителей, поверх
//! [`SecretMasker::mask`] для каждой строки.

mod cell;
mod scan;
mod store;

pub use cell::MaskCell;
pub use scan::{MASK_PREFIX, MaskStream, SecretMasker};
pub use store::{ENV_PREFIX, EnvStore, MIN_SECRET_LEN, SecretStore, env_var_name, known_values};
