# ZeroAgent

Универсальный самостоятельно размещаемый агент на Rust с упором на локальные модели: веб-интерфейс, рои субагентов, периодические задания, нативная долговременная память [ZeroAIMemory](../ZeroAIMemory) и связь агентов через `zero-agent-relay`.

**Статус:** этап R0, основание: журнал, эффекты, зонды. Бинарник `zero-agent` с командами `doctor`, `demo-run`, `replay`; клиент памяти отложен до завершения ZeroAIMemory.

## Быстрый старт

Тулчейн Rust лежит в проекте (`.toolchain/`), в систему ничего не ставится. Окружение подключается в той же оболочке, где идут команды:

```sh
. scripts/dev-env.sh          # PowerShell: . .\scripts\dev-env.ps1
cargo build -p zero-agent
cargo test --workspace --locked
```

Бинарник — `target/debug/zero-agent` (на Windows `zero-agent.exe`). Данные лежат в `ZA_HOME`; по умолчанию это `%LOCALAPPDATA%\zero-agent` или `~/.local/share/zero-agent`. Журнал проекта создаётся в `ZA_HOME/projects/<project_id>/journal.db`, не в каталоге проекта.

```sh
zero-agent doctor                          # SQLite, llama.cpp (LLAMA_URL), версия Rust; --json для машинного вывода
zero-agent demo-run --project ./demo --steps 5
#   1-я строка stdout — id run, затем done; в ./demo/log.txt строки step 1…step 5
zero-agent demo-run --project ./demo --steps 5 --crash-at execute:3   # аварийное завершение для проверки
zero-agent demo-run --project ./demo --steps 5 --resume <id run>      # продолжение; needs_decision и id прерываний
zero-agent replay --project ./demo --run <id run>                     # хэш значимых событий run
```

Коды выхода: `0` — готово; `1` — ошибка, в том числе неверные аргументы, `Fail` в `doctor` и расхождение журнала с кодом при `--resume`; `3` — нужно решение пользователя (исход необратимого действия неизвестен, run в `paused`).

Логи пишутся в stderr, по умолчанию уровень `warn`. Фильтр задаётся `ZA_LOG` (приоритетнее) или `RUST_LOG`: например, `ZA_LOG=info` или `ZA_LOG=warn,za_store=debug`.

## Документы

| Документ | Что там |
|---|---|
| [docs/specs/2026-11-06-zero-agent-tz/](docs/specs/2026-11-06-zero-agent-tz/README.md) | Техническое задание |
| [docs/plans/2026-11-06-roadmap.md](docs/plans/2026-11-06-roadmap.md) | Дорожная карта и планы этапов |
| [docs/design/2026-11-07-ui-prototype.html](docs/design/2026-11-07-ui-prototype.html) | Интерактивный прототип интерфейса, открывается в браузере |
| [docs/research/2026-11-06-landscape/](docs/research/2026-11-06-landscape/) | Исследование: память, харнессы, фреймворки, продукты, бенчмарки, промпты, стек |
| [docs/journal.md](docs/journal.md) | Журнал работ |
| [CHANGELOG.md](CHANGELOG.md) | Изменения |

Лицензия: [MIT](LICENSE).
