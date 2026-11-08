# ZeroAIMemory: программный контракт для zero-agent (сверка с кодом)

**Снимок.** Репозиторий `H:\Documents\GitHub\ZeroAIMemory`, ветка `m6-token-economy`, HEAD `13b0bac`, 2026-11-06. Незакоммичены только `crates/zam-server/tests/mcp.rs`, `xtask/src/eval/sessions.rs`, план и спецификация M6. Исполняемый код сервера и клиента совпадает с HEAD. Версия `0.0.10-m5.7` (`Cargo.toml`), хотя код уже содержит часть M6.

**Граница заметок.** По уточнению координатора здесь только интерфейс, проверенный по коду: MCP, REST, CLI, хуки, конфигурация, схема, миграции. Полный разбор документации лежит в `10-zeroaimemory-docs.md`. Документы здесь нужны только для сверки, расхождения собраны в разделе 13.

**Метод.** Ничего не собиралось и не запускалось, код только читался. Поэтому поведение установлено чтением кода и тестов, а не прогоном. Заключения из чтения вынесены в «Выводы», непроверенное — в «Пробелы».

**Сокращения путей:**

| Сокращение | Путь |
|---|---|
| `tools.rs` | `crates/zam-server/src/mcp/tools.rs` |
| `mcp/mod.rs` | `crates/zam-server/src/mcp/mod.rs` |
| `api/X.rs` | `crates/zam-server/src/api/X.rs` |
| `search/mod.rs` | `crates/zam-server/src/api/search/mod.rs` |
| `store/X.rs` | `crates/zam-store/src/X.rs` |
| `cli/main.rs`, `cli/mcp.rs` | `crates/zam-cli/src/main.rs`, `crates/zam-cli/src/cmd/mcp.rs` |
| `hook/X.rs` | `crates/zam-cli/src/cmd/hook/X.rs` |
| `guide` | `crates/zam-server/src/agent_setup/guide.ru.md` |

---

## 1. MCP: инструменты, транспорт, сессия

### Takeaway
MCP даёт 11 инструментов и больше ничего: resources и prompts не объявлены, уведомлений нет. Это тонкий адаптер над REST. Для zero-agent главное, что явную сессию можно задать заголовком `X-Zam-Session` на каждый HTTP-запрос к `/mcp`, и `zam mcp` для этого не нужен.

### Cited Findings

**Транспорт и capabilities**
- Streamable HTTP на `POST {base_path}/mcp`, без MCP-сессий: `with_legacy_session_mode(false)`, `NeverSessionManager`. Ответы `application/json`, keep-alive SSE выключен — `mcp/mod.rs:115-125`.
- SSE возможен только если обработчик шлёт уведомления до ответа, а таких нет — `mcp/mod.rs:3-6`.
- Capabilities — только tools: `ServerCapabilities::builder().enable_tools()` — `mcp/mod.rs:211`. **Resources и prompts нет.**
- `serverInfo`: `zeroaimemory`, версия пакета, title `ZeroAIMemory`; `instructions` — строка `INSTRUCTIONS` — `mcp/mod.rs:99-106,212-216`.
- SDK: `rmcp =3.5.1` (`Cargo.toml`). Тесты инициализируют с `protocolVersion: "2025-06-18"` — `tests/mcp.rs:126`.
- Аутентификация: обязателен `Authorization: Bearer`, иначе 401 problem+json до `rmcp`. Cookie WebUI не действует — `mcp/mod.rs:11-14,187-200`.
- `Origin`, если есть, обязан совпасть с `Host`, иначе 403. Запрос без `Origin` проходит. `Host` не проверяется — `mcp/mod.rs:22-28,140-159`.
- Предел тела: `entries_json_max` = заметочный предел + base64 от `reports.mcp_max_bytes` + запас — `api/mod.rs:91-96`. Большие тела держат разрешения: не больше двух одновременно, иначе 503 `busy` — `mcp/mod.rs:7-10`.
- Отмена: `notifications/cancelled` или обрыв соединения останавливают следующие внутренние запросы — `mcp/mod.rs:356-358,444-446`.

**Сквозные правила ответа**
- Ответ — Markdown; `format: "json"` даёт JSON ответа REST в тексте без `structuredContent`. Последняя строка всегда `~N tok`, в том числе у ошибок — `tools.rs:4-6,510-526`.
- Ошибка сервиса — результат `isError: true` с текстом и подсказкой. Неизвестный инструмент — JSON-RPC `invalid_params`. Неверные аргументы — `isError: true` «Ошибка аргументов: …» — `tools.rs:7-11,529-547`; `mcp/mod.rs:241-246`.
- Подсказки: 409 даёт текущую ревизию, кандидатов и дифф до 4000 символов; 428 говорит, какую ревизию передать; 429 даёт `Retry-After` — `tools.rs:27,567-616`.
- Аннотация `readOnlyHint` стоит у `search, read, tree, recall, code` — `tools.rs:152-157`.
- Пространство по умолчанию: решение из `X-Zam-Solution`, иначе `knowledge` — `tools.rs:692-696`.
- Целые числа принимаются и строкой цифр — `tools.rs:657-665`.

**Инструменты** (схемы `tools.rs:159-392`). Обязательные поля выделены жирным.

| Инструмент | Параметры | REST | Ответ Markdown |
|---|---|---|---|
| `memory_search` | **`query`** (до 512), `mode` brief/chunks/notes, `scope{solution, include_knowledge, include_rules, all_solutions, path_prefix, area}`, `filters{kinds, status, certainty, insight_kinds, decided_by, file}`, `top_k` 1..100, `budget_tokens`, `expand` none/section/neighbors/graph, `rerank` off/on, `skip_seen`, `format` | `POST /search` | карточки; пусто — «Ничего не найдено.» |
| `memory_read` | **`items[1..20]{`**`id`**`, tier L0/L1/L2, section, max_tokens, if_rev, since_rev, locator}`**, `budget_tokens`, `format` | `POST /read` | текст; ошибка внутри элемента |
| `memory_cache_put` | **`query`**, **`summary`**, **`sources[{id, rev?, section?}]`**, `promote{as note/insight, path?, kind?}`, `scope`, `filters`, `format` | `POST /cache` | одна строка `Выжимка c_… · Nt · ист. N · state [→ promoted]` |
| `memory_tree` | `space`, `path`, `depth` 1..5, `format` | `GET /spaces/{s}/tree` | список `n:<id> путь rev · L0` |
| `memory_write` | **`path`** `[<space>:/]<путь>`, `content` или `ops[{op, section?, content}]`, `base_rev`, `message`, `l0`, `format` | `POST /nodes`, `PUT`, `PATCH /nodes/{id}` | `Записано n:… rev…` и `> warning:` |
| `memory_log` | **`kind`** changelog/insight/release/term/task/idea/question/report, **`title`**, `body`, `data`, `images[{name, data}]`, `tags`, `certainty`, `triggers[]`, `links[]`, `refs{files}`, `scope` solution/global, `space`, `force`, `message`, `idempotency_key`, `format` | `POST /spaces/{s}/entries` | `Записано <ref> …`, у отчёта только URL |
| `memory_update_entry` | **`id`**, **`action`** supersede/retract/revisit/certify/append/feedback/status/claim/release/answer/propose/decide, `rev` и поля действия (`status, next_step, blocked_by, ttl_min, agent, option, text, channel, by, decision, horizon, space, title, body, data, tags, triggers, links, refs, certainty, reason, value, message`) | маршруты `/entries/{id}/…` | одна строка |
| `memory_link` | `src`, `type`, `dst` или `remove: <edge_id>`, `format` | `POST /edges`, `DELETE /edges/{id}` | строка |
| `memory_recall` | **`event`** session_start/prompt/post_tool/recovery/brief, `prompt`, `task`, `error`, `tool`, `session_id`, `budget_tokens`, `format` | `POST /recall` | `text` или «Подсказок нет.» |
| `memory_vault` | **`action`** get/set/list, `name`, `id`, `value`, `type`, `purpose`, `address`, `limits`, `notes`, `scope` global/solution/GUID, `format` | `/spaces/{s}/vault`, `/vault/{id}` | метаданные; `get` раскрывает значение с аудитом |
| `memory_code` | **`action`** map/symbol/refs/impact, `query`, `path`, `focus{paths, idents, text}`, `budget_tokens` до 20000, `files[{path, symbols?[{qual, body_hash}]}]`, `format` | `/code/map`, `/code/symbols`, `/code/impact` | карта или карточка; символа нет — не ошибка |

**Скрытые поля: передаются, но не объявлены в схеме**
- `memory_search`: `include_history` — `tools.rs:792-806`.
- `memory_cache_put`: `include_history` — `tools.rs:863-874`.
- `memory_log`: `area`, `status`, `supersedes_id` — `tools.rs:1118-1134`.
- `memory_update_entry`: `area`, `scope` у `supersede` — `tools.rs:1203-1218`.
- `memory_recall`: `agent_block_version`, `source` — `tools.rs:1432-1445`.

**Недоступно через MCP: есть в REST, но инструмент не передаёт**
- `no_cache`, `session_id` и `brief_min_rel` поиска — `tools.rs:792-806`; тест подтверждает «у MCP нет `no_cache`» — `tests/mcp.rs`.
- `solution`, `area`, `parent_id`, `record`, `code_map` у `/recall` — `tools.rs:1432-1445`.
- `session_id` у `/read` — `tools.rs:824`.
- `echo` у `/cache`.

**Привязка к сессии**
- Источники `X-Zam-Session` у `zam mcp` по порядку: `ZAM_SESSION_ID` на всё время процесса; затем файл `.zeroaimemory/cache/agent-<pid>.json` по цепочке предков, поиск не чаще раза в 1 с — `cli/mcp.rs:6-14,40,60-97`.
- `CLAUDE_CODE_SESSION_ID` прокси намеренно не читает — `cli/mcp.rs:10-12`.
- Файл привязки пишет хук `session-start`: `{session_id, agent_pid, started_at}` на предков до процесса агента, не глубже 4. Читатель принимает файл, только если `agent_pid` есть в его цепочке и файл не старше 12 ч — `hook/binding.rs:1-20,29`.
- Имена процессов агента зашиты в `procs::AGENT_PROC_NAMES`; спецификация называет `claude`, `codex`, `node` — `hook/binding.rs:5-7`. **Для другого агента привязка по файлу работать не будет без изменения кода** (см. выводы).
- Заголовок `X-Zam-Session` на HTTP — явная сессия: `explicit = rest.header(SESSION_HEADER).is_some()` — `mcp/mod.rs:257`.
- Без него сервер подставляет неявную сессию: последняя незавершённая за `IMPLICIT_SESSION_HOURS = 12` ч с учётом `X-Zam-Machine` и `X-Zam-Solution` — `mcp/mod.rs:77,258-265,321-346`. Она годится только счётчику записей: неявная сессия не пишет `session_seen` и не помечает выданное — `mcp/mod.rs:35-42`.
- Поле `session_id` в аргументах есть только у `memory_recall`; у `search` и `read` его нет.
- Учёт: один на вызов инструмента, событие `mcp`. С явной сессией идёт в `session_tokens`, иначе в `tokens_unattributed` по суткам UTC — `mcp/mod.rs:285-315`.

**Прокси `zam mcp`**
- Читает stdin построчно, каждое сообщение — отдельный `POST /mcp`, ответ — строка в stdout; SSE разворачивает — `cli/mcp.rs:3-5,15-16`.
- Пределы: строка stdin 4 МиБ (`-32600`), тело ответа 64 МиБ, параллельно 16 запросов, `initialize` идёт первым — `cli/mcp.rs:112-117,360-376`.
- Ошибки транспорта — JSON-RPC `-32000`, разбор — `-32700` — `cli/mcp.rs:19-23,119`.
- Таймауты `Timeouts::Mcp`: DNS и connect по 2 с, отправка запроса 30 с, тело 30 мин, ожидание ответа 10 мин (`MCP_RECV_RESPONSE`), чтение тела 5 мин — `crates/zam-cli/src/client.rs:88-127`.
- Своего предела на запрос у сервера нет — `client.rs:101-105`.

### Inferences
- zero-agent может работать как MCP-клиент напрямую по HTTP: Bearer, `X-Zam-Session`, `X-Zam-Solution`, `X-Zam-Machine`, `X-Zam-Area` на каждый вызов. Файл привязки, хуки и `zam mcp` ему не нужны.
- `memory_recall` с `session_id` в аргументах, но без заголовка, расходится с учётом. `/recall` считает сессию явной (`api/sessions.rs:127-128`), а `account()` смотрит только заголовок (`mcp/mod.rs:257,301`). Значит, токены уйдут в `unattributed`. Вывод из чтения кода, тестом не подтверждён. zero-agent должен всегда слать заголовок.
- Поведение MCP и REST по `(seen)` различается намеренно: у MCP фрагмент остаётся с пометкой `(выдавалось)`, у прямого REST `skip_seen` по умолчанию `true` и фрагмент скрывается — `search/mod.rs` шапка, `via_mcp`.
- Схема `memory_read` не содержит `focus` (M6 §6). Поле уйдёт в `items` и REST ответит 400 из-за `deny_unknown_fields`, так как `items` передаётся целиком — `tools.rs:824`, `api/read.rs:120-132`.

### Gaps
- Какие версии протокола MCP согласует `rmcp 3.5.1` — по коду сервера не определить, в тесте только запрос `2025-06-18`.
- Точный состав `AGENT_PROC_NAMES` прочитан не из `procs.rs`, а из комментариев.

---

## 2. REST API

### Takeaway
Единственная версия `/api/v1` без механизма согласования. Ошибки — RFC 9457. Нет SSE, WebSocket и long-poll. Пакетная операция одна — `POST /read` до 20 целей. Пагинация курсором только у записей журнала и таблицы файлов карты кода; остальные списки — `limit`.

### Cited Findings

**Общее**
- Авторизация: `Authorization: Bearer zam_…` или cookie `zam_session` с CSRF `x-csrf-token` — `auth/mod.rs`.
- Заголовки запроса:

| Заголовок | Назначение |
|---|---|
| `X-Zam-Solution` | решение; некорректный GUID игнорируется |
| `X-Zam-Session` | id сессии, до 128 печатных символов |
| `X-Zam-Machine` | машина, те же правила |
| `X-Zam-Area` | область, те же правила |
| `X-Zam-User` | выбор пользователя администратором; иначе 403 |
| `If-Match: <rev>` | принимает `3`, `"3"`, `W/"3"` — `api/nodes/write.rs:180-189` |
| `Idempotency-Key` | только создание записи журнала; повтор — 200, расхождение — 422 |

  Источник: `attrib.rs:25-33,97-115`, `api/entries.rs:3-7`.
- Сжатие gzip и br через `CompressionLayer`, кроме блобов — `app.rs:333-375`.
- CORS-слоя нет (grep пуст). `/metrics` нет (grep пуст).
- Лимит: 5 неудачных попыток токена за 60 с на IP — `auth/rate.rs:8,10`. Лимита на обычные запросы я не нашёл.

**Коды problem** (`error.rs`):

| HTTP | `type` |
|---|---|
| 400 | `about:blank`, `/problems/by-required` |
| 401 | `about:blank` |
| 403 | `about:blank`, `solution-access`, `password-change-required`, `idea-human-decision`, `idea-human-owned` |
| 404 | `about:blank`, `/problems/code-symbol-not-found` |
| 408 | `/problems/timeout` |
| 409 | `conflict`, `possible-duplicate`, `term-conflict`, `task-claimed`, `already-answered`, `report-immutable`, `idea-agent-origin`, `already-decided`, `already-adopted` |
| 413 | `too-large` |
| 422 | `unprocessable`, `reason-required`, `option-required`, `locator-invalid`, `certainty`, `idempotency-mismatch`, `cache-cross-db`, `cache-forbidden-source`, `cache-source-not-found`, `cache-source-inactive` |
| 428 | `precondition-required` |
| 429 | лимит с `Retry-After` |
| 503 | `busy` с `Retry-After`, `vault-locked` |

**Маршруты** (`api/mod.rs:121-187` — слой записи в память; `197-356` — служебные). Все изменяющие маршруты первой группы с `X-Zam-Session` увеличивают `memory_writes` — `api/sessions.rs:448-473`.

| Группа | Маршруты | Контракт |
|---|---|---|
| Служебные | `GET /health`, `GET /ready` (без аутентификации) | `{status, version, git, features, token_counter, tokenizer_id, uptime_s}`; сейчас `features = ["m6","budget","snippet"]` — `api/health.rs:14-26` |
| Auth | `POST /auth/login`, `logout`, `GET /auth/me`, `POST /auth/password`, `GET/POST /auth/tokens`, `DELETE /auth/tokens/{id}`, `GET/PATCH /auth/settings` | свои токены и настройки: только сессия WebUI, токену 403 — `auth/user.rs:220-226`, `api/user_settings.rs` |
| Пространства | `GET /spaces`, `GET /spaces/{space}/tree?path=&depth=1..5` | токену без Human |
| Решения | `GET/POST /solutions`, `GET/PATCH /solutions/{guid}`, `GET /solutions/{guid}/access`, `POST …/access/{user}/approve|reject|revoke` | 201 создано, 200 владелец или участник, 202 `{state: pending}`, 403 отклонено |
| Узлы | `POST /nodes {space, path, kind note|folder, content?, l0?, message?}`, `GET/PUT/PATCH/DELETE /nodes/{id}`, `GET /nodes/by-path?space=&path=`, `GET /nodes/{id}/outline`, `POST …/move|restore|revert`, `GET …/revisions[/{rev}]`, `GET …/diff`, `GET …/backlinks|links`, `GET /trash` | чтение: `format=json|md|html`, `tier=L0|L1|L2`, `section`, `rev` — `api/nodes/read.rs`; `PUT` без `If-Match` — 428; `PATCH {base_rev, ops, message?}` → 200/409/413/422 |
| Журналы | `GET/POST /spaces/{s}/entries`, `GET/PATCH/DELETE /entries/{id}`, `POST /entries/{id}/restore|answer|claim|release|propose|decide|adopt|supersede|retract|feedback`, `GET …/revisions|backlinks|links` | лента `?kind,status,certainty,area,tag,data_kind,decided_by,since,until,cursor,limit` (до 200) → `{items, next_cursor}`; `PATCH`, `DELETE` требуют `If-Match` |
| Словарь | `GET /spaces/{s}/glossary[?term=]` | `{version, items}` или карточка |
| Vault | `GET/POST /spaces/{s}/vault`, `GET/PATCH/DELETE /vault/{id}`, `GET …/revisions` | `?reveal=1` раскрывает, аудит |
| Граф | `GET /graph?node=|entry=&depth=1..3&types=`, `GET /graph?file=`, `POST /edges`, `DELETE /edges/{id}` | до 500 узлов и 2000 рёбер — `store/graph.rs:55-64`; удаляются только `explicit` |
| Вложения | `POST /blobs`, `DELETE /nodes/{id}/attachments/{sha}/{name}`, `GET /blobs/{sha}/{name}` (Range), `GET /previews/{sha}`, `GET /nodes/{id}/attachments` | 201 `{sha256, link, mime, size}` |
| Поиск | `POST /search`, `POST /read`, `POST /recall` | разделы 3, 4 |
| Кэш | `POST /cache`, `GET /cache`, `GET/DELETE /cache/{id}`, `POST /cache/{id}/promote` | ответ `POST` без `summary`, с `echo: true` — полная форма |
| Хуки и файлы | `GET /triggers?solution=` (ETag), `GET /file-refs` (ETag), `POST /files/changed`, `POST /files/verified` | до 500 файлов; 304 по `If-None-Match` |
| Карта кода | `POST /code/sync`, `GET/PUT /code/files`, `GET /code/status|map`, `GET /code/symbols[/{id}]`, `POST /code/impact`, `DELETE /code` | слабый ETag у `status` и `map` без `focus`; id символов нестабильны, адрес — `путь#qual` — `api/code.rs` шапка |
| Сессии | `GET/POST /sessions`, `GET /sessions/unattributed?days=`, `GET/PATCH /sessions/{id}` | раздел 7 |
| Подключение | `GET /agent-setup`, `GET /agent-guide?lang=`, `GET /downloads/zam/{platform}` (без аутентификации) | блок инструкции и протокол |
| Индекс | `GET /index/status[?only=rechunk]` | любой аутентифицированный; токен видит своего пользователя — `api/index.rs:3-12` |
| Админ | `/admin/users…`, `/admin/tokens/{id}`, `/admin/audit`, `/admin/settings`, `/admin/reindex`, `/admin/agent-setup/preview` | только cookie администратора |
| Рендер | `POST /render`, `GET /render/highlight.css` | для WebUI |
| Отчёт | `GET /entries/{id}/attachments/{sha}/{name}` | только сессии WebUI |

**Пагинация:** `/entries` курсор `<ts>|<id>`, порядок `DESC` — `store/entries/read.rs:99`; `/code/files` постранично; прочие только `limit`.

**Потоки:** `event-stream`, `Sse`, WebSocket в `api/` не найдены. Вызовы `st.index.notify()` — внутреннее пробуждение воркера, а не клиентские уведомления.

### Inferences
- Для zero-agent REST полнее MCP по управлению: регистрация решения и сессии, `parent_id`, `PATCH /sessions/{id}`, `no_cache`, `session_id`, `brief_min_rel`.
- Клиент на Rust может держать типизированный REST-адаптер и MCP оставить только для инструментов, отдаваемых LLM.

### Gaps
- Отдельной схемы OpenAPI нет; форма JSON задаётся обработчиками.
- Полные ответы всех маршрутов перечислены не поштучно: для контракта понадобятся контрактные тесты.

---

## 3. Поиск

### Takeaway
Гибрид FTS5 и Qdrant через weighted RRF с бюджетом в токенах. Сервис деградирует до FTS с `degraded: true` вместо отказа. Бюджет считает сервер своим счётчиком и включает весь Markdown ответа вместе с `~N tok`.

### Cited Findings
- Тело `SearchReq`: `query` (до 512), `scope`, `filters`, `include_history`, `mode`, `top_k` (10, 1..=100), `budget_tokens`, `brief_min_rel`, `format` (json по умолчанию у REST), `expand`, `rerank`, `skip_seen`, `session_id`, `no_cache` — `search/mod.rs:702-731`. `deny_unknown_fields` нет.
- `scope`: `solution, include_knowledge, include_rules, all_solutions, path_prefix, spaces, include_human, area` — `search/targets.rs:43-54`.
- `filters`: `kinds, status, certainty, decided_by, file, insight_kinds` — `search/mod.rs:309-325`. **Фильтра по тегам в поиске нет**: упоминаний `tags` в `search/mod.rs` и `store/search.rs` нет. Теги входят только в контекстный префикс чанка — `store/nodes/chunks.rs:498-520`.
- `KINDS`: `note, insight, changelog, release, term, task, question, folder, attachment, idea` — `search/mod.rs`. Задачи и идеи ищутся только явным `kinds`; открытые вопросы тоже — шапка `search/mod.rs`.
- Каналы: vector (dense + sparse + сводки), bm25, trigram, alias, опционально folder. Формула: `вес / (rrf_k + место)`, затухание по свежести, бусты вида, области и certainty — шапка `search/mod.rs`, `deploy/config.example.toml:147-192`.
- Реранк: `rerank=on`, первые `ml.rerank_top` (20). Без модели — `warnings: ["reranker not installed"]`, сбой — `["reranker failed"]`, не 500.
- Шаг 0 — кэш выжимок: до двух элементов `kind: "cache"` первыми. Запросы со `scope.spaces` или `all_solutions` шаг 0 не делают — `search/cache_step.rs:1-12`.
- Строка карточки: `[.82] r7 · th · superseded · 2026-10-10 14:03:22 · 340t` — `card_tail`, `search/mod.rs:1746-1767`, тест `3549-3580`. Это формат v2 и он **уже реализован**.
- Пометки: `(seen)` (REST и CLI, без фрагмента), `(выдавалось)` (MCP, с фрагментом), `(выдавалось, в выжимке)` (MCP, цель — источник действующей выжимки этого ответа, в JSON `in_cache: true`) — `search/mod.rs` вызов `mark_cache_sources`, `tests/mcp.rs` (незакоммиченные тесты).
- Честный бюджет: `reserve` учитывает выжимки, хвост и строку `~N tok`; проверка `fits_budget`. Первая карточка выдаётся всегда — `search/mod.rs` около `pack_items`.
- Отсечка `brief_min_rel` действует в brief и notes, по умолчанию 0 — `deploy/config.example.toml:203-206`.
- Счётчик токенов: `count_tokens`, вид и id отдаёт `/health`; без модели — оценка `estimate-v6` — `store/nodes/chunks.rs:340-365`.
- Ответ JSON содержит `items, degraded, warnings, expanded, timings{…, total_ms}` и `pending_count` — `search/mod.rs:2813-2830`, шапка. Причины деградации: `no_runtime, no_model, gpu_model_on_cpu, qdrant_down, broken, qdrant_locked, model_mismatch, embed_failed, search_failed` — `index_state.rs:51-59`.
- Дата: `agent_ts` → `yyyy-MM-dd HH:mm:ss` UTC в тексте — `crates/zam-core/src/time.rs`; в JSON — RFC 3339. Дата в строке `[cache]` — только `yyyy-MM-dd` — `search/cache_step.rs:156`.

**`seen` и эпохи**
- `session_seen`: PK `(session_id, owner_id, target_kind, target_id, part, epoch)` — миграция 0025.
- Выданным считается элемент, у которого в той же ревизии отмечены все чанки или цель целиком (`part=''`); учитывается только явная сессия и текущая эпоха — шапка `search/mod.rs`.
- Эпоху растят: `session_start` с `source` compact/resume/clear, `event: recovery`, `POST /sessions {id, new_epoch: true}` — `api/recall.rs:106-113,309`, `api/sessions.rs:12-18`.
- `POST /read` повтор отдаёт полный текст с пометкой `уже выдавалось в этой сессии` и `seen: true` — `api/read.rs:57-65,1059`.

**`/read`**
- До `read.max_items` (20), бюджет по умолчанию 1500. Не влезший элемент обрезается с `…ещё N токенов`, следующие идут как `(budget)` — `api/read.rs:38-43`.
- Ошибка элемента лежит в элементе (`not_found, forbidden, solution_access, invalid`), ответ 200, заголовок `X-Zam-Read-Error` — `api/read.rs:44-47,66-68`.
- Ссылки: `n:`, `i:|c:|r:|t:|k:|q:|w:|e:`, `a:<chunk>`, `b:<sha256>`, `d:<id>`, путь `<space>:/<path>`; `p:` токену не существует.

### Inferences
- Для zero-agent: бюджет `budget_tokens` — верхняя граница ответа, но `~N tok` и размеры измерены счётчиком сервера, а не токенизатором LLM. Планировщику контекста нужен запас или `/health.tokenizer_id`.
- Длинную заметку `memory_read` с умолчанием 1500 обрежет. zero-agent должен передавать бюджет явно.
- Запись не индексируется по тегам для фильтра. Классификация по тегам возможна только через `GET /spaces/{s}/entries?tag=`, и только у записей журнала.

### Gaps
- Точный JSON элемента `items` по всем видам не выписан, смотреть `search/mod.rs` рядом с `Item` и `md_body`.
- Счётчик без модели — оценка. Её погрешность в этих заметках не проверялась.

---

## 4. `/recall`: подсказки и пакеты

### Takeaway
`POST /recall` — точка, через которую нативный агент заменяет хуки. Она агентонезависима: zero-agent может вызывать её сам в своих точках жизненного цикла.

### Cited Findings
- Тело `RecallReq`: `event, source, session_id, solution, area, prompt, error, tool, budget_tokens, agent_block_version, record, task, parent_id, code_map`; неизвестные поля игнорируются — `api/recall.rs:318-345`, шапка строка 141.
- Ответ: `{text, items[{id, kind, title, certainty, reason, age_days?}], tokens, timings, warnings, hooks?}` — `api/recall.rs:7-8`.

| `event` | Содержание | Бюджет | Повторы |
|---|---|---|---|
| `session_start` | напоминание о блоке, запросы доступа, задачи, ответы человека, открытые вопросы, идеи, окружение, Rules, L1 решения, ловушки, revisit, changelog, словарь, проверенные команды, строка карты кода | `digest_tokens` 1500 | не фильтруется |
| `session_start` с `source` compact/resume, либо `recovery` | задачи, ответы без фильтра, решения за 7 дней, затем дайджест в остатке | `recovery_tokens` 2000 | новая эпоха |
| `prompt` | термины до 2, поиск insight и Rules с баллом ≥ 0,6 до 3, идеи до `ideas.prompt_max` | `digest_tokens` | раз за сессию |
| `post_tool` | `error_regex`, затем FTS, до 3 | то же | показывается при каждой ошибке |
| `brief` | правила L0, решения axiom/theorem, термины, ловушки, файлы, код; без `task` — мини-дайджест | `brief_tokens` 800 | не отсекаются |

  Источник: `api/recall.rs:25-140,203-220,283-303`, `deploy/config.example.toml:239-252`.
- Срок сбора `recall.timeout_ms` = 200 мс; по превышении отдаётся собранное и `warnings: ["recall timeout"]` — `api/recall.rs:97-102`.
- Значения `budget_tokens`: 1..=200000 — `api/recall.rs:143`.
- Настройки подсказок `hooks.*` не гасят `recovery` и `brief` — `api/recall.rs:245-256`.
- Формат подсказки: `ZeroAIMemory: <заголовок> [<вид>·<cert>] i:<id>` — `api/recall.rs:11-13`.

### Inferences
- `recall.timeout_ms` = 200 мс задан под хуки. На HDD дайджест может быть неполным, а zero-agent на старте может ждать дольше. Нужен параметр срока на запрос (CR-7).

### Gaps
- Содержимое секций при выключенном `ideas.proactive` и при разных `source` не проверялось сверх шапки модуля.

---

## 5. CLI

### Takeaway
`zam` — синхронный клиент без tokio с контрактом кодов выхода. Для zero-agent он полезен как установщик, синхронизатор карты кода и транспорт отчётов; остальное закрывает REST.

### Cited Findings
- Коды выхода: 0 успех, 1 конфигурация, 2 сервер недоступен, 3 токен, 4 конфликт, 5 не найдено, 6 неверный ввод — `cli/main.rs:3-4`.
- У команд есть общие флаги `--url`, `--token`, `--json` — `crates/zam-cli/src/cmd/mod.rs` `Common`.
- Подключение: флаги → `ZAM_URL`/`ZAM_TOKEN` → блок в `CLAUDE.md`/`AGENTS.md` вверх от cwd → `~/.zeroaimemory/config.toml` — `crates/zam-cli/src/locate.rs:1`.

| Команда | Подкоманды и важные флаги |
|---|---|
| `status` | проверка подключения |
| `tree`, `read`, `write`, `history`, `search`, `attach`, `report` | `read --tier --section --rev --batch --max-tokens --budget --if-rev --since-rev --locator`; `write --file -m --base-rev --l0 --ref`; `search --all --mode --top-k --rerank`; `report --title --tag --area --ref --key --solution` |
| `log` | `changelog`, `term`, `insight` |
| `insight` | `list, show, supersede, retract, revisit, certify, feedback, append` |
| `task` | `list, show, new, status, claim, release` |
| `question` | `list, ask, answer` |
| `idea` | `list, new, propose, accept, defer, decline` |
| `glossary`, `release`, `cache`, `vault`, `solution access|rename`, `link`, `links` | `release add|compare`; `cache put|list|show|rm`; `vault list|get|set` |
| `init` | `--name --id --url --token --agent claude|codex|both --no-hooks --no-mcp -y --lang` |
| `guide`, `mcp`, `hook`, `triggers sync`, `refs check|verify` | служебные агентные |
| `code` | `sync [--full --background --prune]`, `status`, `map [--focus --task --tokens]`, `sym`, `refs`, `impact [--since]` |

Источник: `cli/main.rs:28-306`, `crates/zam-cli/src/cmd/code/mod.rs:31-88`.

**`zam-server`** (`crates/zam-server/src/main.rs:17-69`): `serve`, `hash-password`, `migrate`, `reindex [--user --space --full]`, `qdrant fetch|status|reset`, `models fetch|status`, `cuda fetch`, `service install|uninstall|start|stop|status`; скрытые `doc-worker`, `service run`.

**Не существует в коде:** `zam sessions`, `zam session snapshot`, `zam export`, `zam import`, `zam-server backup`, `restore`, `doctor` (проверено по `Cmd` и списку `cmd/`).

### Inferences
- Разбор кода на tree-sitter есть только в клиенте (фича `parse`). Если zero-agent хочет карту кода, ему проще вызывать `zam code sync` либо подключить `zam-code` с `parse`.
- `--by` и `--origin` обязательны, когда `zam` запущен агентом. Детектор агента смотрит `CLAUDECODE`, `ZAM_AGENT`, переменные Codex — `hook/mod.rs:486-489`. zero-agent должен выставлять `ZAM_AGENT=1`.

### Gaps
- Полный текстовый вывод команд не проверялся, контрактом считать только `--json` и коды выхода.

---

## 6. Хуки

### Takeaway
Шесть команд хуков, два форматтера: Claude Code и Codex. Всегда код 0, fail-open. Хуки — адаптер к чужим агентам; нативному zero-agent они не нужны, но их поведение задаёт, что он обязан делать сам.

### Cited Findings

| Команда | События | Бюджет | Что делает | Вывод |
|---|---|---|---|---|
| `session-start` | `SessionStart` | 300 мс | `POST /sessions`, `/recall`, `GET /triggers`, `/file-refs`, очереди, привязка MCP, фоновая синхронизация карты | `additionalContext` |
| `prompt` | `UserPromptSubmit` | 250 мс | `/recall(prompt)`, журнал длины, очереди | контекст или тишина |
| `pre-tool` | `PreToolUse` | 50 мс, без сети | триггеры `file_glob`, `command_regex`, знание о файле, маркер правок, очередь субагентов | контекст |
| `post-tool` | `PostToolUse`, `PostToolUseFailure` | 250 мс при ошибке, 50 мс при успехе | `/recall(post_tool)`, журнал, хэши в очередь изменённых файлов, петля ошибки | контекст |
| `checkpoint` | `Stop`, `PreCompact` | 100 мс | напоминание записать итоги; на `PreCompact` новая эпоха и журнал | `Stop`: `{"decision":"block","reason":…}`; `PreCompact`: пусто |
| `subagent-start` | `SubagentStart` | 300 мс | сессия `<session>:<agent_id>`, `/recall(brief)` | контекст субагента |

Источник: `hook/mod.rs:79-123`, шапки `hook/*.rs`.

- Вход: один JSON на stdin до 1 МБ; при обрезке поля спасаются частично. Поля: `hook_event_name, session_id, cwd, tool_name, tool_input, tool_response, source, prompt, agent_id, agent_type, trigger, stop_hook_active, error` — `hook/input.rs:1-13,95-120`.
- Выход: `{"hookSpecificOutput":{"hookEventName":…,"additionalContext":…}}`. Предел Claude 9000 символов, Codex 7500 байт — `hook/output.rs:1-16,52-64`.
- `SessionEnd`, `PostCompact`, `SubagentStop` в `Kind::accepts` **нет** — `hook/mod.rs:113-122`.
- Настройки в `.claude/settings.json` и `.codex/hooks.json` генерирует сервер — `agent_setup/mod.rs:234-263`.
- `.mcp.json` теперь stdio: `{"type":"stdio","command":"zam","args":["mcp"]}` — `agent_setup/mod.rs:271-277`.
- Локальные файлы `.zeroaimemory/cache/`: `session.json`, `agent-<pid>.json`, `shown-*`, `events-*.jsonl`, `changed-*.jsonl`, `tokens-*.jsonl`, `spawn-*.json`, `triggers.json`, `file-refs.json`, `hooks.json` — шапки `hook/*.rs`.
- Журнал сессии хранит только метаданные: длина промпта, `exe`, путь — `hook/journal.rs:1-14`.
- Десять флагов подсказок `hooks.*` — `hook/mod.rs:140-157`, `api/user_settings.rs` `HOOK_KINDS`.

### Inferences
Обязательные действия нативного агента вместо хуков:

| Момент | Вызов |
|---|---|
| старт | `POST /sessions`, `POST /recall {session_start}`, `GET /triggers` |
| запрос пользователя | `/recall {prompt}` |
| перед правкой или командой | локальная сверка триггеров по кэшу |
| ошибка инструмента | `/recall {post_tool}` |
| правка файла | `POST /files/changed` |
| сжатие контекста | `POST /sessions {new_epoch}`, затем `/recall {recovery}` |
| субагент | `POST /sessions {parent_id}`, `/recall {brief}` |
| конец | `PATCH /sessions/{id} {ended: true}` |

Сопоставление триггеров реализовано только в клиенте `zam`, его придётся повторить или вынести в крейт (CR-5).

### Gaps
- Форматы Cursor, Gemini и других в коде отсутствуют: `Agent` только `Claude`, `Codex` — `hook/mod.rs:63-66`.

---

## 7. Доменная модель

### Takeaway
Две основные сущности: **узел** (заметка или папка с ревизиями) и **запись журнала** (`entries` восьми видов). Отдельно живут Vault, граф, кэш выжимок, сессии, вложения и карта кода.

### Cited Findings

**Пространства:** `Rules, Human, Knowledge, Solution` — `store/user_db.rs:151-156`.

**Узел**
- Ревизии только дописываются; автор, токен, сессия, машина, сообщение, `user_id` — миграции 0002, 0003, 0007.
- Frontmatter распознаёт только `tags, description, l0, index, certainty, evidence, relations, refs, verified_at, verified_at_commit`; остальное лежит в `raw` — `crates/zam-markdown/src/frontmatter.rs:1-8`.
- Операции `memory_write.ops`: `replace_section, append_to_section, insert_after_heading, append, prepend` — `tools.rs:242-243`.
- У заметки нет статуса и замещения: `supersede` в `store/nodes` и `api/nodes` нет (grep пуст). Есть только ревизии, откат, корзина.

**Записи журнала** (`store/entries/mod.rs`):

| Вид | Ссылка | Статусы | `certainty` | Особое |
|---|---|---|---|---|
| `changelog` | `c:` | нет | нет | `data.kind`: feat, fix, refactor, docs, research, ops, other; только в решении |
| `release` | `r:` | нет | нет | `data.version, date, git_tag?, commits?` |
| `insight` | `i:` | active, revisit, superseded, retracted | да | `data.kind`: gotcha, failure, discovery, decision, experiment, pattern; триггеры, `data.decision`, `data.evidence` |
| `term` | `t:` | active, superseded, retracted | да | `data.definition` ≤ 300, `aliases, not_to_confuse, code`; 409 `term-conflict` |
| `task` | `k:` | open, in_progress, blocked, done, dropped | нет | `next_step, blocked_by, owner, links, estimate_min`; сервер: `claimed_by, started_at, done_at` |
| `question` | `q:` | open, answered, obsolete | нет | `options` до 8, `recommendation`; ответ создаёт insight `decision` |
| `idea` | `w:` | new, proposed, accepted, deferred, declined, done | нет | `origin` human/agent, `priority`, `horizon`, `links` |
| `report` | `p:` | нет | нет | неизменяем, токену не виден |

Источник: `store/entries/mod.rs` `allowed_statuses`, `ref_letter`, `has_certainty`; `api/entries.rs:71-73`; `api/insights.rs:81`.

- Запись: `id, space_id, kind, title (≤500), body, data, tags (до 50), area, status, certainty, supersedes_id, superseded_by_id, retract_reason, rev, author{kind, user_id, token_id, session_id, machine}, index_state, created_at, updated_at, deleted_at` — `store/entries/mod.rs` `Entry`.
- Замещение: `POST /entries/{id}/supersede` → 201 `{entry, superseded}`, старая запись остаётся `superseded`, ничего не удаляется — `api/insights.rs` шапка.
- `certainty`: агентская запись без значения получает `hypothesis`; `theorem` агента требует `data.evidence{checked, method, result}`, иначе 422; `axiom` сервер не запрещает, правило лежит в инструкции — `crates/zam-server/src/certainty.rs:1-15`.
- `by: human` при ответе даёт `axiom`, `by: agent` — `hypothesis`; поле обязательно — `api/questions.rs` шапка.
- Триггеры: `file_glob, command_regex, error_regex, keyword`, до 32 на запись — `store/entries/triggers.rs`.
- Рёбра: `links_to, relates_to, depends_on, part_of, caused_by, fixes, supersedes, derived_from, mentions, contradicts` — `store/graph.rs:73-86`. Межбазовые связи — 422.

**Сессии.** `POST /sessions {id?, agent?, solution?, area?, parent_id?, agent_type?, tokens?, new_epoch?}` с `deny_unknown_fields` → 201 — `api/sessions.rs:181-193`. Срок хранения 30 дней от `last_seen_at`; `usage` бессрочно — `crates/zam-server/src/maintenance.rs:24`, шапка. Зарезервирован id `unattributed` — `store/sessions.rs:191`.

**Карта кода.** Клиент шлёт метаданные: `POST /code/sync` → `need`, затем `PUT /code/files`, до 500 файлов за раз. Ранги считаются лениво; `warnings: ["rank pending"]`. Ссылка памяти `путь#qual` устаревает по телу символа: `symbol_changed`, `symbol_removed` — `api/code.rs` шапка, `api/files.rs` шапка.

**Документы.** Вложение разбирается в `doc-worker`; фрагмент `a:<chunk>` имеет `locator` `page:N|slide:N|para:N|sheet:Лист!A1:F40`. **Отчёты.** MCP до 16 МиБ, CLI multipart до 200 МиБ, агент их не читает.

**Что вызывать и когда** — итог выше в таблице раздела 6 и протокол `guide`: поиск → чтение пачкой → `memory_cache_put`; `task` с `claim` и `next_step`; `question` для решений человека; `insight` для открытий; `changelog` для изменений.

### Inferences
- Для артефактов с жизненным циклом запись журнала богаче заметки: у записи есть статус и замещение, у заметки только ревизии.

### Gaps
- Переходы идей подробно не выписаны, таблица — `check_idea_transition` в `store/ideas.rs`.

---

## 8. Идентичность, арендаторы, права

### Takeaway
Граница изоляции — **пользователь** (отдельная SQLite-база). Токен принадлежит пользователю и имеет только `rw` или `ro`. Сущности «агент» в модели доступа нет: все агенты с токенами одного пользователя видят одно и то же.

### Cited Findings
- Токен: `zam_` + 40 символов base62, в базе хранится sha256 — `crates/zam-core/src/token.rs`. Таблица `tokens`: `id, user_id, name, prefix, secret_hash, access ('rw','ro'), created_at, last_used_at, last_ip, revoked_at` — `migrations/system/0001_init.sql:10-21`. **Срока действия и привязки к решению нет.**
- Выпуск токена: только сессия WebUI или администратор; токен выпустить токен не может — `auth/user.rs:220-226`, `api/admin.rs:298`.
- Принципалы: `Token`, `Admin`, `User`; любой токен пишет как `agent`, сессия WebUI как `human` — `auth/mod.rs`, `attrib.rs:3-6`.
- Политика: Human по токену 404; запись по `ro` 403; Rules по токену только при `rules.agent_write` — `api/scope.rs:21-31`. Решения о доступе не кэшируются, отзыв действует со следующего запроса — `api/scope.rs:36-37`.
- Решение: GUID в `solution.toml`, первый зарегистрировавший — владелец, память лежит в базе владельца. Участник видит только пространство решения, `rw` или `ro` — `api/solutions.rs:1-25`.
- Vault всегда личный, даже в общем решении — `api/vault.rs` шапка.
- Knowledge — личное пространство пользователя; чужой Knowledge не подмешивается никогда — `api/recall.rs:92-96`.

### Inferences
- Для «агент А не должен видеть X агента Б» модели нет. В рамках одного пользователя видимость общая.
- Если zero-agent запускает недоверенных субагентов, единственная изоляция сейчас — отдельный пользователь или `ro`-токен (CR-2).

### Gaps
- Лимиты числа пользователей и решений не проверялись.

---

## 9. Конфигурация, развёртывание, встраивание

### Takeaway
Память рассчитана на отдельный сервис. Формально `zam_server` — библиотека с `build()` и `run()`, но это не стабильный API: версия `0.0.x`, база и индекс тянут ONNX, Qdrant и фоновые воркеры. Для zero-agent рекомендуется граница HTTP.

### Cited Findings
- Слои: умолчания → TOML → `ZAM_<РАЗДЕЛ>__<КЛЮЧ>` → аргументы — `deploy/config.example.toml:2`.
- Секции: `server, admin, log, qdrant, ml, index, search, insights, read, cache, recall, limits, vault, docs, reports, code, ideas` — `deploy/config.example.toml`.
- Порты: сервер `0.0.0.0:7700`, Qdrant `127.0.0.1:6333/6334` — там же, строки 10, 47-48. TLS нет; `base_path`, `trusted_proxies`.
- Каталог данных `./data`; `system.db`, `users/<id>.db`, `blobs/`, `models/`, `qdrant/`, `previews/`, `logs/`, `master.key`; файл блокировки запрещает второй экземпляр.
- SQLite в WAL, `synchronous=NORMAL`, один писатель на базу, 4 читателя — `store/db.rs:34,279-295`.
- Toolchain: `channel 1.99.1`, `rust-version 1.92`, edition 2024, цели `x86_64-unknown-linux-gnu`, `-musl` — `rust-toolchain.toml`, `Cargo.toml`.
- ML: `ort =2.0.0-rc.14` с `load-dynamic`, ONNX Runtime 1.31.0; GPU-сборка `gpu_cuda13` (CUDA 13, cuDNN 9) — `deploy/onnxruntime-gpu.toml`.
- Модели: `bge-m3-int8` 568 МБ (CPU), `bge-m3-fp16` 1,13 ГБ (GPU), реранкеры `bge-reranker-v2-m3` 1,14 ГБ (Apache-2.0) и `jina-reranker-v2` 280 МБ (CC-BY-NC) — `deploy/models.toml`.
- Qdrant 1.19.2 управляемым дочерним процессом; режимы `managed`, `external`, `edge` (прототип, не подключён) — `deploy/qdrant.toml`, `config.example.toml:37-40`.
- pdfium chromium/8100 динамически; без него PDF не индексируются — `deploy/pdfium.toml`.
- Служба: systemd или Windows SCM через `zam-server service` — `crates/zam-server/src/main.rs:73-114`.
- Без ORT или модели сервер стартует в FTS-only.

**Библиотека**

| Крейт | Назначение | Пригодность для линковки |
|---|---|---|
| `zam-core` | `Id`, `TargetRef`, `SolutionGuid`, токены, время, снимок | лёгкий, разумный кандидат |
| `zam-code` | типы протокола, ранг; фича `parse` — tree-sitter | кандидат |
| `zam-markdown` | разбор, секции, чанкер, `mask_text` | кандидат |
| `zam-search` | чистые функции слияния и подсказок | кандидат, но внутренний API |
| `zam-store` | SQLite, миграции | только если отказаться от сервера: два процесса на одну базу запрещены блокировкой |
| `zam-ml`, `zam-index` | ONNX и Qdrant | тяжёлые |
| `zam-server` | `build(cfg, env_password) -> (Router, AppState, BootReport)`, `run`, `run_with` — `app.rs:214-219`, `lib.rs:216,222` | возможно, но требует весь стек |

- `publish` в манифестах не задан; зависимости между крейтами только `path`. Версия `0.0.10-m5.7`, README: API и формат данных до 1.0 могут меняться.
- `zam-server` у `zam-cli` — dev-зависимость, сам клиент от сервера не зависит — `crates/zam-cli/Cargo.toml:43-44`.

### Inferences
- Рекомендуемая схема: zero-agent ходит к внешнему `zam-server` по HTTP; из крейтов подключает самое большее `zam-core` и `zam-code`, закрепляя коммит.
- Встраивание `zam_server::build` в процесс агента связало бы агента с ONNX, Qdrant и строго фиксированным toolchain.

### Gaps
- Минимальная поддерживаемая версия Rust для внешнего потребителя при `rust-version = 1.92` и тулчейне 1.99.1 не проверена: зависимости могут требовать новее.

---

## 10. Статус M6 и что изменится

### Takeaway
M6 выполнена примерно на треть: задачи до 7a включительно. Контракт ещё будет меняться, особенно снимок сессии, фокусное чтение и экспорт. Закладываться на незавершённое нельзя.

### Cited Findings

**Уже в коде**
- MCP-привязка, `explicit`, `via_mcp`, `tokens_unattributed` — `api/sessions.rs:86-105`, `mcp/mod.rs`.
- Карточки v2, честный бюджет, `POST /cache` без `summary`, дата UTC.
- Эпохи, «уже выдавалось», `reread_calls`, `in_cache`.
- Схема `0025`: `seen_epoch, holdout, reread_calls, tokens_unattributed, session_snapshots, recv_count`.
- `zam_core::snapshot`, `store/snapshots.rs`, `store::set_holdout` — **только нижний слой**.

**Ещё нет в коде (проверено grep)**

| Что | Проверка | Задача |
|---|---|---|
| `focus` в `/read` | нет в `ReadItem`, `deny_unknown_fields` → 400 | 7b |
| Rules-резерв, `recall.digest_entries`, `digest_code_map_tokens` | нет в `config.rs` | 8a |
| `zam sessions`, `rules_heavy` | нет команды | 8b |
| `kind_boost_mode`, `w_attach` | нет в `config.rs` | 10, 11 |
| `holdout` в `POST /sessions` | нет в `NewSession` → 400 | 12 |
| `PUT|GET /sessions/{id}/snapshot` | нет в `api/mod.rs` | 17c |
| `SessionEnd`, `PostCompact` в хуках | нет в `accepts` | 17b |
| `zam session snapshot` | нет команды | 17b |
| `backup`, `restore`, `/export`, `/import` | нет команд и маршрутов | 18 |
| блок v7 | `AGENT_BLOCK_VERSION = 6` — `crates/zam-core/src/lib.rs:21` | 13a |

Порядок задач и план дальше: `docs/plans/2026-11-04-m6-token-economy.md:271-316`. Журнал: задачи 7b и далее пусты — `…-log.md:2536-2582`.

**Планируемые изменения контракта** (`docs/specs/2026-11-04-m6-token-economy.md`, сверка с кодом выше): новая версия `0.0.11-m6`, `.mcp.json` уже stdio, поля `holdout`, новые ключи `read.focus_*`, `recall.snapshot_*`, `snapshot.*`, `backup.*`, `import.*`. Умолчания бюджетов определят сетки и могут измениться: дайджест {800, 1000, 1500}, восстановление {1200, 1500, 2000}, `brief` {500, 800}.

**M7 и позже:** `doctor`, `rollback --to`, сборщик мусора блобов, WebUI бэкапа; v2 — режим подтверждения записей агента, офлайн-очередь CLI.

### Inferences
- Контракт zero-agent стоит закреплять на проверке `/health.features` и версии, а не на предположениях о сборке. Ключ `features` как раз для этого добавлен.

### Gaps
- Сроков у оставшихся задач нет.

---

## 11. Общая область и библиотека скриптов (вопрос заказчика 1)

### Takeaway
**Общая область есть только в пределах пользователя: Knowledge.** Она работает между проектами и агентами этого пользователя. Между пользователями глобальной области нет. Хранить скрипты можно уже сейчас, но строгого контракта, версии языка и выбора «актуальной версии» модель не обеспечивает. Лучше всего подходит запись журнала, а не заметка, и для неё нужен новый вид `artifact` (CR-1).

### Cited Findings
- Knowledge поднимается в поиск и дайджест автоматически: `include_knowledge` по умолчанию `true` — `search/mod.rs` шапка, `search/targets.rs:4-5`.
- `scope=global` в `memory_log` пишет insight в Knowledge пользователя запроса — `api/insights.rs` шапка, `target_scope`, строки 645-673.
- Задачи, вопросы, идеи и вложения тоже живут в Knowledge. `changelog` и `release` — только в решении — `api/entries.rs:29`.
- Чужой Knowledge не виден никому, участник решения видит только пространство решения — `api/scope.rs:12-16`, `api/recall.rs:92-96`.
- Rules тоже личные, пишет агент только при `rules.agent_write`.

**Как разложить скрипт на существующие сущности**

| Требование | Заметка (`node`) | Запись `insight` `pattern` | Вложение |
|---|---|---|---|
| Описание | `l0`, `description` | `title`, `body` | только через заметку |
| Находимость | поиск по тексту и пути | поиск, триггеры, подсказки | фрагменты в поиске |
| Строгий контракт (вход, выход) | только текст; ключи frontmatter вне списка — в `raw`, без проверки и фильтра | `data` принимается, но схема вида не задана | нет |
| Версия языка | то же | то же | нет |
| История | ревизии, дифф, откат | ревизии `entry_revisions`, дифф `since_rev` | по sha256 |
| Устаревание | **нет статуса и замещения** | `supersede`, `retract`, `revisit` | нет |
| Тело исходника | до 5 МБ | до 5 МБ в `body`; чанки индексируются | до 200 МБ |
| Фильтр в поиске | `file`, `path_prefix` | `insight_kinds=[pattern]`, `status` | `kinds=[attachment]` |
| Подсказка в момент действия | нет | **триггеры** `file_glob`, `command_regex`, `error_regex`, `keyword` | нет |
| Секреты | маскируются при индексации, в ревизии остаются исходные — док | то же | то же |

Источник: `store/entries/triggers.rs`, `frontmatter.rs:1-8`, `api/insights.rs`, `store/nodes`, `deploy/config.example.toml:256-258`.

- `insight` проверяет `data.kind` и `data.decision`; произвольные поля `data` схемой вида не описаны. Запрос к конкретному ключу `data` в API есть только как `data_kind` — `store/entries/mod.rs` `EntryFilter`.
- Ссылка на файл умеет устаревание: `refs.files` с хэшем даёт `stale`, символ — `symbol_changed`. Но это ссылка на файл **решения**, путь относительно его корня — `api/files.rs` шапка. Для Knowledge она не адресует версионируемый общий артефакт.
- Карта кода только у решений; Knowledge — 422 — `api/code.rs` шапка.

### Inferences
- **Сейчас, без изменений памяти:** `insight` kind `pattern` с `scope=global`, тегом `script`, телом с описанием и контрактом, исходник в `body` или вложением к заметке, устаревание — `supersede`. Контракт проверяет сам zero-agent.
- Замещение даёт цепочку `supersedes_id`/`superseded_by_id`, а поиск по умолчанию показывает только действующие статусы. Это почти готовый механизм «актуальная версия».
- Запуск и песочница — не дело памяти. Память хранит определение, исполняет zero-agent. Это согласуется с тем, что на сервере нет генерации и исполнения.
- Дедупликация insight на близких скриптах может дать 409 `possible-duplicate`, поэтому новая версия должна идти через `supersede`.

### Gaps
- Как ведёт себя порог `dedup_threshold` на коде, не измерялось.
- Обмен скриптами между пользователями: возможно общее решение-библиотека (отдельный GUID с участниками). Механизм есть, но как «глобальная» область не проверен и не попадёт в поиск без `X-Zam-Solution` или `all_solutions`.

---

## 12. Общая память роя (вопрос заказчика 2)

### Takeaway
Параллельные чтение и запись безопасны: один писатель на базу плюс оптимистичные ревизии. FTS видит запись сразу после подтверждения, векторный канал отстаёт. Идентичность субагента есть только на уровне сессии. **Шины нет, допущение подтверждается: шина живёт в zero-agent.** Ближайшая замена очереди — задачи с `claim`, но без «взять следующую» и без уведомлений.

### Cited Findings

**Идентичность и связи**
- `sessions`: `id, token_id, solution_guid, area, machine, agent, parent_id, agent_type, started_at, last_seen_at, ended_at, memory_writes, seen_epoch, holdout` — миграции 0013, 0017, 0025; `store/sessions.rs:69-90`.
- `parent_id` без внешнего ключа: родителя могло не быть — миграция 0017. Индекс `sessions_parent` есть, **API выборки детей нет**: `GET /sessions?solution=&limit=`, `deny_unknown_fields` — `api/sessions.rs:377-382`.
- `parent_id` используется только в `brief`: задачи родителя идут первыми — `api/recall.rs:136,338`.
- Автор ревизии хранит `session_id, token_id, machine, user_id` — миграции 0002, 0007. Столбцов `agent_id` нет: тип и родитель восстанавливаются через `sessions`, а сессии удаляются через 30 дней.
- Хук задаёт id субагента `<session>:<agent_id>`, длинный — `sub-<32 hex>` — `hook/subagent_start.rs`.

**Запись**
- Один поток-писатель на базу, транзакции IMMEDIATE, 4 читателя; ожидание блокировки 30 с — `store/db.rs:1-18,34-36`. Запись сериализована на базу, а у общего решения база одна — владельца.
- Запись узла — одна транзакция: ревизия, узел, чанки, FTS, задание outbox — `store/nodes/write.rs:1-3`.

**Конфликты**

| Операция | Защита | Конфликт |
|---|---|---|
| Создание заметки | путь уникален | 409 `Conflict` (занят) |
| `PUT /nodes/{id}` | `If-Match` обязателен | 428 без него; 409 с диффом |
| `PATCH /nodes/{id}` | `base_rev`; сравнение байт затронутой секции | правка **другой** секции проходит при устаревшем `base_rev`; своя — 409 `SectionChanged`; `append` и `prepend` применяются всегда |
| `PATCH`, `DELETE /entries/{id}` | `If-Match` обязателен | 428, 409 с `rev` |
| `supersede`, `retract` | `If-Match` необязателен | 409 при наличии |
| `memory_update_entry` `append` | через `PATCH`, `rev` нужен | 428, 409 |
| Создание записи | `Idempotency-Key` в границах автора | 200 повтор, 422 расхождение |
| Новый insight | дедупликация | 409 `possible-duplicate`, `force` |
| Вопрос | заголовок | 409 `possible-duplicate` |
| Термин | имя и псевдоним | 409 `term-conflict` |
| Задача `claim` | транзакция писателя | 409 `task-claimed` с `claimed_by` |

Источник: `store/nodes/revise.rs:21-39,96-101`, `api/nodes/write.rs:199-260`, `api/entries.rs:3-11`, `store/tasks.rs:332-368`.

- `claim` рекомендательный: `api/entries.rs` захват не проверяет (grep `claim` пуст), чужой агент может править задачу — также `guide:53`. Истёкший захват перехватывается; той же сессией продлевается. Закрытую задачу захватить нельзя — `store/tasks.rs:357-368`.
- Захват не создаёт ревизию — `store/tasks.rs` шапка, поэтому `rev` при этом не меняется.
- Снятие чужого захвата — только человек — `api/tasks.rs`.
- Выжимка заменяется только того же автора и той же сессии — `api/cache.rs` шапка. Выжимки других сессий выдаются при `cache.cross_session = true`.

**Read-your-writes**

| Канал | Когда виден |
|---|---|
| `GET /nodes/{id}`, `/entries/{id}`, `/read` | сразу после ответа записи (WAL, читатели видят подтверждённое) |
| FTS в `/search` | сразу: FTS в той же транзакции; заметки в очереди ищутся с полным `w_fts` — `config.example.toml:153-157` |
| vector | после воркера: `notify()` пробуждает, затем инференс и `upsert` с `wait=true` — `zam-index/src/worker.rs` |
| Графовые рёбра, триггеры | в транзакции записи |
| Дедупликация по косинусу | только после векторной индексации; заголовок — сразу |
| `GET /triggers`, `/file-refs` у хуков | кэш клиента, обновляется на `session-start` |

- Признак отставания: `pending_count` в ответе поиска; значение 0 гарантирует, что подтверждённые записи видны векторному каналу — `search/mod.rs:2412-2415`.
- Замер задержки «запись до векторного индекса»: CPU 1,98 с, GPU 0,46 с на 10 тыс. заметок, HDD — README §«Производительность». Это документ, в этой работе не перемерялось.
- Задание на цель одно: новая ревизия заменяет активное, «грязное» возвращается в очередь — `store/jobs.rs:1-16`. Значит, при частых правках отставание вектора растёт, но очередь не пухнет.

**Сессия и `seen` в рое**
- Отдельная сессия на субагента даёт отдельный `session_seen`. На общем подключении `zam mcp` субагент идёт с сессией родителя, поэтому `seen` не прячет фрагмент — `api/sessions.rs:94-96`, `tests/mcp.rs:3140`.
- `memory_writes` и учёт токенов идут в сессию заголовка.

**Шина или очередь**
- Уведомлений нет: нет SSE, WebSocket, long-poll, подписок (раздел 2).
- `since` в ленте фильтрует **`created_at`**, не `updated_at` — `store/entries/mod.rs` `EntryFilter`, `read.rs:41`. Опросом нельзя увидеть изменение статуса существующей записи.
- Лента `DESC` — `read.rs:99`, а FIFO-выборки нет.
- `claim` без операции «взять следующую»: список, затем попытка, при 409 следующий кандидат.
- `index_jobs` — внутренняя очередь сервера, наружу не выставлена.
- Вопросы и `next_step` — асинхронная связь, но приходят только в дайджесте.

### Inferences
- **Допущение подтверждено.** Шина, рассылка задач и живые статусы роя — в zero-agent. Память хранит то, что должно пережить рой: решения, ловушки, задачи, итоги.
- Схема: каждому субагенту явная сессия `POST /sessions {id, parent_id, agent_type, agent, solution}` и свой `X-Zam-Session`; пакет `/recall {brief, task, parent_id}`; задачи через `claim`, итог в `insight`/`changelog`.
- Общая заметка плохо подходит для роя как рабочая доска: одно обновление на секцию, остальные получают 409. Лучше писать отдельные записи журнала или разные секции. `append` не конфликтует.
- Гонка дедупликации: проверка в обработчике стоит до транзакции записи (`api/insights.rs` `dedup`). Два субагента могут записать близкие insight параллельно, особенно пока вектор не проиндексирован. Вывод из чтения; тестом не проверен.
- Запись сериализована на базу, поэтому пропускная способность ограничена одним писателем и диском. Рой на HDD будет ждать друг друга, предел ожидания 30 с. Числовой предел не измерялся.
- Если всем субагентам нужен немедленный взаимный обмен фактами, FTS-запрос с `kinds` видит их сразу. Семантический поиск надёжен только при `pending_count = 0`.
- Для субагента читать «моё только что записанное» надо по id из ответа записи, а не поиском.

### Gaps
- Нагрузочного замера параллельной записи и `claim` не найдено и не проводилось.
- Поведение WAL-контрольной точки под роем: в коде есть предупреждение о секундах на HDD (`store/db.rs:14-17`), чисел для такой нагрузки нет.

---

## 13. Расхождения кода с заявленным намерением

| # | Заявлено | Код | Следствие |
|---|---|---|---|
| 1 | README: M6 «запланирован» | часть M6 реализована, `/health.features` содержит `m6` | README отстаёт |
| 2 | README: блок инструкции v2 при `zam init` | `AGENT_BLOCK_VERSION = 6` — `zam-core/src/lib.rs:21` | устарело |
| 3 | README, `deploy/README-install.md`, `guide:9`: MCP по HTTP с `ZAM_TOKEN` в `.mcp.json` | фрагмент — stdio `zam mcp` — `agent_setup/mod.rs:271-277` | HTTP работает, но без сессии у чужих агентов; для zero-agent с заголовком работает полностью |
| 4 | `cli/main.rs:284`: `hook` — пять видов | `Kind` — шесть, есть `subagent-start` | комментарий устарел |
| 5 | `api/recall.rs:51`: `hooks` из девяти ключей | `HOOK_KINDS` — десять | шапка устарела |
| 6 | `mcp/mod.rs:13`: «лимит, отзыв, срок» токена | столбца срока нет | «срок» не существует |
| 7 | `tools.rs:15`, `guide:17`: префиксы ссылок | `w:` работает, в списках нет | документация неполна |
| 8 | Описание `memory_update_entry`: `rev` кроме `feedback, claim, release, answer` | `propose`, `decide` тоже без `rev` (схема верна) | описание неполно |
| 9 | Спецификация M6 §8а: столбец `trigger` | `trigger_kind` — миграция 0025 | осознанно, спецификация не обновлена |
| 10 | Спецификация §7.1: `mcp/mod.rs:266` и др. | строки сдвинулись | только ссылки |
| 11 | ТЗ §12: `GET /metrics` в целевом API | маршрута нет; ТЗ сам оговаривает, что этап не назначен | не рассчитывать |
| 12 | `guide:53`: захват рекомендательный | совпадает с кодом | расхождения нет, важно для роя |
| 13 | `config.example.toml:90`: `ml.unload_after` | не реализован, WARN | не рассчитывать |
| 14 | Схема `memory_search`: фильтр по файлу и видам | `tags` нет | нельзя фильтровать теги |
| 15 | Шапка `read.rs`: ссылки `i|c|r|t|k|q` | `w:` принимается через `entry_ref` | неполно |

---

## 14. Запросы на изменения (change requests)

Приоритет: **P0** нужен для первого выпуска zero-agent, **P1** для роя и библиотеки, **P2** желательно.

| ID | Приор. | Изменение | Обоснование |
|---|---|---|---|
| CR-1 | P1 | Новый вид записи `artifact`, ссылка `s:`: `data{artifact_type: script|utility|prompt, language, language_version, entrypoint, input_schema, output_schema, deps, version}` со строгой проверкой; статусы active/deprecated/superseded; тело или вложение; фильтры поиска `artifact_type`, `language`. | Заказчик хочет строгий контракт, версию языка и замещение. `insight` даёт лишь свободный `data`. |
| CR-2 | P1 | Токены с областью: привязка к решению и пространствам, срок действия, необязательный `agent_id`. | Сейчас только `rw`/`ro` на пользователя, срока нет; нельзя выдать субагенту узкий токен. Заодно закрывает выпуск токена только через WebUI: нужен машинный выпуск дочерних токенов. |
| CR-3 | P1 | Первоклассная идентичность субагента: `agent_id`, `agent_type` в ревизиях и `author`; `GET /sessions?parent_id=`; `GET /sessions/{id}/children`. Срок сессий с учётом дерева. | Сейчас связь есть, но её нельзя выбрать, а после 30 дней тип автора теряется. |
| CR-4 | P0 | Фид изменений: `GET /spaces/{s}/changes?since_seq=` или `updated_since` в `/entries`, монотонный курсор, виды событий. | `since` смотрит `created_at`, изменение статуса опросом не увидеть. Шина остаётся в агенте, но агенту нужно досинхронизироваться после разрыва. |
| CR-5 | P1 | Вынести сопоставление триггеров и маскировку в отдельный крейт (`zam-triggers`, запланированный `zam-secrets`). | Нативный агент иначе повторяет логику `pre-tool`. |
| CR-6 | P1 | `POST /entries/claim-next {space, filter, ttl_min}`: атомарно взять следующую открытую задачу; опционально строгий режим захвата. | Рой иначе перебирает список с 409. Это не шина, а безопасная выдача работы. |
| CR-7 | P0 | Параметры агента в `/recall`: `timeout_ms` на запрос; явный `agent: zero-agent` без форматтеров хуков; `source` и `session_id` в объявленной схеме MCP. | 200 мс заданы под хуки; скрытые поля делают контракт неявным. |
| CR-8 | P0 | Единый учёт явной сессии: `session_id` в `memory_search`, `memory_read` и согласовать `account()` с телом запроса. | Сейчас `memory_recall` с полем уходит в `unattributed`. |
| CR-9 | P0 | Описание контракта: OpenAPI и версия MCP-схем; `/health.features` как официальный список возможностей; обещание совместимости хотя бы для `/api/v1`. | До 1.0 всё может меняться, а агенту нужно проверять совместимость на старте. |
| CR-10 | P1 | Журнал `insight`: уникальность близких записей в транзакции или ключ дедупликации, задаваемый агентом; `dedup` после индексации. | Параллельные субагенты обходят проверку. |
| CR-11 | P2 | Параметр `wait_indexed` / `min_seq` в `/search`: дождаться индексации своей записи, срок ограничен. | Семантический read-your-writes сейчас только через опрос `pending_count`. |
| CR-12 | P1 | Групповая запись: `POST /entries:batch` и `POST /files/changed` с общей сессией. | Рой пишет много коротких фактов; каждая запись — отдельная транзакция и HTTP. |
| CR-13 | P2 | Глобальная область между пользователями: пространство `shared` или системное решение с ролями. | Библиотеку скриптов нельзя разделить между пользователями без обходного решения. |
| CR-14 | P2 | Фильтр `filters.tags` и выборка по ключам `data`. | Теги в поиске не фильтруются. |
| CR-15 | P2 | Сервисный режим без WebUI-зависимостей для токенов и настройка лимитов на токен. | Для автоматического развёртывания роя. |
| CR-16 | P2 | Поле `structuredContent` в MCP или стабильная JSON-схема ответов при `format: json`. | Текст сейчас не контракт. |
| CR-17 | P1 | Завершить снимок сессии с нейтральным входом, доступным нативному агенту: `PUT /sessions/{id}/snapshot` без адаптеров журналов. | Таблица и типы есть, маршрутов нет; zero-agent сам знает своё состояние и не нуждается в разборе журнала. |

### Inferences
- P0 не блокирует старт: можно начать с поисков и записей через MCP с заголовками. CR-4, CR-7, CR-8, CR-9 снимают хрупкость контракта.
- Шину в память добавлять не нужно. CR-4 и CR-6 дают досинхронизацию и атомарную выдачу работы, не превращая память в брокер.

### Gaps
- Объём изменений по CR не оценивался: это решение владельца ZeroAIMemory, ни одно не согласовано.
