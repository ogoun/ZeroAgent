---
name: router
description: Use when the next step could go to more than one skill, or to none.
---
# Router

Pick at most one skill for the current step, or none.

## Steps and skills
- clarify a vague request: grill
- write down what to build and how to tell it is done: spec
- split agreed work into tasks: tickets
- find where a change goes in unfamiliar code: orient
- write code test first: tdd
- find the cause of a fault: diagnose
- check a change: review
- back a report on this run with tool results from its journal: evidence
- rely on exact library or API behavior: source-driven
- answer from web sources: research-with-citations
- pin down terms or record a decision: domain-model
- look back at finished work: retro
- pass the work on: handoff

A skill catalog, when one is shown, may list more skills; each description says when its skill applies.

## How to choose
1. Name the step you are about to take in a few words and find it above or in the catalog.
2. Take a skill only if its steps would change what you do. A skill you would follow anyway only costs context.
3. One skill at a time. Take the next one when the work moves to a different kind of step.
4. If two fit, take the one for the earlier step: clarify before specify, specify before split, debug before review.
5. To load it, call the skill tool with the exact name if that tool is listed. If it is not, tell the user which skill fits: they can load it by typing a slash and its name, for example /tdd.

## A typical path for a new feature
grill, spec, tickets, then for each ticket tdd, review and evidence. In an unfamiliar codebase start with orient.

## No skill when
- The task is a small, clear change or a direct question.
- The user asked for a specific way of working: follow it instead.
- A skill for this step is already loaded: keep using it.

## Stop when
You have chosen one skill or decided on none. Choose again only when the kind of step changes.

## Do not
- Load skills to look thorough.
- Follow a skill against a direct instruction from the user.
