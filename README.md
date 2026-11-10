# ZeroAgent

Универсальный самостоятельно размещаемый агент на Rust с упором на локальные модели: веб-интерфейс, рои субагентов, периодические задания, нативная долговременная память [ZeroAIMemory](../ZeroAIMemory) и связь агентов через `zero-agent-relay`.

**Статус:** этап R1-B1, цикл агента: ходы модели и инструментов поверх журнала, пауза, отмена, бюджеты, продолжение после сбоя (`za-agent`). Модельный слой R1-A: коннекторы llama.cpp, vLLM, OpenRouter, Anthropic, OpenAI и совместимых серверов, общая очередь к моделям, монитор ресурсов. Бинарник `zero-agent` с командами `doctor`, `demo-run`, `replay` (R0), `models`, `ask`, `resources` (R1-A), `run`, `answer` (R1-B1). Управление контекстом — R1-B2, инструменты работы с файлами и командами — R1-C, клиент памяти отложен до завершения ZeroAIMemory.

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

Тесты `Gateway` идут в реальном времени; на медленной машине длительности тестов растягивает `ZA_TEST_TIME_SCALE` (целое 1–6, по умолчанию 1, в CI — 4).

Логи пишутся в stderr, по умолчанию уровень `warn`. Фильтр задаётся `ZA_LOG` (приоритетнее) или `RUST_LOG`: например, `ZA_LOG=info` или `ZA_LOG=warn,za_store=debug`.

## Run (R1-B1)

`run` ведёт задачу через цикл агента: ходы модели, вызовы инструментов, пауза, продолжение. Журнал run — в `ZA_HOME/projects/<project_id>/journal.db`; каталог проекта должен существовать. Инструментов работы с файлами и командами пока нет (R1-C): модель отвечает текстом и может спросить пользователя (`ask_user`, тиры `standard` и `full`).

```sh
zero-agent run --project . "Ответь одним словом: столица Франции?"
#   stderr: run 019a…   stdout: Париж
zero-agent run --project DIR [--model ID] [--mode read-only|plan|ask|accept-edits|auto]
               [--work-mode coder|researcher|operator|writer] [--tier small|standard|full]
               [--max-wait СЕК] [--max-steps N] [--max-tokens N] [--max-usd USD] [--max-gpu-ms МС] [--max-duration СЕК] [--agents-md] "<задача>"
zero-agent run --project DIR --resume RUN [--model ID] [--input "<текст>"]
               [--max-steps N] [--max-tokens N] [--max-usd USD] [--max-gpu-ms МС] [--max-duration СЕК]
zero-agent answer --project DIR --interrupt ID (--text "<ответ>" | --approve | --deny [--note "<текст>"] | --done | --not-done)
```

- Первая строка stderr — `run <id>`, раньше любых предупреждений и логов: по нему run продолжают. Строки нет, только если у `--resume` нет журнала проекта. Ответ модели идёт в stdout по мере генерации, в конце — перевод строки.
- Модель по умолчанию — `models.default`. Неготовый эндпоинт (`inactive`, `unreachable`, зонд не завершился) — причина в stderr и код 1, run не создаётся. Отказ модели посреди run (429, обрыв, сервер недоступен) ставит run на паузу с причиной `provider`.
- Тир промпта выбирается по размеру контекста модели: меньше 32768 — `small`, меньше 65536 — `standard`, иначе `full`; `--tier` его переопределяет. `--max-wait` — предел ожидания в очереди к модели; без него заявка ждёт сколько нужно, по истечении — пауза `queue` или `resource`. `--agents-md` добавляет `AGENTS.md` проекта в промпт в пределах лимита роли.
- Ctrl+C — пауза run в безопасной точке: стрим модели обрывается, идущий инструмент дорабатывает, run встаёт в `paused` (`command`). Второй Ctrl+C — выход сразу, без ожидания паузы (код 4); `--resume` такой run продолжит.
- Модель спросила пользователя или ждёт одобрения — run на паузе, в stderr вопрос и строка `zero-agent: ответьте: zero-agent answer --interrupt <id> …`. `answer` записывает ответ и выходит, цикл продолжает `run --resume`.
- `--resume` продолжает run после паузы, после `blocked_on_budget` и после аварийного завершения процесса: шаг, оборванный сбоем, повторяется по журналу, и запрос к модели совпадает с прерванным байт в байт. `--model` меняет модель только на границе шага. `--input` добавляет сообщение пользователя.

Коды выхода `run`:

| Код | Значение |
|---|---|
| 0 | run `done` или `partial` |
| 1 | ошибка или `failed` (в stderr `zero-agent: run завершился ошибкой: <причина>`); в том числе неизвестный run у `--resume`, неготовая модель |
| 3 | нужно решение пользователя: ответ на вопрос (`needs_input`), одобрение (`needs_approval`), неизвестный исход действия (`needs_decision`) |
| 4 | прочая пауза (`provider`, `queue`, `resource`, `command`, …) или `blocked_on_budget` |
| 5 | `blocked`: модель трижды подряд ответила пустым ходом |
| 6 | `cancelled` |

На Windows аварийное завершение процесса (`abort`) тоже даёт код 3. Настоящее «нужно решение» отличает строка `zero-agent: ответьте:` в stderr.

У `answer` код 0 — ответ записан; 1 — нет журнала или прерывания, вид ответа не подходит к прерыванию (`--text` — для `input-…`, `--approve`/`--deny` — для `appr-…`, `--done`/`--not-done` — для `unk-…`) или прерывание уже решено.

## Бюджеты run (R1-B1)

Пределы расхода run по умолчанию задаёт раздел `[budgets.run]` того же `ZA_HOME/config.toml`. Все поля необязательны, поле без значения — без предела. Незнакомое имя поля — ошибка конфигурации: опечатка не снимает ограничение молча.

```toml
[budgets.run]
tokens = 200000        # input + output всех вызовов модели (input включает кэш)
gpu_ms = 600000        # время GPU, мс
usd = 2.0              # стоимость, USD
steps = 50             # ходов модели: 50-й ход последний
duration_ms = 1800000  # активное время run, мс: время в паузе не считается
```

Флаги `run` задают пределы поверх `[budgets.run]`, по одному на поле: `--max-steps` — `steps`, `--max-tokens` — `tokens`, `--max-usd` — `usd` (конечное неотрицательное число), `--max-gpu-ms` — `gpu_ms`, `--max-duration СЕК` — `duration_ms` (в секундах). Каждый флаг задаёт предел на весь run, включая уже потраченное, а не прибавку: run, остановленный на 20 шагах, с `--resume --max-steps 30` сделает ещё 10. Бюджет фиксируется при старте run и хранится в его снимке. `run --resume` без флагов бюджета продолжает с тем же бюджетом; `[budgets.run]` при продолжении не перечитывается. С флагами они меняют свои поля поверх текущего бюджета run, остальные пределы сохраняются. Так run в `blocked_on_budget` продолжают с новым пределом исчерпанного вида: подсказка в stderr называет нужный флаг (`run --resume <id> --max-steps 100`, `--max-duration 3600`, `--max-usd 5`).

Бюджет проверяется на границе шага, до начала следующего хода и до чтения нового ввода. `steps` ограничивает число ходов: при `steps = 2` третий ход не начинается. Остальные пределы срабатывают, когда расход их превысил; равенство пределу — ещё не превышение. Превышение записывается событием `budget.exceeded {kind, used, limit}` и переводит run в `blocked_on_budget`. Ввод, пришедший во время последнего хода, не теряется: его прочитает граница после продолжения run с новым бюджетом. Дневной бюджет облачной группы по-прежнему задаётся `daily_budget_usd` у эндпоинта в `[models]`.

## Контекст (R1-B2)

Раздел `[context]` того же `ZA_HOME/config.toml` задаёт тир run и пороги разгрузки контекста. Все поля необязательны. Незнакомое имя поля или значение вне диапазона — ошибка конфигурации.

```toml
[context]
tier = "standard"   # small | standard | full; без поля — по размеру окна модели
compact_at = 0.75   # компакция истории при заполнении окна, 0.70–0.80
prune_at = 0.65     # удаление старых результатов инструментов, меньше compact_at
spill_share = 0.15  # предел вывода инструмента в контексте, доля окна, 0.15–0.20
```

Флаг `--tier` важнее `[context].tier`. Раздел фиксируется при старте run и хранится в его снимке: `run --resume` продолжает с теми же значениями, даже если конфигурация изменилась.

Заполнение окна считается так: оценка запроса хода плюс резерв вывода хода сравнивается с долей окна. Оценка — расход прошлого хода (`usage`) плюс новые сообщения; новые сообщения у llama.cpp считает `/tokenize`, у остальных — байты / 4. Резерв — лимит вывода хода: не меньше бюджета мышления тира плюс 1024 токена на ответ. Если размер окна модели неизвестен, run считает его равным 8192 и пишет событие `context.alert {kind: "n_ctx_unknown"}`. Если сервер принял заметно меньше токенов, чем оценка запроса (меньше половины), run пишет `context.alert {kind: "truncation_suspected"}`: сервер, вероятно, запущен с меньшим окном и срезал начало промпта.

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
