# Skills, методологии и доменные репозитории для zero-agent

Дата сбора: 2026-11-06. Метод: `gh api` (метаданные, README, деревья файлов всех 26 репозиториев, плюс `anthropics/skills` и `agentskills/agentskills`), выборочное чтение SKILL.md как данных (около 12 файлов), WebSearch/WebFetch. Ни один скилл не выполнялся. Глубина неравномерная: большинство репозиториев изучены по README и дереву, а не по всему содержимому. Страницы WebFetch проходят через модель-суммаризатор, поэтому цифры из них стоит сверять с оригиналом, прежде чем цитировать в спецификации.

## Какие репозитории существуют и чего они стоят как источники

### Takeaway
Все 26 репозиториев существуют, ни один не архивирован и не пуст. Reddit-ссылка недоступна (fetch отказан), вместо неё использован репозиторий Coolify. Звёзды здесь не являются сигналом качества: у некоторых репозиториев они аномальны, а маркетинговые числа в README расходятся с описаниями.

### Cited Findings
Метаданные на 2026-11-06 (звёзды / лицензия / последний push), получены через `gh api repos/...`:

| Репозиторий | ★ | Лицензия | Push | Что это по факту |
|---|---|---|---|---|
| [OthmanAdi/planning-with-files](https://github.com/OthmanAdi/planning-with-files) | 27.9k | MIT | 11-04 | Один скилл + хуки: план на диске, повторная инъекция, stop-gate |
| [affaan-m/ECC](https://github.com/affaan-m/ECC) | 298k | MIT | 11-06 | Харнесс-надстройка: 71 агент, 314 скиллов, хуки, instincts, AgentShield |
| [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills) | 113.6k | MIT | 11-01 | 25 скиллов полного SDLC с верификационными гейтами |
| [ruvnet/ruflo](https://github.com/ruvnet/ruflo) | 81k | MIT | 11-06 | Мета-харнесс: swarm, федерация, векторная память, MCP |
| [wshobson/agents](https://github.com/wshobson/agents) | 42.3k | MIT | 11-05 | 94 плагина, 202 агента, 185 скиллов; генераторы под 7 харнессов |
| [mattpocock/skills](https://github.com/mattpocock/skills) | 316k | MIT | 11-05 | Малые компонуемые инженерные скиллы |
| [alirezarezvani/claude-skills](https://github.com/alirezarezvani/claude-skills) | 29.4k | MIT | 11-05 | 411 скиллов, 807 stdlib-скриптов; публикует замеры срабатывания |
| [phuryn/pm-skills](https://github.com/phuryn/pm-skills) | 27.7k | MIT | 10-29 | 69 PM-скиллов, 42 цепочки, 9 плагинов |
| [luongnv89/claude-howto](https://github.com/luongnv89/claude-howto) | 46.6k | MIT | 11-03 | Учебник по Claude Code, не методология |
| [gamedev-skills/awesome-gamedev-agent-skills](https://github.com/gamedev-skills/awesome-gamedev-agent-skills) | 7.1k | Apache-2.0 | 10-28 | 77 скиллов + роутер, 12 движков |
| [AgriciDaniel/claude-seo](https://github.com/AgriciDaniel/claude-seo) | 27k | MIT | 11-01 | 26 подскиллов + 19 агентов SEO/GEO |
| [career-ops-hq/career-ops](https://github.com/career-ops-hq/career-ops) | 80.6k | MIT | 11-06 | Поиск работы: оценка вакансий, CV-PDF, трекер |
| [Panniantong/Agent-Reach](https://github.com/Panniantong/Agent-Reach) | 109.5k | MIT | 11-05 | CLI-доступ к соцсетям и логин-платформам |
| [Shubhamsaboo/awesome-llm-apps](https://github.com/Shubhamsaboo/awesome-llm-apps) | 144k | Apache-2.0 | 11-06 | Шаблоны приложений и скиллов |
| [punkpeye/awesome-mcp-servers](https://github.com/punkpeye/awesome-mcp-servers) | 97.4k | MIT | 11-02 | Каталог MCP (4632 записи по подсчёту `^- [`) |
| [mlabonne/llm-course](https://github.com/mlabonne/llm-course) | 84.9k | Apache-2.0 | **02-05** | Курс по LLM; наименее свежий в выборке |
| [microsoft/ai-agents-for-beginners](https://github.com/microsoft/ai-agents-for-beginners) | 79k | MIT | 11-02 | 18 уроков |
| [NirDiamant/RAG_Techniques](https://github.com/NirDiamant/RAG_Techniques) | 30.2k | NOASSERTION | 11-02 | Ноутбуки RAG-техник |
| [NirDiamant/agents-towards-production](https://github.com/NirDiamant/agents-towards-production) | 21.8k | NOASSERTION | 11-02 | Туториалы; много спонсорских |
| [chiphuyen/aie-book](https://github.com/chiphuyen/aie-book) | 18.2k | нет в API | 07-03 | Материалы к книге, 46 файлов, код почти отсутствует |
| [morluto/rea](https://github.com/morluto/rea) | 436k | MIT | 11-06 | Единый MCP для реверс-инжиниринга |
| [Donchitos/Claude-Code-Game-Studios](https://github.com/Donchitos/Claude-Code-Game-Studios) | 35.9k | MIT | 10-19 | Шаблон «студии»: 49 агентов, 74 скилла |
| [docusealco/docuseal](https://github.com/docusealco/docuseal) | 19k | AGPL-3.0 | 11-02 | Self-hosted e-signature + API + MCP |
| [ZSeven-W/openpencil](https://github.com/ZSeven-W/openpencil) | 7.1k | MIT | 11-05 | Векторный дизайн-инструмент на Rust + MCP + CLI |
| [NationalSecurityAgency/ghidra](https://github.com/NationalSecurityAgency/ghidra) | 98.3k | Apache-2.0 | 11-05 | SRE-фреймворк, последний релиз Ghidra_12.2 (2026-10-22) |
| [coollabsio/coolify](https://github.com/coollabsio/coolify) | 65.7k | Apache-2.0 | 11-06 | Self-hosted PaaS + REST + MCP |

- У `morluto/rea` 436 205 звёзд и 108 199 форков при создании 2026-04-14 и 1653 подписчиках — [gh api repos/morluto/rea](https://github.com/morluto/rea). README предупреждает, что похожее имя или скопированный README не доказывают официальность загрузки — [rea README](https://github.com/morluto/rea). Аналогичное предупреждение «Official sources only» есть у [ECC](https://github.com/affaan-m/ECC).
- Счётчики расходятся между описанием и README: Game Studios 72 vs 74 скилла ([описание/README](https://github.com/Donchitos/Claude-Code-Game-Studios)); pm-skills «100+» vs 69 скиллов + 42 workflow ([README](https://github.com/phuryn/pm-skills)).
- В README ruflo бейдж загрузок подписан как «Illustrative download counter» (иллюстрация от 98 000 000), рядом верифицированный датасет 97 022 120 событий — [ruflo README](https://github.com/ruvnet/ruflo).
- README Agent-Reach содержит спонсоров: прокси и anti-bot сервисы (Bright Data, RapidProxy, BrowserAct) — [Agent-Reach README](https://github.com/Panniantong/Agent-Reach).

### Inferences
- Рост rea (436k за 7 месяцев) не проверен; я не утверждаю накрутку, но и не считаю звёзды доказательством качества. Для zero-agent репутационные метрики нельзя включать в оценку доверия к скиллу: подделать их дешевле всего.
- Практическая ценность выше у источников, публикующих замеры и ограничения (alirezarezvani `VERIFIED.md`, Game Studios, planning-with-files), чем у самых звёздных.

### Gaps
- Происхождение звёзд не анализировалось (нужны данные по stargazers во времени).
- Reddit-пост про Coolify прочитать не удалось.

## Какое текущее состояние открытого стандарта Agent Skills (SKILL.md) на ноябрь 2026

### Takeaway
SKILL.md — де-факто стандарт, с 22 октября 2026 передан в AAIF (Linux Foundation), governance принят 4 ноября. Формат намеренно минимален: нет версий спецификации, зависимостей, подписей, определённого пути хранения и маркетплейса; всё это живёт в расширениях клиентов и открытых предложениях. Zero-agent должен реализовать ядро строго, расширения разбирать терпимо, а исполняемые расширения не доверять по умолчанию.

### Cited Findings
**Статус и управление**
- Anthropic объявила о передаче стандарта в AAIF 22 октября 2026: имя, логотип, GitHub-организация и домен agentskills.io; заявлено более 70 интегрировавших платформ, 1 млн опубликованных скиллов, почти 280 млн установок (данные вендора, не аудит) — [Anthropic](https://www.anthropic.com/news/donating-agent-skills-to-the-agentic-ai-foundation).
- `GOVERNANCE.md` принят 4 ноября 2026: проект LF Projects, TSC из Lead и Core Maintainers, членство индивидуальное, код и спецификация Apache-2.0, документация CC-BY-4.0, изменения спецификации через файл-предложение, спонсора, публичное обсуждение и решение Core Maintainers — [GOVERNANCE.md](https://github.com/agentskills/agentskills/blob/HEAD/GOVERNANCE.md).
- Lead Maintainers: Mahesh Murag, David Soria Parra; пять Core Maintainers — [MAINTAINERS.md](https://github.com/agentskills/agentskills/blob/HEAD/MAINTAINERS.md).
- Расхождение: статья [buildmvpfast](https://www.buildmvpfast.com/blog/agent-skills-linux-foundation-aaif-donation-2026) утверждает, что процесса предложений нет и последний коммит 9 августа; это устарело, коммиты #648/#649 от 2026-11-04 добавили governance — [commits](https://github.com/agentskills/agentskills/commits/main).
- В репозитории 0 тегов и 0 релизов; спецификация без номера версии — `gh api repos/agentskills/agentskills/tags`, [спецификация](https://agentskills.io/specification).
- В витрине клиентов 46 имён: Agentman, Amp, Autohand Code CLI, bub, ChatGPT & Codex, Claude, Claude Code, Command Code, Cursor, Databricks Genie Code, Deep Code, Emdash, Factory, fast-agent, Firebender, Gemini CLI, GitHub Copilot, Google AI Edge Gallery, Goose, Hermes Agent, Junie, Kiro, Laravel Boost, Letta, Mistral AI Vibe, Mux, nanobot, Ona, OpenClaw, OpenCode, OpenHands, pi, Piebald, Pulumi Neo, Qodo, Roo Code, Snowflake Cortex Code, Spring AI, Superconductor, Tabnine, TRAE, Vita, VS Code, VT Code, Workshop, ZeroClaw — [clients.jsx](https://github.com/agentskills/agentskills/blob/HEAD/docs/snippets/clients.jsx). Заявленные 70+ витрина не покрывает; README называет installer `npx skills` «70+ agents» — [addyosmani README](https://github.com/addyosmani/agent-skills).

**Формат**
- Скилл — каталог с `SKILL.md` (YAML frontmatter + Markdown); опционально `scripts/`, `references/`, `assets/` — [спецификация](https://agentskills.io/specification).
- Обязательны `name` (1–64, `a-z0-9-`, без `--`, без дефиса по краям, совпадает с именем каталога) и `description` (1–1024). Опциональны `license`, `compatibility` (до 500), `metadata` (string→string), `allowed-tools` (Experimental) — [спецификация](https://agentskills.io/specification).
- Progressive disclosure: метаданные ~100 токенов при старте; тело <5000 токенов и <500 строк при активации; ресурсы по требованию; ссылки на один уровень вглубь — [спецификация](https://agentskills.io/specification).
- `skills-ref validate` проверяет frontmatter, но библиотека «for demonstration purposes only» — [skills-ref](https://github.com/agentskills/agentskills/tree/HEAD/skills-ref).

**Руководство для клиентов (это прямой чек-лист для zero-agent)** — всё из [adding-skills-support](https://agentskills.io/client-implementation/adding-skills-support.md):
- Сканировать `<project>/.<client>/skills/`, `<project>/.agents/skills/`, `~/.<client>/skills/`, `~/.agents/skills/`; `.claude/skills/` — прагматичная совместимость. Спецификация места хранения не задаёт.
- Границы сканирования (глубина 4–6, до 2000 каталогов); пропуск `.git/`, `node_modules/`.
- Коллизии: проект перекрывает пользователя; предупреждение в лог.
- Проектные скиллы из недоверенного репозитория рекомендуется подключать только после пометки папки как доверенной.
- Лояльный разбор: несоответствие имени каталогу или длина → предупреждение и загрузка; пустой description или неразбираемый YAML → пропуск. Частый дефект — неэкранированное двоеточие в значении.
- Каталог в system prompt или в описании dedicated-инструмента; при dedicated-инструменте `name` — enum.
- Активация моделью (большинство реализаций без keyword-триггеров) и явная пользователем (`/skill-name`). Отфильтрованные скиллы скрывать из каталога.
- Обёртка `<skill_content name="...">`, перечисление ресурсов без их чтения, защита от компакции, дедупликация активаций, опциональный запуск в подагенте.

**Расширения и незакрытые вопросы**
- Открытые обсуждения: стандартная папка [#15](https://github.com/agentskills/agentskills/issues/15) (130 комментариев), версионирование/lock [#46](https://github.com/agentskills/agentskills/issues/46), зависимости [#100](https://github.com/agentskills/agentskills/issues/100) и [#110](https://github.com/agentskills/agentskills/issues/110), активация [#57](https://github.com/agentskills/agentskills/issues/57), удалённый импорт [#42](https://github.com/agentskills/agentskills/issues/42), `mcpServers` [#21](https://github.com/agentskills/agentskills/issues/21), секреты [#86](https://github.com/agentskills/agentskills/issues/86), runtime пишет в `metadata` вложенные структуры вопреки спецификации [#514](https://github.com/agentskills/agentskills/issues/514).
- Маркетплейс/дистрибуция: открытый PR [#254](https://github.com/agentskills/agentskills/issues/254) предлагает `/.well-known/agent-skills/index.json` с `digest: sha256:...` и типами `skill-md`/`archive`; не принят.
- Де-факто дистрибуция: `npx skills add` (vercel-labs/skills), `skills.sh`, плагин-маркетплейсы Claude Code/Codex/Copilot, Gemini `skills install` — [mattpocock README](https://github.com/mattpocock/skills).
- Де-факто поля вне спецификации: `disable-model-invocation` (упомянуто в руководстве клиентов; используется в [wayfinder](https://github.com/mattpocock/skills/blob/HEAD/skills/engineering/wayfinder/SKILL.md)), `user-invocable` и `hooks` ([planning-with-files SKILL.md](https://github.com/OthmanAdi/planning-with-files/blob/HEAD/skills/planning-with-files/SKILL.md)), `context: fork` и `argument-hint`; по сообщению вторичного источника, такие ключи дают ошибку при загрузке в claude.ai/Skills API — [buildmvpfast](https://www.buildmvpfast.com/blog/agent-skills-linux-foundation-aaif-donation-2026).
- Скилл planning-with-files объявляет во frontmatter shell-хуки на `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `PreCompact` — [SKILL.md](https://github.com/OthmanAdi/planning-with-files/blob/HEAD/skills/planning-with-files/SKILL.md).
- Разрывы переносимости на практике: установка отдельного скилла копирует только `skills/<name>/` и теряет репозиторные `references/` — [addyosmani #361](https://github.com/addyosmani/agent-skills/issues/361); на Windows без symlink-режима зеркальные деревья становятся текстовыми заглушками — [alirezarezvani README](https://github.com/alirezarezvani/claude-skills).

### Inferences
- Рекомендуемый профиль zero-agent: строгий writer (всё, что мы публикуем, проходит спецификацию) и лояльный reader; нативный путь `.zero/skills/`, плюс `.agents/skills/`, плюс `.claude/skills/` как чтение для совместимости.
- Frontmatter-`hooks` превращают «текстовый» скилл в автозапуск shell. Для zero-agent исполняемые расширения должны быть отдельным capability, по умолчанию выключенным для третьих лиц. Это важнейшее отличие от наивной реализации.
- Лучше использовать dedicated-инструмент активации, а не чтение файла: только он даёт контроль прав, обёртку для компакции, телеметрию и проверку целостности в момент активации.
- Lock, digest, зависимости, секреты стоит хранить вне SKILL.md (sidecar `skills.lock`, manifest пакета), чтобы не ломать совместимость и переехать на стандарт, когда #254/#46 будут приняты.

### Gaps
- Нет точного списка «70+ платформ» и данных, какие клиенты реально поддерживают `allowed-tools`.
- Первичный пресс-релиз Linux Foundation не прочитан; сроки версионированных релизов спецификации неизвестны.
- Механика Claude/Codex plugin marketplace как формата здесь не разбиралась детально; нужен отдельный проход, если zero-agent будет читать их manifest.

## Какие высокоценные workflow повторяются в коллекциях

### Takeaway
Коллекции сходятся на цепочке «выровнять понимание → спецификация → нарезка → план на диске → TDD → независимая проверка → доказательство → ретро». Различие в том, кто контролирует процесс: малые компонуемые скиллы (Pocock) или управляющие фреймворки. Эмпирика из выборки предупреждает против избытка процесса.

### Cited Findings
Формат: **название** — рецепт — источники.

**Выравнивание и спецификация**
1. **Grill / interview** — по одному вопросу, пока не закрыта каждая ветка дерева решений, и только потом работа — [grill-me](https://github.com/mattpocock/skills/blob/HEAD/skills/productivity/grill-me/SKILL.md), [interview-me](https://github.com/addyosmani/agent-skills/blob/HEAD/skills/interview-me/SKILL.md).
2. **Spec before code** — превратить обсуждение в спецификацию, сверить модули, опубликовать в трекер — [to-spec](https://github.com/mattpocock/skills/blob/HEAD/skills/engineering/to-spec/SKILL.md), [spec-driven-development](https://github.com/addyosmani/agent-skills/blob/HEAD/skills/spec-driven-development/SKILL.md), [write-prd](https://github.com/phuryn/pm-skills).
3. **Glossary + ADR** — вести общий словарь домена и решения прямо по ходу интервью, чтобы сократить токены и имена — [grill-with-docs](https://github.com/mattpocock/skills/blob/HEAD/skills/engineering/grill-with-docs/SKILL.md), [documentation-and-adrs](https://github.com/addyosmani/agent-skills/blob/HEAD/skills/documentation-and-adrs/SKILL.md).
4. **Wayfinder (карта решений)** — работу больше одной сессии вести как карту тикетов-решений в трекере; карта — индекс, решение живёт в одном тикете — [wayfinder](https://github.com/mattpocock/skills/blob/HEAD/skills/engineering/wayfinder/SKILL.md).
5. **Throwaway prototype** — ответить на вопрос дизайна одноразовым HTML или несколькими вариантами UI — [prototype](https://github.com/mattpocock/skills/blob/HEAD/skills/engineering/prototype/SKILL.md), `/prototype` и `/vertical-slice` в [Game Studios](https://github.com/Donchitos/Claude-Code-Game-Studios).

**План и исполнение**
6. **Tracer-bullet tickets** — разбить спецификацию на вертикальные срезы с явными рёбрами блокировки — [to-tickets](https://github.com/mattpocock/skills/blob/HEAD/skills/engineering/to-tickets/SKILL.md), [planning-and-task-breakdown](https://github.com/addyosmani/agent-skills/blob/HEAD/skills/planning-and-task-breakdown/SKILL.md).
7. **План на диске (3 файла)** — `task_plan.md` + `findings.md` + `progress.md`, повторная инъекция заголовка плана каждый ход, восстановление после `/clear` — [planning-with-files](https://github.com/OthmanAdi/planning-with-files).
8. **Completion gate** — не давать агенту остановиться при незавершённой фазе, с лимитом блокировок и детектором застоя — [planning-with-files](https://github.com/OthmanAdi/planning-with-files).
9. **Plan attestation** — SHA-256 утверждённого плана; при расхождении инъекция отказывает (`[PLAN TAMPERED]`) — [planning-with-files](https://github.com/OthmanAdi/planning-with-files).
10. **Frontier-параллелизм** — тикеты как граф; свежий подагент на каждый готовый тикет, свой worktree, общение через указатели, интеграционная ветка — [implement-spec](https://github.com/mattpocock/skills/blob/HEAD/skills/engineering/implement-spec/SKILL.md).
11. **Approve-once autonomy** — один раз одобрить план, далее автономно, но с TDD, коммитом на задачу и паузой на рисках — `/build auto` в [addyosmani](https://github.com/addyosmani/agent-skills).
12. **Handoff** — сжать сессию в документ для следующего агента — [handoff](https://github.com/mattpocock/skills/blob/HEAD/skills/productivity/handoff/SKILL.md), vault между харнессами в [ECC](https://github.com/affaan-m/ECC).

**Верификация и качество**
13. **TDD вертикальными срезами** — red-green-refactor по одному срезу — [tdd](https://github.com/mattpocock/skills/blob/HEAD/skills/engineering/tdd/SKILL.md), [test-driven-development](https://github.com/addyosmani/agent-skills/blob/HEAD/skills/test-driven-development/SKILL.md), [ECC](https://github.com/affaan-m/ECC).
14. **Диагностика с красным циклом** — сначала построить быстрый цикл, который краснеет на этом баге → минимизировать → гипотезы → инструментировать → фикс с регрессией → cleanup — [diagnosing-bugs](https://github.com/mattpocock/skills/blob/HEAD/skills/engineering/diagnosing-bugs/SKILL.md), [debugging-and-error-recovery](https://github.com/addyosmani/agent-skills/blob/HEAD/skills/debugging-and-error-recovery/SKILL.md).
15. **Run-and-look evidence** — изменение, видимое пользователю, не закрыто без запуска, наблюдения и сохранённого скриншота; «parse check is not a run» — [Game Studios](https://github.com/Donchitos/Claude-Code-Game-Studios).
16. **Двухосевое/многоосевое ревью** — Standards + Spec ([code-review](https://github.com/mattpocock/skills/blob/HEAD/skills/engineering/code-review/SKILL.md)) или пять осей: корректность, читаемость, архитектура, безопасность, производительность ([code-review-and-quality](https://github.com/addyosmani/agent-skills/blob/HEAD/skills/code-review-and-quality/SKILL.md)).
17. **Doubt-driven** — нетривиальное решение отдать на состязательную проверку в свежем контексте, пока исправление дёшево; определены критерии нетривиальности — [doubt-driven-development](https://github.com/addyosmani/agent-skills/blob/HEAD/skills/doubt-driven-development/SKILL.md).
18. **Source-driven** — решения по фреймворку опирать на официальную документацию и цитировать, а не на память — [source-driven-development](https://github.com/addyosmani/agent-skills/blob/HEAD/skills/source-driven-development/SKILL.md).
19. **Scope creep check** — сверить, не вырос ли diff за пределы заявленной цели — [scope-creep-detector](https://github.com/Shubhamsaboo/awesome-llm-apps/tree/HEAD/agent_skills/scope-creep-detector), `/scope-check` в [Game Studios](https://github.com/Donchitos/Claude-Code-Game-Studios).
20. **Constraint-driven** — принять стандарт качества один раз и применять везде — [constraint-driven-development](https://github.com/addyosmani/agent-skills/blob/HEAD/skills/constraint-driven-development/SKILL.md); path-scoped rules в [Game Studios](https://github.com/Donchitos/Claude-Code-Game-Studios).
21. **Merge-danger в PR** — тело PR: минимальная визуализация, before/after-доказательство, one-way/two-way door и радиус поражения — [pr](https://github.com/mattpocock/skills/blob/HEAD/skills/engineering/pr/SKILL.md).

**Самоконтроль и обучение**
22. **Agent-run forensics** — сравнить по транскрипту, что агент сделал и что заявил — [agent-run-forensics](https://github.com/alirezarezvani/claude-skills/tree/HEAD/engineering/agent-run-forensics).
23. **Human gate** — отказать в выпуске плана/миграции, пока не проверил названный человек и нет открытых блокеров — [human-gate](https://github.com/alirezarezvani/claude-skills/tree/HEAD/engineering/human-gate); career-ops: «You press Submit» — [career-ops](https://github.com/career-ops-hq/career-ops).
24. **Retro по фактам** — после сессии предложить улучшения среды или правки скиллов только при подтверждении логами — [retro](https://github.com/mattpocock/skills/blob/HEAD/skills/engineering/retro/SKILL.md), [skill-retro](https://github.com/alirezarezvani/claude-skills/tree/HEAD/engineering/skill-retro).
25. **Instincts** — автоматически извлечённые паттерны с confidence; при старте вводятся только с порогом (по умолчанию 0.7, не более 6), затем кластеризуются в скиллы — [ECC](https://github.com/affaan-m/ECC).
26. **Trigger testing** — на каждый скилл положительные промпты без его имени и один близкий отрицательный; CI блокирует новый скилл, срабатывающий реже чем на половине промптов — [VERIFIED.md](https://github.com/alirezarezvani/claude-skills/blob/HEAD/VERIFIED.md).

**Маршрутизация и дозирование процесса**
27. **Router-skill** — определить контекст по файлам проекта, затем загрузить минимальный набор; движок эксклюзивен, дисциплины аддитивны — [router](https://github.com/gamedev-skills/awesome-gamedev-agent-skills/blob/HEAD/router/SKILL.md); аналоги [ask-matt](https://github.com/mattpocock/skills/blob/HEAD/skills/engineering/ask-matt/SKILL.md), [using-agent-skills](https://github.com/addyosmani/agent-skills/blob/HEAD/skills/using-agent-skills/SKILL.md).
28. **Единый рычаг rigor** — `minimal/standard/full` управляет шестью настройками; override для отдельных систем — [Game Studios](https://github.com/Donchitos/Claude-Code-Game-Studios).

**Доменные цепочки**
29. **Audit fan-out** — параллельные специализированные агенты проверяют объект, результат сводится в приоритизированный план — [claude-seo](https://github.com/AgriciDaniel/claude-seo) (до 17 агентов одновременно).
30. **Discovery-цепочка** — идеи → допущения → приоритизация допущений → эксперименты; `/discover` — [pm-skills](https://github.com/phuryn/pm-skills).
31. **Screening pipeline** — проверить актуальность объекта, оценить по рубрике, сгенерировать артефакт, внести в трекер, решение за человеком — [career-ops](https://github.com/career-ops-hq/career-ops).
32. **Evidence-first RE** — найти поведение → трассировать до кода → вернуть находки, доказательства и неизвестные → реализовать и протестировать — [rea](https://github.com/morluto/rea).
33. **Art direction pipeline** — зафиксировать визуальную цель, строить семейства ассетов, детерминированно нормализовать, проверить внутри игры; вести provenance — [create-game-assets](https://github.com/gamedev-skills/awesome-gamedev-agent-skills/tree/HEAD/skills/disciplines/create-game-assets).
34. **Layered design** — `design_skeleton` → `design_content` → `design_refine`, с точечной подгрузкой знаний — [OpenPencil](https://github.com/ZSeven-W/openpencil).

**Эмпирические данные (все — самоизмерения авторов)**
- Четыре игры из одного брифа: на `standard` 30 документов и 58 минут до первой строки кода против 1 документа и 0 минут на `minimal`; два слепых ревьюера и плейтест поставили `standard` на последнее место — [Game Studios](https://github.com/Donchitos/Claude-Code-Game-Studios). Поэтому `minimal` сделан умолчанием.
- Возобновление после стирания контекста: 5.0 хода с планом на диске против 13.3 без него, на всех ветках тесты зелёные; автор сам помечает как internal benchmark — [planning-with-files](https://github.com/OthmanAdi/planning-with-files).
- 96.7% vs 6.7% у planning-with-files измеряет следование трёхфайловому шаблону, а не удержание цели — [там же](https://github.com/OthmanAdi/planning-with-files). Цифру нельзя цитировать как прирост качества.
- Только 152 из 389 проверенных скиллов сработали на всех plain-language промптах и молчали на отрицательном; выборка мала (2 положительных промпта) — [VERIFIED.md](https://github.com/alirezarezvani/claude-skills/blob/HEAD/VERIFIED.md).
- Pocock критикует GSD, BMAD и Spec-Kit за то, что те «владеют процессом» и отнимают контроль — [mattpocock README](https://github.com/mattpocock/skills).

### Inferences
- Самый сильный паттерн — разделение **механизма** и **политики**: механизм (хранилище плана, stop-gate, запись доказательств) принадлежит ядру, а политика (сколько вопросов, когда TDD) — скиллам и уровню rigor.
- По замеру alirezarezvani 237 из 389 скиллов (61%) не прошли строгий тест срабатывания. Выборка мала, но ценность большого каталога, вероятно, завышена; проверка срабатывания важнее количества.
- Умолчанием zero-agent должен быть низкий rigor с автоматическим повышением по риску действия (необратимость, прод, деньги), а не по типу задачи.
- Многие методы описаны как инженерные, но обобщаются: grill, plan-on-disk, doubt, gate работают и для документов, маркетинга, PM.

### Gaps
- Нет независимых контролируемых исследований эффективности любой из методик; все цифры — от авторов.
- Содержимое многих SKILL.md (pm-skills, claude-seo, ECC) изучено только по описаниям README.

## Что встроить, что поставлять паками, что пропустить

### Takeaway
В ядро идут механизмы, которые скилл физически не может обеспечить или которые нужны для безопасности. Методики поставляются первым набором подписанных редактируемых скиллов. Доменные знания — опциональные паки. Пропускаются персонажные иерархии, swarm-маркетинг и массовые каталоги.

### Cited Findings
- Хуки и скиллы как текст ненадёжны: planning-with-files описывает уровни хостов — жёсткая блокировка лишь в Claude Code, Codex, Continue; follow-up инъекция в других; только уведомление в прочих — [README](https://github.com/OthmanAdi/planning-with-files).
- Мод-плагины ruflo работают с правами аккаунта и не изолированы — [ruflo README](https://github.com/ruvnet/ruflo).
- wshobson генерирует артефакты для 7 харнессов из одного источника и прямо пишет, что LLM-судья и Monte Carlo не валидированы на человеческой разметке — [wshobson README](https://github.com/wshobson/agents).
- ruflo заявляет «100+ агентов», федерацию, swarm; бенчмарки опубликованы самим автором — [ruflo README](https://github.com/ruvnet/ruflo).

### Inferences
Вся таблица ниже — моя рекомендация.

| Решение | Что | Почему |
|---|---|---|
| **Ядро** | Runtime Agent Skills: discovery, каталог, `activate_skill`, обёртка, защита от компакции, дедупликация | Требует харнесса |
| **Ядро** | План на диске как первоклассный объект: фазы, инъекция, attest, ledger JSONL | Workflow 7, 9; нужно переживать компакцию |
| **Ядро** | Completion gate с лимитом и детектором застоя | Скилл не может удержать останов сам |
| **Ядро** | Evidence ledger: заявление «готово» связывается с командой/артефактом/скриншотом | Workflow 15; основа самоотчёта |
| **Ядро** | Подагент со свежим контекстом и worktree; указатели вместо копий | Workflow 10, 17 |
| **Ядро** | Классификатор риска и approval для необратимых действий | Workflow 23; send_documents, deploy, публикации |
| **Ядро** | Единый rigor с эскалацией по риску | Workflow 28 |
| **Ядро** | Журнал запусков для forensics, retro; handoff-экспорт | Workflow 12, 22, 24 |
| **Ядро** | Пайплайн установки скиллов (см. раздел о рисках) и `skills.lock` | Безопасность |
| **Ядро** | Harness для trigger/eval скиллов | Workflow 26 |
| **Ядро** | MCP-клиент с фильтрацией инструментов и политиками | См. раздел MCP |
| **Ядро, база** | Память с provenance и порогами; предложения в скиллы только через ревью | Workflow 25, но без автоприменения |
| **Bundled core pack** | grill, spec, tickets, wayfinder, TDD, diagnose, review, doubt, source-driven, research-with-citations, domain-model/ADR, retro, handoff, router | Методики редактируемы, не должны быть зашиты |
| **Опциональные паки** | `pm`, `marketing-seo`, `docs-office`, `deploy`, `re`, `gamedev`, `career`, `compliance-finance` | Доменные знания и зависимости |
| **Паки-интеграции** | DocuSeal, Coolify, OpenPencil, Ghidra/REA, Godot/Unity/Blender | Внешние процессы, отдельные разрешения |
| **Пропустить** | Иерархии из десятков персонажей (49 агентов; C-level персоны) как базовый механизм | Роль в ревью полезна, симуляция штата нет; см. замер Game Studios |
| **Пропустить** | Swarm/federation/self-learning из ruflo | Нет независимых подтверждений, большая поверхность |
| **Пропустить** | Адаптерная фабрика под чужие харнессы | zero-agent читает стандарт; экспорт не приоритет |
| **Пропустить** | Каталоги на 400+ скиллов в поставке | Шум каталога, ненадёжные триггеры |
| **Пропустить** | Копирование браузерных логинов и обход анти-бота в стиле Agent-Reach | Риск аккаунтов; см. раздел рисков |
| **Пропустить** | Автоматическая мутация скиллов без человека | Перекликается с memory poisoning |
| **Отложить** | Self-improving/Monte Carlo-оценка скиллов, RL-среды из REA | Зрелость низкая |

### Gaps
- Как именно устроены instincts ECC и насколько они полезны, не проверялось: прочитан README, код `continuous-learning-v2` нет.
- Стоимость в токенах повторной инъекции для zero-agent неизвестна; PWF заявляет 289 мс на срабатывание хука, но токены не приводит в прочитанных разделах.

## Какие интеграции нужны для Group C

### Takeaway
Лучший шаблон — интеграция как **пара: пак (скилл + политика рисков) + существующий MCP/CLI инструмента**, а не собственный клиент. У DocuSeal, Coolify и OpenPencil MCP уже встроен, у Ghidra нет, и нужен headless-слой.

### Cited Findings
**Документы и подпись**
- DocuSeal: API, webhooks, встроенные `app/controllers/mcp/*` с `search_templates`, `load_template`, `create_template`, `send_documents`, `search_documents`; MCP-токены отдельной моделью — [дерево](https://github.com/docusealco/docuseal/tree/HEAD/app/controllers/mcp). Прочитан только `send_documents`; остальные имена выведены из имён контроллеров.
- Лицензия AGPL-3.0; часть функций Pro: HTML API шаблонов, DOCX/PDF field tags, embedded, bulk send, conditional fields — [README](https://github.com/docusealco/docuseal).
- Генерация DOCX/PDF/PPTX/XLSX: в `anthropics/skills` эти скиллы лежат с собственным `LICENSE.txt`; README говорит, что «многие» скиллы Apache-2.0, а документные включены отдельно — [anthropics/skills](https://github.com/anthropics/skills).
- В `awesome-mcp-servers` DocuSeal не найден — поиск по [README](https://github.com/punkpeye/awesome-mcp-servers).

**Дизайн**
- OpenPencil: MIT, `op-mcp` (stdio через бинарник `--mcp <path>`, HTTP `127.0.0.1:<port>/mcp` от работающего приложения), без Node; CLI `op`, `op start --headless --file design.op`; `.op` — JSON, diffable; экспорт в React/Tailwind, Vue, Svelte, Flutter, SwiftUI, Compose, RN; инкрементальный codegen; скилл [openpencil-skill](https://github.com/ZSeven-W/openpencil-skill) — [README](https://github.com/ZSeven-W/openpencil).
- Figma: `GLips/Figma-Context-MCP`, `dannote/figma-use` — [awesome-mcp-servers](https://github.com/punkpeye/awesome-mcp-servers).
- README OpenPencil отмечает ложные срабатывания антивируса и предлагает `curl | bash`/`irm | iex` — [README](https://github.com/ZSeven-W/openpencil).

**Deploy**
- Coolify: 360+ one-click сервисов, управление через dashboard, API, CLI, MCP — [README](https://github.com/coollabsio/coolify).
- `app/Mcp/` содержит Tools (82 файла, не обязательно равно числу инструментов), Resources (`InfrastructureOverviewResource`) и Prompts (`ExplainFailedDeploy`, `TroubleshootApplication`) — [дерево](https://github.com/coollabsio/coolify/tree/HEAD/app/Mcp). MCP включается middleware на уровне инстанса и команды (`EnsureMcpEnabled`, `EnsureTeamMcpEnabled`).
- `ChecksTokenAbilities` скрывает инструменты, недоступные токену, чтобы read-only соединения не видели write-инструментов — [файл](https://github.com/coollabsio/coolify/blob/HEAD/app/Mcp/Concerns/ChecksTokenAbilities.php).
- Среди инструментов есть `GetEnvValues`, `Deploy`, `DockerCleanup`, `CreateDatabase`.
- В каталоге есть сторонний `frndchagas/coolify-mcp` — [awesome-mcp-servers](https://github.com/punkpeye/awesome-mcp-servers).

**Реверс-инжиниринг**
- В дереве Ghidra нет MCP (совпадение `mcp` только случайное, `ArmCpuState.java`); есть `analyzeHeadless`, `pyghidraRun`, модуль PyGhidra (CPython-библиотека, ScriptProvider), последний релиз 12.2 — [дерево](https://github.com/NationalSecurityAgency/ghidra/tree/HEAD/Ghidra/Features/PyGhidra), [releases](https://github.com/NationalSecurityAgency/ghidra/releases).
- Драйверы: [LaurieWired/GhidraMCP](https://github.com/LaurieWired/GhidraMCP) (11.8k★), [cyberkaida/reverse-engineering-assistant](https://github.com/cyberkaida/reverse-engineering-assistant) (947★), [clearbluejar/pyghidra-mcp](https://github.com/clearbluejar/pyghidra-mcp) (459★), [mrexodia/ida-pro-mcp](https://github.com/mrexodia/ida-pro-mcp) (13.4k★).
- pyghidra-mcp: без GUI, на pyghidra/JPype, единица анализа — проект из нескольких бинарников, транспорт streamable-http, beta; пример с `notepad.exe → kernel32 → kernelbase → ntdll` занял 22 вызова — [clearbluejar](https://clearbluejar.github.io/posts/pyghidra-mcp-headless-ghidra-mcp-server-for-project-wide-multi-binary-analysis).
- re-mcp-ghidra: Ghidra 12+, JDK 21+, stdio/proxy/HTTP daemon — [PyPI](https://pypi.org/project/re-mcp-ghidra/). Ryuumonbuchi заявляет 216 инструментов, только Linux, независимых обзоров нет — [PyPI](https://pypi.org/project/ryuumonbuchi/).
- REA: один MCP + CLI для нативных бинарников (провайдеры Hopper, Ghidra, IDA, Kuna), JS/Electron, .NET, сайтов; встроенные статические инспекторы (PE imports/certs, Go/Rust, GGUF, IL2CPP, Godot PCK); результаты с доказательствами и ограничениями; компактные схемы по умолчанию (`REA_MCP_INPUT_SCHEMA_PROFILE`), профили вывода; Node ≥22 — [README](https://github.com/morluto/rea).
- REA декларирует применение только к ПО, которым вы владеете или анализ которого разрешён, и запрещает крэкнутые инструменты — [README](https://github.com/morluto/rea).
- `x64dbgMCP`, Binary Ninja, radare2, Frida представлены в каталоге единичными записями — [awesome-mcp-servers](https://github.com/punkpeye/awesome-mcp-servers).

**Game dev**
- Skills: awesome-gamedev (77 + роутер, 12 движков, версии пинятся к релизу движка, `validate-skills.py`, `godot --headless` для тестов и экспорта) — [README](https://github.com/gamedev-skills/awesome-gamedev-agent-skills).
- Движки через MCP: `Coding-Solo/godot-mcp`, `IvanMurzak/Unity-MCP`, `ahujasid/blender-mcp`, Unreal — несколько записей; секция Gaming — 75 записей — [awesome-mcp-servers](https://github.com/punkpeye/awesome-mcp-servers).
- Game Studios: 15 хуков, 14 path-scoped правил, 62 шаблона; всё заточено под Claude Code — [README](https://github.com/Donchitos/Claude-Code-Game-Studios).

### Inferences
| Домен | Подключение | Риск-класс | Что нужно от ядра |
|---|---|---|---|
| Документы | Локальные генераторы DOCX/PDF/XLSX/PPTX как bundled-скиллы + проверка рендером | Низкий | Sandbox, скриншот рендера как evidence |
| Подпись | DocuSeal по сети (API/MCP); read-only и черновики по умолчанию | Высокий: `send_documents` рассылает реальным людям | Approval с предпросмотром получателей; AGPL не встраивать в кодовую базу |
| Дизайн | OpenPencil MCP/CLI headless; `.op` в git; Figma — опционально | Средний | Запуск внешнего бинарника, валидация через экспорт + скриншот |
| Deploy | Coolify MCP с отдельным read-only токеном; write только по approval | Высокий: `GetEnvValues` раскрывает секреты, Deploy меняет прод | Редакция секретов в выводе, scoped токены |
| RE | Абстракция провайдера: pyghidra-mcp или REA как backend; статические инспекторы отдельно | Высокий: анализируемый файл враждебен | Контейнер без сети, вывод бинарника как untrusted, юридический гейт |
| Game dev | Роутер + движок; `godot --headless`; MCP редактора | Средний | Evidence-скриншот обязателен |

- Выбор Ghidra-слоя: для автономности zero-agent нужен headless (pyghidra-mcp/re-mcp-ghidra), GUI-плагин подходит только для совместной работы с человеком. Перед фиксацией нужен практический тест; REA стоит сравнить как готовый backend, но его 7472 файла и темп релизов создают риск зависимости.
- Строки из бинарников, метаданные и декомпиляция могут содержать prompt injection; вывод RE-инструментов должен помечаться как недоверенный. Это экстраполяция на RE общей угрозы, источника с замером не найдено.
- Шаблон из Coolify (скрывать, а не запрещать) и из REA (компактные схемы) стоит сделать правилами MCP-слоя ядра.

### Gaps
- Лицензионные условия `LICENSE.txt` документных скиллов Anthropic не прочитаны; переиспользовать их до проверки нельзя.
- Не проверено, какие MCP-инструменты DocuSeal доступны в CE и в self-hosted. Сравнение Ghidra MCP-серверов на практике не проводилось.
- Unity/Unreal-серверы не оценивались, много похожих проектов без явного лидера. Godot-серверов в каталоге несколько.
- Подтверждения, что LaurieWired/GhidraMCP требует GUI, из первичных данных не собрано (сужу по общему описанию плагинов); уточнить в README.

## Какие классы MCP-серверов обязательны для универсального агента

### Takeaway
Обязательный минимум — поиск/извлечение, браузер, git-форж, база данных, sandbox-выполнение, память/знания, документы/рабочие приложения и мониторинг, причём файлы, shell и базовый web лучше сделать нативными инструментами. Каталог из 4632 записей не означает качества: нужен курируемый allowlist и шлюз.

### Cited Findings
- Распределение записей ([README raw](https://raw.githubusercontent.com/punkpeye/awesome-mcp-servers/HEAD/README.md)): Developer Tools 558, Finance & Fintech 490, Knowledge & Memory 368, Search & Data Extraction 270, Security 267, Communication 184, Databases 149, Cloud Platforms 149, Aggregators 138, Workplace & Productivity 138, Marketing 124, Browser Automation 122, Coding Agents 101, Monitoring 98, Art & Culture 98, Multimedia 82, Gaming 75, Social 73, Research 63, File Systems 52, Data Platforms 50, Data Science 46, Legal 39, Version Control 34, Command Line 29, OS Automation 23, Code Execution 23, Product Management 19. Подсчёт мой, по строкам `^- [` в секциях. Важно: запрос README через GitHub API вернул ровно 512 000 байт, а полный raw-файл весит 1.6 МБ, поэтому подсчёт сделан по raw.
- Каталог отмечает официальные реализации 🎖️ и язык/область/ОС; часть записей с бейджем Glama, есть метрика качества определений инструментов TDQS — [README](https://github.com/punkpeye/awesome-mcp-servers).
- Спецификация MCP 2026-07-28: stateless-транспорт, усиление авторизации, W3C trace context в `_meta`; по вторичным источникам устаревают Roots, Sampling, Logging — [release](https://github.com/modelcontextprotocol/specification/releases/tag/2026-07-28), [обзор](https://www.developersdigest.tech/blog/mcp-2026-07-28-breaking-changes).
- Сервер REA ограничивает размер схем профилями, потому что клиенты могут ограничивать кадр `tools/list` — [README](https://github.com/morluto/rea). OpenPencil отдаёт знания сегментами вместо одного промпта — [README](https://github.com/ZSeven-W/openpencil).
- Реальные инциденты: MCPoison (CVE-2025-54136) в Cursor — подмена уже одобренной конфигурации; тайпсквот Postmark MCP с BCC на адрес атакующего — [CSA](https://labs.cloudsecurityalliance.org/research/csa-research-note-ai-skill-supply-chain-attacks-20260624-csa/).

### Inferences
| Уровень | Класс | Примечание |
|---|---|---|
| **Нативно, не MCP** | Файлы, shell, patch, базовый fetch, LSP | Критичны для латентности, контроля и sandbox |
| **Must-have** | Search & extraction | Для research и всех доменов |
| **Must-have** | Browser automation | Проверка UI, сайты без API |
| **Must-have** | Forge/Version control (GitHub/GitLab/трекер) | Issues как внешняя память задач |
| **Must-have** | Databases (read-only по умолчанию) | Данные, аналитика |
| **Must-have** | Code execution / sandbox | Изоляция недоверенного кода |
| **Must-have** | Knowledge & Memory | Лучше нативное ядро + опциональные backend |
| **Must-have** | Workplace (Docs, Drive, Calendar, Notion) + Communication (почта, Slack) | С approval на отправку |
| **Must-have** | Monitoring/Observability | Замыкает цикл deploy и диагностики |
| **Pack** | Cloud, Design, RE, Gaming, Multimedia, Finance, Legal, Marketing, Social | Подключаются доменом |
| **Инфраструктура** | Aggregators/gateway | Лучше собственный слой ядра: lazy tool loading, пин digest описаний, разделение токенов |

- Требования к MCP-слою: пин хеша описаний инструментов и повторное подтверждение при изменении (ответ на rug-pull); lazy-загрузка схем; скрытие инструментов по правам; реализация ревизии 2026-07-28 как основной.
- Описания инструментов надо считать тем же классом угрозы, что и SKILL.md.

### Gaps
- Число записей не отражает качество или использование; отбор лучших серверов в каждом классе не выполнен.
- Первичный текст MCP 2026-07-28 здесь не читался; детали устаревания взяты из вторичных обзоров. Tool search и code execution в MCP не подтверждены найденными источниками.
- Вероятно, смежный исследователь покрывает MCP глубже; здесь только классы.

## Какие риски supply-chain у скиллов и какой нужен vetting

### Takeaway
Скилл — это исполняемая инструкция с правами агента. Сканеры сами по себе недостаточны: payload-less атаки дают 0% обнаружения, упаковка обходит сканеры в 90%+ случаев. Подпись доказывает происхождение, но не безопасность. Нужна глубокая оборона: пин по хешу, запуск с минимальными правами, sandbox-детонация, контроль на уровне исполнения и повторная проверка при каждом обновлении.

### Cited Findings
**Масштаб**
- Snyk ToxicSkills: 3984 скилла ClawHub и skills.sh на 2026-02-05; 534 (13.4%) с CRITICAL; 1467 (36.82%) с любым дефектом; 76 подтверждённых вредоносных; 91% вредоносных сочетают prompt injection; у ClawHub 10.9% содержат hardcoded secrets — [Snyk](https://snyk.io/blog/toxicskills-malicious-ai-agent-skills-clawhub/). Внимание: заголовок в выдаче гласит «1467 Malicious Payloads», но извлечённое тело различает 1467 дефектных и 76 вредоносных. Заголовок страницы Snyk также говорит о «prompt injection в 36%», тогда как в извлечённой таблице 36.82% — это любой дефект. Перед цитированием проверить по оригиналу вручную.
- Koi Security: 341 вредоносный из 2857 (11.9%) на ClawHub, февраль 2026; кампания ClawHavoc выросла до 1184; полезная нагрузка — Atomic macOS Stealer через поддельные prerequisite-шаги в SKILL.md — [CSA](https://labs.cloudsecurityalliance.org/research/csa-research-note-ai-skill-supply-chain-attacks-20260624-csa/).
- Unit 42: пять вредоносных скиллов обошли VirusTotal и ClawScan, в том числе раздуванием файла — [Cyber Security News](https://cybersecuritynews.com/openclaw-skill-marketplace-exposes-ai-agents/), [CSA](https://labs.cloudsecurityalliance.org/research/csa-research-note-ai-skill-supply-chain-attacks-20260624-csa/).

**Почему сканеров мало**
- Semantic Compliance Hijacking: вредоносная цель переписана как «правила соответствия», агент сам генерирует и запускает код; 77.67% утечка, 67.33% RCE, 0.00% обнаружения манипулированных файлов; 3 фреймворка × 3 модели — [arXiv:2605.14460](https://arxiv.org/abs/2605.14460).
- Атаки на реестр: до 86% pairwise win rate и 80% Top-10 в discovery; 77.6% выбора вредоносного варианта; 36.5–100% ухода от блокирующего вердикта — [arXiv:2605.11418](https://arxiv.org/abs/2605.11418).
- SkillCloak: обфускация обходит большинство сканеров более чем в 80%, до 96% для гибридного; self-extracting packing — более 90% для всех восьми сканеров на 1613 реальных вредоносных скиллах. SkillDetonate (sandbox + taint): 97% при 2% FP и 87% на реальных образцах — [arXiv:2607.02357](https://arxiv.org/abs/2607.02357). По вторичному источнику, около 2.5 минут на скилл — [buildmvpfast](https://www.buildmvpfast.com/blog/agent-skills-linux-foundation-aaif-donation-2026).

**Классы атак (по источникам выше)**
- Внешняя загрузка бинарников (часто запароленные ZIP), обфускация base64/Unicode, отключение защит, бэкдоры — [Snyk](https://snyk.io/blog/toxicskills-malicious-ai-agent-skills-clawhub/).
- Персистентность через файлы памяти (`SOUL.md`, `MEMORY.md`) — [Snyk](https://snyk.io/blog/toxicskills-malicious-ai-agent-skills-clawhub/).
- Rug-pull: поведение меняется после ревью или в обновлении — [CSA](https://labs.cloudsecurityalliance.org/research/csa-research-note-ai-skill-supply-chain-attacks-20260624-csa/).
- Низкий порог публикации: SKILL.md и недельный GitHub-аккаунт — [Snyk](https://snyk.io/blog/toxicskills-malicious-ai-agent-skills-clawhub/).
- Подмена через репозиторий: многие проекты содержат собственные каталоги агентов; у Coolify в дереве есть `.agents/skills`, `.claude/skills`, `.cursor/skills`, `.mcp.json`, `.cursor/mcp.json`; у REA — `.agents/skills` — [дерево Coolify](https://github.com/coollabsio/coolify), [дерево REA](https://github.com/morluto/rea). (Это обычные файлы разработки, но они показывают вектор.)

**Защитные инструменты**
- Целостности в формате нет: frontmatter без хешей и подписей — [dev.to](https://dev.to/piiiico/agent-skills-has-no-integrity-layer-we-built-one-33o5).
- NVIDIA подписывает весь каталог через OpenSSF Model Signing, detached `skill.oms.sig`; strict-проверка ловит добавленные файлы; порядок: SkillSpector → skill card → подпись. Явно: подпись не доказывает безопасность — [NVIDIA](https://docs.nvidia.com/skills/signing-agent-skills).
- Lock-манифест обнаруживает дрейф, но не является provenance или проверкой содержания — [skills-lock](https://leadmagic.io/gtm-skills/skills-lock).
- `uvx mcp-scan@latest --skills` — [Snyk](https://snyk.io/blog/toxicskills-malicious-ai-agent-skills-clawhub/); AgentShield — [ECC](https://github.com/affaan-m/ECC); `skill-security-auditor` — [alirezarezvani](https://github.com/alirezarezvani/claude-skills); CI «security + eval gate» — [awesome-llm-apps](https://github.com/Shubhamsaboo/awesome-llm-apps); `ruflo verify` — [ruflo](https://github.com/ruvnet/ruflo).
- Рекомендации CSA: инвентаризация, пины и integrity, ревью и кода, и естественного языка, re-review при каждом изменении, least privilege, sandbox, логирование чтений `.ssh/`, `.env`, keychain и неожиданных исходящих соединений — [CSA](https://labs.cloudsecurityalliance.org/research/csa-research-note-ai-skill-supply-chain-attacks-20260624-csa/).
- Anthropic пока только предлагает security guidance в дорожной карте — [Anthropic](https://www.anthropic.com/news/donating-agent-skills-to-the-agentic-ai-foundation).

**Агентный доступ к аккаунтам**
- Agent-Reach копирует логин сайта из Chrome в собственный браузер; заявляет, что пароли не читаются; всё хранится в `~/.agent-reach/` — [docs/README_en](https://github.com/Panniantong/Agent-Reach/blob/HEAD/docs/README_en.md).

### Inferences
**Рекомендуемый пайплайн для zero-agent** (собственная разработка на основе цитированных данных):

| Шаг | Контроль | От чего защищает |
|---|---|---|
| 0. Tiers | Bundled → доверенный publisher → проверенный → неизвестный; разные права | Базовая политика |
| 1. Получение | Только явным действием; preview перед установкой; проверка имени против тайпсквотинга | Подмена издателя, тайпсквот |
| 2. Pin | `skills.lock` с SHA-256 всего дерева, источник, коммит; активация проверяет хеш | Rug-pull, тихие обновления |
| 3. Статика | Scan: скрипты, base64/Unicode, невидимые символы, размер, архивы, URL, frontmatter `hooks`, `allowed-tools` | Массовый мусор; это только фильтр |
| 4. Семантика | LLM-ревью description и тела: несоответствие заявленной цели, «правила соответствия», требования отключить защиту, установка prerequisite | Payload-less; ненадёжно само по себе |
| 5. Детонация | Для tier «неизвестный»: запуск в sandbox с фейковыми канарейками, taint | Упаковка, динамические payload |
| 6. Права | Deny by default: сеть, секреты, shell, запись вне workspace; capabilities манифеста; `allowed-tools` никогда не расширяет прав без подтверждения | Сводит ущерб к минимуму даже при пропуске |
| 7. Runtime | Содержимое скилла — данные с меткой источника; подтверждение high-risk действий; мониторинг чтения `.ssh`, `.env`; запрет загрузки и запуска бинарников из инструкций | Основной рубеж |
| 8. Проектные скиллы | Workspace trust; без него `.agents/skills`, `.mcp.json`, hooks не загружаются | Вредоносный клон |
| 9. Обновления | Новый хеш → полный цикл; показ diff; без авто-обновления для внешних скиллов | Rug-pull |
| 10. Подпись | Поддержать OMS/Sigstore как проверку publisher, когда есть | Только provenance |
| 11. Отзыв | Blocklist, инвентаризация, запись активаций для расследования, ротация секретов при инциденте | Реакция |
| 12. Память | Запись в память с provenance; скилл не меняет память и другие скиллы без ревью | Персистентность |

- Главный вывод: поскольку обнаружение принципиально неполно, проект должен **исходить из обхода сканера** и ограничивать последствия на уровне прав и sandbox.
- Выбор скилла моделью тоже уязвим (77.6%): нельзя разрешать автоактивацию скилла неизвестного tier и показывать его в общем каталоге вместе с доверенными; похожие скиллы должны разрешаться по политике, а не по красоте description.
- `npx skills` и `npx ...@latest` исполняют код из реестра без пина, поэтому установщик скиллов в zero-agent должен быть встроенным и не запускать сторонний код; `curl | bash` не должен быть рекомендуемым путём в паках.
- MCP-сервер и скилл нужно вести через единый инвентарь и общий pipeline: `npx -y pkg@latest` в конфигурации MCP (как у ruflo) эквивалентен неприколотому обновлению кода.

### Gaps
- Частота реальных заражений среди пользователей неизвестна; данные описывают каталоги, а не инциденты.
- Сравнение надёжности семантического ревью LLM не найдено; arXiv:2605.11418 показывает, что автоматическая классификация обходится.
- Точный процесс модерации skills.sh, Anthropic Marketplace и Vercel CLI не подтверждён: поиск не дал первичных источников. Неясно, какие проверки там есть на ноябрь 2026.
- Цифры Koi, Trend Micro и Unit 42 взяты через вторичные источники, а не из оригинальных отчётов.

## Что брать из репозиториев знаний (Group B)

### Takeaway
Group B не даёт кода для ядра, но служит чек-листом и справочником. Главная польза — словарь паттернов и списки рисков; прямые зависимости брать не нужно.

### Cited Findings
- [ai-agents-for-beginners](https://github.com/microsoft/ai-agents-for-beginners): 18 уроков, включая tool use, agentic RAG, planning, multi-agent, metacognition, production, protocols (MCP, A2A, NLWeb), context engineering, memory, CUA, deploying, local agents, securing.
- [agents-towards-production](https://github.com/NirDiamant/agents-towards-production): разделы tool integration, data processing, RAG, memory, deployment, multi-agent, GPU, security (LlamaFirewall, Apex), frameworks, model customization, tracing, evaluation, UI; README содержит большие блоки Tutorial Sponsors, многие материалы привязаны к вендору.
- [RAG_Techniques](https://github.com/NirDiamant/RAG_Techniques): foundational, query enhancement, context enrichment, advanced retrieval, iterative/adaptive, evaluation, memory-augmented, explainability, advanced architectures.
- [llm-course](https://github.com/mlabonne/llm-course): fundamentals/scientist/engineer; включая agents, inference optimization, deployment, securing; последний push февраль 2026.
- [awesome-llm-apps](https://github.com/Shubhamsaboo/awesome-llm-apps): Agent Skills с заявленным security+eval CI gate; Always-on Agents (HN-брифинг, Release Radar), Generative UI, Voice, game-playing agents, memory apps; скилл Advisor-Orchestrator-Worker.
- [aie-book](https://github.com/chiphuyen/aie-book): автор предупреждает «NOT a tutorial book»; есть chapter-summaries, prompt-examples, case-studies, resources.
- [claude-howto](https://github.com/luongnv89/claude-howto): пошаговый разбор slash commands, memory, skills, subagents, MCP, hooks, plugins, checkpoints; полезен как инвентарь того, что пользователи ждут от современного харнесса.

### Inferences
- Для спецификации zero-agent наиболее полезны: claude-howto (feature parity), microsoft (таксономия), RAG_Techniques + llm-course (раздел knowledge/memory), agents-towards-production (производственный чек-лист evaluation/tracing/security).
- RAG не должен быть ядром: файлы плюс поиск и опциональный индекс, RAG-техники — пак.
- Лицензия NOASSERTION у двух репозиториев Diamant и отсутствие лицензии у aie-book означают: только чтение, без копирования материалов.
- Шаблоны Always-on (мониторинг + проактивный брифинг) и Advisor-Orchestrator-Worker стоит рассмотреть как модели фоновых задач и маршрутизации моделей.

### Gaps
- Содержание уроков, ноутбуков и файлов aie-book не прочитано: только оглавления. Формальные метрики RAG-техник не сравнивались.
- Условия лицензий Diamant не проверены.
