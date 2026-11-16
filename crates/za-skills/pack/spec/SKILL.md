---
name: spec
description: Use when a feature or change is agreed in words but needs a short written spec with acceptance criteria before work starts.
---
# Spec

Write a one-page spec that says what will be built and how to tell that it is done.

## Steps
1. Gather the inputs: the request, the answers so far, related code and docs. If key facts are missing, ask the user first (the grill skill helps).
2. Write the spec in these parts:
   - Problem: who has it and why it matters, in two or three sentences.
   - Goal and non-goals: what is in scope and what is explicitly out.
   - Behavior: what the user or caller sees, including errors and limits.
   - Acceptance criteria: numbered, each checkable by a test or a command, with concrete inputs and expected results.
   - Constraints: performance, security, compatibility, dependencies.
   - Open questions.
3. Read it as a tester would: can each criterion fail? Replace vague words such as fast, simple or robust with numbers, or remove them.
4. Show the spec to the user and apply the corrections.

## Stop when
The user accepts the spec, and every acceptance criterion is testable.

## Do not
- Design the internals in detail: that belongs to the plan.
- Add features nobody asked for.
