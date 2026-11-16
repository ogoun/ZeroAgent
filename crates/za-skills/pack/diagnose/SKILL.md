---
name: diagnose
description: Use when something behaves differently from what you expect and the cause is unknown: a failing test, a wrong result, a crash or a fault that comes and goes.
---
# Diagnose

Explain the fault from evidence first, then change the code at the place the evidence points to.

## Set up
1. Write the fault down exactly: the command, input and environment, the expected and the actual result, the error text copied as is.
2. Find the shortest command that makes it happen again. If it will not happen again, work from what it left behind (logs, journals, versions, recent changes) and say that the diagnosis rests on those traces.
3. Start a log of observations. Each entry says what you checked, what you expected and what you saw.

## Loop
4. Propose a cause that fits every entry in the log, and one thing it predicts that you have not seen yet.
5. Look. A useful look narrows the space: put a case that works next to one that fails and move one toward the other (inputs, commits, config, environment) until one difference is left.
6. Log the result. A failed prediction rules the cause out: go back to step 4 with the new entry.

## Fix
7. When a prediction holds, write a test that fails because of the cause. Then make the smallest change in the code that holds the cause, even if the symptom appears somewhere else.
8. Run the original command and the tests near the change. Report the cause, the log entries that show it and the fix.

## Stop when
- The original command gives the expected result, and the new test fails without the fix.
- Or no check within your reach can tell the remaining causes apart. Report the log, the causes still standing and the one observation that would separate them, and ask the user whether they can provide it.

## Do not
- Edit code as an experiment before the log names a cause for it to test.
- Hide the fault with catch-all handlers, retries, longer timeouts or skipped tests.
- Change more than one thing per look: the log could not tell which one mattered.
