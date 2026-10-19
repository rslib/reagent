"""Summarize a Harbor job: pass rate with a 95% interval, tokens per solved
task and failure types.

Usage:
    uv run python -m reagent_harbor.report <job> [--against <job>] [--tasks <file>] [--json]

The interval resamples tasks, not trials: repeated trials of one task are
highly correlated, so treating them as independent makes the interval about
half as wide as it should be. A trial with no reward (an infrastructure error
or a timeout) counts as a failure.
"""

from __future__ import annotations

import argparse
import json
import random
import sys
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path

_EVENTS = Path("agent") / "reagent_events.jsonl"
# Summary stop reasons that name a harness-level failure.
_STOP_FAILURES = {"max_tokens", "refusal", "error", "cancelled"}
_SAMPLES = 10_000
_SEED = 0


@dataclass(frozen=True)
class Trial:
    """One trial of one task."""

    task: str
    reward: float
    exception: str | None
    prompt_tokens: int
    output_tokens: int
    turns: int
    stop: str | None


def _read_events(path: Path) -> tuple[int, int, int, str | None]:
    prompt = output = turns = 0
    stop = None
    if not path.exists():
        return prompt, output, turns, stop
    for line in path.read_text().splitlines():
        if not line.strip():
            continue
        event = json.loads(line)
        kind = event.get("type")
        if kind == "turn_start":
            turns += 1
        elif kind == "usage":
            prompt += (
                event.get("input", 0)
                + event.get("cache_read", 0)
                + event.get("cache_write", 0)
            )
            output += event.get("output", 0)
        elif kind == "summary":
            stop = event.get("stop") or event.get("exit")
    return prompt, output, turns, stop


def load_job(job: Path) -> list[Trial]:
    """Reads every trial folder of a job."""
    trials = []
    for result_path in sorted(job.glob("*/result.json")):
        result = json.loads(result_path.read_text())
        rewards = (result.get("verifier_result") or {}).get("rewards") or {}
        reward = rewards.get("reward")
        exception = (result.get("exception_info") or {}).get("exception_type")
        prompt, output, turns, stop = _read_events(result_path.parent / _EVENTS)
        trials.append(
            Trial(
                task=result.get("task_name") or result_path.parent.name.rsplit("__", 1)[0],
                reward=float(reward) if reward is not None else 0.0,
                exception=exception,
                prompt_tokens=prompt,
                output_tokens=output,
                turns=turns,
                stop=stop,
            )
        )
    return trials


def failure_type(trial: Trial) -> str:
    """Why a trial failed: the exception, a harness stop or a wrong answer."""
    if trial.reward >= 1.0:
        return "passed"
    if trial.exception:
        return trial.exception
    if trial.stop in _STOP_FAILURES:
        return trial.stop
    return "wrong_answer"


def _by_task(trials: list[Trial]) -> dict[str, float]:
    rewards: dict[str, list[float]] = defaultdict(list)
    for trial in trials:
        rewards[trial.task].append(trial.reward)
    return {task: sum(values) / len(values) for task, values in rewards.items()}


def pass_rate(trials: list[Trial]) -> float:
    """The mean over tasks of each task's mean reward."""
    per_task = _by_task(trials)
    return sum(per_task.values()) / len(per_task) if per_task else 0.0


def _interval(values: list[float], samples: int, seed: int) -> tuple[float, float]:
    if not values:
        return 0.0, 0.0
    rng = random.Random(seed)
    means = sorted(
        sum(rng.choices(values, k=len(values))) / len(values) for _ in range(samples)
    )
    return means[int(0.025 * samples)], means[int(0.975 * samples) - 1]


def bootstrap_interval(
    trials: list[Trial], samples: int = _SAMPLES, seed: int = _SEED
) -> tuple[float, float]:
    """A 95% interval for the pass rate, resampling tasks."""
    return _interval(list(_by_task(trials).values()), samples, seed)


def paired_difference(
    new: list[Trial], old: list[Trial], samples: int = _SAMPLES, seed: int = _SEED
) -> tuple[float, float, float, int]:
    """The pass-rate difference new minus old on shared tasks, its 95%
    interval and the number of shared tasks."""
    new_tasks, old_tasks = _by_task(new), _by_task(old)
    shared = sorted(new_tasks.keys() & old_tasks.keys())
    diffs = [new_tasks[task] - old_tasks[task] for task in shared]
    mean = sum(diffs) / len(diffs) if diffs else 0.0
    low, high = _interval(diffs, samples, seed)
    return mean, low, high, len(shared)


def summarize(trials: list[Trial]) -> dict:
    """Pass rate, interval, token totals and failure counts for a job."""
    low, high = bootstrap_interval(trials)
    solved = sum(trial.reward for trial in trials)
    tokens = sum(trial.prompt_tokens + trial.output_tokens for trial in trials)
    failures = Counter(
        failure_type(trial) for trial in trials if failure_type(trial) != "passed"
    )
    return {
        "tasks": len(_by_task(trials)),
        "trials": len(trials),
        "pass_rate": pass_rate(trials),
        "interval": [low, high],
        "solved": solved,
        "prompt_tokens": sum(trial.prompt_tokens for trial in trials),
        "output_tokens": sum(trial.output_tokens for trial in trials),
        "tokens_per_solved": tokens / solved if solved else None,
        "failures": dict(sorted(failures.items())),
    }


def _read_tasks(path: Path) -> set[str]:
    names = set()
    for line in path.read_text().splitlines():
        name = line.split("#", 1)[0].strip()
        if name:
            names.add(name)
    return names


def _print_summary(summary: dict) -> None:
    low, high = summary["interval"]
    print(f"tasks {summary['tasks']}, trials {summary['trials']}")
    print(
        f"pass rate {summary['pass_rate']:.1%} (95% interval {low:.1%} to {high:.1%})"
    )
    per_solved = summary["tokens_per_solved"]
    shown = f"{per_solved:,.0f}" if per_solved is not None else "none solved"
    print(f"tokens per solved task {shown}")
    print(
        f"prompt tokens {summary['prompt_tokens']:,}, output tokens {summary['output_tokens']:,}"
    )
    for kind, count in summary["failures"].items():
        print(f"  {kind}: {count}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="reagent_harbor.report")
    parser.add_argument("job", type=Path)
    parser.add_argument("--against", type=Path, help="an older job to compare with")
    parser.add_argument("--tasks", type=Path, help="only tasks named in this file")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)

    trials = load_job(args.job)
    old = load_job(args.against) if args.against else None
    if args.tasks:
        keep = _read_tasks(args.tasks)
        trials = [trial for trial in trials if trial.task in keep]
        if old is not None:
            old = [trial for trial in old if trial.task in keep]

    summary = summarize(trials)
    if old is not None:
        diff, low, high, shared = paired_difference(trials, old)
        summary["against"] = {
            "difference": diff,
            "interval": [low, high],
            "shared_tasks": shared,
        }

    if args.json:
        print(json.dumps(summary, indent=2))
        return 0
    _print_summary(summary)
    if old is not None:
        against = summary["against"]
        low, high = against["interval"]
        print(
            f"difference {against['difference']:+.1%} on {against['shared_tasks']} shared tasks "
            f"(95% interval {low:+.1%} to {high:+.1%})"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
