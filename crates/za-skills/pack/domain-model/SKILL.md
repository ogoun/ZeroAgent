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
Record a decision when a later run that reads only the code, AGENTS.md and docs/ would likely choose differently.
1. Look for existing records first: AGENTS.md or docs/ say where, docs/adr is common. If a record already covers the question, the new decision gets its own record that names the one it replaces. The old text stays as it was, apart from a status line if the format has one.
2. Follow the numbering and layout of the latest record. With no layout yet, write the decision in one sentence, then the facts and constraints behind it, each rejected option with the reason it lost, what changes for code and data, and the signal that should make someone revisit it.
3. Keep it to about a page and link the plan and code it affects. When the decision was the user's, ask them to confirm the wording.

## Stop when
Every key term has one meaning, and each open decision is either recorded or listed as open.

## Do not
- Invent terms that neither the user nor the code uses.
- Write a record for a question that is still open: list it as open instead.
