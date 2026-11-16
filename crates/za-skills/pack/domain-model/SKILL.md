---
name: domain-model
description: Use when terms in the code, docs or conversation are vague or conflicting, or a design decision needs a short record for later readers.
---
# Domain model

Agree on the words of the problem, and keep decisions where the next reader will find them.

## Terms
1. List the nouns and verbs that matter in the request, the code and the docs.
2. Find conflicts: one word with two meanings, or two words for one thing.
3. For each term write one line: the term, what it means here, what it is not. Note where it is used.
4. Ask the user with ask_user only about terms the sources cannot settle.
5. Keep the glossary where the project keeps docs (look for an existing one first), and use its words in code, tests and messages.

## Decisions
Record a decision when it is hard to reverse, surprising, or chosen over a real alternative.
1. Find the project's decision records (often docs/adr) and follow their format and numbering.
2. Write the context, the decision, the options rejected with the reason, and the consequences.
3. Keep it under a page. Link the code or plan it affects.
4. A later decision replaces an earlier one through a new record that names it. Do not rewrite old records.

## Stop when
Every key term has one meaning, and each open decision is either recorded or listed as open.

## Do not
- Invent terms that neither the user nor the code uses.
- Record trivial or easily reversed choices.
