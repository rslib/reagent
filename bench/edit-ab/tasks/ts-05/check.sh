#!/usr/bin/env bash
# Runs the tests in this folder. Exit 0 means pass.
# Fails when a test file differs from the original.
set -u
cd "$(dirname "$0")"
TEST_FILES=(paginate.test.ts)
EXPECTED=dafd87c578562ffd5ae1fe7394ab41ca3e561afffa674b3fdb8849ff2f92cea1
sum256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi; }
ACTUAL=$(cat "${TEST_FILES[@]}" 2>/dev/null | sum256 | cut -d' ' -f1)
if [ "$ACTUAL" != "$EXPECTED" ]; then
  echo "test files were changed" >&2
  exit 1
fi
exec node --test paginate.test.ts
