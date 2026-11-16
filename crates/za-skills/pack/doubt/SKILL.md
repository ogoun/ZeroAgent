---
name: doubt
description: Use before you report that work is done, fixed or passing, to test the claim against fresh evidence.
---
# Doubt

Treat your own conclusion as a hypothesis until a check you have just run supports it.

## Steps
1. Write the claim in one sentence: what works, for which input, under which conditions.
2. Name the evidence that would prove it: a command, a test, an output, a file state.
3. Run it now. Earlier output, memory and a sub-agent's report do not count.
4. Read the whole result: exit code, failures, warnings, skipped tests.
5. Pick the case most likely to break the claim (an edge input, another platform, cleanup, the original reproduction) and check it too.
6. Compare the result with the user's request item by item. Anything asked for and not done is not done.
7. Report what was verified and how, and what was not.

## Stop when
Each part of the claim has fresh evidence, or is reported as unverified.

## Do not
- Say done, fixed, works or passes without output that shows it.
- Quietly narrow the claim to fit what passed.
- Retry until green without knowing why it was red.
