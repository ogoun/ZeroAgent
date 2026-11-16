---
name: tickets
description: Use when an agreed spec or plan has to become an ordered list of tasks that can be picked up and finished one at a time.
---
# Tickets

Turn an agreed spec into tickets. Each ticket fits in one run, can be checked by someone who did not write it, and leaves the project working.

## Steps
1. Number the acceptance criteria of the spec. If one cannot be checked, or the spec is not agreed yet, settle that with the user before splitting.
2. Find the code each criterion touches: names, files, tests, the build and test commands (the orient skill helps). Read AGENTS.md for project rules.
3. Draft tickets around results, not activities. Write each one as:
   - Result: what will be observably different when it is done.
   - Check: the command or manual action that shows it, and the output or behavior to expect.
   - Files: the paths or areas expected to change, and the ones to read first.
   - Needs: the tickets that must be done before it.
   - Covers: the criteria it serves.
4. Size each ticket against one run. The worker has one context window: the files to read and change, the test output and the conversation must fit in it with room to spare. If the reading alone would take more than about half of the window, or the change crosses more than one module boundary, split it. If two tickets cannot be checked apart, merge them.
5. Cut where the project still builds and its tests pass. A ticket that changes an interface also updates its callers, or keeps the old form until a later ticket removes it.
6. Order by Needs. Among tickets that are free to start, put first the one whose outcome is least certain, so a surprise shows up before other work rests on it.
7. Check coverage: every criterion appears under some ticket, and every ticket serves a criterion or is named as groundwork for one.
8. Save the list where the project keeps plans: the place AGENTS.md names, or docs/plans/ if it exists. If there is no such place, ask the user where to put it. Show the list to the user.

## Stop when
Every criterion is covered, every ticket has a result, a check, files and needs, and the list is saved and shown to the user.

## Do not
- Write a ticket that only names an activity, such as "investigate X" or "clean up Y". Say what it produces.
- Start implementing while splitting.
- Add tickets for work the spec does not ask for.
