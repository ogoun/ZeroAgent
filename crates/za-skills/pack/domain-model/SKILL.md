---
name: domain-model
description: Use when terms in the code, docs or conversation are vague or conflicting, or a design decision needs a short record for later readers.
---
# Domain model

Agree on the words of the problem, and keep decisions where the next reader will find them.

## Terms
1. Collect the nouns and verbs that carry meaning in the request, the code and the docs.
2. Mark the collisions: one word used for two things, or two words used for one thing.
3. Give each term one line: the word, what it means in this project, what it must not be confused with, and where it lives in the code.
4. Ask the user only about terms the sources cannot settle.
5. Keep the glossary where the project keeps docs (look for an existing one first), and use its words in code, tests and messages.

## Decisions
Write a record when a later reader would otherwise reopen the question: an option was dropped for a reason, the choice binds other code or stored data, or a known risk was accepted.
1. Find the project's decision records (often docs/adr) and follow their format and numbering.
2. If there is no format yet, write the context, the decision, the options dropped and why, the consequences and any accepted risk.
3. Keep it under a page. Link the code or plan it affects.
4. A later decision replaces an earlier one through a new record that names it. Do not rewrite old records.

## Stop when
Every key term has one meaning, and each open decision is either recorded or listed as open.

## Do not
- Invent terms that neither the user nor the code uses.
- Record choices that are cheap to undo.
