---
name: tickets
description: Use when an accepted spec or plan must be split into small tasks that can each be built, tested and reviewed on their own.
---
# Tickets

Split agreed work into thin tasks, each delivering a piece of behavior that can be tested.

## Steps
1. Start from the acceptance criteria. Each ticket covers one or a few of them end to end: interface, logic, storage and tests together.
2. Prefer thin slices through all layers to one ticket per layer. Make the first ticket the simplest path that works end to end.
3. For each ticket write a title, the behavior it adds, its checks (tests or commands), the files or modules it will likely touch, and the tickets it depends on.
4. Size: one ticket fits one focused session. Split anything larger; merge tickets that cannot be tested apart.
5. Order by dependency and risk: put unknowns and risky parts early.
6. Check coverage: every acceptance criterion maps to a ticket, and no ticket adds scope nobody asked for.
7. Record the list where the project tracks work (a plan file, issues) or with plan_update.

## Stop when
Every criterion is covered, each ticket has its own checks, and the order is clear.

## Do not
- Write tickets such as set up X or refactor Y with no visible behavior, unless a later ticket needs them and says why.
- Hide open questions inside tickets. List them separately.
