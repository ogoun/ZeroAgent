//! Core pack (поставка части 02, раздел 6; D120): 14 скиллов своими текстами (N-13) в
//! `crates/za-skills/pack/<name>/SKILL.md`, встроенные в бинарник (`include_str!`).
//!
//! Формат: frontmatter только `name` и `description` («Use when …» — когда применять),
//! тело — короткие шаги, критерий остановки и что не делать. Английский, ASCII-пунктуация,
//! не больше [`PACK_TOKEN_LIMIT`] токенов cl100k, o200k и оценки Qwen на файл
//! (`cargo xtask prompt-budget`, случаи `skills/<name>`). Тексты входят в хэш промптов под
//! именами [`PACK_TEXT_NAMES`] (PRM-010): их правка меняет `prompt_texts_hash()`.
//!
//! Целостность поставки — целостность бинарника (D114); править скилл поставки
//! пользователь может только своей копией в `ZA_HOME/skills/<name>`, которая его затеняет.

/// Предел текста скилла поставки, токены (D120): активация одного скилла не должна
/// съедать окно 16k.
pub const PACK_TOKEN_LIMIT: u32 = 700;

/// Список поставки — один на имена, тексты и имена в хэше промптов; порядок — по байтам
/// имени (так их выдаёт [`crate::bundled`]).
macro_rules! pack {
    ($($name:literal),* $(,)?) => {
        /// Число скиллов поставки — длина списка: новый скилл не требует править её руками.
        const PACK_LEN: usize = [$($name),*].len();
        /// Имена скиллов поставки по порядку байтов.
        pub const PACK: [&str; PACK_LEN] = [$($name),*];
        /// Имена текстов поставки в хэше промптов: `skills/<name>`, в порядке [`PACK`].
        pub const PACK_TEXT_NAMES: [&str; PACK_LEN] = [$(concat!("skills/", $name)),*];
        /// Пары (имя каталога, текст `SKILL.md`) в порядке [`PACK`].
        pub(crate) static BUNDLED: [(&str, &str); PACK_LEN] = [
            $(($name, include_str!(concat!("../pack/", $name, "/SKILL.md")))),*
        ];
    };
}

pack![
    "diagnose",
    "domain-model",
    "evidence",
    "grill",
    "handoff",
    "orient",
    "research-with-citations",
    "retro",
    "review",
    "router",
    "source-driven",
    "spec",
    "tdd",
    "tickets",
];
