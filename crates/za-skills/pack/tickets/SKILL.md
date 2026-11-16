---
name: tickets
description: Use when an agreed spec has to be split into tickets that separate runs can take in turn, each closed by its own Check.
---
# Tickets

Turn an agreed spec into tickets. A ticket is the task one run receives, and its Check is how that run, and every run after it, can tell the ticket is closed.

## Steps
1. Number the acceptance criteria of the spec. If one cannot be checked, or the spec is not agreed yet, settle that with the user before splitting.
2. Find the code each criterion touches: names, files, tests, the build and test commands (the orient skill helps). Read AGENTS.md for project rules.
3. Draft tickets around results, not activities. Write each one as:
   - Needs: the tickets whose Check must pass before this one starts.
   - Files: the paths to read first and the ones expected to change.
   - Result: what will be observably different when it is done.
   - Check: the command or manual step, and the output or behavior that shows the Result.
   - Covers: the criteria it serves.
4. Compare the Checks. Two tickets with the same Check are one ticket.
5. Size each ticket against the context window of the run that takes it: its Files, the output of its Check and the talk around them must fit with room to spare. If reading the Files alone would take about half of the window, split the ticket by its Result.
6. Keep finished Checks passing. A ticket that changes an interface also updates its callers, or keeps the old form until a later ticket removes it.
7. Order by Needs only. When several tickets are free to start, put first the one that the most other tickets need.
8. Check coverage: every criterion appears under some ticket, and every ticket serves a criterion or is named as groundwork for one.
9. Save the list where the project keeps plans: the place AGENTS.md names, or docs/plans/ if it exists. If there is no such place, ask the user where to put it. Show the list to the user.

## Stop when
Every criterion is covered, and a fresh run given only the saved list and AGENTS.md could take the first free ticket, do it and run its Check.

## Do not
- Write a ticket that only names an activity, such as "investigate X" or "clean up Y". Say what it produces.
- Start implementing while splitting.
- Add tickets for work the spec does not ask for.
