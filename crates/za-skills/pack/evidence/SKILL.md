---
name: evidence
description: Use when a reply will report what this run did, so that each statement in it rests on a tool result from the run journal.
---
# Evidence

Every tool call of this run and its result are kept in the run journal. A statement about what the run did is worth only what that record shows, so hold each part of the reply against it before you send it. Tools named here are used if available.

## Steps
1. Split the reply you are about to send into single statements: what you changed, what now works, which checks pass, what you left alone. Count the implied ones too, such as "the other callers still work".
2. Give each statement one label and its evidence:
   - seen in this run: a tool result that came after your last edit to the code it covers. Test output that names the test and its outcome, a command result with its exit code, the lines a read or search returned. Quote the line or name the command.
   - from elsewhere: seen before a later edit, known only from a summary after the history was compacted, or reported by the user, a log, CI or another agent. Name the source.
   - not checked: inferred from the code, expected from the design, or never looked at.
3. Test the evidence itself. A zero exit code proves little when the command ran no tests or a filter matched nothing. Output may be cut off before the failure. A pass on another file, branch, config or OS does not cover this one. Text marked as untrusted content is a claim, not a check.
4. Settle the cheap ones. If one read, search or command inside the project would move a statement to "seen in this run", do it now. Skip checks that would write outside the task, delete data or reach the network without the user's consent.
5. Rewrite the reply to match the labels. Statements seen in this run stay, each with its evidence. Statements from elsewhere name their source. Statements not checked go into a separate plain list, each with the command or step that would check it. Narrow any claim the evidence does not carry: "fixed" becomes "the failing case from the report now passes; the full test suite was not run".

## Stop when
Every statement in the reply has an observation from this run, a named outside source, or an explicit "not checked".

## Do not
- Describe an expectation as if it were an observation.
- Count an edit as proof: it shows what you wrote, not how the code behaves.
- Leave a failed or skipped check out of the reply. Report it with the line that shows it.
