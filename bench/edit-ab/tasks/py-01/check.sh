#!/usr/bin/env bash
# Runs the tests in this folder. Exit 0 means pass.
# Fails when a test file differs from the original.
set -u
cd "$(dirname "$0")"
TEST_FILES=(test_inventory.py)
EXPECTED=7d72d124617599b837264f2878f71b7ced9b5fe3f8f52269c0709fd9ac8fd3aa
sum256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi; }
ACTUAL=$(cat "${TEST_FILES[@]}" 2>/dev/null | sum256 | cut -d' ' -f1)
if [ "$ACTUAL" != "$EXPECTED" ]; then
  echo "test files were changed" >&2
  exit 1
fi
exec python3 -m unittest discover -s . -p "test_*.py"
