---
name: router
description: Use when you are unsure which skill fits the current step, or whether to load any skill at all.
---
# Router

Pick at most one skill for the current step, or none.

## How to choose
1. Name the step you are about to take in a few words: clarify, specify, split, explore, test first, debug, review, verify, research, define terms, look back, hand over.
2. Match it to a skill by its description in the activate_skill list. Each description says when the skill applies.
3. Load a skill with activate_skill only if its steps change what you would do. A skill you would follow anyway only costs context.
4. One skill at a time. Load the next one when the work moves to a different kind of step.
5. If two skills fit, take the one for the earlier step: clarify before specify, specify before split, debug before review.

## A typical path for a new feature
grill, spec, tickets, then for each ticket tdd, review and doubt. In an unfamiliar codebase start with wayfinder.

## No skill when
- The task is a small, clear change or a direct question.
- The user asked for a specific way of working: follow it instead.
- A skill for this step is already loaded: keep using it.

## Stop when
You have chosen one skill or decided on none. Choose again only when the kind of step changes.

## Do not
- Load skills to look thorough.
- Follow a skill against a direct instruction from the user.
