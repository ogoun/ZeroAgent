---
name: review
description: Use when asked to review a diff, branch or pull request, or before you merge your own changes.
---
# Review

Search a change for situations in which it does the wrong thing, and describe each one so that its author can reproduce it.

## Map the change
1. Find what the change is for: the task, issue or plan, the commit messages. If nothing states it, write down the purpose you infer and say that you inferred it.
2. Read the whole diff once, then the code around each changed part. List what the change touches:
   - results: functions whose output or errors differ, and the code that calls them;
   - effects: files written or removed, processes started, network calls, stored data and its format;
   - contracts: public interfaces, config keys, file formats, prompts, versions, golden files;
   - outside data: the places where user input, files, web pages or tool output come in.

## Probe each item
- Correctness: empty, huge, repeated or malformed input; paths and text on another OS; a failure halfway through; two callers at once; data stored by an older version.
- Security: can outside data reach a path, a shell, a query or a prompt without a check? Can the change act outside the project, put a secret into a log, or skip a permission check?
- Tests: which test turns red if this line is wrong? Run the tests that cover the change when you can. Changed behavior that no test would catch is a finding in itself.
- Simplicity: code that nothing needs, a second copy of an existing helper, a name that says something else.

Confirm each suspicion in the code or with a run before you write it down.

## Write up
- Open with one line: does the change do what it is for, as far as you checked.
- Then the findings, ordered by how hard their damage would be to undo once the change is in use: effects that cannot be taken back first, then wrong results, then extra work for the next reader. Write each one as a short scenario: the place (file and line), the input or sequence, what happens, what should happen. Propose a fix only when it is not obvious.
- Close with what you could not check, such as another OS, a job that runs only in CI, or missing data.

## Stop when
Every item on the map has been probed, and every finding is confirmed.

## Do not
- Report a matter of taste as a defect. When style matters, point to the project rule.
- Rewrite the change yourself unless asked.
- Leave an item of the map unprobed without naming it in the closing list.
