# rules

Hard constraints for this harness. Each line earned its place by being violated
twice. Nothing here is aspirational.

## the defence budget

1. **One guard, not two.** magpie's loop guard is off for this provider profile.
   Two guards multiply false positives; a long `write_stdin` wait already reads as
   a loop to a pattern matcher.
2. **Defence never sits in the exec path.** Risk is classified once, at plan time,
   and cached by command *shape*. The hot path is one dict lookup.
3. **Hard-deny stays under ten rules.** Every rule costs a human interruption.
   If you cannot name what it prevents, delete it.
4. **Default is record-and-audit.** The command runs. The record remembers.
   A human is asked only for T2.
5. **A guard never ends a turn.** It degrades: `ok` -> `narrow` -> `abort` on budget.
6. **Progress is a workspace digest change**, never a call-pattern match.

## the loop

7. **One round-trip per turn.** N tool calls, one model call.
8. **No full-tree scan at boot.** Cold start is a stat of the config, nothing more.
9. **Clip at the tool boundary.** Full output goes to the record; the model gets a handle.
10. **Context is append-only.** Compaction is the only thing allowed to rewrite it,
    and it is logged, because it is the only thing that breaks the prompt cache.

## the discipline

11. **No self-review.** The verifier is a separate process and never reads the
    producing agent's claims.
12. **Nothing is enabled until it is proven** on recorded real calls, within
    tolerance, with matching errors, and not slower.
13. **Write the exam before the code.** A harness change lands only if it beats
    the rerun variance on the scoring set.
14. **Fix at the highest level that works**: architecture, then types, then lint,
    then CI, then tests, then docs last. A rule that lives only in a document fails
    silently and forever.
15. **Two strikes, not one.** A single mistake does not buy a new rule.
16. **One worktree per item.** Blast radius is bounded by isolation, not by checks.
17. **Record is append-only and hash-chained.** If you find yourself wanting to
    rewrite history, the run is already a failure.
