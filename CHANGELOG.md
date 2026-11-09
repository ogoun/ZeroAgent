# Журнал изменений

Формат: новые записи сверху.

## Не выпущено

### Исправлено
- `za-core`: при `ReplayMismatch` демонстрационный run переводится в `paused` с причиной `replay_mismatch`, а не остаётся в `running`.
- `za-store`: повреждённый файл журнала (`SQLITE_CORRUPT`, `SQLITE_NOTADB`) — `StoreError::Corrupt`, а не `Sqlite`; `EINVAL` от `fsync` каталога на Unix терпится с `warn`; предупреждение о занятом чекпоинте WAL — один раз за серию и при первом превышении 64 МиБ, остальное в `debug`.
- `za-core`: расхождение имени или аргументов эффекта с журналом при повторе (в том числе дрейф `prefix_hash` у `model` демонстрации) — ошибка `EffectError::ReplayMismatch { key, field }` вместо `warn`; `demo-run --resume` завершается кодом 1. `EffectError::Unknown` и `ReplayMismatch` отравляют `RunCtx`, как ошибка журнала. `serde_json` с функцией `float_roundtrip`: дробные аргументы читаются из журнала точно и не дают ложного расхождения.
- `za-store`: контроль роста WAL (ARC-035) — `journal_size_limit` 64 МиБ и `wal_checkpoint(TRUNCATE)` писателем при WAL больше 8 МиБ (проверка каждые 16 пачек), без ожидания читателей.
- `za-store`: `put_blob` восстанавливает пропавший или неполный файл blob при существующей строке; на Unix после `rename` синхронизируется каталог `blobs/`, при его создании — каталог проекта.
- `zero-agent`: `--resume`, `replay` и тестовые хелперы повторяют открытие журнала на `Locked` (10 × 100 мс).
- `deny.toml`: убрана неиспользуемая лицензия `Zlib`.

### Изменено
- `za-store`: `StoreError` помечен `#[non_exhaustive]`, новые варианты `Corrupt` (повреждение журнала), `SchemaTooNew { found, supported }`, `Serde` вместо общего `Sqlite`.

### Добавлено
- `za-protocol`: причины паузы run `pause_reason` (`needs_decision`, `replay_mismatch`, `queue`, `resource`, `provider`).
- `zero-agent`: логи `tracing` в stderr (`tracing-subscriber 0.3.23`, фильтр `filter::Targets`), уровень по умолчанию `warn`, переопределение `ZA_LOG` или `RUST_LOG`. Golden-хэш реплея демонстрации `crates/zero-agent/tests/golden/demo-replay.hash`; реплей после сбоя проверяется в приёмке. Раздел «Быстрый старт» в README.
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
