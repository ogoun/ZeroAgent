# R0 Основание: план реализации

> **Для агентов-исполнителей:** ОБЯЗАТЕЛЬНЫЙ ПОД-СКИЛЛ: superpowers:subagent-driven-development (рекомендуется) или superpowers:executing-plans, задача за задачей. Шаги отмечаются чекбоксами (`- [ ]`).

**Goal:** журнал и эффекты на SQLite, запись и реплей искусственного run на Windows и Linux, переживающие `kill -9`, плюс зонды llama.cpp и ZeroAIMemory.

**Architecture:** Cargo workspace. `za-protocol` даёт типы, `za-store` — SQLite с одним потоком-писателем, `za-core` — протокол эффектов намерение→квитанция и восстановление, `za-llm` и `za-memory` — проверка внешних систем, бинарник `zero-agent` собирает это в `doctor`, `demo-run`, `replay`.

**Tech Stack:** Rust 1.99.1 (edition 2024), tokio, rusqlite bundled, serde, jiff, uuid v7, blake3, reqwest + rustls, clap; axum только в тестах R0.

**Spec:** `docs/specs/2026-11-06-zero-agent-tz/`: часть 01 (разделы 3–6, 9, 10), часть 04 (разделы 2, 3, 6, 17), часть 10 (раздел 2). Карта всех этапов: `docs/plans/2026-11-06-roadmap.md`.

## Global Constraints

- Платформы: Windows x64, Linux x64; тесты зелёные на обеих.
- Зависимости минимальны: только crates.io, с закреплёнными версиями. Запрещены `git =`, внешние `path =`, submodule, код других агентов и агентные фреймворки (ARC-012, N-13).
- Журнал только дополняется. Гарантия at-least-once на шаг, обещание exactly-once запрещено (CORE-001).
- Версия SQLite ≥ 3.51.3 (ARC-031). Один поток-писатель на файл, пул читателей (ARC-030).
- Временные файлы только внутри проекта: тесты используют `env!("CARGO_TARGET_TMPDIR")`, рабочие файлы `.tmp/`. Системные `%TEMP%` и `/tmp` запрещены (ARC-021).
- `tracing` для логов. Ошибки библиотек через `thiserror`, без `unwrap` вне тестов.
- Телеметрии наружу нет (ARC-043).
- Сообщения коммитов на русском, в стиле `feat(store): …`, как в ZeroAIMemory.
- Журнал работ `docs/journal.md` и `CHANGELOG.md` обновляются в каждой задаче, меняющей поведение (OPS-021).

## Review Focus

1. **Диск заполнен при записи в журнал:** `append` возвращает ошибку, ни одно подтверждённое событие не теряется, процесс не падает. Тест в задаче 3.
2. **Второй процесс на том же проекте:** второй `open` получает понятную ошибку `StoreError::Locked`, а не порчу данных. Тест в задаче 3.
3. **Часы пошли назад:** порядок событий определяет `seq`, а не `ts`. Тест в задаче 3.
4. **Путь с кириллицей и пробелами:** данные проекта лежат в `Рабочий стол/проект 1` на Windows. Тест в задаче 11.
5. **Внешняя система отвечает мусором:** llama.cpp или память вернули HTML, пустое тело, обрыв. Результат — типизированная ошибка и `L0`, а не паника. Тесты в задачах 8 и 9.

---

## Файловая структура

```text
Cargo.toml                     workspace, общие версии
rust-toolchain.toml            channel = "1.99.1"
deny.toml                      лицензии и источники
.github/workflows/ci.yml       матрица Windows и Linux
xtask/                         cargo xtask policy
crates/za-protocol/src/        ids.rs, event.rs, run.rs, effect.rs, canonical.rs
crates/za-store/src/           lib.rs, writer.rs, migrate.rs, events.rs, blobs.rs, runs.rs, effects.rs
crates/za-store/migrations/    0001_init.sql
crates/za-store/tests/         conformance.rs, crash.rs
crates/za-store/src/bin/       crash_writer.rs  (только для теста)
crates/za-core/src/            lib.rs, runtime.rs, recovery.rs, demo.rs
crates/za-llm/src/             lib.rs, prefix.rs, probe.rs
crates/za-memory/src/          lib.rs, client.rs, level.rs
crates/zero-agent/src/         main.rs, doctor.rs, home.rs
crates/zero-agent/tests/       r0_acceptance.rs
```

Журнал лежит в `ZA_HOME/projects/<project_id>/journal.db`, где `ZA_HOME` — переменная окружения либо `%LOCALAPPDATA%\zero-agent`, `~/.local/share/zero-agent` (ARC-020).

---

### Task 1: Workspace, CI и политика зависимостей

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `deny.toml`, `.github/workflows/ci.yml`
- Create: `xtask/Cargo.toml`, `xtask/src/main.rs`, `xtask/src/policy.rs`
- Create: пустые крейты `crates/za-protocol`, `za-store`, `za-core`, `za-llm`, `za-memory`, `zero-agent` с `src/lib.rs` (`main.rs` у `zero-agent`)
- Modify: `.gitignore` (`target`, `.tmp/` уже есть)

**Interfaces:**
- Produces: `cargo xtask policy` с кодом 0 при соблюдении правил; функции `check_lock_sources(lock: &str) -> Vec<Violation>`, `check_direction(metadata: &Metadata) -> Vec<Violation>`, `check_core_size(lines: usize, limit: usize) -> Option<Violation>`.

- [ ] **Step 1: Установить тулчейн**

Run: `winget install Rustlang.Rustup`, затем `rustup show` в корне репозитория.
Expected: активен `1.99.1` по `rust-toolchain.toml`. Если канала нет, взять тот, что в `ZeroAIMemory/rust-toolchain.toml`.

- [ ] **Step 2: Написать падающие тесты `xtask/src/policy.rs`**

```rust
#[test]
fn git_dependency_is_rejected() {
    let lock = "[[package]]\nname = \"x\"\nversion = \"1.0.0\"\nsource = \"git+https://example.com/x\"\n";
    assert_eq!(check_lock_sources(lock).len(), 1);
}
#[test]
fn crates_io_source_is_allowed() {
    let lock = "[[package]]\nname = \"x\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
    assert!(check_lock_sources(lock).is_empty());
}
#[test]
fn core_over_limit_is_reported() {
    assert!(check_core_size(5001, 5000).is_some());
    assert!(check_core_size(5000, 5000).is_none());
}
```

- [ ] **Step 3: Запустить: `cargo test -p xtask` — FAIL, функции не определены**

- [ ] **Step 4: Реализовать `policy.rs`**

Источник в `Cargo.lock` допустим только если отсутствует или начинается с `registry+https://github.com/rust-lang/crates.io-index`. Запрещённые имена пакетов: `rig-core`, `adk-rust`, `autoagents`. Направление: `za-core` не зависит от `za-server`, `za-peer`, `za-browser`; `za-protocol` не зависит ни от одного `za-*`. Лимит `za-core` хранится в `xtask/core-size-limit.txt`, старт 5000 строк (ARC-011).

- [ ] **Step 5: Запустить: `cargo test -p xtask && cargo xtask policy` — PASS**

- [ ] **Step 6: Создать `.github/workflows/ci.yml`**

Матрица `windows-latest`, `ubuntu-latest`: `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`, `cargo xtask policy`, `cargo deny check`.

- [ ] **Step 7: Коммит** `chore: workspace, CI и политика зависимостей`

---

### Task 2: Типы протокола

**Files:**
- Create: `crates/za-protocol/src/{ids,event,run,effect,canonical}.rs`
- Modify: `crates/za-protocol/src/lib.rs`

**Interfaces:**
- Produces:

```rust
pub struct RunId(pub Uuid); impl RunId { pub fn new() -> Self } // UUIDv7
pub struct EventId(pub Uuid);
#[derive(Copy, Ord)] pub struct Seq(pub u64);
pub struct BlobHash(pub String); // blake3 hex

pub enum Actor { User, Runtime, Model, Tool, Peer }
pub enum RunRole { Top, Orchestrator, Worker }
pub enum RunStatus { Queued, Running, Pausing, Paused, Cancelling, BlockedOnBudget,
                     Done, Partial, Blocked, Failed, Cancelled }
impl RunStatus { pub fn can_transition(self, to: RunStatus) -> bool;
                 pub fn is_terminal(self) -> bool; }

pub enum EffectClass { Pure, ReadOnly, Idempotent, NonIdempotent }
pub struct EffectKey(pub String);
impl EffectKey { pub fn derive(run: RunId, step: u64, effect_seq: u64) -> Self; }

pub enum EventKind { RunCreated, RunStatus, TurnStarted, ModelRequest, ModelUsage,
    ToolStarted, ToolFinished, EffectIntent, EffectReceipt, InterruptOpened,
    InterruptResolved, Unknown(String) }
impl EventKind { pub fn as_str(&self) -> &str; pub fn parse(s: &str) -> Self; }

pub const SCHEMA_V: u16 = 1;
pub struct NewEvent { pub run_id: RunId, pub parent_seq: Option<Seq>, pub actor: Actor,
                      pub kind: EventKind, pub payload: Value, pub tainted: bool }
pub struct Event { pub seq: Seq, pub id: EventId, pub run_id: RunId, pub parent_seq: Option<Seq>,
    pub ts: Timestamp, pub actor: Actor, pub kind: EventKind, pub schema_v: u16,
    pub payload: Value, pub blob_ref: Option<BlobHash>, pub tainted: bool }

pub fn canonical_json(v: &Value) -> String; // ключи отсортированы рекурсивно
```

- [ ] **Step 1: Написать падающие тесты**

```rust
#[test] fn key_is_stable() {
    let r = RunId(Uuid::parse_str("0190b6a0-0000-7000-8000-000000000001").unwrap());
    assert_eq!(EffectKey::derive(r, 2, 3), EffectKey::derive(r, 2, 3));
    assert_ne!(EffectKey::derive(r, 2, 3), EffectKey::derive(r, 2, 4));
}
#[test] fn terminal_status_has_no_exits() {
    assert!(!RunStatus::Cancelled.can_transition(RunStatus::Running));
    assert!(RunStatus::Running.can_transition(RunStatus::Pausing));
    assert!(RunStatus::Paused.can_transition(RunStatus::Running));
}
#[test] fn event_kind_names_are_dotted_and_unknown_survives() {
    assert_eq!(EventKind::ToolStarted.as_str(), "tool.started");
    assert_eq!(EventKind::parse("future.thing"), EventKind::Unknown("future.thing".into()));
}
#[test] fn canonical_sorts_nested_keys() {
    let v = json!({"b":1,"a":{"d":1,"c":2}});
    assert_eq!(canonical_json(&v), r#"{"a":{"c":2,"d":1},"b":1}"#);
}
```

- [ ] **Step 2: Запустить `cargo test -p za-protocol` — FAIL**

- [ ] **Step 3: Реализовать типы**

`EffectKey`: `"{run}:{step}:{effect_seq}"`. Переходы по схеме раздела 4.3 части 01: `queued→running`, `running⇄paused` через `pausing`, `running→cancelling→cancelled`, `running→blocked_on_budget`, из `blocked_on_budget` в `running`, из `running` в терминальные; `paused→cancelling`. Сбой провайдера не ведёт в `failed`, а в `paused` (это правило цикла R1, а не переходов).

- [ ] **Step 4: Запустить `cargo test -p za-protocol` — PASS**

- [ ] **Step 5: Коммит** `feat(protocol): идентификаторы, события, статусы, канонический JSON`

---

### Task 3: Журнал на SQLite

**Files:**
- Create: `crates/za-store/src/{lib,writer,migrate,events}.rs`, `crates/za-store/migrations/0001_init.sql`
- Test: `crates/za-store/tests/conformance.rs`

**Interfaces:**
- Consumes: типы задачи 2.
- Produces:

```rust
pub struct ProjectStore { /* cheap clone */ }
pub enum StoreError { Locked, Full, SqliteTooOld(String), Sqlite(String), Io(String), Closed }
impl ProjectStore {
    pub fn open(dir: &Path) -> Result<Self, StoreError>;
    pub async fn append(&self, e: NewEvent) -> Result<Seq, StoreError>;
    pub async fn append_batch(&self, e: Vec<NewEvent>) -> Result<Vec<Seq>, StoreError>;
    pub async fn read_after(&self, after: Seq, limit: usize) -> Result<Vec<Event>, StoreError>;
    pub async fn read_run(&self, run: RunId, after: Seq, limit: usize) -> Result<Vec<Event>, StoreError>;
    pub fn sqlite_version(&self) -> String;
}
```

Схема `0001_init.sql` — таблицы `runs`, `events`, `effects`, `interrupts`, `snapshots`, `blobs` по разделу 5.2 части 01; прочие таблицы добавляются миграциями своих этапов.

- [ ] **Step 1: Написать падающие тесты `conformance.rs`**

Хелпер `fn dir(name) -> PathBuf`: `env!("CARGO_TARGET_TMPDIR")/name`, каталог пересоздаётся.

```rust
#[tokio::test] async fn seq_is_monotonic_and_read_after_resumes() {
    let s = open("seq");
    let a = s.append(ev()).await.unwrap(); let b = s.append(ev()).await.unwrap();
    assert!(b > a);
    assert_eq!(s.read_after(a, 10).await.unwrap().len(), 1);
}
#[tokio::test] async fn concurrent_appends_get_unique_seq() { /* 100 задач, все seq различны */ }
#[tokio::test] async fn order_comes_from_seq_not_clock() {
    // Review Focus 3: события с ts в прошлом читаются в порядке seq
}
#[test] fn second_open_is_locked() {
    let d = dir("lock"); let _a = ProjectStore::open(&d).unwrap();
    assert!(matches!(ProjectStore::open(&d), Err(StoreError::Locked)));
}
#[tokio::test] async fn disk_full_is_an_error_not_a_panic() {
    // Review Focus 1: PRAGMA max_page_count задан малым через тестовый хелпер;
    // append большого payload даёт Err(StoreError::Full); ранее подтверждённые события читаются
}
#[test] fn sqlite_version_is_at_least_3_51_3() { /* сравнение кортежей */ }
```

- [ ] **Step 2: Запустить `cargo test -p za-store` — FAIL**

- [ ] **Step 3: Реализовать `migrate.rs`**

SQL встроен через `include_str!`, версия в `PRAGMA user_version`, каждая миграция в транзакции.

- [ ] **Step 4: Реализовать `writer.rs`**

Один `std::thread`, очередь `std::sync::mpsc`, ответ через `tokio::sync::oneshot`. Group commit: забрать первую команду, затем без ожидания всё накопленное, не больше 256, в одной транзакции `BEGIN IMMEDIATE`. PRAGMA: `journal_mode=WAL`, `synchronous=FULL`, `foreign_keys=ON`. Читатели: четыре отдельных соединения с `query_only=ON`. Блокировка файла `journal.lock` через `std::fs::File::try_lock`.

`synchronous=FULL` выбран ради CORE-005; если замер покажет заметную задержку, сравнить с `NORMAL` и записать результат в журнал работ.

- [ ] **Step 5: Запустить `cargo test -p za-store` — PASS**

Если `sqlite_version_is_at_least_3_51_3` падает, поднять версию `rusqlite`, а не снижать порог.

- [ ] **Step 6: Коммит** `feat(store): журнал событий, писатель и миграции`

---

### Task 4: Устойчивость к `kill -9`

**Files:**
- Create: `crates/za-store/src/bin/crash_writer.rs`
- Test: `crates/za-store/tests/crash.rs`

**Interfaces:**
- Consumes: `ProjectStore::{open, append, read_after}`.
- Produces: тестовый бинарник `crash_writer <dir>`: пишет события и после подтверждения каждого печатает `seq` в stdout с немедленным flush.

- [ ] **Step 1: Написать падающий тест**

```rust
#[test] fn acknowledged_events_survive_kill() {
    for round in 0..20 {
        let d = dir(&format!("crash{round}"));
        let mut c = spawn_writer(&d);
        let acked = read_seqs_for(&mut c, Duration::from_millis(50 + round * 37));
        c.kill().unwrap(); // TerminateProcess на Windows, SIGKILL на Linux
        c.wait().unwrap();
        let s = ProjectStore::open(&d).unwrap();
        let got = read_all_seqs(&s);
        assert!(acked.iter().all(|q| got.contains(q)), "потеряно подтверждённое событие");
    }
}
```

- [ ] **Step 2: Запустить `cargo test -p za-store --test crash` — FAIL, бинарника нет**

- [ ] **Step 3: Реализовать `crash_writer.rs`**

- [ ] **Step 4: Запустить — PASS 20 раундов на обеих ОС**

Открытие после убийства не должно давать `Locked`: блокировка `File::try_lock` снимается ОС при смерти процесса.

- [ ] **Step 5: Коммит** `test(store): подтверждённые события переживают kill -9`

---

### Task 5: Блобы, runs, снимки

**Files:**
- Create: `crates/za-store/src/{blobs,runs}.rs`
- Modify: `crates/za-store/tests/conformance.rs`, `crates/za-store/src/events.rs`

**Interfaces:**
- Produces:

```rust
pub const BLOB_THRESHOLD: usize = 64 * 1024;
impl ProjectStore {
    pub async fn put_blob(&self, data: &[u8]) -> Result<BlobHash, StoreError>; // идемпотентно
    pub async fn get_blob(&self, h: &BlobHash) -> Result<Option<Vec<u8>>, StoreError>;
    pub async fn create_run(&self, r: NewRun) -> Result<Run, StoreError>;
    pub async fn get_run(&self, id: RunId) -> Result<Option<Run>, StoreError>;
    pub async fn set_status(&self, id: RunId, to: RunStatus, expected_generation: u64) -> Result<u64, StoreError>;
    pub async fn save_snapshot(&self, id: RunId, seq: Seq, state: Value) -> Result<(), StoreError>;
    pub async fn latest_snapshot(&self, id: RunId) -> Result<Option<(Seq, Value)>, StoreError>;
}
pub struct NewRun { pub id: RunId, pub project_id: String, pub parent_id: Option<RunId>,
                    pub role: RunRole, pub model_profile: Option<String> }
pub struct Run { pub id: RunId, pub parent_id: Option<RunId>, pub root_id: RunId, pub role: RunRole,
                 pub status: RunStatus, pub generation: u64 }
// StoreError дополняется: InvalidTransition { from, to }, StaleGeneration { current }
```

- [ ] **Step 1: Написать падающие тесты**

```rust
#[tokio::test] async fn large_payload_goes_to_blob_and_reads_back() {
    // payload 100 КиБ: event.blob_ref заполнен, read_run возвращает исходный payload
}
#[tokio::test] async fn invalid_transition_is_rejected() { /* cancelled → running даёт InvalidTransition */ }
#[tokio::test] async fn stale_generation_is_rejected() { /* SWM-064: старая generation даёт StaleGeneration */ }
#[tokio::test] async fn child_inherits_root() { /* root_id потомка равен root_id родителя */ }
```

- [ ] **Step 2: Запустить `cargo test -p za-store` — FAIL**

- [ ] **Step 3: Реализовать**

Блоб хранится в строке таблицы `blobs` до 1 МиБ, больше — файлом `blobs/<hash>` рядом с БД. В `append` payload больше `BLOB_THRESHOLD` заменяется на `null` с заполненным `blob_ref`; при чтении подставляется обратно (ARC-032).

- [ ] **Step 4: Запустить `cargo test -p za-store` — PASS**

- [ ] **Step 5: Коммит** `feat(store): блобы, runs, снимки`

---

### Task 6: Протокол эффектов и восстановление

**Files:**
- Create: `crates/za-store/src/effects.rs`, `crates/za-core/src/{runtime,recovery}.rs`
- Modify: `crates/za-core/src/lib.rs`

**Interfaces:**
- Consumes: `ProjectStore`, `EffectKey`, `EffectClass`.
- Produces:

```rust
// za-store
pub struct EffectRow { pub key: EffectKey, pub class: EffectClass, pub intent_seq: Seq,
                       pub receipt_seq: Option<Seq>, pub result: Option<Value> }
impl ProjectStore {
    pub async fn record_intent(&self, run: RunId, key: &EffectKey, class: EffectClass, name: &str, args: Value) -> Result<Seq, StoreError>;
    pub async fn record_receipt(&self, run: RunId, key: &EffectKey, result: Value) -> Result<Seq, StoreError>;
    pub async fn effect(&self, run: RunId, key: &EffectKey) -> Result<Option<EffectRow>, StoreError>;
    pub async fn pending_effects(&self, run: RunId) -> Result<Vec<EffectRow>, StoreError>;
    pub async fn open_interrupt(&self, id: &str, run: RunId, kind: &str, payload: Value) -> Result<(), StoreError>;
    pub async fn interrupts(&self, run: RunId) -> Result<Vec<InterruptRow>, StoreError>;
}
// za-core
pub struct RunCtx { /* store, run, step, effect_seq */ }
pub struct EffectSpec { pub class: EffectClass, pub name: String, pub args: Value }
pub enum EffectError { Store(StoreError), Unknown { interrupt_id: String }, Failed(String) }
impl RunCtx {
    pub fn new(store: ProjectStore, run: RunId) -> Self;
    pub fn next_step(&mut self);
    pub async fn effect<F, Fut>(&mut self, spec: EffectSpec, f: F) -> Result<Value, EffectError>
      where F: FnOnce() -> Fut, Fut: Future<Output = Result<Value, String>>;
}
pub enum Recovery { Clean, NeedsDecision { interrupt_ids: Vec<String> } }
pub async fn recover(store: &ProjectStore, run: RunId) -> Result<Recovery, StoreError>;
```

- [ ] **Step 1: Написать падающие тесты**

```rust
#[tokio::test] async fn completed_effect_is_not_executed_twice() {
    // два RunCtx с одинаковыми run/step: счётчик вызовов замыкания равен 1,
    // второй вызов возвращает записанный результат
}
#[tokio::test] async fn idempotent_intent_without_receipt_is_retried() {
    // record_intent без receipt, class=Idempotent: effect выполняет замыкание
}
#[tokio::test] async fn non_idempotent_intent_without_receipt_interrupts() {
    // CORE-003: EffectError::Unknown, замыкание не вызвано, в interrupts есть UnknownOutcome
}
#[tokio::test] async fn interrupt_id_is_deterministic() { /* CORE-007: одинаковый при повторе */ }
#[tokio::test] async fn intent_is_written_before_execution() {
    // внутри замыкания pending_effects уже содержит этот ключ
}
#[tokio::test] async fn recover_reports_unknown_outcomes() { /* Recovery::NeedsDecision */ }
```

- [ ] **Step 2: Запустить `cargo test -p za-core` — FAIL**

- [ ] **Step 3: Реализовать `RunCtx::effect`**

Ключ из `(run, step, effect_seq)`, `effect_seq` растёт в рамках шага. Алгоритм:

```text
row = store.effect(key)
receipt есть            → вернуть result
intent есть, нет receipt:
   NonIdempotent        → open_interrupt("unk-{key}", UnknownOutcome), Err(Unknown)
   прочие классы        → выполнить заново
intent нет              → record_intent, выполнить
результат Ok            → record_receipt, вернуть
результат Err           → record_receipt с {"error":…}, Err(Failed)
```

Значения пока `serde_json::Value`: типизированный `effect<T>` — решение R1, эргономика не проверена прототипом (R03).

- [ ] **Step 4: Запустить `cargo test -p za-core` — PASS**

- [ ] **Step 5: Коммит** `feat(core): протокол эффектов намерение→квитанция и восстановление`

---

### Task 7: Префикс и его хэш

**Files:**
- Create: `crates/za-llm/src/prefix.rs`
- Modify: `crates/za-llm/src/lib.rs`

**Interfaces:**
- Consumes: `canonical_json`.
- Produces:

```rust
pub struct ToolSchema { pub name: String, pub description: String, pub parameters: Value }
pub struct PromptBlocks { pub tools: Vec<ToolSchema>, pub core: String, pub adapter: String,
                          pub overlay: String, pub agents_md: String }
pub fn render_system(p: &PromptBlocks) -> String;   // единственное system-сообщение (CTX-004)
pub fn render_tools(p: &PromptBlocks) -> String;    // канонический порядок и ключи (CTX-001)
pub fn prefix_hash(p: &PromptBlocks) -> String;     // blake3 hex от tools + system
```

- [ ] **Step 1: Написать падающие тесты**

```rust
#[test] fn tool_order_does_not_change_hash() { /* tools в разном порядке: хэш один */ }
#[test] fn key_order_in_schema_does_not_change_hash() { /* разный порядок ключей parameters */ }
#[test] fn any_block_change_changes_hash() { /* правка core меняет хэш */ }
#[test] fn golden_hash_is_stable() {
    assert_eq!(prefix_hash(&fixture()), include_str!("../tests/golden/prefix.hash").trim());
}
```

- [ ] **Step 2: Запустить `cargo test -p za-llm` — FAIL**

- [ ] **Step 3: Реализовать**

Инструменты сортируются по `name`, сериализуются через `canonical_json`. Блоки склеиваются через `\n\n`, пустые пропускаются. Нормализовать `\r\n` в `\n`, иначе хэш разойдётся между Windows и Linux. Файл `tests/golden/prefix.hash` создать из первого прогона и зафиксировать в `.gitattributes` как `-text`.

- [ ] **Step 4: Запустить `cargo test -p za-llm` — PASS**

- [ ] **Step 5: Коммит** `feat(llm): рендер и хэш стабильного префикса`

---

### Task 8: Зонд llama.cpp

**Files:**
- Create: `crates/za-llm/src/probe.rs`
- Test: `crates/za-llm/tests/probe.rs`

**Interfaces:**
- Produces:

```rust
pub struct LlamaProbe { pub n_ctx: u32, pub model: String, pub total_slots: Option<u32> }
pub enum ProbeError { Unreachable(String), BadResponse(String) }
pub async fn probe_llama(base_url: &str) -> Result<LlamaProbe, ProbeError>;
```

- [ ] **Step 1: Написать падающие тесты на поддельном axum-сервере**

```rust
#[tokio::test] async fn reads_n_ctx_from_root() { /* /props: {"n_ctx":32768,"model_path":"m.gguf"} */ }
#[tokio::test] async fn reads_n_ctx_from_generation_settings() {
    /* /props: {"default_generation_settings":{"n_ctx":8192}} */
}
#[tokio::test] async fn html_body_is_bad_response() { /* Review Focus 5: не паника */ }
#[tokio::test] async fn connection_refused_is_unreachable() { /* порт без сервера */ }
#[tokio::test] #[ignore = "нужен запущенный llama-server: LLAMA_TEST_URL"]
async fn live_probe() { /* n_ctx > 0 */ }
```

- [ ] **Step 2: Запустить `cargo test -p za-llm --test probe` — FAIL**

- [ ] **Step 3: Реализовать**

`GET {base}/props`, таймаут 5 с, `n_ctx` из корня или `default_generation_settings`. Точное положение поля на текущей версии llama.cpp не проверялось, поэтому поддерживаются оба и обязателен живой тест из шага 1 при наличии сервера.

- [ ] **Step 4: Запустить `cargo test -p za-llm` — PASS**

- [ ] **Step 5: Коммит** `feat(llm): зонд llama.cpp`

---

### Task 9: Клиент памяти и уровни совместимости

**Files:**
- Create: `crates/za-memory/src/{client,level}.rs`
- Test: `crates/za-memory/tests/client.rs`, `crates/za-memory/tests/live.rs`

**Interfaces:**
- Produces:

```rust
pub struct MemoryConfig { pub url: String, pub token: String, pub machine: String }
pub enum Level { L0, L1, L2 }
pub struct Health { pub version: String, pub features: Option<Vec<String>>, pub tokenizer_id: Option<String> }
pub struct MemoryClient { /* reqwest */ }
pub enum MemoryError { Unavailable(String), Auth, Conflict(u32), Rejected { status: u16, detail: String }, BadResponse(String) }
impl MemoryClient {
    pub fn new(cfg: MemoryConfig) -> Self;
    pub async fn health(&self) -> Result<Health, MemoryError>;
    pub async fn level(&self) -> Level;  // при ошибке L0, без паники
    pub async fn create_session(&self, s: &NewSession) -> Result<(), MemoryError>;
    pub async fn recall(&self, session: &str, solution: Option<&str>, r: &RecallReq) -> Result<RecallResp, MemoryError>;
}
pub struct NewSession { pub id: String, pub agent: String, pub solution: Option<String>,
                        pub parent_id: Option<String>, pub agent_type: Option<String> }
pub struct RecallReq { pub event: String, pub session_id: String, pub budget_tokens: Option<u32>, pub prompt: Option<String> }
pub struct RecallResp { pub text: String, pub tokens: Option<u32>, pub warnings: Vec<String> }
pub fn is_unknown_field(detail: &str, field: &str) -> bool; // MEM-010
```

- [ ] **Step 1: Написать падающие тесты на поддельном сервере**

```rust
#[tokio::test] async fn no_features_means_l1() { /* /health {status,version,git,uptime_s} */ }
#[tokio::test] async fn m6_feature_means_l2() { /* features:["m6","budget","snippet"] */ }
#[tokio::test] async fn unreachable_means_l0() {}
#[tokio::test] async fn garbage_health_means_l0() { /* Review Focus 5: HTML */ }
#[tokio::test] async fn session_header_is_sent_on_recall() {
    /* сервер проверяет Authorization, X-Zam-Session, X-Zam-Machine */
}
#[tokio::test] async fn recall_passes_session_id_in_body() { /* поле session_id есть */ }
#[test] fn unknown_field_only_when_named() {
    assert!(is_unknown_field("unknown field `new_epoch`, expected one of", "new_epoch"));
    assert!(!is_unknown_field("invalid uuid", "new_epoch"));
}
```

- [ ] **Step 2: Запустить `cargo test -p za-memory` — FAIL**

- [ ] **Step 3: Реализовать**

Таймауты connect 2 с, recall 1 с (MEM-024). Ошибки разбираются как RFC 9457. Токен в логи не попадает: тип `MemoryConfig` без `derive(Debug)` по полю `token`.

- [ ] **Step 4: Запустить `cargo test -p za-memory` — PASS**

- [ ] **Step 5: Написать живой тест `tests/live.rs`**

`#[ignore]`, читает `ZAM_TEST_URL`, `ZAM_TEST_TOKEN`: `health`, `create_session`, `recall {event:"session_start"}`. Запускать только на тестовом экземпляре, не на рабочей памяти: сессии создаются реально.

- [ ] **Step 6: Коммит** `feat(memory): клиент, заголовки сессии, уровни совместимости`

---

### Task 10: Демонстрационный run

**Files:**
- Create: `crates/za-core/src/demo.rs`
- Modify: `crates/za-core/src/lib.rs`

**Interfaces:**
- Consumes: `RunCtx`, `ProjectStore`, `prefix_hash`.
- Produces:

```rust
pub struct DemoConfig { pub steps: u32, pub workdir: PathBuf, pub crash_at: Option<CrashPoint> }
pub enum CrashPoint { AfterIntent(u32), AfterExecute(u32) } // номер шага
pub enum DemoOutcome { Done, NeedsDecision(Vec<String>) }
pub async fn run_demo(store: &ProjectStore, run: RunId, cfg: &DemoConfig) -> Result<DemoOutcome, EffectError>;
pub async fn replay_hash(store: &ProjectStore, run: RunId) -> Result<String, StoreError>;
```

Каждый шаг: эффект `model` (класс `Idempotent`, детерминированный ответ по номеру шага, без сети) и эффект `tool` (класс `NonIdempotent`, дописывает строку `step N` в `workdir/log.txt`). `replay_hash` — blake3 от канонического JSON значимых полей событий run (`kind`, `actor`, `payload`), без `ts` и `id`.

- [ ] **Step 1: Написать падающие тесты**

```rust
#[tokio::test] async fn demo_writes_each_step_once() { /* log.txt содержит 1..N по одному разу */ }
#[tokio::test] async fn rerun_after_finish_changes_nothing() { /* файл и replay_hash те же */ }
#[tokio::test] async fn replay_hash_ignores_time() { /* два run с одним сценарием и разным временем */ }
```

- [ ] **Step 2: Запустить `cargo test -p za-core` — FAIL**

- [ ] **Step 3: Реализовать `run_demo`**

`crash_at` вызывает `std::process::abort()` в указанной точке: так тест задачи 11 получает детерминированную смерть.

- [ ] **Step 4: Запустить `cargo test -p za-core` — PASS**

- [ ] **Step 5: Коммит** `feat(core): демонстрационный run для проверки журнала`

---

### Task 11: Бинарник, `doctor` и приёмка R0

**Files:**
- Create: `crates/zero-agent/src/{doctor,home}.rs`
- Modify: `crates/zero-agent/src/main.rs`
- Test: `crates/zero-agent/tests/r0_acceptance.rs`
- Modify: `docs/journal.md`, `CHANGELOG.md`, `README.md`

**Interfaces:**
- Consumes: всё выше.
- Produces: команды `zero-agent demo-run --project <dir> [--steps N] [--resume <run>] [--crash-at intent:N|execute:N]`, `zero-agent replay --project <dir> --run <id>` (печатает хэш), `zero-agent doctor [--json]`.

```rust
pub fn za_home() -> PathBuf; // ZA_HOME либо платформенный каталог
pub fn project_id(dir: &Path) -> String; // blake3 канонического пути, первые 16 hex
pub struct DoctorReport { pub sqlite: Check, pub llama: Check, pub memory: Check, pub rust: String }
pub enum Check { Ok(String), Warn(String), Fail(String) }
```

- [ ] **Step 1: Написать падающие тесты**

Запускают бинарник через `env!("CARGO_BIN_EXE_zero-agent")`, `ZA_HOME` внутри `CARGO_TARGET_TMPDIR`.

```rust
#[test] fn crash_after_execute_resumes_without_repeating_idempotent_effects() {
    // --crash-at execute:3, ненулевой выход; --resume: выход с кодом 3 (нужно решение),
    // в журнале есть UnknownOutcome для шага 3 — намерение без квитанции
}
#[test] fn crash_after_intent_is_not_silently_retried() { /* NonIdempotent не выполнен повторно */ }
#[test] fn replay_hash_is_identical_across_two_reads() {}
#[test] fn cyrillic_path_with_spaces_works() {
    // Review Focus 4: проект в "Рабочий стол/проект 1"
}
#[test] fn doctor_warns_when_memory_is_down_and_exits_zero() {
    // memory.url указывает в никуда: Warn, код 0 (MEM-011)
}
#[test] fn doctor_never_prints_the_token() {}
```

- [ ] **Step 2: Запустить `cargo test -p zero-agent` — FAIL**

- [ ] **Step 3: Реализовать `main.rs`, `home.rs`, `doctor.rs`**

`clap` derive. Коды выхода: 0 готово, 1 ошибка, 3 нужно решение пользователя. `doctor` проверяет версию SQLite, доступность llama.cpp (`LLAMA_URL`) и памяти (`ZAM_URL`, `ZAM_TOKEN`), показывает уровень L0–L2.

- [ ] **Step 4: Запустить всё: `cargo test --workspace && cargo xtask policy` — PASS на обеих ОС**

- [ ] **Step 5: Проверить критерий R0 вручную на рабочей станции**

```powershell
cargo run -p zero-agent -- doctor
cargo run -p zero-agent -- demo-run --project .tmp/r0 --steps 20
cargo run -p zero-agent -- replay --project .tmp/r0 --run <id>
```

Expected: `doctor` показывает версию SQLite ≥ 3.51.3 и статус внешних систем; повторный `replay` даёт тот же хэш.

- [ ] **Step 6: Обновить `docs/journal.md`, `CHANGELOG.md`, статус в `README.md`**

Записать фактическую задержку `append` при `synchronous=FULL` и версию SQLite.

- [ ] **Step 7: Коммит** `feat: бинарник zero-agent, doctor, приёмка R0`

---

## Покрытие требований R0

| Требование | Задача |
|---|---|
| ARC-001 (частично), ARC-002 (частично: статическая сборка `x86_64-unknown-linux-musl` не проверяется ни локально, ни в CI) | 1, 11 |
| ARC-006, NFR-08 | 4, 11 |
| ARC-010…012 | 1 |
| ARC-020, ARC-021 | 11, глобальные ограничения |
| ARC-030…032, ARC-034…036 | 3, 5; ARC-035 — финальный fix-раунд |
| CORE-001…005, CORE-006 (частично: снимки есть в хранилище, но ядро и демо их не используют; resume — полный повтор узла с отдачей завершённых эффектов из журнала, без «снимок + реплей хвоста»), CORE-007, CORE-012 | 2, 6 |
| CTX-001, CTX-002, CTX-004 | 7 |
| MDL-003…004 (частично) | 8 |
| MEM-010…011, MEM-020 (частично), MEM-024, MEM-130 (начало) | 9, 11 |
| OPS-014 (частично), OPS-021 | 11 |

Остальные требования относятся к следующим этапам: см. `docs/plans/2026-11-06-roadmap.md`.
