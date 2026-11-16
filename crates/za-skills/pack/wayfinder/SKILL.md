---
name: wayfinder
description: Use when you start work in an unfamiliar codebase or area and need a map before changing anything.
---
# Wayfinder

Build a quick, accurate map of unfamiliar code, then go deep only where the task needs it.

## Steps
1. Read the entry docs: README, AGENTS.md, CONTRIBUTING, the docs index. Note the build, test and run commands.
2. Get the shape: top-level folders, manifests and workspaces, languages. Use file name searches and directory listings, not full reads.
3. Find the entry points: main functions, CLI commands, servers, exported APIs.
4. Trace one path that matters for the task, from an entry point to the code that does the work. Search for names and read only those regions.
5. Note the conventions: error handling, logging, configuration, test layout, naming. New code must follow them.
6. Run the build or the tests once to confirm the commands work, if that is allowed.
7. Write a short map: key folders and their roles, the traced path with file and line, the commands, the conventions, open questions.

## Stop when
You can name the files you will change and the tests that cover them.

## Do not
- Read the whole repository, or every file in a folder.
- Change code while mapping.
- Trust stale docs over the code. Note where they differ.
