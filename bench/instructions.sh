#!/usr/bin/env bash
#
# Exact instruction counts (Callgrind Ir) and estimated cycle counts for the
# hot-path benches, and a gate that fails a change whose Ir count or estimated
# cycle count rose more than 2% over its base commit.
#
# Usage:
#   bench/instructions.sh [BASE]       Linux only: save the benches of BASE as
#                                      baseline "base", then run the benches of
#                                      this tree against it and fail when any Ir
#                                      count or estimated cycle count rose more
#                                      than 2%
#   bench/instructions.sh --self-test  any OS: build both benches; on Linux with
#                                      Valgrind, also run them once and check
#                                      every bench printed an instruction and an
#                                      estimated cycle count
#
# BASE defaults to `git merge-base HEAD trunk`.
#
# Requirements:
#   - Linux with Valgrind (Callgrind) for the gate
#   - gungraun-runner of the gungraun version in Cargo.lock, on PATH or named by
#     GUNGRAUN_RUNNER:
#       cargo install gungraun-runner --version "$(awk ...)" --locked
#   - git: a temporary worktree of BASE is made and removed
#
# This tree builds in CARGO_TARGET_DIR=$HOME/.cache/cargo-target/gungraun and
# CARGO_BUILD_BUILD_DIR=$HOME/.cache/cargo-target/gungraun-build (export them
# to override). The base builds in temporary folders of its own, and both runs
# keep their results in one temporary GUNGRAUN_HOME.

set -euo pipefail

LIMIT_PCT=2
# The find, grep and capture benches touch the file system or a child process:
# directory reads and pipe read sizes vary between runs of the same code, so
# their counts move by up to about 10%; the other benches repeat exactly.
NOISY_LIMIT_PCT=12
BASELINE=base
BENCH_TARGET=instructions
# The cli package is `reagent` and lives in crates/cli; the base-package loop
# below maps that name. Its bench needs the package's `testing` feature, which
# this tree's runs and a base that has the bench get.
PACKAGES=(-p reagent-ai -p reagent-tools -p reagent-config -p reagent-agent -p reagent)
# The one feature the benches need: the cli package's scripted fake provider.
BENCH_FEATURES=reagent/testing

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/cargo-target/gungraun}"
export CARGO_BUILD_BUILD_DIR="${CARGO_BUILD_BUILD_DIR:-$HOME/.cache/cargo-target/gungraun-build}"
export NO_COLOR=1

say() { printf 'bench/instructions.sh: %s\n' "$*" >&2; }
die() {
    say "$*"
    exit 1
}

# Turns the default output of a run into one row per bench: the header, then
# the new and baseline Ir count, then the new and baseline Estimated Cycles
# count (empty without a baseline). Estimated Cycles is the simulated cost
# Callgrind reports from the L1, LL and RAM misses.
#
# The five fields of a row are separated by a unit separator (\037) and not a
# tab: an empty field, such as the baseline of a new bench, stays empty, which
# `read` with a tab IFS would collapse into its neighbour.
parse_counts() {
    awk '
        function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s); return s }
        # Reads the text after a metric label into pair_new and pair_old: `new|old`
        # against a baseline, `new` alone (padded with spaces) without one.
        function pair(line,   part, right) {
            if (index(line, "|") > 0) {
                split(line, part, "|")
                pair_new = trim(part[1])
                right = part[2]
                sub(/[ \t].*$/, "", right)
                pair_old = trim(right)
                # A bench the baseline lacks shows N/A as its old count.
                if (pair_old == "N/A") pair_old = ""
            } else {
                line = trim(line)
                sub(/[ \t].*$/, "", line)
                pair_new = line
                pair_old = ""
            }
        }
        function flush() {
            if (name != "") printf "%s\037%s\037%s\037%s\037%s\n", name, ir_new, ir_old, cy_new, cy_old
            name = ""; ir_new = ""; ir_old = ""; cy_new = ""; cy_old = ""
        }
        {
            gsub(/\033\[[0-9;]*m/, "")
            if ($0 ~ /^[ \t]*$/) next
            if ($0 ~ /^[ \t]/) {
                if ($0 ~ /^[ \t]*Instructions:/) {
                    line = $0
                    sub(/^[ \t]*Instructions:/, "", line)
                    pair(line)
                    ir_new = pair_new; ir_old = pair_old
                } else if ($0 ~ /^[ \t]*Estimated Cycles:/) {
                    line = $0
                    sub(/^[ \t]*Estimated Cycles:/, "", line)
                    pair(line)
                    cy_new = pair_new; cy_old = pair_old
                }
                next
            }
            flush()
            if ($0 ~ /::/) name = $0
        }
        END { flush() }
    '
}

# The change from `old` to `new` as a signed percentage, or `n/a` without a
# baseline.
pct_change() {
    awk -v n="$1" -v o="$2" 'BEGIN {
        if (o > 0) printf "%+.2f", (n - o) / o * 100; else printf "n/a"
    }'
}

# Whether `new` rose more than `limit` percent over `old`.
over_limit() {
    awk -v n="$1" -v o="$2" -v l="$3" 'BEGIN {
        exit !(o > 0 && (n - o) / o * 100 > l)
    }'
}

# The gungraun-runner version pinned in Cargo.lock.
runner_version() {
    awk '/^name = "gungraun-runner"$/ { getline; gsub(/[^0-9.]/, "", $0); print; exit }' Cargo.lock
}

# Fails unless gungraun-runner of the pinned version is reachable.
check_runner() {
    local want have
    want=$(runner_version)
    [[ -n $want ]] || die "Cargo.lock has no gungraun-runner entry"
    if [[ -n ${GUNGRAUN_RUNNER:-} ]]; then
        have=$("$GUNGRAUN_RUNNER" --version 2>/dev/null | awk '{ print $NF }') || have=""
    elif command -v gungraun-runner >/dev/null 2>&1; then
        have=$(gungraun-runner --version | awk '{ print $NF }')
    else
        die "gungraun-runner $want not found; install it with: cargo install gungraun-runner --version $want --locked"
    fi
    [[ $have == "$want" ]] ||
        die "gungraun-runner $have does not match Cargo.lock ($want); install it with: cargo install gungraun-runner --version $want --locked --force"
}

# The temporary worktree the gate makes, removed by the EXIT trap below.
WORKTREE_TMP=""
WORKTREE=""

cleanup() {
    if [[ -n $WORKTREE ]]; then
        git worktree remove --force "$WORKTREE" >/dev/null 2>&1 || true
    fi
    if [[ -n $WORKTREE_TMP ]]; then
        rm -rf "$WORKTREE_TMP"
    fi
}

at_repo_root() {
    cd "$(git rev-parse --show-toplevel)" 2>/dev/null || die "not inside a git repository"
}

run_benches() {
    cargo bench "${PACKAGES[@]}" --features "$BENCH_FEATURES" --bench "$BENCH_TARGET" -- "$@"
}

self_test() {
    at_repo_root
    say "building both $BENCH_TARGET benches without running them"
    cargo bench "${PACKAGES[@]}" --features "$BENCH_FEATURES" --bench "$BENCH_TARGET" --no-run

    if [[ $(uname -s) != Linux ]] || ! command -v valgrind >/dev/null 2>&1; then
        say "the Callgrind run is skipped: it needs Linux with valgrind"
        return 0
    fi

    check_runner
    local log err rows count missing=0 missing_cycles=0
    log=$(mktemp)
    err=$(mktemp)
    say "running both benches once under Callgrind"
    if ! run_benches >"$log" 2>"$err"; then
        tail -n 30 "$err" >&2
        die "the self-test run failed"
    fi
    rows=$(parse_counts <"$log")
    [[ -n $rows ]] || {
        tail -n 30 "$err" >&2
        die "the run printed no instruction counts"
    }
    count=0
    while IFS=$'\037' read -r name ir _ir_base cycles _cycles_base; do
        count=$((count + 1))
        # A count of 0 means Callgrind never saw the bench function, as when
        # the bench binary has no symbols.
        if [[ -z $ir || $ir == 0 ]]; then
            say "no instruction count for $name"
            missing=1
        fi
        if [[ -z $cycles || $cycles == 0 ]]; then
            say "no estimated cycle count for $name"
            missing_cycles=1
        fi
        printf '%14s %14s  %s\n' "$ir" "$cycles" "$name"
    done <<<"$rows"
    rm -f "$log" "$err"
    ((missing == 0)) || die "some benches printed no instruction count"
    ((missing_cycles == 0)) || die "some benches printed no estimated cycle count"
    say "self-test passed: $count benches printed an instruction and an estimated cycle count"
}

gate() {
    local base=${1:-}
    at_repo_root
    command -v valgrind >/dev/null 2>&1 ||
        die "valgrind not found; the gate needs Callgrind (on Debian: apt install valgrind)"
    check_runner
    if [[ -z $base ]]; then
        base=$(git merge-base HEAD trunk) || die "git merge-base HEAD trunk failed; pass a BASE commit"
    fi

    local tmp tree old_log new_log new_err rc rows name over_entry limit compared=0
    local ir ir_base cycles cycles_base ir_pct cycles_pct
    local -a over=()
    tmp=$(mktemp -d)
    tree=$tmp/tree
    WORKTREE_TMP=$tmp
    WORKTREE=$tree
    trap cleanup EXIT

    say "worktree of $base in $tree"
    git worktree add --detach "$tree" "$base" >&2 ||
        die "cannot make a worktree of $base"

    if [[ ! -f $tree/crates/ai/benches/instructions.rs ||
        ! -f $tree/crates/tools/benches/instructions.rs ]]; then
        say "$base has no instructions benches yet (crates/ai/benches/instructions.rs, crates/tools/benches/instructions.rs); nothing to compare"
        exit 0
    fi

    # The bench target of a package the base commit does not have yet cannot
    # run there: the baseline is the packages both trees have.
    local base_packages=()
    local package dir
    for package in "${PACKAGES[@]}"; do
        [[ $package == -p ]] && continue
        # The backends are named reagent-<crate> after their folder, except the
        # cli package `reagent`, which lives in crates/cli.
        dir=${package#reagent-}
        [[ $package == reagent ]] && dir=cli
        if [[ -f $tree/crates/$dir/benches/$BENCH_TARGET.rs ]]; then
            base_packages+=(-p "$package")
        else
            say "$base has no $package $BENCH_TARGET bench: it stays out of the baseline"
        fi
    done
    if ((${#base_packages[@]} == 0)); then
        say "$base has no $BENCH_TARGET benches yet; nothing to compare"
        exit 0
    fi

    # The cli bench needs the `testing` feature of its package, and cargo
    # refuses a feature of a package the run does not build, so it goes only
    # to a base that has that bench. Unquoted, so an empty value adds no word.
    local base_features=""
    if [[ " ${base_packages[*]} " == *" reagent "* ]]; then
        base_features="--features $BENCH_FEATURES"
    fi

    # Both runs share one gungraun results folder, so the baseline is found,
    # but the base builds in its own target folders: cargo names workspace
    # artifacts the same for both checkouts, and with a shared folder it would
    # call the base binary up to date and run it again for this tree.
    export GUNGRAUN_HOME=$tmp/gungraun
    old_log=$tmp/base.out
    say "running the benches of $base under Callgrind to save baseline \"$BASELINE\""
    (cd "$tree" && CARGO_TARGET_DIR=$tmp/target CARGO_BUILD_BUILD_DIR=$tmp/build \
        cargo bench "${base_packages[@]}" $base_features --bench "$BENCH_TARGET" -- "--save-baseline=$BASELINE") >"$old_log" ||
        die "the run of $base failed"

    new_log=$tmp/new.out
    new_err=$tmp/new.err
    say "running the benches of this tree against baseline \"$BASELINE\", limit ${LIMIT_PCT}% on Ir and Estimated Cycles"
    set +e
    run_benches "--baseline=$BASELINE" >"$new_log" 2>"$new_err"
    rc=$?
    set -e

    rows=$(parse_counts <"$new_log")
    if [[ -z $rows ]]; then
        tail -n 30 "$new_err" >&2
        die "the run printed no instruction counts (exit $rc)"
    fi

    printf '%-58s %13s %13s %8s %15s %15s %8s\n' \
        "bench" "Ir (base)" "Ir (new)" "Ir chg" "Cycles (base)" "Cycles (new)" "Cy chg"
    while IFS=$'\037' read -r name ir ir_base cycles cycles_base; do
        if [[ -z $ir_base ]]; then
            printf '%-58s %13s %13s %8s %15s %15s %8s\n' \
                "$name" "-" "$ir" "new" "-" "$cycles" "new"
            continue
        fi
        compared=$((compared + 1))
        ir_pct=$(pct_change "$ir" "$ir_base")
        cycles_pct=$(pct_change "$cycles" "$cycles_base")
        printf '%-58s %13s %13s %8s %15s %15s %8s\n' \
            "$name" "$ir_base" "$ir" "$ir_pct%" "$cycles_base" "$cycles" "$cycles_pct%"
        limit=$LIMIT_PCT
        [[ $name == *tools::find* || $name == *tools::grep* || $name == *tools::capture* ]] && limit=$NOISY_LIMIT_PCT
        if over_limit "$ir" "$ir_base" "$limit"; then
            over+=("$name: Ir ${ir_pct}% over ${limit}%")
        fi
        if [[ -z $cycles ]]; then
            over+=("$name: Estimated Cycles printed no count")
        elif over_limit "$cycles" "$cycles_base" "$limit"; then
            over+=("$name: Estimated Cycles ${cycles_pct}% over ${limit}%")
        fi
    done <<<"$rows"

    if ((compared == 0)); then
        tail -n 30 "$new_err" >&2
        die "no bench was compared with baseline \"$BASELINE\"; did the run of $base save it?"
    fi
    if ((${#over[@]} > 0)); then
        say "${#over[@]} metric(s) over the limit (Ir and Estimated Cycles: ${LIMIT_PCT}%, find, grep and capture ${NOISY_LIMIT_PCT}%):"
        for over_entry in "${over[@]}"; do
            printf '  %s\n' "$over_entry" >&2
        done
    fi
    if ((rc != 0)) || ((${#over[@]} > 0)); then
        if ((rc != 0)); then
            say "the run of this tree failed (exit $rc)"
            tail -n 30 "$new_err" >&2
        fi
        exit 1
    fi
    say "all benches are within their limit of $base (${LIMIT_PCT}% on Ir and Estimated Cycles, find, grep and capture ${NOISY_LIMIT_PCT}%)"
}

case ${1:-} in
--self-test)
    [[ $# -eq 1 ]] || die "--self-test takes no arguments"
    self_test
    ;;
-h | --help)
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
    ;;
*)
    if [[ $(uname -s) != Linux ]]; then
        printf 'bench/instructions.sh: Callgrind needs Linux; run it on a Linux machine\n'
        exit 2
    fi
    [[ $# -le 1 ]] || die "usage: bench/instructions.sh [BASE] | bench/instructions.sh --self-test"
    gate "${1:-}"
    ;;
esac
