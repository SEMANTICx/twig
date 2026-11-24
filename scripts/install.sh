#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
prefix=${PREFIX:-"$HOME/.local"}
cargo build --manifest-path "$root/Cargo.toml" --release --locked
install -Dm755 "$root/target/release/twig" "$prefix/bin/twig"
printf 'Installed %s/bin/twig\nRun twig config init, set TWIG_MODEL and use twig chat /path/to/repo\n' "$prefix"
