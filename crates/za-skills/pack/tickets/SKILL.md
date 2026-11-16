---
name: tickets
description: Use when an accepted spec or plan must be split into small tasks that can each be built, tested and reviewed on their own.
---
# Tickets

Turn agreed work into an ordered list of small tasks, each ending in a result that someone can check.

## Steps
1. Number the acceptance criteria of the spec, so that tickets can refer to them.
2. Cut by behavior, not by component: after each ticket, some command, request or screen does something new that can be seen. Work that only prepares for a later ticket belongs to the ticket that first uses it.
3. Make the first ticket the thinnest version that runs at all. Each later ticket widens it.
4. Write each ticket as one sentence: after it, who can do what, and which test or command shows it. Add the numbers of the criteria it serves and of the tickets it needs first.
5. A ticket is small enough when its sentence names one behavior and its check is one command. Otherwise split it by behavior. Two tickets that cannot be checked apart become one.
6. Place each ticket after the tickets it needs. Among the rest, start with the one whose outcome could change the plan, while a wrong assumption is still cheap.
7. Cross-check both ways: every criterion appears in some ticket, and no ticket adds behavior the spec does not ask for.
8. Save the list where the project tracks work (a plan file, issues), or with a plan tool if one is available.

## Stop when
Every criterion has a ticket, every ticket has its own check, and the order is fixed.

## Do not
- Create tickets with nothing to check, such as set up X or refactor Y, unless a later ticket depends on them and says why.
- Bury open questions inside tickets. List them apart, with who can answer them.
