#!/usr/bin/env bash
# Runs the tests in this folder. Exit 0 means pass.
# Fails when a test file differs from the original.
set -u
cd "$(dirname "$0")"
TEST_FILES=(validate.test.ts)
EXPECTED=86175bfedf3982acf2852b6f0f7219114eff6b9d3c770f8d350684aab2e4551e
sum256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi; }
ACTUAL=$(cat "${TEST_FILES[@]}" 2>/dev/null | sum256 | cut -d' ' -f1)
if [ "$ACTUAL" != "$EXPECTED" ]; then
  echo "test files were changed" >&2
  exit 1
fi
exec node --test validate.test.ts
