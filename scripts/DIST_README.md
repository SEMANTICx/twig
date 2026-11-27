# ✻ twig — Official Binary Distribution

**A fast, single-binary coding-agent CLI, borderless TUI & web cockpit for Linux, macOS, and Windows.**

> This repository hosts **official prebuilt binaries, SHA-256 checksums, and zero-login one-line installers** for `twig`.

---

## One-Line Install (No Login Required)

### Linux & macOS

```bash
curl -fsSL https://raw.githubusercontent.com/SEMANTICx/twig-dist/main/install.sh | sh
```

Pin a specific version or custom install directory:

```bash
curl -fsSL https://raw.githubusercontent.com/SEMANTICx/twig-dist/main/install.sh | TWIG_VERSION=v0.2.0 sh
```

### Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/SEMANTICx/twig-dist/main/install.ps1 | iex
```

### Supported Targets

| OS | Architecture | Release Artifact |
|---|---|---|
| **Linux** (static `musl`, zero `glibc` dependency) | `x86_64` | `twig-v*-x86_64-unknown-linux-musl.tar.gz` |
| **Linux** | `aarch64` / `arm64` | `twig-v*-aarch64-unknown-linux-gnu.tar.gz` |
| **macOS** (Apple Silicon) | `arm64` | `twig-v*-aarch64-apple-darwin.tar.gz` |
| **macOS** (Intel) | `x86_64` | `twig-v*-x86_64-apple-darwin.tar.gz` |
| **Windows** | `x86_64` | `twig-v*-x86_64-pc-windows-msvc.zip` |

Every release includes per-archive `.sha256` digests and `SHA256SUMS.txt` verified automatically during installation.

---

## Highlights

- **Single Static Binary (`<15ms` startup, `~10 MB` RAM)** — Zero Node.js or Python runtime dependencies.
- **Gateway-Neutral + Multi-CLI Routing** — Connect any OpenAI-compatible gateway (`TWIG_BASE_URL` / `TWIG_API_KEY`), route models per role (`code`, `plan`, `advisor`, `reviewer`, `scout`), or bridge local `cli:claude`, `cli:codex`, and `cli:gemini` runners.
- **Borderless TUI (`twig chat`) & Web Cockpit (`twig web`)** — Result-focused tool folding (`Ctrl+O`), unfolded `Update(file) +N -M` cards with **intra-line word diffs**, diff approval previews with inline rejection feedback (`Tab`), `Alt+Enter` queued follow-ups (`↑` to edit), `Ctrl+B` background detach, `Ctrl+T`/`↓` Subagent & Task drawer, `/btw` floating side-cards, **plus terminal GFM box tables, LaTeX math, and Mermaid/SVG diagrams**.
- **Two-Stage Lossless Compaction & BLAKE3 Ledger** — Offloads large tool outputs to addressable `artifact:tool-*.txt` files and records tamper-evident hash-chained sessions (`twig record`).
- **Recursive Collective Intelligence (RCI) & Verification** — Isolated Git-worktree subagents with a **Split-Fact Blackboard** (`/blackboard`), `.twig/notes/<role>.md` protocol cards, **Equal-Budget Ladder** grading (`twig ladder`), label-free gated recursion (`twig rci`), and clean-room differential verification (`twig verify`).

---

## Quick Start

```bash
# 1. Verify environment, sandbox, and model gateway configuration
twig doctor

# 2. Launch interactive TUI inside any Git repository
twig chat

# 3. Launch browser / mobile cockpit
twig web --port 3426

# 4. Run non-interactive single-shot execution with an acceptance check
twig exec --check "cargo test" "Fix failing edge case and verify"
```

### Essential `twig chat` Shortcuts

| Key / Prefix | Action |
|---|---|
| `/` · `@file#L10-50` · `$skill` · `!cmd` · `#note` | Slash commands · file/line-slice context · skill invoke · local Bash mode · quick memory save |
| `Shift+Tab` | Cycle `Auto-Accept Edits` ⇄ `◈ Plan (read-only)` ⇄ `🔒 Review Edits (diff approval)` |
| `Ctrl+O` | Expand / fold intermediate tool outputs |
| `Ctrl+B` / `Ctrl+T` (`↓`) | Detach running Bash command to background / open Subagents & Tasks drawer |
| `Alt+Enter` / `↑` | Queue follow-up prompt while busy / pop queued prompt back into input to edit |
| `Esc×2` / `/btw <q>` | Rewind chat & Git worktree snapshot / ask ephemeral floating side question |

### Gateway & Role Configuration

Set environment variables or use `/model` and `/roles` inside `twig chat`:

```bash
export TWIG_BASE_URL="http://127.0.0.1:3425/v1"   # Any OpenAI-compatible /v1 endpoint
export TWIG_API_KEY="your-api-key"
export TWIG_MODEL="gpt-5.4"                       # Or cli:claude / cli:codex / cli:gemini
```
