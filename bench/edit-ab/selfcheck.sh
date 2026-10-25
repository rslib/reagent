#!/usr/bin/env bash
# For every task: the unmodified task must FAIL check.sh and the task with
# solution.patch applied must PASS. Prints a table, exits non-zero on any
# wrong result. Start with --set hard to check the hard set (default: the
# small set in tasks/). Pass task names to check only those.
set -u
# A cached .pyc from the unpatched run can be stale when the patch keeps the file size.
export PYTHONDONTWRITEBYTECODE=1
here="$(cd "$(dirname "$0")" && pwd)"
set_dir=tasks
if [ "${1:-}" = "--set" ]; then
  case "${2:-}" in
    small) set_dir=tasks ;;
    hard) set_dir=hard ;;
    *) echo "usage: selfcheck.sh [--set small|hard] [task...]" >&2; exit 2 ;;
  esac
  shift 2
fi
tasks=("$@")
if [ ${#tasks[@]} -eq 0 ]; then
  for d in "$here/$set_dir"/*/; do tasks+=("$(basename "$d")"); done
fi
bad=0
printf '%-12s %-12s %-12s %s\n' task before patch result
for t in "${tasks[@]}"; do
  tmp="$(mktemp -d)"
  cp -R "$here/$set_dir/$t/." "$tmp/"
  if (cd "$tmp" && ./check.sh >/dev/null 2>&1); then before=PASS; else before=FAIL; fi
  if (cd "$tmp" && patch -p1 -s <solution.patch >/dev/null 2>&1); then
    if (cd "$tmp" && ./check.sh >/dev/null 2>&1); then after=PASS; else after=FAIL; fi
  else
    after=NOPATCH
  fi
  if [ "$before" = FAIL ] && [ "$after" = PASS ]; then res=ok; else res=WRONG; bad=1; fi
  printf '%-12s %-12s %-12s %s\n' "$t" "$before" "$after" "$res"
  rm -rf "$tmp"
done
exit $bad
