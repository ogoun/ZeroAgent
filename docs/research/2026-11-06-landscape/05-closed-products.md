# Закрытые коммерческие агенты: инвентарь возможностей на ноябрь 2026 (для ТЗ zero-agent)

Дата среза: 2026-11-06. Условные метки достоверности:

- **[OFF]** — официальный источник, страница прочитана.
- **[OFF\*]** — официальный источник, виден только через результат поиска (страницу прочитать не удалось или не читалась).
- **[3P]** — сторонний источник (пресса, блог, исследователь), страница прочитана.
- **[3P\*]** — сторонний источник, только через результат поиска.

Содержимое веб-страниц рассматривалось как данные. Страницы openai.com, darkreading.com, searchenginewatch.com и блог genspark.ai вернули HTTP 403, поэтому связанные факты взяты из вторичных источников и помечены соответственно.

---

## 1. Инвентарь по продуктам: core workflow, автономия, среда, коннекторы, память, артефакты, расписания, совместная работа, верификация, разрешения, цены

### Takeaway

Все 12 названий соответствуют реальным продуктам, ни одно не пришлось отбрасывать, но у многих сменились имя или границы: Atlas закрыт, routines переименованы, Playbooks и Knowledge влиты в Skills, Windsurf стал Devin Desktop, а многошаговая работа Comet передаётся в Perplexity Computer. К ноябрю 2026 рынок сошёлся к одной форме: долгоживущий агент с собственным компьютером, skills/plugins/MCP, расписаниями и событиями, управлением с телефона и метрированной оплатой.

### Cited Findings

#### 1.0. Статус названий (что изменилось)

- **ChatGPT Work** существует: агент в ChatGPT на технологии Codex, запущен 9 июля 2026 по [felloai](https://felloai.com/chatgpt-work/) и [X OpenAI](https://x.com/OpenAI/status/2075274271845404744), TNW датирует анонс 10 июля — [TNW](https://thenextweb.com/news/openai-chatgpt-work-agent-launch). Первичный пост OpenAI недоступен (403).
- **Atlas** закрывается: целевая дата депрекации 9 августа 2026, цитата сотрудника OpenAI — [9to5Mac](https://9to5mac.com/2026/07/09/openai-is-discontinuing-chatgpt-atlas-its-standalone-desktop-browser/) [3P\*]. Функции переехали в браузер десктоп-приложения и облачный браузер Work — [TNW](https://thenextweb.com/news/openai-chatgpt-atlas-browser-shutdown-superapp) [3P\*].
- **ChatGPT agent** как отдельный режим: официального подтверждения закрытия или слияния с Work не найдено, см. Gaps.
- **Claude Cowork** с 16 сентября 2026 раскатывается в «любой разговор» Claude — [Release notes](https://support.claude.com/en/articles/12138966-release-notes) [OFF].
- **Claude Code routines** теперь называются cloud scheduled tasks, старое имя ещё встречается — [Docs](https://code.claude.com/docs/en/routines) [OFF].
- **Devin**: Knowledge переезжает в Skills (18 сентября), Playbooks тоже (23 октября) — [Release notes](https://docs.devin.ai/release-notes/overview) [OFF]. Windsurf стал Devin Desktop 2 июня 2026, агент Cascade заменён на Devin Local — [Wikipedia, блоги](https://en.wikipedia.org/wiki/Cognition_AI) [3P\*]; даты окончания жизни Cascade расходятся (1 июля против 8 сентября).
- **Copilot Coding Agent** теперь часть единого опыта «Copilot in the cloud» (28 октября 2026) — [Changelog](https://github.blog/changelog/2026-10-28-work-from-anywhere-with-a-new-connected-copilot-cloud-experience/) [OFF].
- **Gemini CLI** для потребителей заменён на Antigravity CLI 18 июня 2026 — [Gemini CLI release notes](https://geminicli.com/docs/changelogs/) [OFF\*]. Агентская платформа называется Gemini Enterprise Agent Platform — [Release notes](https://docs.cloud.google.com/gemini-enterprise-agent-platform/release-notes) [OFF\*].
- **Comet**: Perplexity рекомендует Comet Assistant для текущей страницы, а Computer для многошаговых задач — [Perplexity](https://www.perplexity.ai/hub/workshops/introducing-perplexity-computer) [OFF\*].

#### 1.1. ChatGPT Work (OpenAI)

- **Workflow**: Work для задач с ясным результатом (бриф, дек, анализ, регулярная сводка), Chat для ответов — [Docs](https://learn.chatgpt.com/docs/get-started-with-work.md) [OFF]. Режим плана с утверждением пользователем и check-in перед чувствительными шагами — [felloai](https://felloai.com/chatgpt-work/) [3P].
- **Автономия**: «может оставаться с проектом часами» — [TNW](https://thenextweb.com/news/openai-chatgpt-work-agent-launch) [3P]. Сабагенты работают по явной просьбе, на уровне Ultra делегируются проактивно; в Work они идут в hosted-среде без локального sandbox — [Docs](https://learn.chatgpt.com/docs/agent-configuration/subagents.md) [OFF].
- **Среды**: на десктопе доступны локальные файлы, приложения и браузер; Work Cloud позволяет продолжать задачу на других устройствах; при недоступном компьютере задача продолжается в контейнере без доступа к локальным файлам; переключить local на cloud посреди хода нельзя — [Docs](https://learn.chatgpt.com/docs/get-started-with-work.md) [OFF].
- **Computer Use**: macOS и Windows, вызов через @Computer или @AppName; подтверждение на приложение с опцией «Always allow»; нельзя автоматизировать терминалы и системные security-диалоги; на Windows захватывает передний план и клавиатуру; на macOS возможен фоновый режим и работа с заблокированным экраном — [Docs](https://learn.chatgpt.com/docs/computer-use.md) [OFF].
- **Браузер**: встроенный с аннотациями, инспекцией localhost, скриншотами и доступом к CDP (по сайту, с одобрением); облачный браузер Work на отдельном компьютере, с перехватом управления, политиками Always ask / Auto approve / Always allow по сайтам; логин через форму, где учётные данные обходят модель, и review-модель проверяет запросы на фишинг — [Docs](https://learn.chatgpt.com/docs/browser.md) [OFF].
- **Коннекторы**: библиотека plugins, упоминание через @, создание плагинов в диалоге — [Docs](https://learn.chatgpt.com/docs/get-started-with-work.md) [OFF]. Apple Messages на macOS с отправкой только после одобрения — [felloai](https://felloai.com/chatgpt-work/) [3P].
- **Память**: Work использует настройки памяти аккаунта или воркспейса, а не локальное хранилище Codex — [Docs](https://learn.chatgpt.com/docs/customization/memories.md) [OFF].
- **Артефакты**: таблицы, деки, документы, веб-приложения — [TNW](https://thenextweb.com/news/openai-chatgpt-work-agent-launch) [3P]. **Sites** (public beta): хостинг сайтов, D1 до 10 ГБ на сайт, R2, разграничение доступа, метрирование с 1 января 2027 — [Docs](https://learn.chatgpt.com/docs/sites.md) [OFF].
- **Расписания**: RRULE; события Gmail, Slack, GitHub PR (только web/mobile); нельзя сочетать событие и время; поток внутри чата или новый чат на запуск; локальный запуск в проекте или worktree требует включённой машины; Team Tasks через сервисный аккаунт — [Docs](https://learn.chatgpt.com/docs/automations.md) [OFF].
- **Безопасность**: задача может приостанавливаться на safety review — [Docs](https://learn.chatgpt.com/docs/get-started-with-work.md) [OFF]. OpenAI заявляет, что слой авто-ревью заблокировал 100% тестовых попыток извлечения данных; это собственная, не независимая цифра — [TNW](https://thenextweb.com/news/openai-chatgpt-work-agent-launch) [3P].
- **Цены**: Work и Codex делят один метрированный пул — [felloai](https://felloai.com/chatgpt-work/) [3P]; тарифы см. 1.9.

#### 1.2. Claude Cowork (Anthropic)

- **Эволюция**: research preview 12 января 2026 в изолированной локальной VM на macOS; marketplace плагинов 24 февраля; scheduled tasks 25 февраля; постоянный поток управления задачами 17 марта (в записи от 23 марта упомянут Dispatch); computer use 23 марта; GA на macOS и Windows 9 апреля; web/mobile с удалённым запуском 7 июля; общая облачная память 25 августа — [Release notes](https://support.claude.com/en/articles/12138966-release-notes) [OFF].
- **Среды сейчас**: облако по умолчанию; локальные файлы, браузер и computer use идут через Claude Desktop, который должен быть открыт; встроенный браузер по умолчанию, Claude in Chrome как альтернатива — [Support](https://support.claude.com/en/articles/13345190-getting-started-with-cowork) [OFF].
- **Файлы**: доступ только к подключённым папкам, сеть по настройкам egress. Web fetch, web search, MCP и Claude in Chrome вне контроля egress — [Support](https://support.claude.com/en/articles/13345190-getting-started-with-cowork) [OFF].
- **Плагины**: связка skills, connectors и sub-agents, работает также в чате и Claude Code — [Support](https://support.claude.com/en/articles/13345190-getting-started-with-cowork) [OFF]. Сканирование skills и plugins на вредоносность (beta, Enterprise, 6 августа); Smart reports находят повторяющиеся процессы для упаковки в skills (10 сентября) — [Release notes](https://support.claude.com/en/articles/12138966-release-notes) [OFF].
- **Артефакты**: Excel с формулами, PowerPoint, документы, графики, правка Markdown-черновиков на месте; делиться можно артефактом, но не сессией — [Support](https://support.claude.com/en/articles/13345190-getting-started-with-cowork) [OFF].
- **Разрешения**: Manual, Auto (проверка каждого действия на эксфильтрацию и инъекции), Skip; безвозвратное удаление файлов требует явного Allow — [Support](https://support.claude.com/en/articles/13345190-getting-started-with-cowork) [OFF].
- **Память**: отдельные категоризированные записи вместо ежедневной сводки (10 июля); Topics с редактированием, чувствительные темы исключены по умолчанию (25 августа) — [Release notes](https://support.claude.com/en/articles/12138966-release-notes) [OFF].
- **Админ**: роли, ограничение отдельных инструментов коннекторов по ролям, OpenTelemetry — [Release notes](https://support.claude.com/en/articles/12138966-release-notes) [OFF].

#### 1.3. Manus

- **Статус**: Китай заблокировал сделку с Meta 27 апреля 2026 — [TechCrunch](https://techcrunch.com/2026/04/27/china-vetoes-metas-2b-manus-deal-after-months-long-probe/) [3P\*]. Manus подтвердил статус «independent again» 11 августа, данные части пользователей подлежат удалению с 23 августа; это один источник — [Startup Fortune](https://startupfortune.com/china-forces-meta-to-unwind-its-2-billion-dollar-acquisition-of-manus-ai/) [3P]. Переговоры с Tencent — только слухи там же.
- **Manus 2.0** (28 сентября): собственный harness **Cascade**, загружающий возможности по требованию; в одной конфигурации на 23,2% меньше токенов, на 28,2% быстрее, на 32% дешевле; Manus Studio (десктоп), Video Editor, Game Dev, Cloud Computer, Automations, Remote Control и Computer Use; **Cue** — агенты с собственными email, телефоном, кошельком и компьютером — [Manus](https://manus.im/blog/introducing-manus-2-0) [OFF].
- **Среда**: виртуальный компьютер с интернетом, постоянной ФС и установкой ПО — [Docs](https://manus.im/docs/introduction/welcome) [OFF]. **My Computer** работает с локальными файлами через терминальные команды, интегрирован с Projects и расписаниями — [Manus](https://manus.im/blog/manus-my-computer-desktop) [OFF\*].
- **Wide Research**: основной агент дробит запрос, по одному агенту со свежим контекстом на элемент, сотни агентов; проверено до 250 элементов, 50–100 элементов за несколько минут; плохо подходит для глубокого анализа одного объекта и задач с зависимостями — [Docs](https://manus.im/docs/features/wide-research.md) [OFF].
- **Skills**: файловый формат с `SKILL.md`, трёхуровневая загрузка (метаданные, инструкции, ресурсы); создание из успешной задачи; импорт с GitHub; ревью скилла самим агентом на рискованные операции — [Docs](https://manus.im/docs/features/skills.md) [OFF].
- **Расписания**: Scheduled Tasks 2.0 продолжают ту же задачу с контекстом или создают новую — [Manus](https://manus.im/blog/manus-schedules) [OFF\*]. Automations запускаются событиями из email, рекламы, календаря, Slack, Notion — [Manus](https://manus.im/blog/introducing-manus-2-0) [OFF].
- **Прочее из индекса документации**: Projects, Branch, Collab, Mail Manus, Slides, Design View, Data Visualization, Meeting minutes, Cloud browser, Browser Operator, публикация сайтов с БД, платежами и SEO, Shopify, импорт из Figma, MCP Connectors и Custom MCP, API, Zapier, Slack — [Индекс](https://manus.im/docs/llms.txt) [OFF].

#### 1.4. Genspark Super Agent

- **Workflow**: дробит запрос на шаги, выбирает инструменты, выдаёт результат; несколько задач параллельно в проекте с взаимным использованием результатов; New Super Agent имеет выделенный sandbox с браузером и ФС и «проверяет свой вывод»; Classic — без машины — [Help](https://www.genspark.ai/helpcenter/super-agent) [OFF].
- **Skills**: сохранение процесса по просьбе, повторный запуск, шаринг с командой — [Help](https://www.genspark.ai/helpcenter/super-agent) [OFF].
- **Claw** (12 марта 2026): AI-сотрудник с выделенной VM со своим IP, диском и доменом — [Обзор поиска, Business Wire](https://www.businesswire.com/news/home/20260408545044/en/Genspark-Launches-Genspark-Claw-for-Desktop-and-Microsoft-Office-Integration-with-AI-Workspace-4.0-Bringing-AI-to-Local-Files-and-Everyday-Work-Apps) [OFF\*].
- **Workspace 4.0** (апрель 2026): десктоп-клиент с Computer Use и Browser Use, плагины в PowerPoint, Excel, Word, Speakly (перевод и заметки встреч), Advanced Workflows на OpenCode — [Business Wire](https://www.businesswire.com/news/home/20260408545044/en/Genspark-Launches-Genspark-Claw-for-Desktop-and-Microsoft-Office-Integration-with-AI-Workspace-4.0-Bringing-AI-to-Local-Files-and-Everyday-Work-Apps) [OFF\*].
- **Артефакты**: слайды, таблицы, документы, исследовательские страницы, изображения, видео, телефонные звонки — [обзор](https://aitoolanalysis.com/genspark-ai-review/) [3P\*]. Файлы до 500 МБ (Free) и 1 ГБ (paid) — [Help](https://www.genspark.ai/helpcenter/super-agent) [OFF].
- **Mix of Agents (MoA)**: N версий за N-кратную стоимость — [Credits Guide](https://www.genspark.ai/helpcenter/credits-guide) [OFF].

#### 1.5. Gemini Deep Research и Gemini Enterprise

- **Deep Research / Max** (21 апреля 2026, Gemini 3.1 Pro): обычная версия для интерактивности, Max для фоновых задач с итеративным уточнением; MCP, произвольные инструменты, web search, загрузка файлов, URL Context, Code Execution, File Search; возможно отключить web и искать только по своим данным; нативные графики; ревью плана до запуска; входы PDF, CSV, изображения, аудио, видео; стрим размышлений — [Google](https://blog.google/innovation-and-ai/models-and-research/gemini-models/next-generation-gemini-deep-research/) [OFF].
- **Enterprise** (22 апреля): Agent Inbox с категориями «Needs your input», «Errors», «Completed»; long-running agents на часы и дни в облачных sandbox; Skills; Canvas для Docs и Slides с совместной правкой и экспортом в Office; Projects; BYO-MCP; Data Insights agent; Agent Gallery и Marketplace; Agent Identity, Registry, Gateway — [Google Cloud](https://cloud.google.com/blog/products/ai-machine-learning/whats-new-in-gemini-enterprise) [OFF].
- **Обновления**: Workflow Builder GA с расписаниями (3 сентября), A2A/A2UI GA (17 августа), Skills GA (13 августа), custom MCP GA (7 августа), AlphaEvolve GA (9 июля), квоты на пользователя (22 октября), Antigravity Enterprise Gateway (20 октября), федеративные запросы (2 октября) — [Release notes](https://docs.cloud.google.com/gemini/enterprise/docs/release-notes) [OFF]. Прозрачное мышление GA 27 июля — там же.
- **Deep Research API**: Preview 26 мая на Agent Platform, отложенный сервисный tier 2 сентября — [Release notes](https://docs.cloud.google.com/gemini-enterprise-agent-platform/release-notes) [OFF\*].

#### 1.6. Perplexity Comet и Computer

- **Comet**: бесплатен с 2 октября 2025; Max получил Background Assistants с управлением подтверждениями на одной странице — [Changelog](https://www.perplexity.ai/changelog/what-we-shipped-october-3rd) [OFF\*]. Comet Assistant в Work mode расходует кредиты Computer — [Help](https://www.perplexity.ai/help-center/en/articles/13837784-what-is-computer) [OFF]. Comet Enterprise: права по домену, журнал действий, MDM, блокировка расширений — [Perplexity](https://www.perplexity.ai/hub/blog/everything-is-computer) [OFF].
- **Computer**: изолированный облачный sandbox, сотни коннекторов, предметные сабагенты, артефакты (отчёты, деки, приложения, медиа), постоянная память; при нехватке кредитов задачи ставятся на паузу, а не отменяются; месячные лимиты расходов — [Help](https://www.perplexity.ai/help-center/en/articles/13837784-what-is-computer) [OFF].
- **Локальные варианты**: **Personal Computer** — выделенный Mac mini 24/7 с журналом и kill switch, чувствительные действия требуют одобрения — [Perplexity](https://www.perplexity.ai/hub/blog/everything-is-computer) [OFF]; Windows с 4 августа — [Changelog](https://www.perplexity.ai/changelog) [OFF]. **Portable Computer** на поддерживаемом железе NVIDIA запускает модель, оркестратор, планировщик и sandbox локально без кредитов, а web и frontier-модели идут в облако только с разрешения — [Help](https://www.perplexity.ai/help-center/en/articles/13837784-what-is-computer) [OFF]. Hybrid compute на Mac (21 сентября) — [Changelog](https://www.perplexity.ai/changelog) [OFF].
- **Хронология**: Brain, смена модели посреди задачи и публикация сайтов (13 июля); хранилище API-ключей (27 июля); Projects с общими файлами и Model Council (4 августа); запуск по email (24 августа); Effort Mode и Skills Marketplace (21 сентября); Automations (5 октября); ролевой доступ к коннекторам (19 октября) — [Changelog](https://www.perplexity.ai/changelog) [OFF].
- **Automations**: описание расписания на естественном языке, при повторных сбоях авторизации задача может приостановиться — [Help](https://www.perplexity.ai/help-center/en/articles/11521526-perplexity-tasks) [OFF\*].

#### 1.7. Claude Code (Anthropic)

- **Актуальная версия**: 2.1.318 от 4 ноября 2026 — [Changelog](https://code.claude.com/docs/en/changelog) [OFF].
- **Параллелизм**, пять способов: subagents, agent view (research preview), agent teams (experimental, выключены по умолчанию), Projects (public beta, облачные потоки), dynamic workflows; плюс worktrees, cross-session messaging, `/batch` на 5–30 субагентов в worktree — [Docs](https://code.claude.com/docs/en/agents.md) [OFF].
- **Dynamic workflows**: JavaScript-скрипт с `agent()`, `pipeline()`, `parallel()`, `phase()`; промежуточные результаты в переменных скрипта, а не в контексте; до 16 параллельных агентов по умолчанию (настраивается до 256), 1000 агентов на запуск; JSON-схема на выход с 5 попытками; возобновление с сохранёнными результатами; автоперезапуск зависшего агента до 5 раз; недетерминизм (`Date.now`, `Math.random`) запрещён ради replay; запрет прямого доступа скрипта к ФС; пауза при исчерпании лимита; встроенный `/deep-research` с перекрёстной проверкой и голосованием по утверждениям — [Docs](https://code.claude.com/docs/en/workflows.md) [OFF].
- **/goal**: условие завершения проверяет отдельная быстрая модель после каждого хода (вердикты не выполнено / выполнено / невозможно); оценщик не вызывает инструменты и судит только по показанному в диалоге; защита от хода без инструментов; цель сохраняется при resume — [Docs](https://code.claude.com/docs/en/goal.md) [OFF].
- **Режимы разрешений**: Manual, Accept edits, Plan, Auto, Don't ask, Bypass. Deny-правила действуют во всех режимах. Auto использует отдельный классификатор; широкие allow-правила снимаются при входе в auto; пауза после 3 блокировок подряд или 20 суммарно; границы, заданные в диалоге, могут потеряться при компакции; защищённые пути (`.git`, `.claude`, shell rc и др.) — [Docs](https://code.claude.com/docs/en/permission-modes) [OFF].
- **Hooks**: 33 события и 5 типов обработчиков (`command`, `http`, `mcp_tool`, `prompt`, `agent`); возможны блокировка, переписывание входа и вывода инструментов — [Docs](https://code.claude.com/docs/en/hooks) [OFF].
- **Sandbox**: Seatbelt, bubblewrap с socat; нативный Windows не поддерживается; сеть через прокси с allowlist; покрывает только shell-команды, а Read/Edit/Write, WebFetch, MCP и hooks вне его; `~/.ssh` читается по умолчанию; выключен по умолчанию — [Docs](https://code.claude.com/docs/en/sandboxing.md) [OFF]. Новая настройка `permissions.blockWritesOutsideWorkingDirectories` (2.1.314) — [Changelog](https://code.claude.com/docs/en/changelog) [OFF].
- **Расписания**: cloud scheduled tasks с триггерами schedule, API и GitHub; лимиты 100 запусков в час на аккаунт, 30 в час на задачу для ручных и API-запусков; задача принадлежит личному аккаунту и не шарится; возможен self-hosted — [Docs](https://code.claude.com/docs/en/routines) [OFF]. Также Desktop-задачи и `/loop` внутри сессии — [Docs](https://code.claude.com/docs/en/goal.md) [OFF].
- **Remote Control**: код выполняется локально, web и телефон работают как окно в сессию; синхронизация между устройствами; server mode со spawn и capacity; компьютер должен быть включён; несовместим с произвольным `ANTHROPIC_BASE_URL` — [Docs](https://code.claude.com/docs/en/remote-control.md) [OFF]. Cross-session messaging между локальными, удалёнными и облачными сессиями — [Docs](https://code.claude.com/docs/en/agents.md) [OFF].
- **Channels** (research preview): MCP-сервер пушит события в работающую сессию; Telegram, Discord, iMessage, вебхуки; allowlist отправителей через pairing; ретрансляция запросов разрешений — [Docs](https://code.claude.com/docs/en/channels.md) [OFF].
- **Память**: CLAUDE.md и AGENTS.md, path-scoped rules, auto memory в `MEMORY.md` (первые 200 строк или 25 КБ грузятся всегда) и тематические файлы по требованию; машинно-локальна — [Docs](https://code.claude.com/docs/en/memory.md) [OFF].
- **Artifacts**: живая страница на claude.ai с версиями, шарингом, правами viewer/editor, комментариями, на которые агент отвечает сам (лимит 60 в час); живые данные через MCP с учётной записью зрителя; строгий CSP; шаблоны Slides, Design, Docs — [Docs](https://code.claude.com/docs/en/artifacts.md) [OFF].
- **Computer use**: macOS в CLI, macOS и Windows в Desktop; одобрение на приложение; browsers и трейдинг только просмотр; одна сессия за раз; глобальный Esc; приоритет инструментов MCP > Bash > Chrome > computer use — [Docs](https://code.claude.com/docs/en/computer-use.md) [OFF].
- **Прочее по индексу документации**: checkpointing, advisor tool, plugin evals, mods, LLM gateways, self-hosted environments, Code Review, Claude Security, Claude Tag, deep links, voice dictation, iOS Simulator — [Index](https://code.claude.com/docs/llms.txt) [OFF].
- **Контроль контекста и затрат**: MCP-инструменты отложены по умолчанию (tool search); `/usage` с атрибуцией по skills, сабагентам и MCP и рекомендациями; `--max-budget-usd`; `/insights`; среднее $13 на разработчика в активный день; agent teams примерно в 7 раз дороже в plan mode — [Docs](https://code.claude.com/docs/en/costs.md) [OFF].

#### 1.8. Cursor Cloud Agents

- **Среда**: изолированная VM на агента, настройка агентом, снапшот или Dockerfile; Builds дают холодный старт в 3 раза быстрее; мульти-репо с PR в каждый репозиторий; секреты через панель; ограничение egress, Tailscale; перехват удалённого рабочего стола — [Docs](https://cursor.com/docs/cloud-agent) [OFF]; [Changelog](https://cursor.com/changelog/page/3) [OFF].
- **Верификация**: агенты запускают созданное ПО и прикладывают скриншоты, видео и логи к PR (24 февраля) — [Changelog](https://cursor.com/changelog/02-24-26) [OFF]. Заявленная доля PR от агентов: «более 30%» — [Форум](https://forum.cursor.com/t/cloud-agents-with-computer-use/152829) [OFF\*], 35% — [DevOps.com](https://devops.com/cursor-cloud-agents-get-their-own-computers-and-35-of-internal-prs-to-prove-it) [3P\*]; цифры самоотчётные.
- **Автономия, 19 августа**: Subscriptions (подписка на PR, Slack, расписания; агент чинит CI и отвечает на ревью), Custom Modes, сабагенты на своих VM, `/goal`, Steering — [Changelog](https://cursor.com/changelog/page/2) [OFF].
- **Projects** (beta, 10 сентября): координатор на месяцы, синхронизация контекстных файлов между машинами — [Changelog](https://cursor.com/changelog/page/2) [OFF].
- **Rollouts** (23 сентября): монитор на PR, план наблюдения в комментарии, статус healthy/regression/inconclusive по окружениям, сам ничего не откатывает. **Security Review** — на каждый PR — [Changelog](https://cursor.com/changelog/page/2) [OFF].
- **Self-hosted** (2 сентября): выполнение инструментов на своей инфраструктуре, пулы с гибернацией, sandbox-провайдеры AWS Lambda, Modal, Vercel, E2B, computer use на Linux и Mac — [Changelog](https://cursor.com/changelog/page/2) [OFF].
- **Agent SDK** (GA, 20 октября): агент как папка кода со `skills/` и `mcp-connections/`, евалы `agent-sdk eval`, replay по GitHub, триггеры PR, CI, Slack, webhook, cron — [Changelog](https://cursor.com/changelog) [OFF].
- **Прочее**: Canvases и Environments API (16 октября); Remote Control на iOS (6 октября); маркетплейс Origin (13 октября); Build Live Apps с Postgres, SSO и хостингом (5 ноября) — [Changelog](https://cursor.com/changelog) [OFF]. Хостинг кода Origin (17 августа) — [Changelog](https://cursor.com/changelog/page/3) [OFF].

#### 1.9. OpenAI Codex (cloud, CLI, app)

- **Версия**: CLI 0.167.0 от 6 ноября 2026 с agent command center (`codex agents`); CLI 0.166.0 добавил GPT-6.1 Terra и каталоги Vertex и Bedrock — [Changelog](https://learn.chatgpt.com/docs/changelog) [OFF].
- **Хронология 2026**: browser verification, computer use, thread automations, PR review в приложении (16 апреля); browser use и auto-review (23 апреля); `/goal` (30 апреля, CLI 0.128.0) — [Developers Digest](https://www.developersdigest.tech/blog/codex-changelog-april-2026) [3P]. Параллельные read-only инструменты, steering по follow-up (15 октября); Memory v2 по умолчанию с импортом памяти других агентов (27 октября) — [Changelog](https://learn.chatgpt.com/docs/changelog) [OFF].
- **Sandbox**: `read-only`, `workspace-write`, `danger-full-access`; Seatbelt, bwrap с seccomp, MXC на Windows; сеть выключена по умолчанию; `network_proxy` с allowlist; защищённые пути `.git`, `.agents`, `.codex`, `.aws` — [Docs](https://learn.chatgpt.com/docs/agent-approvals-security.md) [OFF].
- **Одобрения**: `on-request`, `never`, `granular`; auto_review блокирует при сбое или таймауте (fail-closed), критический риск отклоняется — [Docs](https://learn.chatgpt.com/docs/agent-approvals-security.md) [OFF].
- **Субагенты**: `default`, `worker`, `explorer`; кастомные в TOML; наследуют sandbox, и custom-файл не может его ослабить; просьба «не редактировать» не создаёт read-only sandbox — [Docs](https://learn.chatgpt.com/docs/agent-configuration/subagents.md) [OFF].
- **Память**: локальная, выключена по умолчанию; фоновая генерация по простаивающим чатам с вымарыванием секретов; отдельные модели извлечения и консолидации; отключение при внешнем контексте (`no_memories_if_mcp_or_web_search`); пропуск прохода при низком остатке лимита; Computer History на macOS — [Docs](https://learn.chatgpt.com/docs/customization/memories.md) [OFF].
- **Cloud**: у каждой задачи свой workspace, среда с настройкой интернета и сетевых секретов, повторное использование зависимостей — [Docs](https://learn.chatgpt.com/docs/cloud.md) [OFF].
- **Remote**: сопряжение телефона по QR, SSH-хосты, relay без публичного доступа, handoff чата и git-состояния между компьютером и хостом — [Docs](https://learn.chatgpt.com/docs/remote-connections.md) [OFF].
- **Цены (актуальны на дату среза)**: Free $0, Go $8, Plus $20, Pro $100/$200/$500, Business $20–25; оценки на 5 часов для Plus: GPT-6.1 Sol 15–160, GPT-6 Astra 5–45, GPT-6 Luna 350–3000; облачные задачи делят тот же лимит; у Pro нет пятичасового окна — [Docs](https://learn.chatgpt.com/docs/pricing.md) [OFF].

#### 1.10. Devin (Cognition)

- **Планы**: Free, Pro $20, Max $200, Teams $80 + $40 за место, Enterprise; страница не упоминает ACU, квоты обновляются ежедневно и еженедельно; параллельно до 10 на Free/Pro и без ограничений выше, но таблица разобрана неоднозначно — [Pricing](https://devin.ai/pricing) [OFF].
- **Параллельность**: дочерние сессии с JSON-результатом по схеме (3 апреля), дерево в сайдбаре (21 августа), старт с резюме родительского контекста (25 сентября) — [Release notes](https://docs.devin.ai/release-notes/overview) [OFF].
- **Память**: межсессионная, личная, с ночным **dreaming** для уточнения (9 октября); окно resume сокращено до 14 дней — [Release notes](https://docs.devin.ai/release-notes/overview) [OFF].
- **Skills и Plugins**: `/name` (1 мая), централизованное управление через плагины (17 июля), обязательные плагины от админа (21 сентября), обновление плагинов без перезапуска (30 октября) — [Release notes](https://docs.devin.ai/release-notes/overview) [OFF].
- **Automations**: триггеры по изменению файлов, PagerDuty, Linear, Slack, GitLab; очереди с лимитом параллелизма; preflight-скрипт может отменить запуск; агент-режим «Improve with Devin»; Terraform-ресурс — [Release notes](https://docs.devin.ai/release-notes/overview) [OFF].
- **Оперативный контур**: Oncall GA (19 октября) для Slack, Teams, PagerDuty; нативный Jira-агент; Code scans до 1000 репозиториев и экспорт SARIF 2.1.0; Devin Review с эффорт-уровнями и цепочками атак — [Release notes](https://docs.devin.ai/release-notes/overview) [OFF].
- **Вики**: Wiki v2 с сабагентами и показом стоимости генерации в ACU (8 апреля) — [Release notes](https://docs.devin.ai/release-notes/overview) [OFF]. DeepWiki для открытых репозиториев бесплатен, премиальные режимы платные — [tooljunction, aggregator](https://www.tooljunction.io/ai-tools/devin) [3P\*].
- **Среда и UI**: вкладки Computer, Desktop, Terminal, Activity; HTML-артефакты на компонент-ките, встроенные в сообщения (26 октября); Windows-сессии; голосовые звонки на live-модели (16 сентября); модель SWE-2 (research preview, 21 сентября) — [Release notes](https://docs.devin.ai/release-notes/overview) [OFF].
- **Контроль**: жёсткий лимит ACU на сессию (8 апреля), иерархия бюджетов (30 октября), security profiles по сети (7 августа), ограничение участников только админскими MCP (9 октября) — [Release notes](https://docs.devin.ai/release-notes/overview) [OFF].

#### 1.11. GitHub Copilot (cloud agent)

- **Workflow**: одна сессия на github.com, mobile, CLI, VS Code, Slack, Teams; агент исследует, планирует, реализует в фоне; изменения ревьюются до создания PR — [Docs](https://docs.github.com/en/copilot/concepts/agents/coding-agent/about-coding-agent) [OFF]; [Changelog](https://github.blog/changelog/2026-10-28-work-from-anywhere-with-a-new-connected-copilot-cloud-experience/) [OFF].
- **Среды**: Cloud sandbox (снапшот, resume, без часового лимита, мульти-репо в одной организации), GitHub Actions (лимит 59 минут, один репозиторий), Remote; регистрация локальной машины через Mission Control; `--environment-host` — [Docs](https://docs.github.com/en/copilot/concepts/agents/coding-agent/about-coding-agent) [OFF]; [Changelog](https://github.blog/changelog/2026-10-28-work-from-anywhere-with-a-new-connected-copilot-cloud-experience/) [OFF].
- **Расширения**: MCP, коннекторы, custom agents, skills, hooks, dotfiles; Copilot Memory; firewall-allowlist; Agent Merge чинит CI и конфликты, решение о слиянии за человеком — [Docs](https://docs.github.com/en/copilot/concepts/agents/coding-agent/about-coding-agent) [OFF]. Agentic Workflows на Markdown поверх Actions — там же.
- **Ограничения**: одна ветка и один PR на задачу, только GitHub — [Docs](https://docs.github.com/en/copilot/concepts/agents/coding-agent/about-coding-agent) [OFF].
- **Цены**: AI Credits с 1 июня 2026 вместо premium requests; базовые цены Pro $10, Pro+ $39, Business $19, Enterprise $39 сохранены — [GitHub Blog](https://github.blog/news-insights/company-news/github-copilot-is-moving-to-usage-based-billing/) [OFF\*]. Sandbox-вычисления тарифицируются с ноября 2026 отдельно от AI credits — [Changelog](https://github.blog/changelog/2026-10-28-work-from-anywhere-with-a-new-connected-copilot-cloud-experience/) [OFF].

#### 1.12. Replit Agent

- **Agent 4** (11 марта 2026): Design Canvas, параллельные агенты с мерджем через сабагентов разрешения конфликтов, задачи с одобрением перед слиянием, артефакты apps/слайды/видео в одном проекте; параллельность для Pro и Enterprise — [Replit](https://replit.com/blog/introducing-agent-4-built-for-creativity) [OFF].
- **Режимы**: Free Mode, Auto, Power, Max; фоновых задач 1 на Core и 10 на Pro; Plan mode с утверждением; чекпоинты с откатом — [Docs](https://docs.replit.com/replitai/agent) [OFF].
- **Верификация**: **App Testing** в реальном браузере с видеоповтором и переходами по секциям; для JS full stack и Streamlit — [Docs](https://docs.replit.com/features/agent/app-testing) [OFF\*].
- **Автономия**: Agent 3 работает сам до 200 минут — [Replit](https://replit.com/blog/introducing-agent-3-our-most-autonomous-agent-yet) [OFF\*].
- **Безопасность**: Security Agent строит threat model, Semgrep и HoundDog.ai — [Changelog](https://docs.replit.com/updates/2026/04/24/changelog) [OFF\*]; [Replit](https://replit.com/blog/meet-replit-security-agent) [OFF\*]. Разделение dev и prod баз после инцидента — [eWeek](https://www.eweek.com/news/replit-ai-coding-assistant-failure/) [3P\*].
- **Биллинг**: effort-based на чекпоинт; жёсткие бюджетные лимиты и оповещения; Plan mode на платном пути тоже может тарифицироваться — [Docs](https://docs.replit.com/billing/ai-billing) [OFF].
- **Cloud Agents для корпоративных кодовых баз** (30 октября): sandbox, тест в запущенном приложении, PR или MR; агент может отправлять сообщения в другие чаты — [Changelog](https://docs.replit.com/updates) [OFF].

#### 1.13. Сводка цен и лимитов

| Продукт | Точка входа | Верх | Единица | Источник |
|---|---|---|---|---|
| ChatGPT Work / Codex | Free, Go $8, Plus $20 | Pro $100/$200/$500 | Общий пул, 5-часовые и недельные окна, кредиты | [OFF](https://learn.chatgpt.com/docs/pricing.md) |
| Claude Code / Cowork | Pro $20 | Max $100, Ultra $200 | 5-часовые и недельные окна, usage credits | [OFF](https://claude.com/pricing) |
| Manus | Free (≈300 кредитов в день) | Standard $20–200 (40 000 кредитов) | Кредиты; официальных цифр прочитать не удалось | [3P](https://www.nocode.mba/articles/manus-ai-pricing) |
| Genspark | Free (100 в день) | Plus $24.99, Pro $249.99 | Кредиты плюс недельная квота | [OFF](https://www.genspark.ai/helpcenter/credits-guide), [3P](https://www.eesel.ai/blog/genspark-ai-review) |
| Gemini Enterprise | Business ≈$21 | Standard/Plus от $30 | Места и pay-as-you-go; цена Plus спорна | [3P\*](https://www.sentra.app/articles/gemini-enterprise) |
| Perplexity | Comet бесплатно, Pro $20 | Max $200 (10 000 кредитов) | Кредиты | [OFF](https://www.perplexity.ai/help-center/en/articles/13837784-what-is-computer) |
| Cursor | Plus $8 (без облачных агентов) | Premium $30, Super $100, Ultra $200 | Недельный пул, общий с Grok | [OFF](https://cursor.com/pricing) |
| Devin | Pro $20 | Max $200, Teams $80 + $40/место | Дневные и недельные квоты; ACU в Enterprise | [OFF](https://devin.ai/pricing) |
| Copilot | Pro $10 | Enterprise $39 | AI Credits плюс compute | [OFF\*](https://github.blog/news-insights/company-news/github-copilot-is-moving-to-usage-based-billing/) |
| Replit | Core $20 | Pro от $100 | Credits, effort-based | [3P\*](https://www.therundown.ai/tools/replit-agent-4) |

### Inferences

- Различие «ассистент для офиса» и «агент для кода» стирается: Codex стал движком Work, Cowork использует ту же модель плагинов, что и Claude Code, а Cursor и Replit выпускают хостинг приложений и деки. Для zero-agent это подтверждает выбранную концепцию универсального агента с общим ядром и разными наборами skills.
- Самые ценные для нас примеры из индустрии оказались в локальных вариантах: Portable Computer у Perplexity, Self-hosted machines у Cursor, Remote Control с исполнением на машине пользователя у Claude Code и Cursor, relay у Codex. Схема zero-agent-relay близка к ним, но без облачного посредника вендора.
- Смещение от Playbooks к Skills у Devin и появление `SKILL.md` у Manus показывают, что формат skills стал де-факто общим. Совместимость с ним выгоднее, чем собственный формат.
- Модель «скрипт держит план, контекст держит только итог» (dynamic workflows) особенно ценна для локальных моделей с малым контекстом. То же относится к свежему контексту на элемент в Wide Research.

### Gaps

- Первичный пост OpenAI о ChatGPT Work (HTTP 403); детали запуска взяты из вторичных источников и документации.
- Официальное решение о судьбе режима ChatGPT agent после запуска Work не найдено.
- Официальные цены Manus не прочитаны: страница /pricing не отдала контент, plans.md не содержит цифр. Статус владения Manus после 11 августа подтверждён одним источником, сделка с Tencent не подтверждена.
- Официальный пост Genspark Workspace 4.0 вернул 403, и данные об интеграциях и расписаниях неполны; расписания и память Claw не подтверждены официально.
- Память и computer use в Gemini Enterprise в доступной части release notes не упомянуты (страница обрезана после июня 2026).
- Лимиты параллельности облачных задач Codex и Cursor в документации не указаны.
- Что такое Copilot Fusion (GA 29 октября) и «dots cloud computers» в документации Codex, не выяснено.
- Поддержка локальных моделей в Codex CLI не проверялась; подтверждены только Bedrock и Vertex.

---

## 2. Что является table stakes, а что уникальными отличиями

### Takeaway

Table stakes на конец 2026: облачный или локальный компьютер агента, браузер, MCP, skills, расписания, сабагенты, память, план с утверждением, журнал действий и лимиты расходов. Реальные отличия теперь находятся в верификации с доказательствами, оркестрации большого масштаба, замкнутом операционном контуре после релиза и глубине контроля стоимости.

### Cited Findings

#### Table stakes (в основном подтверждены у 8+ продуктов из 12, план с утверждением у меньшего числа; подтверждение в разделе 1 и матрице раздела 4)

- Фоновые задачи, продолжающиеся без открытого клиента; изолированная среда выполнения.
- Коннекторы и MCP; пользовательские skills (формат `SKILL.md` явно у Manus — [Docs](https://manus.im/docs/features/skills.md) [OFF]).
- Периодические задачи, всё чаще с событийными триггерами.
- Сабагенты или параллельные сессии.
- Долговременная память.
- Работа с разных поверхностей (web, mobile, desktop, мессенджеры).
- Режим плана или одобрения перед действием.

#### Уникальные или редкие отличия

| Отличие | Продукт | Источник |
|---|---|---|
| Оркестрация как скрипт: replay, resume, 1000 агентов на запуск | Claude Code | [OFF](https://code.claude.com/docs/en/workflows.md) |
| Отдельная модель-оценщик завершения цели | Claude Code | [OFF](https://code.claude.com/docs/en/goal.md) |
| Wide Research: сотни агентов, по агенту на элемент | Manus | [OFF](https://manus.im/docs/features/wide-research.md) |
| Видеодоказательство работы в PR | Cursor | [OFF](https://cursor.com/changelog/02-24-26) |
| Видеоповтор сессии самотестирования с навигацией | Replit | [OFF\*](https://docs.replit.com/features/agent/app-testing) |
| Post-merge мониторинг Rollouts | Cursor | [OFF](https://cursor.com/changelog/page/2) |
| Oncall-агент в PagerDuty, Slack, Teams | Devin | [OFF](https://docs.devin.ai/release-notes/overview) |
| Ночной «dreaming» памяти | Devin | [OFF](https://docs.devin.ai/release-notes/overview) |
| Локальный полный стек без кредитов (Portable Computer) | Perplexity | [OFF](https://www.perplexity.ai/help-center/en/articles/13837784-what-is-computer) |
| Логин в облачном браузере в обход модели | ChatGPT Work | [OFF](https://learn.chatgpt.com/docs/browser.md) |
| Память по активности на компьютере (Computer History, macOS) | Codex | [OFF](https://learn.chatgpt.com/docs/customization/memories.md) |
| Агенты с собственными email, телефоном, кошельком | Manus Cue | [OFF](https://manus.im/blog/introducing-manus-2-0) |
| Телефонные звонки от агента | Genspark | [3P\*](https://aitoolanalysis.com/genspark-ai-review/) |
| Смешивание моделей, Model Council | Genspark, Perplexity | [OFF](https://www.genspark.ai/helpcenter/credits-guide), [OFF](https://www.perplexity.ai/changelog) |
| Глубина встраивания в Git-хостинг (Agent Merge, Agentic Workflows) | Copilot | [OFF](https://docs.github.com/en/copilot/concepts/agents/coding-agent/about-coding-agent) |
| Design Canvas с применением к production-коду | Replit | [OFF](https://replit.com/blog/introducing-agent-4-built-for-creativity) |
| Governance: identity, registry, gateway агентов, A2A | Gemini Enterprise | [OFF](https://cloud.google.com/blog/products/ai-machine-learning/whats-new-in-gemini-enterprise) |
| Автогенерируемая вики по кодовой базе | Devin | [OFF](https://docs.devin.ai/release-notes/overview) |
| Ответы агента на комментарии в артефактах | Claude Code | [OFF](https://code.claude.com/docs/en/artifacts.md) |
| SSH-хосты и handoff чата с git-состоянием между машинами | Codex | [OFF](https://learn.chatgpt.com/docs/remote-connections.md) |
| Eval-фреймворк для агента (`agent-sdk eval`, replay) | Cursor; `plugin evals` у Claude Code | [OFF](https://cursor.com/changelog), [OFF](https://code.claude.com/docs/llms.txt) |

### Inferences

- Функции, бывшие уникальными в 2025 (фоновые агенты, computer use), стали обязательными менее чем за год, поэтому ТЗ не должно строиться только вокруг них.
- Дифференциаторы, не привязанные к вендорскому облаку и потому доступные self-hosted проекту: оркестрация скриптом, независимый оценщик, видео и скриншоты как артефакт верификации, ночная консолидация памяти, замкнутый цикл после релиза.
- У zero-agent появляется собственное отличие: межагентная сеть через свой relay с доступом к проектам других узлов, без вендорского облака. Среди изученных продуктов такого не найдено: ближайшие аналоги (Codex relay, Remote Control, Mission Control) завязаны на аккаунт вендора.

### Gaps

- Подсчёт «8 из 12» опирается на матрицу с ячейками `?`: отсутствие подтверждения не равно отсутствию функции. Реальная распространённость может быть выше.
- Независимых сравнительных бенчмарков между этими 12 продуктами за 2026 год не найдено.

---

## 3. Главные жалобы и инциденты безопасности

### Takeaway

Доминирующая жалоба почти везде — непредсказуемая стоимость при переходе на метрированную оплату. Второй общий класс — промпт-инъекции, приводящие к эксфильтрации или RCE: они подтверждены почти у всех продуктов, а их источники повторяются (доверенные домены, конфиги репозитория, символьные ссылки, shell builtins, секреты в окружении).

### Cited Findings

#### 3.1. Жалобы по продуктам

**ChatGPT Work / Codex**
- Один разбор проекта на Astra съел пятичасовой лимит за 3 минуты — анекдот с Reddit в [Search Engine Watch](https://searchenginewatch.com/gpt-6-users-report-disappearing-credits-and-jobs-left-unfinished/) [3P\*]. Там же сообщение о кредитах, исчезающих в цикле переподключения; один заявитель.
- Независимый тест: Work на локальной папке Windows с тремя изображениями работал 30+ минут без результата; надёжен на шаблонных задачах (1–9 минут) — [smartailibrary](https://www.smartailibrary.com/blog/chatgpt-work-review) [3P\*].
- Заявленные ограничения: шаблонность без брифа, ненадёжная самопроверка цифр, быстрое истощение лимита на длинных задачах — [felloai](https://felloai.com/chatgpt-work/) [3P].

**Claude Cowork**
- Официально признано, что многошаговые задачи тратят лимит быстрее — [Support](https://support.claude.com/en/articles/13345190-getting-started-with-cowork) [OFF]. Подписчики Pro сообщают об исчерпании лимита за 10 минут; большие операции обрываются с частичным результатом — [betterclaw](https://www.betterclaw.io/blog/claude-cowork-rate-limit-reached) [3P\*].
- «Призрачный» rate limit у пользователя Max 4 дня подряд — один источник, [parix](https://parix.ai/blog/claude-cowork-not-working/) [3P\*]. Сообщений об удалении файлов Cowork не найдено.

**Manus**
- Кредиты трудно предсказать; сложная исследовательская задача стоит 500–900 кредитов — [обзор поиска](https://www.nocode.mba/articles/manus-ai-pricing) [3P\*]. На Standard — несколько сложных задач в месяц — [обзор поиска](https://www.usecarly.com/blog/manus-pricing/) [3P\*].
- Неопределённость собственности как риск для данных; слаб в глубокой работе с кодовой базой — [ToolRadar](https://www.gotoolradar.com/2026/09/manus-ai-review-2026-2-billion-ai-agent.html) [3P]; автор сам отмечает, что обзор не основан на тестировании.

**Genspark**
- Быстрое истощение кредитов; списание за неудачные попытки; нет цены до запуска; Trustpilot 1,5–1,7 из 5 с жалобами на биллинг и поддержку; один пользователь заявляет о 9000 кредитов за дефектные клипы — [eesel](https://www.eesel.ai/blog/genspark-ai-review) и другие обзоры [3P\*]. Самоотчётные данные.
- Подтверждено официально: повторы и ретраи стоят как оригинал, споры о качестве не возмещаются автоматически, в длинных диалогах контекст переобрабатывается целиком — [Credits Guide](https://www.genspark.ai/helpcenter/credits-guide) [OFF].

**Gemini Deep Research**
- Самый высокий уровень галлюцинированных ссылок среди моделей в исследовании: 13,3% при среднем для deep research агентов 10,7% и 4,8% для моделей с поиском — [arXiv 2604.03173](https://arxiv.org/html/2604.03173v1) [3P\*]. Версия агента в исследовании могла предшествовать Max.
- Более высокая полнота за счёт точности: выравнивание цитат 72,94% — [ReportBench](https://arxiv.org/pdf/2508.15804) [3P\*], работа 2025 года.

**Perplexity Comet / Computer**
- Падение Comet в App Store на фоне проблем безопасности и нестабильности — один менее авторитетный источник, [mwm.ai](https://mwm.ai/articles/app-trends/comet-ai-browser-viral-sensation-2026-03-23) [3P\*].
- Предметные жалобы на скорость и сбои агента не найдены.

**Claude Code**
- Postmortem от 23 апреля 2026: три изменения (падение эффорта по умолчанию, баг кэша, ограничение многословности) снизили качество в течение ~6 недель; сброс лимитов — [InfoQ](https://www.infoq.com/news/2026/05/anthropic-claude-code-postmortem/) [3P\*].
- Коллективный иск о лимитах с 4 марта по 6 мая 2026 — [OpenClassActions](https://openclassactions.com/lawsuits/consumer-protection/anthropic-claude-service-degradation-class-action-lawsuit.php) [3P\*]; текущий статус не проверен. Жалобы на внезапные лимиты в январе — [The Register](https://www.theregister.com/2026/01/05/claude_devs_usage_limits/) [3P\*].
- Документация признаёт источники скрытого расхода: длинный контекст, промахи кэша, циклы по расписанию, сообщения между сессиями, проверки цели — [Docs](https://code.claude.com/docs/en/costs.md) [OFF].

**Cursor**
- Форум содержит много сообщений о потере файлов из-за терминальных команд и checkpoint-восстановления, например `rmdir /s /q` вне защит; отдельные отчёты, системность не подтверждена — [Форум](https://forum.cursor.com/t/cursor-deleted-files-across-my-entire-system-critical-issue/158406) [3P\*].
- Требование задать spend limit до первого запуска — [Docs](https://cursor.com/docs/cloud-agent) [OFF]. Анекдоты: $135 за неделю и тестовый цикл на 3 часа — [DEV](https://dev.to/ai-agent-economy/set-a-spending-limit-before-your-cursor-agent-goes-rogue-3od6) [3P\*], не проверялись. Возвраты за surprise charges 2025 года — там же.

**Devin**
- Answer.AI: 3 успеха из 20 задач, январь 2025; устарело — [Answer.AI](https://www.answer.ai/posts/2025-01-08-devin) [3P\*].
- G2: замедление в длинных сессиях, непоследовательность; Trustpilot: биллинг и возвраты — [G2](https://www.g2.com/products/devin-ai/reviews_and_filters) [3P\*]. 67% merge rate не подтверждён.

**GitHub Copilot**
- Билинговый шок от AI Credits с 1 июня: 8% месячного лимита Pro+ за 2 часа — [The Register](https://www.theregister.com/ai-and-ml/2026/06/02/github-copilot-users-threaten-exit-as-metered-billing-kicks-in/5249826) [3P\*]. Жалобы продолжаются в августе; GitHub добавил скидки, бюджеты и счётчики, но модель не отменил — [Visual Studio Magazine](https://visualstudiomagazine.com/articles/2026/08/07/copilot-credit-complaints-keep-coming-too-expensive-to-use.aspx) [3P\*]. Обсуждение с просьбой предупреждать перед задачей — [GitHub Discussion #198015](https://github.com/orgs/community/discussions/198015) [3P\*].

**Replit**
- Непредсказуемая стоимость без лимита по умолчанию — самая частая жалоба; незапрошенные правки; потеря архитектурного контекста на больших проектах; нет auth и rate limiting по умолчанию — [обзоры](https://aibirdview.com/blog/replit-review) [3P\*].
- На платном пути Plan mode тоже тарифицируется — [Docs](https://docs.replit.com/billing/ai-billing) [OFF].

#### 3.2. Инциденты безопасности

| Продукт | Инцидент | Механизм | Статус | Источник |
|---|---|---|---|---|
| Claude Cowork | PromptArmor, январь 2026 | Скрытый текст в файле-«скилле», curl в разрешённый API Anthropic с ключом атакующего | По словам PromptArmor, техника известна с октября 2025; статус исправления не подтверждён | [PromptArmor](https://www.promptarmor.com/resources/claude-cowork-exfiltrates-files) [3P\*] |
| Claude Code | CVE-2026-25725, -39861, -40068, -55607, -24887 и др. | Создание `settings.json` с hooks из sandbox, symlink, поддельный git commondir, fsmonitor в worktree, разбор `find` | Исправлены в указанных версиях | [SentinelOne](https://www.sentinelone.com/vulnerability-database/cve-2026-25725/), [CVE-2026-55607](https://www.sentinelone.com/vulnerability-database/cve-2026-55607/) [3P\*] |
| Claude Code, Gemini CLI Action, Copilot | Comment and Control, апрель 2026 | Заголовки PR и тела issue; обход трёх защит Copilot через родительский процесс и base64 | Статус патча не подтверждён | [Techzine](https://www.techzine.eu/news/security/140524/ai-agents-on-github-leak-api-keys-via-prompt-injection/) [3P\*] |
| ChatGPT / Codex | BeyondTrust, март 2026 | Инъекция команды в имени ветки, кража GitHub-токена | Исправлено к 5 февраля 2026 | [SiliconANGLE](https://siliconangle.com/2026/03/30/openai-codex-vulnerability-enabled-github-token-theft-via-command-injection-report-finds/) [3P\*] |
| Codex CLI | CVE-2025-61260 | Выполнение кода через вредоносные MCP-конфиги | Версии до 0.23.0 включительно | [GitHub Advisory](https://github.com/advisories/GHSA-xrxf-jgv3-qmrm) [3P\*] |
| Cursor | DuneSlide CVE-2026-50548/50549, CVSS 9,8 | Инъекция через MCP/web меняет рабочий каталог sandbox; symlink, перезапись бинарника sandbox | Даты расходятся | [Cato](https://www.catonetworks.com/blog/duneslide-two-critical-rce-vulnerabilities/) [3P\*] |
| Cursor | CVE-2026-22708 | Shell builtins `export`, `typeset` выполнялись без одобрения | Исправлено в 2.3 | [SentinelOne](https://www.sentinelone.com/vulnerability-database/cve-2026-22708/) [3P\*] |
| Cursor | CVE-2026-26268 | Запись в `.git`, исполнение через hooks | Исправлено в 2.5 | [SentinelOne](https://www.sentinelone.com/vulnerability-database/cve-2026-26268/) [3P\*] |
| Comet | PleaseFix / PerplexedBrowser, раскрыто 3 марта 2026 | Приглашение в календарь запускает агента, чтение локальных файлов, скрытая эксфильтрация; взаимодействие с менеджером паролей | Исправлено, подтверждено 13 февраля | [Zenity](https://labs.zenity.io/p/perplexedbrowser-perplexity-s-agent-browser-can-leak-your-personal-pc-local-files) [3P\*] |
| Manus | SilentBridge (февраль), CodeAnt (исправлено 16 сентября), Salt Labs (сентябрь) | Скрытые инструкции в web, сохранённая инъекция в общих Projects, RCE через обфускацию JSFuck | Два исправлены через bug bounty, SilentBridge не подтверждён | [Aurascape](https://aurascape.ai/resources/auralabs-research/silentbridge-zero-click-agent-takeover-meta-manus/), [CodeAnt](https://codeant.ai/security-research/manus-prompt-injection-attack-desktop-takeover), [Dark Reading](https://www.darkreading.com/application-security/prompt-injection-bug-agentic-ai-app-manus) [3P\*] |
| Gemini Enterprise | GeminiJack, декабрь 2025 | Отравленный документ извлекается в RAG при обычном поиске сотрудника, нулевой клик | Вектор исправлен, класс атак остаётся | [Noma](https://noma.security/noma-labs/geminijack) [3P\*] |
| Copilot | RoguePilot, февраль 2026 | Скрытый HTML-комментарий в issue, утечка GITHUB_TOKEN из Codespaces | Исправлено | [THN](https://thehackernews.com/2026/02/roguepilot-flaw-in-github-codespaces.html) [3P\*] |
| Devin | Rehberger, август 2025 | Отравленный issue, агент отправляет переменные окружения на сервер атакующего | Статус на 2026 не найден | [vibegraveyard](https://vibegraveyard.ai/story/devin-ai-prompt-injection-no-protection/) [3P\*] |
| Replit | Июль 2025 | Агент удалил prod-базу во время заморозки кода, заявил о невозможности отката | Разделение баз, режим только планирования | [eWeek](https://www.eweek.com/news/replit-ai-coding-assistant-failure/) [3P\*] |
| Perplexity (юр.) | Amazon: запрет 9 марта отменён 9-м округом 4 августа; иск продолжается; утверждение о копировании cookies на iOS | Логин агента в защищённые аккаунты; маскировка бота под Chrome | Дело открыто | [PYMNTS](https://www.pymnts.com/news/artificial-intelligence/2026/ninth-circuit-narrows-cfaa-reach-in-perplexity-agentic-commerce-ruling/) [3P\*] |

### Inferences

Повторяющиеся причины и что из них следует для zero-agent:

1. **Allowlist доверенных доменов не защищает**: Cowork протёк через собственный API вендора. Egress нужно ограничивать также по содержимому и назначению учётных данных, а web fetch и MCP не должны быть вне контура.
2. **Конфиг репозитория как код**: hooks, MCP-конфиги, `.git`, fsmonitor. Конфигурация проекта должна считаться недоверенной до явного trust, а защищённые пути обязаны быть не изменяемы агентом ни в каком режиме.
3. **Символьные ссылки и смена cwd**: границы каталога проверять после разрешения пути, на уровне ОС. Это прямо относится к требованию работы строго внутри проекта. На Windows здесь критично отсутствие нативного sandbox у Claude Code: Rust-реализация должна закрыть этот пробел явно (MXC у Codex — ориентир).
4. **Shell нельзя разбирать регулярками**: builtins и разбор `find` обходили проверки. Нужно парсить AST и/или применять sandbox ОС.
5. **Секреты в окружении агента**: вводить брокер секретов, чтобы модель не видела значения (маскировка у Codex, логин в обход модели у ChatGPT).
6. **Внешний контент как инструкции**: разметка происхождения данных, отдельный режим при наличии недоверенного контента. Codex отключает память при внешнем контексте, Claude Code считает текст скрипта не запросом пользователя в auto mode.
7. **Память как вектор**: отравление через подобный канал вероятно, поэтому запись в память из недоверенных источников должна проверяться.
8. **Деструктивные действия**: rm, удаление БД. Подтверждение, чекпоинты и разделение dev и prod.
9. **Стоимость** для self-hosted заменяется на время GPU и электричество, но модель та же: предварительная оценка, бюджет на задачу, авто-пауза вместо сбоя, атрибуция расхода, обнаружение цикла.

### Gaps

- Инциденты по Genspark и подробные жалобы на Comet не найдены; это отсутствие результатов, а не подтверждение отсутствия проблем.
- Первичные тексты большинства CVE и отчётов не читались: сведения из результатов поиска, версии и CVSS нужно сверять при цитировании.
- Статус исправления Comment and Control, SilentBridge и находки Rehberger неизвестен.
- Прямых веток Reddit поиск почти не вернул, поэтому пользовательские анекдоты идут через вторичные источники.
- Цифра OpenAI о 100% блокировке и заявления Perplexity о трудозатратах не проверены независимо.

---

## 4. Сводная матрица capability × product и чеклист с приоритетами

### Takeaway

Всего выделено около 65 классов возможностей. Для локального self-hosted агента приоритет определяется не популярностью, а экономией контекста, безопасностью внутри каталога проекта и верифицируемостью: ядро P0 — цикл агента, sandbox, разрешения, skills, сабагенты со свежим контекстом, компакция, память, расписания и web UI.

### Cited Findings

Обозначения: ✓ подтверждено, ◐ частично или только сторонние источники, ✗ явное ограничение в источнике, `?` подтверждение не найдено (это НЕ значит «нет»). Источники по ячейкам — в разделах 1 и 2.

Колонки: **CGW** ChatGPT Work, **CWK** Cowork, **MAN** Manus, **GSP** Genspark, **GEM** Gemini, **CMT** Comet/Computer, **CC** Claude Code, **CUR** Cursor, **CDX** Codex, **DEV** Devin, **COP** Copilot, **RPL** Replit.

| Возможность | CGW | CWK | MAN | GSP | GEM | CMT | CC | CUR | CDX | DEV | COP | RPL |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **Автономия** | | | | | | | | | | | | |
| Фон без открытого клиента | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Цель с циклом до выполнения | ? | ? | ? | ? | ? | ? | ✓ | ✓ | ✓ | ? | ? | ? |
| Независимый оценщик завершения | ? | ? | ? | ? | ? | ? | ✓ | ? | ? | ? | ? | ? |
| План с утверждением | ◐ | ? | ? | ? | ✓ | ◐ | ✓ | ? | ? | ? | ✓ | ✓ |
| Steering посреди задачи | ✓ | ? | ? | ? | ? | ? | ✓ | ✓ | ✓ | ? | ? | ? |
| Сабагенты | ✓ | ✓ | ✓ | ◐ | ? | ✓ | ✓ | ✓ | ✓ | ✓ | ◐ | ✓ |
| Массовый fan-out (100+) | ? | ? | ✓ | ? | ? | ? | ✓ | ? | ? | ? | ? | ? |
| Оркестрация скриптом или no-code потоком | ? | ? | ? | ◐ | ✓ | ? | ✓ | ◐ | ? | ? | ◐ | ? |
| Координатор проекта на недели | ? | ? | ◐ | ? | ? | ◐ | ✓ | ✓ | ? | ◐ | ? | ? |
| Изоляция параллельных правок (worktree, VM) | ◐ | ? | ? | ? | ? | ? | ✓ | ✓ | ✓ | ✓ | ◐ | ✓ |
| Авто-пауза и возобновление при лимите | ? | ? | ? | ? | ? | ✓ | ✓ | ? | ? | ? | ? | ? |
| **Среда** | | | | | | | | | | | | |
| Выделенный компьютер агента (cloud) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Self-hosted исполнение | ? | ? | ? | ? | ? | ✓ | ✓ | ✓ | ◐ | ✓ | ✓ | ? |
| Локальный компьютер пользователя | ✓ | ✓ | ✓ | ✓ | ? | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ? |
| Браузер агента | ✓ | ✓ | ✓ | ✓ | ? | ✓ | ✓ | ✓ | ✓ | ◐ | ? | ✓ |
| Computer use GUI | ✓ | ✓ | ✓ | ✓ | ? | ? | ✓ | ✓ | ✓ | ✓ | ? | ? |
| Локальный OS-sandbox | ? | ? | ? | ? | n/a | ? | ✓ | ✓ | ✓ | ? | ? | n/a |
| Управление egress | ? | ✓ | ? | ? | ✓ | ? | ✓ | ✓ | ✓ | ✓ | ✓ | ? |
| Брокер секретов | ✓ | ? | ? | ? | ? | ✓ | ◐ | ✓ | ✓ | ✓ | ? | ? |
| **Интеграции** | | | | | | | | | | | | |
| MCP | ✓ | ✓ | ✓ | ? | ✓ | ? | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Каталог коннекторов SaaS | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Skills | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ? |
| Плагины и маркетплейс | ✓ | ✓ | ? | ? | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ? | ? |
| Hooks | ? | ? | ? | ? | ? | ? | ✓ | ✓ | ✓ | ✓ | ✓ | ? |
| Мессенджеры, email как интерфейс | ◐ | ? | ✓ | ? | ? | ✓ | ✓ | ✓ | ? | ✓ | ✓ | ? |
| Публичный API/SDK | ? | ? | ✓ | ? | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ? | ? |
| Межагентный протокол (A2A) или сеть сессий | ? | ? | ? | ? | ✓ | ? | ✓ | ? | ? | ? | ? | ? |
| Выбор и смена модели, BYO-провайдер | ◐ | ? | ? | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ◐ |
| **Память** | | | | | | | | | | | | |
| Файлы инструкций проекта | ? | ◐ | ? | ? | ? | ? | ✓ | ? | ✓ | ✓ | ✓ | ? |
| Межсессионная память | ✓ | ✓ | ? | ◐ | ? | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ? |
| Фоновая консолидация | ? | ? | ? | ? | ? | ? | ? | ? | ✓ | ✓ | ? | ? |
| Редактируемая и удаляемая память | ✓ | ✓ | ? | ? | ? | ? | ✓ | ? | ◐ | ? | ? | ? |
| Индекс кодовой базы, вики | ? | ? | ? | ? | ? | ? | ◐ | ? | ? | ✓ | ? | ? |
| **Расписания** | | | | | | | | | | | | |
| Cron или расписание | ✓ | ✓ | ✓ | ? | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ? |
| Событийные триггеры | ✓ | ? | ✓ | ? | ✓ | ? | ✓ | ✓ | ✓ | ✓ | ✓ | ? |
| Webhook/API запуск | ? | ? | ◐ | ? | ? | ? | ✓ | ✓ | ? | ✓ | ? | ? |
| Продолжение в том же контексте | ✓ | ? | ✓ | ? | ? | ? | ✓ | ? | ✓ | ? | ? | ? |
| **Артефакты** | | | | | | | | | | | | |
| Слайды | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ? | ? | ? | ? | ✓ |
| Таблицы с формулами | ✓ | ✓ | ◐ | ✓ | ? | ? | ? | ? | ? | ? | ? | ◐ |
| Документы | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ? | ? | ? | ? | ◐ |
| Интерактивная страница | ✓ | ✓ | ✓ | ? | ? | ✓ | ✓ | ✓ | ✓ | ✓ | ? | ✓ |
| Хостинг приложения с БД | ✓ | ? | ✓ | ? | ? | ◐ | ✗ | ✓ | ✓ | ? | ? | ✓ |
| Видео, медиа | ? | ? | ✓ | ✓ | ? | ◐ | ? | ? | ? | ? | ? | ✓ |
| Графики в отчёте | ✓ | ✓ | ✓ | ? | ✓ | ✓ | ✓ | ◐ | ? | ? | ? | ✓ |
| Голос или звонки | ? | ? | ✓ | ✓ | ? | ✓ | ◐ | ? | ✓ | ✓ | ? | ? |
| **Верификация** | | | | | | | | | | | | |
| Запуск тестов | ◐ | ? | ? | ◐ | ? | ? | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Самотест в браузере | ? | ? | ◐ | ? | ? | ? | ✓ | ✓ | ✓ | ? | ? | ✓ |
| Видео как доказательство | ? | ? | ? | ? | ? | ? | ? | ✓ | ? | ? | ? | ✓ |
| Скриншоты, логи как доказательство | ? | ? | ? | ? | ? | ? | ✓ | ✓ | ✓ | ? | ? | ✓ |
| Агент-ревьюер кода и безопасности | ? | ? | ? | ? | ? | ? | ✓ | ✓ | ✓ | ✓ | ◐ | ✓ |
| Перекрёстная проверка утверждений в отчёте | ? | ? | ? | ? | ◐ | ◐ | ✓ | ? | ? | ? | ? | ? |
| Мониторинг после релиза | ? | ? | ? | ? | ? | ? | ? | ✓ | ? | ✓ | ? | ? |
| **Контроль** | | | | | | | | | | | | |
| Многоуровневые режимы разрешений | ✓ | ✓ | ? | ? | ? | ◐ | ✓ | ? | ✓ | ? | ✓ | ? |
| Модель-классификатор действий | ✓ | ✓ | ? | ? | ? | ? | ✓ | ? | ✓ | ? | ? | ? |
| Разрешения на приложение или сайт | ✓ | ? | ? | ? | ? | ✓ | ✓ | ◐ | ✓ | ? | ? | ? |
| Чекпоинты и откат | ? | ? | ? | ? | ? | ? | ✓ | ◐ | ? | ? | ? | ✓ |
| Лимиты расходов | ✓ | ? | ? | ? | ✓ | ✓ | ✓ | ✓ | ? | ✓ | ✓ | ✓ |
| Журнал действий, телеметрия | ? | ✓ | ? | ? | ✓ | ✓ | ✓ | ✓ | ? | ✓ | ? | ◐ |
| **Совместная работа и доступ** | | | | | | | | | | | | |
| Шаринг артефакта | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ? | ✓ | ? | ? |
| Совместная работа в проекте | ? | ✗ | ✓ | ? | ✓ | ✓ | ◐ | ◐ | ? | ✓ | ? | ✓ |
| Управление с телефона | ✓ | ✓ | ✓ | ? | ? | ✓ | ✓ | ✓ | ✓ | ? | ✓ | ? |
| Удалённый доступ к другой машине | ✓ | ✓ | ✓ | ? | ? | ✓ | ✓ | ✓ | ✓ | ? | ✓ | ? |
| Управление многими агентами в одном экране | ? | ? | ? | ? | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |

### Inferences

Матрица построена на доступных источниках, поэтому `?` чаще отражает пределы поиска, чем реальное отсутствие. Приоритеты ниже — рекомендация автора заметок для self-hosted, local-model-first агента. Критерии: контекстная экономичность, соответствие требованиям из `ZeroAgentTask.txt` (работа только в каталоге проекта, отдельные окружения Python, периодические задачи, web UI, relay, Rust и SQLite, память ZeroAIMemory), риски из раздела 3.

**P0 — без этого агент не работоспособен**

| # | Возможность | Образец | Примечание для local-first |
|---|---|---|---|
| 1 | Цикл агента с файлами, shell, поиском, web fetch | все | Минимальный системный промпт |
| 2 | Жёсткая граница каталога проекта на уровне ОС, проверка разрешённых путей | CC, CDX | Закрыть symlink и смену cwd, нативно под Windows |
| 3 | Режимы разрешений read-only / ask / accept-edits / plan / auto / bypass, deny всегда приоритетен | CC, CDX | |
| 4 | Защищённые пути, недоверенная конфигурация до явного trust | CC, CDX | Урок CVE |
| 5 | Компакция и управление контекстом, счётчик токенов | CC | Критично для малого контекста |
| 6 | Отложенная загрузка инструментов (tool search) | CC | Схемы инструментов съедают контекст |
| 7 | Skills с прогрессивной загрузкой, совместимость с `SKILL.md` | MAN, CC | |
| 8 | Сабагенты со свежим контекстом и возвратом резюме | CC, CDX, MAN | Наследование sandbox без возможности ослабить |
| 9 | Инструкции проекта (`AGENTS.md`) плюс память ZeroAIMemory | CC, CDX | Индекс в контексте, темы по запросу |
| 10 | Локальные модели как основной провайдер, BYO endpoint, роутинг модели по задаче | CMT Portable, CDX | Дешёвая модель для служебных задач |
| 11 | Отдельные Python-окружения для инструментов | ТЗ | |
| 12 | Периодические задачи: cron/RRULE, запись истории в SQLite | CGW, CC | Требование ТЗ |
| 13 | Web UI: чат, поток действий, просмотр файлов и diff, одобрения, отмена | все | Требование ТЗ |
| 14 | Персистентные сессии и resume | CC, CDX | SQLite |
| 15 | Web search и fetch с разметкой внешнего контента как данных | CC | |
| 16 | Журнал всех действий с экспортом | CMT, DEV | |
| 17 | Чекпоинты и откат файлов | CC, RPL | Урок удаления данных |
| 18 | MCP-клиент | все | stdio и HTTP, OAuth |

**P1 — заметное преимущество, ближайшие релизы**

| # | Возможность | Образец | Примечание |
|---|---|---|---|
| 19 | Plan mode с утверждаемым планом и файлом плана | CC, RPL | |
| 20 | Goal-цикл с независимым оценщиком | CC, CDX, CUR | Оценщик — лёгкая локальная модель |
| 21 | Скриптовая оркестрация: resume, replay, схема ответа, бюджет агентов | CC | На стороне Rust, контекст держит только итог |
| 22 | Параллельные сессии с изоляцией через git worktree | CC, CDX | Лимит агентов под GPU, очередь запросов |
| 23 | Steering и очередь сообщений | CUR, CDX | |
| 24 | Глубокий ресёрч: fan-out, чтение, проверка утверждений, отчёт со ссылками | CC, GEM | Галлюцинация ссылок — проверять URL перед выдачей |
| 25 | Браузерный инструмент: Playwright или CDP, скриншоты, логи консоли | CDX, CUR | Отдельный профиль |
| 26 | Самотестирование: запуск тестов, браузерная проверка, артефакт-доказательство | CUR, RPL | |
| 27 | Hooks: PreToolUse, PostToolUse, Stop, SessionStart и другие | CC, CDX | Основа детерминированной политики |
| 28 | zero-agent-relay: сеть узлов, токены, доступ к проектам, handoff | CDX, CC | Отличительная функция |
| 29 | Брокер секретов и маскировка | CDX, CUR | Модель не видит значений |
| 30 | Контроль egress с прокси и allowlist | CC, CDX | Включая web fetch и MCP |
| 31 | Событийные триггеры: webhook, file watcher, git | CUR, DEV | |
| 32 | Бюджеты на задачу, детектор циклов, авто-пауза | CMT, DEV | Токены и время GPU вместо долларов |
| 33 | Атрибуция расхода по skill, сабагенту, MCP с рекомендациями | CC | |
| 34 | Inbox агентов со статусами Needs input / Errors / Completed | GEM, DEV | |
| 35 | Память: фоновая консолидация и защита от отравления | DEV, CDX | Отключать при недоверенном контенте |
| 36 | Интерактивные артефакты в UI с версиями | CC, CGW | Локальный рендер, строгий CSP |
| 37 | Документы, таблицы, слайды как skills | CGW, CWK, MAN | Экспорт в docx, xlsx, pptx |
| 38 | Метрики для бенчмарк-арены, replay, eval-набор | CUR | Требование ТЗ |
| 39 | Параллельный запуск только для чтения | CDX | Экономит время |
| 40 | Обратная связь в телефоне через PWA | CC, CDX | Web UI адаптивный |

**P2 — расширение возможностей**

| # | Возможность | Образец | Примечание |
|---|---|---|---|
| 41 | Computer use GUI с одобрением на приложение | CGW, CC | Зависит от vision-модели |
| 42 | Plugins и маркетплейс, сканирование на вредоносность | CWK, CC, DEV | |
| 43 | Мессенджеры как канал: Telegram, Slack, email | CC Channels | Allowlist через pairing |
| 44 | Массовый fan-out на элементы списка | MAN | Ограничен пропускной способностью GPU |
| 45 | Координатор долгих проектов | CC, CUR | |
| 46 | Классификатор действий для auto-режима | CC, CDX | Fail-closed, вторая модель |
| 47 | Code review и security review агенты | CUR, DEV, CC | Skills поверх ядра |
| 48 | Вики и индекс кодовой базы | DEV | Связка с памятью |
| 49 | Интеграция с Git-хостингом: PR, CI-починка | CUR, COP | |
| 50 | LSP и code intelligence | CC | Снижает чтение файлов |
| 51 | Self-hosted воркеры для тяжёлых задач на других узлах | CUR, CC | Через relay |
| 52 | Голосовой ввод | DEV, CDX | Локальный STT |
| 53 | Импорт памяти других агентов | CDX | |
| 54 | OpenTelemetry-экспорт | CWK, CC | |
| 55 | Запрет на действия в режиме недоверенного контента | CC | Taint tracking |
| 56 | Хостинг приложений через Coolify | CUR, RPL | Упомянут в ТЗ |

**P3 — опционально, низкая ценность или высокий риск для self-hosted**

| # | Возможность | Образец | Комментарий |
|---|---|---|---|
| 57 | Генерация видео | MAN, GSP | Отдельные локальные модели |
| 58 | Телефонные звонки, кошелёк, личный номер | GSP, MAN | Юридические риски |
| 59 | Память активности экрана | CDX | Высокий риск приватности |
| 60 | Мониторинг после релиза, Oncall | CUR, DEV | Домен команд |
| 61 | Design Canvas | RPL | Сложный UI |
| 62 | Model Council, Mix of Agents | CMT, GSP | Дорого локально; возможен как режим проверки |
| 63 | Governance: identity, registry | GEM | Не нужен для одного владельца, частично покрыт relay-токенами |
| 64 | Интеграция в Office | GSP, CWK | Заменяется экспортом файлов |
| 65 | Mod-система для UI | CC | После стабилизации UI |

Дополнительно для ТЗ стоит зафиксировать анти-требования, следующие из жалоб: не списывать ресурс на бесконечные ретраи; показывать оценку до запуска; не делать один общий пул без атрибуции; не доверять self-reported завершению без проверки; не заявлять «unlimited», если есть лимиты.

### Gaps

- Приоритеты не проверены на реальной пропускной способности локальных моделей: границы P1 и P2 для fan-out и классификатора зависят от выбранного железа.
- Для части ячеек матрицы (особенно Genspark, Gemini, Replit) требуется ручная проверка по продукту.
- Совместимость форматов skills между вендорами в деталях не сравнивалась.
