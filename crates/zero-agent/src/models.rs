//! Команды `models`, `models probe` и `ask`; запуск `Gateway` для команд и `doctor`.
//!
//! Модель вызывается только через [`Gateway`] (RES-001). `Gateway::start` не ждёт зондов,
//! поэтому команды, которым нужно состояние эндпоинта, ждут их не дольше
//! [`PROBE_WAIT`] и печатают состояние с причиной: `probing` (зонд не уложился),
//! `inactive` (нет ключа, ключ отвергнут, нет модели), `unreachable`, `open`, `disabled`.
//!
//! Коды выхода: `0` — успех; `1` — ошибка, в том числе ошибка модели, неготовая модель
//! (`ask`, `models probe`) и ошибка конфигурации. Список `models` — успех при любом
//! состоянии эндпоинтов. `ask` журнал не пишет: это проверочная команда. Вывод `ask`:
//! на терминале — текст по мере стрима, при повторе (D20) — пометка в stderr и ответ
//! заново с новой строки; вне терминала — только итог последней попытки.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use za_llm::config::{ConfigError, ModelsConfig};
use za_llm::profile::{EndpointState, ProbedEndpoint, ProfileStore};
use za_llm::{
    ChatRequest, DEFAULT_PROBE_DEADLINE, EndpointId, EndpointView, EnvSecrets, Gateway,
    GatewayOptions, Message, ModelCall, ModelPort, Part, RequestMeta, Role, SelectError, Selection,
    StreamEvent, WaitState,
};
use za_protocol::{Priority, RunId};
use za_res::{Admission, MonitorConfig, ResourceMonitor};

use crate::config::{AgentConfig, AppConfigError, load_config};

/// Сколько команды ждут первых зондов: предел одного зонда и запас.
pub const PROBE_WAIT: Duration = DEFAULT_PROBE_DEADLINE.saturating_add(Duration::from_secs(5));
/// Каталог профилей моделей и фактов эндпоинтов в `ZA_HOME` (D13).
pub const MODELS_DIR: &str = "models";
/// Период опроса состояния эндпоинта, пока идёт зонд.
const POLL: Duration = Duration::from_millis(50);

/// Ошибка команды. Текст без ключей: ошибки конфигурации — по белому списку
/// (`ConfigParseError`, `ConfigError`), ошибки модели — после `redact` в `za-llm`.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error(transparent)]
    Config(#[from] AppConfigError),
    #[error("{file}: {0}", file = crate::config::CONFIG_FILE)]
    Models(#[from] ConfigError),
    #[error(transparent)]
    Select(#[from] SelectError),
    #[error("{what} {}: {source}", path.display())]
    Io {
        what: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "модели не настроены: раздел [models] в {} или переменная LLAMA_URL",
        crate::config::CONFIG_FILE
    )]
    NoModels,
    #[error("вывод JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Запущенный `Gateway` и, если нужен допуск по ресурсам, монитор (живёт вместе с ним).
pub struct Started {
    pub gateway: Gateway,
    pub monitor: Option<ResourceMonitor>,
}

/// Запускает `Gateway` по конфигурации. `with_resources` — монитор ресурсов узла и
/// допуск заявок (D10): для `ask` и `resources`. Нет `[models]` — пустой список
/// эндпоинтов. Ключи — из `ZA_SECRET_<KEY_REF>` (D4).
pub async fn start_gateway(
    home: &Path,
    cfg: &AgentConfig,
    with_resources: bool,
) -> Result<Started, CommandError> {
    let dir = home.join(MODELS_DIR);
    let store = ProfileStore::open(&dir).map_err(|source| CommandError::Io {
        what: "каталог профилей моделей",
        path: dir,
        source,
    })?;
    let (monitor, admission) = if with_resources {
        let mc =
            MonitorConfig::from_resources(&cfg.resources, home.to_path_buf(), Vec::new(), None);
        let (monitor, rx) = ResourceMonitor::start(mc);
        let admission = Admission::new(rx, cfg.resources.clone(), home.to_path_buf());
        (Some(monitor), Some(admission))
    } else {
        (None, None)
    };
    let gateway = Gateway::start(
        cfg.models.clone().unwrap_or_default(),
        Arc::new(EnvSecrets),
        admission,
        store,
        GatewayOptions::default(),
    )
    .await?;
    Ok(Started { gateway, monitor })
}

/// Эндпоинт по id из списка `Gateway`.
pub fn view_of(g: &Gateway, id: &EndpointId) -> Option<EndpointView> {
    g.endpoints().into_iter().find(|v| &v.id == id)
}

/// Ждёт конца первого зонда эндпоинта не дольше `limit`; итог — его состояние.
pub async fn wait_endpoint(g: &Gateway, id: &EndpointId, limit: Duration) -> Option<EndpointView> {
    let until = tokio::time::Instant::now() + limit;
    loop {
        let v = view_of(g, id)?;
        if v.state != "probing" || tokio::time::Instant::now() >= until {
            return Some(v);
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Состояние эндпоинта одной строкой: `ready`, `inactive: <причина>`, `probing: …`.
/// `waited` — сколько команда ждала зонда; `None` — не ждала (`resources`), и у
/// `probing` нет срока.
pub fn state_text(v: &EndpointView, waited: Option<Duration>) -> String {
    match (v.state.as_str(), &v.reason) {
        ("probing", _) => match waited {
            Some(w) => format!("probing: зонд не завершён за {} с", w.as_secs()),
            None => "probing: зонд идёт".to_owned(),
        },
        ("open", _) => "open: предохранитель открыт после серии отказов (MDL-016)".to_owned(),
        (s, Some(r)) => format!("{s}: {r}"),
        (s, None) => s.to_owned(),
    }
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "-".to_owned(), |x| x.to_string())
}

fn backend_name(v: &EndpointView) -> String {
    serde_json::to_value(v.backend)
        .ok()
        .and_then(|x| x.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn pool_name(v: &EndpointView) -> String {
    serde_json::to_value(v.pool)
        .ok()
        .and_then(|x| x.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Таблица эндпоинтов для человека; `waited` — как у [`state_text`].
pub fn render_endpoints(
    views: &[EndpointView],
    warnings: &[String],
    waited: Option<Duration>,
) -> String {
    let mut out = String::new();
    if views.is_empty() {
        out.push_str(&format!("{}\n", CommandError::NoModels));
    }
    for v in views {
        out.push_str(&format!(
            "{:<16} {:<17} {:<5} контекст {:<7} слотов {:<3} модель {}\n    {}\n",
            v.id.0,
            backend_name(v),
            pool_name(v),
            opt(v.context_tokens),
            opt(v.slots),
            opt(v.model.as_deref()),
            state_text(v, waited),
        ));
    }
    for w in warnings {
        out.push_str(&format!("предупреждение: {w}\n"));
    }
    out
}

/// Конфигурация с непустым `[models]`.
fn models_of(cfg: &AgentConfig) -> Option<&ModelsConfig> {
    cfg.models.as_ref().filter(|m| !m.endpoints.is_empty())
}

/// `zero-agent models [--json]`: эндпоинты с состоянием после первых зондов.
pub async fn cmd_models(home: &Path, json: bool) -> Result<ExitCode, CommandError> {
    let cfg = load_config(home)?;
    let (views, warnings) = if models_of(&cfg).is_some() {
        let s = start_gateway(home, &cfg, false).await?;
        s.gateway.wait_probed(PROBE_WAIT).await;
        let view = s.gateway.view();
        (view.endpoints, view.warnings)
    } else {
        (Vec::new(), Vec::new())
    };
    if json {
        let v = json!({"endpoints": views, "warnings": warnings});
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print!("{}", render_endpoints(&views, &warnings, Some(PROBE_WAIT)));
    }
    Ok(ExitCode::SUCCESS)
}

/// `zero-agent models probe <id>`: повторный зонд; профиль связки и факты эндпоинта
/// сохраняет `Gateway` в `ZA_HOME/models` (D13). Код 0 — эндпоинт готов.
pub async fn cmd_probe(home: &Path, id: &str, json: bool) -> Result<ExitCode, CommandError> {
    let cfg = load_config(home)?;
    let models = models_of(&cfg).ok_or(CommandError::NoModels)?;
    let id = EndpointId(id.to_owned());
    if models.endpoint(&id).is_none() {
        return Err(SelectError::Unknown(id).into());
    }
    let s = start_gateway(home, &cfg, false).await?;
    let state = s.gateway.probe(&id).await;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&probe_json(&id, &state))?
        );
    } else {
        print!("{}", render_probe(&id, &state));
    }
    Ok(match state {
        EndpointState::Ready(_) => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    })
}

fn probe_json(id: &EndpointId, state: &EndpointState) -> serde_json::Value {
    match state {
        EndpointState::Ready(p) => json!({
            "id": id, "state": "ready", "profile": p.profile, "facts": p.facts,
        }),
        EndpointState::Inactive { reason } => {
            json!({"id": id, "state": "inactive", "reason": reason})
        }
        EndpointState::Unreachable { reason } => {
            json!({"id": id, "state": "unreachable", "reason": reason})
        }
    }
}

fn render_probe(id: &EndpointId, state: &EndpointState) -> String {
    match state {
        EndpointState::Ready(p) => render_profile(id, p),
        EndpointState::Inactive { reason } => format!("{id}: inactive: {reason}\n"),
        EndpointState::Unreachable { reason } => format!("{id}: unreachable: {reason}\n"),
    }
}

fn render_profile(id: &EndpointId, p: &ProbedEndpoint) -> String {
    let b = &p.profile.bundle;
    let f = &p.facts;
    let c = &p.profile.caps;
    let mut out = format!("{id}: ready, связка {}\n", f.bundle_key);
    out.push_str(&format!("  модель            {}\n", b.model));
    out.push_str(&format!(
        "  квант             {}\n",
        opt(b.quant.as_deref())
    ));
    out.push_str(&format!(
        "  рантайм           {} {}\n",
        serde_json::to_value(b.runtime)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        opt(b.runtime_version.as_deref())
    ));
    out.push_str(&format!(
        "  контекст          {} (источник {:?}, рантайм {})\n",
        f.context_tokens,
        f.context_source,
        opt(f.runtime_context)
    ));
    out.push_str(&format!("  слотов            {}\n", opt(f.slots)));
    out.push_str(&format!("  сохранение слотов {}\n", opt(f.slot_save)));
    out.push_str(&format!(
        "  reasoning_roundtrip {}, parallel_tools {}, tool_choice_required {}\n",
        c.reasoning_roundtrip, c.parallel_tools, c.tool_choice_required
    ));
    for n in p.profile.notes.iter().chain(&f.notes) {
        out.push_str(&format!("  заметка: {n}\n"));
    }
    out
}

/// Аргументы `ask`.
#[derive(Debug, Clone)]
pub struct AskArgs {
    /// Эндпоинт; `None` — `models.default` (только локальный, D24).
    pub model: Option<String>,
    pub priority: Priority,
    pub text: String,
}

/// `zero-agent ask`: текст ответа — в stdout по мере стрима, `usage` и итог — в stderr.
/// Облачный эндпоинт без ключа — `inactive` без сетевого запроса (MDL-021).
pub async fn cmd_ask(home: &Path, args: AskArgs) -> Result<ExitCode, CommandError> {
    let cfg = load_config(home)?;
    models_of(&cfg).ok_or(CommandError::NoModels)?;
    let s = start_gateway(home, &cfg, true).await?;
    let g = &s.gateway;
    let id = g.select(&Selection {
        run_choice: args.model.map(EndpointId),
        ..Selection::default()
    })?;
    let Some(view) = wait_endpoint(g, &id, PROBE_WAIT).await else {
        return Err(SelectError::Unknown(id).into());
    };
    if !matches!(view.state.as_str(), "ready" | "open") {
        eprintln!(
            "zero-agent: модель {id} не готова: {}",
            state_text(&view, Some(PROBE_WAIT))
        );
        return Ok(ExitCode::FAILURE);
    }
    let call = ModelCall {
        endpoint: id.clone(),
        meta: RequestMeta {
            run_id: RunId::new(),
            priority: args.priority,
            session: None,
            max_wait: None,
        },
        request: ChatRequest {
            messages: vec![Message {
                role: Role::User,
                parts: vec![Part::Text(args.text)],
            }],
            ..ChatRequest::default()
        },
    };
    let mut stream = match g.stream(call).await {
        Ok(st) => st,
        Err(e) => {
            eprintln!("zero-agent: ошибка модели: {:?}: {}", e.class, e.message);
            return Ok(ExitCode::FAILURE);
        }
    };
    let mut out = std::io::stdout();
    // На терминале текст идёт по мере стрима; при повторе (D20) — пометка и ответ
    // заново с новой строки. Вне терминала (конвейер, файл) stdout получает только
    // итог последней попытки — текст из `Finished`, без обрывков.
    let live = out.is_terminal();
    let mut last_wait: Option<WaitState> = None;
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(StreamEvent::TextDelta(t)) if live => {
                let _ = out.write_all(t.as_bytes());
                let _ = out.flush();
            }
            Ok(StreamEvent::Queued(q)) => {
                let shown = matches!(
                    q.state,
                    WaitState::WaitingResource { .. } | WaitState::Cooldown { .. }
                );
                if shown && last_wait.as_ref() != Some(&q.state) {
                    eprintln!(
                        "zero-agent: в очереди, позиция {}: {:?}",
                        q.position, q.state
                    );
                }
                last_wait = Some(q.state);
            }
            Ok(StreamEvent::Restart { attempt, reason }) => {
                if live {
                    let _ = out.write_all(b"\n");
                    let _ = out.flush();
                }
                eprintln!(
                    "zero-agent: повтор, попытка {attempt}: {reason}; ответ печатается заново"
                );
            }
            Ok(StreamEvent::Finished {
                message,
                finish,
                usage,
            }) => {
                if !live {
                    let text: String = message
                        .parts
                        .iter()
                        .filter_map(|p| match p {
                            Part::Text(t) => Some(t.as_str()),
                            _ => None,
                        })
                        .collect();
                    let _ = out.write_all(text.as_bytes());
                    let _ = out.flush();
                }
                eprintln!(
                    "\nfinish: {}\nusage: {}",
                    serde_json::to_string(&finish)?,
                    serde_json::to_string(&usage)?
                );
                return Ok(ExitCode::SUCCESS);
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("\nzero-agent: ошибка модели: {:?}: {}", e.class, e.message);
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    eprintln!("\nzero-agent: ошибка модели: стрим кончился без итога");
    Ok(ExitCode::FAILURE)
}
