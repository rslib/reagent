#!/usr/bin/env bash
# Runs the tests in this folder. Exit 0 means pass.
# Fails when a test file differs from the original.
set -u
cd "$(dirname "$0")"
TEST_FILES=(test_book.py)
EXPECTED=433b7e4e78723adbbf07815282939382a21029b8d23348d2e51715dc7bc19806
sum256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi; }
ACTUAL=$(cat "${TEST_FILES[@]}" 2>/dev/null | sum256 | cut -d' ' -f1)
if [ "$ACTUAL" != "$EXPECTED" ]; then
  echo "test files were changed" >&2
  exit 1
fi
# A fresh cache folder that is never written: a compiled file from an
# earlier test run cannot stand in for a same-size edit made in the same second.
export PYTHONDONTWRITEBYTECODE=1 PYTHONPYCACHEPREFIX="$PWD/.no-pycache"
exec python3 -m unittest discover -s . -p "test_*.py"
