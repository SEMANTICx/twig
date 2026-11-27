# AGENTS.md — twig

`twig` is a thin, high-discipline coding-agent harness written in Rust.
**magpie provides the models; twig provides the loop, tools, policy, verifier, TUI, and tamper-evident record.**

## Architecture

- `src/main.rs` — CLI entrypoint (`chat`, `exec`, `session`, `serve`, `selfcheck`, `doctor`, `config`, `skill`, `eval`, `improve`, `automate`, `subagent`, `review`, `record`, `classify`, `playbooks`, `decisions`).
- `src/config.rs` — `HarnessConfig` (`harness.json` tunable surface + XDG paths + shared `IGNORED_DIRS`). Optimizer may only touch `harness.json`, never source code or judges.
- `src/llm/` — OpenAI-compatible SSE client (`Provider`), persistent per-role model & effort routing (`~/.config/twig/roles.json` for `plan`, `advisor`, `code`, `scout`, `reviewer` with `model.txt` fallback), usage/cache tracking.
- `src/runtime/` — Core daemon & execution loop:
  - `mod.rs` — JSON-RPC stdio server (`Daemon`), frozen system prompt for KV-cache stability + tail `<system-reminder>` injection, `turn`, `/rewind` (chat + Git worktree code), `/fork`, `/btw`, `/operator`, `/roles`, `skill.manage`, `mcp.list`, `tasks.list`, `mode.set`.
  - `session.rs` — Single-pass session recovery, deduplicated repo instruction loading (`AGENTS.md` else `CLAUDE.md`, `.twig/preferences.md`, `.twig/rules/<family>.md`), non-intrusive `GIT_INDEX_FILE` worktree snapshots, repo-scoped session scanner/search, `twig exec` & `twig session` runners, and optional `herdr` status socket broadcasts.
  - `turn.rs` — Single-round-trip multi-tool turn loop + parallel `Tier::Auto` read-only and `subagent` batching + mid-turn steering + repetition breaker.
  - `tools/` (`mod.rs` + `fs.rs`) — Worktree-confined structured tools (`outline` structural symbol map, `read` with line numbers + `artifact:<file>` + `session:<sid>` + lazy subdirectory `AGENTS.md`/`CLAUDE.md`, `edit` with resume-aware Read-Before-Edit `mtime` guard & 4-level progressive indentation matching, `write`, multi-file `apply_patch`, `grep`, `glob`, `bash`, `task` with `send` stdin, resumable `subagent`, `skill`, `mcp`, `todo`, `papercut`) + `.twig/project.json` `post_edit_check` / `check_on_change` hooks.
  - `guidance.rs` — Playbooks, progressive-disclosure `SkillIndex` (Top-5 local pre-ranker when >15 skills), and 3-tier `SkillStore` (`.twig/skills` > `~/.config/twig/skills`, cold vault `~/.local/share/twig-skills-vault`), plus `$skill-name` zero-RTT invocation.
  - `policy.rs` — 3-tier shell classifier (`Auto` / `Audit` / `Deny`) with outside-worktree path downgrade and catastrophic-wildcard prevention.
  - `guard.rs` — Progress/stall detector (warns, narrows to read-only, aborts) and failing-call loop breaker.
  - `context.rs` — Two-stage context compaction: deterministic zero-LLM `Microcompact` (`artifact:tool-<id>.txt`) followed by lossless-archived LLM `Autocompact`.
  - `subagent.rs`, `tasks.rs`, `mcp.rs` — Resumable isolated Git worktree subagents hydrated from parent uncommitted changes (`code`, `test`, `scout`, `advisor`, `reviewer`, plus `cli:codex`/`cli:claude`/`cli:gemini` external CLI bridge, diff receipts & two-tier `/review`), interactive async background tasks, and MCP stdio client.
- `src/exec/` — Bounded Unix-socket shell runner with automatic zombie/FD reclamation + optional Linux `bwrap` namespace isolation.
- `src/tui/` — Borderless warm-terracotta Ratatui TUI (`mod.rs`, `slash.rs`, `overlays.rs`, `client.rs`, `composer.rs`, `transcript.rs`, `markdown.rs`).
- `src/eval/`, `src/verify.rs`, `src/record.rs`, `src/automation/` — Evaluation suite, differential verifier, lazy BLAKE3 hash-chained JSONL journal (zero empty-file clutter), unattended GitHub automation.

## TUI Design Rules

- **Zero 4-sided box borders**: pure typography, hairline rules (`─`), full-width accent selection bar, warm terracotta palette.
- **Zero redundancy across Welcome / Composer / Footer**:
  - Welcome header shows repo path + live loaded environment summary (`skills`, `instructions`, `mcp`, `sandbox`).
  - Composer placeholder owns input prefixes (`/ command · @ file · $ skill · ! shell`).
  - Footer owns live dynamic state (`repo (branch) +N -M · model effort · ctx % · tokens · badges`).

## Quality Gates (Run Before Every Commit)

1. **Zero unnecessary Cargo dependencies** — keep the crate lean.
2. **Design & hygiene lint**: `deslop-please` MUST output `deslop-please: ok` (no allowlist workarounds, every file `< 2000` lines, no field leaks, no shallow modules, no module cycles).
3. **Compiler lint**: `cargo clippy --all-targets -- -D warnings` (0 warnings).
4. **Offline smoke check**: `cargo run --quiet -- selfcheck` (`all checks passed`).
5. **Full test suite**: `cargo test --no-fail-fast` (6 tests exercise nested Linux `bwrap` user namespaces and require host namespace capability).
6. **Release install** (when shipping locally): `cargo build --release && install -Dm755 target/release/twig ~/.local/bin/twig`.
