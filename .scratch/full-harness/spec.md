# Twig full harness acceptance source

## Problem and solution
The user wants a standalone Rust coding harness using magpie for models, Codex-style responsive human/agent interaction without repeated defensive friction, and unattended maintenance like poteto/yetone. Existing prototype has working evaluation but lacks product-level interaction and automation. Complete the requested behaviors end to end; do not claim missing features as done.

## User stories and acceptance
1. Start `twig chat [repo]` and receive streamed model text, tool start/output/end without waiting for the whole task.
2. While a run is active, cancel it and its child commands; send steering to the next safe model boundary without corrupting messages.
3. An approval pauses the exact tool call. Approve/deny it while the session is active, then resume without duplicate execution. Ordinary work needs no prompt.
4. `twig serve` exposes the same operations over line-delimited JSON requests/events; EOF finishes submitted work; malformed requests don't crash the service.
5. Resume a recorded session after restart, including prior task, tool messages, verification state and pending approvals. Never replay side effects automatically.
6. Configure GitHub repository, label and poll cadence; ingest actual issues and explicitly requested PR tasks, dedupe by stable source id, claim durably, recover interrupted jobs, run each in a Git worktree, check and produce local commits/patch reports.
7. Per-repository shipping permissions independently authorize remote replies, branch push, PR creation, merge and releases. Default is local-only. No remote mutations are authorized by this spec alone.
8. Unified verification supports bounded command/test checks and differential replay; unfinished/unverified work never ships. Model reviewer is fresh and separate from author and uses actual diff/evidence.
9. Context budgets trigger recorded compaction preserving user constraints, tool-call/result pairing and recoverable artifact references; skills are discovered cheaply and loaded on demand. Include per-project playbook and preferences.
10. Callable isolated subagents get explicit roles/tasks/worktrees, own context, bounded concurrency and output; no shared dirty writes. Independent reviewer has a read-only boundary.
11. Improvement uses failures, papercuts, tokens, deletion controls, same-model matched comparisons, protected train/dev/final holdout splits and durable lessons. Heldout checks are not shown to proposer or solver, and don't become iterative optimization input. Failed confirmation restores original config.
12. Install runnable binary and packaged playbooks with portable XDG/env configuration/state paths; no compile-time checkout path dependency. Provide doctor/config help and local service/release packaging.
13. Use OS isolation once per command/run where configured: workspace writable, host read-only, credentials withheld, network explicit. Worktree isn't a sandbox. Default interactive trusted workspace mode is clearly documented; unattended execution uses isolation and bounded execution.
14. Responsive output, bounded memory/process lifecycle, no blanket per-call model approval classifier. Measure startup/stream/cancel latency rather than assert arbitrary figures.

## Decisions
Retain Rust and existing small modules; use well-maintained crates/native bwrap/gh instead of hand-written protocols/parsers when practical. Own code plus magpie gateway; don't fork omp/Codex. Local Markdown tracker (no remote issues created), integration branch with per-slice worktrees. Max four workers total. No leaked Claude source or marketing counts used as quality proof.

## Verification
Existing public seams: binary CLI, stdio protocol, actual process runner, gateway response parser, evaluation result schema. Add consumer-visible regression cases at these seams and integration fixtures with fake gateway/gh for deterministic errors. Actual terminal session and real magpie smoke are required. Network publishing/merge/release verified through local command recorder unless explicitly authorized for a real repository. Test cases must not be weakened to obtain green.

## Out of scope
A web platform, multi-tenant SaaS, mobile app, model training and unrestricted self-modification of judges. Shipping code supports authorized operation but no external publication is performed now.
