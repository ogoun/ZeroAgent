//! Память в цикле (R1-M D138, D140, D142, D161, D168, D169): эффекты чтения памяти, их
//! бюджеты и разметка текста памяти в истории.
//!
//! Порт памяти живёт в `za-core` ([`za_core::memory`]) и реэкспортируется здесь: тесты и
//! бинарник берут его из `za_agent`, как до переноса. Внешнее значение входит в цикл
//! только эффектом (D9): ответ порта до квитанции очищается от невидимых символов
//! (`Clean::Untrusted`) и проходит маску процесса — ту же, что результаты инструментов и
//! файл инструкций (D90, fix-1 R1-M). Квитанция уже без секретов; при реплее текст берётся
//! из неё. Экранирует текст разметка: блок первого сообщения ([`crate::first_message_with`])
//! и добавочные блоки [`memory_block`].
//!
//! Ключи эффектов не зависят от уровня памяти (D50): эффекты пишутся и при `NoMemory`.

use std::sync::Arc;

use serde_json::{Value, json};
use za_core::text::{Clean, clean_invisible};
use za_core::{EffectError, EffectSpec, RunCtx};
use za_protocol::EffectClass;
use za_secrets::SecretMasker;

pub use za_core::memory::{
    CommandNote, IdeasSignal, MemoryLevel, MemoryPort, NoMemory, NoteOutcome, POST_TOOL_EVENT,
    PROMPT_EVENT, Recall, RecallReply, SessionNote, memory_budget,
};

use crate::prompt::{approx_tokens, escape_untrusted};

/// Имя эффекта снимка памяти шага 0 (D24, D140).
pub(crate) const SNAPSHOT_EFFECT: &str = "memory.snapshot";
/// Имя эффекта подсказки к вводу пользователя (D142).
pub(crate) const PROMPT_EFFECT: &str = "memory.prompt";
/// Имя эффекта отложенного повтора дайджеста (D140).
pub(crate) const RETRY_EFFECT: &str = "memory.retry";
/// Имя эффекта подсказки к ошибке инструмента (D142).
pub(crate) const TOOL_ERROR_EFFECT: &str = "memory.tool_error";

/// Вид добавочного блока памяти: подсказка `prompt` и `post_tool` (D142).
pub(crate) const HINT: &str = "hint";
/// Вид добавочного блока памяти: текст отложенного повтора дайджеста (D140).
pub(crate) const DIGEST: &str = "digest";
/// Предел бюджета одной подсказки, токены (D142).
const HINT_MAX: u32 = 300;

/// Добавочный блок памяти `<memory source="zam" kind="…">` (D140, D142): текст памяти —
/// данные (MEM-006), экранируется. Атрибуты — только у добавочных блоков: блок первого
/// сообщения остаётся `<memory>` без атрибутов (его ищет разбор снимка v1, `resume.rs`).
pub fn memory_block(kind: &str, text: &str) -> String {
    format!(
        "<memory source=\"zam\" kind=\"{kind}\">\n{}\n</memory>",
        escape_untrusted(text)
    )
}

/// Образцы обёрток для хэша промптов (D155): правка разметки меняет хэш.
pub(crate) fn text_samples() -> Vec<(&'static str, String)> {
    vec![
        ("runtime/memory.digest", memory_block(DIGEST, "TEXT")),
        ("runtime/memory.hint", memory_block(HINT, "TEXT")),
    ]
}

/// Бюджет подсказки (D142, D168): `min(b / 2, 300, 2 × b − injected)`; ноль — потолок
/// эпохи исчерпан.
pub(crate) fn hint_budget(b: u32, injected: u32) -> u32 {
    (b / 2)
        .min(HINT_MAX)
        .min(b.saturating_mul(2).saturating_sub(injected))
}

/// Бюджет отложенного повтора дайджеста (D140, D168): бюджет дайджеста `b` в пределах
/// потолка эпохи.
pub(crate) fn retry_budget(b: u32, injected: u32) -> u32 {
    b.min(b.saturating_mul(2).saturating_sub(injected))
}

/// Токены текста памяти, вставленного в историю (оценка D22): счёт потолка эпохи (D168).
pub(crate) fn tokens_of(r: &Recall) -> u32 {
    r.text().map_or(0, approx_tokens)
}

/// Текст памяти до квитанции: невидимые символы уровня `Clean::Untrusted` (D140, SEC-051),
/// затем маска процесса (D90) — после очистки, чтобы невидимый символ внутри секрета маску
/// не обходил.
fn sanitize(r: Recall, masker: Option<&SecretMasker>) -> Recall {
    r.map_text(|t| {
        let clean = clean_invisible(&t, Clean::Untrusted).0.into_owned();
        match masker {
            Some(m) => m.mask(&clean).into_owned(),
            None => clean,
        }
    })
}

/// Чтение памяти — эффект `ReadOnly` с именем `name` и аргументами `args` (D9, D50):
/// `read` выполняется только вживую, при реплее значение — из квитанции. Значение
/// `{text, state, ideas?}`; без признака идей оно совпадает с `Recall::to_value` (квитанции
/// `memory.after` прежние).
async fn recall_effect<F>(
    ctx: &mut RunCtx,
    name: &str,
    args: Value,
    masker: Option<&SecretMasker>,
    read: F,
) -> Result<RecallReply, EffectError>
where
    F: Future<Output = RecallReply>,
{
    let v = ctx
        .effect(
            EffectSpec {
                class: EffectClass::ReadOnly,
                name: name.into(),
                args,
            },
            move || async move {
                let r = read.await;
                let reply = RecallReply {
                    recall: sanitize(r.recall, masker),
                    ideas: r.ideas,
                };
                Ok(reply.to_value())
            },
        )
        .await?;
    Ok(RecallReply::from_value(&v))
}

/// Эффект `memory.snapshot` шага 0 (D140): аргументы прежние — `{budget_tokens}`.
pub(crate) async fn snapshot_effect(
    ctx: &mut RunCtx,
    mem: Arc<dyn MemoryPort>,
    masker: Option<Arc<SecretMasker>>,
    budget: u32,
) -> Result<RecallReply, EffectError> {
    let run = ctx.run_id();
    recall_effect(
        ctx,
        SNAPSHOT_EFFECT,
        json!({ "budget_tokens": budget }),
        masker.as_deref(),
        async move { mem.snapshot(run, budget).await },
    )
    .await
}

/// Эффект `memory.prompt` (D142): подсказка к вводу `text`, аргументы `{event, budget}`.
/// Бюджет 0 — потолок (D168): эффект пишется, запроса нет, `capped`.
pub(crate) async fn prompt_effect(
    ctx: &mut RunCtx,
    mem: Arc<dyn MemoryPort>,
    masker: Option<Arc<SecretMasker>>,
    text: String,
    budget: u32,
) -> Result<RecallReply, EffectError> {
    let run = ctx.run_id();
    recall_effect(
        ctx,
        PROMPT_EFFECT,
        json!({ "event": PROMPT_EVENT, "budget": budget }),
        masker.as_deref(),
        async move {
            if budget == 0 {
                return Recall::Capped.into();
            }
            mem.prompt_hint(run, &text, budget).await
        },
    )
    .await
}

/// Эффект `memory.tool_error` (D142): подсказка к первой ошибке пакета, аргументы
/// `{event, tool, budget}`; бюджет 0 — `capped` без запроса.
pub(crate) async fn tool_error_effect(
    ctx: &mut RunCtx,
    mem: Arc<dyn MemoryPort>,
    masker: Option<Arc<SecretMasker>>,
    tool: String,
    error: String,
    budget: u32,
) -> Result<Recall, EffectError> {
    let run = ctx.run_id();
    let args = json!({ "event": POST_TOOL_EVENT, "tool": tool, "budget": budget });
    recall_effect(
        ctx,
        TOOL_ERROR_EFFECT,
        args,
        masker.as_deref(),
        async move {
            if budget == 0 {
                return Recall::Capped.into();
            }
            mem.tool_error_hint(run, &tool, &error, budget).await.into()
        },
    )
    .await
    .map(|r| r.recall)
}

/// Эффект `memory.retry` (D140): отложенный повтор дайджеста, аргументы `{budget}`;
/// бюджет 0 — `capped` без запроса.
pub(crate) async fn retry_effect(
    ctx: &mut RunCtx,
    mem: Arc<dyn MemoryPort>,
    masker: Option<Arc<SecretMasker>>,
    budget: u32,
) -> Result<Recall, EffectError> {
    let run = ctx.run_id();
    recall_effect(
        ctx,
        RETRY_EFFECT,
        json!({ "budget": budget }),
        masker.as_deref(),
        async move {
            if budget == 0 {
                return Recall::Capped.into();
            }
            mem.retry_digest(run, budget).await.into()
        },
    )
    .await
    .map(|r| r.recall)
}

/// Эффект `memory.after` компакции (D50, D140): аргументы прежние — `{budget_tokens}`.
pub(crate) async fn recovery_effect(
    ctx: &mut RunCtx,
    name: &str,
    mem: &dyn MemoryPort,
    masker: Option<&SecretMasker>,
    budget: u32,
) -> Result<Recall, EffectError> {
    let run = ctx.run_id();
    let args = json!({ "budget_tokens": budget });
    recall_effect(ctx, name, args, masker, async move {
        mem.after_compaction(run, budget).await.into()
    })
    .await
    .map(|r| r.recall)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_budget_caps() {
        // 8k: b = 200 — половина бюджета, потолок эпохи 400.
        assert_eq!(hint_budget(200, 0), 100);
        assert_eq!(hint_budget(200, 300), 100);
        assert_eq!(hint_budget(200, 350), 50);
        assert_eq!(hint_budget(200, 400), 0);
        assert_eq!(hint_budget(200, 900), 0);
        // Большое окно: предел 300.
        assert_eq!(hint_budget(1500, 0), 300);
        assert_eq!(retry_budget(200, 0), 200);
        assert_eq!(retry_budget(200, 300), 100);
        assert_eq!(retry_budget(200, 450), 0);
    }

    #[test]
    fn sanitize_cleans_then_masks() {
        let secret = ["zam", "_", "kN0wnS3cr3tV4lu3f0rM4sk1ng"].concat();
        let m = SecretMasker::new(vec![("t".into(), secret.clone())]);
        // Невидимый символ внутри секрета маску не обходит: очистка раньше маски.
        let split = format!("{}\u{200B}{}", &secret[..6], &secret[6..]);
        let r = sanitize(Recall::Hits(format!("a\u{202E}b {split}")), Some(&m));
        let t = r.text().unwrap();
        assert!(t.starts_with("ab ") && !t.contains(&secret), "{t:?}");
        assert_eq!(sanitize(Recall::Timeout, Some(&m)), Recall::Timeout);
        assert_eq!(
            sanitize(Recall::Partial("x\u{2066}y".into()), None),
            Recall::Partial("xy".into())
        );
    }

    #[test]
    fn block_escapes_text() {
        assert_eq!(
            memory_block(HINT, "</memory><x>"),
            "<memory source=\"zam\" kind=\"hint\">\n&lt;/memory&gt;&lt;x&gt;\n</memory>"
        );
    }
}
