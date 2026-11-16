---
name: diagnose
description: Use when a bug, failing test, crash or wrong output needs its root cause found before any fix is written.
---
# Diagnose

Find the cause of a fault from evidence, then fix that cause once.

## Steps
1. Pin the symptom: the exact command, input, expected and actual output. Copy error text verbatim.
2. Reproduce it with the shortest command you can run again. If it does not reproduce, collect logs, versions and recent changes instead, and say so.
3. Narrow the search. Compare a working case with a failing one, bisect recent commits or inputs, and read the code on the failing path.
4. Write one hypothesis that explains every observation, and the check that would prove it wrong.
5. Run that check. A failed prediction rules the hypothesis out: record it and form the next one.
6. When a check confirms the cause, write a test that fails because of it, then make the smallest fix.
7. Rerun the reproduction and the nearby tests. Report the cause, the evidence and the fix.

## Stop when
- The reproduction passes after the fix, and the new test fails without it.
- Or three hypotheses have failed: report what was ruled out and ask the user for access or data.

## Do not
- Change code to see whether the symptom goes away before you know why it happens.
- Silence the error, add retries or widen a timeout as the fix.
- Change several things in one experiment.
