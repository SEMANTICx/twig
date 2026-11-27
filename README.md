# ✻ twig

**A high-discipline, gateway-neutral coding-agent harness in pure Rust — combining a Claude Code-caliber borderless TUI, tamper-evident BLAKE3 session ledgers, clean-room differential verification, and Recursive Collective Intelligence (RCI).**

[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-D77757)](#platform-support)
[![Rust](https://img.shields.io/badge/rust-static%20single%20binary-DEA584)](#architecture)
[![License: MIT](https://img.shields.io/badge/license-MIT-80C08A)](LICENSE)

> **Bring any model, keep ironclad engineering discipline.** Point `twig` at any OpenAI-compatible gateway (default: `magpie`), route heterogeneous models per role (`code`, `plan`, `advisor`, `reviewer`, `scout`), or bridge local `cli:claude` / `cli:codex` / `cli:gemini` subscriptions directly inside isolated Git worktrees.

---

## Contents

1. [Why `twig`?](#why-twig)
2. [How `twig` Compares (`twig` vs `Claude Code` vs `Codex CLI`)](#how-twig-compares)
3. [Installation](#installation)
4. [Quick Start](#quick-start)
5. [Interactive TUI (`twig chat`)](#interactive-tui-twig-chat)
6. [Recursive Collective Intelligence (RCI) & Multi-Agent Orchestration](#recursive-collective-intelligence-rci--multi-agent-orchestration)
7. [Verification, Lossless Compaction & Self-Improvement](#verification-lossless-compaction--self-improvement)
8. [3-Tier Skill System & MCP Integration](#3-tier-skill-system--mcp-integration)
9. [Architecture](#architecture)
10. [CLI Reference & Environment Variables](#cli-reference--environment-variables)

---

## Why `twig`?

Most coding agents suffer from three structural failure modes: **vendor lock-in**, **unverified model claims** ("tests pass!" when nothing ran), and **shared-context multi-agent token waste**. `twig` solves all three in a single zero-dependency static binary (`<15ms` cold start, `~10 MB` RSS):

- **Claude Code-Caliber Terminal UX + Scientific Rendering** — Distinct full-width request cards, `◆` assistant replies, `Ctrl+O` result-focused tool folding, unfolded `● Update(file) +N -M` cards with **intra-line word-level diffs**, rich approval diff previews with inline rejection feedback (`Tab`), `Alt+Enter` queued follow-up pills (`↑` pops back to edit), `Ctrl+B` live backgrounding, `Ctrl+T`/`↓` interactive subagent/task drawer, `/btw` floating side-cards, **plus native terminal GFM box tables, Unicode LaTeX math (`$...$` / `$$...$$`), and Mermaid/SVG diagrams**.
- **Gateway-Neutral + Native CLI Bridges** — Mix and match OpenAI-compatible endpoints (`magpie`, vLLM, OpenRouter, LiteLLM) with local CLI engines (`cli:claude`, `cli:codex`, `cli:gemini`) per role (`code`, `plan`, `advisor`, `reviewer`, `scout`).
- **Two-Stage Lossless Context Compaction** — Stage 1 deterministically offloads large older tool outputs to `artifact:tool-<id>.txt` with zero LLM calls; Stage 2 folds rolling history into a bounded summary while archiving full verbatim JSON (`context-<blake3>.json`) so the agent can paginate any historical line via `read(path="artifact:...", start_line=...)` even after 200+ tool calls.
- **Evidence Over Claims (`twig verify` & `Record`)** — Every session is written to a **BLAKE3 hash-chained JSONL ledger** (`twig record`), gated by project `post_edit_check` / `check_on_change` hooks, Stop Hooks (`exit 2` blocks completion and feeds diagnostics back), and clean-room black-box differential oracle replay (`twig verify`).
- **Recursive Collective Intelligence (RCI)** — Implements Stanford/Berkeley/NVIDIA RCI principles (`arXiv:2611.32606`): disjoint `slice` assignment + **Split-Fact Blackboard** across parallel Git-worktree subagents, `.twig/notes/<role>.md` protocol cards, **Equal-Budget Ladder** grading (`twig ladder` across `Level 0..=3`, $V(n_B)$, $\text{pass@}n_B$), and **label-free gated recursion** (`twig rci`).

---

## How `twig` Compares

| Capability | `twig` | `Claude Code CLI` | `Codex CLI` |
|---|---|---|---|
| **Runtime Footprint** | **Single static Rust binary** (`musl`, `<15ms` boot, `~10 MB` RAM) | Node.js + Ink React TUI (~100+ MB RAM) | Rust TUI (Apache-2.0) |
| **Model & Vendor Freedom** | **Any OpenAI gateway + per-role routing + `cli:claude`/`cli:codex`/`cli:gemini` bridges** | Coupled to Anthropic Messages protocol | Coupled to OpenAI Responses API |
| **Terminal Rich Rendering** | **Word-level diffs + CJK-aligned GFM tables + LaTeX math + Mermaid/SVG box diagrams** + built-in `twig web` cockpit | Word-level diffs & polished TUI; raw LaTeX / Mermaid source | Clean minimal patch TUI; raw LaTeX / Mermaid source |
| **Context Compaction** | **2-stage lossless** (`artifact:tool-*.txt` addressable line-by-line + folded summary) | LLM `/compact` summary + prompt cache | Responses API state chaining + truncation |
| **Multi-Agent Discipline** | **Isolated Git worktrees + Split-Fact Blackboard + RCI Equal-Budget Ladder (`pass@n_B`)** | Subagents + background tasks | Cloud tasks / local sub-runs |
| **Audit & Verification** | **BLAKE3 tamper-evident chain + black-box `twig verify` + ablation-gated `twig improve`** | Permissions + Hooks | Kernel Landlock / Seatbelt sandbox |

---

## Installation

### 1. One-Line Prebuilt Installer (No Login Required)

**Linux & macOS:**
```bash
curl -fsSL https://raw.githubusercontent.com/SEMANTICx/twig-dist/main/install.sh | sh
```

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/SEMANTICx/twig-dist/main/install.ps1 | iex
```

> Verifies SHA-256 checksums against `SHA256SUMS.txt` on [`SEMANTICx/twig-dist`](https://github.com/SEMANTICx/twig-dist) and installs to `~/.local/bin/twig` (or `%LOCALAPPDATA%\twig\bin`). Pin a release via `TWIG_VERSION=v0.2.0`.

### 2. Build from Source (Private / Dev Checkout)

```bash
git clone https://github.com/SEMANTICx/twig.git
cd twig
cargo build --release --locked
install -Dm755 target/release/twig ~/.local/bin/twig
```

### Platform Support

| Platform | TUI & Core Loop | Shell & Process Cleanup | Clipboard (Text / Image) | Worktree & OS Sandbox |
|---|---|---|---|---|
| **Linux** (`x86_64` static `musl`, `aarch64`) | Full | `sh -c`, `waitid` pgid tree reaper | OSC 52 + `wl-copy` / `wl-paste` | Git worktree + `bwrap` namespace isolation |
| **macOS** (`arm64`, `x86_64`) | Full | `sh -c`, process-group cleanup | OSC 52 + `pbcopy` / `pngpaste` | Git worktree + read-only tool guard |
| **Windows** (`x86_64-msvc`) | Full (Windows Terminal) | `cmd /C`, `taskkill /T /F` | OSC 52 + `clip` / PowerShell | Git worktree + read-only tool guard |
| **Mobile SSH / Termux** | Responsive down to `32 cols` + `twig web` | Host shell | OSC 52 | Host isolation |

---

## Quick Start

```bash
# 1. Check gateway connectivity, sandbox, active role routing, and skill health
twig doctor

# 2. Run deterministic offline harness self-checks (zero network/LLM calls)
twig selfcheck

# 3. Start the interactive TUI inside any project
cd /path/to/your/repo
twig chat

# 4. Launch the mobile/desktop browser cockpit (live SSE + inline SVG/math + Bearer auth)
twig web --port 3426

# 5. Run headless in CI or scripts with an external acceptance gate
twig exec --check "cargo test --quiet" --json "Fix off-by-one bounds bug in parser"
```

---

## Interactive TUI (`twig chat`)

```text
  ✻ twig 0.2.0  ~/Dev/twig
    skills 10 (+13 vault)  ·  docs AGENTS.md  ·  mcp 0  ·  sandbox bwrap

    ▎
    ▎  ❯ Audit context compaction and fix budget overflow on 200-tool runs
    ▎

    ▸ 14 tools (9 read · 3 search · 2 bash)  3s · ^O expand

    ● Update(src/runtime/context.rs)  +42 -11  0s
      ⎿  @@ -291,6 +291,8 @@
          294   let t = s.trim_start();
          295 - t.starts_with("<system-reminder>")
          295 + t.starts_with("<system-reminder>") || t.starts_with("Compacted history")

    ◆ Folded prior summaries into a single rolling summary and capped artifact refs.

    ↳ Queued  Run full test suite next   (↑ edit · /unqueue)
    ✻ Synthesizing  (12s · queued:1 · esc interrupt · ^B bg · enter steer · alt+enter queue)
    ─────────────────────────────────────────────────────────────────────────────────────────
    ❯ Type steering guidance and press Enter (or Esc to interrupt)…
    ─────────────────────────────────────────────────────────────────────────────────────────
    ◈ Subagents  ·  2 running · 1 done  (↓ / ^T inspect)
      ✦  reviewer*   src/runtime/turn.rs — verify budget synthesis turn  8s
      ✦  scout*      src/tui/overlays.rs — check drawer keybindings      5s
    ✻ twig (main) +42 -11 · gpt-5.4 high · ctx 38% · 48.2k tok · cache 81%   ^O tools · Shift+Tab · ^T · ?
```

### Composer Modes & Inline Prefixes

| Prefix | Visual Mode & Behavior |
|---|---|
| *(plain text)* | **Chat / Steer Mode (`❯`)** — Sends a new turn when idle, or injects live mid-turn **steering guidance** (`Enter`) while running |
| `/` | **Slash Palette** — Fuzzy-filtered command menu (`Enter` executes zero-arg commands immediately; `Tab` autocompletes) |
| `@` | **Context & Line-Slice Mention** — `@src/lib.rs`, **exact line slices `@src/lib.rs#L10-50` (`:10-50`)**, inline images `@shot.png`, or live git/shell state (`@diff`, `@staged`, `@last`) |
| `$` | **Zero-RTT Skill Invocation** — Autocompletes active & cold-vault skills (`$karpathy-guidelines`), injecting the skill body on send |
| `!` | **Cyan Local Bash Mode (`$`)** — Morphs composer to cyan `! bash mode`; executes locally in `0` tokens and exposes output to `@last` |
| `#` | **Lavender Memory Mode (`♯`)** — Morphs composer to `# memory mode`; `Enter` opens a 1-key picker to append the rule to `.twig/preferences.md` (repo) or `~/.config/twig/preferences.md` (global) |

### Keyboard Workflow & Slash Commands

| Shortcut / Command | Action |
|---|---|
| `Shift+Tab` | **3-Mode Permission Cycle**: `Auto-Accept Edits` (default) ⇄ `◈ Plan Mode` (read-only) ⇄ `🔒 Review Edits` (diff approval before every edit) |
| `Ctrl+O` / `/expand` | Toggle **Result-Focused Folding** (collapses finished exploration bursts & intermediate thoughts; keeps Requests, `Update` diffs & Replies) |
| `Alt+Enter` / `↑` | **Queue Follow-Up Prompt** while busy (`↳ Queued` pill above composer); press **`↑` in an empty composer** to pop it back for editing |
| `Ctrl+B` | **Detach to Background**: detaches a slow foreground `bash` command into an async session background process without killing it |
| `Ctrl+T` / `↓` | **Subagents & Background Tasks Drawer**: inspect live subagent tasks/errors or background shell tails (`Enter`), or press **`x`** to kill a task |
| `Tab` / `3` *(in Approval)* | **Inline Rejection Feedback**: type instructions directly on Option 3 (`Tell twig instead: ...`) so the model pivots immediately |
| `/btw <question>` | **Floating Ephemeral Side-Card**: answers above the composer reusing KV cache with zero history/transcript pollution (`Esc`/`Space` dismisses) |
| `Esc×2` / `/rewind` | Interactive turn rewind (`Enter` rewinds chat only; `r` / `--code` restores exact Git worktree tree snapshot; `f` forks session) |
| `/plan` → `/go [note]` | Explore & design read-only on the `plan` model, then `/go` to restore write access and execute on the `code` model |
| `/dual-plan <goal>` | Blind parallel tool-verified planning across `plan` × `advisor` models followed by a synthesized consensus brief |
| `/diff` / `/review` / `/walkthrough` | Line-numbered diff / fresh read-only `blockers` vs `suggestions` review (`xhigh` on sensitive paths) / logical architecture walkthrough |
| `/commit [hint]` / `/pr` / `/worktree` | Repo-style smart commit (zero AI trailers) / push & open PR / live mid-session handoff to an isolated Git worktree |
| `Ctrl+F` / `Ctrl+R` / `Ctrl+G` | Case-folding CJK-safe transcript search / fuzzy prompt history picker / compose draft in `$EDITOR` |
| `Alt+Y` / `Alt+Shift+Y` / `Alt+V` / `Alt+M` | Cycle-copy code blocks / copy full reply / attach clipboard screenshot / toggle mouse capture for native text selection |

---

## Recursive Collective Intelligence (RCI) & Multi-Agent Orchestration

Inspired by **Recursive Collective Intelligence** (*He et al., Stanford / UC Berkeley / NVIDIA, arXiv:2611.32606*), `twig` treats multi-agent collaboration as a rigorous budget-accounted engineering discipline rather than unstructured roleplay:

### 1. Why Shared-Context Debate Wastes Tokens — and How `twig` Fixes It
When 4 agents read the same context and debate conclusions, accuracy stays at or below equal-budget majority vote ($V(n_B)$) while burning $3\text{–}6\times$ the tokens. Genuine **Level 3 Collective Emergence** ($P_{\text{team}} > \text{pass@}n_B$) happens when agents inspect **disjoint slices** and forward **verbatim `path:line` facts** onto a shared blackboard:

- **Disjoint `slice` + `Blackboard` (`subagent` tool & `/operator`)**:
  Parallel `subagent` calls take an explicit `slice` (e.g. `"src/runtime/*"` vs `"src/tui/*"`) in isolated Git worktrees (`--ro-bind` in `bwrap` for `scout`/`advisor`/`reviewer`). Decisive `path:line` & `[Observed]` facts are automatically harvested onto the session's **RCI Split-Fact Blackboard** (`/blackboard`) and forwarded to downstream waves.
- **Role Protocol Cards (`.twig/notes/<role>.md`)**:
  Compact (`<= 1,200` chars) per-role protocol cards (`scout`, `code`, `test`, `advisor`, `reviewer`) are injected automatically into subagents — zero prompt cost when absent, cutting coordination overhead by >40% when active.

### 2. Equal-Budget Ladder (`twig ladder`) & Label-Free Gated Recursion (`twig rci`)

```bash
# 1. Run solo baseline (4 reps) and collective team on the same eval suite
twig eval evals 4 > solo_log.txt
twig ladder solo_results.jsonl team_results.jsonl
```

`twig ladder` grades collective runs against solo runs at matched completion-token budget $n_B = B_{\text{team}} / B_{\text{solo}}$:
- **Level 0**: $P_{\text{team}} \le P_1$ *(net coordination tax)*
- **Level 1**: $P_1 < P_{\text{team}} \le V(n_B)$ *(beats 1 agent, within majority vote)*
- **Level 2**: $V(n_B) < P_{\text{team}} \le \text{pass@}n_B$ *(beats vote, within independent oracle selection)*
- **Level 3**: $P_{\text{team}} > \text{pass@}n_B$ *(genuine collective emergence + equivalent solo budget savings $n_{\text{eq}}$ & exact sign-test $p$-value)*

```bash
# 2. Harvest ONLY BLAKE3-valid, auto-verified collective sessions (zero human labels)
twig rci harvest

# 3. Distill anti-overfit-checked role cards into .twig/notes/{scout,code,test,advisor,reviewer}.md
twig rci distill --online

# 4. Gate generation promotion (requires 0 regressions on solved tasks + accuracy or >=15% budget win)
twig rci gate solo_results.jsonl gen0_team.jsonl gen1_team.jsonl
```

---

## Verification, Lossless Compaction & Self-Improvement

- **Clean-Room Differential Replay (`twig verify`)**:
  Record I/O behavior from a reference binary or legacy script (`twig verify record journal.jsonl ./legacy_bin --raw`), then wire `--journal journal.jsonl` into `twig` so the agent cannot finish until the candidate binary matches every recorded case.
- **Protected `HarnessConfig` Optimizer (`twig improve`)**:
  Splits tasks into `train` and unreachable `heldout`, diagnoses failure modes via contrastive variance, runs **deletion ablations** so dead prompt prose is never added, blocks task-token leakage (`lint`), and promotes changes only when heldout gates pass without regressions.
- **Three-Tier Shell Policy (`src/runtime/policy.rs`)**:
  Zero-latency AST/token classifier: `Auto` (shape-cached read-only commands fan out in parallel), `Audit` (ordinary build/test commands run immediately and log to ledger), and `Deny` (destructive shapes like `rm -rf /`, `git push --force`, `git reset --hard`, or `.env`/`.twig` control-plane writes pause for explicit human approval).

---

## 3-Tier Skill System & MCP Integration

```bash
twig skill list                         # Show Project (.twig/skills), Global Active, and Cold Vault skills
twig skill enable sharp-edges           # Activate a vault skill globally
twig skill enable my-skill --project    # Scope a skill to the current repository
twig skill add ./path/to/skill --enable # Import a SKILL.md bundle into the vault
twig skill doctor                       # Enforce frontmatter, <=500-line cap, and shadow checks
```

- **Cold Vault = Zero Token Tax**: Keep 50+ specialized skills in `~/.local/share/twig-skills-vault` at `0` system-prompt tokens; invoke any vault skill on the fly with `$skill-name` in `twig chat` or `skill(name="...")`.
- **Lazy Stdio MCP (`mcp` tool & `/mcp`)**: Configure servers in `.twig/mcp.json` or `~/.config/twig/mcp.json`; toggle live with `/mcp enable <name>` / `/mcp disable <name>`.

---

## Architecture

`twig` enforces a strict **acyclic 7-layer DAG** verified on every build by `deslop-please` (zero circular dependencies, `<2,000` lines per file, narrow module interfaces, zero external bloat crates):

```text
  ┌─────────────────────────────────────────────────────────────────────┐
  │ Interfaces : tui (Ratatui chat) · web (HTTP+SSE cockpit) · automate │
  ├─────────────────────────────────────────────────────────────────────┤
  │ Evaluation : eval (runner, improve, ablation, trajectory, rci)      │
  ├─────────────────────────────────────────────────────────────────────┤
  │ Runtime    : daemon · turn · tools · subagent · context · policy    │
  ├─────────────────────────────────────────────────────────────────────┤
  │ Verify     : differential oracle replay & branch-coverage gates     │
  ├─────────────────────────────────────────────────────────────────────┤
  │ Model      : llm (OpenAI SSE, role router, cli:claude/codex/gemini) │
  ├─────────────────────────────────────────────────────────────────────┤
  │ Execution  : exec (process-group reaper, PTY/pipes, Linux bwrap)    │
  ├─────────────────────────────────────────────────────────────────────┤
  │ Foundation : config · record (BLAKE3 hash chain + flock) · control  │
  └─────────────────────────────────────────────────────────────────────┘
```

---

## CLI Reference & Environment Variables

### Commands

```text
twig chat [repo] [sid] [--plain]                 Interactive borderless TUI
twig web [--port 3426] [--bind 127.0.0.1]        Browser & mobile cockpit
twig exec [flags] "<prompt>"                     Headless single-shot execution (--role/--model/--check/--json/--events)
twig session <list|search|show|spawn|send|diff|apply|archive|gc>
twig verify [record] <journal.jsonl> <program>   Black-box differential oracle recording & gating
twig eval [dir] [reps] [only]                    Isolated worktree benchmark runner
twig ladder <solo.jsonl> <team.jsonl> [--json]   RCI Equal-Budget Ladder grader (Levels 0..3)
twig rci <status|harvest|distill|gate>           Label-free role-card recursion & release gating
twig improve [dir] [rounds]                      Ablation-controlled harness.json optimizer
twig skill <list|enable|disable|add|doctor>      3-tier skill manager
twig review [repo] <diff_file>                   Independent two-tier read-only diff reviewer
twig subagent [repo] [role] "<task>"             Run an isolated role subagent
twig automate [automation.json] [--once]         Unattended GitHub issue/PR maintenance loop
twig record <session.jsonl>                      Verify BLAKE3 session hash-chain integrity
twig doctor / twig selfcheck / twig config [init]
```

### Environment Variables

| Variable | Default | Purpose |
|---|---|---|
| `TWIG_BASE_URL` | `http://127.0.0.1:3425/v1` | OpenAI-compatible gateway endpoint |
| `TWIG_API_KEY` | `magpie` | Gateway Bearer API key |
| `TWIG_MODEL` | *(saved `/model`)* | Override active model (`cli:claude`, `cli:codex`, `cli:gemini`, or gateway model ID) |
| `TWIG_ROLES` | *(saved `/roles`)* | JSON per-role map (`code`, `plan`, `advisor`, `reviewer`, `scout`) |
| `TWIG_EFFORT` | *(saved `/effort`)* | Reasoning effort (`off`, `low`, `medium`, `high`, `xhigh`) |
| `TWIG_CONFIG` | `~/.config/twig/harness.json` | Custom `HarnessConfig` JSON path |
| `TWIG_SKILLS_DIR` / `TWIG_SKILLS_VAULT` | XDG / AppData defaults | Active global skills dir / zero-cost cold vault dir |
| `TWIG_WORKERS` / `TWIG_WALL_SECS` | `4` / `300` | Parallelism & wall-clock timeout for `twig eval` |
| `TWIG_REDUCED_MOTION` | `0` | Set `1` to disable smooth scroll & breathing animations |

---

## Quality Gates

```bash
deslop-please                                # Enforce strict DAG, module depth & zero field leaks
cargo clippy --all-targets -- -D warnings    # Zero compiler or clippy warnings
cargo run --quiet -- selfcheck               # Deterministic offline invariant checks
cargo test --no-fail-fast                    # Full unit + integration test suite
```

## License

[MIT](LICENSE)
