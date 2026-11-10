//! Компакция истории пилой (CTX-008…014, R1-B2 D46–D50).
//!
//! [`compact`] — одна операция с тем же префиксом: эффекты `memory.before` (хук памяти
//! перед компакцией), `compact` (запрос сводки: та же история и управляющее сообщение
//! с рубрикой [`COMPACT_RUBRIC`], без инструментов и мышления, вывод не больше
//! `compaction_reserve`) и `memory.after` (recovery памяти). Итог — история новой эпохи:
//! первое сообщение (`<env>` run, recovery, блок [`summary_block`]), хвост
//! ([`split_tail`]) и рецитация задачи (CTX-014).
//!
//! Аварийная компакция (D49) берёт вход меньше окна: первое сообщение и наибольший
//! суффикс истории с точки разреза ([`cut_points`]), помещающийся в
//! `n_ctx − output_reserve − compaction_reserve`. Решение о разрезе — чистая функция
//! истории (оценка байты / 4, `approx_tokens`), поэтому реплей строит тот же запрос.
//!
//! Сводка — производный недоверенный ввод (CTX-011): цель, сообщения пользователя и
//! часть модели проходят [`escape_untrusted`]; цель и сообщения пользователя вставляет
//! рантайм из полей run (`task`, `user_inputs`, D47), а не из истории — вторая и
//! следующие компакции их не теряют.

use serde_json::{Value, json};
use za_core::{EffectError, EffectSpec, Journal, RunCtx};
use za_llm::{
    ChatRequest, ErrorClass, Message, ModelCall, ModelError, ModelPort, Part, PromptBlocks,
    ReasoningMode, RequestMeta, Role, ToolChoice, render_tools,
};
use za_protocol::{BlobHash, EffectClass};
use za_store::{ProjectStore, StoreError};

use super::{ContextBudget, TEMPLATE_TOKENS, UserInput, message_text};
use crate::conversation::{Conversation, HistoryError, cut_points, split_tail};
use crate::memory::{MemoryPort, memory_budget};
use crate::prompt::{ControlAction, ControlFrom, approx_tokens, escape_untrusted, render_control};
use crate::request::{FrozenPrefix, RunSetup, TurnOptions, build_request};
use crate::turn::{CallKind, RequestRefs, Signals, TurnError, model_call};

/// Рубрика (D46), текст note управляющего сообщения action="compact":
pub const COMPACT_RUBRIC: &str = "Write a summary of the work so far for a fresh context. Use exactly these sections:\n## Decisions\n## Files changed\n## Errors\n## Next step\nFacts only, no tool calls. The goal and user messages are kept verbatim by the runtime; do not repeat them.";

/// Имя эффекта хука памяти перед компакцией (D50, `Idempotent`).
pub(crate) const MEMORY_BEFORE_EFFECT: &str = "memory.before";
/// Имя эффекта recovery памяти после компакции (D50, `ReadOnly`).
const MEMORY_AFTER_EFFECT: &str = "memory.after";
/// Заголовок раздела следующего шага в сводке модели (D47).
const NEXT_STEP: &str = "## Next step";
/// Хвост с активного запроса — не больше `n_ctx / 5` (20 %, D48).
const TAIL_DIVISOR: u32 = 5;
/// Начало текста рецитации (CTX-014, D48).
pub(crate) const RECITATION: &str = "Current task: ";
/// Рецитация, когда активный запрос целиком в хвосте (m-B): текст не повторяется.
pub(crate) const RECITE_IN_TAIL: &str = "the latest user message above.";
/// Пометка обрезанной рецитации.
pub(crate) const CLIPPED: &str = " …[truncated]";
/// Раздел `## User messages` без вводов.
pub(crate) const NO_USER_MESSAGES: &str = "none";
/// Вводы пользователя в сводке — не больше `n_ctx / 4` токенов (25 %, Ruling minor).
const USER_MESSAGES_DIVISOR: u32 = 4;
/// Аварийный хвост — не больше `n_ctx / 10` (Ruling I-1: бюджет меньше обычного).
const EMERGENCY_TAIL_DIVISOR: u32 = 10;

/// D47 (Ruling I-3): разделы рантайма — `## Goal` (цель) и `## User messages` (по
/// строке на ввод, у поправки — префикс `[correct] `; вводов нет — `none`), затем
/// часть модели в блоке `<summary from="model">…</summary>`. Всё через
/// `escape_untrusted`: модель не закроет блок (`</summary>` → `&lt;/summary&gt;`), а её
/// строки `## User messages` или `[correct] …` остаются внутри блока модели — снаружи
/// их не спутать с разделами рантайма. Функция идемпотентна для уже экранированного
/// ввода истории.
pub fn summary_block(
    goal: &str,
    user_messages: &[(String, bool /* correct */)],
    model_part: &str,
) -> String {
    let mut s = String::from("## Goal\n");
    s.push_str(&escape_untrusted(goal));
    s.push_str("\n## User messages\n");
    if user_messages.is_empty() {
        s.push_str(NO_USER_MESSAGES);
        s.push('\n');
    }
    for (text, correct) in user_messages {
        if *correct {
            s.push_str("[correct] ");
        }
        s.push_str(&escape_untrusted(text));
        s.push('\n');
    }
    s.push_str("<summary from=\"model\">\n");
    s.push_str(&escape_untrusted(model_part.trim()));
    s.push_str("\n</summary>");
    s
}

/// Строка опущенных старых вводов пользователя в сводке (Ruling minor задачи 3).
fn omitted_users_line(n: usize) -> String {
    format!("[{n} earlier user messages omitted]")
}

/// Пометка опущенных сообщений во входе аварийной компакции (D49).
fn omitted_messages_line(n: usize) -> String {
    format!("[{n} earlier messages omitted]")
}

/// Образцы текстов для модели, собираемых рантаймом, с фиксированными аргументами
/// (PRM-010, M-1 ревью задачи 5): разметка сводки (`## Goal`, `## User messages`,
/// `[correct] `, `<summary from="model">`) и строки опущенного.
pub(crate) fn text_samples() -> Vec<(&'static str, String)> {
    vec![
        ("runtime/compact.omitted_messages", omitted_messages_line(1)),
        ("runtime/compact.omitted_users", omitted_users_line(1)),
        (
            "runtime/compact.summary_block",
            summary_block("g", &[("u".into(), false), ("c".into(), true)], "m"),
        ),
        (
            "runtime/compact.summary_block_empty",
            summary_block("g", &[], "m"),
        ),
    ]
}

/// Текст в пределах `budget` токенов (байты / 4): длиннее — срез по границе символа и
/// пометка `…[truncated]`.
fn clip(text: &str, budget: u32) -> String {
    let max = budget as usize * 4;
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{CLIPPED}", &text[..end])
}

/// Рецитация задачи run (R1-B2 D55: после nudge цикла): управляющее сообщение
/// `reminder` с текстом `Current task: <задача>`, задача — в пределах 25 % окна.
pub(crate) fn recite_task(task: &str, n_ctx: u32) -> String {
    render_control(
        ControlFrom::Runtime,
        ControlAction::Reminder,
        "",
        &format!("{RECITATION}{}", clip(task, n_ctx / USER_MESSAGES_DIVISOR)),
    )
}

/// Вводы для сводки в пределах `budget` токенов (байты / 4): с новых к старым, пока
/// помещаются; выпавшие старые — строка `[N earlier user messages omitted]` в начале
/// списка. Цель в бюджет не входит и всегда остаётся целиком.
fn limit_user_messages(users: Vec<(String, bool)>, budget: u32) -> Vec<(String, bool)> {
    let mut used = 0u64;
    let mut keep = 0;
    for (text, _) in users.iter().rev() {
        let cost = u64::from(approx_tokens(text)) + 1;
        if used + cost > u64::from(budget) {
            break;
        }
        used += cost;
        keep += 1;
    }
    let omitted = users.len() - keep;
    let mut out = Vec::with_capacity(keep + 1);
    if omitted > 0 {
        out.push((omitted_users_line(omitted), false));
    }
    out.extend(users.into_iter().skip(omitted));
    out
}

/// Раздел `## Next step` части модели (D47, D50): строки после заголовка до
/// следующего заголовка `## `; `None` — раздела нет.
pub(crate) fn next_step_of(model_part: &str) -> Option<String> {
    let mut lines = model_part.lines();
    lines.find(|l| l.trim() == NEXT_STEP)?;
    let body: Vec<&str> = lines.take_while(|l| !l.starts_with("## ")).collect();
    Some(body.join("\n").trim().to_owned())
}

/// Управляющее сообщение с рубрикой — последнее сообщение запроса компакции (D46).
pub(crate) fn rubric_message() -> String {
    render_control(
        ControlFrom::Runtime,
        ControlAction::Compact,
        "",
        COMPACT_RUBRIC,
    )
}

/// Оценка сообщения с рубрикой, токены (байты / 4 и шаблон, D41).
pub(crate) fn rubric_tokens() -> u32 {
    approx_tokens(&rubric_message()).saturating_add(TEMPLATE_TOKENS as u32)
}

/// Оценка запроса без токенизатора (D41: байты / 4 вверх, 4 токена шаблона на
/// сообщение) — как `ContextMeter::estimate` без базы и без счётчика модели. Чистая
/// функция запроса: на ней стоят решения, которые реплей обязан повторить (D49).
fn approx_request_tokens(req: &ChatRequest) -> u32 {
    let tools = render_tools(&PromptBlocks {
        tools: req.tools.clone(),
        ..PromptBlocks::default()
    });
    let head = u64::from(approx_tokens(&req.system)) + u64::from(approx_tokens(&tools));
    let total = req
        .messages
        .iter()
        .fold(head, |acc, m| acc.saturating_add(message_cost(m)));
    u32::try_from(total).unwrap_or(u32::MAX)
}

/// Оценка одного сообщения: байты / 4 его текста и 4 токена шаблона (D41).
fn message_cost(m: &Message) -> u64 {
    u64::from(approx_tokens(&message_text(m))).saturating_add(TEMPLATE_TOKENS)
}

/// Вход компакции.
pub struct CompactInput<'a> {
    pub frozen: &'a FrozenPrefix,
    /// Блоб `FrozenPrefix` (для `model.request`, D8).
    pub frozen_ref: &'a BlobHash,
    pub setup: &'a RunSetup,
    pub conv: &'a Conversation,
    /// Блок `<env>` первого сообщения run.
    pub env: &'a str,
    /// Задача run (`task`, D47).
    pub goal: &'a str,
    pub user_inputs: &'a [UserInput],
    /// `## Next step` прошлой сводки или пустая строка (D50).
    pub last_next_step: &'a str,
    /// Аварийный вход (D49) сразу; иначе — только после `ContextOverflow` запроса.
    pub emergency: bool,
}

/// Итог компакции: история новой эпохи и её данные.
#[derive(Debug, Clone, PartialEq)]
pub struct Compacted {
    pub conv: Conversation,
    /// Сводка первого сообщения: разделы рантайма (`## Goal`, `## User messages`) и блок
    /// модели `<summary from="model">` (D47).
    pub summary: String,
    /// Раздел `## Next step` сводки (пусто — раздела нет).
    pub next_step: String,
    /// Вводы пользователя с пересчитанным `at`: в хвосте — новый индекс, прочие — `None`.
    pub user_inputs: Vec<UserInput>,
    /// Оценки запроса хода до и после (байты / 4, D41), токены.
    pub before: u32,
    pub after: u32,
    /// Сводку дал аварийный вход (D49).
    pub emergency: bool,
    /// Сообщений, опущенных во входе аварийной компакции (`[N earlier messages omitted]`).
    pub omitted: usize,
    /// Сообщений прежней истории в хвосте новой эпохи.
    pub tail: usize,
}

/// D46–D50: эффекты memory.before, compact, memory.after; новая история новой эпохи.
///
/// Запрос `compact` — тот же префикс и та же история плюс рубрика; `ContextOverflow`
/// ответа — один повтор с аварийным входом (D49). Сводка без `## Next step` — один
/// повтор, затем принимается как есть с `warn` (D47). Вход аварийной компакции, не
/// помещающийся в окно и после разреза, — `TurnError::Model` класса `ContextOverflow`
/// без вызова модели. Ошибки модели и обрыв сигналом — как у хода ([`TurnError`]).
pub async fn compact(
    ctx: &mut RunCtx,
    j: &Journal,
    model: &dyn ModelPort,
    mem: &dyn MemoryPort,
    input: CompactInput<'_>,
    turn: u32,
    sig: &mut Signals,
) -> Result<Compacted, TurnError> {
    let store = j.store().clone();
    let run = ctx.run_id();
    let n_ctx = input.setup.n_ctx;
    let budget = ContextBudget::for_context(n_ctx);
    // Блоб адресован содержимым: чтение текста ввода — не внешнее значение.
    let mut users = Vec::with_capacity(input.user_inputs.len());
    for u in input.user_inputs {
        let text = blob_text(&store, &u.text)
            .await
            .map_err(|e| store_err(ctx, e))?;
        users.push((text, u.correct));
    }

    // (1) Хук памяти перед компакцией (D50): ключи не зависят от уровня памяти.
    let next = input.last_next_step.to_owned();
    ctx.effect(
        EffectSpec {
            class: EffectClass::Idempotent,
            name: MEMORY_BEFORE_EFFECT.into(),
            args: json!({ "next_step": next }),
        },
        move || async move {
            mem.before_compaction(run, &next).await;
            Ok(json!({}))
        },
    )
    .await?;

    // (2) Запрос сводки.
    let mut emergency = input.emergency;
    let (mut req_conv, mut omitted) = if emergency {
        emergency_input(&input, &budget)?
    } else {
        normal_input(&input)?
    };
    let mut retried = false;
    let model_part = loop {
        let call = compaction_call(&input, run, &req_conv, &budget);
        let refs = RequestRefs {
            frozen: input.frozen_ref.clone(),
            messages: req_conv
                .clone()
                .persist(&store)
                .await
                .map_err(|e| store_err(ctx, e))?
                .refs,
            prefix: input.frozen.prefix_hash.clone(),
            prompt_version: input.frozen.prompt_version.clone(),
        };
        match model_call(
            ctx,
            j,
            model,
            call,
            &refs,
            turn,
            sig,
            CallKind::Compact,
            None,
        )
        .await
        {
            Ok(out) => {
                let text = text_of(&out.message);
                if next_step_of(&text).is_some() {
                    break text;
                }
                if retried {
                    tracing::warn!(run = %run, "сводка компакции без раздела Next step принята как есть");
                    break text;
                }
                tracing::warn!(run = %run, "сводка компакции без раздела Next step: повтор");
                retried = true;
            }
            Err(TurnError::Model { error, .. })
                if error.class == ErrorClass::ContextOverflow && !emergency =>
            {
                tracing::warn!(run = %run, error = %error,
                    "запрос компакции не поместился в окно: аварийный вход");
                emergency = true;
                (req_conv, omitted) = emergency_input(&input, &budget)?;
            }
            Err(e) => return Err(e),
        }
    };

    // (3) Recovery памяти (D50): при L0 эффект пишется, текста нет.
    let budget_tokens = memory_budget(n_ctx);
    let v = ctx
        .effect(
            EffectSpec {
                class: EffectClass::ReadOnly,
                name: MEMORY_AFTER_EFFECT.into(),
                args: json!({ "budget_tokens": budget_tokens }),
            },
            move || async move {
                let recall = mem.after_compaction(run, budget_tokens).await;
                Ok(json!({ "text": recall.text() }))
            },
        )
        .await?;
    let recovery = v.get("text").and_then(Value::as_str);

    // Новая эпоха: первое сообщение, хвост, рецитация (D46, D48).
    let shown = limit_user_messages(users.clone(), n_ctx / USER_MESSAGES_DIVISOR);
    let summary = summary_block(input.goal, &shown, &model_part);
    let next_step = next_step_of(&model_part).unwrap_or_default();
    let request_at = input.user_inputs.last().and_then(|u| u.at);
    let split = if emergency {
        emergency_split(input.conv, request_at, n_ctx / EMERGENCY_TAIL_DIVISOR)
    } else {
        split_tail(input.conv, request_at, n_ctx / TAIL_DIVISOR)
    };
    // Рецитация (m-B): ввод, целиком попавший в хвост, не повторяется; иначе — ввод
    // (или задача) в пределах 25 % окна.
    let in_tail = request_at.is_some_and(|at| at >= split);
    let current = if in_tail {
        RECITE_IN_TAIL.to_owned()
    } else {
        let text = users.last().map_or(input.goal, |(t, _)| t.as_str());
        clip(text, n_ctx / USER_MESSAGES_DIVISOR)
    };
    let recitation = render_control(
        ControlFrom::Runtime,
        ControlAction::Reminder,
        "",
        &format!("{RECITATION}{current}"),
    );
    let conv = input
        .conv
        .rebased(
            user_message(new_first(input.env, recovery, &summary)),
            split,
            vec![user_message(recitation)],
        )
        .map_err(history_err)?;
    let user_inputs = input
        .user_inputs
        .iter()
        .map(|u| UserInput {
            at: u.at.filter(|&i| i >= split).map(|i| 1 + i - split),
            ..u.clone()
        })
        .collect();
    let opts = TurnOptions::default();
    let before =
        approx_request_tokens(&build_request(input.frozen, input.setup, input.conv, &opts));
    let after = approx_request_tokens(&build_request(input.frozen, input.setup, &conv, &opts));
    Ok(Compacted {
        conv,
        summary,
        next_step,
        user_inputs,
        before,
        after,
        emergency,
        omitted,
        tail: input.conv.messages().len().saturating_sub(split),
    })
}

/// Вход обычной компакции: вся история и рубрика (D46).
fn normal_input(input: &CompactInput<'_>) -> Result<(Conversation, usize), TurnError> {
    let mut c = input.conv.clone();
    c.push_user(rubric_message()).map_err(history_err)?;
    Ok((c, 0))
}

/// Хвост аварийной компакции (Ruling I-1): как [`split_tail`] с бюджетом `max_tokens`,
/// и если суффикс больше бюджета — первая точка разреза дальше, с которой он
/// помещается; не помещается ни с одной — минимальный хвост с последней точки
/// разреза. Без точек разреза — как [`split_tail`].
fn emergency_split(conv: &Conversation, request_at: Option<usize>, max_tokens: u32) -> usize {
    let msgs = conv.messages();
    let split = split_tail(conv, request_at, max_tokens);
    let suffix =
        |from: usize| -> u64 { msgs[from.min(msgs.len())..].iter().map(message_cost).sum() };
    if suffix(split) <= u64::from(max_tokens) {
        return split;
    }
    let points: Vec<usize> = cut_points(conv)
        .into_iter()
        .filter(|&p| p > split)
        .collect();
    points
        .iter()
        .copied()
        .find(|&p| suffix(p) <= u64::from(max_tokens))
        .or_else(|| points.last().copied())
        .unwrap_or(split)
}

/// Вход аварийной компакции (D49): первое сообщение (с `[N earlier messages omitted]`
/// при `N > 0`) и наибольший суффикс с точки разреза, помещающийся с рубрикой в
/// `n_ctx − output_reserve − compaction_reserve`; суффикс может быть пустым. Не
/// помещается и так — `ContextOverflow`.
fn emergency_input(
    input: &CompactInput<'_>,
    budget: &ContextBudget,
) -> Result<(Conversation, usize), TurnError> {
    let limit = u64::from(
        budget
            .n_ctx
            .saturating_sub(budget.output_reserve)
            .saturating_sub(budget.compaction_reserve),
    );
    let msgs = input.conv.messages();
    let Some(first) = msgs.first() else {
        return Err(overflow("история пуста"));
    };
    let tools = render_tools(&PromptBlocks {
        tools: input.frozen.tools.clone(),
        ..PromptBlocks::default()
    });
    let head = u64::from(approx_tokens(&input.frozen.system))
        + u64::from(approx_tokens(&tools))
        + u64::from(rubric_tokens());
    // suffix[i] — оценка сообщений с i до конца.
    let mut suffix = vec![0u64; msgs.len() + 1];
    for i in (0..msgs.len()).rev() {
        suffix[i] = suffix[i + 1].saturating_add(message_cost(&msgs[i]));
    }
    let mut starts = cut_points(input.conv);
    starts.push(msgs.len());
    for start in starts {
        let omitted = start - 1;
        let head_msg = with_omitted(first, omitted);
        let total = head
            .saturating_add(message_cost(&head_msg))
            .saturating_add(suffix[start]);
        if total <= limit {
            let c = input
                .conv
                .rebased(head_msg, start, vec![user_message(rubric_message())])
                .map_err(history_err)?;
            tracing::info!(omitted, total, limit, "вход аварийной компакции");
            return Ok((c, omitted));
        }
    }
    Err(overflow("вход аварийной компакции не помещается в окно"))
}

/// Первое сообщение с пометкой об опущенных сообщениях (D49): строка дописывается к
/// тексту, а не отдельным сообщением — история остаётся валидной.
fn with_omitted(first: &Message, omitted: usize) -> Message {
    let mut m = first.clone();
    if omitted == 0 {
        return m;
    }
    let note = format!("\n\n{}", omitted_messages_line(omitted));
    match m.parts.iter_mut().rev().find_map(|p| match p {
        Part::Text(t) => Some(t),
        _ => None,
    }) {
        Some(t) => t.push_str(&note),
        None => m.parts.push(Part::Text(note.trim_start().to_owned())),
    }
    m
}

/// Вызов модели компакции: тот же префикс, `tool_choice = None` (CTX-011), без
/// мышления, вывод не больше `compaction_reserve` (D46).
fn compaction_call(
    input: &CompactInput<'_>,
    run: za_protocol::RunId,
    conv: &Conversation,
    budget: &ContextBudget,
) -> ModelCall {
    let opts = TurnOptions {
        tool_choice: ToolChoice::None,
        max_output_tokens: Some(budget.compaction_reserve),
        reasoning: Some(ReasoningMode::Off),
        ..TurnOptions::default()
    };
    ModelCall {
        endpoint: input.setup.endpoint.clone(),
        meta: RequestMeta {
            run_id: run,
            priority: input.setup.priority,
            session: Some(run.to_string()),
            max_wait: input.setup.max_wait,
        },
        request: build_request(input.frozen, input.setup, conv, &opts),
    }
}

/// Первое сообщение новой эпохи (D46): `<env>` run, recovery памяти (как снимок в
/// первом сообщении run, экранирован), сводка (D47: разделы рантайма и блок модели).
fn new_first(env: &str, recovery: Option<&str>, summary: &str) -> String {
    let mut out = env.to_owned();
    if let Some(r) = recovery.filter(|r| !r.trim().is_empty()) {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("<memory>\n");
        out.push_str(&escape_untrusted(r));
        out.push_str("\n</memory>");
    }
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(summary);
    out
}

fn user_message(text: String) -> Message {
    Message {
        role: Role::User,
        parts: vec![Part::Text(text)],
    }
}

/// Текст ответа: части `Text` подряд.
fn text_of(m: &Message) -> String {
    m.parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

async fn blob_text(store: &ProjectStore, h: &BlobHash) -> Result<String, StoreError> {
    let raw = store
        .get_blob(h)
        .await?
        .ok_or_else(|| StoreError::Corrupt(format!("блоб ввода пользователя {} не найден", h.0)))?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// Ошибка журнала внутри компакции: контекст отравлен, как у эффекта (D11).
fn store_err(ctx: &mut RunCtx, e: StoreError) -> TurnError {
    let err = EffectError::Store(e);
    ctx.observe::<()>(&Err(err.clone()));
    TurnError::Effect(err)
}

/// Нарушенный инвариант истории новой эпохи — ошибка кода, а не модели.
fn history_err(e: HistoryError) -> TurnError {
    TurnError::Effect(EffectError::Failed(format!("история компакции: {e}")))
}

fn overflow(msg: &str) -> TurnError {
    TurnError::Model {
        error: ModelError::new(ErrorClass::ContextOverflow, msg),
        last_wait: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_step_section() {
        assert_eq!(
            next_step_of("## Decisions\nd\n## Next step\nrun tests\nthen fix\n## Other\nx")
                .as_deref(),
            Some("run tests\nthen fix")
        );
        assert_eq!(next_step_of("## Decisions\nd"), None);
        assert_eq!(next_step_of("## Next step").as_deref(), Some(""));
    }

    #[test]
    fn omitted_note_is_appended() {
        let m = user_message("<env>\n</env>\n\nt".into());
        assert_eq!(with_omitted(&m, 0), m);
        let m3 = with_omitted(&m, 3);
        assert_eq!(
            text_of(&m3),
            "<env>\n</env>\n\nt\n\n[3 earlier messages omitted]"
        );
    }

    #[test]
    fn first_message_layout() {
        assert_eq!(
            new_first("<env>e</env>", None, "<summary>s</summary>"),
            "<env>e</env>\n\n<summary>s</summary>"
        );
        assert_eq!(
            new_first("<env>e</env>", Some("r<x>"), "S"),
            "<env>e</env>\n\n<memory>\nr&lt;x&gt;\n</memory>\n\nS"
        );
        assert_eq!(new_first("", Some(" "), "S"), "S");
    }
}
