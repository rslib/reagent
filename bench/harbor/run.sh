#!/usr/bin/env bash
# Run reagent on a Harbor dataset through the ReagentAgent adapter.
#
# Usage:
#   run.sh [--dry-run] [--dev] <model> <dataset> <trials> [extra harbor args...]
#
# --dev runs only the tasks in dev-tasks.txt, 8 at a time, with a 10-minute
# agent cap. Extra harbor args come after these, so they can override them.
#
# The model credential is passed through the environment: the adapter reads
# the key variable the model prefix names (ANTHROPIC_API_KEY, OPENCODE_API_KEY
# or CMD_API_KEY) from the process env. The printed command therefore never
# contains the key value.
#
# Examples:
#   ./run.sh anthropic/claude-sonnet-5-5 terminal-bench/terminal-bench-2-1 3
#   ./run.sh opencode-go/kimi-k2.6 terminal-bench/terminal-bench-2-1 3
#   ./run.sh --dry-run anthropic/claude-sonnet-5-5 terminal-bench/terminal-bench-2-1 3 --n-concurrent 4
#   ./run.sh --dev anthropic/claude-haiku-5-5 terminal-bench/terminal-bench-2-1 1

set -euo pipefail

DRY_RUN=0
DEV=0
while [[ "${1:-}" == "--dry-run" || "${1:-}" == "--dev" ]]; do
  if [[ "$1" == "--dry-run" ]]; then
    DRY_RUN=1
  else
    DEV=1
  fi
  shift
done

if [[ $# -lt 3 ]]; then
  echo "usage: run.sh [--dry-run] [--dev] <model> <dataset> <trials> [extra harbor args...]" >&2
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

DEV_ARGS=()
if (( DEV )); then
  DEV_ARGS=(-n 8 --ak agent_timeout_sec=600)
  while IFS= read -r task; do
    task="${task%%#*}"
    task="${task//[[:space:]]/}"
    [[ -n "$task" ]] && DEV_ARGS+=(-i "$task")
  done < dev-tasks.txt
fi

COMMAND_ARGS=(
  run
  --dataset "$DATASET"
  --agent reagent_harbor.agent:ReagentAgent
  --model "$MODEL"
  --n-attempts "$TRIALS"
  ${DEV_ARGS[@]+"${DEV_ARGS[@]}"}
  "$@"
)

echo "uv run harbor ${COMMAND_ARGS[*]}"
if (( DRY_RUN )); then
  exit 0
fi

exec uv run harbor "${COMMAND_ARGS[@]}"
