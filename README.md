# twig

A thin agent harness in Rust. **magpie provides the models; twig provides the loop,
the verifier and the record.**

Codex-shaped interaction: a stdio JSON-RPC session, an event stream, tiered
approvals, an append-only record. Codex's tax is left out: no per-call
classifier, no approval prompt on ordinary commands, no full-tree scan at boot,
no pattern-matching loop guard.

## measured

| | value |
|---|---|
| pre-Uber release binary | 3.0 MB; historical measurement |
| pre-Uber cold start (spawn, `ping`, exit) | p50 1.4 ms, p95 2.3 ms; not remeasured for this patch |
| pre-Uber policy classification | 266 ns/call; historical measurement |
| model calls per model turn | 1, including multiple tool requests; not a guarantee for an entire task |
| current checks | `cargo test`; `cargo clippy --all-targets -- -D warnings` |

## run

    cargo build --release
    ./target/release/twig selfcheck                 # offline, no model needed
    ./target/release/twig serve <worktree> <runs>   # JSON-RPC over stdio

    export TWIG_BASE_URL=http://127.0.0.1:3425/v1   # magpie's default gateway
    export TWIG_API_KEY=magpie
    export TWIG_ROLES='{"code":"...","plan":"...","judge":"...","filter":"..."}'

## the scoring set

    ./target/release/twig eval evals 2      # every task twice, fresh repo each run

Each task in `evals/` is a repo plus a `check` command. The harness copies the
repo, gives the agent only the goal, then runs `check` in the copy. **The check
is the judge; the agent's claim is never consulted.** Every task targets one
known failure mode:

| task | targets |
|---|---|
| 01-simple-bug | baseline |
| 02-root-cause | symptom in one file, cause in a shared helper with two callers |
| 03-no-cheating | the fastest "fix" is weakening the assertion |
| 04-slow-silent | an 8 s silent command: the shape magpie #3655 misread as a loop |
| 05-keep-wip | an uncommitted edit that `git reset --hard` would destroy |
| 06-feature | new behaviour without breaking the old |
| 07-big-output | failures buried in a 73 KB output stream |
| 08-long-file | relevant code beyond the read limit |
| 09-hanging-suite | project-specific test setup documented in CONTRIBUTING |
| 10-run-server | live HTTP verification and server cleanup |
| 11-hidden-spec | visible tests cover only part of a written specification |

A harness change is accepted only if it beats rerun variance here.

### Uber-style continuous improvement

`twig improve evals 1` evaluates a baseline, proposes configuration edits, and
tests a deletion-only ablation control every round. It keeps the existing
regression gate and confirmation rerun; removing text does not get a lower bar.
Only `harness.json` changes automatically. Source code, policy and judges are not
editable by the optimizer. Re-running the same suite is confirmation, not an
independent held-out evaluation.

Provider input/output/cached tokens and model identifiers are recorded on each
reply and aggregated per run. Missing usage remains unknown, never zero cost.
Comparisons require identical task sets and models; incomplete coverage cannot
win a token-based comparison. Token totals are consumption, not dollars: input,
output and cached tokens have different prices. No price table is configured.
Every field remains unknown unless every reply reports it. Outcome and trajectory
counts have independent regression gates, so one score cannot hide deterioration
in the other.

The `papercut` tool files structured friction reports: component, attempted,
expected, actual and recovery. Reports enter the append-only record, group by
component in evaluation summaries, and become evidence for the proposer even
when the task succeeds. Reports are untrusted observations, not proof of a bug;
they do not change policy or approve actions.

Optional `trajectory` rules in `task.json` grade observable execution separately
from output correctness. Supported contracts: `successful_command` (an argv
prefix plus a successful exit), `read_file` and `never_command`. The checks do
not trust assistant claims or `echo` text. Shell parsing is intentionally narrow:
only simple commands establish positive evidence; control flow, redirection,
expansion and comments are unsupported. A never-command rule reports unknown
for unparsed commands, never fabricating a forbidden attempt. These are opt-in
contracts, not a general shell security policy. Outcome and trajectory results
are stored separately. Both must be proven true for an overall pass; unsupported
evidence remains ungraded, not a proven failure. Candidate adoption rejects
changes in unknown-evidence coverage. No preferred workflow is inferred.

Shell output is drained through a nonblocking Unix socket into a bounded 1 MiB
capture; excess is discarded while draining continues. No temporary output file
can grow without bound. A finished shell returns without waiting for descendant
EOF. Linux `waitid(WNOWAIT)` retains the group leader's PID until cleanup, so
session teardown cannot target a reused process-group ID. Background processes
remain session-owned; a bounded drainer discards later logs until teardown.
Timeout messages include duration and partial output, and kill the whole group.
File reads explicitly mark line truncation. Linux is required for this runner.
This is process cleanup, **not an OS security sandbox**: cwd/worktree
isolation does not prevent a shell command from accessing host files or network.

Reference: [Uber Engineering: Optimizing Uber's Agent Skills](https://www.uber.com/us/en/blog/optimizing-ubers-agent-skills/).


## one guard only

magpie has its own loop guard (Settings → Models → Replies → *Stop looping
replies*, key `noLoopGuard`). When both are on, false positives add up. A long
`write_stdin` wait has already been misread as a loop and ended a turn early
(magpie #3655).

The magpie switch is **global**: turning it off removes loop protection for
every agent routed through that gateway. Turn it off only if twig is the main
client. Otherwise point twig at a separate magpie instance.

## protocol

One request per line on stdin. Events and replies come back on stdout.

| method | does |
|---|---|
| `turn` | runs until the model stops calling tools. With `journal` + `candidate`, the result is replayed before it is accepted |
| `approval.list` / `approval.resolve` | T2 calls waiting on a human; approving one runs it and tells the model |
| `playbooks.route` | goal → playbook, with the steps to copy into the todo list |
| `record.verify` / `record.tail` | hash-chain check, recent entries |
| `policy.stats` / `guard.report` / `audit.drain` | introspection |

Events: `turn.started`, `tool.planned`, `tool.started`, `tool.completed`,
`approval.requested`, `approval.resolved`, `guard.tripped`, `verify.passed`,
`verify.failed`, `turn.completed`.

## the three tiers

| tier | behaviour |
|---|---|
| `auto` | read-only and runs no repo code (`ls`, `cat`, `rg`, `git diff`, …), not audited |
| `audit` | **default**: runs now, recorded, audited out of band |
| `deny` | ≤12 destructive patterns; held for `approval.resolve` |

## where each idea came from

| source | what twig uses |
|---|---|
| Codex | protocol/session split, event stream, policy as data, append-only record |
| pstack (poteto) | playbook routing; a skipped step needs a written reason; no self-review; the decision trail |
| yetone / magpie | cheap triage before an expensive model; outer loop; one worktree per item |
| fast-anything | the old behaviour is the oracle; exact replay; branch-coverage gate and cost gate |
| harness² / 2610.11655 | classify failures as process or content before choosing what to fix |
| mini-SWE-agent / Pi | small execution interface; bash/read plus the reporting-only papercut tool |
| Uber Engineering | token telemetry, agent-filed papercuts, removal controls, trajectory contracts |

## not done

- **No source adapters.** The outer loop has the queue, worktree isolation and
  triage, but no GitHub/Discord/X readers and no scheduler.
- **No propose-probe-compose.** `twig classify` measures process vs content
  failures first. Build PPP only if process failures dominate.
- **No streaming deltas, no TUI, no context compaction beyond output clipping.**
