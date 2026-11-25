# twig

A thin, high-discipline coding-agent harness in Rust. **magpie provides the models; twig provides the loop, tools, policy, verifier, TUI, and tamper-evident record.**

Codex-shaped architecture without the tax: a stdio JSON-RPC daemon (`twig serve`), a borderless warm-terracotta Ratatui TUI (`twig chat`), structured worktree-confined tools, tiered command policy, parallel isolated subagents, 3-tier curated skills, and a BLAKE3 hash-chained session log.

## Highlights

- **Borderless TUI (`twig chat`)**: Pure-typography welcome header with live loaded environment summary (`skills`, `docs`, `mcp`, `sandbox`), hairline composer with `/`, `@file`, `$skill`, `!shell` palettes, interactive `/rewind` (`Esc×2`), `/plan` (`Shift+Tab`), `/model`, `/effort`, `/skills`, `/diff`, `/review`, `/undo`, `/evolve`, `Ctrl+R` persistent history, `Alt+V` clipboard image attach, and live Git `+N -M` footer stat.
- **Structured Tools & Side-Channels**: `read`, `edit` (with `/undo` snapshots), `write`, `grep`, `glob`, `bash`, async background `task`, parallel `subagent` batches (isolated Git worktrees + `bwrap`), `skill`, `mcp` (tools, prompts, resources), `todo`, and `papercut`. Oversized tool outputs spill to `artifact:tool-<id>.txt` and survive context compaction.
- **3-Tier Hybrid Skill System**: Project (`.twig/skills`) > Global (`~/.config/twig/skills`) + inactive Cold Vault (`~/.local/share/twig-skills-vault`). Manage live via `twig skill <list|enable|disable|add|doctor>` or TUI `/skills`, invoke any active or vaulted skill directly with `$skill-name`, and automatically pre-rank top-5 candidates when `> 15` skills are enabled.
- **Three-Tier Shell Policy & Progress Guard**:
  - `Auto`: read-only commands with shape-cached classification.
  - `Audit` (default): runs immediately, recorded out of band.
  - `Deny`: token-aware destructive command guard (`rm -rf /`, `git push -f/--force`, `git reset --hard`, `git clean -fdx`, etc.) held for human approval.
  - Progress guard warns, narrows to read-only, breaks repeating failed calls, and preserves lossless recovery checkpoints.
- **Evaluation & Continuous Improvement**: `twig eval` runs isolated repos against external judges; `twig improve` tunes only `harness.json` with deletion-only ablation controls and independent outcome/trajectory regression gates.

## Quick Start

```bash
cargo build --release
install -Dm755 target/release/twig ~/.local/bin/twig

twig selfcheck               # offline checks (no network)
twig doctor                  # verify config, git/bwrap, magpie gateway, and skills
twig chat                    # interactive TUI in current repo
twig skill list              # inspect project / global / cold-vault skills
twig skill doctor            # lint skills for bloat, shadowing, and hygiene
```

### Environment Variables

```bash
export TWIG_BASE_URL=http://127.0.0.1:3425/v1   # magpie default gateway
export TWIG_API_KEY=magpie
export TWIG_MODEL=...                           # or use /model in TUI (persisted)
export TWIG_ROLES='{"code":"...","plan":"...","judge":"...","filter":"..."}'
export TWIG_SKILLS_DIR=~/.config/twig/skills
export TWIG_SKILLS_VAULT=~/.local/share/twig-skills-vault
```

## Quality Gates

```bash
deslop-please
cargo clippy --all-targets -- -D warnings
cargo run --quiet -- selfcheck
cargo test --no-fail-fast
```

See [AGENTS.md](AGENTS.md) for architecture details and contributor/agent conventions.
