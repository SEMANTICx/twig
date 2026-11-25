# twig

**A thin, high-discipline coding-agent harness in Rust.**

> Any OpenAI-compatible gateway (default: `magpie`) provides the models; **twig** provides the execution loop, structured tools, tiered safety policy, isolated subagents, curated skills, verifier, borderless TUI, and tamper-evident session record.

---

## Why twig?

Most coding agents accumulate hidden latency and prompt bloat: per-call LLM classifiers, repetitive permission popups for harmless reads, full-tree scans on boot, and megabytes of unchecked skill text.

`twig` takes the opposite approach:

- **One round-trip per model turn** — multiple tool calls execute within a single turn, with mid-turn steering (`Enter` while running) and automatic retry on transient upstream errors.
- **Borderless warm-terracotta TUI** — zero 4-sided box borders, live environment pulse on launch (`skills · docs · mcp · sandbox`), 60fps smooth scroll, diff line gutters, live Git `+N -M` stat, and Kitty OSC 99 notifications.
- **Three-tier shell policy (no LLM classifier)** — `Auto` (shape-cached read-only commands), `Audit` (default, runs immediately and logs out of band), and `Deny` (token-aware destructive guard that pauses for human approval).
- **3-tier hybrid skill system** — project (`.twig/skills`) > global (`~/.config/twig/skills`) + cold vault (`~/.local/share/twig-skills-vault`), with zero-RTT `$skill-name` invocation and automatic Top-5 local pre-ranking when `> 15` skills are enabled.
- **Evidence over claims** — external acceptance checks, differential replay verification, BLAKE3 hash-chained JSONL session records, and ablation-controlled `harness.json` tuning (`twig improve`).

---

## Requirements

- **OS**: Linux (uses Linux `waitid(WNOWAIT)` process-group cleanup and optional `bwrap` namespace isolation)
- **Tools**: `git` (required), `bwrap` / bubblewrap (optional, for sandboxed commands & subagents), `gh` (optional, for `twig automate`), `wl-copy` / `wl-paste` (optional, Wayland clipboard text/image support)
- **Model Gateway**: Any OpenAI-compatible `/v1/chat/completions` endpoint (defaults to local `magpie` at `http://127.0.0.1:3425/v1`)

---

## Installation

### From Source (Recommended)

```bash
git clone https://github.com/SEMANTICx/twig.git
cd twig
cargo build --release --locked
install -Dm755 target/release/twig ~/.local/bin/twig
```

Or install directly with Cargo:

```bash
cargo install --git https://github.com/SEMANTICx/twig.git --locked
```

### From GitHub Releases

Prebuilt Linux binaries (`x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`) are built automatically on every `v*` tag:

```bash
VERSION=v0.2.0
ARCH=$(uname -m)
curl -fsSL "https://github.com/SEMANTICx/twig/releases/download/${VERSION}/twig-${VERSION}-${ARCH}-unknown-linux-gnu.tar.gz" \
  | tar -xz --strip-components=1 -C ~/.local/bin "twig-${VERSION}-${ARCH}-unknown-linux-gnu/twig"
```

---

## Quick Start

```bash
# 1. Verify environment, config, bwrap, magpie gateway, and skills
twig doctor

# 2. Run offline harness self-check (no model/network required)
twig selfcheck

# 3. Launch the interactive TUI in any repository
cd /path/to/your/repo
twig chat
```

---

## Interactive TUI (`twig chat`)

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
| `@` | Repository file & inline image mention (`@src/main.rs`, `@shot.png`) |
| `$` | Direct skill autocomplete & zero-RTT invocation (supports active + `[vault]` skills) |
| `!` | Run a local shell command immediately in the repo without spending tokens |

### Key Slash Commands & Shortcuts

| Command / Key | Action |
|---|---|
| `Shift+Tab` / `/plan` | Toggle read-only planning mode |
| `Esc×2` / `/rewind` | Open interactive turn rewind picker (restore & edit earlier prompt) |
| `/undo` | Revert the last `edit` or `write` tool change |
| `/model` / `/effort` | Switch active model via gateway / set reasoning effort (`off`..`xhigh`), persisted across sessions |
| `/skills [list\|enable\|disable\|doctor]` | Inspect, hot-toggle, and lint 3-tier skills without restarting |
| `/diff` / `/review` | Inspect git diff with line numbers / run an independent read-only subagent review |
| `/tasks` / `/mcp` | Inspect running background shell tasks / configured MCP servers, prompts, and resources |
| `/evolve` | Propose a human-reviewed `.twig/preferences.md` or project skill improvement from session friction |
| `/memory` / `/init` | Edit `.twig/preferences.md` in `$EDITOR` / scaffold `.twig/` |
| `/fork` / `/resume` / `/archive` / `/export` | Branch, resume, archive, or export sessions to Markdown |
| `/compact` / `/verify` / `/status` / `/doctor` | Force lossless context compaction / verify BLAKE3 hash chain / inspect telemetry & environment |
| `Ctrl+R` / `Ctrl+F` / `Ctrl+G` / `Ctrl+O` | Persistent prompt history / search transcript / compose in `$EDITOR` / expand tool outputs |
| `Alt+V` / `Alt+Y` / `Alt+M` / `?` | Paste clipboard image / copy code block (`+Shift` full reply) / toggle mouse select / shortcut overlay |

---

## Agent Tools & Runtime Architecture

### Built-in Tools

| Tool | Behavior |
|---|---|
| `read`, `edit`, `write`, `grep`, `glob` | Worktree-confined structured file tools; `edit`/`write` push snapshots for `/undo`; `read` supports `artifact:<file>` pagination |
| `bash` | Bounded 1 MiB Unix-socket shell runner with process-group cleanup, optional `bwrap` isolation, and tiered policy |
| `task` | Session-scoped async background shell tasks (`start`, `status`, `wait`, `cancel`, `list`) with cancellation support |
| `subagent` | Parallel isolated Git worktrees (`code`, `test`, `scout`, `reviewer`); read-only roles mount `/workspace` `--ro-bind` and auto-clean worktrees |
| `skill` | Progressive-disclosure skill catalog (`query`) and on-demand body loader (`name`), including cold-vault fallback |
| `mcp` | Lazy stdio JSON-RPC 2.0 MCP client (`.twig/mcp.json` & `~/.config/twig/mcp.json`) supporting tools, prompts, and resources with timeouts & auto-recovery |
| `todo` / `papercut` | Live progress checklist / structured harness defect reporting |

### 3-Tier Skill Management (`twig skill`)

```bash
twig skill list                         # Inspect project, global, and vault skills
twig skill enable <name> [--project]    # Symlink to global Tier-0 or copy into .twig/skills/
twig skill disable <name> [--project]   # Disable without deleting from vault
twig skill add <path> [--name N] [--enable]
twig skill doctor                       # Audit frontmatter, 500-line budget, shadowing & hygiene
```

- **Tier 1 (Project)**: `<repo>/.twig/skills/<name>/SKILL.md` (highest precedence, confined to repo)
- **Tier 2 (Global Active)**: `~/.config/twig/skills/<name>/` (recommended `<= 10–15` evergreen skills)
- **Tier 3 (Cold Vault)**: `~/.local/share/twig-skills-vault/<name>/` (zero Layer-1 token cost; invoke on-demand via `$name` or `skill(name="...")`)

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

## CLI Reference

```text
twig chat [repo] [sid] [--plain]             Interactive TUI (or --plain stdio mode)
twig serve [worktree] [runs] [sid]           JSON-RPC daemon over stdio
twig doctor                                  Check config, model, git/bwrap, gateway, and skills
twig selfcheck                               Offline verification suite (no network)
twig config [init]                           Print or initialize harness.json
twig skill <list|enable|disable|add|doctor>  Manage 3-tier curated skills
twig record <file.jsonl>                     Verify BLAKE3 session hash chain
twig classify <file.jsonl>                   Classify process vs content failures
twig playbooks <goal>                        Test playbook routing for a goal
twig decisions [file.tsv]                    Inspect decision trail
twig eval [dir] [reps] [only]                Run scoring suite in fresh isolated repos
twig improve [dir] [rounds]                  Tune harness.json with ablation controls
twig automate [cfg] [--once]                 Unattended GitHub issue/PR maintenance loop
twig subagent [repo] [role]                  Run an isolated role subagent
twig review [repo] <diff>                    Fresh read-only independent diff reviewer
```

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
