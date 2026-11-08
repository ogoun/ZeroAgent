# ТЗ zero-agent

Техническое задание на разработку универсального самостоятельно размещаемого агента `zero-agent` и сервиса `zero-agent-relay`.

**Статус:** версия 0.3 от 2026-11-06, готово к составлению плана реализации.

| Часть | Файл |
|---|---|
| 00. Обзор, цели, решения, каталог функций | [00-overview.md](00-overview.md) |
| 01. Архитектура, данные, протоколы, эксплуатация | [01-architecture.md](01-architecture.md) |
| 02. Ядро: цикл, контекст, модели, промпты, инструменты, скиллы, MCP, качество | [02-agent-core.md](02-agent-core.md) |
| 03. Иерархия, рой, шина, управление | [03-swarm.md](03-swarm.md) |
| 04. Контракт с ZeroAIMemory | [04-memory-contract.md](04-memory-contract.md) |
| 05. Безопасность и модель угроз | [05-security.md](05-security.md) |
| 06. zero-agent-relay | [06-relay.md](06-relay.md) |
| 07. Web UI | [07-web-ui.md](07-web-ui.md) |
| 08. Прикладные возможности, задания, реестр утилит | [08-capabilities.md](08-capabilities.md) |
| 09. Арена бенчмарков | [09-arena.md](09-arena.md) |
| 10. Этапы, приёмка, риски, трассировка | [10-roadmap.md](10-roadmap.md) |
| A. Черновики системных промптов | [appendix-a-prompts.md](appendix-a-prompts.md) |

## Основания

- Исходное задание: [`ZeroAgentTask.txt`](../../../ZeroAgentTask.txt).
- Заметки заказчика N-01…N-22: [`00-customer-notes.md`](../../research/2026-11-06-landscape/00-customer-notes.md).
- Исследование R01…R10: каталог [`docs/research/2026-11-06-landscape/`](../../research/2026-11-06-landscape/).

| Метка | Заметка |
|---|---|
| R01 | [Контракт ZeroAIMemory по коду](../../research/2026-11-06-landscape/01-zeroaimemory-contract.md) |
| R02 | [Открытые харнессы](../../research/2026-11-06-landscape/02-open-source-harnesses.md) |
| R03 | [Фреймворки](../../research/2026-11-06-landscape/03-frameworks.md) |
| R04 | [Скиллы и домены](../../research/2026-11-06-landscape/04-skills-and-domains.md) |
| R05 | [Закрытые продукты](../../research/2026-11-06-landscape/05-closed-products.md) |
| R06 | [Бенчмарки и арена](../../research/2026-11-06-landscape/06-benchmarks-arena.md) |
| R07 | [Сообщество и состояние отрасли](../../research/2026-11-06-landscape/07-community-and-sota.md) |
| R08 | [Промпты и локальные модели](../../research/2026-11-06-landscape/08-prompt-and-local-models.md) |
| R09 | [Rust, песочница, relay, UI](../../research/2026-11-06-landscape/09-rust-sandbox-relay-ui.md) |
| R10 | [Замысел ZeroAIMemory по документации](../../research/2026-11-06-landscape/10-zeroaimemory-docs.md) |
