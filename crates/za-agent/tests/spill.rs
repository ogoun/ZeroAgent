//! Выгрузка большого вывода (R1-B2 D44) и удаление старых результатов (D45), задача 2:
//! голова и хвост по строкам и по границе символа, лимит после экранирования, файл
//! выгрузки и `.gitignore`, реплей без повторного вызова, новая эпоха истории по порогу.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use za_agent::{
    CallVerdict, ContextConfig, Conversation, EnvInfo, LoopDeps, LoopError, LoopOutcome, NoMemory,
    PlannedCall, RunSpec, SPILL_DIR, SpillCfg, ToolEnv, builtin_tools, control_channel,
    escape_untrusted, prune, run_tools, signal_channel, spill, spill_limit_bytes, start_run,
};
use za_core::{
    Approval, EventBus, Journal, RunCtx, Tool, ToolCx, ToolKind, ToolOutput, ToolRegistry, ToolSpec,
};
use za_llm::normalize::CallSource;
use za_llm::{
    BackendKind, BoxFuture, EndpointId, FinishReason, Message, ModelError, Part, Role,
    ScriptedModel, StreamEvent, ToolCall, ToolSchema, Usage, default_caps,
};
use za_protocol::{
    Budget, EffectClass, Event, EventKind, Priority, RunId, RunMode, RunRole, Seq, Tier,
};
use za_store::{NewRun, ProjectStore};

// ---- Помощники ----

/// Множитель времени тестов: `ZA_TEST_TIME_SCALE` (целое 1–6, по умолчанию 1).
fn time_scale() -> u32 {
    static SCALE: OnceLock<u32> = OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("ZA_TEST_TIME_SCALE")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map_or(1, |n| n.clamp(1, 6))
    })
}

/// Предел ожиданий, которые при регрессии повисли бы.
fn limit() -> Duration {
    Duration::from_secs(30) * time_scale()
}

fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("spill")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn spill_file(project: &Path, name: &str) -> PathBuf {
    project.join(SPILL_DIR).join(format!("{name}.txt"))
}

/// Голова и хвост вокруг строки пропуска `… omitted …`: голова — до строки (без `\n`,
/// вставленного перед ней при разрезе внутри строки), хвост — после неё.
fn split(content: &str) -> (&str, &str, &str) {
    let at = content.find("… omitted ").expect("нет строки пропуска");
    let end = at + content[at..].find('\n').expect("строка пропуска без \\n") + 1;
    let marker = &content[at..end];
    let head = &content[..at];
    // Перед строкой пропуска при разрезе внутри строки вставлен перевод строки.
    let head = if marker.contains(" bytes; ") {
        head.strip_suffix('\n').unwrap_or(head)
    } else {
        head
    };
    (head, &content[at..end], &content[end..])
}

// ---- Выгрузка (D44) ----

#[test]
fn small_output_untouched() {
    let d = dir("small");
    let s = "a".repeat(100);
    let r = spill(&d, "r-1-1", &s, &s, 1000).unwrap();
    assert_eq!(r.content, s);
    assert_eq!(r.path, None);
    assert_eq!((r.omitted_lines, r.omitted_bytes), (0, 0));
    assert!(!spill_file(&d, "r-1-1").exists());
}

#[test]
fn big_output_head_tail_and_file() {
    let d = dir("big");
    let raw: String = (0..1000).map(|i| format!("строка {i:04}\n")).collect();
    let r = spill(&d, "r-3-1", &raw, &raw, 4000).unwrap();
    assert!(r.content.starts_with("строка 0000"), "{}", &r.content[..40]);
    assert!(r.content.ends_with("строка 0999\n"));
    assert!(r.content.contains("omitted "));
    assert!(
        r.content
            .contains("lines; search .zero-agent/work/spill/r-3-1.txt"),
        "{}",
        split(&r.content).1
    );
    assert_eq!(r.path.as_deref(), Some(".zero-agent/work/spill/r-3-1.txt"));
    assert_eq!(r.omitted_lines + r.content.lines().count() - 1, 1000);
    assert_eq!(
        std::fs::read(spill_file(&d, "r-3-1")).unwrap(),
        raw.as_bytes()
    );
    assert!(std::str::from_utf8(r.content.as_bytes()).is_ok());
    assert!(r.content.len() <= 4000 + 120, "{}", r.content.len());
    // Голова 60 %, хвост 40 % лимита — по целым строкам.
    let (head, _, tail) = split(&r.content);
    assert!(head.len() <= 2400 && head.ends_with('\n'));
    assert!(tail.len() <= 1600 && tail.starts_with("строка "));
    assert_eq!(r.omitted_bytes, raw.len() - head.len() - tail.len());
}

#[test]
fn limit_applies_after_escaping() {
    let d = dir("escaped");
    let raw = "<a>\n".repeat(2000);
    let escaped = escape_untrusted(&raw);
    assert_eq!(raw.len(), 8000);
    // `<a>` → `&lt;a&gt;`: 10 байт на строку (бриф: «22000» — у `&lt;a&gt;\n` 10 байт).
    assert_eq!(escaped.len(), 20_000);
    let r = spill(&d, "r-1-2", &escaped, &raw, 4000).unwrap();
    assert!(r.content.len() <= 4000 + 120, "{}", r.content.len());
    assert!(!r.content.contains('<'));
    assert!(r.content.starts_with("&lt;a&gt;\n"));
    assert_eq!(
        std::fs::read(spill_file(&d, "r-1-2")).unwrap(),
        raw.as_bytes()
    );
}

#[test]
fn single_huge_line_cut_on_char_boundary() {
    let d = dir("huge_line");
    const MIB5: usize = 5 * 1024 * 1024;
    let raw = "я".repeat(MIB5 / 2);
    assert_eq!(raw.len(), MIB5);
    let r = spill(&d, "r-1-3", &raw, &raw, 4001).unwrap();
    assert!(std::str::from_utf8(r.content.as_bytes()).is_ok());
    let (head, marker, tail) = split(&r.content);
    assert!(!head.is_empty() && !tail.is_empty());
    assert_eq!(head.len() % 2, 0, "голова режет символ");
    assert_eq!(tail.len() % 2, 0, "хвост режет символ");
    assert!(head.len() <= 2400 && tail.len() <= 1601);
    assert_eq!(r.omitted_lines, 0);
    assert_eq!(r.omitted_bytes, MIB5 - (head.len() + tail.len()));
    assert_eq!(
        marker,
        format!(
            "… omitted {} bytes; search .zero-agent/work/spill/r-1-3.txt\n",
            r.omitted_bytes
        )
    );
    assert!(r.content.len() <= 4001 + 120, "{}", r.content.len());
    assert_eq!(std::fs::read(spill_file(&d, "r-1-3")).unwrap().len(), MIB5);
}

#[test]
fn gitignore_created() {
    let d = dir("gitignore");
    let raw = "x\n".repeat(1000);
    spill(&d, "r-1-4", &raw, &raw, 100).unwrap();
    assert_eq!(
        std::fs::read_to_string(d.join(".zero-agent/work/.gitignore")).unwrap(),
        "*\n"
    );
}

#[test]
fn limit_bytes_from_window_and_tool() {
    // D44: ⌊0,15 × 8192 × 4⌋ = 4915; `output_limit` уменьшает, но не увеличивает.
    assert_eq!(spill_limit_bytes(8192, 0.15, None), 4915);
    assert_eq!(spill_limit_bytes(8192, 0.15, Some(4096)), 4096);
    assert_eq!(spill_limit_bytes(8192, 0.15, Some(10_000)), 4915);
}

/// Инструмент чтения: каждый вызов — `out` и +1 к счётчику.
struct Big {
    spec: ToolSpec,
    out: String,
    calls: Arc<AtomicUsize>,
}

impl Tool for Big {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, _args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let content = self.out.clone();
        Box::pin(async move {
            ToolOutput {
                content,
                is_error: false,
            }
        })
    }
}

fn tool_spec(name: &str, output_limit: Option<usize>) -> ToolSpec {
    ToolSpec {
        schema: ToolSchema {
            name: name.into(),
            description: "t".into(),
            parameters: json!({"type": "object"}),
        },
        class: EffectClass::ReadOnly,
        kind: ToolKind::Read,
        approval: Approval::Mode,
        roles: vec![RunRole::Top],
        min_tier: Tier::Small,
        timeout: Duration::from_secs(60),
        output_limit,
        tainted: false,
    }
}

#[tokio::test]
async fn spill_survives_replay() {
    let d = dir("replay");
    let s = ProjectStore::open(&d.join("store")).unwrap();
    let run = RunId::new();
    s.create_run(NewRun {
        id: run,
        project_id: "p".into(),
        parent_id: None,
        role: RunRole::Top,
        model_profile: None,
    })
    .await
    .unwrap();
    let j = Journal::new(s.clone(), EventBus::new(EventBus::DEFAULT_CAPACITY));
    let raw: String = (0..5120).map(|i| format!("<l {i:05}>\n")).collect();
    assert_eq!(raw.len(), 50 * 1024);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::default();
    reg.register(Arc::new(Big {
        // Лимит 4 КиБ: `output_limit` меньше лимита окна 4915.
        spec: tool_spec("big", Some(4096)),
        out: raw.clone(),
        calls: Arc::clone(&calls),
    }))
    .unwrap();
    let env = ToolEnv {
        run,
        project_dir: d.clone(),
        mode: RunMode::Ask,
        parallel: false,
        spill: Some(SpillCfg {
            n_ctx: 8192,
            share: 0.15,
        }),
    };
    let planned = [PlannedCall {
        call: ToolCall {
            id: "c1".into(),
            name: "big".into(),
            arguments: json!({}),
            raw_arguments: None,
        },
        source: CallSource::Native,
        verdict: CallVerdict::Run,
    }];
    let batch = || async {
        let (_tx, mut sig) = signal_channel();
        let mut ctx = RunCtx::resume_at(s.clone(), run, 0, 0);
        tokio::time::timeout(
            limit(),
            run_tools(&mut ctx, &j, &reg, &env, &planned, &mut sig),
        )
        .await
        .expect("run_tools не вернулся за предел теста")
        .unwrap()
    };
    let live = batch().await;
    let name = format!("{run}-0-0");
    let file = spill_file(&d, &name);
    let bytes = std::fs::read(&file).expect("нет файла выгрузки");
    assert_eq!(bytes, raw.as_bytes());
    let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
    let Part::ToolResult {
        content, is_error, ..
    } = &live.results[0]
    else {
        panic!("не результат");
    };
    assert!(!is_error);
    assert!(content.len() <= 4096 + 120, "{}", content.len());
    assert!(!content.contains('<'));
    assert!(
        content.contains(&format!(
            "lines; search .zero-agent/work/spill/{name}.txt\n"
        )),
        "{}",
        split(content).1
    );

    // Повтор из журнала: та же квитанция, инструмент и запись файла не повторяются.
    let replay = batch().await;
    assert_eq!(replay.results, live.results);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
    assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), mtime);
}

// ---- Удаление старых результатов (D45) ----

fn call_msg(id: &str) -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall(ToolCall {
            id: id.into(),
            name: "dump".into(),
            arguments: json!({}),
            raw_arguments: None,
        })],
    }
}

fn result_part(id: &str, content: String) -> Part {
    Part::ToolResult {
        call_id: id.into(),
        content,
        is_error: false,
    }
}

/// История: задача и ходы с одним вызовом `c{k}` и его результатом.
fn history(results: &[String]) -> Conversation {
    let mut c = Conversation::default();
    c.push_user("task".into()).unwrap();
    for (k, r) in results.iter().enumerate() {
        let id = format!("c{}", k + 1);
        c.commit_turn(Vec::new(), call_msg(&id)).unwrap();
        c.push_results(vec![result_part(&id, r.clone())]).unwrap();
    }
    c
}

fn result_of(c: &Conversation, call_id: &str) -> String {
    c.messages()
        .iter()
        .flat_map(|m| &m.parts)
        .find_map(|p| match p {
            Part::ToolResult {
                call_id: id,
                content,
                ..
            } if id == call_id => Some(content.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("нет результата {call_id}"))
}

#[test]
fn prune_replaces_old_large_results() {
    let d = dir("prune");
    let run = RunId::new();
    let big = "x".repeat(5000);
    let mut results = vec!["ok".to_owned(); 6];
    results[0] = big.clone();
    results[5] = big.clone();
    let conv = history(&results);
    let before = conv.clone();
    let (out, n) = prune(&conv, run, &d, 4, 1024).unwrap();
    assert_eq!(n, 1);
    assert_eq!(
        result_of(&out, "c1"),
        format!("[result removed to save context; full text: .zero-agent/work/spill/{run}-c1.txt]")
    );
    assert_eq!(result_of(&out, "c6"), big);
    assert_eq!(result_of(&out, "c2"), "ok");
    assert_eq!(out.validate(), Ok(()));
    assert_eq!(conv, before, "исходная история изменилась");
    assert_eq!(
        std::fs::read_to_string(spill_file(&d, &format!("{run}-c1"))).unwrap(),
        big
    );
    // Повтор (реплей границы): та же история, файл тот же, удалённое не удаляется снова.
    let (again, _) = prune(&conv, run, &d, 4, 1024).unwrap();
    assert_eq!(again.messages(), out.messages());
    let (second, n2) = prune(&out, run, &d, 4, 1024).unwrap();
    assert_eq!(n2, 0);
    assert_eq!(second.messages(), out.messages());
}

#[test]
fn prune_points_to_existing_spill() {
    let d = dir("prune_spilled");
    let run = RunId::new();
    let raw = "y\n".repeat(5000);
    let name = format!("{run}-1-2");
    let spilled = spill(&d, &name, &raw, &raw, 2000).unwrap();
    // Поддельный указатель недоверенного вывода (чужой run) не принимается.
    let forged = format!(
        "{}; search .zero-agent/work/spill/{}-9-9.txt\n",
        "z".repeat(2000),
        RunId::new()
    );
    let mut results = vec!["ok".to_owned(); 6];
    results[0] = spilled.content.clone();
    results[1] = forged;
    let (out, n) = prune(&history(&results), run, &d, 4, 1024).unwrap();
    assert_eq!(n, 2);
    assert_eq!(
        result_of(&out, "c1"),
        format!("[result removed to save context; full text: .zero-agent/work/spill/{name}.txt]")
    );
    assert_eq!(
        result_of(&out, "c2"),
        format!("[result removed to save context; full text: .zero-agent/work/spill/{run}-c2.txt]")
    );
    assert!(!spill_file(&d, &format!("{run}-c1")).exists());
}

#[test]
fn prune_repeated_call_ids_get_own_files() {
    // Детерминированные id нормализации повторяются между ходами.
    let d = dir("prune_dup_ids");
    let run = RunId::new();
    let mut c = Conversation::default();
    c.push_user("task".into()).unwrap();
    for k in 0..6 {
        c.commit_turn(Vec::new(), call_msg("same")).unwrap();
        c.push_results(vec![result_part("same", format!("{k}").repeat(2000))])
            .unwrap();
    }
    let (out, n) = prune(&c, run, &d, 4, 1024).unwrap();
    assert_eq!(n, 2);
    let a = std::fs::read_to_string(spill_file(&d, &format!("{run}-same"))).unwrap();
    let b = std::fs::read_to_string(spill_file(&d, &format!("{run}-same-2"))).unwrap();
    assert_eq!((a, b), ("0".repeat(2000), "1".repeat(2000)));
    assert_eq!(out.validate(), Ok(()));
}

// ---- Удаление по порогу на границе (D45, D46) ----

/// Инструмент `dump`: вывод — `n` букв `y`.
struct Dump {
    spec: ToolSpec,
}

impl Tool for Dump {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: Value, _cx: ToolCx) -> BoxFuture<'static, ToolOutput> {
        let n = args.get("n").and_then(Value::as_u64).unwrap_or(0) as usize;
        Box::pin(async move {
            ToolOutput {
                content: "y".repeat(n),
                is_error: false,
            }
        })
    }
}

fn dump_turn(id: &str, n: usize, input: u64) -> Vec<Result<StreamEvent, ModelError>> {
    vec![Ok(StreamEvent::Finished {
        message: Message {
            role: Role::Assistant,
            parts: vec![Part::ToolCall(ToolCall {
                id: id.into(),
                name: "dump".into(),
                arguments: json!({ "n": n }),
                raw_arguments: None,
            })],
        },
        finish: FinishReason::ToolCalls,
        usage: Usage {
            input,
            ..Usage::default()
        },
    })]
}

fn answer(t: &str) -> Vec<Result<StreamEvent, ModelError>> {
    vec![Ok(StreamEvent::Finished {
        message: Message {
            role: Role::Assistant,
            parts: vec![Part::Text(t.into())],
        },
        finish: FinishReason::Stop,
        usage: Usage::default(),
    })]
}

fn run_spec(project: &Path) -> RunSpec {
    RunSpec {
        project_id: "p".into(),
        project_dir: project.to_path_buf(),
        role: RunRole::Top,
        parent: None,
        endpoint: EndpointId("local".into()),
        run_mode: RunMode::Ask,
        work_mode: None,
        tier: None,
        budget: Budget::default(),
        priority: Priority::Top,
        max_wait: None,
        task: "Собери данные".into(),
        agents_md: None,
        env: EnvInfo {
            os: "windows".into(),
            shell: "powershell".into(),
            project_dir: "H:/p".into(),
            date: "2026-11-10".into(),
            tier: Tier::Full,
            n_ctx: 1,
            budget: "x".into(),
            branch: "main".into(),
        },
        context: ContextConfig::default(),
    }
}

async fn events(s: &ProjectStore, run: RunId) -> Vec<Event> {
    s.read_run(run, Seq(0), 100_000).await.unwrap()
}

/// Квитанция эффекта по намерению с этими аргументами.
fn receipt<'a>(evs: &'a [Event], name: &str, args: &Value) -> (&'a Event, &'a Event) {
    let intent = evs
        .iter()
        .find(|e| {
            e.kind == EventKind::EffectIntent
                && e.payload["name"] == name
                && &e.payload["args"] == args
        })
        .unwrap_or_else(|| panic!("нет эффекта {name} {args}"));
    let rec = evs
        .iter()
        .find(|e| e.kind == EventKind::EffectReceipt && e.payload["key"] == intent.payload["key"])
        .unwrap_or_else(|| panic!("нет квитанции {name} {args}"));
    (intent, rec)
}

fn tokens_of(rec: &Event) -> u64 {
    rec.payload["result"]["value"]["tokens"]
        .as_u64()
        .unwrap_or_else(|| panic!("квитанция без tokens: {}", rec.payload))
}

#[tokio::test]
async fn prune_triggers_by_threshold() {
    let d = dir("threshold");
    let store = ProjectStore::open(&d.join("store")).unwrap();
    // Точный счётчик: байты / 100 — оценку задаёт расход хода 6 (база D41).
    let model = Arc::new(
        ScriptedModel::new(default_caps(BackendKind::LlamaCpp))
            .with_context(8192)
            .with_token_counter(|s| (s.len() / 100) as u32),
    );
    let mut reg = ToolRegistry::default();
    for t in builtin_tools() {
        reg.register(t).unwrap();
    }
    reg.register(Arc::new(Dump {
        spec: tool_spec("dump", None),
    }))
    .unwrap();
    let deps = LoopDeps {
        journal: Journal::new(store.clone(), EventBus::new(EventBus::DEFAULT_CAPACITY)),
        model: model.clone(),
        tools: Arc::new(reg),
        capabilities: Vec::new(),
        memory: Arc::new(NoMemory),
        crash: None,
    };
    // Ходы 1–5 без расхода (базы нет, оценка всего запроса мала); ход 6 сообщает
    // input 2400: на границе 7 оценка = 2400 + 4000 / 100 + 4 = 2444.
    // Окно 8192 даёт тир small, Effort(Low): резерв 3072 (D40; бриф: reasoning Off и
    // резерв 1000 при оценке 4500 — Off через RunSpec недостижим, мера та же):
    // 2444 + 3072 = 5516 ≥ 0,65 × 8192 = 5324,8 и < 0,75 × 8192 = 6144 — только удаление.
    model.push(dump_turn("c1", 4000, 0));
    for k in 2..=5 {
        model.push(dump_turn(&format!("c{k}"), 10, 0));
    }
    model.push(dump_turn("c6", 4000, 2400));
    model.push(answer("готово"));
    let run = RunId::new();
    let (_ctl, rx) = control_channel(store.clone(), run);
    let out: Result<LoopOutcome, LoopError> =
        tokio::time::timeout(limit(), start_run(&deps, run, run_spec(&d), rx))
            .await
            .expect("start_run не завершился в срок");
    out.unwrap();

    let evs = events(&store, run).await;
    let pruned: Vec<&Event> = evs
        .iter()
        .filter(|e| e.kind == EventKind::ContextPruned)
        .collect();
    assert_eq!(pruned.len(), 1, "context.pruned: {pruned:?}");
    let p = &pruned[0].payload;
    assert_eq!(p["epoch"], 1);
    assert!(p["count"].as_u64().unwrap() >= 1, "{p}");
    assert_eq!(p["before"], 2444, "{p}");

    // Эффект context {pass: 1} — после удаления (pass 0 того же шага) и до события.
    let (i0, r0) = receipt(&evs, "context", &json!({"step": 7, "pass": 0}));
    let (i1, r1) = receipt(&evs, "context", &json!({"step": 7, "pass": 1}));
    assert_eq!(tokens_of(r0), 2444);
    assert!(i0.seq < i1.seq && r1.seq < pruned[0].seq);
    let after = tokens_of(r1);
    assert!(after < 2444, "следующий запрос не короче: {after}");
    assert_eq!(p["after"], after);
    // Порог компакции не достигнут: заглушки компакции нет.
    assert!(
        !evs.iter()
            .any(|e| e.kind == EventKind::ContextAlert && e.payload["kind"] == "threshold"),
        "context.alert threshold без порога компакции"
    );

    // Ход 7 видит замену; запроса компакции не было — ровно 7 запросов модели.
    let calls = model.calls();
    assert_eq!(calls.len(), 7);
    let req7 = &calls[6].request.messages;
    let removed =
        format!("[result removed to save context; full text: .zero-agent/work/spill/{run}-c1.txt]");
    let r1_content = req7
        .iter()
        .flat_map(|m| &m.parts)
        .find_map(|p| match p {
            Part::ToolResult {
                call_id, content, ..
            } if call_id == "c1" => Some(content.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(r1_content, removed);
    assert_eq!(
        std::fs::read_to_string(spill_file(&d, &format!("{run}-c1"))).unwrap(),
        "y".repeat(4000)
    );
    // Эпоха — в снимке.
    let snaps = store.snapshots(run).await.unwrap();
    let last: za_agent::Snapshot = serde_json::from_value(snaps.last().unwrap().1.clone()).unwrap();
    assert_eq!(last.epoch, 1);
}
