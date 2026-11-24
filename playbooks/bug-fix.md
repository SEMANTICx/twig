---
name: bug-fix
when: bug, broken, crash, wrong, fails, regression, repro, defect
---

# bug-fix

Reproduce before you touch anything. A fix without a failing case first is a guess
that happens to compile.

## steps

1. **Restate the defect** in one sentence: what happens, what should happen, on which input.
2. **Reproduce it.** Run the smallest command that shows the wrong behaviour. Record the exact output.
3. **Record the repro as a journal** so the fix can be checked later without you:
   capture the real invocations and the current (wrong) results under `verify/journal/`.
4. **Find the root cause.** Read every caller of the function you are about to change.
   Fix it once, where all callers route through.
5. **Fix the smallest thing** that makes the repro pass.
6. **Replay the journal.** Every previously-correct case must still match, exactly.
7. **Run the narrowest existing test** that covers the changed path.
8. **Commit** with the repro command and its before/after output in the body.

## skip rules

Skipping a step is allowed only with a written reason, in this form:

    skip: 3 — the path has no recorded calls; covered by tests/unit/test_foo.py instead

Two skips on the same step across sessions means the step is wrong, not the task.
Rewrite the playbook instead of skipping again.

## forbidden

- Widening a check to make the repro pass.
- Catching an exception to silence the symptom.
- Editing the journal to match new behaviour.

## done when

The original repro command prints the correct result, the journal replays clean,
and no caller of the changed function was left untouched.
