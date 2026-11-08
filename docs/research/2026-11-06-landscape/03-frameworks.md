# Агентные фреймворки: архитектурные примитивы для zero-agent (срез 2026-11-06)

Метод: первичные источники (README, docs и исходники репозиториев через `gh api` на HEAD, официальные сайты протоколов), вторичные источники только там, где они помечены. Код из репозиториев не запускался. Звёзды и даты получены через GitHub API 2026-11-06.

Статус проверки ссылок задания: все 12 ссылок существуют, включая discussion 7066. Уточнения: `microsoft/autogen` помечен в README как архивный, хотя GitHub-флаг `archived=false` — см. вопрос 4. Оркестрация LlamaIndex фактически живёт в отдельном репозитории `run-llama/llama-agents` (редирект с `run-llama/workflows-py`). Несуществующие адреса, проверенные зондами: `rig-rs/rig`, `ag-ui-protocol/rust-sdk`, `a2aproject/a2a-rust`, `langgraph-rs/langgraph` (404). Rust-реализации протоколов нашлись под другими адресами (вопрос 3).

## Вопрос 1. Что каждый делает лучше всего, что копировать и что пропустить

### Takeaway
Рынок сошёлся на одной форме: цикл агента + граф поверх него + пауза как структурированный терминальный результат + единый поток событий с `seq`. Лучшие идеи: `Capability` и `Deferred`-инструменты у Pydantic AI, суперстепы и conformance-тест чекпоинтера у LangGraph, `graph_signature_hash` у MAF, разделение трёх уровней guardrails у OpenAI, граница host call у Chidori. Групповые чаты, ролевые команды и DSL как ядро не копировать: AutoGen этим путём уже прошёл и перешёл на граф.

### Cited Findings

#### Состояние проектов

| Проект | Звёзды | Релиз | Позиционирование и статус |
|---|---|---|---|
| langgraph | 44.6k | sdk 0.4.14, 2026-11-06 | "Build resilient agents" |
| pydantic-ai | 21.1k | v2.66.0, 2026-11-04 | агенты, realtime, плюс отдельный Harness 0.x |
| openai-agents-python | 30.5k | v0.24.0, 2026-10-15 | всё ещё 0.x; добавлены sandbox agents |
| adk-python | 22.1k | v2.13.0, 2026-10-23 | ADK 2.0: появился Workflow runtime |
| crewAI | 60.6k | 1.15.36, 2026-11-06 | Crews + Flows |
| mastra | 29.4k | core 1.82.0, 2026-11-04 | TS, частично `ee/`-лицензия |
| agent-framework (MAF) | 14.6k | python-1.24.0, 2026-10-30 | python-1.0.0 от 2026-04-02 |
| autogen | 61.7k | python-v0.7.5, 2025-09-30 | архивный по README |
| llama_index | 52.8k | v0.14.25, 2026-09-21 | фокус сместился на разбор документов |
| agno | 43.1k | v3.1.12, 2026-10-29 | "self-driving agent platform" |
| dify | 160.5k | 1.19.0, 2026-11-04 | платформа, движок вынесен в `graphon` |
| rig | 9.1k | v0.46.0, 2026-10-31 | библиотека + кодинг-агент на Bevy |

Источник таблицы: GitHub API (`repos/*`, `releases/latest`). Лицензия Mastra с каталогами `ee/` — [LICENSE.md](https://github.com/mastra-ai/mastra/blob/HEAD/LICENSE.md). Дата MAF 1.0 — по коммиту тега [python-1.0.0](https://github.com/microsoft/agent-framework/releases/tag/python-1.0.0).

#### Сравнение по измерениям

**Граф выполнения и машина состояний**
- LangGraph: суперстепы в стиле Pregel и типизированные каналы (`last_value`, `binop`, `topic`, `delta` и др.) — [каталог channels](https://github.com/langchain-ai/langgraph/tree/HEAD/libs/langgraph/langgraph/channels). Отдельно есть `Send` и `Command` — [types.py](https://github.com/langchain-ai/langgraph/blob/HEAD/libs/langgraph/langgraph/types.py).
- MAF: Runner прямо описан как "run a workflow in Pregel supersteps", с `max_iterations=100` — [_runner.py](https://github.com/microsoft/agent-framework/blob/HEAD/python/packages/core/agent_framework/_workflows/_runner.py). Паттерны sequential, parallel, Magentic построены поверх workflow API — [discussion 7066](https://github.com/microsoft/autogen/discussions/7066). Есть и функциональный API с `@step` — [samples](https://github.com/microsoft/agent-framework/blob/HEAD/python/samples/03-workflows/README.md).
- ADK 2.0: граф `Workflow` с узлами-агентами и задачами — [README](https://github.com/google/adk-python/blob/HEAD/README.md).
- LlamaIndex Agent Workflows: событийная модель, шаги потребляют и испускают события — [llama-agents](https://github.com/run-llama/llama-agents).
- Dify: очередной `Engine`, событийное выполнение, импорт DSL; вынесен в [graphon](https://github.com/langgenius/graphon) (Python, Apache-2.0, 86 звёзд), закреплён в зависимостях [api/pyproject.toml](https://github.com/langgenius/dify/blob/HEAD/api/pyproject.toml).
- OpenAI Agents SDK: цикл runner без графа, оркестрация через handoffs и `Agent.as_tool` — [README](https://github.com/openai/openai-agents-python/blob/HEAD/README.md).
- Pydantic: цикл агента плюс отдельный `pydantic-graph` с builder, parallel, joins, decisions — [docs/graph](https://github.com/pydantic/pydantic-ai/tree/HEAD/docs/graph/builder).

**Durable execution, checkpoint, resume** — подробно в вопросе 2.

**Human-in-the-loop**
- LangGraph: `interrupt(value)` реализован через исключение; при resume узел перезапускается с начала; значения сопоставляются строго по индексу; с `langgraph>=1.2.12` есть `response_schema` с валидацией ответа; статические `interrupt_before/after` предназначены для отладки — [Interrupts](https://docs.langchain.com/oss/python/langgraph/interrupts).
- Pydantic AI: `ApprovalRequired`, `CallDeferred`, `DeferredToolRequests`, `DeferredToolResults`, `ToolDenied`. Два потока: завершить запуск с запросами или обработать их handler-ом; они компонуются. Валидация аргументов повторяется при возобновлении — [deferred-tools](https://github.com/pydantic/pydantic-ai/blob/HEAD/docs/deferred-tools.md).
- OpenAI: `needs_approval` (bool или async-предикат), `RunResult.interruptions`, `RunState` с `approve`/`reject`, сериализация между процессами; прерывания вложенных `as_tool` всплывают на внешний запуск; предикат fail-closed при кривых аргументах — [human_in_the_loop](https://github.com/openai/openai-agents-python/blob/HEAD/docs/human_in_the_loop.md). Там же предупреждение: sticky-решения для shell и file-edit привязаны к типу и имени инструмента, а не к агенту.
- ADK: `rerun_on_resume=True` для оркестрирующих узлов (частичные ответы запускают разрешённые ветки) и `False` для листовых (ждать все) — [architecture-checkpoint-resume](https://github.com/google/adk-python/blob/HEAD/.agents/skills/adk-architecture/references/architecture-checkpoint-resume.md).
- Mastra: `suspend()` в шаге, `resumeData`, схема `resumeSchema` на zod — [suspend-and-resume](https://github.com/mastra-ai/mastra/blob/HEAD/docs/src/content/en/docs/workflows/suspend-and-resume.mdx).
- LlamaIndex: пара событий `InputRequiredEvent`/`HumanResponseEvent` — [human_in_the_loop](https://github.com/run-llama/llama-agents/blob/HEAD/docs/src/content/docs/llamaagents/workflows/human_in_the_loop.md).
- Dify: паузы как сущность БД с таблицами причин — [миграции и `workflow_pause_service.py`](https://github.com/langgenius/dify/tree/HEAD/api/services).

**Определение и валидация инструментов; структурированный вывод**
- Pydantic AI: три режима вывода (Tool, Native, Prompted), выходные функции, output validators с поддержкой частичного вывода, обработка усечения по токенам, стриминг структурированного вывода — [output.md](https://github.com/pydantic/pydantic-ai/blob/HEAD/docs/output.md).
- OpenAI: `is_enabled`, `input_type` и `input_filter` на handoff; handoff — это инструмент модели — [handoffs](https://github.com/openai/openai-agents-python/blob/HEAD/docs/handoffs.md).
- Vercel AI SDK 7: `toolSearch()` с `deferLoading` не грузит инструменты в первый промпт, совпадения становятся вызываемыми на следующем шаге — [вторичный обзор](https://www.developersdigest.tech/blog/vercel-ai-sdk-whats-new-2026).
- MCP `Tool Annotations as Risk Vocabulary` — [блог MCP](https://blog.modelcontextprotocol.io/posts/2026-03-16-tool-annotations/).

**Handoffs и мультиагентность**
- OpenAI: handoff с `input_filter` и `nest_handoff_history`, плюс агент как инструмент — [handoffs](https://github.com/openai/openai-agents-python/blob/HEAD/docs/handoffs.md).
- MAF: sequential, concurrent, handoff, group collaboration как шаблоны поверх workflow — [README](https://github.com/microsoft/agent-framework/blob/HEAD/README.md).
- CrewAI: Crews (ролевая модель) и Flows; Flow получил `conversation.py`, `human_feedback.py` — [lib/crewai/src/crewai/flow](https://github.com/crewAIInc/crewAI/tree/HEAD/lib/crewai/src/crewai/flow).

**Guardrails**
- OpenAI: input guardrails только на первом агенте, output только на финальном, tool guardrails на каждом вызове. Параллельный режим по умолчанию: tripwire может сработать, когда агент уже потратил токены и выполнил инструменты — [guardrails](https://github.com/openai/openai-agents-python/blob/HEAD/docs/guardrails.md).
- Pydantic AI Harness: guard возвращает один из пяти исходов (allow, block, redact, retry, require approval), реализовано как три capability на хуках — [harness/guardrails](https://github.com/pydantic/pydantic-ai/blob/HEAD/docs/harness/guardrails.md).
- Mastra: входные и выходные processors, включая нормализацию Unicode и детектор инъекций — [guardrails](https://github.com/mastra-ai/mastra/blob/HEAD/docs/src/content/en/docs/agents/guardrails.mdx).
- MAF ADR-0024 (статус proposed): FIDES, детерминированная защита от инъекций через information-flow control и метки, а не эвристики — [ADR](https://github.com/microsoft/agent-framework/blob/HEAD/docs/decisions/0024-prompt-injection-defense.md).

**Трассировка и наблюдаемость**
- OpenAI: собственная иерархия `task_span`, `turn_span`, `agent_span`, `generation_span`, `function_span` — [tracing](https://github.com/openai/openai-agents-python/blob/HEAD/docs/tracing.md).
- MAF и rig: OpenTelemetry из коробки; rig заявляет GenAI semantic conventions — [MAF README](https://github.com/microsoft/agent-framework/blob/HEAD/README.md), [rig README](https://github.com/0xPlaygrounds/rig/blob/HEAD/README.md).
- Конвенции OTel GenAI, включая агентные спаны, на HEAD всё ещё имеют статус Development; у репозитория нет опубликованных релизов — [gen-ai-agent-spans.md](https://github.com/open-telemetry/semantic-conventions-genai/blob/HEAD/docs/gen-ai/gen-ai-agent-spans.md). Посты о стабилизации опровергнуты — [DEV](https://dev.to/azena-ai/opentelemetrys-genai-semantic-conventions-are-not-stable-yet-heres-what-actually-shipped-in-2026-3mke).

**Evals**
- Mastra: scorers, datasets, experiments, gates and verdicts, multi-turn, CI, Vitest — [docs/evals](https://github.com/mastra-ai/mastra/tree/HEAD/docs/src/content/en/docs/evals).
- Pydantic Evals: core concepts и онлайн-оценка — [docs/evals](https://github.com/pydantic/pydantic-ai/tree/HEAD/docs/evals).
- ADK: `adk eval` по `evalset.json` с конфигом критериев — [README](https://github.com/google/adk-python/blob/HEAD/README.md).
- rig-cassette: effect logs, HTTP-кассеты, golden-файлы агентов, скрабер — [README](https://github.com/0xPlaygrounds/rig/blob/HEAD/crates/rig-cassette/README.md).
- Chidori: чекпоинт коммитится как тестовая фикстура, replay без вызовов LLM — [README](https://github.com/ThousandBirdsInc/chidori/blob/HEAD/README.md).

**Память**
- Pydantic AI — самое ясное разделение: хранение диалога, `ConversationStore`, `StepPersistence`, межсессионная `Memory`, durable engine — пять разных задач; "память диалога — это и есть история сообщений" — [persistence.md](https://github.com/pydantic/pydantic-ai/blob/HEAD/docs/persistence.md).
- Mastra Observational Memory: фоновые Observer и Reflector ведут плотный журнал наблюдений, заменяющий сырую историю — [observational-memory](https://github.com/mastra-ai/mastra/blob/HEAD/docs/src/content/en/docs/memory/observational-memory.mdx). Также working memory, semantic recall, memory processors — [docs/memory](https://github.com/mastra-ai/mastra/tree/HEAD/docs/src/content/en/docs/memory).
- LangGraph: разделение checkpointer (на поток) и store (между потоками) — [Persistence](https://docs.langchain.com/oss/python/langgraph/durable-execution).
- MAF: ADR по стратегии компакции контекста — [ADR-0019](https://github.com/microsoft/agent-framework/blob/HEAD/docs/decisions/0019-python-context-compaction-strategy.md).

**Типизированная инъекция зависимостей**
- Pydantic AI: `deps` произвольного типа, доступ из инструкций, инструментов, output validators через `RunContext` — [dependencies](https://github.com/pydantic/pydantic-ai/blob/HEAD/docs/dependencies.md).
- OpenAI: `RunContextWrapper` — [context](https://github.com/openai/openai-agents-python/blob/HEAD/docs/context.md).
- CrewAI (негативный пример): при восстановлении из чекпоинта не возвращаются замыкания, bound methods, callable guardrails; чекпоинт рассматривается как данные, импорт callback-путей только по `CREWAI_DESERIALIZE_CALLBACKS=1` — [checkpointing](https://github.com/crewAIInc/crewAI/blob/HEAD/docs/edge/en/concepts/checkpointing.mdx).

**Расширяемость**
- Pydantic AI: `Capability` объединяет инструменты, хуки, инструкции, настройки модели и выбор модели; это "основная точка расширения" для памяти, guardrails, учёта стоимости и approvals — [capabilities](https://github.com/pydantic/pydantic-ai/blob/HEAD/docs/capabilities/overview.md).
- rig: типизированные хуки с action-перечислениями (`RunStartAction`, `ModelTurnAction`, `OutcomeAction`) — [hook.rs](https://github.com/0xPlaygrounds/rig/blob/HEAD/crates/rig-agent/src/agent/hook.rs).

#### Rust-экосистема

| Проект | Звёзды, свежесть | Что это | Ценность |
|---|---|---|---|
| [rig](https://github.com/0xPlaygrounds/rig) | 9.1k, v0.46 | 20+ провайдеров, 10+ векторных баз, агенты, стриминг, typed hooks; плюс кодинг-агент на Bevy и ядре `rig-ecs` (агенты, ходы, вызовы как сущности) | кассеты, хуки, слой провайдеров |
| [adk-rust](https://github.com/zavora-ai/adk-rust) | 743, push 2026-11-01 | ~48 крейтов: граф, чекпоинтеры, A2A, ACP, AWP, harness, платежи | самый полный по заявкам `adk-graph` |
| [Chidori](https://github.com/ThousandBirdsInc/chidori) | 1.4k, Apache-2.0 | один Rust-бинарник, встроенный JS-движок; все эффекты через host call | эталон идеи "граница эффектов" |
| [graph-flow](https://github.com/a-agmon/rs-graph-llm) | 386, MIT | `Task`, `NextAction`, `Context`, `Session`, `PostgresSessionStorage` | самый простой рабочий API |
| [swiftide](https://github.com/bosun-ai/swiftide) | 802 | agent harness, typed task graphs, паузы и возобновление, RAG | типизированные графы |
| [AutoAgents](https://github.com/liquidos-ai/AutoAgents) | 778, v0.4.0 | ReAct-исполнители, derive-макросы инструментов, WASM-песочница инструментов, guardrails, OTel | WASM-песочница |
| [goose](https://github.com/aaif-goose/goose) | 56.3k | продукт на Rust, в составе AAIF, использует ACP | не фреймворк, но зрелый референс |
| [codex](https://github.com/openai/codex) | 133k | Rust-агент, `app-server` с протоколом thread/item, переигрывание approval при переподключении клиента — [README](https://github.com/openai/codex/blob/HEAD/codex-rs/app-server/README.md) | референс протокола локальный агент ↔ UI |

Детали adk-graph (самоописание, независимо не подтверждено): режимы `Sync/Async/Exit`, узел с объявленным `NonIdempotent` заставляет записать предыдущие чекпоинты первыми, at-least-once с явной оговоркой, `RetentionPolicy`, fork с lineage, `with_governed_node` с at-most-once, единый conformance-набор для бэкендов — [adk-graph/README](https://github.com/zavora-ai/adk-rust/blob/HEAD/adk-graph/README.md). Лицензия по API NOASSERTION; `releases/latest` даёт v2.3.0 (2026-10-09), а README показывает 3.0.0; основной автор один (863 коммита против 50 у следующего).

Также найдены, но не анализировались: `zeroclaw-labs/zeroclaw` (33k), `moltis-org/moltis` (2.9k), `ldclabs/anda`, `mofa-org/mofa`, `floneum/kalosm`, `Abraxas-365/langchain-rust`.

Бенчмарк производительности от авторов AutoAgents (Rust менее 1.1 ГБ против более 4.7 ГБ у Python) — [DEV](https://dev.to/saivishwak/benchmarking-ai-agent-frameworks-in-2026-autoagents-rust-vs-langchain-langgraph-llamaindex-338f); источник заинтересованный, один прогон.

### Inferences

| Откуда | Копировать | Пропустить |
|---|---|---|
| LangGraph | суперстеп как граница атомарности, per-task pending writes, `response_schema` у прерываний, conformance-набор чекпоинтера, режим `sync` | полный набор каналов, позиционное сопоставление resume, `interrupt` через исключение с перезапуском узла |
| Pydantic AI | `Capability`, `Deferred*`, три режима вывода, `deps`, таблицу "какая задача — какой механизм", `UIAdapter` | адаптеры к девяти движкам, разделение core/harness как внешнюю сложность |
| OpenAI Agents | три уровня guardrails, handoff как инструмент, `input_filter`, fail-closed предикат | параллельные guardrails по умолчанию для агентов с побочными эффектами, sticky approval без области действия |
| ADK | `rerun_on_resume`, повторное использование `run_id` при resume, честный документ о собственных ограничениях | журнал как единственный путь resume со стоимостью O(истории) без снимков |
| CrewAI | запись чекпоинта по событиям, fork с lineage, `StreamFrame` | ролевую метафору как ядро, best-effort запись чекпоинта, восстановление объекта из файла |
| Mastra | time travel с отказом при смене определения, разделение resumable streams и durable run, Observational Memory, evals с gates | |
| MAF | `graph_signature_hash`, поля `WorkflowCheckpoint`, практику ADR, FIDES как направление | Foundry-обвязку, две языковые реализации |
| LlamaIndex | событийная модель как идея, `Context.to_dict()` | RAG-абстракции в ядре |
| Agno | `checkpoint="tool-batch"`, `continue_from` с привязкой к безопасной границе tool-call, авто-fork завершённых запусков | Company OS |
| Dify | пауза как сущность БД с причиной, формы human-input | платформу и DSL в ядре |
| rig | кассеты, action-хуки | ECS/Bevy как ядро для обычного агента (сильная ставка; для агента с веб-UI выгода не показана) |
| Chidori | все эффекты через единый шлюз, replay без затрат | встроенный JS-движок |

Сквозные выводы:
- Конвергентная форма паузы: LangGraph (`Command(resume=map по id)`), Pydantic AI (`DeferredToolResults`), OpenAI (`RunState`), ADK (`FunctionResponse`), AG-UI (терминальный `RunFinished` с `outcome=interrupt` и новый запуск с ответами) и MCP MRTR (`input_required` и повтор с `inputResponses`) делают одно и то же. Пауза — это конечный результат со списком запросов, возобновление — новый вызов с ответами по ID. Это главный кандидат на примитив.
- Конвергентная форма потока: `ProtocolEvent` у LangGraph, `StreamFrame` у CrewAI и `seq`-подобные конверты везде содержат монотонный номер и привязку к контексту.
- Для локальных моделей с малым контекстом (требование [ZeroAgentTask.txt](../../../ZeroAgentTask.txt)) особенно важны `toolSearch`-подобная отложенная загрузка, режим Prompted/грамматика и компакция в духе Observational Memory.

### Gaps
- DI в LangGraph (`Runtime`/`context_schema`), LlamaIndex, Agno, ADK, MAF не проверены по первичным источникам.
- Не проверял реальный API `Tool` в rig: `crates/rig-core/src/tool/mod.rs` не содержит трейта в ожидаемой форме, вероятно, перенесён.
- Заявки adk-rust и Chidori не проверены запуском и независимыми отчётами; продакшен-использование всех Rust-фреймворков не подтверждено.
- Внутренняя логика guardrails Agno и CrewAI, а также memory-слой CrewAI и Agno не разобраны.

## Вопрос 2. Состояние дел в durable execution для агентов

### Takeaway
Сформировались две школы: снимок с курсором (LangGraph, MAF, OpenAI `RunState`) и журнал эффектов с replay (Temporal, Restate, DBOS, Chidori, события ADK), и они сближаются через дельта-чекпоинты. Вопрос идемпотентности инструментов нигде не решён автоматически: стандартная гарантия — at-least-once на шаге, и ответственность лежит на авторе инструмента. Новейший край — формальная проверка безопасности fork и restore.

### Cited Findings

#### Гранулярность чекпоинта

| Система | Единица записи | Курсор | Что происходит при resume |
|---|---|---|---|
| LangGraph | `StateSnapshot` на суперстеп | `next`, цепочка родителей, pending writes | перезапуск прерванного узла с начала |
| MAF | `WorkflowCheckpoint` на границе суперстепа | `iteration_count`, `previous_checkpoint_id` | восстановление сообщений, состояния, буферов fan-in, ожидающих запросов |
| OpenAI | сериализованный `RunState` | внутри состояния, корреляция по tool-call id | применить решения и продолжить |
| Pydantic AI, ядро | история сообщений + deferred-результаты | неявно в транскрипте | новый запуск по истории |
| Pydantic `StepPersistence` | снимок на границе завершённого tool-call | снимок | продолжить или fork |
| Agno | `runs` — только терминальные состояния; `tool-batch` — после каждой партии инструментов (K+1 записей) | индекс сообщения | `/continue` |
| CrewAI | по событиям `on_events`, дефолт `task_completed`, возможно `llm_call_completed` | lineage ID | пропуск завершённых задач |
| ADK | журнал событий сессии | сканирование событий | fast-forward завершённых, перезапуск прерванного |
| Temporal, Restate, DBOS | activity, `ctx.run`, step | журнал | replay до первого незавершённого шага |
| Chidori | каждый host call | журнал вызовов | replay журнала до паузы, затем живое выполнение |

Источники: таблица сравнения ADK — [architecture-workflow-resumability](https://github.com/google/adk-python/blob/HEAD/.agents/skills/adk-architecture/references/architecture-workflow-resumability.md); [_checkpoint.py](https://github.com/microsoft/agent-framework/blob/HEAD/python/packages/core/agent_framework/_workflows/_checkpoint.py); [persistence.md Pydantic](https://github.com/pydantic/pydantic-ai/blob/HEAD/docs/persistence.md); [Agno checkpointing](https://github.com/agno-agi/agno/blob/HEAD/cookbook/02_agents/18_checkpointing/README.md); [CrewAI checkpointing](https://github.com/crewAIInc/crewAI/blob/HEAD/docs/edge/en/concepts/checkpointing.mdx); [Restate durable agents](https://docs.restate.dev/ai/patterns/durable-agents); [DBOS в Pydantic AI](https://github.com/pydantic/pydantic-ai/blob/HEAD/docs/durable_execution/dbos.md); [Chidori](https://github.com/ThousandBirdsInc/chidori/blob/HEAD/README.md).

#### Режимы и детали
- LangGraph `Durability = Literal["sync","async","exit"]`: запись до следующего шага, параллельно со следующим шагом, только при выходе — [types.py](https://github.com/langchain-ai/langgraph/blob/HEAD/libs/langgraph/langgraph/types.py).
- `DeltaChannel` (beta) хранит в блобе чекпоинта только sentinel и восстанавливает значение replay записей предков через редьюсер; редьюсер обязан быть детерминированным и инвариантным к разбиению на батчи — [delta.py](https://github.com/langchain-ai/langgraph/blob/HEAD/libs/langgraph/langgraph/channels/delta.py). Это гибрид снимка и журнала.
- Дельта-чекпоинты заявлены и в adk-rust — [README](https://github.com/zavora-ai/adk-rust/blob/HEAD/README.md).
- `langgraph-checkpoint-conformance` проверяет контракт бэкенда: round-trip блобов, метаданные, изоляцию namespace — [README](https://github.com/langchain-ai/langgraph/blob/HEAD/libs/checkpoint-conformance/README.md).
- MAF: чекпоинт привязан к определению через `graph_signature_hash` и может восстанавливаться другим экземпляром; содержит только зафиксированное состояние — [_checkpoint.py](https://github.com/microsoft/agent-framework/blob/HEAD/python/packages/core/agent_framework/_workflows/_checkpoint.py). Durable Task и Azure Functions вынесены в [отдельный репозиторий](https://github.com/microsoft/agent-framework/blob/HEAD/docs/features/durable-agents/README.md).
- Pydantic AI формулирует главное различие: "Durability is not storage"; движок журналирует запросы к модели и обёрнутые вызовы инструментов, при рестарте переигрывает завершённые и повторяет выполнявшийся; сохранение и fork движка не требуют — [overview](https://github.com/pydantic/pydantic-ai/blob/HEAD/docs/durable_execution/overview.md). Девять движков, плюс builder с тирами `CallableOperationBackend` и `RegisteredOperationBackend` — [backends](https://github.com/pydantic/pydantic-ai/blob/HEAD/docs/durable_execution/backends.md). Второй движок на агенте вызывает `UserError`.
- Mastra: resumable streams "не делают запуск durable" — при падении процесса запуск останавливается — [resumable-streams](https://github.com/mastra-ai/mastra/blob/HEAD/docs/src/content/en/docs/agents/resumable-streams.mdx). `createDurableAgent` в beta, цикл агента запускается внутри workflow, события идут через PubSub — [durable-agents](https://github.com/mastra-ai/mastra/blob/HEAD/docs/src/content/en/docs/harness/durable-agents.mdx).
- LlamaIndex: состояние определено на границе шага, `Context.to_dict()`/`from_dict()`, есть пакет `llama-agents-dbos` — [durable_workflows](https://github.com/run-llama/llama-agents/blob/HEAD/docs/src/content/docs/llamaagents/workflows/durable_workflows.md).
- CrewAI: авто-запись чекпоинта best-effort: ошибка логируется, запуск продолжается — [checkpointing](https://github.com/crewAIInc/crewAI/blob/HEAD/docs/edge/en/concepts/checkpointing.mdx).
- Temporal: LLM-вызовы и инструменты как Activities в рецептах; большие payload выносятся из Event History через Claim Check; approval через Signals; есть рецепты для OpenAI Agents SDK, Google ADK, Vercel AI SDK и Strands — [AI Cookbook](https://docs.temporal.io/ai-cookbook), [durable-agent-with-tools](https://docs.temporal.io/ai-cookbook/durable-agent-with-tools).
- Rust-доступ к движкам есть: [temporalio/sdk-rust](https://github.com/temporalio/sdk-rust) (v1.1.2), [restatedev/sdk-rust](https://github.com/restatedev/sdk-rust) (94 звезды).
- Vercel `WorkflowAgent` и durable `generate()` — [вторичный обзор](https://www.developersdigest.tech/blog/vercel-ai-sdk-whats-new-2026), первоисточник не проверялся.

#### Time travel и fork
- Mastra `timeTravel()` перезапускает с любого шага; при изменённом определении падает с понятной ошибкой до выполнения, снимок не трогается — [time-travel](https://github.com/mastra-ai/mastra/blob/HEAD/docs/src/content/en/docs/workflows/time-travel.mdx).
- Agno: `continue_from` (`end`, `last_user`, индекс K), завершённые запуски автоматически форкаются, граница привязывается к tool-call-безопасному индексу, чтобы не оставить вызов без результата — [time_travel](https://github.com/agno-agi/agno/blob/HEAD/cookbook/02_agents/20_time_travel/README.md).
- MAF заявляет time-travel в workflows — [README](https://github.com/microsoft/agent-framework/blob/HEAD/README.md). CrewAI: fork под новым lineage — [checkpointing](https://github.com/crewAIInc/crewAI/blob/HEAD/docs/edge/en/concepts/checkpointing.mdx).

#### Идемпотентность и исследования
- LangGraph: неидемпотентные операции ставить после прерывания или в отдельный узел; даже там повтор после частичного сбоя возможен, поэтому внешние записи должны быть идемпотентны или дедуплицироваться — [Interrupts](https://docs.langchain.com/oss/python/langgraph/interrupts).
- ADK: resume best-effort и at-least-once; "пауза перезапускает единицу с начала, что перекладывает контракт идемпотентности на автора узла" — [architecture-workflow-resumability](https://github.com/google/adk-python/blob/HEAD/.agents/skills/adk-architecture/references/architecture-workflow-resumability.md).
- Restate заявляет "executed exactly once" для побочных эффектов инструментов — [durable agents](https://docs.restate.dev/ai/patterns/durable-agents). Шаг с сохранённым результатом делает тривиальной генерацию стабильного idempotency key — [What is Durable Execution](https://restate.dev/what-is-durable-execution).
- "When Can Agents Safely Checkpoint, Fork, Restore, and Merge?" (arXiv 2608.22928, август 2026): правка выполнения не может отменить выданную авторизацию или уже отправленный запрос; агент считается недоверенным; алгоритм точно решает безопасность правки и выдаёт все безопасные продолжения либо проверяемое доказательство их отсутствия; покрыты checkpoint и шесть форм fork/restore/merge; механизация в Lean — [arXiv](https://arxiv.org/abs/2608.22928). Читался только абстракт.
- AgentRewind (arXiv 2608.14380): чекпоинты, совмещающие контекст агента и управляемое окружение; откат как действие самого агента с "rewind memory" — [arXiv](https://arxiv.org/abs/2608.14380). Цифры 87.8% против 78.0% взяты только из сниппета поиска — [PDF](https://arxiv.org/pdf/2608.14380). Продолжение и критика опоры на инициативу агента — [arXiv 2609.18304](https://arxiv.org/pdf/2609.18304).

Противоречие: ADK утверждает, что ни один мейнстримный фреймворк не восстанавливается replay всей истории — [источник](https://github.com/google/adk-python/blob/HEAD/.agents/skills/adk-architecture/references/architecture-workflow-resumability.md). Это верно для перечисленных четырёх, но Pydantic AI на движках и Chidori восстанавливаются именно replay журнала, а `DeltaChannel` — частичным replay. Кроме того, ADK признаёт, что `Workflow` пишет чекпоинт, но не читает его (`TODO: resume from checkpoint event`).

### Inferences
- Для встраиваемого локального агента на SQLite лучшая схема: журнал эффектов как источник истины и снимки как оптимизация чтения. Журнал даёт бесплатно replay, стрим UI, трассировку и evals, а снимок ограничивает стоимость resume, чего не хватает ADK.
- Внешний движок (Temporal, Restate) не нужен: он противоречит требованию одного бинарника на SQLite. Абстракцию над движками, как в Pydantic AI, делать не надо; единственный движок — свой журнал.
- При локальном WAL-SQLite стоимость синхронной записи мала, поэтому три режима durability избыточны: достаточно синхронной записи с group commit. Замеры нужны на этапе реализации.
- Заявление "exactly once" корректно только для наблюдаемого журнала. Если процесс умер между внешним эффектом и записью результата, шаг выполнится повторно. Честная модель: журналирование намерения до выполнения и квитанции после; при восстановлении "намерение без квитанции" для `NonIdempotent` — это не автоповтор, а прерывание к человеку или сверка.
- Ключ идемпотентности выводить из `(run_id, step_id, effect_seq)`, а не из данных модели, и журналировать, чтобы он пережил повтор.
- Откат контекста без отката окружения небезопасен. Для агента, работающего в каталоге проекта, чекпоинт должен ссылаться на снимок рабочего дерева (скрытый git-ref), как предлагает AgentRewind.
- Fork должен наследовать журнал вплоть до точки ветвления и отмечать внешние эффекты как уже совершённые. Полный решатель из arXiv 2608.22928 сейчас избыточен, но модель "авторизация и отправленный запрос необратимы" стоит заложить в схему журнала.

### Gaps
- Основная документация LangGraph по durable-execution не получена: URL `oss/python/langgraph/durable-execution` вернул обзор persistence; режимы взяты из исходников. Полное поведение `async` при падении не проверялось.
- Страница Temporal не описывает детерминизм и Continue-As-New в просмотренной части; лимиты Event History не проверены.
- Версионирование определения workflow при replay журнала (Temporal patching и аналоги) не изучено; из проверенного только `graph_signature_hash` и отказ Mastra.
- Независимых бенчмарков накладных расходов чекпоинтов не найдено.
- Вторичные обзоры (zylos.ai, frenchydigital и др.) не использованы как факты.

## Вопрос 3. Какой стандартный поток событий агент-UI победил

### Takeaway
Единого победителя нет, рынок расслоился по поверхностям. AG-UI стал нейтральным стандартом для веб-фронтендов поверх разных фреймворков, Vercel UI Message Stream доминирует в стеке React/Next, ACP (Zed/JetBrains) победил в редакторах, A2A и MCP закрывают агент-агент и агент-инструмент. Для zero-agent: своя внутренняя шина событий, AG-UI как основной внешний формат для web UI, адаптеры по образцу `UIAdapter`.

### Cited Findings
- AG-UI: 17k звёзд, `@ag-ui/core` 1.0.4, MIT — [репозиторий](https://github.com/ag-ui-protocol/ag-ui), [package.json](https://github.com/ag-ui-protocol/ag-ui/blob/HEAD/sdks/typescript/packages/core/package.json). События: Lifecycle (`RunStarted`, `RunFinished` с `outcome`, `RunError`, `Step*`), Text, Tool Calls с дельтами аргументов и `ToolCallResult`, State (`StateSnapshot`, `StateDelta` на RFC 6902 JSON Patch, `MessagesSnapshot`), Activity, Reasoning (включая зашифрованное значение), Subagents, `Raw`, `Custom` — [events](https://docs.ag-ui.com/concepts/events).
- Прерывания AG-UI: терминальная модель, запуск заканчивается исходом interrupt, клиент начинает новый запуск с ответами на каждое прерывание — [interrupts](https://github.com/ag-ui-protocol/ag-ui/blob/HEAD/docs/concepts/interrupts.mdx).
- Заявленная поддержка: LangGraph, CrewAI, MAF, Google ADK, Mastra, Pydantic AI, Agno, Claude Agent SDK, Cloudflare Agents, Oracle Agent Spec, A2A — [README](https://github.com/ag-ui-protocol/ag-ui/blob/HEAD/README.md). Проверка по деревьям файлов: нативный код с `ag-ui` в путях есть в MAF (328 путей), Agno (54) и Pydantic AI; в деревьях LangGraph, CrewAI, Mastra, ADK, OpenAI, Dify таких путей нет, их интеграции живут снаружи (подсчёт по именам файлов, 2026-11-06). OpenAI Agents SDK в июне был "в работе" — [Thesys](https://www.openui.com/blog/state-of-generative-ui-report); источник заинтересован, Thesys продвигает OpenUI.
- MAF оформил выбор AG-UI отдельным ADR — [ADR-0010](https://github.com/microsoft/agent-framework/blob/HEAD/docs/decisions/0010-ag-ui-support.md).
- Rust-SDK AG-UI: только community, крейты `ag-ui-core` и `ag-ui-client`, API клиента ещё в работе — [README](https://github.com/ag-ui-protocol/ag-ui/blob/HEAD/sdks/community/rust/crates/ag-ui-client/README.md).
- Vercel: SSE-протокол UI Message Stream, заголовок `x-vercel-ai-ui-message-stream: v1`, части text/reasoning/tool-input/source/file/data/custom — [stream protocol](https://github.com/vercel/ai/blob/HEAD/content/docs/04-ai-sdk-ui/50-stream-protocol.mdx). AI SDK 7 вышел 2026-06-25; добавлены `WebSocketChatTransport`, `steer()`, `queue()` — [вторичный обзор](https://www.developersdigest.tech/blog/vercel-ai-sdk-whats-new-2026). Более 16 млн загрузок в неделю, AG-UI подключается только внешней трансляцией — [обзор по сниппету поиска](https://vercel.com/kb/guide/vercel-ai-sdk-vs-tanstack-ai), страница не открывалась.
- Pydantic AI нативно поддерживает оба протокола через абстрактный `UIAdapter` — [ui/overview](https://github.com/pydantic/pydantic-ai/blob/HEAD/docs/ui/overview.md). Mastra держит `client-sdks/ai-sdk` — [дерево](https://github.com/mastra-ai/mastra/tree/HEAD/client-sdks/ai-sdk).
- Thoughtworks в апреле 2026 поставил AG-UI на "Trial" и усомнился в необходимости отдельного слоя — [вторичный пересказ](https://www.kamrancode.com/blog/ag-ui-explained-what-it-is-and-how-it-differs-from-mcp); первоисточник не открывался.
- Собственные протоколы: LangGraph `ProtocolEvent` с `seq`, `method`, `namespace` на wire-формате `langchain-protocol`, `stream_events(version="v3")` — [stream/_types.py](https://github.com/langchain-ai/langgraph/blob/HEAD/libs/langgraph/langgraph/stream/_types.py), [Interrupts](https://docs.langchain.com/oss/python/langgraph/interrupts). CrewAI `StreamFrame` с `id`, `seq`, `type` — [контракт](https://github.com/crewAIInc/crewAI/blob/HEAD/docs/edge/en/learn/streaming-runtime-contract.mdx). Codex `app-server` — [README](https://github.com/openai/codex/blob/HEAD/codex-rs/app-server/README.md).

Три разных "ACP":
- Agent Client Protocol (Zed): редактор ↔ кодинг-агент, стабильная версия протокола 1, схема v1.29.1, родной Rust-крейт — [репозиторий](https://github.com/agentclientprotocol/agent-client-protocol). ACP Registry с января 2026 встроен в JetBrains IDE и Zed — [gihyo](https://gihyo.jp/article/2026/01/acp-registry); Junie, Claude Agent, Codex доступны через него в JetBrains — [JetBrains](https://blog.jetbrains.com/ja/blog/2026/02/19/acp-agent-client-protocol-jetbrains-ide/).
- IBM Agent Communication Protocol: заархивирован, "ACP is now part of A2A under the Linux Foundation" — [i-am-bee/acp](https://github.com/i-am-bee/acp).
- Agentic Commerce Protocol: не про UI; реализован в adk-rust — [README](https://github.com/zavora-ai/adk-rust/blob/HEAD/README.md).

Соседние стандарты:
- MCP 2026-07-28: stateless-ядро, `initialize` и `Mcp-Session-Id` убраны; MRTR заменяет серверные `elicitation/create`, `sampling/createMessage`, `roots/list`; Tasks, MCP Apps и EMA стали расширениями; Roots, Sampling, Logging и HTTP+SSE устарели с окном 12 месяцев; Tier 1 — TypeScript, Python, Go, C#, а Rust на момент релиза "supports the new spec in beta" — [спецификация](https://blog.modelcontextprotocol.io/posts/2026-07-28/). `rmcp` v3.9.0 от 2026-11-05 — [rust-sdk](https://github.com/modelcontextprotocol/rust-sdk).
- A2A: v1.0 — первая стабильная спецификация — [LF](https://www.linuxfoundation.org/press/a2a-protocol-surpasses-150-organizations-lands-in-major-cloud-platforms-and-sees-enterprise-production-use-in-first-year); v1.0.1 от 2026-05-28 — [releases](https://github.com/a2aproject/A2A/releases); принят в AAIF как Growth Stage рядом с MCP, goose, AGENTS.md — [блог A2A](https://a2a-protocol.org/latest/blog/2026/08/27/a-new-chapter-for-a2a-joining-the-agentic-ai-foundation/). Официальный Rust-SDK `a2a-rs` (крейт `a2a-lf`) — [репозиторий](https://github.com/a2aproject/a2a-rs).
- Встроенный UI от агента: MCP Apps и A2UI — [Thesys](https://www.openui.com/blog/state-of-generative-ui-report), заинтересованный источник.

Матрица в деревьях репозиториев (число путей, признак наличия, а не качества): A2A — CrewAI 200, MAF 152, ADK 134, Mastra 44, Agno 41; в LangGraph, OpenAI и Dify ноль. MCP — есть везде, кроме дерева LangGraph (0).

### Inferences
- Разумная архитектура zero-agent: типизированный внутренний `AgentEvent` записывается в журнал, а адаптеры проецируют его в AG-UI для web UI, Vercel-стрим (опционально), ACP для редакторов, A2A для внешних агентов.
- AG-UI подходит лучше всего как основной внешний формат: семантика прерываний совпадает с рекомендуемой моделью паузы, а `StateDelta` и `MessagesSnapshot` дают переподключение. Но официального серверного Rust-SDK нет, значит, wire-типы надо написать самим или взять `ag-ui-core` после оценки.
- Важен разрыв доставки и исполнения, описанный у Mastra: при наличии журнала с `seq` возобновление потока сводится к чтению `events WHERE seq > last_seen`. Это даёт durable и resumable streams одним механизмом.
- `zero-agent-relay` логично строить на семантике A2A (agent card, задачи), а не изобретать свой протокол. Это проектная рекомендация, совместимость A2A с relay-доступом к проектам надо проверить.
- MCP-клиент zero-agent должен сразу поддерживать MRTR и не проектироваться вокруг долгоживущей сессии; Sampling закладывать не следует.

### Gaps
- Надёжной независимой статистики доли AG-UI по сравнению с Vercel нет: цифры поддержки самоотчётные, сравнимых метрик использования не найдено.
- Точная дата A2A v1.0 расходится по вторичным источникам (релиз март, пресс-релиз LF 9 апреля); первичного тега v1.0.0 не проверял.
- Соответствие крейта `rmcp` 3.9 финальной спецификации 2026-07-28 не проверялось.

## Вопрос 4. Что говорит autogen discussion 7066 и что из этого следует

### Takeaway
Обсуждение подтверждено: 2025-10-02 Microsoft объявила слияние AutoGen и Semantic Kernel в Microsoft Agent Framework с новым графовым workflow API. Обещанное там сопровождение критических исправлений не удержалось: с октября 2026 README называет проект архивным и без security-исправлений. Вывод: групповой чат не является основой, основа — граф и журнал, а паттерны — библиотека поверх них.

### Cited Findings
- Discussion 7066 "AutoGen Update", автор ekzhu (Eric Zhu), категория Announcements, 2025-10-02, 9 голосов, 4 комментария верхнего уровня — [discussion](https://github.com/microsoft/autogen/discussions/7066).
- Содержание: за два года 98 релизов, 3776 коммитов, 2488 закрытых issue, 50.4k звёзд, 559 контрибьюторов. AutoGen и Semantic Kernel сливаются в MAF; одноагентный интерфейс "почти идентичен" AutoGen с добавлением управления потоком разговора, middleware и hosted tools. Главное изменение — новый граф-ориентированный workflow API, поверх которого построены sequential, parallel, Magentic и другие паттерны — [discussion](https://github.com/microsoft/autogen/discussions/7066).
- Обещание: "AutoGen will still be maintained... critical bug fixes and security patches", но без значительных новых функций — [discussion](https://github.com/microsoft/autogen/discussions/7066).
- Комментарии: вопрос о форке сообщества от operation-hp остался без ответов; благодарности; нерелевантный "whitepaper" и малосодержательный пост (шум) — [discussion](https://github.com/microsoft/autogen/discussions/7066).
- Текущее состояние: README "Project archived and no longer maintained... not providing bug fixes, security fixes, new features, or support"; изменение внесено коммитом от 2026-10-16 (#8366), предыдущий баннер maintenance от 2026-04-06 (#7521) — [README](https://github.com/microsoft/autogen/blob/HEAD/README.md), [история](https://github.com/microsoft/autogen/commits/HEAD/README.md). Последний релиз python-v0.7.5 от 2025-09-30.
- Расхождение: GitHub API показывает `archived=false`, последний push 2026-10-28 (проверено 2026-11-06), то есть архивность заявлена в README, но GitHub-архивирование не включено.
- MAF развился: 1.0 в апреле 2026, 53+ ADR, включая FIDES, CodeAct, навыки, хостинг — [docs/decisions](https://github.com/microsoft/agent-framework/tree/HEAD/docs/decisions).

### Inferences
- Подтверждён тезис "оркестрационные паттерны не примитив". Самый известный чат-ориентированный фреймворк перешёл на Pregel-подобный граф; LangGraph, ADK 2.0, MAF и Mastra пришли к той же точке.
- Ставка на чужой фреймворк рискованна: за год обещание сопровождения сократилось до статуса архива. Это подтверждает решение не зависеть от них, а также зависеть от протоколов (MCP, A2A), а не от SDK.
- В анонсе к интерфейсу AutoGen добавлены управление потоком разговора, middleware и hosted tools. Значит, эти части лучше закладывать сразу, а не добавлять после ядра оркестрации.
- Совместимость с AutoGen для zero-agent не нужна.

### Gaps
- Судьба форка сообщества из этого обсуждения не прослежена; AG2 упомянут в [обзоре Thesys](https://www.openui.com/blog/state-of-generative-ui-report), но его связь с AutoGen здесь не проверялась.
- Полный текст [migration guide](https://aka.ms/autogen-to-af) и блог-анонс не читались.

## Вопрос 5. Рекомендация: минимальный набор примитивов для Rust-ядра

### Takeaway
Рекомендуется восемь примитивов вокруг одного журнала на SQLite: `Effect`, `Journal`, `Node/Graph`, `Interrupt`, `Tool`, `Capability`, `Model`, `AgentEvent`. Типизированный `Deps` идёт через дженерики. Цикл агента, handoff, sub-agent, память, guardrails, evals — код поверх примитивов, а не части ядра. Весь раздел является инженерной рекомендацией на основе фактов выше, а не проверенным решением.

### Cited Findings
Ключевые опоры рекомендации:
- Граница эффектов — [Chidori](https://github.com/ThousandBirdsInc/chidori/blob/HEAD/README.md), журнал — [Restate](https://docs.restate.dev/ai/patterns/durable-agents), [ADK](https://github.com/google/adk-python/blob/HEAD/.agents/skills/adk-architecture/references/architecture-workflow-resumability.md).
- Снимок как источник resume — [ADK Direction](https://github.com/google/adk-python/blob/HEAD/.agents/skills/adk-architecture/references/architecture-workflow-resumability.md).
- Объявление `NonIdempotent` — [adk-graph](https://github.com/zavora-ai/adk-rust/blob/HEAD/adk-graph/README.md).
- Модель паузы — [AG-UI](https://github.com/ag-ui-protocol/ag-ui/blob/HEAD/docs/concepts/interrupts.mdx), [MCP MRTR](https://blog.modelcontextprotocol.io/posts/2026-07-28/), [Pydantic Deferred](https://github.com/pydantic/pydantic-ai/blob/HEAD/docs/deferred-tools.md).
- Capability — [Pydantic](https://github.com/pydantic/pydantic-ai/blob/HEAD/docs/capabilities/overview.md); action-хуки — [rig](https://github.com/0xPlaygrounds/rig/blob/HEAD/crates/rig-agent/src/agent/hook.rs).
- Совместимость чекпоинта с определением — [MAF](https://github.com/microsoft/agent-framework/blob/HEAD/python/packages/core/agent_framework/_workflows/_checkpoint.py).
- Крейты протоколов: [rmcp](https://github.com/modelcontextprotocol/rust-sdk), [a2a-rs](https://github.com/a2aproject/a2a-rs), [agent-client-protocol](https://github.com/agentclientprotocol/rust-sdk).

### Inferences

#### Набор примитивов (только в ядре)

```rust
// 1. Effect: единственный шлюз к недетерминизму (LLM, инструмент, время, случайность, сеть).
//    Повтор возвращает записанный результат.
async fn effect<T: Serialize + DeserializeOwned>(&self, e: EffectSpec) -> Result<T>;

enum EffectClass { Pure, ReadOnly, Idempotent, NonIdempotent }

// 2. Journal: append-only лог, источник истины. UI, трассы, evals — его проекции.
struct JournalEntry { run_id, seq: u64, ns: Vec<String>, parent_seq, ts, kind: EntryKind, payload }

// 3. Node и Graph: явная машина состояний вместо Pregel-каналов.
#[async_trait]
trait Node<S, D> { async fn run(&self, cx: &Ctx<D>, s: &S) -> Result<Transition<S>>; }

enum Transition<S> {
    Goto(NodeId, Patch<S>),
    Fan(Vec<(NodeId, Patch<S>)>, JoinId),
    Suspend(Vec<Interrupt>, Patch<S>),
    Done(Patch<S>),
}

// 4. Interrupt: терминальный результат запуска, resume — новый вызов с ответами по id.
struct Interrupt { id: InterruptId, kind: Kind, payload: Value, response_schema: Schema, expires: Option<Ts> }
enum Kind { Approval, Input, ExternalResult, Timer, UnknownOutcome }

// 5. Tool: схема из типа, валидация serde + JSON Schema, класс эффекта обязателен.
trait Tool<D> {
    type Args: DeserializeOwned + JsonSchema;
    type Output: Serialize;
    const EFFECT: EffectClass;
    fn approval(&self, cx: &Ctx<D>, a: &Self::Args) -> ApprovalRule; // fail-closed
    async fn call(&self, cx: &Ctx<D>, a: Self::Args) -> Result<Self::Output, ToolError>;
}

// 6. Capability: тулсет + хуки + инструкции + настройки. Память, guardrails, компакция,
//    учёт бюджета живут здесь.
trait Capability<D> {
    fn tools(&self) -> Vec<DynTool<D>>;
    fn instructions(&self, cx: &Ctx<D>) -> Option<String>;
    async fn on_hook(&self, cx: &Ctx<D>, h: Hook<'_>) -> Action;
}
enum Action { Allow, Block(Reason), Rewrite(Value), Retry(Hint), RequireApproval }

// 7. Model: нормализованный стрим; режим вывода явный.
trait Model { async fn stream(&self, r: ModelRequest) -> BoxStream<ModelEvent>; fn caps(&self) -> ModelCaps; }
enum OutputMode { Tool, Native, Prompted, Grammar } // Grammar для локальных моделей

// 8. AgentEvent: внутренняя шина с адаптерами AG-UI, ACP, A2A, Vercel.
trait EventAdapter { type Wire; fn project(&mut self, e: &JournalEntry) -> Vec<Self::Wire>; }

// Deps: только дженерик. Не сериализуется, собирается заново при resume.
struct Agent<D, O> { /* model, capabilities, output: PhantomData<O> */ }
```

#### Схема SQLite (минимум)

- `runs(id, parent_run, fork_seq, graph_hash, status, ...)`: fork — это новая строка с `parent_run` и `fork_seq`.
- `journal(run_id, seq, ns, kind, payload, PRIMARY KEY(run_id, seq))`.
- `effects(run_id, effect_key UNIQUE, class, intent_seq, receipt_seq, blob_ref)`.
- `snapshots(run_id, seq, state)`: оптимизация чтения, можно удалить и пересобрать.
- `interrupts(id, run_id, kind, status, response)`.
- `blobs(hash, data)`: большие payload вне журнала, аналог Claim Check.

#### Семантика

1. Гарантия: at-least-once на шаг, завершённые вызовы при replay не выполняются повторно, а берутся из журнала. Обещать exactly-once нельзя.
2. Протокол эффекта: запись намерения → выполнение → запись квитанции. Намерение без квитанции при восстановлении: `Pure`, `ReadOnly`, `Idempotent` повторяются, `NonIdempotent` порождает `Interrupt(UnknownOutcome)`.
3. Запись синхронна (WAL с group commit). Режимы `async/exit` не вводить, пока замеры не покажут необходимость.
4. Resume: загрузка снимка, replay хвоста журнала, запуск незавершённого узла. Узел перезапускается с начала, но его завершённые эффекты отдаются из журнала, чем снимается главная боль LangGraph с кодом до `interrupt`.
5. ID прерывания детерминированно выводится из `(run, node, effect_seq)`; позиционного сопоставления нет.
6. Проверка `graph_hash` при resume: несовпадение — явный отказ, если нет явного разрешения.
7. Fork наследует журнал до `fork_seq`, граница привязывается к tool-call-безопасной точке, внешние эффекты помечены как совершённые, рабочее дерево восстанавливается из снимка.
8. Чекпоинт — это данные: никаких путей к коду или callback в журнале.
9. Conformance-набор для `Store`-трейта с первого дня, даже при одном бэкенде.

#### Что собирается поверх ядра

| Функция | Реализация |
|---|---|
| Цикл агента | встроенный `Graph` из узлов `model` и `tools` |
| Handoff | инструмент, возвращающий `Transition::Goto`, с фильтром истории |
| Sub-agent | `Agent` как `Tool` или как вложенный `Node` с собственным `ns` |
| Параллельный research | `Transition::Fan` и join с редьюсером |
| Guardrails | `Capability` на трёх уровнях: ввод, инструмент, вывод; для `NonIdempotent` блокирующие |
| Память | `Capability` над ZeroAIMemory; журнал не равен памяти |
| Компакция | `Capability`, в духе Observational Memory, особенно для малых контекстов |
| Периодические задачи | durable timer как `Interrupt(Timer)` плюс планировщик |
| Трассировка | проекция журнала в OTel с атрибутами `gen_ai.*` за фича-флагом; конвенции ещё Development, значит, маппинг изолировать |
| Evals и арена | кассеты и replay журнала, бенчмарки как runner над `Agent` |
| Протоколы | отдельные крейты-адаптеры |

#### Что не делать

- Pregel-каналы во всём многообразии: один тип состояния с `Patch` и редьюсером на join покрывает потребность.
- GroupChat, роли и "команды" как примитивы.
- YAML/DSL и визуальный редактор в ядре.
- Абстракцию над Temporal, DBOS и прочими движками.
- Три режима durability на старте.
- Параллельные guardrails по умолчанию.
- Статические `interrupt_before/after` как механизм HITL.
- Отдельный класс "Memory" для истории диалога.
- Собственную схему трассировки как единственную.
- ECS-ядро.
- Вызов `Sampling` из MCP-серверов (устарел).

#### Порядок реализации и зависимости

1. `Journal`, `Store` на SQLite, `Effect`, conformance-тесты.
2. `Model` (начать с OpenAI-совместимого протокола и локальных моделей), `Tool`, встроенный цикл.
3. `Interrupt` и resume, затем `Capability`.
4. `AgentEvent` и адаптер AG-UI, затем web UI.
5. `Graph`, fork, снимки рабочего дерева.
6. MCP-клиент на `rmcp`, relay на семантике A2A, ACP.

По зависимостям: брать протокольные крейты (`rmcp`, `a2a-lf`, `agent-client-protocol`), а не фреймворки. `rig-core` рассматривать только как необязательный слой провайдеров за своим трейтом `Model`: он остаётся 0.x и проект сместился в сторону кодинг-агента на Bevy.

### Gaps
- Эргономика `effect()` в Rust: дженерики плюс сериализуемость плюс `async_trait`, возможны макросы; нужен прототип. Стирание типов `Tool<D>` до `DynTool<D>` потребует отдельного решения.
- Нет данных о производительности SQLite-журнала на нагрузке параллельного research; нужен замер перед отказом от `async`-режима.
- Формат снимка рабочего дерева для больших проектов (скрытый git-ref против CoW) не исследован.
- Точность Prompted/Grammar на локальных моделях не оценена; относится к соседнему исследованию локальных моделей.
- Как использовать ZeroAIMemory через `Capability`, зависит от API проекта, который здесь не изучался.
