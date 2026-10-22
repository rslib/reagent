#!/usr/bin/env bash
#
# The footprint gate: builds the `min` profile binary and checks its size
# against bench/size-budget.txt.
#
# Usage:
#   bench/size.sh              build, then check this platform's budget
#   bench/size.sh --self-test  build, check, and check that a budget under the
#                              measured size fails, using a temporary copy
#
# bench/size-budget.txt holds one line per platform,
# `<os>-<arch> <bytes>`, with os from `uname -s` lowercased (darwin, linux) and
# arch from `uname -m` (arm64, x86_64). A line starting with `#` is a comment.
#
# The check prints `size N bytes, budget B bytes (P%)`.
# Exit: 0 within budget, 1 over budget, 2 when the platform has no line.
#
# The build goes to CARGO_TARGET_DIR (default $HOME/.cache/cargo-target/size)
# and CARGO_BUILD_BUILD_DIR (default $HOME/.cache/cargo-target/size-build);
# export them to point it elsewhere.

set -euo pipefail

# Relative paths below and the cargo build start from the repository root.
cd "$(dirname "${BASH_SOURCE[0]}")/.."

BUDGET_FILE=${SIZE_BUDGET:-bench/size-budget.txt}

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/cargo-target/size}"
export CARGO_BUILD_BUILD_DIR="${CARGO_BUILD_BUILD_DIR:-$HOME/.cache/cargo-target/size-build}"
export NO_COLOR=1

say() { printf 'bench/size.sh: %s\n' "$*" >&2; }

# The platform key of this machine, for example `darwin-arm64`.
platform() {
    printf '%s-%s' \
        "$(uname -s | tr '[:upper:]' '[:lower:]')" \
        "$(uname -m)"
}

# The budget `key` has in `file`, or nothing.
budget_for() {
    awk -v key="$2" '$1 == key { print $2; exit }' "$1"
}

# The size of the built binary in bytes.
binary_size() {
    local binary="$CARGO_TARGET_DIR/min/reagent"
    [[ -f $binary ]] || {
        say "no binary at $binary; the build did not write it"
        return 1
    }
    wc -c <"$binary" | tr -d ' '
}

# Builds the min profile binary the budget is about.
build() {
    say "building the min profile binary"
    cargo build --profile min -p reagent
}

# Compares the binary with the budget for this platform in `file`.
# Returns 0 within budget, 1 over, 2 when the platform has no line.
check() {
    local budget key size percent
    key=$(platform)
    size=$(binary_size)
    budget=$(budget_for "$1" "$key")
    if [[ -z $budget ]]; then
        say "$key has no line in $1; add one as \`$key <bytes>\`"
        return 2
    fi
    percent=$(awk -v n="$size" -v b="$budget" 'BEGIN { printf "%.1f", n / b * 100 }')
    printf 'size %s bytes, budget %s bytes (%s%%)\n' "$size" "$budget" "$percent"
    # Exits 1 when the size is over the budget, 0 within it.
    awk -v n="$size" -v b="$budget" 'BEGIN { exit (n > b) }'
}

self_test() {
    build
    check "$BUDGET_FILE"
    local copy key size status=0
    copy=$(mktemp)
    key=$(platform)
    size=$(binary_size)
    # A budget a byte under the size has to fail, so the gate is shown to bite.
    printf '%s %s\n' "$key" "$((size - 1))" >"$copy"
    check "$copy" || status=$?
    rm -f "$copy"
    if ((status != 1)); then
        say "the self-test failed: a budget under the size did not fail the check (status $status)"
        exit 1
    fi
    say "self-test passed: $BUDGET_FILE holds $key, and a budget under the size fails"
}

case ${1:-} in
--self-test)
    [[ $# -eq 1 ]] || {
        say "--self-test takes no arguments"
        exit 2
    }
    self_test
    ;;
-h | --help)
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
    ;;
*)
    [[ $# -eq 0 ]] || {
        say "usage: bench/size.sh [--self-test]"
        exit 2
    }
    build
    check "$BUDGET_FILE"
    ;;
esac
