---
name: tdd
description: Use when writing or changing code whose behavior an automated test can check.
---
# TDD

Grow the code in small cycles, each starting with a failing test.

## Cycle
1. Red: write one test for the next small behavior. Run it and see it fail for the expected reason, not because of a typo or a missing import.
2. Green: write the least code that makes it pass. Run it together with the tests near it.
3. Refactor: with every test green, improve names, remove duplication, simplify. Run the tests again.
4. Commit or note the step, then pick the next behavior.

## Choosing tests
- Test behavior through the public interface, not private details.
- Start with the simplest case, then the edges: empty, one, many, limits, errors.
- One reason to fail per test; name the test after the behavior.
- Use the project's test framework and fixtures. Keep tests fast and independent.
- A bug fix starts with a test that reproduces the bug.

## Stop when
Every behavior in the task has a test that failed before and passes now, and the relevant suite is green.

## Do not
- Write production code for behavior that no failing test asks for.
- Change a test to match broken code.
- Skip the red run: a test that already passes on the old code does not cover the new behavior.
