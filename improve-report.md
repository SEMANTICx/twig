# twig improve

## baseline

rerun disagreement: 1 run(s)

| task | pass | calls | secs | tool timeouts | killed | failure |
|---|---|---|---|---|---|---|
| 01-simple-bug | 0/2 | 5.0 | 17.4 | 0 | 0 | content, content |
| 09-hanging-suite | 2/2 | 6.0 | 27.1 | 0 | 0 | - |
| 11-hidden-spec | 1/2 | 4.0 | 31.4 | 0 | 0 | content |

**3/6 passed**, 152s of agent time

Ungraded (trajectory evidence unknown): 0. These are not proven failures.

Reported input/output/cached tokens: 152085/4190/139392. Complete usage: 6/6 runs. No dollar estimate.


## round 1 (needs +1 passes, or same passes and clearly cheaper)

### r1c1: rejected, same passes but token consumption regressed >10%
> The failed runs report successful tests, but edits and verification occur within the same shell invocation. Separating them tests whether successful output is masking changes that do not persist across calls. This is a narrow experiment, not a demonstrated fix for the unexplained content failures.

changes: bash_description: text 86 -> 216 bytes

| task | pass | calls | secs | tool timeouts | killed | failure |
|---|---|---|---|---|---|---|
| 01-simple-bug | 0/2 | 5.5 | 18.4 | 0 | 0 | content, content |
| 09-hanging-suite | 2/2 | 6.5 | 29.1 | 0 | 0 | - |
| 11-hidden-spec | 1/2 | 5.0 | 35.4 | 0 | 0 | content |

**3/6 passed**, 166s of agent time

Ungraded (trajectory evidence unknown): 0. These are not proven failures.

Reported input/output/cached tokens: 175708/4380/unknown (2/6 runs reported). Complete usage: 6/6 runs. No dollar estimate.


### r1c2: rejected, regression on 11-hidden-spec:trajectory
> This candidate adds an independent check of the delivered artifact rather than changing command grouping. The common concern is the mismatch between apparent execution success and content failure; a fresh inspection can expose missing or unexpected persisted changes. Existing evidence does not justify raising any limits.

changes: system: text 764 -> 1067 bytes

| task | pass | calls | secs | tool timeouts | killed | failure |
|---|---|---|---|---|---|---|
| 01-simple-bug | 0/2 | 5.5 | 18.8 | 0 | 0 | content, content |
| 09-hanging-suite | 2/2 | 6.5 | 34.1 | 0 | 0 | - |
| 11-hidden-spec | 0/2 | 5.0 | 36.0 | 0 | 0 | content, content |

**2/6 passed**, 178s of agent time

Ungraded (trajectory evidence unknown): 0. These are not proven failures.

Reported input/output/cached tokens: 178499/4564/unknown (2/6 runs reported). Complete usage: 6/6 runs. No dollar estimate.


### r1c3: accepted, passes 3 -> 4 (margin 1)
> Deletion-only control: remove one existing prompt paragraph, change no limits.

changes: system: text 764 -> 615 bytes

| task | pass | calls | secs | tool timeouts | killed | failure |
|---|---|---|---|---|---|---|
| 01-simple-bug | 0/2 | 5.0 | 17.9 | 0 | 0 | content, content |
| 09-hanging-suite | 2/2 | 5.0 | 25.4 | 0 | 0 | - |
| 11-hidden-spec | 2/2 | 4.0 | 30.1 | 0 | 0 | - |

**4/6 passed**, 147s of agent time

Ungraded (trajectory evidence unknown): 0. These are not proven failures.

Reported input/output/cached tokens: 140602/3847/unknown (3/6 runs reported). Complete usage: 6/6 runs. No dollar estimate.


**adopted** (passes 3 -> 4 (margin 1)), written to /tmp/twig-rs/harness.json

## confirmation re-run

| task | pass | calls | secs | tool timeouts | killed | failure |
|---|---|---|---|---|---|---|
| 01-simple-bug | 0/2 | 4.5 | 14.1 | 0 | 0 | content, content |
| 09-hanging-suite | 2/2 | 5.5 | 25.4 | 0 | 0 | - |
| 11-hidden-spec | 0/2 | 4.0 | 30.0 | 0 | 0 | content, content |

**2/6 passed**, 139s of agent time

Ungraded (trajectory evidence unknown): 0. These are not proven failures.

Reported input/output/cached tokens: 140397/3895/unknown (4/6 runs reported). Complete usage: 6/6 runs. No dollar estimate.

**not confirmed** (regression on 11-hidden-spec:trajectory): reverted to the baseline config.

## code changes the loop cannot make

- Investigate the mismatch between successful verification output, apparently correct diffs, and content-failure results before attributing these failures to agent behavior. The evidence does not establish a configuration-level cause.
- Check that audited shell execution applies mutations to the same worktree later inspected by the scorer, and that those mutations survive tool completion. If auditing uses an isolated workspace, ensure approved changes are propagated before returning success; configuration cannot repair a persistence bug.
- Include concrete content-check failure reasons and the evaluated artifact state in run diagnostics. Distinguishing missing edits, unexpected edits, and incorrect behavior would allow targeted configuration changes instead of speculative guidance.
