# ZeroAgent

Универсальный самостоятельно размещаемый агент на Rust с упором на локальные модели: веб-интерфейс, рои субагентов, периодические задания, нативная долговременная память [ZeroAIMemory](../ZeroAIMemory) и связь агентов через `zero-agent-relay`.

**Статус:** этап R1-A, модельный слой: коннекторы llama.cpp, vLLM, OpenRouter, Anthropic, OpenAI и совместимых серверов, общая очередь к моделям, монитор ресурсов. Бинарник `zero-agent` с командами `doctor`, `demo-run`, `replay` (R0), `models`, `ask`, `resources` (R1-A); цикл агента — R1-B, клиент памяти отложен до завершения ZeroAIMemory.

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

## Модели (R1-A)

Модели описываются в `ZA_HOME/config.toml`. Ключей в файле нет: эндпоинт ссылается на ключ именем `key_ref`, а сам ключ берётся из переменной окружения `ZA_SECRET_<KEY_REF>` (имя в верхнем регистре, `-` → `_`: `key_ref = "open-router"` → `ZA_SECRET_OPEN_ROUTER`). Облако не вызывается, пока ключ не задан: такой эндпоинт `inactive`. Моделью по умолчанию может быть только локальная. Раздел `[resources]` необязателен.

```toml
[models]
default = "local"

[[models.endpoints]]
id = "local"
backend = "llama_cpp"              # llama_cpp, vllm, openai_compatible, openrouter, anthropic, openai_responses, openai_chat
base_url = "http://127.0.0.1:8080" # llama.cpp — корень сервера; прочие — корень версии API (…/v1)
max_parallel = 2                   # не больше слотов сервера (-np)

[[models.endpoints]]
id = "claude"
backend = "anthropic"
model = "claude-sonnet-5-5"
key_ref = "anthropic"              # ключ — в ZA_SECRET_ANTHROPIC
daily_budget_usd = 5.0              # дневной предел расхода группы, USD
price = { input_per_mtok = 3.0, output_per_mtok = 15.0 }   # USD за миллион токенов; без price бюджет — ошибка (стоимость сообщает только openrouter)

[models.roles]
worker = "local"

[resources]                         # пороги допуска, МиБ; все поля необязательны
vram_reserve_mib = 1024             # запас VRAM для фоновых заданий
disk_margin_mib = 5120
nvidia_smi = "nvidia-smi"
```

Без раздела `[models]` и с `LLAMA_URL` используется один эндпоинт `local` по этому адресу.

```sh
zero-agent models                          # эндпоинты и состояние: ready, inactive, unreachable, probing, open, disabled; --json
zero-agent models probe local              # повторный зонд: контекст, слоты, квант, профиль в ZA_HOME/models
zero-agent ask --model local "Столица Франции?"   # ответ в stdout, usage в stderr; --priority interactive|top|worker|job
zero-agent resources                       # CPU, RAM, GPU/VRAM, тома, данные агента, очередь; --json, --watch
zero-agent doctor                          # + GPU и состояние каждой модели
```

`ask` на терминале печатает ответ по мере генерации; если стрим оборвался и запрос повторён, в stderr появляется пометка, а ответ печатается заново с новой строки. В конвейер или файл уходит только итоговый ответ. `models`, `ask` и `doctor` зондируют эндпоинты и сохраняют профили в `ZA_HOME/models`.

Коды выхода команд моделей: `0` — успех (у `models` — при любом состоянии эндпоинтов); `1` — ошибка конфигурации, неготовая модель (`ask`, `models probe`) или ошибка модели. Команды ждут первого зонда не дольше 35 с.

Живые проверки бэкендов: `LLAMA_TEST_URL=http://127.0.0.1:8080 cargo test -p za-llm --test live -- --ignored` (также `VLLM_TEST_URL`, `OPENROUTER_TEST_KEY`, `ANTHROPIC_TEST_KEY`, `OPENAI_TEST_KEY`, модель — `ZA_TEST_<PROVIDER>_MODEL`).

Тесты `Gateway` идут в реальном времени; на медленной машине длительности тестов растягивает `ZA_TEST_TIME_SCALE` (целое 1–8, по умолчанию 1, в CI — 4).

Логи пишутся в stderr, по умолчанию уровень `warn`. Фильтр задаётся `ZA_LOG` (приоритетнее) или `RUST_LOG`: например, `ZA_LOG=info` или `ZA_LOG=warn,za_store=debug`.

## Документы

| Документ | Что там |
|---|---|
| [docs/specs/2026-11-06-zero-agent-tz/](docs/specs/2026-11-06-zero-agent-tz/README.md) | Техническое задание |
| [docs/plans/2026-11-06-roadmap.md](docs/plans/2026-11-06-roadmap.md) | Дорожная карта и планы этапов |
| [docs/design/2026-11-07-ui-prototype.html](docs/design/2026-11-07-ui-prototype.html) | Интерактивный прототип интерфейса, открывается в браузере |
| [docs/research/2026-11-06-landscape/](docs/research/2026-11-06-landscape/) | Исследование: память, харнессы, фреймворки, продукты, бенчмарки, промпты, стек |
| [docs/adr/](docs/adr/) | Архитектурные решения (ADR) |
| [docs/journal.md](docs/journal.md) | Журнал работ |
| [CHANGELOG.md](CHANGELOG.md) | Изменения |

Лицензия: [MIT](LICENSE).
