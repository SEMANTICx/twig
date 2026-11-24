---
name: autonomous-run
when: overnight, unattended, while i sleep, autonomous, loop, pipeline, land the stack
---

# autonomous-run

For work that outlives your attention. The purpose is not speed, it is that the
morning review reads a decision trail instead of a chat log.

## steps

1. **Write down what "done" means** before starting, as commands and outputs.
   If you cannot, this is not an autonomous run; it is an investigation.
2. **Take one narrow slice**, land it, then take the next. Never batch.
3. **Every turn ends in a check**, not in a sentence. No progress without output.
4. **One commit per accepted slice.** Discard a slice that produced no measurable
   gain — do not leave it as dead weight for the reviewer.
5. **Log every judgment call** to `decisions.tsv`: what you chose, what you rejected,
   why. This is the only thing the human reads in the morning.
6. **On a guard trip, narrow — never stop.** If progress stalls, shrink the toolset
   and write the plan to disk so the next attempt inherits the state.
7. **Leave the workspace clean.** Anything half-done goes in a written next step, not
   in uncommitted files.

## morning check

    ./twig decisions        # what was decided, and what was rejected
    ./twig verify           # journal replay verdict
    ./twig record verify    # chain intact, nothing rewritten

## forbidden

- Marking a feature done without an end-to-end check (this is the most common failure).
- Rewriting history in the record to look tidier.
- Continuing past an aborted budget.

## done when

`decisions.tsv` explains every accepted change, the journal replays clean, and the
next slice is written down for whoever picks it up.
