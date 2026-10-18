#!/usr/bin/env bash
# Run reagent on a Harbor dataset through the ReagentAgent adapter.
#
# Usage:
#   run.sh [--dry-run] <model> <dataset> <trials> [extra harbor args...]
#
# The model credential (ANTHROPIC_API_KEY, and ANTHROPIC_BASE_URL when set)
# is passed through the environment: Harbor reads it from the process env. The
# printed command therefore never contains the key value.
#
# Examples:
#   ./run.sh claude-sonnet-5-5 terminal-bench/terminal-bench-2-1 3
#   ./run.sh --dry-run claude-sonnet-5-5 terminal-bench/terminal-bench-2-1 3 --n-concurrent 4

set -euo pipefail

DRY_RUN=0
if [[ "${1:-}" == "--dry-run" ]]; then
  DRY_RUN=1
  shift
fi

if [[ $# -lt 3 ]]; then
  echo "usage: run.sh [--dry-run] <model> <dataset> <trials> [extra harbor args...]" >&2
  exit 2
fi

MODEL="$1"
DATASET="$2"
TRIALS="$3"
shift 3

# Harbor's console script does not put the cwd on sys.path, so PYTHONPATH
# makes `reagent_harbor.agent:ReagentAgent` importable.
cd "$(dirname "$0")"
export PYTHONPATH="$PWD${PYTHONPATH:+:$PYTHONPATH}"

COMMAND_ARGS=(
  run
  --dataset "$DATASET"
  --agent reagent_harbor.agent:ReagentAgent
  --model "$MODEL"
  --n-attempts "$TRIALS"
  "$@"
)

echo "uv run harbor ${COMMAND_ARGS[*]}"
if (( DRY_RUN )); then
  exit 0
fi

exec uv run harbor "${COMMAND_ARGS[@]}"
