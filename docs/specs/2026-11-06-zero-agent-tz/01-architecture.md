# ТЗ zero-agent. Часть 01. Архитектура и эксплуатация

## 1. Обзор компонентов

```text
                    браузер пользователя (SPA, Svelte 5)
                          │ HTTPS/HTTP loopback: SSE, POST, WS (терминал)
┌─────────────────────────▼──────────────────────────────────────────────┐
│ zero-agent (daemon)                                                    │
│  ┌──────────┐ ┌────────────┐ ┌───────────┐ ┌──────────┐ ┌───────────┐  │
│  │ za-server│ │ control    │ │ scheduler │ │ za-peer  │ │ za-arena  │  │
│  │ API, UI  │ │ router     │ │ jobs      │ │ сеть     │ │ экспорт   │  │
│  └────┬─────┘ └─────┬──────┘ └─────┬─────┘ └────┬─────┘ └─────┬─────┘  │
│       └─────────────┴──────┬───────┴────────────┴─────────────┘        │
│  ┌─────────────────────────▼───────────────────────────────────────┐   │
│  │ za-core: runs, журнал, эффекты, граф, capability, шина          │   │
│  └──┬──────────┬──────────┬───────────┬─────────┬─────────┬────────┘   │
│  za-llm    za-tools   za-memory    za-mcp   za-skills  za-browser      │
│     │          │          │           │                                │
│     │     za-jail (запуск процессов)                                   │
│     │          │          │           │                                │
└─────┼──────────┼──────────┼───────────┼────────────────────────────────┘
      │          │          │           │
 рантайм LLM  дочерние   zam-server    MCP-серверы
 llama.cpp,   процессы   (внешний      (процессы)    
 vLLM, облако            сервис) ──── Qdrant

                                         ▲
 другие zero-agent ◄──WSS, токен──► zero-agent-relay
```

**Принцип.** Цикл агента не имеет собственных путей к файлам и сети: всё идёт через `Tool` и `Effect`, поэтому всё попадает в журнал. Секреты остаются в daemon.

## 2. Процессы и бинарники

| Бинарник | Роль | Где работает |
|---|---|---|
| `zero-agent` | Daemon, CLI-клиент (`zero-agent run`, `doctor`) | Служба Windows, systemd unit или обычный процесс |
| `zero-agent-relay` | Сервис связи агентов | Любая машина с публичным адресом |
| `za-desktop-helper` | Управление рабочим столом | Сессия пользователя (только при включении, P2) |

Требования:

| ID | Требование | Приор. |
|---|---|---|
| ARC-001 | Один самодостаточный бинарник `zero-agent` со встроенным UI (`memory-serve`, как в ZAM); запасной вариант `rust-embed` | P0 |
| ARC-002 | Платформы: Windows x64, Linux x64. Статическая сборка `x86_64-unknown-linux-musl` для установки в контейнеры арены | P0 |
| ARC-003 | Без Node, Python и Docker в рантайме ядра. Необязательные зависимости (Node для Playwright MCP, uv) ставятся управляемо и выключаются | P0 |
| ARC-004 | Режимы запуска: фоновая служба и явный запуск из терминала; поведение ядра одинаковое | P0 |
| ARC-005 | Несколько проектов в одном daemon, каждый со своими runs и решением ZAM | P0 |
| ARC-006 | Падение и перезапуск daemon не теряют ни одного подтверждённого события | P0 |

## 3. Структура workspace

Workspace по образцу `codex-rs`: протокол отдельно от ядра, запуск процессов за отдельным трейтом.

| Крейт | Ответственность | Зависит от |
|---|---|---|
| `za-protocol` | Типы событий, команд, API, версионируются отдельно | — |
| `za-store` | SQLite: писатель, читатели, миграции, conformance-набор | `za-protocol` |
| `za-core` | Run, журнал, эффекты, граф, прерывания, capability, шина, control plane | `za-store`, `za-llm` (трейт) |
| `za-llm` | Провайдеры, профили, нормализация, токены | `za-protocol` |
| `za-tools` | Нативные инструменты | `za-jail` |
| `za-jail` | Трейт `Runner`: запуск процессов, дерево, лимиты. Реализация без изоляции, точка расширения SEC-100 | — |
| `za-shellparse` | Разбор bash и PowerShell для правил и UX | — |
| `za-memory` | Клиент и шлюз ZeroAIMemory, только HTTP | `za-protocol` |
| `za-mcp` | Клиент и сервер MCP, инвентарь, пины | `rmcp` |
| `za-skills` | Обнаружение, активация, lock, установка | — |
| `za-browser` | Трейт `BrowserDriver`, сайдкар, CDP | `za-mcp` |
| `za-sched` | Задания, аренда, календарь | `za-store` |
| `za-peer` | Транспорт `PeerTransport`, доступ по проектам, потоки | `za-protocol` |
| `za-server` | HTTP API, SSE, WS, встроенный UI, адаптеры AG-UI | `za-core` |
| `za-adapters` | ACP, A2A, ATIF, OTLP | `za-protocol` |
| `za-arena` | Реестр прогонов, статистика, реплей | `za-protocol` |

**ARC-010 (P0).** Направление зависимостей проверяется в CI. `za-core` не зависит от `za-server`, `za-peer`, `za-browser`.

**ARC-011 (P0).** Ratchet размера: CI сравнивает строки `za-core` с записанным порогом. Рост порога фиксируется в журнале изменений с причиной.

### Стек

| Область | Выбор | Запасной вариант | Риск |
|---|---|---|---|
| Runtime, HTTP | `tokio`, `axum 0.8`, `tower-http` | — | axum 0.x |
| SQLite | `rusqlite` bundled, поток-писатель | `sqlx` | Нет проверки запросов на этапе компиляции |
| Миграции | Как в ZAM; `rusqlite_migration` как запасной | — | — |
| LLM-клиент | Свой тонкий `za-llm` на `reqwest` | `genai` | Поддержка своего кода |
| MCP | `rmcp` с пином версии | Свой тонкий клиент: stateless-ядро проще старого | Rust SDK в бете по ревизии 2026-07-28 |
| PTY | `portable-pty` | `process-wrap` без PTY | Редкие релизы |
| Git | Чтение `gix`, запись системным `git` | `git2` | gix 0.x |
| Diff, patch | `similar`, `diffy` | — | — |
| Парсинг | `tree-sitter`, bash, powershell | — | Пин как в ZAM |
| Поиск в файлах | `grep-searcher`, `ignore` | — | — |
| Cron | `croner` | `cron` | DST не проверен |
| Схемы | `schemars`, `jsonschema` | — | — |
| Логи | `tracing`; OTLP за feature-флагом | — | Конвенции GenAI в статусе Development |
| TLS | `rustls`, `rcgen` | — | — |
| Время | `jiff`, как в ZAM | — | — |
| Хэши | `blake3` | — | — |

**ARC-012 (P0).** Зависимости минимальны (N-13). Разрешены обычные библиотеки из crates.io и npm с закреплёнными версиями, в том числе протокольные (`rmcp`, `agent-client-protocol`) и системные (`mxc-sdk`). Запрещено: копировать код других агентов и харнессов, подключать их как submodule, `git =` или `path =`, зависеть от агентных фреймворков. Код ZeroAIMemory в сборку не подключается. CI проверяет `Cargo.lock`, лицензии и известные уязвимости (`cargo deny`, `cargo audit`). Новая зависимость вносится с записанной причиной.

## 4. Модель исполнения

### 4.1. Примитивы ядра

В ядре восемь примитивов (R03). Всё остальное собирается поверх них.

| Примитив | Назначение |
|---|---|
| `Effect` | Единственный шлюз к недетерминизму. Повтор возвращает записанный результат |
| `Journal` | Append-only лог, источник истины |
| `Node`/`Graph` | Явная машина состояний, переходы `Goto`, `Fan`, `Suspend`, `Done` |
| `Interrupt` | Терминальный результат шага: approval, ввод, внешний результат, таймер, неизвестный исход |
| `Tool` | Схема из типа, обязательный класс эффекта, правило одобрения |
| `Capability` | Тулсет, хуки, инструкции, настройки |
| `Model` | Нормализованный стрим, явный режим вывода |
| `AgentEvent` | Внутренняя шина с адаптерами |

Над ними как код, а не как примитивы: цикл агента, handoff, субагенты, рой, guardrails, память, компакция, бюджеты, задания.

**Не делать:** Pregel-каналы в полном объёме, GroupChat и роли как примитивы, DSL в ядре, абстракцию над внешними движками, режимы durability `async/exit` без замера.

### 4.2. Семантика надёжности

| ID | Требование | Приор. |
|---|---|---|
| CORE-001 | Гарантия at-least-once на шаг. Обещание exactly-once запрещено | P0 |
| CORE-002 | Протокол эффекта: запись намерения → выполнение → запись квитанции | P0 |
| CORE-003 | При восстановлении намерение без квитанции: классы `Pure`, `ReadOnly`, `Idempotent` повторяются; `NonIdempotent` порождает `Interrupt(UnknownOutcome)` | P0 |
| CORE-004 | Ключ идемпотентности выводится из `(run_id, step_id, effect_seq)` и журналируется | P0 |
| CORE-005 | Запись синхронная, WAL, group commit | P0 |
| CORE-006 | Resume: снимок + реплей хвоста + запуск незавершённого узла; завершённые эффекты узла отдаются из журнала | P0 |
| CORE-007 | ID прерывания выводится детерминированно, сопоставление не позиционное | P0 |
| CORE-008 | `graph_hash` проверяется при resume; несовпадение даёт явный отказ без явного разрешения | P1 |
| CORE-009 | Fork наследует журнал до `fork_seq`, граница ставится в точке, безопасной для пар вызов–результат, внешние эффекты помечены совершёнными, рабочее дерево восстанавливается из снимка | P1 |
| CORE-010 | Откат контекста без отката окружения запрещён: чекпоинт ссылается на снимок рабочего дерева | P1 |
| CORE-011 | Авторизация и отправленный внешний запрос необратимы, откат их не отменяет; модель видит это в `<state>` | P1 |
| CORE-012 | Чекпоинт содержит только данные, без путей к коду и callback | P0 |

### 4.3. Состояния run

```text
queued → running ⇄ paused → done | partial | blocked | failed | cancelled
             │  ↘ pausing ↗       ▲
             │  ↘ cancelling ─────┘
             └─ blocked_on_budget (выход через correct или resume)
```

Сбой провайдера, лимит и нехватка ресурса переводят run в `paused` с причиной, а не в `failed`.

## 5. Данные

### 5.1. Расположение

| Что | Где | Запись нативными инструментами |
|---|---|---|
| Глобальная БД, секреты, конфигурация агента, журналы | `ZA_HOME` (`%LOCALAPPDATA%\zero-agent`, `~/.local/share/zero-agent`) | Нет |
| Журнал и блобы проекта | `ZA_HOME/projects/<project_id>/` | Нет |
| Конфигурация проекта | `<project>/.zero-agent/config.toml`, `skills/`, `AGENTS.md` | Только чтение |
| Рабочие данные | `<project>/.zero-agent/work/` (spill, tools, browser, worktrees) | Чтение и запись; под `.gitignore` |
| Идентичность решения ZAM | `<project>/.zeroaimemory/solution.toml` | Только чтение |

**ARC-020 (P0).** Журнал хранится вне каталога проекта, чтобы он не попадал в git и не мешал работе. От команды shell это не защищает.

**ARC-021 (P0).** Ни один временный файл агента не создаётся в системных `%TEMP%` и `/tmp`. Дочерним процессам выставляются `TMP`, `TEMP`, `TMPDIR` внутрь `.zero-agent/work/tmp/<run>/`.

### 5.2. Схема SQLite (минимум)

Глобальная БД `agent.db`: `projects`, `peers`, `peer_acl`, `jobs`, `job_runs`, `grants`, `secrets_meta`, `models`, `model_profiles`, `mcp_servers`, `skills_inventory`, `arena_runs`.

БД проекта `journal.db`:

```sql
runs(id, project_id, parent_id, root_id, role, status, graph_hash,
     fork_of, fork_seq, zam_session, worktree, budget_json, model_profile,
     created_at, updated_at, generation);

events(seq INTEGER PRIMARY KEY, id BLOB /*UUIDv7*/, run_id, parent_seq, ts,
       actor, kind, schema_v, payload JSON, blob_ref, tainted INT);

effects(run_id, effect_key, class, intent_seq, receipt_seq, blob_ref,
        PRIMARY KEY(run_id, effect_key));

interrupts(id, run_id, kind, status, payload, response, expires_at);
control_commands(command_id PRIMARY KEY, run_id, action, mode, note,
                 issued_by, generation, state, created_at, applied_at);
snapshots(run_id, seq, state);
blobs(hash PRIMARY KEY, size, path_or_data, mime);

plans(run_id, version, sha256, body, attested_by, attested_at);
evidence(id, run_id, claim, kind, ref, verdict, created_at);
checkpoints(id, run_id, seq, git_ref, created_at);

work_items(id, swarm_id, parent, state, deps, write_scope, payload,
           lease_owner, lease_epoch, lease_expires, version, budget,
           idempotency_key, created_at, updated_at);
bus_messages(id, swarm_id, topic, from_run, to_run, kind, payload, tainted, created_at);
blackboard(id, swarm_id, key, value, source_run, expires_at);
swarm_forecasts(swarm_id, mode, reason, expected_gain, actual_gain, inputs_json);

memory_outbox(id, kind, payload, idempotency_key, state, attempts, next_at);
usage(run_id, model, input, cached, output, reasoning, gpu_ms, cost_eq, ts);
audit(seq, prev_hash, hash, actor, action, target, ts);
```

Требования:

| ID | Требование | Приор. |
|---|---|---|
| ARC-030 | Один поток-писатель на файл, очередь команд, пул читателей | P0 |
| ARC-031 | Версия SQLite ≥ 3.51.3, проверка в CI и `doctor` (исправление WAL-reset bug) | P0 |
| ARC-032 | Большие payload в `blobs` по BLAKE3, в журнале ссылка | P0 |
| ARC-033 | FTS5 по текстовым событиям. Проверить наличие в bundled-сборке тестом | P1 |
| ARC-034 | Conformance-тесты `Store` с первого дня | P0 |
| ARC-035 | WAL-чекпоинты с контролем роста файла при долгих читателях | P0 |
| ARC-036 | `seq` служит курсором `Last-Event-ID` и репликации через relay | P0 |
| ARC-037 | Срок хранения журнала настраивается, блобы чистятся по ссылкам; по умолчанию без автоудаления | P1 |
| ARC-038 | Векторы не хранятся в zero-agent: семантический поиск делегируется ZAM | P0 |

## 6. Протоколы

### 6.1. События

`AgentEvent` пишется в журнал; UI, CLI, relay и арена читают один поток с возобновлением по `seq`.

Обязательные семейства событий: `run.*` (создан, статус), `turn.*`, `model.request`, `model.delta`, `model.usage`, `tool.started`, `tool.delta`, `tool.finished`, `interrupt.*`, `control.*`, `progress.report`, `agent.heartbeat`, `plan.updated`, `file.changed`, `checkpoint.created`, `memory.*`, `bus.*`, `swarm.*`, `budget.*`, `evidence.*`.

Схема версионируется полем `schema_v`. Адаптеры проецируют журнал:

| Адаптер | Направление | Приор. |
|---|---|---|
| Внутренний JSON + SSE | Web UI | P0 |
| ATIF v1.8 | Арена, трассы | P1 |
| AG-UI | Внешние фронтенды; SSE и семантика прерываний совместимы | P2 |
| ACP (Agent Client Protocol) | Редакторы, Harbor, использование чужих харнессов как воркеров | P1 |
| A2A | Внешние агенты | P2 |
| OTLP с атрибутами `gen_ai.*` | Внешняя наблюдаемость | P2 |

### 6.2. HTTP API (версия `/api/v1`)

| Группа | Маршруты |
|---|---|
| Проекты | `GET/POST /projects`, `GET/PATCH/DELETE /projects/{id}`, `POST /projects/{id}/trust` |
| Runs | `POST /projects/{id}/runs`, `GET /runs/{id}`, `GET /runs/{id}/tree`, `POST /runs/{id}/messages` |
| События | `GET /events?project=&after_seq=` (SSE), `GET /runs/{id}/events` |
| Управление | `POST /runs/{id}/control {command_id, action, mode, note, expected_generation}` |
| Прерывания | `GET /interrupts`, `POST /interrupts/{id}` |
| Файлы | `GET /projects/{id}/files`, `GET /files/content`, `GET /runs/{id}/diff`, `POST /checkpoints/{id}/restore` |
| Терминал | `WS /terminals/{id}` |
| Задания | `GET/POST /jobs`, `PATCH/DELETE /jobs/{id}`, `POST /jobs/{id}/run`, `GET /jobs/{id}/history` |
| Модели | `GET/POST /models`, `POST /models/{id}/probe` |
| MCP, скиллы, утилиты | `GET/POST /mcp`, `GET/POST /skills`, `GET /tools-registry` |
| Пиры | `GET/POST /peers`, `GET /peers/{id}/projects`, `PATCH /peers/{id}/acl` |
| Память | `GET /memory/status`, `GET /memory/outbox`, прокси чтения для UI |
| Арена | `GET/POST /arena/runs`, `GET /arena/compare` |
| Служебное | `GET /health`, `GET /ready`, `GET /metrics` |

Ошибки по RFC 9457. Все изменяющие запросы принимают `Idempotency-Key`. Публикуется OpenAPI, версия проверяется контрактными тестами.

## 7. Конфигурация

**ARC-040 (P0).** Слои: встроенные умолчания → глобальный TOML → проектный `.zero-agent/config.toml` → переменные `ZA_<РАЗДЕЛ>__<КЛЮЧ>` → аргументы. Схема как у ZAM.

**ARC-041 (P0).** Проектная конфигурация не может ослабить глобальную политику безопасности и не читается до доверия к проекту (SEC-030).

**ARC-042 (P1).** `zero-agent config explain` показывает итоговое значение и источник каждого ключа.

Основные разделы: `server`, `models`, `context`, `tools`, `network`, `memory`, `swarm`, `budgets`, `skills`, `mcp`, `relay`, `scheduler`, `ui`, `arena`, `telemetry`.

**ARC-043 (P0).** Телеметрия наружу выключена и по умолчанию отсутствует.

## 8. Наблюдаемость

| ID | Требование | Приор. |
|---|---|---|
| OPS-001 | `tracing` с уровнями и JSON-логами | P0 |
| OPS-002 | Метрики: токены, cached-токены и доля кэша, время до первого токена, скорость, шаги, ошибки по классам, ремонт вызовов, компакции, стоимость по run | P0 |
| OPS-003 | Инспектор отправленных байтов: точный запрос к модели по каждому ходу, разбивка контекста по слоям | P0 |
| OPS-004 | Дифф отрендеренных префиксов при падении `cached_tokens` | P1 |
| OPS-005 | Атрибуция расхода по скиллу, субагенту, MCP-серверу, памяти | P1 |
| OPS-006 | OTLP-экспорт за флагом | P2 |
| OPS-007 | `GET /metrics` в формате Prometheus | P2 |

## 9. Долгая работа и эксплуатация

| ID | Требование | Приор. |
|---|---|---|
| OPS-010 | Служба Windows через `windows-service`, systemd unit с `Type=notify` и watchdog. Этап R2: без неё нет переживания перезагрузки (G6, SCH-011) | P0 |
| OPS-011 | Пинг watchdog только из цикла, проверяющего планировщик и писателя SQLite | P1 |
| OPS-012 | `jobs` с арендой и эпохой; просроченная аренда освобождается; `tool.started` без `tool.finished` при восстановлении даёт статус `unknown` | P0 |
| OPS-013 | Дерево процессов инструмента: Job Object без breakaway, группа процессов или cgroup. Смерть daemon завершает потомков | P0 |
| OPS-014 | `zero-agent doctor`: версии, порты, GPU, модели, ZAM, SQLite, права, миграции, с безопасными исправлениями | P0 |
| OPS-015 | Версионированные миграции; перед миграцией резервная копия; схема вперёд-совместима на одну версию | P0 |
| OPS-016 | Обновление: загрузка → проверка подписи → бэкап БД → `versions/<ver>/` → переключение → health-check → подтверждение или откат | P2 |
| OPS-017 | Имя бинарника не содержит `update`, `setup`, `install`; манифест встраивается (эвристика UAC) | P1 |
| OPS-018 | Релизы подписаны, установка не требует `curl | bash` | P1 |
| OPS-019 | Бэкап и восстановление: `zero-agent backup`, снимок БД через SQLite Online Backup | P1 |
| OPS-020 | Режим завершения: дождаться безопасной точки, перевести run в `paused`, продолжить после старта по настройке | P0 |
| OPS-021 | Журнал изменений проекта, журнал работ и документация ведутся с первого коммита | P0 |

## 10. Нефункциональные требования

Цели проектирования, уточняются замером на целевой станции.

| ID | Параметр | Цель |
|---|---|---|
| NFR-01 | Старт daemon до готовности API | ≤ 2 с |
| NFR-02 | Накладные расходы харнесса на ход без учёта модели и инструмента | ≤ 50 мс p95 |
| NFR-03 | Время от `pause` до применения вне инструмента | ≤ 200 мс |
| NFR-04 | Отмена стрима и дерева процессов | ≤ 2 с |
| NFR-05 | Первое сообщение в UI после события | ≤ 100 мс |
| NFR-06 | Доля кэшированного префикса на ходах без компакции | ≥ 90 % |
| NFR-07 | Журнал 1 млн событий без деградации чтения UI | p95 ≤ 100 мс на страницу |
| NFR-08 | Потеря подтверждённых событий при `kill -9` | 0 |
| NFR-09 | RSS daemon без браузера | ≤ 200 МБ |
| NFR-10 | Параллельно активных run на узел | ≥ 50 (ограничено слотами модели, а не ядром) |
| NFR-11 | Размер бинарника | ≤ 80 МБ |
