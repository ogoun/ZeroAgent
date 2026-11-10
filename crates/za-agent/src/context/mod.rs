//! Контекст модели (R1-B2): бюджеты окна, лимит вывода хода, раздел `[context]`,
//! счётчик токенов, защита от тихой обрезки, вводы пользователя (CTX-008…016,
//! D40–D43, D65, D66).
//!
//! [`ContextBudget`] — строка таблицы 2.3 части 02 по `n_ctx` (D40). [`output_plan`] —
//! лимит вывода хода с бюджетом мышления: рассуждение у llama.cpp входит в
//! `max_tokens`, поэтому лимит не меньше бюджета мышления и запаса на ответ. Этот же
//! лимит — «резерв» в порогах удаления старых результатов и компакции (одна мера
//! `оценка + резерв`, D43, D45, D46).
//!
//! [`ContextMeter`] считает токены запроса (D41): база — `usage.input + usage.output`
//! последнего успешного хода, дельта — сообщения после базы, точно через
//! `ModelPort::count_tokens` или оценкой байты / 4 вверх. Оценку на границе шага
//! получает эффект `context` драйвера (D42), поэтому реплей принимает те же решения.
//!
//! [`spill`] выгружает большой вывод инструмента в файл (D44), [`prune`] удаляет старые
//! результаты инструментов из истории (D45).

mod prune;
mod spill;

pub use prune::prune;
pub(crate) use prune::{PRUNE_KEEP_TURNS, PRUNE_MIN_BYTES};
pub(crate) use spill::{NOT_SAVED as SPILL_NOT_SAVED, cut as spill_cut};
pub use spill::{SPILL_DIR, SpillCfg, SpillResult, spill, spill_limit_bytes, spill_name};

use serde::{Deserialize, Serialize};
use za_llm::{
    ChatRequest, EndpointId, Message, ModelPort, Part, PromptBlocks, ReasoningMode, Usage,
    render_tools,
};
use za_protocol::{BlobHash, Tier, canonical_json};

use crate::memory::memory_budget;
use crate::prompt::approx_tokens;

// ---- Бюджеты окна (D40) ----

/// Бюджеты окна модели, токены: строка таблицы 2.3 части 02 (D40).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBudget {
    pub n_ctx: u32,
    /// Промпт (блоки 2–5 с `AGENTS.md`).
    pub prompt: u32,
    /// Описания инструментов; `None` — «по запросу» (64k+).
    pub tools: Option<u32>,
    /// Снимок памяти: [`memory_budget`].
    pub memory: u32,
    pub output_reserve: u32,
    pub compaction_reserve: u32,
}

impl ContextBudget {
    /// Строка таблицы 2.3 по `n_ctx` (D40): меньше 16384 — 8k; меньше 32768 — 16k;
    /// меньше 65536 — 32k; иначе 64k+. `memory` — [`memory_budget`] (не дубль).
    pub fn for_context(n_ctx: u32) -> ContextBudget {
        let (prompt, tools, output_reserve, compaction_reserve) = if n_ctx < 16_384 {
            (800, Some(550), 1000, 1000)
        } else if n_ctx < 32_768 {
            (900, Some(800), 2000, 1500)
        } else if n_ctx < 65_536 {
            (2000, Some(1500), 4000, 3000)
        } else {
            (3000, None, 8000, 6000)
        };
        ContextBudget {
            n_ctx,
            prompt,
            tools,
            memory: memory_budget(n_ctx),
            output_reserve,
            compaction_reserve,
        }
    }
}

/// Запас на ответ сверх бюджета мышления (D40): как `ANSWER_RESERVE` коннектора
/// Anthropic.
pub const ANSWER_RESERVE: u32 = 1024;
/// Бюджет мышления `ReasoningMode::Default` (D40): уровень `High`.
const DEFAULT_THINKING: u32 = 16_384;
/// Нижняя граница потолка мышления (D40).
const MIN_THINKING_CAP: u32 = 1024;

/// Бюджет мышления режима (D40): `Off` — 0, `Effort(e)` — `e.budget_tokens()`,
/// `Budget(n)` — n, `Default` — 16384.
pub fn thinking_budget(r: &ReasoningMode) -> u32 {
    match r {
        ReasoningMode::Off => 0,
        ReasoningMode::Effort(e) => e.budget_tokens(),
        ReasoningMode::Budget(n) => *n,
        ReasoningMode::Default => DEFAULT_THINKING,
    }
}

/// Лимит вывода хода (D40).
#[derive(Clone, Debug, PartialEq)]
pub struct OutputPlan {
    pub max_output_tokens: u32,
    /// `Some(Budget(cap))` — бюджет мышления режима run выше потолка, ход идёт с ним;
    /// `None` — режим run как есть.
    pub reasoning: Option<ReasoningMode>,
}

/// Лимит вывода хода (D40): `think = thinking_budget(r)`, потолок
/// `cap = max(1024, n_ctx / 4)`; `think > cap` — ход с `Budget(cap)`;
/// `max_output_tokens = output_reserve` при `think == 0`, иначе
/// `max(output_reserve, think + ANSWER_RESERVE)`.
pub fn output_plan(b: &ContextBudget, r: &ReasoningMode) -> OutputPlan {
    let mut think = thinking_budget(r);
    let cap = MIN_THINKING_CAP.max(b.n_ctx / 4);
    let mut reasoning = None;
    if think > cap {
        think = cap;
        reasoning = Some(ReasoningMode::Budget(cap));
    }
    let max_output_tokens = if think == 0 {
        b.output_reserve
    } else {
        b.output_reserve.max(think.saturating_add(ANSWER_RESERVE))
    };
    OutputPlan {
        max_output_tokens,
        reasoning,
    }
}

// ---- Раздел [context] (D65) ----

/// Раздел `[context]` конфигурации (D65); замораживается на run в `RunSetup.context`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Тир run; `None` — по `n_ctx`. Флаг `--tier` важнее.
    pub tier: Option<Tier>,
    /// Порог компакции, доля окна (0,70–0,80).
    pub compact_at: f32,
    /// Порог удаления старых результатов, доля окна (больше 0 и меньше `compact_at`).
    pub prune_at: f32,
    /// Лимит выгрузки вывода инструмента, доля окна (0,15–0,20).
    pub spill_share: f32,
}

impl Default for ContextConfig {
    fn default() -> Self {
        ContextConfig {
            tier: None,
            compact_at: 0.75,
            prune_at: 0.65,
            spill_share: 0.15,
        }
    }
}

/// Значение `[context]` вне допустимого диапазона.
#[derive(Debug, thiserror::Error)]
#[error("context.{field}: {value} вне диапазона {range}")]
pub struct ContextConfigError {
    pub field: &'static str,
    pub value: String,
    pub range: &'static str,
}

impl ContextConfig {
    /// Диапазоны D65: `compact_at` 0,70–0,80, `spill_share` 0,15–0,20, `prune_at`
    /// больше 0 и меньше `compact_at`. NaN — вне любого диапазона.
    pub fn validate(&self) -> Result<(), ContextConfigError> {
        let err = |field, value: f32, range| ContextConfigError {
            field,
            value: value.to_string(),
            range,
        };
        if !(0.70..=0.80).contains(&self.compact_at) {
            return Err(err("compact_at", self.compact_at, "0.70–0.80"));
        }
        if !(0.15..=0.20).contains(&self.spill_share) {
            return Err(err("spill_share", self.spill_share, "0.15–0.20"));
        }
        if !(self.prune_at > 0.0 && self.prune_at < self.compact_at) {
            return Err(err(
                "prune_at",
                self.prune_at,
                "больше 0 и меньше compact_at",
            ));
        }
        Ok(())
    }
}

// ---- Счётчик (D41) ----

/// Оценка токенов запроса: `exact` — все части посчитаны токенизатором модели.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Estimate {
    pub tokens: u32,
    pub exact: bool,
}

/// Токенов шаблона чата на сообщение (D41): приближение, `/apply-template` не
/// используется.
const TEMPLATE_TOKENS: u64 = 4;
/// Оценка запроса, выше которой проверяется тихая обрезка (D43).
const TRUNCATION_MIN_ESTIMATE: u32 = 1024;

/// Счётчик контекста run (D41, D43); входит в снимок (D66).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextMeter {
    /// Сообщений истории, покрытых базой.
    pub base_len: usize,
    /// `usage.input + usage.output` последнего успешного хода; `None` — базы нет.
    pub base_tokens: Option<u64>,
    /// Оценка последнего запроса хода (для D43).
    pub last_estimate: Option<u32>,
    /// D43: компакция на ближайшей границе; ход последней компакции по подозрению на обрезку.
    pub compact_pending: bool,
    pub truncation_turn: Option<u32>,
    /// D70.
    pub suppressed: bool,
}

impl ContextMeter {
    /// База после успешного хода: `input + output` и длина истории после фиксации хода.
    /// `usage.input == 0` (провайдер не сообщил расход) — базы нет.
    pub fn after_turn(&mut self, history_len: usize, usage: &Usage) {
        if usage.input == 0 {
            self.base_len = 0;
            self.base_tokens = None;
        } else {
            self.base_len = history_len;
            self.base_tokens = Some(usage.input.saturating_add(usage.output));
        }
    }

    /// После переписывания истории (удаление, компакция): базы нет.
    pub fn reset(&mut self) {
        self.base_len = 0;
        self.base_tokens = None;
    }

    /// Оценка запроса (D41): с базой — база плюс сообщения после неё; без базы —
    /// system, `render_tools` и все сообщения. Сообщение — текст [`message_text`] и 4
    /// токена шаблона. Счёт — `model.count_tokens`; первый отказ счётчика переводит
    /// остаток на оценку байты / 4 вверх (`exact = false`), без новых запросов.
    pub async fn estimate(
        &self,
        req: &ChatRequest,
        model: &dyn ModelPort,
        ep: &EndpointId,
    ) -> Estimate {
        let mut c = Counter {
            model,
            ep,
            live: true,
            exact: true,
        };
        let base = self
            .base_tokens
            .filter(|_| self.base_len <= req.messages.len());
        let (mut total, msgs) = match base {
            Some(b) => (b, &req.messages[self.base_len..]),
            None => {
                let tools = render_tools(&PromptBlocks {
                    tools: req.tools.clone(),
                    ..PromptBlocks::default()
                });
                let head = c.count(&req.system).await + c.count(&tools).await;
                (head, &req.messages[..])
            }
        };
        for m in msgs {
            total = total
                .saturating_add(c.count(&message_text(m)).await)
                .saturating_add(TEMPLATE_TOKENS);
        }
        Estimate {
            tokens: u32::try_from(total).unwrap_or(u32::MAX),
            exact: c.exact,
        }
    }
}

/// Счёт текстов одной оценки.
struct Counter<'a> {
    model: &'a dyn ModelPort,
    ep: &'a EndpointId,
    /// Счётчик модели ещё не отказал в этой оценке.
    live: bool,
    exact: bool,
}

impl Counter<'_> {
    async fn count(&mut self, text: &str) -> u64 {
        if text.is_empty() {
            return 0;
        }
        if self.live {
            match self.model.count_tokens(self.ep, text).await {
                Some(n) => return u64::from(n),
                None => {
                    self.live = false;
                    self.exact = false;
                }
            }
        }
        u64::from(approx_tokens(text))
    }
}

/// Текст сообщения для счёта (D41): тексты частей (и рассуждения), `content`
/// результатов, имя и канонический JSON аргументов вызовов — через `\n`.
pub(crate) fn message_text(m: &Message) -> String {
    let mut parts = Vec::with_capacity(m.parts.len());
    for p in &m.parts {
        match p {
            Part::Text(t) => parts.push(t.clone()),
            Part::Reasoning { text, .. } => parts.push(text.clone()),
            Part::ToolCall(c) => {
                parts.push(c.name.clone());
                parts.push(canonical_json(&c.arguments));
            }
            Part::ToolResult { content, .. } => parts.push(content.clone()),
        }
    }
    parts.join("\n")
}

/// D43: подозрение на тихую обрезку — оценка запроса больше 1024 и расход меньше
/// половины оценки; `input == 0` (расход неизвестен) — `false`.
pub fn truncation_suspected(estimate: u32, input: u64) -> bool {
    input > 0 && estimate > TRUNCATION_MIN_ESTIMATE && input.saturating_mul(2) < u64::from(estimate)
}

// ---- Вводы пользователя (D66) ----

/// D66: ввод пользователя (вход и `correct`) в порядке поступления.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserInput {
    /// Блоб текста после `escape_untrusted`, без обёртки `control`.
    pub text: BlobHash,
    pub correct: bool,
    /// Индекс сообщения в текущей истории; `None` — вне истории (после компакции).
    pub at: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use za_llm::ToolCall;

    #[test]
    fn message_text_joins_parts() {
        let m = Message {
            role: za_llm::Role::Assistant,
            parts: vec![
                Part::Text("t".into()),
                Part::ToolCall(ToolCall {
                    id: "c".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"b": 1, "a": 2}),
                    raw_arguments: None,
                }),
                Part::ToolResult {
                    call_id: "c".into(),
                    content: "r".into(),
                    is_error: false,
                },
            ],
        };
        assert_eq!(message_text(&m), "t\nread\n{\"a\":2,\"b\":1}\nr");
    }

    #[test]
    fn budget_tokens_cap_applies_to_explicit_budget() {
        let b = ContextBudget::for_context(8192);
        assert_eq!(
            output_plan(&b, &ReasoningMode::Budget(5000)),
            OutputPlan {
                max_output_tokens: 3072,
                reasoning: Some(ReasoningMode::Budget(2048))
            }
        );
        assert_eq!(thinking_budget(&ReasoningMode::Default), 16_384);
    }

    #[test]
    fn nan_is_out_of_range() {
        let c = ContextConfig {
            prune_at: f32::NAN,
            ..ContextConfig::default()
        };
        assert_eq!(c.validate().unwrap_err().field, "prune_at");
    }
}
