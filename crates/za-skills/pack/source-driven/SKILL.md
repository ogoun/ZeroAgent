---
name: source-driven
description: Use when code depends on a library, API, tool or standard whose exact behavior or version matters.
---
# Source-driven

Let the primary source for the version in use decide how the code behaves, not memory.

## Steps
1. Before writing, list the facts your change will rely on: a signature, a default, an error, a limit, a format, an order of events.
2. Pin the version each fact belongs to, from the lock file, the manifest, the version output of the tool or the configuration.
3. Settle each fact from the closest source for that version: the dependency's code in the package cache or vendor folder, its changelog, its official docs, the text of the standard. Go to the web only when nothing local has it.
4. When the docs and the code of that version disagree, the code wins. When neither is clear, write a small probe, run it and keep its output.
5. Note where each fact came from (a path or link, and the section). Put it into a code comment only when a reader would otherwise be surprised.
6. If the observed behavior differs from the docs, tell the user about the gap.

## Stop when
Every fact on the list points to a source for the right version or to a probe you ran.

## Do not
- Use an API as you remember it, or as it is documented for another version.
- Trust a blog post or a forum answer that you have not checked against the source.
- Change a dependency version to make the docs fit, unless the user agrees.
