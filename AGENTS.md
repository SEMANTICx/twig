# AGENTS.md — twig

`twig` is a thin, high-discipline coding-agent harness written in Rust.
**magpie provides the models; twig provides the loop, tools, policy, verifier, TUI, and tamper-evident record.**

## 7-Layer Downward Architecture (Enforced by `tests/architecture.rs`)

1. **L1 · Foundation** (`src/config.rs`, `src/record.rs`, `src/control.rs`) — `HarnessConfig` (`harness.json` tunable surface + XDG paths + `IGNORED_DIRS`), lazy BLAKE3 hash-chained `.jsonl` `Record` (`verify_chain` / `verify_rows`) + kernel `flock` `Lease`, and `Control` cancellation/approval gate.
2. **L2 · Execution & Sandbox** (`src/exec/`) — Bounded Unix-socket shell runner with process-group reaping (`run_process_cancellable`) and Linux `bwrap` namespace isolation (`bwrap_userns_usable`).
3. **L3 · Model & CLI Bridges** (`src/llm/`) — OpenAI-compatible SSE client (`Provider`), typed `ConfigOp`, role routing (`roles.json`), and canonical external CLI runner bridge (`llm::cli` for `cli:codex`/`claude`/`agy`/`gemini`).
4. **L4 · Differential Verifier** (`src/verify.rs`) — Oracle journal equivalence verifier (`gate`).
5. **L5 · Session & Agent Runtime** (`src/runtime/`):
   - `mod.rs` & `rpc.rs` — `Daemon` JSON-RPC dispatcher + stdio framing (`serve_stdio_loop`), frozen system prompt + tail `<system-reminder>` injection.
   - `session.rs` & `git_ops.rs` — Canonical `SessionSnapshot` ledger recovery/scanner/search/archive, `paused` vs crash-interrupted approval distinction, `GIT_INDEX_FILE` snapshots, uncommitted worktree hydration, and confined Git patch application.
   - `turn.rs`, `policy.rs`, `guard.rs`, `context.rs` — Multi-tool turn loop + parallel `Tier::Auto` and `subagent` batching, 3-tier shell policy, progress/stall guard, and 2-stage context compaction.
   - `tools/` (`mod.rs`, `fs.rs`, `code_intel.rs`, `inspect.rs`, `lsp.rs`) — Worktree-confined tools (`outline` symbols/xrefs/deps/LSP, `read` text/binary/Wasm/HAR/artifacts, Read-Before-Edit `edit`, `write`, `apply_patch`, `grep`, `glob`, `bash`, `task`, `subagent`, `skill`, `mcp`, `todo`, `papercut`).
   - `guidance.rs`, `blackboard.rs`, `subagent.rs`, `workflows.rs`, `tasks.rs`, `mcp.rs` — Playbooks & 3-tier `SkillStore`, RCI role-card blackboard primitives, `SubagentConfig::for_repo` & `SubagentPool`, `/review` + `/walkthrough` + `/dual-plan` workflows, async tasks, and MCP stdio registry.
6. **L6 · Evaluation & Automation** (`src/eval/`, `src/automation/`) — Eval suite, ablation checks, RCI session harvest/distillation, and unattended GitHub issue/PR automation.
7. **L7 · Interfaces** (`src/tui/`, `src/web.rs`, `src/cli.rs`, `src/main.rs`) — Borderless warm-terracotta Ratatui TUI (`theme.rs` leaf SSOT, `highlight.rs`, `markdown.rs`, `math_table.rs`, `transcript.rs`, `composer.rs`, `overlays.rs`, `strips.rs`, `selection.rs`, `client.rs`, `slash.rs`, `mod.rs` with `PendingRpc`), mobile/desktop Web Cockpit, headless CLI handlers, and thin `main.rs` dispatcher.

## TUI & Engineering Invariants

- **Single Source of Truth**: Every `Color::Rgb` token, `horizontal_pad`, and cell-width-safe `clip_width` lives exclusively in leaf `src/tui/theme.rs` (zero `super::*` imports, zero 4-sided box borders).
- **Deep Modules**: Every `src/**/*.rs` file stays `<= 1,650` lines (`tests/architecture.rs`) and hides more complexity than it exports (`deslop-please: ok`).

## Quality Gates (Run Before Every Commit)

1. `deslop-please` -> `deslop-please: ok` (zero allowlist workarounds, shallow modules, field leaks, or cycles).
2. `cargo clippy --all-targets -- -D warnings` (0 warnings).
3. `cargo run --quiet -- selfcheck` (`all checks passed`).
4. `cargo test --no-fail-fast` (includes `tests/architecture.rs` and Linux `bwrap` isolation suites).
5. Release install: `cargo build --release && install -Dm755 target/release/twig ~/.local/bin/twig`.
