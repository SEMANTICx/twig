# twig

**A thin, high-discipline coding-agent harness in Rust — cross-platform (Linux, macOS, Windows) with a responsive borderless TUI from phone screens to ultrawide monitors.**

> Any OpenAI-compatible gateway (default: `magpie`) provides the models; **twig** provides the execution loop, structured tools, tiered safety policy, isolated subagents, curated skills, verifier, borderless TUI, and tamper-evident session record.

---

## Why twig?

Most coding agents accumulate hidden latency and prompt bloat: per-call LLM classifiers, repetitive permission popups for harmless reads, full-tree scans on boot, and megabytes of unchecked skill text.

`twig` takes the opposite approach:

- **One round-trip per model turn** — multiple tool calls execute within a single turn, with mid-turn steering (`Enter` while running) and automatic retry on transient upstream errors.
- **Responsive borderless warm-terracotta TUI** — zero 4-sided box borders, adapts cleanly across **phone / Termux / Blink SSH (`36×16`)**, **tmux split panes (`60×20`)**, **laptops (`100×28`)**, and **ultrawide monitors (`180×45`)**.
- **Cross-platform by design** — native execution, clipboard text/image, file locking, XDG/AppData config paths, and isolated Git-worktree subagents on **Linux, macOS, and Windows**.
- **Three-tier shell policy (no LLM classifier)** — `Auto` (shape-cached read-only commands), `Audit` (default, runs immediately and logs out of band), and `Deny` (token-aware destructive guard that pauses for human approval).
- **3-tier hybrid skill system** — project (`.twig/skills`) > global (`~/.config/twig/skills`) + cold vault (`~/.local/share/twig-skills-vault`), with zero-RTT `$skill-name` invocation and automatic Top-5 local pre-ranking when `> 15` skills are enabled.
- **Evidence over claims** — external acceptance checks, differential replay verification, BLAKE3 hash-chained JSONL session records, and ablation-controlled `harness.json` tuning (`twig improve`).

---

## Platform Support

| Platform | TUI & Core Loop | Structured Tools & Shell | Clipboard (Text / Image) | Subagents & Isolation |
|---|---|---|---|---|
| **Linux** (`x86_64` musl, `aarch64`) | Full | Full (`sh -c`, `waitid` pgid cleanup) | OSC 52 + `wl-copy` / `wl-paste` | Isolated Git worktree + optional `bwrap` OS sandbox |
| **macOS** (`arm64`, `x86_64`) | Full | Full (`sh -c`, pgid cleanup) | OSC 52 + `pbcopy` / `pngpaste` | Isolated Git worktree (`mode.set` RO for scout/reviewer) |
| **Windows** (`x86_64-msvc`) | Full (Windows Terminal) | Full (`cmd /C`, `taskkill /T` tree cleanup) | OSC 52 + `clip` / PowerShell clipboard | Isolated Git worktree (`mode.set` RO for scout/reviewer) |
| **Mobile SSH / Termux** | Responsive compact layout (`>= 32 cols`) | Remote host or Termux shell | OSC 52 | Runs on connected host |

---

## Installation

### 1. One-Line Prebuilt Installer (Recommended)

**Linux & macOS:**
```bash
curl -fsSL https://raw.githubusercontent.com/SEMANTICx/twig/main/scripts/install.sh | sh
```

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/SEMANTICx/twig/main/scripts/install.ps1 | iex
```

*(Both scripts verify SHA-256 checksums against GitHub Releases and install into `~/.local/bin` or `%LOCALAPPDATA%\twig\bin`. Pin a version with `TWIG_VERSION=v0.2.0`.)*

### 2. Cargo / `cargo-binstall`

```bash
# Instant prebuilt download via cargo-binstall (no local compilation)
cargo binstall --git https://github.com/SEMANTICx/twig.git twig

# Or build from source
cargo install --git https://github.com/SEMANTICx/twig.git --locked
```

### 3. Manual Build from Source

```bash
git clone https://github.com/SEMANTICx/twig.git
cd twig
cargo build --release --locked
```

---

## Quick Start

```bash
# 1. Verify environment, config, git/bwrap, model gateway, and skills
twig doctor

# 2. Run offline harness self-check (no model/network required)
twig selfcheck

# 3. Launch the interactive TUI in any repository
cd /path/to/your/repo
twig chat

# 4. Run a headless single-shot task or manage first-class sessions from CLI/scripts
twig exec --role plan "Inspect architecture and list bottlenecks"
twig exec --check "cargo test" --json "Fix off-by-one in parser"
twig exec --events "Trace and fix failing test"
twig session list
twig session search "parser"
```

---

## Responsive Borderless TUI (`twig chat`)

`twig` adapts layout, margins, gutters, shortcut overlays, approval panels, and footer telemetry to any viewport width and height:

| Breakpoint | Typical Device | Layout Adaptation |
|---|---|---|
| **Compact (`< 58 cols`)** | Phone SSH / Termux portrait (`36×16`) | Stacked welcome header with tail-elided path & wrapped env pulse, tight 1-col margins, hidden diff line-number gutter, compact placeholder `Ask twig… (/ @ $ !)`, single-column `?` shortcut guide, concise footer (`✻ repo · model · badges`), compact approval options |
| **Split (`58–87 cols`)** | Tmux / IDE split pane (`60×20`) | Inline welcome header, full diff gutters, shortened right footer hint (`? help · ^R hist`), 1-column `?` overlay below 76 cols |
| **Desktop / Ultrawide (`>= 88 cols`)** | Laptop & desktop (`100×28` – `180×45`) | Full welcome pulse, full live telemetry (`repo (branch) +N -M · model effort · ctx% · tokens · cache%`), 2-column `?` shortcut reference |

```text
  ✻ twig 0.2.0  ~/Dev/my-project
    skills 10 (+13 vault)  ·  docs AGENTS.md  ·  mcp 0  ·  sandbox off

 ───────────────────────────────────────────────────────────────────────────
  ❯ Ask twig anything… (/ command · @ file · $ skill · ! shell)
 ───────────────────────────────────────────────────────────────────────────
  ✻ my-project (main) +12 -3 · claude-sonnet-4 high     ? shortcuts · Shift+Tab plan · ^R history
```

### Input Prefixes

| Prefix | Purpose |
|---|---|
| `/` | Fuzzy slash-command palette |
| `@` | Repository file, inline image (`@shot.png`), or live context (`@diff`, `@staged`, `@last`) |
| `$` | Direct skill autocomplete & zero-RTT invocation (supports active + `[vault]` skills) |
| `!` | Run a local shell command immediately in the repo without spending tokens |

### Key Slash Commands & Shortcuts

| Command / Key | Action |
|---|---|
| `Shift+Tab` / `/plan` | Toggle read-only planning mode (automatically routes to `plan` role model) |
| `/study [on\|off\|<target>]` | Toggle opt-in REA clean-room study mode (`[Observed]`/`[Derived]`/`[Inferred]`/`[Unknown]` provenance; 0 tokens when off) |
| `/go [instructions]` | Exit `/plan` & `/study`, switch back to `code` role model, and execute the agreed plan/spec + tests |
| `/roles [<role> <model> [effort]]` | Configure heterogeneous models or external CLI workers (`cli:codex`, `cli:claude`, `cli:gemini`) per role in `roles.json` |
| `/operator` | Toggle Agent-as-Memory multi-session orchestration mode (zero token overhead when off) |
| `/dual-plan <goal>` | Run blind independent tool-driven plans (`outline`/`read`/`grep`/`glob`) on `plan` × `advisor` models, then synthesize a brief |
| `/btw <question>` | Ask an ephemeral side question reusing KV cache without polluting session history |
| `/walkthrough` / `/commit [hint]` | Generate a logical walkthrough of `git diff` / stage & commit with a repo-style-matched message |
| `/apply <sid>` / `/gc` | Apply a subagent session patch cleanly into the worktree (reversible via `/undo`) / clean up 0-byte & stale subagent remnants |
| `Esc×2` / `/rewind [N] [--code]` | Rewind conversation turns (`Enter` chat-only, `r` / `--code` also restores Git worktree snapshot) |
| `/undo` | Revert the last `edit`, `write`, or `apply_patch` file change |
| `/model` / `/effort` | Switch active fallback model via gateway / set reasoning effort (`off`..`xhigh`) |
| `/skills [list\|enable\|disable\|doctor]` | Inspect, hot-toggle, and lint 3-tier skills without restarting |
| `/diff` / `/review` | Inspect git diff / run two-tier (`blockers` vs `suggestions`) independent review with auto `xhigh` escalation on sensitive paths |
| `/tasks` / `/mcp [enable\|disable\|init-rea]` | Inspect background tasks / list or hot-toggle MCP servers (including opt-in `rea-agents` scaffold) |
| `/evolve` | Guard-first ratchet proposal (executable tests/`post_edit_check` first, bounded `<= 2.4K` `.twig/preferences.md` consolidation) |
| `/memory` / `/init` | Edit `.twig/preferences.md` in `$EDITOR` (`/memory show` displays budget) / scaffold `.twig/` |
| `/fork` / `/resume` / `/archive` / `/export` | Branch, repo-scoped resume (`a` toggles all repos), archive, or export sessions to Markdown |
| `/compact` / `/verify` / `/status` / `/doctor` | Two-stage context compaction / verify BLAKE3 hash chain / inspect telemetry & environment |
| `Ctrl+R` / `Ctrl+F` / `Ctrl+G` / `Ctrl+O` | Persistent prompt history / search transcript / compose in `$EDITOR` / expand tool outputs |
| `Alt+V` / `Alt+Y` / `Alt+M` / `?` | Paste clipboard image / copy code block (`+Shift` full reply) / toggle mouse select / shortcut overlay |

---

## Agent Tools & Runtime Architecture

### Built-in Tools

| Tool | Behavior |
|---|---|
| `outline`, `read`, `edit`, `write`, `apply_patch`, `grep`, `glob` | Worktree-confined structured tools: zero-dependency symbol `outline`, line-numbered `read` (`artifact:<file>`, `session:<sid>`, lazy subdirectory `AGENTS.md`/`CLAUDE.md`), Read-Before-Edit `mtime` lock (persisted across `/resume`), 4-level progressive indentation `edit`, multi-file `apply_patch`, and `.twig/project.json` `post_edit_check` / `check_on_change` hooks |
| `bash` | Bounded 1 MiB shell runner (`sh -c` on Unix, `cmd /C` on Windows) with automatic process-group/FD reclamation, parallel `Auto` read batching, optional Linux `bwrap` isolation, and tiered policy |
| `task` | Session-scoped async background shell tasks (`start`, `send` stdin, `status`, `wait`, `cancel`, `list`) with cancellation support |
| `subagent` | Parallel & resumable (`resume_sid`) isolated Git worktrees (`code`, `test`, `scout`, `advisor`, `reviewer`); returns `sid`, `diff_stat`, and `artifact:subagent-<sid>.patch` |
| `skill` | Progressive-disclosure skill catalog (`query`) and on-demand body loader (`name`), including cold-vault fallback |
| `mcp` | Lazy stdio JSON-RPC 2.0 MCP client (`.twig/mcp.json` & config `mcp.json`) supporting tools (`Audit` tier, blocked in read-only mode), prompts, and resources |
| `todo` / `papercut` | Live progress checklist / structured harness defect reporting |

### 3-Tier Skill Management (`twig skill`)

```bash
twig skill list                         # Inspect project, global, and vault skills
twig skill enable <name> [--project]    # Enable globally (symlink on Unix, copy on Windows) or in .twig/skills/
twig skill disable <name> [--project]   # Disable without deleting from vault
twig skill add <path> [--name N] [--enable]
twig skill doctor                       # Audit frontmatter, 500-line budget, shadowing & hygiene
```

- **Tier 1 (Project)**: `<repo>/.twig/skills/<name>/SKILL.md` (highest precedence, confined to repo)
- **Tier 2 (Global Active)**: `~/.config/twig/skills` (Windows: `%APPDATA%\twig\skills`, recommended `<= 10–15` evergreen skills)
- **Tier 3 (Cold Vault)**: `~/.local/share/twig-skills-vault` (Windows: `%LOCALAPPDATA%\twig-skills-vault`; zero Layer-1 token cost; invoke on-demand via `$name` or `skill(name="...")`)

### Instruction Discovery

On every turn, `twig` automatically loads repository instructions in precedence order (bounded to 32 KB):
1. `AGENTS.md`
2. `CLAUDE.md`
3. `.twig/preferences.md`

### Safety, Guard & Compaction

- **Three-Tier Shell Policy**:
  - `Auto`: read-only commands (`ls`, `cat`, `rg`, `git status/diff/log`, …) with shape caching.
  - `Audit`: default for ordinary work; runs immediately and logs to the session record.
  - `Deny`: token-aware destructive guard (`rm -rf /`, `rm -fr ~`, `git push -f/--force`, `git reset --hard`, `git clean -fdx`, `dd of=/dev/...`, `mkfs*`, etc.) held for explicit human approval (`1. Allow once` / `2. Always allow shape` / `3. Deny`).
- **Progress Guard & Loop Breaker**: detects stalled turns (warns → narrows to read-only → aborts safely) and nudges the agent when identical failing tool calls repeat.
- **Lossless Compaction**: archives full history to JSON, preserves verbatim user constraints, valid tool-call pairs, and all `artifact:tool-*.txt` side-channel references across compaction and resume.

---

## Releasing & Distribution

Every `v*` tag triggers [`.github/workflows/release.yml`](.github/workflows/release.yml) to lint and build 5 prebuilt archives + `SHA256SUMS.txt`:

- `twig-<tag>-x86_64-unknown-linux-musl.tar.gz` (static portable Linux x64)
- `twig-<tag>-aarch64-unknown-linux-gnu.tar.gz` (Linux ARM64)
- `twig-<tag>-aarch64-apple-darwin.tar.gz` (macOS Apple Silicon)
- `twig-<tag>-x86_64-apple-darwin.tar.gz` (macOS Intel)
- `twig-<tag>-x86_64-pc-windows-msvc.zip` (Windows x64)

Cut a release with:
```bash
git tag v0.2.0
git push origin v0.2.0
```

Once public, the recommended multi-channel distribution stack is:
1. **GitHub Releases + `install.sh` / `install.ps1`** (already wired in this repo)
2. **`cargo-binstall`** (`[package.metadata.binstall]` already configured in `Cargo.toml`)
3. **Homebrew Tap (`brew install SEMANTICx/tap/twig`)** for macOS/Linux + **Scoop bucket (`scoop install twig`)** for Windows + **AUR (`twig-bin`)** for Arch Linux — all pointing directly at the GitHub Release archives and `SHA256SUMS.txt`.

---

## Configuration & Environment Variables

| Variable | Default | Description |
|---|---|---|
| `TWIG_BASE_URL` | `http://127.0.0.1:3425/v1` | OpenAI-compatible gateway URL |
| `TWIG_API_KEY` | `magpie` | Bearer token for gateway |
| `TWIG_MODEL` | *(saved `/model`)* | Override default model (takes precedence over saved `model.txt`) |
| `TWIG_ROLES` | — | JSON role map, e.g. `'{"code":"...","plan":"...","filter":"...","judge":"..."}'` (`scout→filter`, `reviewer→judge`) |
| `TWIG_EFFORT` | *(saved `/effort`)* | Reasoning effort: `off`, `low`, `medium`, `high`, `xhigh` |
| `TWIG_CONFIG` | `~/.config/twig/harness.json` | Path to `HarnessConfig` JSON |
| `TWIG_SKILLS_DIR` | `~/.config/twig/skills` | Active global skills directory |
| `TWIG_SKILLS_VAULT` | `~/.local/share/twig-skills-vault` | Inactive cold skill vault directory |
| `TWIG_WORKERS` | `4` | Parallel worker count for `twig eval` |
| `TWIG_WALL_SECS` | `300` | Per-task wall-clock limit for `twig eval` |

---

## Development & Quality Gates

```bash
deslop-please                                # Architecture & hygiene lint (0 errors)
cargo clippy --all-targets -- -D warnings    # Zero warnings
cargo run --quiet -- selfcheck               # Offline self-check
cargo test --no-fail-fast                    # Full unit + integration suite
```

See [AGENTS.md](AGENTS.md) for codebase architecture and contributor guidelines.

---

## License

[MIT](LICENSE)
