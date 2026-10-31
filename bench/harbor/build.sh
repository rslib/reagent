#!/usr/bin/env bash
# Build a statically linked x86_64 Linux musl binary of reagent and copy it to
# bench/harbor/dist/reagent.
#
# The task containers that terminal-bench runs agents in are x86_64 Linux, so
# the binary is cross-compiled here (even from an Apple Silicon Mac).
#
# Requires cargo-zigbuild and zig; zig supplies the cross C toolchain.
#   Install with:
#     brew install zig
#     cargo install cargo-zigbuild

set -euo pipefail

if ! command -v cargo-zigbuild >/dev/null 2>&1; then
  echo "missing cargo-zigbuild; install it with: cargo install cargo-zigbuild" >&2
  exit 1
fi
if ! command -v zig >/dev/null 2>&1; then
  echo "missing zig; install it with: brew install zig" >&2
  exit 1
fi

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET=x86_64-unknown-linux-musl

cd "$ROOT"
rustup target add "$TARGET" >/dev/null
cargo zigbuild --profile min -p reagent --no-default-features --target "$TARGET"

TARGET_DIR="$(cargo metadata --format-version 1 --no-deps |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
SRC="$TARGET_DIR/$TARGET/min/reagent"
if [[ ! -x "$SRC" ]]; then
  echo "expected binary not found: $SRC" >&2
  exit 1
fi

mkdir -p "bench/harbor/dist"
cp "$SRC" "bench/harbor/dist/reagent"
chmod +x "bench/harbor/dist/reagent"
echo "Built bench/harbor/dist/reagent ($(du -h "bench/harbor/dist/reagent" | cut -f1))"
