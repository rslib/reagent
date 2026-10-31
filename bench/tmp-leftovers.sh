#!/usr/bin/env bash
# Runs the test suites with TMPDIR set to an empty folder and fails when they
# leave anything there or in /tmp. The one allowed leftover is reagent-spill:
# test processes and killed workers never reach the exit cleanup, and the next
# run's first spill sweeps their folders once they are past the grace time.
set -euo pipefail

cd "$(dirname "$0")/.."

tmp="$(mktemp -d)"
marker="$(mktemp)"
trap 'rm -rf "$tmp" "$marker"' EXIT

echo "bench/tmp-leftovers.sh: running the suites with TMPDIR=$tmp"
TMPDIR="$tmp/" cargo nextest run --workspace --locked --no-fail-fast
TMPDIR="$tmp/" cargo nextest run -p reagent --features testing --no-fail-fast

spill="$tmp/reagent-spill"
if [ -d "$spill" ]; then
	runs="$(find "$spill" -mindepth 1 -maxdepth 1 -name 'run-*' | wc -l | tr -d ' ')"
	echo "bench/tmp-leftovers.sh: reagent-spill holds $runs run folders, swept by the next run"
fi

left="$(find "$tmp" -mindepth 1 -maxdepth 1 ! -name reagent-spill)"
new_tmp="$(find /tmp -mindepth 1 -maxdepth 1 -newer "$marker" \( -name 'reagent*' -o -name '.tmp*' \) 2>/dev/null || true)"

if [ -n "$left" ] || [ -n "$new_tmp" ]; then
	echo "bench/tmp-leftovers.sh: the suites left these behind:"
	[ -n "$left" ] && echo "$left"
	[ -n "$new_tmp" ] && echo "$new_tmp"
	exit 1
fi
echo "bench/tmp-leftovers.sh: nothing left behind"
