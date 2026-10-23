#!/usr/bin/env bash
# Runs the tests in this folder. Exit 0 means pass.
# Fails when a test file differs from the original.
set -u
cd "$(dirname "$0")"
TEST_FILES=(test_dates.py)
EXPECTED=e630932ade085ba75f1f5edfb9631afce4c1cb5a2c339c4578830643b54161dc
sum256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi; }
ACTUAL=$(cat "${TEST_FILES[@]}" 2>/dev/null | sum256 | cut -d' ' -f1)
if [ "$ACTUAL" != "$EXPECTED" ]; then
  echo "test files were changed" >&2
  exit 1
fi
exec python3 -m unittest discover -s . -p "test_*.py"
