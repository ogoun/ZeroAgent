---
name: grill
description: Use when a request is vague or hides choices and risky assumptions, and a wrong guess would be costly to undo.
---
# Grill

Turn a vague request into clear requirements by asking the user, one question at a time, before the work starts.

## Steps
1. Read what already exists (code, docs, earlier messages), so that you ask only what the sources cannot answer.
2. List the unknowns: goal, users, scope, inputs and outputs, limits, failure cases, what done looks like.
3. Rank them by the cost of a wrong guess and start with the most expensive.
4. Ask one question per ask_user call, in plain words, with two to four concrete options and the one you recommend.
5. After each answer update the list: drop what it settled, add what it opened.
6. Restate the agreed requirements as a short list and ask for a final yes.

## Stop when
The user confirms the summary, or the remaining unknowns are cheap to change later. Write those down as assumptions.

## Do not
- Ask what you can find out yourself.
- Send many questions at once, or use internal jargon.
- Start building before the summary is confirmed, unless the user says to go ahead.
