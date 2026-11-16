---
name: tdd
description: Use when writing or changing code whose behavior an automated test can check.
---
# TDD

Express each small behavior as a test before the code exists, make it pass with plain code, then tidy up while the tests stay green.

## Cycle
1. Red. Choose the next small behavior and express it as one test. Run that test alone. It must fail on the check of the new behavior itself; a stop before that check means the test is not ready yet, so repair it and run it again.
2. Green. Change the code until that test passes, and stop there. Then run the tests of the module you touched.
3. Refactor. Make the new code and its tests easy to read and to change, while everything stays green. Rerun the same tests after each change.
4. Move to the next behavior. Commit only if the user asked for commits.

## Choosing behaviors
- The main case first, then the edges: no input, a single item, a full batch, exactly at a limit and one past it, malformed input, a dependency that fails.
- Drive the code the way its callers do (public functions, the command line, the API) and assert on what they can observe.
- Small, fast, independent tests. Name each one as a statement about the behavior, so that a failure reads like a bug report.
- The project's own test framework, helpers and fixtures. Temporary files stay inside the project.
- A reported bug becomes a test that reproduces it, before any fix.

## Stop when
Each behavior of the task has a test that failed on its own check before the change and passes after it, and the tests of the touched code pass.

## Do not
- Write code ahead of the test that needs it.
- Weaken a test or fake its data to get it to pass.
- Report tests as passing without naming the command you ran.
