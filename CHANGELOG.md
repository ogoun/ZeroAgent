# Журнал изменений

Формат: новые записи сверху.

## Не выпущено

### Добавлено
- `zero-agent`: бинарник с командами `doctor [--json]` (SQLite ≥ 3.51.3, llama.cpp по `LLAMA_URL` — только предупреждение, версия компилятора сборки), `demo-run` (`--steps`, `--resume`, `--crash-at intent:N|execute:N`, id run первой строкой) и `replay` (хэш значимых событий); коды выхода 0/1/3. Каталог данных `ZA_HOME` (по умолчанию `%LOCALAPPDATA%\zero-agent`, `~/.local/share/zero-agent`), журнал в `ZA_HOME/projects/<project_id>/`. Приёмочные тесты R0. `za-store`: `sqlite_library_version`, `MIN_SQLITE`. Зависимость `clap 4.6.7`.
- `za-protocol`: прямой переход статуса `running → paused` (немедленная пауза в безопасной точке рантайма). `za-store`: `set_status_with_reason` — причина перехода в payload `run.status`. `za-llm`: функция `probe` (по умолчанию) — без неё крейт не тянет `reqwest`; `za-core` подключает `za-llm` без функций по умолчанию.
- `za-core`: демонстрационный run `run_demo` (шаги `model`/`Idempotent` и `tool`/`NonIdempotent` с записью `step N` в `workdir/log.txt`, точки аварийного завершения `CrashPoint`, пауза `paused` с причиной `needs_decision` и исход `DemoOutcome::NeedsDecision` при неизвестном исходе, продолжение после паузы без открытых решений) и `replay_hash` — BLAKE3 значимых событий run без времени, id и `seq`.
- `za-llm`: зонд llama.cpp `probe_llama` — `GET /props`, размер контекста `n_ctx` (из корня или `default_generation_settings`), имя модели, `total_slots`; ошибки `Unreachable` (соединение 2 с, всего 5 с) и `BadResponse`. Без прокси из окружения. Зависимости `reqwest` (rustls, встроенные корни webpki-roots), в тестах `axum`; в `deny.toml` разрешена CDLA-Permissive-2.0.
- `za-llm`: рендер стабильного префикса промпта (`render_tools` с сортировкой и каноническим JSON, `render_system` — единственное system-сообщение, нормализация CRLF) и его хэш `prefix_hash` (BLAKE3) с золотым тестом.
- `za-core`: протокол эффектов намерение → выполнение → квитанция (`RunCtx::effect`) с явным исходом в квитанции (`outcome` ok/error), отравление контекста после ошибки журнала (`EffectError::Poisoned`), повтор незавершённых `Pure`/`ReadOnly`/`Idempotent`, прерывание `unknown_outcome` с детерминированным id для `NonIdempotent`, `recover`.
- `za-store`: хранилище эффектов и прерываний (`record_intent` с FULL для `NonIdempotent`, `record_receipt`, `effect`, `pending_effects`, `open_interrupt`, `interrupts`), ошибки `UnknownEffect` и `EffectConflict`.
- `za-store`: blobs по BLAKE3 (в строке до 1 МиБ, больше — файлом `blobs/<hash>`), вынос payload событий длиннее 64 КиБ в blob с прозрачным чтением, runs с переходами статусов по таблице и проверкой `generation`, события `run.created`/`run.status`, снимки состояния.
- `za-store`: тест устойчивости к `kill -9` — бинарник `crash_writer` и 25 раундов убийства писателя; подтверждённые события не теряются ни при `Normal`, ни при `append_durable`.
- `za-store`: журнал событий на SQLite — поток-писатель с group commit, пул читателей, миграции, блокировка от второго процесса, ошибка `Full` при нехватке места. Надёжность фиксации `Durability` (по умолчанию `Normal`) и `append_durable` с `synchronous=FULL`.
- `za-protocol`: идентификаторы, события, статусы run, классы и ключи эффектов, канонический JSON.
- ТЗ v0.3: `docs/specs/2026-11-06-zero-agent-tz/` (части 00–10, приложение A), ключевые решения согласованы.
- Предложения для ZeroAIMemory: `docs/memory_upgrade.md`.
- Исследование R01…R10: `docs/research/2026-11-06-landscape/`.
- Планы: дорожная карта и R0 в `docs/plans/`.
- ТЗ: очередь к моделям, контроль ресурсов, распределение по узлам, переоценка плана (N-17…N-22).
- Журнал работ `docs/journal.md`.
- `.tmp/` в `.gitignore`.
