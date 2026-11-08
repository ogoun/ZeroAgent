# Бенчмарки агентов и дизайн harness для «арены» zero-agent (состояние на ноябрь 2026)

Как читать заметку. «Факт» — утверждение со ссылкой. «Вывод» — моё рассуждение, источника у него нет. README и служебные файлы репозиториев прочитаны напрямую через `gh api`. Статьи и сайты прочитаны через WebFetch или поисковые выдержки, то есть через промежуточное извлечение, поэтому числа оттуда стоит перепроверять перед публикацией в спецификации. Имена моделей оставлены так, как они написаны в источниках. Ни один бенчмарк не запускался, все оценки времени на нашем железе — расчётные.

## Карточки бенчмарков: что это, чего стоит, как запускается

### Takeaway
Все имена из списка существуют, кроме двух уточнений: `SWE-EVO` живёт теперь в организации `SWE-EVO/SWE-EVO`, а `Terminal-Bench` стал непрерывным бенчмарком (актуальна v4.1.0, а не 2.0). OSWorld 2.1 в списке заказчика указан дважды. Почти всё работает только на Linux/Docker, поэтому на Windows 11 реальная платформа арены — WSL2. Цифры лидеров почти нигде не сравнимы между версиями, а три пункта списка имеют подтверждённые серьёзные дефекты или загрязнение (SWE-bench Verified, OSWorld 2.1, публичная часть SWE-Bench Pro); то же верно для смежного BrowseComp.

### Cited Findings

#### Таблица соответствия имён
| Имя заказчика | Реальный объект | Комментарий |
|---|---|---|
| AgentBench FC | `THUDM/AgentBench`, версия Function Calling от 2025-10-10 | Точное совпадение — [README](https://github.com/THUDM/AgentBench) |
| WebArena-Verified | `ServiceNow/webarena-verified` | Точное — [README](https://github.com/ServiceNow/webarena-verified) |
| Terminal-Bench | TB 1 → 2.0 → 2.1 → непрерывные релизы 3.0, 4.0, 4.1 | Текущая `v4.1.0` от 2026-11-03 — [releases](https://github.com/harbor-framework/terminal-bench/releases) |
| SWE-bench Verified | Подмножество SWE-bench | Точное, но фактически выведен из оборота для фронтира — [OpenAI, выдача поиска](https://openai.com/index/why-we-no-longer-evaluate-swe-bench-verified/) |
| τ-bench 2.0 | `sierra-research/tau2-bench`, линия τ → τ² → τ³ (v1.0) → τ-bench 2.0 | Точное, не путать с τ² — [README](https://github.com/sierra-research/tau2-bench) |
| AgentDojo | `ethz-spylab/agentdojo` | Точное — [README](https://github.com/ethz-spylab/agentdojo) |
| OSWorld 2.1 | `xlang-ai/OSWorld-V2`, релиз `osworld-v2.1` | Точное, указан дважды — [README](https://github.com/xlang-ai/OSWorld-V2) |
| Long-Horizon Terminal-Bench 2.0 | LHTB 2.0, `zli12321/LHTB` | Точное — [README](https://github.com/zli12321/LHTB) |
| FrontierCS 2 | `FrontierCS/FrontierCS-2`, не путать с FrontierCS 1.0 | Точное — [README](https://github.com/FrontierCS/FrontierCS-2) |
| SWE-Bench Pro V2 | Публичный сплит V2 в `scaleapi/SWE-bench_Pro-os` | Точное — [README](https://github.com/scaleapi/SWE-bench_Pro-os) |
| BU Bench V2.1 | `browser-use/benchmark`, текущий релиз v2.1.1 | Точное — [README](https://github.com/browser-use/benchmark) |
| WildClawBench | `InternLM/WildClawBench` | Точное — [README](https://github.com/InternLM/WildClawBench) |
| UniClawBench | `HKU-MMLab/UniClawBench` | Точное — [README](https://github.com/HKU-MMLab/UniClawBench) |
| SWE-EVO | `bdqnghi/SWE-EVO` перенаправляется на `SWE-EVO/SWE-EVO` | Точное — [репозиторий](https://github.com/SWE-EVO/SWE-EVO) |
| General AgentBench | `cxcscmu/General-AgentBench` | Точное — [README](https://github.com/cxcscmu/General-AgentBench) |
| MCP-Universe / MCPMark | Два отдельных бенчмарка; MCP-Universe умеет запускать задачи MCPMark | [MCP-Universe](https://github.com/SalesforceAIResearch/MCP-Universe), [MCPMark](https://github.com/eval-sys/mcpmark) |

#### AgentBench FC
- **Измеряет:** LLM как агента в средах `alfworld`, `dbbench`, `knowledgegraph`, `os_interaction`, `webshop`, через function calling и AgentRL — [README](https://github.com/THUDM/AgentBench).
- **Задач:** для версии FC число не найдено.
- **Репо и лицензия:** Apache-2.0; [github.com/THUDM/AgentBench](https://github.com/THUDM/AgentBench). Последний push — 2026-02-08 (метаданные GitHub).
- **Запуск:** Docker Compose, контроллер AgentRL, Redis, MySQL, сервер Freebase (Virtuoso DB скачивается отдельно). `webshop` требует ~16 ГБ RAM, `alfworld` течёт по памяти и диску до рестарта воркера — [README](https://github.com/THUDM/AgentBench).
- **Интеграция:** своя через AgentRL; в каталоге адаптеров Harbor нет — [adapters](https://github.com/harbor-framework/harbor/tree/main/adapters). В inspect_evals есть каталог `agent_bench` — [src](https://github.com/UKGovernmentBEIS/inspect_evals/tree/main/src/inspect_evals), объём покрытия не проверен.
- **Время и цена:** не найдены.
- **Топ:** официальная таблица — Google Sheet, открыть не удалось. Сторонний [steel.dev](https://leaderboard.steel.dev/leaderboards/agentbench/): AgentRL + Qwen2.5-32B-Instruct 70.4%, Claude Sonnet 4.5 58.9%, строки помечены как самоотчёт и частично противоречат друг другу.
- **Дефекты:** утечка ресурсов в `alfworld`; слабая поддержка.
- **Windows (вывод):** средне, через WSL2; ценность низкая.

#### WebArena-Verified
- **Измеряет:** веб-агента на шести самохостимых сайтах (`shopping`, `shopping_admin`, `reddit`, `gitlab`, `wikipedia`, `map`) с детерминированными оценщиками — [README](https://github.com/ServiceNow/webarena-verified).
- **Задач:** 812, плюс Hard-подмножество на 258 — [README](https://github.com/ServiceNow/webarena-verified).
- **Репо и лицензия:** Apache-2.0; PyPI `webarena-verified`; HF `AmineHA/WebArena-Verified` — [README](https://github.com/ServiceNow/webarena-verified).
- **Запуск:** Docker-образы сайтов, уменьшенные до 92%; `wikipedia` требует ~100 ГБ данных, `map` ~60 ГБ, для `shopping` рекомендовано минимум 4 ГБ RAM — [docs/environments](https://github.com/ServiceNow/webarena-verified/tree/main/docs/environments).
- **Интеграция:** самый удобный контракт в списке. Агент отдаёт `agent_response` JSON и сетевой след `.har`, оценка идёт офлайн без живых сайтов — [README](https://github.com/ServiceNow/webarena-verified). Адаптера Harbor нет; есть интеграция в [BrowserGym](https://docsearch.algolia.com/mcp/docs/repo/servicenow/browsergym).
- **Время и цена:** не найдены.
- **Метрика:** детерминированная проверка ответа и сетевого следа, без LLM-судьи — [README](https://github.com/ServiceNow/webarena-verified).
- **Топ:** официального лидерборда не найдено; агрегаторы расходятся — [выдача](https://benchlm.ai/benchmarks/webarena-verified).
- **Дефекты:** каждая задача пересмотрена вручную, что снимает хрупкость исходного WebArena; независимого аудита не найдено.
- **Windows (вывод):** средне: Docker через WSL2, главная цена — диск. Без `wikipedia` и `map` экономится ~160 ГБ.

#### Terminal-Bench (2.0, 2.1, 4.x)
- **Измеряет:** работу агента в контейнеризированном терминале, включая биологию, отладку, безопасность — [TB 2.0](https://github.com/harbor-framework/terminal-bench-2).
- **Задач:** 2.0 и 2.1 — 89; в `tasks/` текущей ветки 68 записей (подсчёт через GitHub API); 4.1 добавил CPU-набор на 63 задачи — [релиз v4.1.0](https://github.com/harbor-framework/terminal-bench/releases/tag/v4.1.0), [TB 2.1](https://www.tbench.ai/news/terminal-bench-2-1).
- **Репо и лицензия:** Apache-2.0 — [github.com/harbor-framework/terminal-bench](https://github.com/harbor-framework/terminal-bench).
- **Запуск:** родной Harbor: `harbor run -d terminal-bench/terminal-bench@4.1.0`, CPU — `terminal-bench/terminal-bench-cpu@4.1.0`. Сопровождающие разрабатывают под Modal и советуют прогнать oracle 5 раз — [README](https://github.com/harbor-framework/terminal-bench).
- **Время:** с 4.0 единый лимит агента 8 часов — [TB 4.0](https://www.tbench.ai/news/terminal-bench-4-0).
- **Цена:** на лидерборд-прогон 4.0 Sonnet 5 потратил ~21.6 млрд токенов, Opus 5 ~6.5 млрд — [TB 4.0](https://www.tbench.ai/news/terminal-bench-4-0).
- **Метрика:** доля решённых задач.
- **Топ:** таблица 4.x на Harbor Hub не загрузилась при чтении. Последние прочитанные: 2.1 — GPT-5.3-Codex 79.1%, GPT-5.4 77.3% — [TB 2.1](https://www.tbench.ai/news/terminal-bench-2-1).
- **Дефекты:** в 2.0 исправлено 28 из 89 задач: 9 — дрейф внешних зависимостей, 8 — нехватка ресурсов, остальное — расхождение инструкции и тестов; у Opus 4.6 в Claude Code скор вырос с 58.0% до 70.1% только от правки задач — [TB 2.1](https://www.tbench.ai/news/terminal-bench-2-1). Инфраструктурный шум Anthropic — 6 п.п. между крайними конфигурациями ресурсов (p<0.01) — [Anthropic](https://www.anthropic.com/engineering/infrastructure-noise). В 4.0 удалено 8 задач, включая 2 с публичными решениями — [TB 4.0](https://www.tbench.ai/news/terminal-bench-4-0). В 4.1 усилена защита от reward hacking и закреплены образы — [релиз](https://github.com/harbor-framework/terminal-bench/releases/tag/v4.1.0).
- **Windows (вывод):** через WSL2 просто. Нативно Harbor на Windows имеет открытые баги (см. раздел про harness).

#### SWE-bench Verified
- **Измеряет:** правку реальных GitHub-issue на Python-репозиториях — [репо](https://github.com/SWE-bench/SWE-bench).
- **Задач:** 500 (по памяти, в этой сессии первичным источником не подтверждено); в Harbor есть `swebench-verified@1.0` — [registry.json](https://github.com/harbor-framework/harbor/blob/main/registry.json).
- **Лицензия:** MIT — [репо](https://github.com/SWE-bench/SWE-bench).
- **Запуск:** x86_64, от 120 ГБ диска, 16 ГБ RAM, 8 ядер; `max_workers` меньше `min(0.75·cpu, 24)`; в Docker Desktop надо расширить виртуальный диск — [README](https://github.com/SWE-bench/SWE-bench). Также sb-cli и Modal.
- **Интеграция:** предсказания-патчи (любой агент), Harbor, Inspect (`swe_bench`), HAL.
- **Топ:** перестал быть значимым.
- **Дефекты:** OpenAI 23 февраля 2026 отказалась от него, сославшись на дефектные тесты и загрязнение; аудит 138 задач; страница дала 403, поэтому опора на выдачу поиска — [OpenAI](https://openai.com/index/why-we-no-longer-evaluate-swe-bench-verified/). Прочитать полный текст не удалось, точный процент дефектов не указываю.
- **Windows (вывод):** средне, только WSL2; смысл ограничен сверкой с литературой.

#### τ-bench 2.0
- **Измеряет:** разговорного агента с инструментами и симулированным пользователем (airline, retail, telecom), текст и full-duplex голос, 6 языков; домен `intake` на 200+ задач — [README](https://github.com/sierra-research/tau2-bench).
- **Задач:** `text-v1` — 50 задач на домен, 900 вызовов на 1 trial по всем языкам; `voice-v1` — 278 вызовов — [README](https://github.com/sierra-research/tau2-bench).
- **Лицензия:** MIT.
- **Запуск:** чистый Python `>=3.12,<3.14` через `uv`, Docker не нужен для текста. Дымовой режим: `--domain retail --num-tasks 2` — [README](https://github.com/sierra-research/tau2-bench).
- **Интеграция:** Python-класс `HalfDuplexAgent`, Gym-интерфейс — [Agent Developer Guide](https://github.com/sierra-research/tau2-bench/blob/main/src/tau2/agent/README.md). Harbor: адаптер `tau3-bench` (линия 1.x) — [adapters](https://github.com/harbor-framework/harbor/tree/main/adapters). Inspect: `tau2`.
- **Время и цена:** не найдены.
- **Метрика:** награда по задаче, pass^1; судьи вроде естественности речи на награду не влияют.
- **Топ:** результатов 2.0 не найдено. Для 1.x: Claude Opus 4.5 85.3% в сторонней таблице — [developersdigest](https://www.developersdigest.tech/benchmarks/tau-bench); официальный taubench.com не прочитан.
- **Дефекты:** 2.0 несравним с 1.x; NL-судья `gpt-5.4-mini` и симулятор пользователя требуют внешнего LLM — [README](https://github.com/sierra-research/tau2-bench). В τ³ исправлено 75+ задач — [там же](https://github.com/sierra-research/tau2-bench). Команда HAL для надёжных метрик использует очищенные 26 задач airline из-за дефектов оценки — [HAL](https://hal.cs.princeton.edu/reliability).
- **Windows (вывод):** легко, самый дружелюбный из списка.

#### AgentDojo
- **Измеряет:** полезность и устойчивость агента к prompt injection через инструменты — [репо](https://github.com/ethz-spylab/agentdojo).
- **Задач:** по памяти 97 пользовательских задач и 629 проверок безопасности; в этой сессии не подтверждено.
- **Лицензия:** MIT.
- **Запуск:** `pip install agentdojo`, Docker только для `workspace_plus` в варианте Inspect — [README](https://github.com/ethz-spylab/agentdojo), [Inspect](https://github.com/UKGovernmentBEIS/inspect_evals/tree/main/src/inspect_evals/agentdojo).
- **Интеграция:** Python-пайплайн агента; Inspect `agentdojo`; адаптера Harbor нет.
- **Метрика:** utility, utility under attack, targeted ASR.
- **Топ:** страница прямо говорит, что это не лидерборд. Максимум utility без атаки 88.66%, под атакой 77.27% (claude-3-7-sonnet), минимум ASR 1.11% — [results](https://agentdojo.spylab.ai/results/). Данные старые.
- **Дефекты:** матрица модель × атака неполна; набор моделей устарел.
- **Windows (вывод):** легко.

#### OSWorld 2.1
- **Измеряет:** computer use на длинных реальных задачах с checkpoint-оценкой — [Epoch](https://epoch.ai/benchmarks/osworld-2-1/review).
- **Задач:** 108 — [Epoch](https://epoch.ai/benchmarks/osworld-2-1/review); существует офлайн-подмножество на 82 задачи — [BenchLM](https://benchlm.ai/benchmarks/osworld-2-1).
- **Репо и лицензия:** Apache-2.0; задачи и ассеты на HF, часть gated — [README](https://github.com/xlang-ai/OSWorld-V2).
- **Запуск:** VM с Ubuntu; Docker для Linux-хостов с KVM или AWS; нужен self-host сайтов `Task-Web/OSWorld-web` и строгая привязка всех компонентов к одному релизу. Хост от 50 ГБ, `--max_steps` 200–500 — [README](https://github.com/xlang-ai/OSWorld-V2), [AWS-гид](https://github.com/xlang-ai/OSWorld-V2/blob/main/docs/PUBLIC_EVALUATION_GUIDELINE_v2.1.md). Также упомянут VMware и, менее зрело, VirtualBox — [провайдеры](https://github.com/xlang-ai/OSWorld-V2/blob/main/docs/PROVIDER_SETUP.md).
- **Интеграция:** Python-раннер `mm_agents/`, скриншотный агент. Harbor-адаптер `osworld` покрывает только **старый** OSWorld-Verified (361 из 369 задач, QEMU/KVM) — [адаптер](https://github.com/harbor-framework/harbor/tree/main/adapters/osworld). Inspect `osworld` покрывает подмножество версии 1 — [Inspect](https://github.com/UKGovernmentBEIS/inspect_evals/tree/main/src/inspect_evals/osworld).
- **Время и цена:** не найдены. В AWS-гиде упомянута покупка резидентного IP (около $1 за ГБ) для части сценариев — [гид](https://github.com/xlang-ai/OSWorld-V2/blob/main/docs/PUBLIC_EVALUATION_GUIDELINE_v2.1.md).
- **Метрика:** partial score (среднее по чекпойнтам) и strict pass.
- **Топ:** Claude Opus 5.5 81.8% partial, по лабораторным самоотчётам — [BenchLM](https://benchlm.ai/benchmarks/osworld-2-1); 48.7 binary — [выдача](https://scholarcompass.net/en/tasks/computer-use-agents/benchmarks/osworld-2-1); противоречие: llm-stats называет лидером Gemini 4 Carbon, 0.815 — [llm-stats](https://llm-stats.com/benchmarks/osworld-2.1-partial).
- **Дефекты:** вердикт Epoch — **Flawed**. Ошибки в 36 из 50 проверенных задач: 40 ошибок, из них 24 ложноотрицательные, 12 ложноположительные, 4 невозможные задачи; верхняя оценка влияния 50.4% максимума; агент Astra потерял ~20.8% скора на затронутых задачах — [Epoch](https://epoch.ai/benchmarks/osworld-2-1/review).
- **Windows (вывод):** тяжело. WSL2 на Windows 11 по умолчанию разрешает nested virtualization — [Microsoft](https://learn.microsoft.com/en-us/windows/wsl/wsl-config), но работоспособность KVM-образа OSWorld в ней не проверена. Запасной вариант — Linux-машина за relay.

#### Long-Horizon Terminal-Bench 2.0
- **Измеряет:** устойчивый прогресс агента на сотнях шагов; проверка скрытым rebuild-from-artifact верификатором — [README](https://github.com/zli12321/LHTB).
- **Задач:** 77 (46 старых и 31 новая), 13 доменов — [README](https://github.com/zli12321/LHTB).
- **Репо и лицензия:** Apache-2.0; HF `IntelligenceLab/Long-Horizon-Terminal-Bench` и `LHTB-leaderboard` с траекториями — [README](https://github.com/zli12321/LHTB).
- **Запуск:** **модифицированный Harbor из репозитория**. Стоковый Harbor игнорирует `continue_until_timeout`, задачи идут в один проход и скорят ниже — [README](https://github.com/zli12321/LHTB). Прогоны авторов — на Daytona. Требования к диску/RAM/GPU не указаны — [arXiv](https://arxiv.org/html/2610.14146v1).
- **Интеграция:** Harbor-агент; замеры сделаны через Terminus-2.
- **Время и цена:** 3 часа на задачу, чекпойнт каждые 30 минут; в среднем $48.41 на задачу по прейскуранту без кэша; полный сьют от ~$285 (Hunyuan hy4) до ~$30,581 (GPT-6-astra) — [arXiv](https://arxiv.org/html/2610.14146v1).
- **Метрика:** плотная награда R; solved при R≥0.95, strict при R=1.
- **Топ:** GPT-6-astra 0.692 и 37/77; GPT-5.6-sol 0.672 и 32/77; из открытых весов DeepSeek V4 Flash 0.524 и 10/77 — [README](https://github.com/zli12321/LHTB).
- **Дефекты:** на прогоне с промежуточным верификатором в песочнице агента 14 из 17 идеальных результатов получены чтением оценщика (закомментированный блок README) — [README, исходник](https://raw.githubusercontent.com/zli12321/LHTB/main/README.md). Лестница реконструкции даёт 0.5–0.67 даже без полного решения, сравнивать надо solved, а не mean. Один trial на задачу, значит CI по повторам нет. Часть задач без скора считается нулём — [README](https://github.com/zli12321/LHTB).
- **Важное для быстрой итерации:** медианная модель набирает ~2/3 финальной награды за 30 минут — [README](https://github.com/zli12321/LHTB).
- **Windows (вывод):** через WSL2, но модифицированный Harbor и 3-часовые задачи делают его только on-demand.

#### FrontierCS 2
- **Измеряет:** может ли агент превзойти опубликованный человеческий метод на реальном исследовательском коде — [README](https://github.com/FrontierCS/FrontierCS-2).
- **Задач:** 48 из 150, 13 доменов — [README](https://github.com/FrontierCS/FrontierCS-2).
- **Лицензия:** CC-BY-4.0 (метаданные GitHub). Прогоны — HF `FrontierCS2/FrontierCS-2-runs`.
- **Запуск:** **AWS EKS, EFS, Terraform, Modal для GPU** — [README](https://github.com/FrontierCS/FrontierCS-2). 1–8 CPU, 2–32 ГиБ; GPU-задачи на L4/A10G/A100/H100 — [arXiv](https://arxiv.org/html/2610.29932v1).
- **Интеграция:** свой driver и `frontier-submit`; модель через OpenAI-совместимый шлюз; не Harbor.
- **Время и цена:** 5 часов агента; финальная оценка от 2 до 1,800 минут — [arXiv](https://arxiv.org/html/2610.29932v1). Цена на задачу приведена только на графике.
- **Метрика:** pass только при превосходстве над эталоном по всем метрикам и нагрузкам; FCI и FDI.
- **Топ:** Claude Opus 5.5 в Claude Code — 58% — [arXiv](https://arxiv.org/html/2610.29932v1).
- **Дефекты:** отказы провайдера считаются провалами; одно из заданий без held-out-набора — [arXiv](https://arxiv.org/html/2610.29932v1).
- **Windows (вывод):** локально не предусмотрен. Альтернатива — FrontierCS 1.0 Algorithmic с адаптером Harbor и `frontier eval` на Docker — [README](https://github.com/dyxg/Frontier-CS).

#### SWE-Bench Pro V2
- **Измеряет:** длинные SE-задачи на Python, Go, JS/TS — [README](https://github.com/scaleapi/SWE-bench_Pro-os), [arXiv](https://arxiv.org/html/2610.24206v1).
- **Задач:** 642 в 11 репозиториях, HARD-51 — [v2/README](https://github.com/scaleapi/SWE-bench_Pro-os/blob/main/v2/README.md).
- **Лицензия:** MIT на код; задачи наследуют лицензии исходных репозиториев — [arXiv](https://arxiv.org/html/2610.24206v1).
- **Запуск:** родные Harbor-задачи, образы `ghcr.io/scaleapi/swe-bench_pro-v2:<id>` (linux/amd64); примеры на Modal, Modal ≥1.5.1 нужен для пофазовой сетевой политики — [v2/README](https://github.com/scaleapi/SWE-bench_Pro-os/blob/main/v2/README.md). Локальный Docker в v2 не описан.
- **Интеграция:** `locked_*` агенты плюс `patch_replay:PatchReplayAgent`; любой OpenAI-совместимый эндпоинт через `--ae OPENAI_API_BASE` — [v2/README](https://github.com/scaleapi/SWE-bench_Pro-os/blob/main/v2/README.md).
- **Время и цена:** 50 минут на задачу; средние расходы ~$2.3–3 — [arXiv](https://arxiv.org/html/2610.24206v1).
- **Топ:** Opus 5 99.4%, HARD-51 98.0%; Kimi K3 97.7% в блоге и 97.5% в статье (расхождение) — [блог](https://labs.scale.com/blog/swe-bench-pro-v2), [arXiv](https://arxiv.org/html/2610.24206v1).
- **Дефекты:** 480 из 731 задач исправлены или удалены; передовая модель копировала решения с GitHub на 351 решённой задаче, и лишь 237 устояли при блокировке GitHub; поддельные зависимости — [arXiv](https://arxiv.org/html/2610.24206v1). Разрыв public/private у Opus 5: 638/642 против 222/272, ~17.8 п.п. — [блог](https://labs.scale.com/blog/swe-bench-pro-v2). Часы экспертов расходятся: 404 в статье, 1,897 в блоге, вероятно, разный охват.
- **Windows (вывод):** только WSL2. Сетевая блокировка на Docker заявлена в таблице возможностей Harbor — [sandboxes](https://github.com/harbor-framework/harbor/blob/main/docs-mintlify/sandboxes/pre-integrated-sandboxes.mdx), но с `locked_*` локально не проверена.

#### BU Bench V2.1
- **Измеряет:** браузерного агента на реальных сайтах — [README](https://github.com/browser-use/benchmark).
- **Задач:** 200; legacy V1 — 100, Stealth — 80 — [README](https://github.com/browser-use/benchmark), [выдача](https://github.com/browser-use/benchmark/blob/main/README.md).
- **Лицензия:** SPDX GitHub не определяет; тексты зашифрованы, публикация запрещена — [README](https://github.com/browser-use/benchmark).
- **Запуск:** «Use macOS or Linux»; `uv`, BrowserCode 0.1.20; `--browser local_headless` с локальным Chrome — [README](https://github.com/browser-use/benchmark).
- **Интеграция:** `--executor` выбирает BrowserCode или browser-use; интерфейса для произвольного агента README не описывает. Адаптера Harbor нет.
- **Время и цена:** лимит 3,600 с на задачу, по умолчанию 100 параллельно; цена не указана — [README](https://github.com/browser-use/benchmark).
- **Метрика:** взвешенный рубричный скор с partial credit; судья LLM (`gpt-5.6-luna`, xhigh) — [README](https://github.com/browser-use/benchmark).
- **Топ:** публичных результатов V2.1 не найдено; графики репозитория — от старого набора на 60 задач и несравнимы — [README](https://github.com/browser-use/benchmark).
- **Дефекты:** живой веб и CAPTCHA, задача 185 переделана, проверка на сохранённых данных не завершена — [README](https://github.com/browser-use/benchmark). Автор бенчмарка — производитель агента.
- **Windows (вывод):** официально нет, через WSL2.

#### WildClawBench
- **Измеряет:** end-to-end работу в реальном окружении: продуктивность, код, общение, поиск, творчество, безопасность — [README](https://github.com/InternLM/WildClawBench).
- **Задач:** 60, двуязычные, 6 категорий — [README](https://github.com/InternLM/WildClawBench).
- **Лицензия:** MIT; HF `internlm/WildClawBench`, `-Harbor`, `-Trajectories`.
- **Запуск:** Docker-контейнер на задачу; образы-тарболы по харнессам; `harbor run -p ./WildClawBench-Harbor` — [README](https://github.com/InternLM/WildClawBench). Размеры образов не указаны.
- **Интеграция:** **любой Harbor-агент** — [README](https://github.com/InternLM/WildClawBench).
- **Время и цена:** 222–708 минут и $2–96 на полный сьют при последовательном счёте для большинства моделей — [README](https://github.com/InternLM/WildClawBench).
- **Метрика:** взвешенный скор: правила, аудит состояния среды, LLM/VLM-судья — [arXiv](https://arxiv.org/html/2605.10912v1).
- **Топ:** GPT-5.6 Sol 67.2%. Модели весового класса локального запуска: Qwen3.8 27B 48.0% (self-hosted vLLM), Muse Glimmer 30B 47.6%, Gemma 4 31B IT 37.6% — [README](https://github.com/InternLM/WildClawBench).
- **Дефекты:** смена харнесса при той же модели даёт до 18 п.п. — [выдача arXiv](https://arxiv.org/html/2605.10912v1); наблюдаемо: MiMo V2 Pro 29.9→48.1 — [README](https://github.com/InternLM/WildClawBench). Систематического аудита безопасности нет — [AcademiClaw](https://arxiv.org/pdf/2605.02661). Живой поиск вносит дрейф (вывод). Оценщики внедряются только после завершения агента — [README](https://github.com/InternLM/WildClawBench).
- **Windows (вывод):** средне, Docker на WSL2.

#### UniClawBench
- **Измеряет:** проактивного агента: навыки, исследование, длинный контекст, мультимодальность, кросс-платформа — [README](https://github.com/HKU-MMLab/UniClawBench).
- **Задач:** 400 (200 EN и 200 ZH, по 40 на способность) — [README](https://github.com/HKU-MMLab/UniClawBench).
- **Лицензия:** MIT.
- **Запуск:** Docker + buildx, Python 3.11+, ассеты ~4.1 ГБ; по умолчанию сборка идёт через китайские зеркала, переопределяются — [README](https://github.com/HKU-MMLab/UniClawBench). Хосты авторов: i7-13700, 16 ГБ, контейнер 2 ГБ — [arXiv](https://arxiv.org/html/2607.08768v1).
- **Интеграция:** свой бэкенд в `lib/runner/`; исполнители OpenClaw, Nanobot, EDICT; адаптера Harbor нет. Супервизор и симулятор — агенты Codex на GPT-5.4 — [arXiv](https://arxiv.org/html/2607.08768v1).
- **Время и цена:** ~17.4 минуты на задачу; лимит 30 минут, для long-context 45; ~0.8–1.2 млн входных токенов на задачу — [arXiv](https://arxiv.org/html/2607.08768v1).
- **Метрика:** Pass Rate и Average Score.
- **Топ:** Claude Opus-4.8 PR 0.475, AS 0.702; GPT-5.4 лучший AS 0.774 — [arXiv](https://arxiv.org/html/2607.08768v1).
- **Дефекты:** зависимость от LLM-судьи: согласие с людьми 92.0% по pass/fail, Pearson r=0.71 по скору, на 50 траекториях; живая среда — [arXiv](https://arxiv.org/html/2607.08768v1). Рецензирования нет.
- **Windows (вывод):** средне; скрипты на bash, Docker на WSL2. Полезная деталь: runtime-статусы `infra_error`, `rate_limit`, `budget_exhausted` отделены от вердикта — [README](https://github.com/HKU-MMLab/UniClawBench).

#### SWE-EVO
- **Измеряет:** эволюцию ПО по release notes/SRS, многошаговые изменения — [arXiv](https://arxiv.org/html/2512.18470v2).
- **Задач:** 48, в среднем 21 файл — [выдача arXiv](https://arxiv.org/html/2512.18470v2). Примеры проектов расходятся: README называет Django и NumPy, аннотация — scikit-learn и pydantic.
- **Лицензия:** MIT — [репо](https://github.com/SWE-EVO/SWE-EVO).
- **Запуск и интеграция:** скрипты OpenHands и SWE-agent, затем `evaluate_instance.py --trajectories_path`; адаптера Harbor нет — [репо](https://github.com/SWE-EVO/SWE-EVO).
- **Время и цена:** не найдены.
- **Метрика:** Fix Rate с частичным прогрессом.
- **Топ:** GPT-5.4 + OpenHands 25% (GPT-5 21% в более ранней версии) против 65% на SWE-bench Verified — [выдача arXiv](https://arxiv.org/html/2512.18470v2).
- **Дефекты:** мало задач, нет лидерборда. Сильные модели проваливаются прежде всего на следовании инструкциям — [там же](https://arxiv.org/html/2512.18470v2).
- **Windows (вывод):** средне, как SWE-bench.

#### General AgentBench
- **Измеряет:** универсального агента, которому одновременно доступны все инструменты, и test-time scaling — [README](https://github.com/cxcscmu/General-AgentBench).
- **Задач:** 496 выбранных из 3,623 в шести наборах: BrowseComp 124, WebVoyager 65, SWE-bench Verified 50, Terminal-Bench 80, MathHay 75, Tau2 50, MCP-Bench 52 — [arXiv](https://arxiv.org/html/2602.18998v1).
- **Лицензия:** MIT. Последний push — 2026-04-14 (метаданные GitHub).
- **Запуск:** зависит от инфраструктуры всех вложенных бенчмарков; LiteLLM и Serper — [README](https://github.com/cxcscmu/General-AgentBench).
- **Интеграция:** Host как единый MCP-интерфейс: агент общается только с ним — [README](https://github.com/cxcscmu/General-AgentBench), [arXiv](https://arxiv.org/abs/2602.18998v1).
- **Время и цена:** время не указано; масштабирование обошлось примерно в $29.6 тыс. (параллельное) и $24.4 тыс. (последовательное) — [arXiv](https://arxiv.org/html/2602.18998v1).
- **Топ:** средний Claude Sonnet 4.5 48.0, GPT-5 45.9 — [arXiv](https://arxiv.org/html/2602.18998v1).
- **Находки:** потери при переходе к общему режиму от −0.2% до −31.2%; context ceiling при последовательном масштабировании и разрыв проверяемости при параллельном — [arXiv](https://arxiv.org/html/2602.18998v1), [README](https://github.com/cxcscmu/General-AgentBench).
- **Дефекты:** до 4 сэмплов из-за цены; вложенные наборы устаревают (SWE-bench Verified и TB 2.0).
- **Windows (вывод):** тяжело: сумма требований всех вложенных наборов.

#### MCP-Universe
- **Измеряет:** LLM на реальных MCP-серверах: 6 доменов, 11 серверов — [arXiv](https://arxiv.org/abs/2508.14704).
- **Задач:** 231 по агрегатору, первичной цифры не нашёл — [выдача](https://www.sourcepulse.org/projects/13026377).
- **Лицензия:** Apache-2.0; PyPI `mcpuniverse`.
- **Запуск:** Python, Docker для части серверов, ключи сервисов (SerpAPI и др.) — [README](https://github.com/SalesforceAIResearch/MCP-Universe).
- **Интеграция:** свой фреймворк агента; пользовательские бенчмарки через конфиги; адаптера Harbor нет.
- **Топ:** GPT-5 43.72%, Grok-4 33.33%, Claude-4.0-Sonnet 29.44% — [arXiv](https://arxiv.org/abs/2508.14704).
- **Дефекты:** динамические оценщики берут живую истину, скор зависит от даты запуска — [arXiv](https://arxiv.org/abs/2508.14704).
- **Windows (вывод):** средне; заметная цена — ключи внешних API.

#### MCPMark (Verified)
- **Измеряет:** сложное использование MCP: Notion, GitHub, Filesystem, Postgres, Playwright — [README](https://github.com/eval-sys/mcpmark).
- **Задач:** 127 standard и 10 easy на сервис — [README](https://github.com/eval-sys/mcpmark).
- **Лицензия:** Apache-2.0.
- **Запуск:** локально или Docker, проверено на macOS и Linux; Filesystem не требует аккаунтов; Notion и GitHub требуют учётных записей — [README](https://github.com/eval-sys/mcpmark).
- **Интеграция:** `MCPMarkAgent` или `--agent react`, новые скаффолды через PR — [README](https://github.com/eval-sys/mcpmark). Адаптера Harbor нет.
- **Метрика:** pass@1, pass@k, pass^k, avg@k — встроенный учёт стабильности — [README](https://github.com/eval-sys/mcpmark).
- **Топ:** gpt-5.5 (xhigh) 92.9%, kimi-k2.7 81.1% на Verified — [README](https://github.com/eval-sys/mcpmark).
- **Дефекты:** Verified закрепил версии серверов и верификаторы; старые результаты объявлены несравнимыми — [PR #264](https://github.com/eval-sys/mcpmark/pull/264). Для фронтира близок к насыщению.
- **Windows (вывод):** средне; `easy` + Filesystem и Postgres годятся в CI.

### Inferences
- На фронтирных моделях часть бенчмарков насыщена (SWE-Bench Pro V2, MCPMark), но у локальных весов этого может не быть. Насыщение надо мерить на нашей связке, а не переносить с чужих таблиц.
- LLM-судьи и симуляторы (BU Bench, UniClawBench, τ, WildClawBench) ломают чисто локальный контур. Судью надо зафиксировать как отдельную версионируемую зависимость и учитывать её цену отдельно от цены агента.
- Лимиты в минутах откалиброваны под облачную скорость вывода, поэтому локальная модель проигрывает по таймауту, а не по способностям. Арене нужен второй режим с бюджетом в токенах, отчётливо помеченный как несравнимый с публичным.
- Расчётные верхние границы последовательного прогона: SWE-Bench Pro V2 ~535 ч, HARD-51 ~42.5 ч, LHTB 2.0 ~231 ч, FrontierCS 2 ~240 ч, UniClawBench EN ~58 ч при среднем времени авторов. Полные прогоны на одной GPU нереалистичны как регулярная практика.

### Gaps
- Текущий лидерборд TB 4.x, официальные таблицы AgentBench FC, τ-bench и BU Bench V2.1 прочитать не удалось.
- Точные числа по SWE-bench Verified: страница OpenAI вернула 403.
- Размеры Docker-образов и требования к GPU для LHTB, WildClawBench, SWE-Bench Pro V2 не опубликованы в прочитанных материалах.
- Время на задачу для OSWorld 2.1, WebArena-Verified, SWE-EVO, τ-bench 2.0, MCPMark не найдено.
- Статистики на локальной связке нет ни у кого, кроме WildClawBench. Скорости на нашей рабочей станции предстоит измерить.

## Какие единые harness существуют и какой покроет большую часть списка

### Takeaway
Брать за основу Harbor: он родной для 5 из 17 пунктов и закрывает ещё 2 старыми версиями, снижая стоимость интеграции с O(mn) до O(m+n). HAL архивирован, Inspect AI оставить вторым каналом для AgentDojo, Safety и через мост `inspect_harbor`. Для остальных бенчмарков писать адаптеры Harbor выборочно, а не под все. Сам Harbor запускать в WSL2, потому что нативный Windows-контур содержит открытые дефекты.

### Cited Findings
- **Harbor.** Apache-2.0, релиз v0.25.0 от 2026-10-25 — [релизы](https://github.com/harbor-framework/harbor/releases). Задача — instruction, environment, tests, solution; адаптеры сводят интеграцию O(mn) к O(m+n); более 80 портированных бенчмарков, 22 агента на май 2026 — [arXiv](https://arxiv.org/html/2609.04298v1). Сейчас в каталоге `installed` заметно больше агентов, включая `acp`, `hermes`, `openclaw`, `goose`, `opencode` — [код](https://github.com/harbor-framework/harbor/tree/main/src/harbor/agents/installed).
- **Как подключить агента.** `BaseInstalledAgent` (предпочтительно): `install`, `run`, `populate_context_post_run`, `configure`; `BaseAgent`, если цикл живёт вне среды; `-a module.path:ClassName`; `--ak skip_install=true` — [документация](https://docs.harborframework.com/agents/custom-agents). Также есть универсальный агент ACP с типами дистрибуции `binary`, `npx`, `uvx`, `local` — [acp.py](https://github.com/harbor-framework/harbor/blob/main/src/harbor/agents/installed/acp.py).
- **ATIF.** Текущая версия v1.8, Harbor принимает v1.0–v1.8; `schema_version`, `agent`, `steps`, `tool_calls`, `observation`, `metrics`, `final_metrics`, `subagent_trajectories`; валидатор `harbor.utils.trajectory_validator` — [документация](https://docs.harborframework.com/agents/atif).
- **Покрытие из каталога адаптеров и `registry.json`.** Есть: `swebench-verified`, `swebenchpro@1.0`, `terminal-bench@2.0`, `terminal-bench-sample@2.0`, `tau3-bench`, `osworld` (Verified), `frontier-cs-algorithm`, `locomo`, `gaia`, `gaia2`, `hle`, `theagentcompany`, `cooperbench`, `programbench`, `featurebench`, `swelancer`, `widesearch` — [adapters](https://github.com/harbor-framework/harbor/tree/main/adapters), [registry](https://github.com/harbor-framework/harbor/blob/main/registry.json). Нет: WebArena, AgentDojo, AgentBench, MCPMark, MCP-Universe, BU Bench, UniClawBench, SWE-EVO, General AgentBench — там же. `registry.json` содержит 80 записей, а Harbor Hub может содержать больше.
- **Вне основного репозитория, но в формате Harbor:** TB 4.x, SWE-Bench Pro V2, WildClawBench-Harbor, LHTB (с патчем) — [TB](https://github.com/harbor-framework/terminal-bench), [SWE Pro](https://github.com/scaleapi/SWE-bench_Pro-os/blob/main/v2/README.md), [WildClaw](https://github.com/InternLM/WildClawBench), [LHTB](https://github.com/zli12321/LHTB).
- **Качество адаптеров.** Многотриальная parity с исходником и трёхступенчатый аудит — [arXiv](https://arxiv.org/html/2609.04298v1); сводка — [parity_summary.csv](https://github.com/harbor-framework/harbor/blob/main/adapters/parity_summary.csv).
- **Среды.** `docker` (дефолт), `podman`, `singularity`, `ssh`, `kata` и многие облачные; Windows-контейнеры заявлены для `docker`, `daytona`, `cua-cloud`, `use-computer` — [таблица](https://github.com/harbor-framework/harbor/blob/main/docs-mintlify/sandboxes/pre-integrated-sandboxes.mdx), [код](https://github.com/harbor-framework/harbor/tree/main/src/harbor/environments). Страница про Windows-задачи — пока заглушка — [docs](https://github.com/harbor-framework/harbor/blob/main/docs-mintlify/tasks/windows-tasks.mdx).
- **Открытые Windows-баги Harbor:** CRLF ломает верификаторы (#1240), `uv tool install harbor` не ставится из-за колеса litellm (#2478), CRLF в списках файлов (#4091) — [issues](https://github.com/harbor-framework/harbor/issues?q=windows). Вопрос о числе адаптеров с Windows-задачами без ответа — [#3332](https://github.com/harbor-framework/harbor/issues/3332).
- **Inspect AI.** MIT. В `inspect_evals` присутствуют `agent_bench`, `agentdojo`, `browse_comp`, `frontier_cs`, `gaia`, `gdpval`, `hle`, `osworld`, `swe_bench`, `swe_lancer`, `tau2`, `theagentcompany` — [каталог](https://github.com/UKGovernmentBEIS/inspect_evals/tree/main/src/inspect_evals). Агент-мост запускает прокси в песочнице и позволяет работать агенту на любом языке, включая собственный бинарник, говорящий по API OpenAI, Anthropic или Google — [agent bridge](https://inspect.aisi.org.uk/agent-bridge.html). Мост `inspect_harbor` гоняет задачи Harbor через Inspect, MIT — [репо](https://github.com/meridianlabs-ai/inspect_harbor).
- **HAL.** Архивирован: результаты через этот harness больше не обновляются — [README](https://github.com/princeton-pli/hal-harness).
- **Устаревший harness Terminal-Bench:** заменён Harbor, миграция через подкласс `BaseInstalledAgent` — [TB 2.0](https://github.com/harbor-framework/terminal-bench-2).
- **Прочие.** BrowserGym покрывает веб-бенчмарки (WebArena Verified, WorkArena и др.) — [выдача](https://docsearch.algolia.com/mcp/docs/repo/servicenow/browsergym). EvalScope оборачивает tau2 — [docs](https://evalscope.readthedocs.io/en/latest/third_party/tau2_bench.html). Также Opik читает метрики ATIF — [docs](https://www.comet.com/docs/opik/integrations/harbor).

| Бенчмарк | Harbor | Inspect | Адаптер zero-agent (вывод) |
|---|---|---|---|
| Terminal-Bench 4.x | родной | через мост | не нужен |
| LHTB 2.0 | патч | нет | не нужен, но форк Harbor |
| SWE-Bench Pro V2 | родной | через мост | `locked_*` по образцу |
| SWE-bench Verified | реестр | есть | не нужен |
| WildClawBench | родной формат | через мост | не нужен |
| τ-bench 2.0 | только линия 1.x | `tau2` | Python-прокладка к `HalfDuplexAgent` |
| OSWorld 2.1 | только Verified | только v1 | `mm_agents`, дорого |
| AgentDojo | нет | есть | Inspect-мост |
| WebArena-Verified | нет | нет | адаптер, контракт простой |
| MCPMark | нет | нет | адаптер, тонкий |
| UniClawBench | нет | нет | бэкенд в `lib/runner` |
| BU Bench | нет | нет | `--executor` в форке |
| SWE-EVO | нет | нет | экспорт патчей |
| General AgentBench | нет | нет | MCP Host |
| MCP-Universe | нет | нет | откладывать |
| FrontierCS 2 | нет | нет | откладывать |
| AgentBench FC | нет | частично | отбросить |

### Inferences
- Рекомендуемая схема: zero-agent собирается как статический Linux-бинарник (musl), тонкий Python-класс `ZeroAgent(BaseInstalledAgent)` загружает его в контейнер и запускает, бинарник сам пишет `trajectory.json` в ATIF v1.8. Если zero-agent реализует ACP, можно обойтись без своего класса, но ATIF всё равно лучше писать нативно ради трассировки.
- Модельный эндпоинт идёт на локальный сервер на хосте, доступ из песочницы разрешается как единственный хост, по образцу `--allow-agent-host` из SWE-Bench Pro. Это заодно закрывает утечки из сети.
- Арена не должна быть новым harness. Это слой поверх Harbor: реестр прогонов, статистика, гейты, таблица. `result.json`, `failure_cause` и `reward_valid` дают сырьё бесплатно.
- Harbor запускать в WSL2 на Windows, данные держать в ext4 внутри VHD, а не на `/mnt/*`. Windows-нативное поведение zero-agent нужно проверять отдельным собственным набором задач, поскольку публичные бенчмарки его почти не затрагивают.
- Машины за relay логично подключать как воркеры Harbor через `ssh`-среду или как отдельные job-раннеры. Какой вариант лучше, предстоит проверить.

### Gaps
- Полный список датасетов Harbor Hub не прочитан: `harbor datasets list` не запускался, Hub отдаёт данные через JS.
- Поддержка resume прерванного job в документации Harbor не найдена; страницы про resume в индексе нет.
- Поведение `--allow-agent-host` на локальном `docker` не проверено.
- Работа Harbor с GPU в Docker на WSL2 не проверена.

## Какой должна быть схема испытаний для стохастического агента

### Takeaway
Единица анализа — задача, а не запуск. Сравнения только парные: одна и та же задача, один дайджест среды, одинаковая конфигурация ресурсов. Интервалы — кластерный или стратифицированный бутстрап по задачам. Остановка — последовательный тест по образцу Fishtest или anytime-valid CI, а не подглядывание за p-value. Эффект изменений харнесса примерно в 5 раз меньше эффекта модели, поэтому на 30 задачах детектируются только крупные улучшения.

### Cited Findings
- **Одиночные прогоны без интервалов — норма индустрии и проблема.** Многие бенчмарки сообщают один запуск без CI — [ICC](https://arxiv.org/abs/2512.06710), [выдача](https://arxiv.org/pdf/2512.06710). LHTB 2.0 — один trial на задачу — [README](https://github.com/zli12321/LHTB).
- **Сколько повторов.** ICC сходится к 8–16 trial на структурированных задачах и требует от 32 на сложном рассуждении; ICC для GAIA 0.304–0.774 — [ICC](https://arxiv.org/abs/2512.06710). Harbor-Index использовал 3 — [arXiv](https://arxiv.org/html/2609.04298v1). MCPMark — k=4 — [README](https://github.com/eval-sys/mcpmark). BC-Bench — 5 проходов и 10,000 бутстрап-ресэмплов — [arXiv](https://arxiv.org/pdf/2608.20851). MLflow советует минимум 3, для публикаций 5 — [протокол](https://mlflow.org/articles/benchmarking-ai-agent-performance/).
- **Уровень ресэмплинга.** Повторы одной задачи не независимы, CI надо строить на уровне задачи — [EpiBench](https://arxiv.org/pdf/2606.13602), [AI for Games](https://arxiv.org/pdf/2609.16679). METR бутстрапит по семействам задач, задачам и запускам, но признаёт, что это переоценивает дисперсию уровня запуска — [TH1.1](https://metr.org/blog/2026-01-29-time-horizon-1-1/).
- **Парность.** Парное сравнение на одних задачах изолирует изменение лучше непарных агрегатов — [AI for Games](https://arxiv.org/pdf/2609.16679). Парный бутстрап — [AgentHPOBench](https://arxiv.org/pdf/2607.29626). SkillsBench определяет `skill_lift` как разницу парных условий — [arXiv](https://arxiv.org/html/2602.12670v1). Правило MLflow «перекрытие CI означает отсутствие различий» консервативно и для парных данных неверно; следует смотреть CI разности — вывод поисковой сводки к [протоколу](https://mlflow.org/articles/benchmarking-ai-agent-performance/).
- **Агрегация.** Среднее чувствительно к выбросам, медиана шумна; рекомендованы стратифицированный бутстрап, IQM, профили производительности, probability of improvement; библиотека `rliable` — [Agarwal et al.](https://arxiv.org/pdf/2108.13264), [Google Research](https://research.google/blog/rliable-towards-reliable-evaluation-reporting-in-reinforcement-learning/?m=1).
- **Последовательные тесты.** Fishtest использует GSPRT и пентаномиальную модель (пары партий), экономящую ресурсы по сравнению с триномиальной — [Fishtest Mathematics](https://official-stockfish.github.io/docs/fishtest-wiki/Fishtest-Mathematics.html). Модель предполагает одинаковые вероятности между воркерами — [FAQ](https://github.com/official-stockfish/fishtest/wiki/Fishtest-faq). Anytime-valid подход: e-значения, confidence sequences, неравенство Вилля, Type-I под опциональной остановкой — [CWI](https://www.cwi.nl/en/research/machine-learning/various-projects-on-e-values-always-valid-confidence-sequences-and-safe-testing/); промышленное применение AsympCS — [WWW 2023, обзор](https://liner.com/review/anytimevalid-confidence-sequences-in-enterprise-ab-testing-platform).
- **Шум инфраструктуры.** 6 п.п. между конфигурациями на TB 2.0 (p<0.01), между 1x и 3x в пределах шума (p=0.40), ошибки инфраструктуры 5.8%→2.1%; ресурсы — переменная эксперимента, прогоны разносить по времени — [Anthropic](https://www.anthropic.com/engineering/infrastructure-noise), [research](https://www.anthropic.com/research/infrastructure-noise).
- **Эффект модели и харнесса.** Сложность бенчмарка ICC=0.75; размах эффекта модели 0.451, харнесса 0.087, разница в 5.2 раза — [Harbor-Index](https://arxiv.org/html/2609.04298v1). Для сильнейших моделей харнесс изменил цену больше, чем результат — [Scale](https://labs.scale.com/blog/swe-bench-pro-v2). Но WildClawBench показывает до 18 п.п. на слабых/средних моделях — [выдача](https://arxiv.org/html/2605.10912v1).
- **Калибровка оракулом.** Oracle решает 642/642, `nop` — ни одной — [SWE Pro](https://github.com/scaleapi/SWE-bench_Pro-os/blob/main/v2/README.md); oracle 5 раз — [TB](https://github.com/harbor-framework/terminal-bench). Около трети самых трудных кандидатов оказались сломанными, а не трудными — [Harbor-Index](https://arxiv.org/html/2609.04298v1).
- **Целостность.** SWE Pro: офлайн-фаза, повторная оценка на чистом образе, большинство из трёх при расхождении — [arXiv](https://arxiv.org/html/2610.24206v1). Opus 4.6 догадался о бенчмарке и расшифровал ключ ответов BrowseComp — [Anthropic](https://www.anthropic.com/engineering/eval-awareness-browsecomp). Намеренное отделение судьи от размышлений агента — [BU Bench](https://github.com/browser-use/benchmark).
- **Надёжность как отдельная ось.** 12 метрик: согласованность, предсказуемость, устойчивость, безопасность; рост точности не даёт автоматического роста надёжности — [arXiv](https://arxiv.org/html/2602.16666), [дашборд](https://hal.cs.princeton.edu/reliability). HAL сравнивает с учётом цены — [HAL](https://hal.cs.princeton.edu/).
- **Таксономия сбоев.** Инфраструктурный слой: `agent_exhausted_budget`, `agent_refused_task`, `sandbox_failed`, `model_provider_failed`, `harbor_failed`, `verifier_failed`, с полями reward valid/retry/grade — [Harbor](https://github.com/harbor-framework/harbor/blob/main/docs-mintlify/jobs/error-classification.mdx). Поведенческий слой: AgentRx, 9 категорий и критический шаг сбоя — [Microsoft](https://www.microsoft.com/en-us/research/blog/systematic-debugging-for-ai-agents-introducing-the-agentrx-framework/?lang=fr-ca); TRACE, ~10,000 траекторий — [ICML 2026](https://icml.cc/virtual/2026/77858); 41 режим отказа с указанием стороны, модель или харнесс — [arXiv](https://alphaxiv.org/abs/2607.28802).
- **Трассировка.** ATIF с метриками и подагентами — [Harbor](https://docs.harborframework.com/agents/atif); `harbor analyze` с рубрикой — [код](https://github.com/harbor-framework/harbor/tree/main/src/harbor/analyze).

### Inferences
- **Протокол A/B (рекомендация).**
  1. Заморозить ключ эксперимента: версия бенчмарка, дайджест образов, коммит агента, дайджест весов и квантование, параметры сервера вывода, профиль ресурсов, версии судьи и симулятора.
  2. A/A-прогон на старте: одинаковая конфигурация дважды даёт реальный шумовой пол арены.
  3. Sanity: `oracle` и `nop` перед каждым новым набором.
  4. Запускать пары A и B на одной задаче с одним сидом и случайным порядком, чтобы дрейф нагрузки распределился поровну.
  5. Метрика на задачу: среднее по k повторов, предпочтительно плотная награда, а не бинарная.
  6. Вывод: CI разности через бутстрап по задачам с сохранением страт, дополнительно по доменам.
  7. Исключить `reward_valid=false`, отдельно гейтить долю таких trial, не считать их нулём молча.
- **Оценка мощности.** Для бинарного исхода и парных задач с долей расхождений d нужно приблизительно n ≈ 7.84·(d−Δ²)/Δ² задач при двустороннем α=0.05 и мощности 0.8 (нормальное приближение, мой расчёт):

| Δ | d=0.1 | d=0.2 |
|---|---|---|
| 20 п.п. | 12 | 31 |
| 10 п.п. | 71 | 149 |
| 5 п.п. | 306 | 619 |

  На 30 задачах при d=0.2 минимально детектируемый эффект около 20 п.п., на 100 задачах около 12 п.п. Значит smoke-набор ловит поломки, но не отличает улучшения на 3–5 п.п. Такие изменения надо мерить по токенам, шагам и плотной награде, где дисперсия обычно меньше.
- **Ранняя остановка.** Перенести схему Fishtest: для пары исход {−1, 0, +1} или непрерывная разница, GSPRT с границами вида [0, Δ₁] либо anytime-valid CI. Предпосылка i.i.d. нарушена разнородностью задач, поэтому порядок задач перемешивать в пределах страт. Правило остановки: принять, отклонить или исчерпать лимит бюджета с вердиктом «не решено».
- **Гейт регрессий.** Это тест не превосходства, а non-inferiority: нижняя граница CI разности ≥ −m. Первичная метрика одна, остальные с Холмом или BH.
- **Метрики цены.** Для локальных моделей: входные, выходные и кэшированные токены, GPU-секунды, энергия, wall-clock, TTFT, доля времени в инструментах и в выводе, шаги, compaction-события, стоимость на успех, отдельно цена судьи. Эквивалентную цену по прейскуранту считать, чтобы сравнивать с внешними таблицами.
- **Таксономия арены.** Три слоя: инфраструктура (из Harbor), поведение (первая критическая ошибка), целостность (чтение оценщика, подделка зависимостей, сеть). Третий слой всегда делает результат недействительным.
- **Лидерборд.** Строка — полный ключ эксперимента, сравнения только с одинаковым ключом бенчмарка и ресурсов, показывать CI и k, Парето точность-цена, а не единый рейтинг.
- **Особый риск zero-agent.** Постоянная память агента между запусками — канал утечки ответов. Каждый trial должен стартовать с чистого или замороженного снимка памяти, а не с общего хранилища.

### Gaps
- Численных данных о дисперсии между trial на TB или WildClawBench в прочитанных материалах нет, поэтому d в таблице — допущение, его надо заменить измеренным.
- Прямых публикаций о применении SPRT к агентным бенчмаркам не найдено; перенос — моя гипотеза.
- Способность локального сервера вывода давать детерминизм при батчинге не проверялась.

## Как ускорить цикл: подмножества, параллелизм, кэш, реплей

### Takeaway
Самый дешёвый выигрыш даёт отбор дискриминативных задач: три репрезентативные задачи на бенчмарк уже восстанавливают ранжирование систем с ρ≈0.923. Вторая ступень — оценка без агента: реплей патчей, HAR и траекторий. Третья — кэш LLM на уровне запроса с промахом как сигналом. На одной GPU узкое место — вывод, поэтому параллелизм ограничен слотами модели, а не контейнерами.

### Cited Findings
- **Избыточность задач.** На 52 бенчмарках 3 задачи дают ρ≈0.923 к полному ранжированию, для WideSearch 0.99, для CyberGym 0.75; основная вариация объясняется одним общим фактором плюс одно направление скаффолда; после 12 бенчмарков все остальные коррелируют с выбранными ρ≥0.7 — [Harbor-Index](https://arxiv.org/html/2609.04298v1).
- **Harbor-Index 1.0.** 82 задачи из 29 бенчмарков; конвейер 6,627 → 1,311 → 307 → 110+ → 100 → 82; лучший результат 28.0% (GPT-5.5 + Codex); $4–293 за прогон — [arXiv](https://arxiv.org/html/2609.04298v1). Корреляции самого индекса с полным набором не опубликованы — там же.
- **Готовые подмножества.** WebArena-Verified Hard (258), HARD-51, MCPMark `easy`, TB CPU (63), `terminal-bench-sample@2.0`, `openthoughts-tblite@2.0`, FeatureBench-lite, OSWorld offline 82 — [WAV](https://github.com/ServiceNow/webarena-verified), [SWE Pro](https://github.com/scaleapi/SWE-bench_Pro-os/blob/main/v2/README.md), [MCPMark](https://github.com/eval-sys/mcpmark), [TB](https://github.com/harbor-framework/terminal-bench/releases/tag/v4.1.0), [registry](https://github.com/harbor-framework/harbor/blob/main/registry.json), [BenchLM](https://benchlm.ai/benchmarks/osworld-2-1).
- **Насыщенные задачи выбрасывают.** TB удаляет задачи, решаемые 5/5 всеми классами моделей — [TB 4.0](https://www.tbench.ai/news/terminal-bench-4-0).
- **Ранний сигнал.** ~2/3 награды LHTB за 30 минут, но многие модели растут и в третьем часу — [README](https://github.com/zli12321/LHTB).
- **Параметры Harbor.** `-n` конкурентность, `-k` попытки, `-r` ретраи, `-l` лимит задач, `-i`/`-x` фильтры, лимиты конкурентности на провайдера — [run-a-job](https://github.com/harbor-framework/harbor/blob/main/docs-mintlify/jobs/run-a-job.mdx). Ретраи по типу ошибки уже классифицированы — [error classification](https://github.com/harbor-framework/harbor/blob/main/docs-mintlify/jobs/error-classification.mdx).
- **Реплей оценки.** `patch_replay:PatchReplayAgent` накладывает захваченный diff на чистый образ и прогоняет верификатор без модели — [SWE Pro](https://github.com/scaleapi/SWE-bench_Pro-os/blob/main/v2/README.md). WebArena-Verified оценивает офлайн по HAR — [README](https://github.com/ServiceNow/webarena-verified).
- **Кэш LLM.** Ключ Inspect: модель и base URL, история сообщений, эпоха, конфиг генерации, инструменты; `per_epoch` разводит повторы; задавать версионированные имена моделей — [Inspect](https://inspect.aisi.org.uk/caching.html). Страницы про кэш в индексе документации Harbor нет — [llms.txt](https://docs.harborframework.com/llms.txt).
- **Запись и воспроизведение.** Общий паттерн: один живой прогон, затем offline; канонический отпечаток запроса — [llm-run-recorder](https://pypi.org/project/llm-run-recorder/0.2.1/); строгий реплей, промах кэша прерывает запуск, bash-вызовы не воспроизводятся из-за побочных эффектов — [Bernstein](https://bernstein.readthedocs.io/en/latest/operations/deterministic-replay/); офлайн-проверки в CI отдельно от недетерминированного LLM-судьи — [evalcraft](https://pypi.org/project/evalcraft/0.5.0/); инструмент для записи LLM и HTTP — [agrepl](https://github.com/taiwrash/agrepl). Проекты небольшие, зрелость не подтверждена.
- **Трассы как датасет.** Публичные траектории есть у WildClawBench, LHTB, OSWorld, FrontierCS 2 — [WildClaw](https://github.com/InternLM/WildClawBench), [LHTB](https://github.com/zli12321/LHTB), [OSWorld](https://github.com/xlang-ai/OSWorld-V2), [FrontierCS](https://github.com/FrontierCS/FrontierCS-2).
- **Подготовка среды.** `configure()` работает в каждом trial, а `install()` пропускается через `--ak skip_install=true` — [Harbor](https://docs.harborframework.com/agents/custom-agents).
- **WSL.** `memory` по умолчанию 50% RAM Windows, `nestedVirtualization` true на Windows 11, `sparseVhd` экспериментальный, `gpuSupport` true — [Microsoft](https://learn.microsoft.com/en-us/windows/wsl/wsl-config).

### Inferences
- **Четыре уровня реплея.**
  1. *Score replay*: новый судья или верификатор на старых артефактах. Цена — только контейнер верификатора.
  2. *Response replay*: тот же запрос даёт записанный ответ, инструменты выполняются заново в чистой песочнице. Для изменений, которые не трогают запросы к модели: исполнитель инструментов, песочница, логи. Промах кэша показывает, что изменение всё-таки влияет на вход модели.
  3. *Prefix-fork*: реплей до шага N, затем живая модель. Подходит для экспериментов с компакцией и памятью: они вступают в силу поздно, и общий префикс не тратит GPU.
  4. *Trace eval*: статические проверки по ATIF без выполнения: циклы, повторные вызовы, нарушения политики, доля контекста в инструментальном выводе.
- Кэш только с полным ключом (дайджест весов, квантование, seed, параметры сэмплинга), иначе он тихо смешивает условия. При `k>1` повторы нужно разводить по seed, иначе кэш схлопнет стохастичность, а CI окажутся ложно узкими.
- Отбор собственного smoke-набора: прогнать базовую связку k=5 на широком наборе, оставить задачи с долей успеха примерно 20–80%, стратифицировать по домену и типу инструмента, отбросить flaky у oracle, пересматривать при смене базовой модели. Высокий ρ из Harbor-Index получен на разбросе фронтирных систем и не гарантирует чувствительности к малым изменениям харнесса одной системы, поэтому подмножество надо валидировать на реальных парах изменений.
- На одной GPU параллельные trial конкурируют за слоты вывода, и нагрузка становится мешающим фактором: под нагрузкой таймауты срабатывают раньше. `n-concurrent` надо привязать к числу слотов сервера и фиксировать в ключе эксперимента. Удалённые машины дают параллелизм только для песочниц, если вывод остаётся один, и полноценный параллелизм, если у них есть своя GPU.
- Быстрый контроль длинных задач: урезать бюджет до 30 минут, оценивать по промежуточной награде. Это сигнал «лучше/хуже», а не оценка, сравнимая с лидербордом.
- Снимки окружения: предсобранные образы и заранее подтянутые слои дают основную экономию. CRIU или снимки QEMU оправданы только для тяжёлых VM вроде OSWorld.
- Отдельно хранить снимки памяти агента как артефакты эксперимента: это делает память воспроизводимой переменной.

### Gaps
- Нет публикаций, измеряющих экономию prefix-fork реплея в агентных бенчмарках.
- Не проверено, можно ли подключить кэш-прокси для вывода к Harbor без потери поля usage.
- Поддержка снимков окружения в Harbor не подтверждена.

## Что запускать в CI, ночью и по запросу

### Takeaway
CI проверяет, что ничего не сломано: детерминированные тесты, реплей и маленький живой smoke-набор без внешних зависимостей. Ночь отвечает, стало ли лучше: парные прогоны на стратифицированном наборе. On-demand — только для решений уровня релиза и исследовательских гипотез. Живые веб-бенчмарки и задачи с LLM-судьёй в обязательный контур не включать.

### Cited Findings
- Паттерн разделения: быстрые офлайн-проверки в CI, недетерминированные live-оценки по расписанию — [evalcraft](https://pypi.org/project/evalcraft/0.5.0/).
- Общие прогоны надо разносить по разному времени и дням, чтобы усреднить шум — [Anthropic](https://www.anthropic.com/engineering/infrastructure-noise).
- MCPMark `easy` позиционирован как набор для smoke и CI — [README](https://github.com/eval-sys/mcpmark). Дымовой режим τ — [README](https://github.com/sierra-research/tau2-bench).
- `--check` в BU Bench — preflight без выполнения задач — [README](https://github.com/browser-use/benchmark). `--dry-run` в τ-bench — [README](https://github.com/sierra-research/tau2-bench).
- Harbor-Index показывает, что 82 тщательно отобранные задачи уже дают сложную панель — [arXiv](https://arxiv.org/html/2609.04298v1).

### Inferences
Бюджеты — цели проектирования, а не измерения.

| Контур | Триггер | Бюджет | Состав | Вердикт |
|---|---|---|---|---|
| **pre-commit** | локально | <1 мин | Юнит-тесты, валидация ATIF, trace-проверки | блок |
| **CI-1** | каждый PR | 5–10 мин | Response replay золотых трасс, oracle/nop на hello-world, 5–10 Windows-задач с заглушкой модели | блок |
| **CI-2** | merge, GPU свободна | 20–40 мин | Живой smoke на 10–20 задач: MCPMark easy Filesystem/Postgres, τ retail/airline EN, AgentDojo-подмножество, WildClaw safety | предупреждение, блок при падении >20 п.п. или инфраструктурных ошибках |
| **Nightly** | ночь | 6–8 ч | Парный прогон 50–100 задач, k=3: WildClawBench, TB CPU-подмножество, HARD-51 частично, τ EN, WebArena-Verified Hard без тяжёлых сайтов, память | гейт non-inferiority, CI |
| **Weekly** | выходные | 24–48 ч | Широкая панель k=5, A/A-контроль, Harbor-Index или его доля, UniClawBench EN, SWE-EVO | обновление лидерборда |
| **On-demand** | гипотеза, релиз | дни | LHTB 2.0 с 30-минутной или полной отсечкой, OSWorld 2.1, BU Bench, MCP-Universe, полный SWE-Bench Pro V2 | решение вручную |
| **Cloud burst** | редкие калибровки | по бюджету | Полные TB 4.1, FrontierCS 2 на Modal/EKS | внешняя сверка |

- Быстрый путь A/B: `arena try <branch>` берёт nightly-набор, запускает по SPRT с лимитом бюджета, отчёт — вердикт, CI разности, дельта токенов и времени, новые типы сбоев, ссылки на расходящиеся траектории.
- Судья и симулятор на внешних API не должны быть условием CI: сетевая ошибка не должна блокировать PR.
- Смена базовой модели или сервера вывода автоматически инициирует новый baseline и пересчёт smoke-набора.

### Gaps
- Реальные длительности ярусов неизвестны до замеров на рабочей станции.
- Цена судьи в локальном контуре не оценена.

## Чего не хватает в списке и как мерить вклад памяти

### Takeaway
В списке нет памяти, навыков, исследовательской работы, экономически значимых задач. Минимальные добавления: LoCoMo и LongMemEval как быстрая регрессия, MemoryArena и MERIT как поведенческая проверка, VibeMemBench/MemDev-Bench/SWE-ContextBench для кодинга, SkillsBench для навыков, BrowseComp или свежие аналоги для поиска, GDPval. Вклад памяти надо мерить как парную разницу с контролем без памяти и с оракулом сверху, а не как recall: все сильные работы 2026 года показывают разрыв между точностью извлечения и поведением.

### Cited Findings

#### Память
- **LoCoMo.** 10 многосессионных диалогов по 100–260 вопросов; адаптер Harbor есть, одна задача на разговор — [адаптер](https://github.com/harbor-framework/harbor/tree/main/adapters/locomo). Сообщается об аудите, показавшем снисходительность LLM-судьи и ~6.4% ошибок в эталоне; информация вторичная — [arXiv 2606.22030](https://arxiv.org/pdf/2606.22030).
- **LongMemEval.** 500 вопросов, 5 способностей, ~115 тыс. токенов и вариант на 1.5 млн токенов; ICLR 2025 — [arXiv](https://arxiv.org/html/2410.10813v2).
- **MemoryArena.** Связанные сессии, 57 шагов в среднем; агенты, почти насыщающие LoCoMo, плохо работают здесь; ICML 2026 — [arXiv](https://arxiv.org/html/2602.16313), [PMLR](https://proceedings.mlr.press/v306/he26am.html).
- **MERIT.** Проверяемая автоматически зависимость от прошлых эпизодов, нижняя граница 0.00 → 0.55–1.00; извлечение по эмбеддингам нестабильно на обновлённых фактах, агент действует по верно извлечённому значению лишь в 55% случаев; смена реализации памяти меняет успех до 60 п.п.; лучшая конфигурация даёт в 2.7–3.9 раза большую предельную пользу на доллар, чем полный реплей; 23,440 эпизодов за $42.57 — [arXiv](https://arxiv.org/abs/2609.05441).
- **Ground Truth First.** Контроль без памяти, 3 реплики, 2 горизонта; ранги архитектур меняются с длиной истории: одна падает с 96% до 72% на девятой неделе, другая растёт до 90% — [arXiv](https://arxiv.org/abs/2607.21962).
- **VibeMemBench.** 111 задач из 90 репозиториев, 3,634 траектории; прямая инъекция проверенного опыта +1.1–4.5 п.п. на 4 из 5 решателей и снижение шагов на всех пяти; 11 из 12 пар решатель-система не превосходят контроль без памяти — [arXiv](https://arxiv.org/abs/2609.23570).
- **MemDev-Bench.** 4,869 исторических задач, 555 базовых, контролируемая релевантность — [arXiv](https://arxiv.org/abs/2610.27524v1). Сырые траектории снижают шаги сильнее, чем системы памяти (по выдаче) — там же.
- **SWE-ContextBench.** Повторное использование опыта между задачами; сводка эффективнее полной траектории — [arXiv](https://arxiv.org/html/2602.08316v2).
- **Положительные результаты.** Проактивная память: +8.3 п.п. на TB 2.0 и +6.8 на τ²-Bench — [arXiv](https://arxiv.org/abs/2607.08716). Препрегистрированный эксперимент на SWE-bench Verified: +10.2 п.п. на 49 парных задачах, не рецензирован — [репо](https://github.com/SaravananJaichandar/coding-agent-memory-benchmark).
- **BEAM** масштабирует диалоговую проверку до 10 млн токенов — [упоминание](https://arxiv.org/pdf/2609.05441). Обзоры Mem0 вендорские — [Mem0](https://mem0.ai/library/agent-memory/ai-memory-benchmarks-in-2026).
- **WildClawBench** позволяет прогнать бенчмарк с пользовательским workspace OpenClaw, включая `MEMORY.md` и навыки, через `--lobster-workspace` — [README](https://github.com/InternLM/WildClawBench).

#### Навыки
- **SkillsBench.** 86 задач, 11 доменов, детерминированные верификаторы; условия: без навыков, курируемые, самогенерируемые; +16.2 п.п. для курируемых, самогенерируемые почти ничего не дают — [arXiv](https://arxiv.org/html/2602.12670v1). У [vals.ai](https://vals.ai/benchmarks/skillsbench) +14.88; числа несравнимы по версии.

#### Исследование и поиск
- **BrowseComp.** 1,266 задач; ответы утекли в открытый веб, в девяти случаях модель нашла их, в двух расшифровала ключ — [Anthropic](https://www.anthropic.com/engineering/eval-awareness-browsecomp). Есть в Inspect — [каталог](https://github.com/UKGovernmentBEIS/inspect_evals/tree/main/src/inspect_evals). Альтернативы со свежими фактами: [335 вопросов, факты за 90 дней](https://arxiv.org/html/2605.28721), [EvoBrowseComp](https://bittide.aicompass.dev/article/e7dd9c20-83d4-4bbd-b4cd-75542ec77e06). В Harbor есть `widesearch`, `deepsynth`, `simpleqa` — [adapters](https://github.com/harbor-framework/harbor/tree/main/adapters).

#### Экономически значимая работа
- **GDPval.** 1,320 задач, 44 профессии, золотое подмножество 220 — [выдача](https://capitalandcompute.net/ai-benchmarks/gdpval/). В Inspect есть `gdpval` — [каталог](https://github.com/UKGovernmentBEIS/inspect_evals/tree/main/src/inspect_evals). GDPval-AA v2.1: лидер Claude Fable 5.1, Elo 1735; агрегаторы расходятся — [Artificial Analysis](https://artificialanalysis.ai/evaluations/gdpval-aa).
- **APEX-Agents.** Банкинг, консалтинг, право, Pass@1, HF CC-BY, инфраструктура Archipelago — [arXiv](https://arxiv.org/pdf/2601.14242).
- **Vending-Bench 2.** Долгая автономная работа, метрика — итоговый баланс — [alphaXiv](https://alphaxiv.org/benchmarks/andon-labs/vending-bench-2).
- **TheAgentCompany.** Есть и в Harbor, и в Inspect — [adapters](https://github.com/harbor-framework/harbor/tree/main/adapters).

#### Длительность автономии
- **METR Time Horizons.** TH1.1: 228 задач, удвоение ~131 день, 95% ~107–161 — [METR](https://metr.org/blog/2026-01-29-time-horizon-1-1/). Страница больше активно не обновляется, последний добавленный — Claude Mythos Preview (май 2026); часть задач приватна — [time-horizons](https://metr.org/time-horizons/). Критика: данные не различают траектории, 80%-горизонты на порядок короче — [GreaterWrong](https://www.greaterwrong.com/posts/sBEzomgnYJmYHki9T/updated-metr-s-data-can-t-distinguish-between-trajectories). Метод: логистика успеха от логарифма человеческого времени — [Epoch](https://epoch.ai/data/metr-time-horizons).

#### Другое
- Harbor-Index — лучшая готовая панель — [arXiv](https://arxiv.org/html/2609.04298v1). LiveClawBench — [arXiv](https://arxiv.org/html/2604.13072v1). ClawBench от TIGER-AI-Lab — [репо](https://github.com/TIGER-AI-Lab/ClawBench). OmniaBench — [arXiv](https://arxiv.org/pdf/2607.14989) (видна в выдаче, не изучена). CooperBench для многоагентности — [adapters](https://github.com/harbor-framework/harbor/tree/main/adapters). Epoch ведёт независимые ревью бенчмарков — [пример](https://epoch.ai/benchmarks/osworld-2-1/review).

### Inferences
- **Схема оценки вклада памяти.** Факторный эксперимент на одном наборе задач:

| Условие | Что даёт |
|---|---|
| `M0` память выключена | Контроль |
| `M1` пустая, агент учится внутри задачи | Вклад внутри эпизода |
| `M2` накопленный снимок на истории | Фактический продукт |
| `M3` оракульная инъекция проверенного опыта | Верхняя граница |
| `M4` полная история в контексте | Базовая альтернатива |
| `M5` снимок с внедрёнными искажениями | Устойчивость |

- **Показатели.**
  - Парный лифт `M2−M0`, «захват» `(M2−M0)/(M3−M0)`, потери системы `M3−M2`.
  - Шаги, токены, повторное исследование как более чувствительные метрики: у VibeMemBench эффект на решение малый, а шаги снижаются всюду.
  - Предельная польза на единицу токенов, накладные расходы памяти отдельно.
  - Разделение цепочки: сохранено → извлечено → применено → помогло. Числа MERIT показывают, что потери на этапе «применено» велики.
  - Обновление фактов и противоречия, как проверка свежести.
  - Кривая по горизонту минимум на двух длинах истории, ради эффекта tenure crossover.
- **Проверка зависимости.** Задача считается memory-задачей, только если `M0` стабильно проваливается, а `M3` стабильно проходит, как в leak check у MERIT и отборе у VibeMemBench.
- **Изоляция.** Снимок памяти версионируется и замораживается на прогон, запись не переносится между trial, иначе теряется независимость и возникает утечка эталона.
- **Мощность.** Если реальный эффект 1–5 п.п., на типичной ночной выборке он не виден по доле решений. Основной гейт памяти — стоимость и шаги, с ограничением non-inferiority по успеху.
- **Для zero-agent** сначала нужен собственный набор «проектная память»: серии связанных задач в одном репозитории и среде Windows, поскольку MemDev-Bench и VibeMemBench проверяют именно этот сценарий, но без привязки к нашему окружению.
- METR-подход годится как методология (логистика по длине задачи), а не как запускаемый бенчмарк. HCAST отдельным публичным набором не найден.

### Gaps
- Статус открытых данных MemoryArena, MERIT и MemDev-Bench не проверен.
- Аудит LoCoMo читался только во вторичном пересказе.
- BEAM и OmniaBench не изучены первично. О HCAST как об отдельном запускаемом наборе ничего не найдено; по памяти это часть задач METR.
- Сравнения качества GDPval на локальных моделях нет.

## Приоритет: где больше всего сигнала на час для локального универсального агента

### Takeaway
Первыми запускать WildClawBench через Harbor, TB 4.1 CPU-подмножество, τ-bench EN, MCPMark easy и AgentDojo, затем HARD-51 и WebArena-Verified Hard. LHTB, OSWorld 2.1, BU Bench и MCP-Universe оставить для редких прогонов. FrontierCS 2, General AgentBench как запускаемый набор, SWE-bench Verified и AgentBench FC в регулярный контур не включать.

### Cited Findings
- WildClawBench: минуты на задачу, есть сравнение с локальными весами, родной формат Harbor, отчётность по харнессам — [README](https://github.com/InternLM/WildClawBench).
- Правки задач и инфраструктура могут давать больше, чем улучшения агента: +12.1 п.п. от исправления TB — [TB 2.1](https://www.tbench.ai/news/terminal-bench-2-1).
- Общий фактор способности доминирует, скаффолд — вторая ось — [Harbor-Index](https://arxiv.org/html/2609.04298v1). Значит, панель из немногих разнородных наборов почти так же информативна, как полный список, в смысле ранжирования.
- Универсальность стоит потерь: общий режим снижает скоры у многих моделей — [General AgentBench](https://arxiv.org/html/2602.18998v1).
- SWE-Bench Pro V2 проектировался как офлайн-протокол, что совпадает с локальной моделью — [v2/README](https://github.com/scaleapi/SWE-bench_Pro-os/blob/main/v2/README.md).

### Inferences
Это экспертная оценка: время — расчётное, сигнал — суждение.

| Ранг | Бенчмарк и режим | Покрывает | Почему | Адаптер | Контур |
|---|---|---|---|---|---|
| 0 | Собственный Windows-набор, oracle/nop | Windows, PowerShell, FS, долгие сессии | Публичные наборы этого не проверяют | свой | CI |
| 1 | **WildClawBench** через Harbor | Широкий срез, безопасность, мультимодальность | Быстро, разнородно, есть локальные референсы | Harbor | nightly |
| 2 | **TB 4.1 CPU**, стратифицированное подмножество | Терминал, инженерия | Индустриальная шкала, верифицируемость | Harbor | nightly |
| 3 | **τ-bench 2.0 EN** | Диалог, политика, инструменты | Без Docker, быстро, ловит регрессии контекста | Python-прокладка | CI-2 |
| 4 | **MCPMark easy → standard** | MCP-клиент | Детерминированные верификаторы, pass^k | адаптер | CI-2/nightly |
| 5 | **AgentDojo** | Injection | Единственная проверка безопасности в чистом виде | Inspect | CI-2 |
| 6 | **Память: LoCoMo → MERIT/MemoryArena → проектный набор** | Память | Ключевая гипотеза продукта | Harbor + свой | nightly |
| 7 | **HARD-51 + SWE-Bench Pro V2 выборка** | Кодинг на 3 языках | Запрет сети, повторная оценка на чистом образе | `locked_*` | nightly |
| 8 | **WebArena-Verified Hard** без `wikipedia`/`map` | Браузер | Детерминированно, офлайн-оценка | адаптер | nightly |
| 9 | **SkillsBench** | Навыки | Парный дизайн уже встроен | Harbor или свой | weekly |
| 10 | **UniClawBench EN** | Проактивность | Широта, но нужен внешний супервизор | бэкенд | weekly |
| 11 | **SWE-EVO** | Многошаговая эволюция | 48 длинных задач, мало дублирования с другими | экспорт | weekly |
| 12 | **LHTB 2.0**, 30-минутная отсечка | Длинный горизонт | Дорого, форк Harbor; ключевой для автономии | Harbor-патч | on-demand |
| 13 | **OSWorld 2.1**, offline-подмножество | Computer use | Тяжело, вердикт Flawed | `mm_agents` | on-demand |
| 14 | **BU Bench `local_headless`** | Реальный веб | Дрейф, судья, ограничения лицензии | форк | on-demand |
| 15 | **MCP-Universe** | Живые MCP | Скор зависит от даты, нужны ключи | свой | on-demand |
| — | General AgentBench | Метод | Брать идею: один агент со всеми инструментами против доменного набора. Обязательный фактор арены, а не отдельный прогон | — | метод |
| — | FrontierCS 2 | Исследования | Только облако; для локального заменить FrontierCS 1.0 Algorithmic | — | cloud |
| — | SWE-bench Verified | Литература | Загрязнён; только для сверки | Harbor | разово |
| — | AgentBench FC | Старое | Не даёт уникального сигнала | — | исключить |

- **Обязательный эксперимент в первый месяц.** Для каждого бенчмарка оценить инфраструктурные ошибки, oracle, разброс A/A, время на задачу на нашей связке. Только после этого уточнить ранги, поскольку они построены на чужих фронтирных числах.
- **Первые вложения (порядок).** Harbor в WSL2 → класс `ZeroAgent` и ATIF → прокси вывода с записью → реестр экспериментов и бутстрап → smoke-набор → SPRT → адаптеры MCPMark и WebArena-Verified. Такой порядок даёт полезный цикл «попробовать, сравнить, оставить» ещё до адаптеров.

### Gaps
- Скорость и качество локальных моделей на TB, τ, MCPMark неизвестны.
- Нет данных о пересечении сигнала между WildClawBench и TB; корреляции Harbor-Index на уровне бенчмарков не опубликованы попарно.
- Уровень реальной потери при запуске SWE-Bench Pro V2 через Docker вместо Modal не проверен.
