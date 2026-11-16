---
name: review
description: Use when asked to review a diff, branch or pull request, or before you merge your own changes.
---
# Review

Judge a change on four axes and report findings the author can act on.

## Steps
1. Learn the intent: the task, the linked issue or plan, and what the change claims to do.
2. Read the whole diff, then the code around each hunk. Follow the callers of anything whose behavior changed.
3. Correctness: logic errors, edge cases (empty, maximum, Unicode, concurrency, errors), broken contracts, missing migrations.
4. Security: untrusted input that reaches paths, shell, queries or HTML; secrets in code or logs; permission checks; new network or file access.
5. Tests: would a test fail without the change? Are edge cases and failure paths covered? Run the relevant tests if you can.
6. Simplicity: dead code, duplication, needless abstraction, misleading names, a shorter way to the same result.
7. Confirm each finding against the code before you report it.

## Report
Group findings by severity: critical (wrong or unsafe), important (fix before merge), minor (nice to have). For each give the file and line, what is wrong, why it matters and a suggested fix. End with a verdict: approve, approve with fixes, or needs changes.

## Stop when
Every hunk has been read, all four axes are checked, and each finding is confirmed in the code.

## Do not
- Report style preferences as defects.
- Rewrite the change yourself unless asked.
- Approve code you did not read.
