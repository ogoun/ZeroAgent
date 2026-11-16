---
name: grill
description: Use when a request is vague or hides choices and risky assumptions, and a wrong guess would be costly to undo.
---
# Grill

Turn a vague request into clear requirements by putting short questions to the user, in turn, before the work starts.

## Steps
1. Read what already exists (code, docs, earlier messages), so that you ask only what the sources cannot answer.
2. List the unknowns: goal, users, scope, inputs and outputs, limits, failure cases, what done looks like.
3. Rank them by the cost of a wrong guess and start with the most expensive.
4. Each question covers exactly one unknown and waits for its answer (use ask_user if it is listed). Use plain words, offer two to four concrete answers to choose from, and say which one you advise and why, in one line.
5. After each answer update the list: drop what it settled, add what it opened.
6. Write the settled requirements as a short numbered list, each one checkable, and let the user correct it.

## Stop when
The user accepts the list, or the remaining unknowns are cheap to change later. Write those down as assumptions.

## Do not
- Send many questions at once.
- Use internal jargon. If a term is needed, explain it in one plain sentence.
- Start building before the list is accepted, unless the user says to go ahead.
