//! Сборка запроса к модели по порядку блоков раздела 2.1 части 02.
//!
//! Блоки 1–5 (описания инструментов, Core с инструкциями capability, адаптер семейства,
//! overlay режима, `AGENTS.md`) замораживаются на run в [`FrozenPrefix`] (CTX-003,
//! CTX-005): префикс запроса (tools и system) байт в байт одинаков на всех ходах, а
//! история только дополняется. Поэтому кэш KV llama.cpp переиспользует весь прежний
//! диалог; у облачных провайдеров кэшируется то, что отмечает коннектор (у Anthropic
//! сейчас — system и tools; точки кэша на истории нет, а короткий префикс может не
//! дотянуть до минимальной кэшируемой длины). Окружение, память и задача — только первое
//! сообщение (блок 6, D24), история — блоки 6–8 как есть.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use za_core::Capability;
use za_llm::{
    ChatRequest, EndpointId, PromptBlocks, ReasoningMode, Sampling, ToolChoice, ToolSchema,
    prefix_hash, render_system, render_tools,
};
use za_protocol::{Priority, RunMode, RunRole, Tier, canonical_json};

use crate::conversation::Conversation;
use crate::prompt::{
    ENV_LIMIT, EnvInfo, PROMPT_VERSION, WorkMode, agents_md_budget, approx_tokens,
    env_in_role_limit, escape_untrusted, fit_agents_md, mode_overlay, prompt_texts_hash,
    render_env, role_core, role_limit, upper_tokens,
};

/// Параметры run, от которых зависит запрос. `tier` и `n_ctx` вычисляет `start_run`
/// (D37); `tier` замораживается вместе с префиксом (D6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunSetup {
    pub role: RunRole,
    pub tier: Tier,
    pub work_mode: Option<WorkMode>,
    pub run_mode: RunMode,
    pub endpoint: EndpointId,
    pub n_ctx: u32,
    pub reasoning: ReasoningMode,
    pub priority: Priority,
    pub max_wait: Option<Duration>,
    pub project_id: String,
    pub project_dir: PathBuf,
}

/// BLAKE3 hex каждого блока префикса: `tools` — от [`render_tools`], остальные — от
/// текста блока (OPS-003: какой блок изменился).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlockHashes {
    pub tools: String,
    pub core: String,
    pub adapter: String,
    pub overlay: String,
    pub agents_md: String,
}

/// Неизменяемая на run часть (блоки 1–5, CTX-003, CTX-005).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrozenPrefix {
    pub system: String,
    /// Описания инструментов, отсортированные по имени.
    pub tools: Vec<ToolSchema>,
    pub prefix_hash: String,
    pub blocks: BlockHashes,
    pub prompt_version: String,
    pub texts_hash: String,
}

fn hex(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex().to_string()
}

/// Потолок инструкций capability, токены верхней оценки (байты): «правила инструментов
/// ≤ 600, правила памяти ≤ 200» распределения Full (часть 02, §4); на младших тирах —
/// тот же потолок, остаток лимита роли обычно меньше.
pub const CAPABILITY_LIMIT: u32 = 800;

/// Разделитель блоков и инструкций в system (`render_system`), байты.
const SEP: u32 = 2;

/// Ошибка сборки префикса.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PrefixError {
    /// Два инструмента с одним именем: порядок в префиксе был бы неоднозначен, а
    /// провайдер отверг бы запрос.
    #[error("инструмент `{0}` передан дважды")]
    DuplicateTool(String),
}

/// Входит ли overlay режима в [`role_limit`]: у top small нет — Core входит в 800 по
/// приложению A, overlay — собственный слот 500 (PRM-008, «500 на слот»); у top
/// standard и full — да (часть 02, §4: блоки 2–5 вместе с `<env>`), у субагентов — да.
fn overlay_in_role_limit(role: RunRole, tier: Tier) -> bool {
    !(role == RunRole::Top && tier == Tier::Small)
}

/// Инструкции capability (блок 2 после Core, D21) в пределах `budget` байт.
///
/// Инструкции — переменный текст: считаются верхней оценкой (байты, `upper_tokens`) с
/// разделителем. Берутся по порядку `caps` целиком; не помещающаяся отбрасывается с
/// `warn`, следующие ещё пробуются, — результат детерминирован. У top small инструкции
/// не входят в system вовсе: на small они придут в описаниях инструментов (решение
/// контроллера, задача 7; перенос — R1-C). Возвращает текст блока 2 и занятые байты.
fn core_block(setup: &RunSetup, caps: &[Capability], budget: u32) -> (String, u32) {
    let mut core = role_core(setup.role).to_string();
    let mut used = 0u32;
    let instructions = caps
        .iter()
        .filter_map(|c| c.instructions.as_deref().map(|i| (c.name.as_str(), i)))
        .filter(|(_, i)| !i.trim().is_empty());
    for (name, text) in instructions {
        if setup.role == RunRole::Top && setup.tier == Tier::Small {
            tracing::warn!(
                capability = name,
                "инструкции capability на top small не входят в system (R1-C: в описаниях инструментов)"
            );
            continue;
        }
        let cost = SEP.saturating_add(upper_tokens(text));
        if used.saturating_add(cost) > budget {
            tracing::warn!(
                capability = name,
                bytes = text.len(),
                budget,
                used,
                "инструкции capability не помещаются в лимит промпта роли и опущены"
            );
            continue;
        }
        used += cost;
        core.push_str("\n\n");
        core.push_str(text);
    }
    (core, used)
}

/// Замораживает префикс run.
///
/// Core роли + инструкции capability (блок 2), адаптер `""` (PRM-009), overlay режима
/// (блок 4), `AGENTS.md` (блок 5) после [`escape_untrusted`] и обрезки по D22; tools
/// отсортированы по имени, повтор имени — [`PrefixError::DuplicateTool`].
///
/// Лимит роли ([`role_limit`], решение контроллера: блоки 2–5 вместе с резервом
/// [`ENV_LIMIT`] у top standard и full и с overlay, кроме top small) делится по
/// приоритету части 02, §4: сначала фиксированные тексты приложения A (Core, overlay —
/// оценка [`approx_tokens`], точный счёт — CI, PRM-007), затем инструкции capability (не
/// больше [`CAPABILITY_LIMIT`]), затем `AGENTS.md` ([`agents_md_budget`]). Переменный
/// текст (инструкции, `AGENTS.md`) считается верхней оценкой — байтами, поэтому итог
/// при любом вводе не выходит за лимит, если в него укладываются фиксированные тексты.
/// Worker и orchestrator `AGENTS.md` не получают (N-07).
pub fn freeze(
    setup: &RunSetup,
    tools: &[ToolSchema],
    caps: &[Capability],
    agents_md: Option<&str>,
) -> Result<FrozenPrefix, PrefixError> {
    let mut tools = tools.to_vec();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    if let Some(w) = tools.windows(2).find(|w| w[0].name == w[1].name) {
        return Err(PrefixError::DuplicateTool(w[0].name.clone()));
    }
    let (role, tier) = (setup.role, setup.tier);
    let limit = role_limit(role, tier);
    let overlay = setup
        .work_mode
        .map(mode_overlay)
        .unwrap_or_default()
        .to_string();
    // Фиксированные блоки, входящие в лимит, с разделителями между ними.
    let fixed = approx_tokens(&render_system(&PromptBlocks {
        core: role_core(role).to_string(),
        overlay: if overlay_in_role_limit(role, tier) {
            overlay.clone()
        } else {
            String::new()
        },
        ..PromptBlocks::default()
    }));
    let reserve = if env_in_role_limit(role, tier) {
        ENV_LIMIT
    } else {
        0
    };
    if fixed.saturating_add(reserve) > limit {
        // Фиксированные тексты проверяет CI-счётчик (PRM-007); оценка байты / 4 их
        // завышает, поэтому здесь только `debug`.
        tracing::debug!(fixed, limit, "оценка фиксированных блоков выше лимита роли");
    }
    let caps_budget = limit
        .saturating_sub(fixed)
        .saturating_sub(reserve)
        .min(CAPABILITY_LIMIT);
    let (core, caps_used) = core_block(setup, caps, caps_budget);
    let mut blocks = PromptBlocks {
        tools,
        core,
        adapter: String::new(),
        overlay,
        agents_md: String::new(),
    };
    if let Some(text) = agents_md {
        if role != RunRole::Top {
            tracing::debug!("AGENTS.md не передаётся субагенту (N-07)");
        } else {
            // Разделитель перед блоком 5 — тоже в счёт.
            let taken = fixed.saturating_add(caps_used).saturating_add(SEP);
            let budget = agents_md_budget(role, tier, taken);
            if let Some(fitted) = fit_agents_md(text, budget) {
                blocks.agents_md = fitted;
            }
        }
    }
    let system = render_system(&blocks);
    let block_hashes = BlockHashes {
        tools: hex(&render_tools(&blocks)),
        core: hex(&blocks.core),
        adapter: hex(&blocks.adapter),
        overlay: hex(&blocks.overlay),
        agents_md: hex(&blocks.agents_md),
    };
    Ok(FrozenPrefix {
        system,
        prefix_hash: prefix_hash(&blocks),
        tools: blocks.tools,
        blocks: block_hashes,
        prompt_version: PROMPT_VERSION.to_string(),
        texts_hash: prompt_texts_hash(),
    })
}

/// Блок 6 (D24): `<env>`, затем `<memory>…</memory>` при непустом снимке (не из одних
/// пробельных символов), затем задача.
///
/// Снимок памяти — данные (MEM-006), экранируется [`escape_untrusted`]. `task` —
/// текст задачи top или готовый блок `<task>` субагента (уже экранирован
/// `render_task`) и вставляется как есть.
pub fn first_message(env: &EnvInfo, memory: Option<&str>, task: &str) -> String {
    let mut out = render_env(env);
    if let Some(m) = memory.filter(|m| !m.trim().is_empty()) {
        out.push_str("\n\n<memory>\n");
        out.push_str(&escape_untrusted(m));
        out.push_str("\n</memory>");
    }
    out.push_str("\n\n");
    out.push_str(task);
    out
}

/// Параметры одного хода.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnOptions {
    pub tool_choice: ToolChoice,
    pub sampling: Sampling,
    pub max_output_tokens: Option<u32>,
    /// `None` — `setup.reasoning`.
    pub reasoning: Option<ReasoningMode>,
}

/// Запрос хода: префикс из `frozen`, история как есть (блоки 6–8),
/// `cache_key = Some(project_id)` (MDL-022).
pub fn build_request(
    frozen: &FrozenPrefix,
    setup: &RunSetup,
    conv: &Conversation,
    opts: &TurnOptions,
) -> ChatRequest {
    ChatRequest {
        system: frozen.system.clone(),
        tools: frozen.tools.clone(),
        messages: conv.messages().to_vec(),
        tool_choice: opts.tool_choice.clone(),
        max_output_tokens: opts.max_output_tokens,
        sampling: opts.sampling.clone(),
        reasoning: opts
            .reasoning
            .clone()
            .unwrap_or_else(|| setup.reasoning.clone()),
        cache_key: Some(setup.project_id.clone()),
    }
}

/// Метка формата хэша запроса; смена формата не совпадёт со старыми квитанциями.
const REQUEST_FORMAT_TAG: &[u8] = b"za-request-v1\0";

/// BLAKE3 hex запроса (D7): метка `za-request-v1\0`, затем канонический JSON
/// `ChatRequest` (`canonical_json`, как у блобов истории и `render_tools`).
///
/// Хэш входит в аргументы эффекта модели и сверяется при реплее, поэтому зависит
/// только от канонического JSON, а не от отладочного вида. Все поля `ChatRequest`
/// сериализуемы (строковые ключи, без своих `Serialize` с ошибкой), так что ошибка
/// `serde_json` на практике не возникает; если возникнет — она возвращается, а не
/// подменяется другим представлением.
pub fn request_hash(req: &ChatRequest) -> Result<String, serde_json::Error> {
    let canon = canonical_json(&serde_json::to_value(req)?);
    let mut h = blake3::Hasher::new();
    h.update(REQUEST_FORMAT_TAG);
    h.update(canon.as_bytes());
    Ok(h.finalize().to_hex().to_string())
}
