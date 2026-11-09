//! Сборка запроса к модели по порядку блоков раздела 2.1 части 02.
//!
//! Блоки 1–5 (описания инструментов, Core с инструкциями capability, адаптер семейства,
//! overlay режима, `AGENTS.md`) замораживаются на run в [`FrozenPrefix`] (CTX-003,
//! CTX-005): префикс запроса байт в байт одинаков на всех ходах, поэтому кэш KV
//! llama.cpp и prompt caching облака переиспользуются. Окружение, память и задача —
//! только первое сообщение (блок 6, D24), история — блоки 6–8 как есть.

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
    render_env, role_core, role_limit,
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

/// Блок 2: Core роли и инструкции capability после него (D21) через пустую строку.
///
/// На тире small инструкции capability не входят в system: Core top занимает почти весь
/// лимит 800 (приложение A), на small они приходят в описаниях инструментов (решение
/// контроллера, R1-C).
fn core_block(setup: &RunSetup, caps: &[Capability]) -> String {
    let mut core = role_core(setup.role).to_string();
    let instructions = caps
        .iter()
        .filter_map(|c| c.instructions.as_deref().map(|i| (c.name.as_str(), i)))
        .filter(|(_, i)| !i.trim().is_empty());
    for (name, text) in instructions {
        if setup.tier == Tier::Small {
            tracing::debug!(
                capability = name,
                "инструкции capability на small не входят в system"
            );
            continue;
        }
        core.push_str("\n\n");
        core.push_str(text);
    }
    core
}

/// Замораживает префикс run.
///
/// Core роли + инструкции capability (блок 2), адаптер `""` (PRM-009), overlay режима
/// (блок 4), `AGENTS.md` (блок 5) после [`escape_untrusted`] и обрезки по D22; tools
/// отсортированы по имени.
///
/// Лимит `AGENTS.md` — остаток [`role_limit`] после блоков 2–4 с разделителями (оценка
/// [`approx_tokens`] фиксированных текстов) и, у top standard и full, резерва
/// [`ENV_LIMIT`] (решение контроллера: лимит по блокам 2–5 вместе с `<env>` и overlay);
/// сам `AGENTS.md` считается верхней оценкой — байтами ([`fit_agents_md`]). Остатка не
/// хватает даже на строку обрезки — блок опускается с `warn`. Worker и orchestrator
/// `AGENTS.md` не получают (N-07).
pub fn freeze(
    setup: &RunSetup,
    tools: &[ToolSchema],
    caps: &[Capability],
    agents_md: Option<&str>,
) -> FrozenPrefix {
    let mut tools = tools.to_vec();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let mut blocks = PromptBlocks {
        tools,
        core: core_block(setup, caps),
        adapter: String::new(),
        overlay: setup
            .work_mode
            .map(mode_overlay)
            .unwrap_or_default()
            .to_string(),
        agents_md: String::new(),
    };
    // Блоки 2–4 с разделителями и разделитель "\n\n" перед блоком 5 (+1 токен).
    let fixed = approx_tokens(&render_system(&blocks)).saturating_add(1);
    let reserve = if env_in_role_limit(setup.role, setup.tier) {
        ENV_LIMIT
    } else {
        0
    };
    if fixed.saturating_add(reserve) > role_limit(setup.role, setup.tier) {
        tracing::warn!(
            fixed,
            limit = role_limit(setup.role, setup.tier),
            "фиксированные блоки промпта превышают лимит роли"
        );
    }
    if let Some(text) = agents_md {
        let budget = agents_md_budget(setup.role, setup.tier, fixed);
        if setup.role != RunRole::Top {
            tracing::debug!("AGENTS.md не передаётся субагенту (N-07)");
        } else if let Some(fitted) = fit_agents_md(text, budget) {
            blocks.agents_md = fitted;
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
    FrozenPrefix {
        system,
        prefix_hash: prefix_hash(&blocks),
        tools: blocks.tools,
        blocks: block_hashes,
        prompt_version: PROMPT_VERSION.to_string(),
        texts_hash: prompt_texts_hash(),
    }
}

/// Блок 6 (D24): `<env>`, затем `<memory>…</memory>` при непустом снимке, затем задача.
///
/// Снимок памяти — данные (MEM-006), экранируется [`escape_untrusted`]. `task` —
/// текст задачи top или готовый блок `<task>` субагента (уже экранирован
/// `render_task`) и вставляется как есть.
pub fn first_message(env: &EnvInfo, memory: Option<&str>, task: &str) -> String {
    let mut out = render_env(env);
    if let Some(m) = memory.filter(|m| !m.is_empty()) {
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

/// BLAKE3 hex канонического JSON запроса (D7).
pub fn request_hash(req: &ChatRequest) -> String {
    let canon = match serde_json::to_value(req) {
        Ok(v) => canonical_json(&v),
        // Сериализация канонических типов не падает; на всякий случай — отладочный вид,
        // разные запросы всё равно дают разные хэши.
        Err(e) => {
            tracing::error!(error = %e, "запрос не сериализован в JSON");
            format!("{req:?}")
        }
    };
    hex(&canon)
}
