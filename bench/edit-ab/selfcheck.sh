#!/usr/bin/env bash
# For every task: the unmodified task must FAIL check.sh and the task with
# solution.patch applied must PASS. Prints a table, exits non-zero on any
# wrong result. Pass task names to check only those.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
tasks=("$@")
if [ ${#tasks[@]} -eq 0 ]; then
  for d in "$here"/tasks/*/; do tasks+=("$(basename "$d")"); done
fi
bad=0
printf '%-8s %-12s %-12s %s\n' task before patch result
for t in "${tasks[@]}"; do
  tmp="$(mktemp -d)"
  cp -R "$here/tasks/$t/." "$tmp/"
  if (cd "$tmp" && ./check.sh >/dev/null 2>&1); then before=PASS; else before=FAIL; fi
  if (cd "$tmp" && patch -p1 -s <solution.patch >/dev/null 2>&1); then
    if (cd "$tmp" && ./check.sh >/dev/null 2>&1); then after=PASS; else after=FAIL; fi
  else
    after=NOPATCH
  fi
  if [ "$before" = FAIL ] && [ "$after" = PASS ]; then res=ok; else res=WRONG; bad=1; fi
  printf '%-8s %-12s %-12s %s\n' "$t" "$before" "$after" "$res"
  rm -rf "$tmp"
done
exit $bad
