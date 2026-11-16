---
name: doubt
description: Use when you are about to say that work is done, fixed or passing, to look first for the case that proves you wrong.
---
# Doubt

Try to refute your own conclusion, and report it only as far as the attempts failed.

## Steps
1. Write the conclusion as one sentence with its scope: which behavior, for which inputs, in which environment.
2. List the ways it could still be false. The usual suspects:
   - the evidence came from other code: a stale build, a cached result, another branch, file or binary;
   - it came from other inputs: an easy case instead of the one in the request or in the original report;
   - it came from another place: a different OS, config, permissions or data than the user has;
   - it covered only a part: tests skipped, filtered or ignored, one package out of several;
   - something else moved too: another caller, a file format, a stored hash, a generated file.
3. For each way, name what you would observe if it were true. Then go and look, after your last edit.
4. A look that finds nothing makes the conclusion stronger. A look that finds something makes it false: fix the work or shrink the conclusion, and say which you did.
5. Compare the result with the request as a whole: a promise, a step or a check that is still open goes into the report as open.

## Stop when
Every way on the list has been looked at, or is reported as not looked at, with the reason.

## Do not
- Present a shrunken conclusion as if it were the original one.
- Repeat a flaky check until it passes and count that as evidence.
- Drop a way from the list because it is hard to check.
