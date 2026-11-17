---
name: orient
description: Use when you must change or explain code in a project you do not know yet and first need to find where the change goes and how to check it.
---
# Orient

Learn only as much of an unfamiliar project as the task needs: where to make the change and how to check it. Every line you read costs context, so look things up first and read in small pieces. Tools named here are used if available.

## Steps
1. Pull search terms from the task: names the user used, error text, a command line flag, a config key, a message the user sees, an endpoint. Exact strings find the spot fastest.
2. Look each term up before you open anything: search for text inside files, glob for file names if it is available.
3. Open only the hits that look right, with read and a start line and limit: a few dozen lines around the match, not the whole file.
4. If nothing matches, get the layout: list the top folders with glob if it is available, or with a short listing command. Pick the folder whose name fits the task and search inside it.
5. Learn how the project is built and checked. AGENTS.md comes first; it may already be in your instructions. Then the build manifest (for example Cargo.toml, package.json, pyproject.toml, go.mod, a Makefile) for modules and commands, and the CI config (for example .github/workflows) for the exact build, test and lint commands. When a README and CI disagree, CI is what actually runs.
6. Find the tests that reach the spot: search for its function or type name in test files. Such a test shows how the code is used and gives you a command to check it.
7. Look one step out from the spot: who calls it and what it calls. Stop at the edge of the change.
8. Write a short note:
   - Task: one line.
   - Where: file and line or symbol to change, plus the callers, tests and config that move with it.
   - Check: the command to run, the test to add or extend, and the result to expect.
   - Rules: project conventions that affect this change, such as error handling, logging, naming, generated files.
   - Unknowns: what you could not settle.

## Stop when
You can name the files, the place of the change and the command that will check it. If about a fifth of the context window is spent and you still cannot, report what you found and ask the user.

## Do not
- Read whole files or whole folders to get a general picture.
- Edit code while orienting.
- Run long builds or test suites only to learn the layout when the manifest or CI already names the commands.
