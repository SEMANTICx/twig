---
name: twig
description: >
  Complete version-matched reference for the twig coding-agent harness: CLI
  subcommands, headless exec, session orchestration, external CLI subagent
  bridges (codex/agy/claude/gemini), verify oracle replay, RCI, hooks, and web cockpit.
---

# twig Reference Guide

`twig` is a single-binary Rust coding-agent harness with tamper-evident BLAKE3
session ledgers, isolated Git worktree subagents, clean-room differential
verification, Recursive Collective Intelligence (RCI), Ratatui TUI, and Web Cockpit.

## 1. CLI Subcommands

```text
twig chat [repo] [sid] [--plain]               # Interactive borderless TUI
twig web [--port 3426] [--bind 127.0.0.1]      # Browser & mobile cockpit (--repo DIR --token TOK)
twig exec [flags] "<prompt>"                   # Headless single-shot turn (exit 0=final, 2=approval, 1=err)
  --repo/-C DIR   --role <code|plan|advisor|reviewer|scout>
  --model/-m ID   --effort <off|low|medium|high|xhigh>
  --resume/-r SID --read-only / --plan   --check "CMD"
  --json          # Emit final JSON summary ({sid, done, reason, text, verified, diff_stat, usage})
  --events        # Stream live JSONL events + final exec.completed event

twig session <subcommand>                      # Multi-session lifecycle & orchestration
  list [--all] [--repo DIR] [--json]           # List sessions ([▲ wait] [● run] [● diff] [✓ ok])
  search <QUERY> [--all] [--json]              # Keyword search across session & subagent transcripts
  show <SID>                                   # Print formatted transcript (also read(path="session:<sid>"))
  spawn [exec_flags...] "<prompt>"             # Start a new headless session (--json)
  send <SID> "<prompt>"                        # Send follow-up turn to an existing session
  diff <SID>                                   # Print saved subagent patch or uncommitted worktree diff
  apply <SID> [--repo DIR]                     # Confined git apply of a subagent/session patch
  archive <SID>                                # Move <SID>.jsonl into sessions/archive/
  gc                                           # Prune 0-byte sessions & clean subagent worktrees

twig verify [record] ...                       # Clean-room 64-bit-exact black-box differential gate
  twig verify record <journal.jsonl> <oracle_bin> [args...] [--raw] [--inputs seeds.jsonl] [--stdin TXT] [--arg-case "FLAGS"]
  twig verify <journal.jsonl> <candidate_bin> [args...] [--raw] [--ignore k1,k2] [--min-cases N] [--min-branch 0..1] [--json]

twig subagent [repo] <role> "<task>"           # Run an isolated role subagent (scout/advisor/reviewer/code/test)
twig review [repo] <diff_file> [evidence]      # Fresh read-only two-tier diff review ({approved, blockers, suggestions})
twig skill <list|enable|disable|add|doctor>    # 3-tier skills (.twig/skills > global > zero-token cold vault)
twig update [--check] [--force]                # Self-update binary from SEMANTICx/twig-dist with SHA-256 verification
twig doctor [--selfcheck | --record <f.jsonl>] # Environment diagnostics, offline checks, or BLAKE3 ledger verification
twig eval [run|ladder|rci|classify] ...        # Isolated worktree benchmark suite & RCI collective grading
twig improve [dir] [rounds]                    # Deletion-ablation gated HarnessConfig optimizer
twig subagent [repo] <role> "<task>"           # Isolated role subagent · `twig review [repo] <diff_file>`
twig guide                                     # Print this version-matched reference guide
```

## 2. Built-in Agent Tools (Inside `twig`)

- **File & Code Intelligence** (worktree-confined, exact `(mtime_ns, len)` Read-Before-Edit guard):
  - `read(path, start_line?, limit?)`: supports repo files, binary/ELF/PNG headers, `.har` distillation, `artifact:<name>`, and `session:<sid>`.
  - `edit(path, old_string, new_string, replace_all?)`: 4-level progressive matching.
  - `write(path, content)` / `apply_patch(patch)`: atomic multi-file `*** Begin Patch` (`Add`/`Update`/`Delete`), reversible via `/undo`.
  - `grep(pattern, path?, glob?, ignore_case?)` / `glob(pattern, path?)`: secret-filtered workspace search.
  - `outline(path, symbol?, mode?)`: `symbols` (default), `xrefs` (repo references), `deps` (imports).
- **Execution & Background Tasks**:
  - `bash(command, run_in_background?, task_action?, task_id?)`: 3-tier policy (`Auto` parallel read-only, `Audit` normal dev commands, `Deny` destructive/secret/control-plane human approval). Manage async tasks via `task_action="list"|"output"|"cancel"`.
- **Multi-Agent & Extensibility**:
  - `subagent(role, task, model?, slice?, tasks?)`: fan out parallel isolated Git-worktree subagents (`bwrap --ro-bind` on Linux for RO roles). Automatically populates the RCI Split-Fact Blackboard (`path:line` facts).
  - `todo(items)` / `skill(name, file?)` / `mcp(action, server?, tool?, arguments?)` / `papercut(component, symptom, trigger)`.

## 3. External CLI Subagent & Role Routing (`codex`, `agy`, `claude`, `gemini`)

Any role (`code`, `plan`, `advisor`, `reviewer`, `scout`) or `subagent(..., model=...)` accepts OpenAI-gateway models OR local CLI engines:
- `cli:codex` / `cli:codex:<model>` -> `codex exec` (`--sandbox read-only` vs `--full-auto` in isolated worktree)
- `cli:agy` / `cli:agy:<model>` -> Antigravity `agy -p` (`--mode plan` vs isolated worktree)
- `cli:claude` / `cli:claude:<model>` -> `claude -p` (`--permission-mode plan` vs isolated worktree)
- `cli:gemini` / `cli:gemini:<model>` -> `gemini -p`
Configure persistently with `/roles <role> <model> [effort]` or `TWIG_ROLES='{"reviewer":"cli:codex","advisor":"cli:claude"}'`.

## 4. Repository Hooks & Configuration (`.twig/`)

- `AGENTS.md` (else `CLAUDE.md`) + `.twig/preferences.md` (`<= 2,400` chars) + `.twig/rules/<claude|gpt|gemini|deepseek>.md`
- `.twig/project.json`:
  - `"post_edit_check"`: fast lint/typecheck run automatically after `edit`/`write`/`apply_patch`
  - `"check_on_change"`: acceptance test run at turn end when files changed
  - `"check"`: mandatory turn-end gate
  - `"stop_hook"`: exit `2` blocks completion and feeds diagnostics back to the model
- `.twig/mcp.json`: lazy stdio MCP servers (`{"mcpServers": {...}}`)
- `.twig/worktree_setup.sh`: executed automatically when `/worktree <slug>` creates a sibling worktree

## 5. Interactive TUI (`twig chat`) & Key Workflows

- **Prefixes**: `/` commands · `@file#L10-50` / `@diff` / `@staged` / `@last` / `@img.png` · `$skill` · `!shell` (0 tokens) · `#memory`
- **Autonomous & Review Loops**:
  - `/goal <objective>`: autonomous `todo`-tracked closed-loop execution + automatic blocker-gated `/review --fix`
  - `/review [--fix]`: fresh read-only review; `--fix` repairs concrete `blockers` (max 2 rounds) and ignores non-blocking `suggestions`
  - `/plan` (`Shift+Tab` cycles `Auto` ⇄ `Plan` ⇄ `Review Edits`) → `/go` · `/dual-plan <goal>` · `/study <target>` · `/evolve`
- **Navigation & Links**: Click or `Ctrl+Click` any URL, `[n]` link, or `path/to/file.rs:42` to open in browser or jump to exact line in `Cursor`/`VS Code`/`Zed` (`TWIG_FILE_OPENER=cursor|vscode|zed|system`), or run `/open [n|target]`. Drag to highlight & copy CJK/ASCII text.

## 6. Studio Web & Mobile Cockpit (`twig web`)

- **3-Pane Studio Workspace** (`127.0.0.1:3426` by default; auto-generates a Bearer token on non-loopback `--bind`):
  - **Brand-Authentic System-Adaptive Themes**: 4 palettes (`Claude` Terracotta/Cream, `Codex` Neutral Monochrome/Emerald, `Studio` Linear Indigo, `Obsidian` Pure OLED) × `System / Dark / Light` with live `prefers-color-scheme` auto-switching.
  - **Projects Switcher & Scheduled Routines**: Switch or add local Git workspaces in the sidebar (`~/.config/twig/projects.json`), and schedule recurring interval routines (`~/.config/twig/schedules.json`) alongside the durable GitHub Issue/PR queue under `Threads | Routines`.
  - **Live SSE Streaming, Execution Modes & Mid-Turn Steer**: Powered by `twig serve` stdio JSON-RPC (`/api/events` + `/api/steer`), `Shift+Tab` 3-state execution mode (`Full Auto` ⇄ `Review Edits` ⇄ `Plan Only`), approval guidance notes, and persistent per-thread drafts.
  - **Checkpoint Time-Travel, Intraline Diffs & Customize Packs**: Per-turn `↺ Restore` (`GIT_INDEX_FILE`), word-level intraline diffs + per-file `↺ Discard`, versioned `Artifacts Preview` (`1/N` + `⤢ Wide`), and 6-tab Studio Settings (`Customize & Packs`, `Models & Roles`, `Runtime & Hooks`, `Skills & Vault`, `MCP & Memory`).
