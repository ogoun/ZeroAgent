# Журнал изменений

Формат: новые записи сверху.

## Не выпущено

### Добавлено
- `za-core`: протокол эффектов намерение → выполнение → квитанция (`RunCtx::effect`) с явным исходом в квитанции (`outcome` ok/error), повтор незавершённых `Pure`/`ReadOnly`/`Idempotent`, прерывание `unknown_outcome` с детерминированным id для `NonIdempotent`, `recover`.
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
