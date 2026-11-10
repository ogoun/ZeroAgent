# ADR 0005. Крейты `za-jail`, `za-tools`, `za-secrets`: запуск процессов, инструменты, секреты

- Статус: принято (R1-C1, 2026-11-10). Источник: решения плана D73, D84, D90, `docs/plans/2026-11-10-r1c1-tools.md`; Ruling «зависимости» и решение заказчика (в) в `.superpowers/sdd/2026-11-10-r1c1-tools/progress.md`.
- Требования: часть 01 ТЗ, таблица крейтов и стек; SEC-006 (процессы только через `Runner`), OPS-013 (смерть daemon завершает потомков), SEC-011, NFR-04, ARC-021; SEC-040, SEC-041, SEC-044 (секреты); ограничение workspace `unsafe_code = "forbid"`.

## Контекст

R1-C1 даёт агенту нативные инструменты (`read`, `search`, `glob`, `edit`, `multi_edit`, `bash`, `request_grant`) и чекпоинты на `git`. Каждый дочерний процесс — команда `bash`, `git` чекпоинта, `nvidia-smi` монитора ресурсов — должен:

- умирать вместе со всем деревом по тайм-ауту, при отмене (бросок future) и при смерти агента;
- не держать вызов после выхода главного процесса, если фоновый потомок держит каналы вывода;
- не получать секреты (`ZA_SECRET_*`) и не писать во временные каталоги системы;
- отдавать ограниченный вывод, а ненулевой код выхода — как данные, а не ошибку.

ОС-изоляции нет (решение заказчика N-15), но точка расширения для неё (SEC-100) нужна сразу. Таблица крейтов части 01 называет `za-tools → za-jail` и `za-jail` без зависимостей; секретов и PTY в ней нет, `process-wrap` назван только запасным вариантом PTY.

## Решение

**`za-jail`** — трейт `Runner { run(CommandSpec) -> BoxFuture<Result<Output, RunnerError>> }` и единственная реализация `DirectRunner` без изоляции (D84):

- дерево процессов — `process-wrap =10.0.1`, `process_wrap::tokio::CommandWrap`: на Windows `JobObject` + `KillOnDrop` (ставит `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: закрытие Job вместе с агентом убивает дерево) и `CreationFlags(CREATE_NO_WINDOW)`; на Unix `ProcessGroup::leader()` + `KillOnDrop` и свой страж группы, который при броске future шлёт `killpg(SIGKILL)` (у `ProcessGroupChild` нет `Drop`, а `kill_on_drop` tokio убивает только лидера);
- выход главного процесса ждётся отдельно от Job или группы (их `wait()` ждёт всех, а фоновый потомок держит каналы): после него EOF вывода ждётся не дольше `KILL_GRACE` = 2 с, затем дерево убивается и ставится `background_killed`. По тайм-ауту дерево убивается сразу, остаток дочитывается не дольше `KILL_GRACE`. В конце вызова дерево убивается всегда: потомки, закрывшие вывод, вызов не переживают;
- stdout и stderr читаются одновременно кусками, каждый до `CAPTURE_LIMIT` = 16 МиБ, остаток дочитывается и отбрасывается (`*_truncated`); `from_utf8_lossy`, `\r\n` → `\n`;
- окружение — разрешённый набор `child_env`: без `ZA_SECRET*` (без учёта регистра на всех ОС), `ZA_HOME` и `remove_env`, с `GIT_TERMINAL_PROMPT=0` и `GCM_INTERACTIVE=never`; `EnvMode::Clear` — только `PATH`, `SystemRoot`, `WINDIR` и `env_set`; `TMP`, `TEMP`, `TMPDIR` — на `tmp_dir`; stdin — null;
- остатки на Unix: `{pgid, starttime}` лидера пишется в файл остатка, `DirectRunner::reap_leftovers(root)` при старте и resume run убивает процессы с `pgrp == pgid` и `starttime` не раньше записанного (перебор `/proc/*/stat`, лидер может быть уже мёртв). Процесс, ушедший через `setsid`, — принятый риск OPS-013;
- ненулевой код выхода — `Exit::Code`, тайм-аут — `Exit::Timeout`; `RunnerError` — только сбой запуска или ввода-вывода.

**`za-tools`** — нативные инструменты и чекпоинты над `za-protocol`, `za-core`, `za-jail` (задачи 6–9).

**`za-secrets`** — трейт `SecretStore`, `EnvStore`, сканер; без крейтов `za-*` и внешних (задача 10). Решение заказчика (в): секреты хранятся в Vault памяти ZeroAIMemory; бэкенд `VaultStore` появится с клиентом памяти по CR-21, контракт Vault ZeroAIMemory не меняется. До него работает только `EnvStore` (`ZA_SECRET_*`). Хранилище ОС (`keyring`, `zbus`, Secret Service) не используется.

**Направления** (проверяет `cargo xtask policy`, тесты в `xtask/src/policy.rs`):

- `za-jail` не зависит от крейтов workspace ни в каком виде;
- `za-res` — от `za-protocol` и `za-jail` (`nvidia-smi` через `DirectRunner` с `EnvMode::Clear` и своим белым списком);
- `za-tools` — от `za-protocol`, `za-core`, `za-jail` (обычные зависимости);
- `za-secrets` — без обычных зависимостей;
- `za-agent` — может от `za-secrets`, не может от `za-tools` и `za-jail` ни в каком виде; `za-core` — ни от одного из трёх;
- `xtask` может зависеть от `za-tools` (случаи бюджета промптов, D93);
- `std::process::Command`, `tokio::process::Command`, `Command::new` в `crates/*/src` — только в `crates/za-jail/` (список исключений с причиной — `PROCESS_ALLOWED`); тесты в `tests/` не проверяются;
- в `Cargo.lock` нет `openssl-sys`, `native-tls`, `libdbus-sys`.

**Зависимости** (версии — из локального реестра cargo, лицензии MIT или Apache-2.0):

| Крейт | Версия | Где | Зачем |
|---|---|---|---|
| `process-wrap` | `=10.0.1`, без функций по умолчанию; `tokio1`, `job-object`, `process-group`, `kill-on-drop`, `creation-flags` | `za-jail` | дерево процессов без `unsafe` в нашем коде |
| `nix` | `=0.31.3`, `signal`, `process` | `za-jail`, только Unix | `killpg`, `kill` без `unsafe` |
| `windows` | `=0.62.2`, `Win32_System_Threading` | `za-jail`, только Windows | тип флага `CREATE_NO_WINDOW` для `CreationFlags`; крейт уже в дереве |
| `sysinfo` | workspace | dev `za-jail` | проверка, что процессы дерева мертвы |

Прирост `Cargo.lock` от `za-jail`: 7 внешних пакетов — `process-wrap`, `nix`, `futures`, `futures-executor`, `futures-io`, `futures-macro`, `futures-sink`. Остальные зависимости (`windows`, `indexmap`, `tokio`, `libc`, `bitflags`, `cfg-if`) уже были в дереве.

## Отклонения от части 01

- **`process-wrap` — основной механизм дерева процессов**, а не запасной вариант PTY. PTY (`portable-pty`) в R1-C1 не нужен: `bash` идёт без терминала.
- **Пятая функция `creation-flags`** сверх четырёх в D73: без неё `JobObject` перезаписывает флаги создания процесса, и `nvidia-smi` терял бы прежний `CREATE_NO_WINDOW` (консольное окно у daemon без консоли). Для типа флага нужен прямой `windows` (уже в дереве).
- **`za-tools` зависит и от `za-core`, `za-protocol`**: контракт инструмента, пути и лексер команд лежат в `za-core` (D99), а не дублируются.
- **`za-secrets`** — новый крейт, которого нет в таблице. Хранилище ОС из плана убрано решением заказчика (в).
- **`nix` вместо `libc`** в `za-jail`: `killpg` и `kill` в `libc` — `unsafe`, а `nix` и так приходит через `process-wrap`. В `za-core` для одной константы `O_NOFOLLOW` остаётся `libc` (D98, задача 2).

## Альтернативы

- **Свой Job Object и группа через WinAPI и `libc`.** Требует `unsafe`, запрещённого в workspace.
- **`tokio::process` с `kill_on_drop` без дерева.** Убивает только главный процесс: внуки (`npm run dev`, `sleep &`) переживают тайм-аут и агента (OPS-013).
- **`wait()` Job или группы как признак конца.** Висит, пока фоновый потомок жив и держит каналы; отсюда отдельное ожидание главного процесса и `KILL_GRACE`.
- **Хранилище ОС (`keyring`) для секретов.** Отклонено заказчиком: секреты — в Vault памяти.

## Последствия

- Любой новый запуск процесса в продукте идёт через `za_jail::Runner`; правило `xtask policy` ловит обход. Изоляция SEC-100 придёт новой реализацией `Runner` без правки потребителей.
- Остаточные риски: `setsid` на Unix уводит процесс из группы и от `reap_leftovers`; на Unix `killpg` в конце вызова по номеру уже пустой группы теоретически может задеть новую группу с тем же номером (номер освобождается только вместе со всеми её процессами; окно — микросекунды). На Windows процесс, порождённый через WMI вне Job, не убивается (исследование 09).
- Если процесс вне Job или группы держит каналы вывода, рантайм tokio на Windows при остановке ждёт блокирующих потоков чтения; поэтому дерево убивается всегда, а тесты проверяют, что после вызова процессов не осталось.
