#!/usr/bin/env sh
set -eu

REPO="${TWIG_REPO:-SEMANTICx/twig-dist}"
INSTALL_DIR="${TWIG_INSTALL_DIR:-$HOME/.local/bin}"

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64|amd64) TARGET="x86_64-unknown-linux-musl" ;;
      aarch64|arm64) TARGET="aarch64-unknown-linux-gnu" ;;
      *) echo "twig install: unsupported Linux arch: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  Darwin)
    case "$ARCH" in
      arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
      x86_64) TARGET="x86_64-apple-darwin" ;;
      *) echo "twig install: unsupported macOS arch: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  *)
    echo "twig install: unsupported OS: $OS (use scripts/install.ps1 on Windows)" >&2
    exit 1
    ;;
esac

if [ -n "${TWIG_VERSION:-}" ]; then
  TAG="$TWIG_VERSION"
else
  TAG="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)"
fi

if [ -z "$TAG" ]; then
  echo "twig install: could not resolve latest release tag from ${REPO} (set TWIG_VERSION=v0.2.0 to pin explicitly)" >&2
  exit 1
fi

PKG="twig-${TAG}-${TARGET}"
BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

echo "Downloading ${PKG}.tar.gz..."
curl -fsSL "${BASE_URL}/${PKG}.tar.gz" -o "${TMP_DIR}/${PKG}.tar.gz"
curl -fsSL "${BASE_URL}/${PKG}.tar.gz.sha256" -o "${TMP_DIR}/${PKG}.tar.gz.sha256"

if [ "${TWIG_SKIP_CHECKSUM:-0}" = "1" ]; then
  echo "Warning: skipping SHA-256 verification (TWIG_SKIP_CHECKSUM=1)" >&2
elif command -v sha256sum >/dev/null 2>&1; then
  (cd "$TMP_DIR" && sha256sum -c "${PKG}.tar.gz.sha256")
elif command -v shasum >/dev/null 2>&1; then
  (cd "$TMP_DIR" && shasum -a 256 -c "${PKG}.tar.gz.sha256")
elif command -v openssl >/dev/null 2>&1; then
  EXPECTED="$(awk '{print $1}' "${TMP_DIR}/${PKG}.tar.gz.sha256" | tr 'A-F' 'a-f')"
  ACTUAL="$(openssl dgst -sha256 "${TMP_DIR}/${PKG}.tar.gz" | awk '{print $NF}' | tr 'A-F' 'a-f')"
  if [ -z "$EXPECTED" ] || [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "twig install: SHA-256 checksum mismatch (expected $EXPECTED, got $ACTUAL)" >&2
    exit 1
  fi
else
  echo "twig install: no SHA-256 verification tool found (install sha256sum, shasum, or openssl, or set TWIG_SKIP_CHECKSUM=1)" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
tar -C "$TMP_DIR" -xzf "${TMP_DIR}/${PKG}.tar.gz"
install -m 755 "${TMP_DIR}/${PKG}/twig" "${INSTALL_DIR}/twig"

echo "Installed twig ${TAG} to ${INSTALL_DIR}/twig"
case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "Add to PATH: export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac
