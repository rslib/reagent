# edit-ab

A paired A/B bench for reagent's edit tool. Each arm is a name plus the reagent
flags it runs with, so two builds or two flag sets compare on the same tasks.
It has two task sets. The small set in `tasks/` has 24 small repair
tasks, 8 each in Python, TypeScript and Rust. Each task is one source file of 100
to 200 lines with 1 to 3 planted bugs that need 1 to 3 separate edits. Some
tasks hold the buggy line text twice, so a plain text match is ambiguous.

The hard set in `hard/` has 12 tasks, 4 each in Python, TypeScript and Rust
(`hard-py-01` to `hard-rs-04`). Each task is 500 to 900 lines of source in one
file, or in two files for 4 tasks. It has 5 planted bugs that need 5 separate
edits, at least one more than 300 lines away from the code the first failing
test names. Each task has a pair of near-identical blocks where one copy is
buggy and the other is right, so a plain text match of the buggy line is
ambiguous. Every bug has its own failing test, so fixing some of the bugs is
not enough to pass.

A task folder has the source, the tests, `TASK.md` (the request the agent gets),
`check.sh` (exit 0 means pass; it also fails when a test file was changed) and
`solution.patch` (the fix, never shown to the agent).

Needs `python3` 3.9, `node` 26 (native type stripping), `cargo`, `patch`, and
`uv` for the scripts. Tasks and scripts use only the standard library.

## Check the tasks

```
bench/edit-ab/selfcheck.sh [--set hard] [task...]
```

Every task must FAIL before `solution.patch` and PASS after it. The script exits
non-zero on any wrong result. `--set hard` checks the hard set instead of the
small one. Pass task names to check only those.

## Run

Run both arms with the same model, the same reasoning level and the same tasks.
The words after `--` go to reagent:

```
uv run --no-project bench/edit-ab/run.py --model <model> --arm base \
  --reasoning <level> --tasks '*' --jobs 4 --timeout 600 --out runs/<name>
uv run --no-project bench/edit-ab/run.py --model <model> --arm try \
  --reasoning <level> --tasks '*' --jobs 4 --timeout 600 --out runs/<name> \
  --reagent <other build> -- <flags>
```

`run.py` copies each task to a fresh temp dir, runs `git init` there, runs
`reagent --json`, then runs `check.sh`. It writes `<task>.<arm>.jsonl`, `.err`,
`.check.txt` and `.diff` per run and one line per run to `<out>/results.jsonl`.
`--reagent <path>` picks the binary (default `reagent` on PATH). `--provider`
is passed through, for testing with `fake:<script>`. `--set {small,hard}` picks
the task folder, `tasks/` or `hard/` (default `small`); each result row records
it as `"set"`, and both arms must use the same set. It refuses to reuse an
`--out` that already holds a result for the same set, task and arm.

## Summarize

```
uv run --no-project bench/edit-ab/summary.py runs/<name>/results.jsonl [--arms A B] [--json]
```

It prints one table per model with both arms side by side, the tasks where the
arms differ, and a sign test p-value on those pairs. `--arms` picks two when the
results hold more. It also reads older results, whose arm is the `mode` key.

## Rules

Runs must be fresh and paired: same tasks, same model, same reasoning level in
both arms. Never reuse old runs or mix in runs from another build. Compare
builds only with fresh runs of both.
