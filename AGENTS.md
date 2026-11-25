# AGENTS.md — twig

`twig` is a thin, high-discipline coding-agent harness written in Rust.
**magpie provides the models; twig provides the loop, tools, policy, verifier, TUI, and tamper-evident record.**

## Architecture

- `src/main.rs` — CLI entrypoint (`chat`, `serve`, `selfcheck`, `doctor`, `config`, `skill`, `eval`, `improve`, `automate`, `subagent`, `review`, `record`, `classify`, `playbooks`, `decisions`).
- `src/config.rs` — `HarnessConfig` (`harness.json` tunable surface + XDG paths). Optimizer may only touch `harness.json`, never source code or judges.
- `src/llm/` — OpenAI-compatible SSE client (`Provider`), role routing (`Roles` with `scout→filter`, `reviewer→judge` fallbacks), reasoning effort, usage/cache tracking.
- `src/runtime/` — Core daemon & execution loop:
  - `mod.rs` — JSON-RPC stdio server (`Daemon`), session recovery, `turn`, `/rewind`, `/fork`, `skill.manage`, `mcp.list`, `tasks.list`, `mode.set`.
  - `turn.rs` — Single-round-trip multi-tool turn loop + mid-turn steering + repetition breaker.
  - `tools.rs` — Worktree-confined structured tools (`read`, `edit`, `write`, `grep`, `glob`, `bash`, `task`, `subagent`, `skill`, `mcp`, `todo`, `papercut`) + `artifact:<file>` side-channel.
  - `guidance.rs` — Playbooks, progressive-disclosure `SkillIndex` (Top-5 local pre-ranker when >15 skills), and 3-tier `SkillStore` (`.twig/skills` > `~/.config/twig/skills`, cold vault `~/.local/share/twig-skills-vault`), plus `$skill-name` zero-RTT invocation.
  - `policy.rs` — 3-tier shell classifier (`Auto` / `Audit` / `Deny`) with shape caching.
  - `guard.rs` — Progress/stall detector (warns, narrows to read-only, aborts) and failing-call loop breaker.
  - `context.rs` — Lossless context compaction preserving constraints, valid tool pairs, and `artifact:tool-*.txt` refs.
  - `subagent.rs`, `tasks.rs`, `mcp.rs` — Parallel isolated Git worktree subagents, async background shell tasks, and MCP stdio client (tools, prompts, resources).
- `src/exec/` — Bounded Unix-socket shell runner + optional Linux `bwrap` namespace isolation.
- `src/tui/` — Borderless warm-terracotta Ratatui TUI (`mod.rs`, `client.rs`, `composer.rs`, `transcript.rs`, `markdown.rs`).
- `src/eval/`, `src/verify.rs`, `src/record.rs`, `src/automation/` — Evaluation suite, differential verifier, BLAKE3 hash-chained JSONL journal, unattended GitHub automation.

## TUI Design Rules

- **Zero 4-sided box borders**: pure typography, hairline rules (`─`), full-width accent selection bar, warm terracotta palette.
- **Zero redundancy across Welcome / Composer / Footer**:
  - Welcome header shows repo path + live loaded environment summary (`skills`, `instructions`, `mcp`, `sandbox`).
  - Composer placeholder owns input prefixes (`/ command · @ file · $ skill · ! shell`).
  - Footer owns live dynamic state (`repo (branch) +N -M · model effort · ctx % · tokens · badges`).

## Quality Gates (Run Before Every Commit)

1. **Zero unnecessary Cargo dependencies** — keep the crate lean.
2. **Design & hygiene lint**: `deslop-please` MUST output `deslop-please: ok` (no allowlist workarounds, every file `< 2000` lines — watch `src/tui/mod.rs`, no field leaks, no module cycles).
3. **Compiler lint**: `cargo clippy --all-targets -- -D warnings` (0 warnings).
4. **Offline smoke check**: `cargo run --quiet -- selfcheck` (`all checks passed`).
5. **Full test suite**: `cargo test --no-fail-fast` (5 tests exercise nested Linux `bwrap` user namespaces and require host namespace capability).
6. **Release install** (when shipping locally): `cargo build --release && install -Dm755 target/release/twig ~/.local/bin/twig`.
