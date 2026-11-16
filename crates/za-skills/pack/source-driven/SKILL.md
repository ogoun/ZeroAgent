---
name: source-driven
description: Use when code depends on a library, API, tool or standard whose exact behavior or version matters.
---
# Source-driven

Base every technical choice on the primary source for the version in use, not on memory.

## Steps
1. Find the version in use: lock files, manifests, the tool's version output, configuration. Write it down.
2. Find the primary source for that version: official docs, the changelog, the dependency's source code, the text of the standard. Prefer a local copy (vendored code, the package cache, installed headers) to the web.
3. Read the part you rely on: signature, defaults, errors, limits, deprecations. Note the link or path and the section.
4. If the docs are unclear, read the source, or write a tiny probe and run it.
5. Write the code to what the source says. Cite the source in a comment only when the reason is not obvious.
6. When the docs and the observed behavior differ, trust the behavior and report the gap.

## Stop when
Every assumption your change depends on is backed by a source you read or a probe you ran.

## Do not
- Use an API from memory or from another version.
- Copy snippets from blogs or answers without checking them against the source.
- Upgrade a dependency to make the docs fit, unless the user agrees.
