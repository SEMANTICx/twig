#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cargo build --manifest-path "$root/Cargo.toml" --release --locked
version=$("$root/target/release/twig" --version 2>/dev/null | head -n1)
name="twig-linux-$(uname -m)"
mkdir -p "$root/target/package/$name"
install -m755 "$root/target/release/twig" "$root/target/package/$name/twig"
cp "$root/README.md" "$root/target/package/$name/README.md"
tar -C "$root/target/package" -czf "$root/target/package/$name.tar.gz" "$name"
sha256sum "$root/target/package/$name.tar.gz" > "$root/target/package/$name.tar.gz.sha256"
printf '%s\n' "$root/target/package/$name.tar.gz"
