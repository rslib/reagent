"""Validates built tasks with Harbor's oracle and nop agents.

Both agents run in collect mode (no ``expect.json``), which records a result
per test. A task is valid when some test passes with the oracle solution and
fails or is missing when nothing is done. The fail-to-pass and pass-to-pass
lists go to ``tasks/<id>/tests/expect.json`` and the summary to ``valid.json``
under the data root. Run it with
``uv run python -m reagent_harbor.internal.validate [--root DATA_ROOT] [--id ID ...] [-n N] [--dry-run]``
from ``bench/harbor``.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
from pathlib import Path

DEFAULT_ROOT = "~/.local/share/reagent/internal-bench"
AGENTS = ("oracle", "nop")


def trial_reward(result_file: Path) -> float:
    """Returns the reward in a trial's ``result.json``; missing means 0."""
    try:
        result = json.loads(result_file.read_text())
        reward = result["verifier_result"]["rewards"]["reward"]
    except (OSError, ValueError, KeyError, TypeError):
        return 0.0
    return float(reward)


def task_reward(jobs_dir: Path, task_id: str) -> float:
    """Returns the reward of the task's trial in the newest job that has one."""
    for job in sorted(jobs_dir.glob("*/"), reverse=True):
        trials = sorted(job.glob(f"{task_id}__*/result.json"))
        if trials:
            return trial_reward(trials[-1])
    return 0.0


def task_results(jobs_dir: Path, task_id: str) -> dict[str, str]:
    """Returns the per-test results of the task's trial in the newest job."""
    for job in sorted(jobs_dir.glob("*/"), reverse=True):
        trials = sorted(job.glob(f"{task_id}__*/result.json"))
        if trials:
            file = trials[-1].parent / "verifier" / "results.json"
            try:
                results = json.loads(file.read_text())
            except (OSError, ValueError):
                return {}
            return results if isinstance(results, dict) else {}
    return {}


ADDED_FN = re.compile(r"^\+\s*(?:pub(?:\([\w:]+\))?\s+)?(?:async\s+)?fn\s+(\w+)", re.M)


def fix_functions(fix_patch: str) -> set[str]:
    """Names of the functions that the solution patch adds."""
    return set(ADDED_FN.findall(fix_patch))


def split_results(
    oracle: dict[str, str], nop: dict[str, str], fix_added: set[str] = frozenset()
) -> tuple[list[str], list[str]]:
    """Returns the sorted fail-to-pass and pass-to-pass test lists.

    A test that is missing without the fix and that the fix itself adds, such
    as a unit test in a source file, is left out: the agent cannot know it.
    """
    passing = {key for key, status in oracle.items() if status == "pass"}
    fail_to_pass = sorted(
        k
        for k in passing
        if nop.get(k) == "fail"
        or (k not in nop and k.split()[-1].split("::")[-1] not in fix_added)
    )
    pass_to_pass = sorted(k for k in passing if nop.get(k) == "pass")
    return fail_to_pass, pass_to_pass


def is_valid(fail_to_pass: list[str]) -> bool:
    return len(fail_to_pass) > 0


def clear_expect(root: Path, task_id: str) -> None:
    """Removes a stored test list so the collect jobs grade every test."""
    (root / "tasks" / task_id / "tests" / "expect.json").unlink(missing_ok=True)


def harbor_command(root: Path, task_id: str, agent: str, n: int) -> list[str]:
    return [
        "uv",
        "run",
        "harbor",
        "run",
        "--path",
        str(root / "tasks" / task_id),
        "--agent",
        agent,
        "--n-attempts",
        "1",
        "-n",
        str(n),
        "--jobs-dir",
        str(root / "validate-jobs" / agent),
        # Docker Hub times out or answers 502 now and then during a build.
        "--max-retries",
        "2",
        "--retry-include",
        "RuntimeError",
    ]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--root", default=os.environ.get("REAGENT_INTERNAL_BENCH", DEFAULT_ROOT)
    )
    parser.add_argument("--id", action="append", dest="ids", default=[])
    parser.add_argument("-n", type=int, default=1)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument(
        "--regrade",
        action="store_true",
        help="recompute the test lists from the stored jobs, without Harbor",
    )
    args = parser.parse_args(argv)

    root = Path(args.root).expanduser()
    tasks = root / "tasks"
    ids = args.ids or sorted(p.name for p in tasks.iterdir() if p.is_dir())

    if args.dry_run:
        for task_id in ids:
            for agent in AGENTS:
                print(shlex.join(harbor_command(root, task_id, agent, args.n)))
        return 0

    valid_file = root / "valid.json"
    valid = json.loads(valid_file.read_text()) if valid_file.exists() else {}
    for task_id in ids:
        if not args.regrade:
            clear_expect(root, task_id)
        rewards = {}
        results = {}
        for agent in AGENTS:
            if not args.regrade:
                # Harbor exits non-zero for failed trials, which is data here.
                subprocess.run(
                    harbor_command(root, task_id, agent, args.n), check=False
                )
            jobs = root / "validate-jobs" / agent
            rewards[agent] = task_reward(jobs, task_id)
            results[agent] = task_results(jobs, task_id)
        fix_patch = root / "tasks" / task_id / "solution" / "fix.patch"
        fix_added = (
            fix_functions(fix_patch.read_text(errors="replace"))
            if fix_patch.exists()
            else set()
        )
        fail_to_pass, pass_to_pass = split_results(
            results["oracle"], results["nop"], fix_added
        )
        expect = {"fail_to_pass": fail_to_pass, "pass_to_pass": pass_to_pass}
        expect_file = root / "tasks" / task_id / "tests" / "expect.json"
        expect_file.write_text(json.dumps(expect, indent=2) + "\n")
        valid[task_id] = {
            "fail_to_pass": len(fail_to_pass),
            "pass_to_pass": len(pass_to_pass),
            "valid": is_valid(fail_to_pass),
            "oracle_reward": rewards["oracle"],
            "nop_reward": rewards["nop"],
        }
        print(f"{task_id}: {valid[task_id]}")
        valid_file.write_text(json.dumps(valid, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
