#!/usr/bin/env bash
# Publish stripped, path-sanitized `twig` binaries and installer scripts to a
# public release-only GitHub repository (`SEMANTICx/twig-dist`) while keeping
# the main source repository (`SEMANTICx/twig`) 100% PRIVATE with zero git history shared.
#
# Usage:
#   ./scripts/publish-dist.sh --init-only           # Create/sync public installer repo only
#   ./scripts/publish-dist.sh v0.2.0                # Build stripped Linux binary & publish release v0.2.0
set -euo pipefail

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
DIST_REPO="${TWIG_DIST_REPO:-SEMANTICx/twig-dist}"
MODE="${1:-}"

if [ -z "$MODE" ]; then
  echo "Usage: $0 <--init-only | vX.Y.Z>" >&2
  exit 1
fi

if ! command -v gh >/dev/null 2>&1; then
  echo "error: GitHub CLI (gh) is required" >&2
  exit 1
fi
gh auth status >/dev/null

# 1. Ensure public distribution-only repository exists.
if ! gh repo view "$DIST_REPO" >/dev/null 2>&1; then
  echo "==> Creating public release-only repository: ${DIST_REPO}"
  gh repo create "$DIST_REPO" --public \
    --description "Official prebuilt binaries and installers for twig"
fi

# 2. Sync ONLY installer scripts & public README via a clean orphan commit (zero source or git history).
STAGE_DIR="$(mktemp -d)"
trap 'rm -rf "$STAGE_DIR"' EXIT

mkdir -p "$STAGE_DIR/scripts"
cp "$ROOT/scripts/DIST_README.md" "$STAGE_DIR/README.md"
cp "$ROOT/scripts/install.sh" "$STAGE_DIR/install.sh"
cp "$ROOT/scripts/install.ps1" "$STAGE_DIR/install.ps1"
cp "$ROOT/scripts/install.sh" "$STAGE_DIR/scripts/install.sh"
cp "$ROOT/scripts/install.ps1" "$STAGE_DIR/scripts/install.ps1"
chmod 755 "$STAGE_DIR/install.sh" "$STAGE_DIR/scripts/install.sh"

(
  cd "$STAGE_DIR"
  git init -q -b main
  git config user.name "twig-release-bot"
  git config user.email "noreply@users.noreply.github.com"
  git add README.md install.sh install.ps1 scripts/install.sh scripts/install.ps1
  git commit -q -m "chore: sync public installer scripts"
  REMOTE_URL="https://github.com/${DIST_REPO}.git"
  git push -q --force "$REMOTE_URL" main:main
)
echo "==> Synced clean orphan installer files to https://github.com/${DIST_REPO}"

if [ "$MODE" = "--init-only" ]; then
  echo "Done. Public install command will work as soon as a v* release is uploaded."
  exit 0
fi

TAG="$MODE"
case "$TAG" in
  v*) ;;
  *) echo "error: release tag must start with 'v' (e.g. v0.2.0)" >&2; exit 1 ;;
esac

# 3. Build stripped, path-remapped Linux binary (prefers static musl when installed, falls back to host).
ARCH="$(uname -m)"
if [ "$ARCH" = "x86_64" ] && rustup target list --installed 2>/dev/null | grep -qx "x86_64-unknown-linux-musl" && command -v musl-gcc >/dev/null 2>&1; then
  TARGET="x86_64-unknown-linux-musl"
  BUILD_ARGS=(--target "$TARGET")
  BIN_PATH="$ROOT/target/$TARGET/release/twig"
else
  case "$ARCH" in
    x86_64|amd64) TARGET="x86_64-unknown-linux-musl" ;;
    aarch64|arm64) TARGET="aarch64-unknown-linux-gnu" ;;
    *) echo "error: unsupported arch $ARCH" >&2; exit 1 ;;
  esac
  BUILD_ARGS=()
  BIN_PATH="$ROOT/target/release/twig"
fi

echo "==> Building stripped, path-remapped release binary (${TARGET})..."
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
RUSTUP_HOME_DIR="${RUSTUP_HOME:-$HOME/.rustup}"
RUSTFLAGS="--remap-path-prefix=${ROOT}=twig --remap-path-prefix=${CARGO_HOME_DIR}=/cargo --remap-path-prefix=${RUSTUP_HOME_DIR}=/rustup" \
  cargo build --manifest-path "$ROOT/Cargo.toml" --release --locked "${BUILD_ARGS[@]}"

PKG="twig-${TAG}-${TARGET}"
OUT_DIR="$STAGE_DIR/out"
mkdir -p "$OUT_DIR/$PKG"
install -m 755 "$BIN_PATH" "$OUT_DIR/$PKG/twig"
cp "$ROOT/scripts/DIST_README.md" "$OUT_DIR/$PKG/README.md"
tar -C "$OUT_DIR" -czf "$OUT_DIR/${PKG}.tar.gz" "$PKG"
(cd "$OUT_DIR" && sha256sum "${PKG}.tar.gz" > "${PKG}.tar.gz.sha256" && cp "${PKG}.tar.gz.sha256" SHA256SUMS.txt)

echo "==> Publishing ${TAG} artifacts to public repo ${DIST_REPO}..."
if gh release view "$TAG" --repo "$DIST_REPO" >/dev/null 2>&1; then
  gh release upload "$TAG" "$OUT_DIR/${PKG}.tar.gz" "$OUT_DIR/${PKG}.tar.gz.sha256" "$OUT_DIR/SHA256SUMS.txt" --clobber --repo "$DIST_REPO"
else
  gh release create "$TAG" "$OUT_DIR/${PKG}.tar.gz" "$OUT_DIR/${PKG}.tar.gz.sha256" "$OUT_DIR/SHA256SUMS.txt" \
    --repo "$DIST_REPO" \
    --title "twig ${TAG}" \
    --notes "Prebuilt binaries for twig ${TAG}. Install via: \`curl -fsSL https://raw.githubusercontent.com/${DIST_REPO}/main/install.sh | sh\`"
fi

echo "==> Published! Anyone can now install without login via:"
echo "    curl -fsSL https://raw.githubusercontent.com/${DIST_REPO}/main/install.sh | sh"
