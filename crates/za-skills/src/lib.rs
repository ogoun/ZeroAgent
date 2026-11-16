//! Скиллы (R1-C2, SKL-001…007, D114–D118): разбор `SKILL.md` своим подмножеством YAML
//! ([`parse_skill_md`]), обход доверенных деревьев ([`tree_listing`] — один код для набора
//! доверия проекта и для чтения скиллов), обнаружение и каталог ([`discover`]), загрузка с
//! проверкой хэша ([`load`]).
//!
//! Зависит только от `za-protocol` и `za-core` (`cargo xtask policy`); `za-agent` может
//! зависеть от него, обратного нет ни в каком виде. Весь ввод-вывод блокирующий: из
//! асинхронного кода — через `spawn_blocking`.

mod discover;
mod load;
mod parse;
mod tree;
mod yaml;

pub use discover::{
    Catalog, DiscoverInput, LOCAL_TREE_LIMITS, Limits, ProjectSkills, SKILL_FILE, SKIP_DIRS,
    SkillMeta, Skipped, discover,
};
pub use load::{LoadError, load};
pub use parse::{
    COMPATIBILITY_MAX_CHARS, DESCRIPTION_MAX_CHARS, MAX_FILE_BYTES, MAX_FRONTMATTER_BYTES,
    NAME_MAX_CHARS, ParseError, Parsed, allowed_tools, parse_skill_md, valid_name,
};
pub use tree::{
    EXTERNAL_LINK, LINK_PREFIX, PROJECT_TREES, Place, TOO_LARGE_PREFIX, TRUST_LIMITS, TreeLimits,
    TreeListing, file_value, tree_hash, tree_listing,
};
pub use yaml::Value;

/// Поставка (core pack, D120): пары (имя каталога, текст `SKILL.md`). Тексты — задача 12;
/// до неё — пустой срез.
pub fn bundled() -> &'static [(&'static str, &'static str)] {
    &[]
}
