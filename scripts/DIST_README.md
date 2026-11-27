# twig — Official Binary Distribution

Fast, single-binary coding-agent CLI & TUI for **Linux**, **macOS**, and **Windows**.

> This repository hosts **official prebuilt binaries, SHA-256 checksums, and one-line installers** for `twig`.

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

---

## Supported Targets

| OS | Architecture | Release Artifact |
|---|---|---|
| **Linux** (static `musl`, zero glibc version dependency) | `x86_64` | `twig-v*-x86_64-unknown-linux-musl.tar.gz` |
| **Linux** | `aarch64` / `arm64` | `twig-v*-aarch64-unknown-linux-gnu.tar.gz` |
| **macOS** (Apple Silicon) | `arm64` | `twig-v*-aarch64-apple-darwin.tar.gz` |
| **macOS** (Intel) | `x86_64` | `twig-v*-x86_64-apple-darwin.tar.gz` |
| **Windows** | `x86_64` | `twig-v*-x86_64-pc-windows-msvc.zip` |

Every artifact is accompanied by a `.sha256` checksum file and aggregated `SHA256SUMS.txt` verified automatically during installation.

---

## Quick Start

```bash
# 1. Check environment, sandbox, and model gateway configuration
twig doctor

# 2. Launch interactive TUI inside any Git repository
twig chat

# 3. Launch local browser/mobile cockpit
twig web

# 4. Run single-shot non-interactive task
twig exec "Summarize recent changes and run tests"
```

### Model Gateway Configuration (`~/.config/twig/harness.json`)

Configure any OpenAI-compatible endpoint (or use `cli:claude` / `cli:codex` / `cli:gemini` local CLI bridges):

```json
{
  "base_url": "https://api.openai.com/v1",
  "api_key_env": "OPENAI_API_KEY",
  "roles": {
    "code": "gpt-5",
    "plan": "gpt-5",
    "reviewer": "gpt-5"
  }
}
```
