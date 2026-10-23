#!/usr/bin/env bash
# Runs the tests in this folder. Exit 0 means pass.
# Fails when a test file differs from the original.
set -u
cd "$(dirname "$0")"
TEST_FILES=(tests/grid.rs)
EXPECTED=0c947238f786f8666f0d91e92872b667f66ec44b7b2cb6899e742002b668ee1c
sum256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi; }
ACTUAL=$(cat "${TEST_FILES[@]}" 2>/dev/null | sum256 | cut -d' ' -f1)
if [ "$ACTUAL" != "$EXPECTED" ]; then
  echo "test files were changed" >&2
  exit 1
fi
exec cargo test -q --offline
